//! Saved sets of open sessions, to come back to after a crash or a forced reboot: one
//! automatic save that follows the open sessions, and up to [`KEPT_MAX`] kept ones made by
//! hand or set aside on a new boot.
//!
//! A save holds what resuming each session needs, taken while its process was alive: the
//! resume command carries launch flags only a live process can tell, so a command built after
//! the reboot would be weaker.
//!
//! [`auto`] decides when the automatic save changes, [`store`] keeps the saves on disk, and
//! [`system`] answers whether the machine is shutting down and which boot this is.

pub mod auto;
pub mod store;
pub mod system;

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{Model, ScanProgress, SessionView, SourceState};
use crate::types::{ActivityKind, Instant, SessionId};
use auto::Observation;

pub use store::{Saves, SavesError, SavesView};
pub use system::BootId;

/// The most kept saves; the next one pushes the oldest out.
pub const KEPT_MAX: usize = 10;

/// The open sessions `model` shows, when it was built from a complete read: the listing and
/// the registry read, every transcript scanned. `None` otherwise, since a partly read model
/// makes running sessions look stopped.
pub fn observation(model: &Model) -> Option<Observation> {
    let health = &model.health;
    let complete = matches!(health.listing, SourceState::Ok { .. })
        && matches!(health.registry, SourceState::Ok { .. })
        && health.transcripts.progress == ScanProgress::Complete;
    if !complete {
        return None;
    }
    let kind = |view: &&SessionView| view.session.activity.kind();
    Some(Observation {
        live: model
            .sessions
            .iter()
            .filter(|view| kind(view).is_live())
            .map(SavedSession::of)
            .collect(),
        unknown: model
            .sessions
            .iter()
            .filter(|view| kind(view) == ActivityKind::Unknown)
            .map(|view| view.session.id.clone())
            .collect(),
    })
}

/// `sessions` in the order a save lists them: the most recently active first.
pub fn most_recent_first(mut sessions: Vec<SavedSession>) -> Vec<SavedSession> {
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity));
    sessions
}

/// One session as a save holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: SessionId,
    /// The session's title as the pages showed it, redacted.
    pub title: Option<String>,
    /// The latest gist of the session, redacted.
    pub gist: Option<String>,
    pub cwd: Option<PathBuf>,
    pub last_activity: Option<Instant>,
    pub resume: Resume,
}

impl SavedSession {
    /// The session as `view` shows it now.
    pub fn of(view: &SessionView) -> Self {
        Self {
            id: view.session.id.clone(),
            title: view.title.as_ref().map(|title| title.as_str().to_owned()),
            gist: view.gist.as_ref().map(|gist| gist.as_str().to_owned()),
            cwd: view.session.cwd.clone(),
            last_activity: view.last_activity,
            resume: match &view.resume {
                Ok(command) => Resume::Command {
                    text: command.to_string(),
                    warnings: command.warnings().iter().map(ToString::to_string).collect(),
                },
                Err(why) => Resume::Unavailable {
                    why: why.to_string(),
                },
            },
        }
    }

    /// Whether the two say the same about resuming the session: the activity time and the
    /// gist move on their own and are not worth a write.
    fn resumes_as(&self, other: &Self) -> bool {
        self.id == other.id
            && self.title == other.title
            && self.cwd == other.cwd
            && self.resume == other.resume
    }
}

/// How a saved session is resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resume {
    /// The command to paste, and what it may get wrong.
    Command { text: String, warnings: Vec<String> },
    /// No command could be formed; `why` says why.
    Unavailable { why: String },
}

/// What made a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveKind {
    /// The automatic save, following the open sessions.
    Auto,
    /// The human's "Save now".
    Manual,
    /// The automatic save of an earlier boot, kept when this one started.
    BeforeReboot,
}

/// A saved set of sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Save {
    pub kind: SaveKind,
    pub at: Instant,
    /// The boot the save was made in; `None` when it could not be read.
    pub boot: Option<BootId>,
    /// The most recently active first.
    pub sessions: Vec<SavedSession>,
}

/// Which save: the automatic one, or a kept one by the millisecond it was made in.
///
/// Witnesses the only forms a save's file name and URL take, so a path built from one never
/// leaves the saves directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SaveId {
    Auto,
    /// Milliseconds since the epoch, never negative.
    Kept(i64),
}

/// The text was not a save's id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a save: {0:?}")]
pub struct SaveIdError(String);

impl SaveId {
    const AUTO: &'static str = "auto";

    pub fn parse(text: &str) -> Result<Self, SaveIdError> {
        if text == Self::AUTO {
            return Ok(Self::Auto);
        }
        let digits = !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
        digits
            .then(|| text.parse::<i64>().ok())
            .flatten()
            .map(Self::Kept)
            .ok_or_else(|| SaveIdError(text.to_owned()))
    }
}

impl fmt::Display for SaveId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str(Self::AUTO),
            Self::Kept(millis) => write!(f, "{millis}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutation: accept a sign, a path separator or an empty text.
    #[test]
    fn a_save_id_is_auto_or_digits_and_reads_back() {
        assert_eq!(SaveId::parse("auto"), Ok(SaveId::Auto));
        assert_eq!(
            SaveId::parse("1700000000123"),
            Ok(SaveId::Kept(1_700_000_000_123))
        );
        for bad in [
            "",
            "+12",
            "-1",
            "../auto",
            "12/3",
            "Auto",
            "1e3",
            "99999999999999999999999",
        ] {
            assert!(SaveId::parse(bad).is_err(), "{bad}");
        }
        for id in [SaveId::Auto, SaveId::Kept(42)] {
            assert_eq!(SaveId::parse(&id.to_string()), Ok(id));
        }
    }
}
