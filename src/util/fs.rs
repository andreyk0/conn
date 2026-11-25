//! Private files and directories, files that are replaced and never half-written, and files
//! opened only when they are files.

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;

/// The mode of every directory this tool creates: the owner's alone.
pub const DIR_MODE: u32 = 0o700;
/// The mode of every file this tool creates: the owner's alone.
pub const FILE_MODE: u32 = 0o600;

/// Creates `dir` and its missing parents with [`DIR_MODE`]; one that exists is left as it is.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(DIR_MODE).create(dir)
}

/// [`create_private_dir`], then sets the mode of `dir` itself to [`DIR_MODE`] whether it was
/// there before or not.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    create_private_dir(dir)?;
    std::fs::set_permissions(dir, Permissions::from_mode(DIR_MODE))
}

/// What a path that is not a regular file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NonRegular {
    #[error("a FIFO")]
    Fifo,
    #[error("a socket")]
    Socket,
    #[error("a character device")]
    CharDevice,
    #[error("a block device")]
    BlockDevice,
    #[error("a directory")]
    Directory,
    #[error("neither a file nor a kind of file this tool knows")]
    Other,
}

/// A file could not be opened as a regular file.
#[derive(Debug, thiserror::Error)]
pub enum OpenRegularError {
    /// The open itself failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// It opened, and is not a regular file; nothing was read from it or written to it.
    #[error("not a regular file: it is {0}")]
    NotRegular(NonRegular),
}

/// Opens `path` as `options` say, only if it is a regular file.
///
/// The open is non-blocking, because opening a FIFO blocks until the other end appears, and
/// the type is read from the descriptor that was opened, so nothing can be swapped in between a
/// check and the open.
pub fn open_regular(options: &mut OpenOptions, path: &Path) -> Result<File, OpenRegularError> {
    let nonblock = i32::try_from(OFlags::NONBLOCK.bits()).unwrap_or(0);
    let file = options.custom_flags(nonblock).open(path)?;
    let kind = file.metadata()?.file_type();
    let refused = if kind.is_file() {
        return Ok(file);
    } else if kind.is_fifo() {
        NonRegular::Fifo
    } else if kind.is_socket() {
        NonRegular::Socket
    } else if kind.is_char_device() {
        NonRegular::CharDevice
    } else if kind.is_block_device() {
        NonRegular::BlockDevice
    } else if kind.is_dir() {
        NonRegular::Directory
    } else {
        NonRegular::Other
    };
    Err(OpenRegularError::NotRegular(refused))
}

/// Replaces `path` with `bytes` so that a reader, or a crash, sees the old file or the new one
/// and never a mixture: the bytes go to a sibling temporary file that is synced and then
/// renamed over `path`. The temporary file is removed when any step fails. The directory must
/// exist; [`write_private`] creates it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = sibling_temp(path)?;
    let written = write_synced(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        // The write error is the one worth reporting; a leftover temp is only litter.
        let _ = std::fs::remove_file(&temp);
        return written;
    }
    sync_parent(path)
}

/// [`write_atomic`] into the state directory: a missing parent directory is created private
/// first.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(parent)?;
    }
    write_atomic(path, bytes)
}

fn write_synced(temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Makes the rename itself durable.
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        Some(_) | None => Ok(()),
    }
}

fn sibling_temp(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.{unique}.tmp", std::process::id()));
    Ok(path.with_file_name(temp_name))
}

#[cfg(test)]
mod tests {
    //! The properties a reader depends on: the new bytes arrive by rename (a different inode,
    //! so an open reader keeps the old file whole), and a failure leaves the old file alone
    //! and no litter.

    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::testutil::Scratch;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Mutation: write straight to `path` (`std::fs::write`) -> the inode stays -> red.
    #[test]
    fn a_rewrite_replaces_the_file_by_rename_and_leaves_no_temporary() {
        let dir = Scratch::new("rename");
        let path = dir.join("state.json");
        write_atomic(&path, b"one").unwrap();
        let first = std::fs::metadata(&path).unwrap().ino();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), first);
        assert_eq!(entries(&dir), ["state.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: leave the temp file in place on a failed rename -> red.
    #[test]
    fn a_failed_replace_keeps_the_old_content_and_removes_the_temporary() {
        let dir = Scratch::new("fail");
        let target = dir.join("state.json");
        // A non-empty directory cannot be renamed over by a file.
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();
        assert!(write_atomic(&target, b"new").is_err());
        assert_eq!(entries(&dir), ["state.json"]);
        assert_eq!(entries(&target), ["keep"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_atomic_does_not_create_a_missing_directory() {
        let dir = Scratch::new("missing");
        let path = dir.join("absent").join("state.json");
        assert!(write_atomic(&path, b"x").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A store written into a state directory that is not there yet creates it private, and
    /// the file private; an existing directory keeps its mode.
    /// Mutation: create the directory with the default mode, or chmod the existing one.
    #[test]
    fn write_private_creates_the_missing_directory_private_and_leaves_an_existing_one() {
        let dir = Scratch::new("private");
        let path = dir.join("state").join("seen.json");
        write_private(&path, b"x").unwrap();
        assert_eq!(mode(&dir.join("state")), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");

        std::fs::set_permissions(dir.join("state"), Permissions::from_mode(0o750)).unwrap();
        write_private(&path, b"y").unwrap();
        assert_eq!(mode(&dir.join("state")), 0o750);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `ensure_private_dir` tightens one that exists; it needs no chmod for a new one.
    /// Mutation: drop the `set_permissions`.
    #[test]
    fn ensure_private_dir_tightens_an_existing_directory() {
        let dir = Scratch::new("tighten");
        let state = dir.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&state).unwrap();
        assert_eq!(mode(&state), 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Each kind of path that is not a file is refused by what it is, without blocking, and a
    /// file opens. A FIFO with no reader would block a plain `File::open` for a read for ever.
    /// Mutation: open without `O_NONBLOCK` (this test hangs), or return the file whatever its
    /// type.
    #[test]
    fn only_a_regular_file_opens() {
        let dir = Scratch::new("regular");
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(open_regular(OpenOptions::new().read(true), &file).is_ok());

        let fifo = dir.join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(matches!(
            open_regular(OpenOptions::new().read(true), &fifo),
            Err(OpenRegularError::NotRegular(NonRegular::Fifo))
        ));
        assert!(matches!(
            open_regular(OpenOptions::new().read(true), &dir),
            Err(OpenRegularError::NotRegular(NonRegular::Directory))
        ));
        assert!(matches!(
            open_regular(OpenOptions::new().read(true), Path::new("/dev/null")),
            Err(OpenRegularError::NotRegular(NonRegular::CharDevice))
        ));
        assert!(matches!(
            open_regular(OpenOptions::new().read(true), &dir.join("absent")),
            Err(OpenRegularError::Io(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
