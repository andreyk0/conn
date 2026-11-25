//! The shell command that resumes a session.

use std::fmt;
use std::path::{Path, PathBuf};

use super::SessionId;
use crate::util::path::{NonUtf8Path, utf8};
use crate::util::shell::quote;

/// A `claude` flag worth carrying across a resume.
///
/// A resume restores the model, agent and permission mode by itself, so those never appear
/// here; bypass mode, the browser, added directories and the launch-time configuration that
/// no transcript records do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeFlag {
    SkipPermissions,
    AllowSkipPermissions,
    Chrome,
    AddDir(PathBuf),
    /// Any other launch flag worth repeating, as the words it was launched with: the flag
    /// (with an inline `=value` if it had one) followed by its separate values.
    Launch(Vec<String>),
}

/// A reason to doubt that a [`ResumeCommand`] restores everything the session had.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResumeWarning {
    /// The session's launch arguments could not be read, so flags that no transcript records
    /// (system prompts, tool and MCP restrictions, settings, plugins) may be missing.
    #[error(
        "launch-time flags (system prompts, tool and MCP restrictions, settings, plugins) are unknown, because {reason}"
    )]
    LaunchFlagsUnknown { reason: String },
    /// No permission mode was recorded, so the launch flags decided about bypass mode.
    #[error("the transcript records no permission mode, so bypass mode follows the launch flags")]
    PermissionModeFollowsLaunch,
    /// No permission mode was recorded and no launch flags are known.
    #[error("the permission mode is unknown, so bypass mode is not requested")]
    PermissionModeUnknown,
    /// Launch flags with no rule: left off, and not known to be restored by a resume.
    #[error("not carried, and not known to be restored by a resume: {}", .0.join(", "))]
    UnclassifiedFlags(Vec<String>),
}

/// Why no resume command could be formed for a session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResumeUnavailable {
    #[error("the session's working directory is unknown")]
    NoWorkingDirectory,
    #[error(transparent)]
    NonUtf8(#[from] NonUtf8Path),
}

/// `cd <dir> && claude <flags> --resume <id>`, ready to paste.
///
/// Witnesses that every path in it is valid UTF-8, so [`fmt::Display`] is lossless and
/// the printed command means the bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    dir: PathBuf,
    flags: Vec<ResumeFlag>,
    session: SessionId,
    warnings: Vec<ResumeWarning>,
}

impl ResumeCommand {
    pub fn new(
        dir: PathBuf,
        flags: Vec<ResumeFlag>,
        session: SessionId,
    ) -> Result<Self, NonUtf8Path> {
        utf8(&dir)?;
        for flag in &flags {
            if let ResumeFlag::AddDir(extra) = flag {
                utf8(extra)?;
            }
        }
        Ok(Self {
            dir,
            flags,
            session,
            warnings: Vec::new(),
        })
    }

    /// The same command with the reasons it may fall short of the original session.
    #[must_use]
    pub fn with_warnings(self, warnings: Vec<ResumeWarning>) -> Self {
        Self { warnings, ..self }
    }

    pub fn flags(&self) -> &[ResumeFlag] {
        &self.flags
    }

    pub fn warnings(&self) -> &[ResumeWarning] {
        &self.warnings
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub const fn session(&self) -> &SessionId {
        &self.session
    }
}

/// The text of a path already proven UTF-8 by [`ResumeCommand::new`].
fn quoted(path: &Path) -> String {
    quote(&path.to_string_lossy()).into_owned()
}

impl fmt::Display for ResumeFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SkipPermissions => f.write_str("--dangerously-skip-permissions"),
            Self::AllowSkipPermissions => f.write_str("--allow-dangerously-skip-permissions"),
            Self::Chrome => f.write_str("--chrome"),
            Self::AddDir(dir) => write!(f, "--add-dir {}", quoted(dir)),
            Self::Launch(words) => {
                let mut words = words.iter().map(|w| quote(w));
                if let Some(first) = words.next() {
                    write!(f, "{first}")?;
                }
                words.try_for_each(|w| write!(f, " {w}"))
            }
        }
    }
}

impl fmt::Display for ResumeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cd {} && claude", quoted(&self.dir))?;
        for flag in &self.flags {
            write!(f, " {flag}")?;
        }
        write!(f, " --resume {}", self.session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    fn id() -> SessionId {
        SessionId::parse(ID).unwrap()
    }

    #[test]
    fn renders_without_flags() {
        let cmd = ResumeCommand::new("/home/dev/proj".into(), vec![], id()).unwrap();
        assert_eq!(
            cmd.to_string(),
            "cd /home/dev/proj && claude --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
        );
    }

    #[test]
    fn renders_flags_in_order_and_quotes_paths() {
        let cmd = ResumeCommand::new(
            "/home/dev/my proj".into(),
            vec![
                ResumeFlag::SkipPermissions,
                ResumeFlag::Chrome,
                ResumeFlag::AddDir("/tmp/it's here".into()),
            ],
            id(),
        )
        .unwrap();
        assert_eq!(
            cmd.to_string(),
            "cd '/home/dev/my proj' && claude --dangerously-skip-permissions --chrome \
             --add-dir '/tmp/it'\\''s here' --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
        );
    }

    #[test]
    fn allow_skip_permissions_is_its_own_flag() {
        let cmd =
            ResumeCommand::new("/p".into(), vec![ResumeFlag::AllowSkipPermissions], id()).unwrap();
        assert_eq!(
            cmd.to_string(),
            "cd /p && claude --allow-dangerously-skip-permissions --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
        );
    }

    /// Mutation: write the words unquoted -> the prompt with a space and a quote breaks -> red.
    #[test]
    fn launch_flags_render_their_words_quoted() {
        let cmd = ResumeCommand::new(
            "/p".into(),
            vec![
                ResumeFlag::Launch(vec!["--strict-mcp-config".into()]),
                ResumeFlag::Launch(vec!["--system-prompt".into(), "be brief, it's late".into()]),
            ],
            id(),
        )
        .unwrap();
        assert_eq!(
            cmd.to_string(),
            "cd /p && claude --strict-mcp-config --system-prompt 'be brief, it'\\''s late' \
             --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
        );
    }

    #[test]
    fn warnings_do_not_change_the_command_text() {
        let plain = ResumeCommand::new("/p".into(), vec![], id()).unwrap();
        let warned = plain
            .clone()
            .with_warnings(vec![ResumeWarning::PermissionModeUnknown]);
        assert_eq!(plain.to_string(), warned.to_string());
        assert_eq!(warned.warnings(), [ResumeWarning::PermissionModeUnknown]);
    }

    #[test]
    fn refuses_a_path_it_cannot_print_faithfully() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert!(ResumeCommand::new(bad.clone(), vec![], id()).is_err());
        assert!(ResumeCommand::new("/p".into(), vec![ResumeFlag::AddDir(bad)], id()).is_err());
    }
}
