//! Local markdown files a session mentions, and the directories the site may show them from.

use std::fmt;
use std::path::{Path, PathBuf};

/// The longest path accepted; Linux's `PATH_MAX`.
const PATH_BYTES_MAX: usize = 4096;

/// The extension of the only files the site shows.
const MARKDOWN_EXTENSION: &str = "md";

/// An absolute path to a markdown file, written plainly.
///
/// Witnesses the shape of the text alone: absolute, every component a name (no `.` or `..`),
/// ending in `.md`, with no control character, at most [`PATH_BYTES_MAX`] bytes. Whether the
/// file exists, where its symbolic links lead and whether it lies under a [`FileRoots`] are
/// questions for whoever reads it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalFile(PathBuf);

/// The text is not a plain absolute path to a markdown file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an absolute path to a .md file without . or .. components: {0:?}")]
pub struct LocalFileError(String);

impl LocalFile {
    pub fn parse(text: &str) -> Result<Self, LocalFileError> {
        let path = Path::new(text);
        let plain = is_plain_absolute(text)
            && text.len() <= PATH_BYTES_MAX
            && !text.chars().any(char::is_control)
            && is_markdown(path);
        if plain {
            Ok(Self(path.to_path_buf()))
        } else {
            Err(LocalFileError(text.to_owned()))
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The last component, which is how a person names the file.
    pub fn file_name(&self) -> String {
        self.0
            .file_name()
            .map_or_default(|name| name.to_string_lossy().into_owned())
    }
}

impl fmt::Display for LocalFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

/// Whether a path names a markdown file by its extension.
pub fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == MARKDOWN_EXTENSION)
}

/// The directories local files are shown from; empty shows none.
///
/// Witnesses that every root is absolute with no `.` or `..` component, so a path's lying
/// under one is a question of its leading components.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileRoots(Vec<PathBuf>);

/// A configured root is not a plain absolute path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an absolute path without . or .. components: {}", .0.display())]
pub struct FileRootError(pub PathBuf);

impl FileRoots {
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, FileRootError> {
        let plain = |root: &PathBuf| root.to_str().is_some_and(is_plain_absolute);
        match roots.iter().find(|root| !plain(root)) {
            Some(bad) => Err(FileRootError(bad.clone())),
            None => Ok(Self(roots)),
        }
    }

    pub fn as_slice(&self) -> &[PathBuf] {
        &self.0
    }

    /// Whether `path` lies under a root, by its components as written.
    pub fn contain(&self, path: &Path) -> bool {
        self.0.iter().any(|root| path.starts_with(root))
    }
}

/// `/` and then names: no empty, `.` or `..` segment, and no trailing `/`. Read from the text,
/// because `Path::components` drops a `.` and a trailing `/` without a trace.
fn is_plain_absolute(text: &str) -> bool {
    text.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_absolute_markdown_path_is_a_local_file() {
        for ok in [
            "/a.md",
            "/run/user/1000/x/scratchpad/plan-brief.md",
            "/h/a b/c.md",
        ] {
            assert_eq!(LocalFile::parse(ok).unwrap().path(), Path::new(ok));
        }
    }

    /// Mutation: drop the segment check (the `..`, `.`, `//` and trailing `/` rows), the
    /// extension check (the `.txt` and `.mdx` rows) or the absolute check (the relative row).
    #[test]
    fn anything_else_is_not() {
        for bad in [
            "a.md",
            "./a.md",
            "/a/../etc/passwd.md",
            "/a/./b.md",
            "/a//b.md",
            "/a.txt",
            "/a.mdx",
            "/a.md/",
            "/",
            "/a\u{7}.md",
            "",
        ] {
            assert!(LocalFile::parse(bad).is_err(), "accepted {bad:?}");
        }
        assert!(LocalFile::parse(&format!("/{}.md", "a".repeat(PATH_BYTES_MAX))).is_err());
    }

    /// Mutation: compare as strings (`/home/devil` would lie under `/home/dev`).
    #[test]
    fn roots_contain_by_component() {
        let roots = FileRoots::new(vec![PathBuf::from("/home/dev")]).unwrap();
        assert!(roots.contain(Path::new("/home/dev/a.md")));
        assert!(!roots.contain(Path::new("/home/devil/a.md")));
        assert!(!FileRoots::default().contain(Path::new("/a.md")));
    }

    #[test]
    fn a_root_must_be_plain_and_absolute() {
        for bad in ["home", "/home/../etc", "/home/./dev", "/home/dev/"] {
            assert!(FileRoots::new(vec![PathBuf::from(bad)]).is_err(), "{bad}");
        }
    }
}
