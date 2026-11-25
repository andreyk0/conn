//! What a session is doing right now.

use super::Instant;
use crate::util::redact::Redacted;

/// The live state of one session. Each variant carries exactly the facts that exist in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// The process is alive and stopped on something only the human can supply.
    Blocked { on: WaitingFor },
    /// The agent is working a turn.
    Running { since: Instant },
    /// The turn ended and background work is still going.
    Watching {
        ended_at: Instant,
        listing: ListingStatus,
    },
    /// The turn ended and nothing is going on.
    Idle {
        ended_at: Instant,
        listing: ListingStatus,
    },
    /// No process; the transcript is on disk and the session can be resumed. `background` is
    /// the state its supervisor reports when it still lists it as a background session, `None`
    /// when it is not listed or the listing could not be read: either way it resumes the same.
    Dormant {
        last_seen: Instant,
        background: Option<BgState>,
    },
    /// No process, and a complete walk of the projects directory found no transcript: there is
    /// nothing to resume. What is left is a record that names it, such as a background session
    /// its supervisor still lists, whose state is `background`.
    Gone { background: Option<BgState> },
    /// The state could not be determined; `reason` says why.
    Unknown { reason: Redacted<String> },
}

/// Whether the supervisor's listing was part of the evidence for a finished turn's activity.
///
/// A turn end is a card whether or not the listing could be read; what the listing would have
/// added (that the process is busy, or waiting) is missing when it is `Unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingStatus {
    Seen,
    Unavailable,
}

/// The kind of an [`Activity`], without its facts, declared in the order a session list puts
/// them: the one that most needs a look first. `Ord` is that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActivityKind {
    Blocked,
    Watching,
    Idle,
    Running,
    Unknown,
    Dormant,
    Gone,
}

impl ActivityKind {
    /// The word a page or a snapshot uses for it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Watching => "watching",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Dormant => "dormant",
            Self::Gone => "gone",
        }
    }

    /// Whether a process is known to be running the session: the kinds that come from a live
    /// process, not from the supervisor's record or the disk.
    pub const fn is_live(self) -> bool {
        match self {
            Self::Blocked | Self::Watching | Self::Idle | Self::Running => true,
            Self::Unknown | Self::Dormant | Self::Gone => false,
        }
    }
}

impl Activity {
    pub const fn kind(&self) -> ActivityKind {
        match self {
            Self::Blocked { .. } => ActivityKind::Blocked,
            Self::Watching { .. } => ActivityKind::Watching,
            Self::Idle { .. } => ActivityKind::Idle,
            Self::Running { .. } => ActivityKind::Running,
            Self::Unknown { .. } => ActivityKind::Unknown,
            Self::Dormant { .. } => ActivityKind::Dormant,
            Self::Gone { .. } => ActivityKind::Gone,
        }
    }
}

/// What a blocked session waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingFor {
    PermissionPrompt,
    InputNeeded,
    SandboxRequest,
    WorkerRequest,
    DialogOpen,
    /// The session is held until its usage quota resets; nothing the human does ends it.
    QuotaHold,
    /// A wait reason this code does not recognise, verbatim.
    Other(String),
}

/// The state a background session reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgState {
    Done,
    Failed,
    Stopped,
    Blocked,
    Working,
}

impl BgState {
    /// How a page or a snapshot says it after "background session".
    pub const fn words(self) -> &'static str {
        match self {
            Self::Done => "is done",
            Self::Failed => "failed",
            Self::Stopped => "is stopped",
            Self::Blocked => "is blocked",
            Self::Working => "is working",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order sessions are listed in is the declaration order of the kinds.
    /// Mutation: swap any two variants of `ActivityKind`.
    #[test]
    fn kinds_sort_most_urgent_first() {
        let mut kinds = [
            ActivityKind::Gone,
            ActivityKind::Dormant,
            ActivityKind::Unknown,
            ActivityKind::Running,
            ActivityKind::Idle,
            ActivityKind::Watching,
            ActivityKind::Blocked,
        ];
        kinds.sort();
        assert_eq!(
            kinds.iter().map(|k| k.name()).collect::<Vec<_>>(),
            [
                "blocked", "watching", "idle", "running", "unknown", "dormant", "gone"
            ]
        );
    }

    /// Mutation: report `Unknown`, `Dormant` or `Gone` as live.
    #[test]
    fn only_kinds_with_a_known_process_are_live() {
        let live: Vec<_> = [
            ActivityKind::Blocked,
            ActivityKind::Watching,
            ActivityKind::Idle,
            ActivityKind::Running,
            ActivityKind::Unknown,
            ActivityKind::Dormant,
            ActivityKind::Gone,
        ]
        .into_iter()
        .filter(|k| k.is_live())
        .map(ActivityKind::name)
        .collect();
        assert_eq!(live, ["blocked", "watching", "idle", "running"]);
    }
}
