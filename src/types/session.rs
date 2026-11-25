//! A Claude Code session as the tool sees it.

use std::path::PathBuf;

use super::{Activity, SessionId};

/// How the session was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Interactive,
    Background,
    Headless,
}

/// One session, live or dormant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    /// Where the session works; absent when neither a process nor the transcript says.
    pub cwd: Option<PathBuf>,
    /// The topic Claude Code derived for the session. Pages show
    /// [`crate::model::SessionView::title`], which is redacted.
    pub ai_title: Option<String>,
    /// The title the session's terminal shows, as written: its window is found by it. A
    /// custom title (`-n`, `/rename`, a hook) comes before the AI title, as in Claude Code.
    pub window_title: Option<String>,
    /// The name the human gave the session.
    pub name: Option<String>,
    pub kind: SessionKind,
    pub activity: Activity,
}

impl Session {
    /// A live process runs the session in a terminal: its window can be raised, and resuming
    /// it elsewhere would run it twice.
    pub const fn runs_in_terminal(&self) -> bool {
        self.activity.kind().is_live() && matches!(self.kind, SessionKind::Interactive)
    }
}
