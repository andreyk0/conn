//! Paths that must become text.

use std::path::{Path, PathBuf};

/// A path that cannot be written into a shell command or a JSON document without losing bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("path is not valid UTF-8: {}", .0.display())]
pub struct NonUtf8Path(pub PathBuf);

/// The path as text, or the refusal to render it lossily.
pub fn utf8(path: &Path) -> Result<&str, NonUtf8Path> {
    path.to_str().ok_or_else(|| NonUtf8Path(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn utf8_path_passes_through() {
        assert_eq!(utf8(Path::new("/home/dev/proj")), Ok("/home/dev/proj"));
    }

    #[test]
    fn non_utf8_path_is_refused() {
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(utf8(bad), Err(NonUtf8Path(bad.to_path_buf())));
    }
}
