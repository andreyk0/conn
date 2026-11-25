//! The append-only event log written by `conn hook`, and the typed view of one entry.
//!
//! The log keeps the raw payload, so nothing the typed [`HookEvent`] drops (the prompt text
//! in particular) is lost to a later derivation and nothing is kept in the typed form that
//! must not be.
//!
//! Log line format (one JSON object per line, UTF-8, `\n`-terminated), exactly one of:
//! - `{"received_at": <RFC 3339 UTC>, "payload": <the hook JSON>}`
//! - `{"received_at": ..., "unparsed": "<stdin, lossily decoded>"}` when stdin was not JSON
//! - `{"received_at": ..., "oversized": <bytes>}` when stdin exceeded the cap

pub mod fold;
pub mod hook;
pub mod log;
pub mod parse;
pub mod settings;

#[cfg(test)]
mod testutil;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{AssistantText, BackgroundTasks, Instant, SessionId, TurnTrigger};
use crate::util::path::NonUtf8Path;

pub use fold::{PromptEvent, SessionEvents, StopEvent, fold, fold_into};
pub use hook::{LOG_FILE, run_hook};
pub use log::{EventLog, LogLineError, ReadBatch};

/// One line of the log: a hook payload exactly as Claude Code sent it, plus when it arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub received_at: Instant,
    pub payload: serde_json::Value,
}

/// The hook events this tool registers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookEventName {
    SessionStart,
    UserPromptSubmit,
    Stop,
}

impl HookEventName {
    pub const ALL: [Self; 3] = [Self::SessionStart, Self::UserPromptSubmit, Self::Stop];

    /// The name Claude Code uses in `hook_event_name` and in `settings.json`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
        }
    }
}

/// Why a session started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartSource {
    Startup,
    Resume,
    Clear,
    Compact,
    /// A source this code does not recognise, verbatim.
    Other(String),
}

/// A hook payload, parsed.
///
/// `UserPromptSubmit` carries no prompt text: the typed event is long-lived and shown on
/// pages, and the prompt is private. The raw log is the only place it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart {
        session: SessionId,
        at: Instant,
        source: StartSource,
    },
    UserPromptSubmit {
        session: SessionId,
        at: Instant,
        /// Who started the turn, classified from the prompt text at parse time.
        trigger: TurnTrigger,
    },
    Stop {
        session: SessionId,
        at: Instant,
        last_assistant_message: Option<AssistantText>,
        background_tasks: BackgroundTasks,
    },
    /// A payload for an event this tool does not handle, or one that does not name a
    /// known event. `session` is whatever the payload said, when it said something valid.
    Unrecognised {
        session: Option<SessionId>,
        event_name: String,
    },
}

/// Failures of the hook command and of rendering its registration.
///
/// Reading the log has its own error, [`LogLineError`], because one bad line must not hide
/// the good ones.
#[derive(Debug, thiserror::Error)]
pub enum EventsError {
    #[error("cannot read the hook payload from stdin: {0}")]
    Stdin(std::io::Error),
    #[error("cannot create state directory {}: {source}", path.display())]
    CreateStateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot append to event log {}: {source}", path.display())]
    Unwritable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The log path is something other than a file, so nothing was appended to it.
    #[error("event log {} is not a regular file ({kind}); nothing was logged", path.display())]
    NotRegular {
        path: PathBuf,
        kind: crate::util::fs::NonRegular,
    },
    #[error("cannot encode the event log line: {0}")]
    Encode(serde_json::Error),
    #[error("cannot render the hook registration: {0}")]
    Registration(#[from] NonUtf8Path),
}
