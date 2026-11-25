//! Reading a local markdown file for its page: the one place the server reads a file a
//! request names.
//!
//! A [`LocalFile`] says only that the text is a plain absolute `.md` path. What may be shown
//! is decided here, twice: the path as written must lie under a configured root before the
//! file system is touched, and the path its symbolic links resolve to must still be a `.md`
//! file under a root (resolved the same way), so `notes.md -> ~/.ssh/id_rsa` is refused.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::types::{FileRoots, LocalFile, is_markdown};

/// The most bytes a page shows; a brief or a plan is far smaller, and the bound is what one
/// request can make the server hold.
pub const FILE_BYTES_MAX: u64 = 2 * 1024 * 1024;

/// Why a file is not shown. `Missing` and `Unreadable` mean it could not be read; the rest
/// mean it was looked at and is not one to show.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{} is not under a directory the config's files.roots names", .0.display())]
    OutsideRoots(PathBuf),
    #[error("{} resolves to {}, which is not a .md file", requested.display(), resolved.display())]
    NotMarkdown {
        requested: PathBuf,
        resolved: PathBuf,
    },
    #[error("{} is not a regular file", .0.display())]
    NotAFile(PathBuf),
    #[error("{} is {bytes} bytes; at most {FILE_BYTES_MAX} are shown", path.display())]
    TooLarge { path: PathBuf, bytes: u64 },
    #[error("{} is not UTF-8 text", .0.display())]
    NotText(PathBuf),
    #[error("no file at {}", .0.display())]
    Missing(PathBuf),
    #[error("cannot read {}: {source}", path.display())]
    Unreadable { path: PathBuf, source: io::Error },
}

/// The text of `file`, when it may be shown.
pub fn read(roots: &FileRoots, file: &LocalFile) -> Result<String, FileError> {
    let requested = file.path();
    if !roots.contain(requested) {
        return Err(FileError::OutsideRoots(requested.to_path_buf()));
    }
    let resolved = fs::canonicalize(requested).map_err(|e| io_error(requested, e))?;
    if !is_markdown(&resolved) {
        return Err(FileError::NotMarkdown {
            requested: requested.to_path_buf(),
            resolved,
        });
    }
    if !resolved_roots(roots)
        .iter()
        .any(|root| resolved.starts_with(root))
    {
        return Err(FileError::OutsideRoots(resolved));
    }
    let handle = File::open(&resolved).map_err(|e| io_error(&resolved, e))?;
    let meta = handle.metadata().map_err(|e| io_error(&resolved, e))?;
    if !meta.is_file() {
        return Err(FileError::NotAFile(resolved));
    }
    let too_large = |bytes| FileError::TooLarge {
        path: resolved.clone(),
        bytes,
    };
    if meta.len() > FILE_BYTES_MAX {
        return Err(too_large(meta.len()));
    }
    // The file may grow between the length and the read; the read is bounded either way.
    let mut bytes = Vec::new();
    handle
        .take(FILE_BYTES_MAX.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(&resolved, e))?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > FILE_BYTES_MAX {
        return Err(too_large(read));
    }
    String::from_utf8(bytes).map_err(|_| FileError::NotText(resolved))
}

/// The roots with their own symbolic links resolved, so a resolved file is compared with
/// like; a root that cannot be resolved admits nothing, and says so.
fn resolved_roots(roots: &FileRoots) -> Vec<PathBuf> {
    roots
        .as_slice()
        .iter()
        .filter_map(|root| match fs::canonicalize(root) {
            Ok(resolved) => Some(resolved),
            Err(error) => {
                tracing::warn!(root = %root.display(), %error, "a files root cannot be resolved; nothing under it is shown");
                None
            }
        })
        .collect()
}

fn io_error(path: &Path, source: io::Error) -> FileError {
    if source.kind() == ErrorKind::NotFound {
        FileError::Missing(path.to_path_buf())
    } else {
        FileError::Unreadable {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Against real files in a scratch directory: one root, files and links inside it, and a
    //! secret outside it.

    use std::os::unix::fs::symlink;

    use super::*;

    /// A scratch directory with a root and a directory outside it.
    struct Scratch(crate::testutil::Scratch);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = crate::testutil::Scratch::new(&format!("file-{label}"));
            fs::create_dir_all(dir.join("root/sub")).unwrap();
            fs::create_dir_all(dir.join("outside")).unwrap();
            Self(dir)
        }

        fn roots(&self) -> FileRoots {
            FileRoots::new(vec![self.0.join("root")]).unwrap()
        }

        fn file(&self, rest: &str) -> LocalFile {
            LocalFile::parse(&self.0.join(rest).to_string_lossy()).unwrap()
        }
    }

    #[test]
    fn a_markdown_file_under_a_root_is_read() {
        let scratch = Scratch::new("ok");
        fs::write(scratch.0.join("root/sub/a.md"), "# brief\n").unwrap();
        let text = read(&scratch.roots(), &scratch.file("root/sub/a.md")).unwrap();
        assert_eq!(text, "# brief\n");
    }

    /// Mutation: drop the check of the resolved path's extension (the `key.md` link to a
    /// non-markdown file is read), or of its root (the link out of the root is read).
    #[test]
    fn a_link_out_of_the_root_or_to_another_kind_of_file_is_refused() {
        let scratch = Scratch::new("links");
        fs::write(scratch.0.join("outside/secret.md"), "s").unwrap();
        fs::write(scratch.0.join("root/id_rsa"), "k").unwrap();
        symlink(
            scratch.0.join("outside/secret.md"),
            scratch.0.join("root/out.md"),
        )
        .unwrap();
        symlink(scratch.0.join("root/id_rsa"), scratch.0.join("root/key.md")).unwrap();
        let roots = scratch.roots();
        assert!(matches!(
            read(&roots, &scratch.file("root/out.md")),
            Err(FileError::OutsideRoots(_))
        ));
        assert!(matches!(
            read(&roots, &scratch.file("root/key.md")),
            Err(FileError::NotMarkdown { .. })
        ));
    }

    /// A path outside the roots is refused before the file system is asked anything, so even
    /// whether it exists is not told.
    /// Mutation: drop the check of the path as written (the absent file is `Missing`).
    #[test]
    fn a_path_outside_the_roots_is_refused_unread() {
        let scratch = Scratch::new("outside");
        assert!(matches!(
            read(&scratch.roots(), &scratch.file("outside/none.md")),
            Err(FileError::OutsideRoots(_))
        ));
    }

    #[test]
    fn missing_large_binary_and_directory_are_each_their_own_answer() {
        let scratch = Scratch::new("kinds");
        let roots = scratch.roots();
        assert!(matches!(
            read(&roots, &scratch.file("root/none.md")),
            Err(FileError::Missing(_))
        ));
        let large = vec![b'a'; usize::try_from(FILE_BYTES_MAX + 1).unwrap()];
        fs::write(scratch.0.join("root/large.md"), large).unwrap();
        assert!(matches!(
            read(&roots, &scratch.file("root/large.md")),
            Err(FileError::TooLarge { .. })
        ));
        fs::write(scratch.0.join("root/bin.md"), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            read(&roots, &scratch.file("root/bin.md")),
            Err(FileError::NotText(_))
        ));
        fs::create_dir(scratch.0.join("root/dir.md")).unwrap();
        assert!(matches!(
            read(&roots, &scratch.file("root/dir.md")),
            Err(FileError::NotAFile(_))
        ));
    }
}
