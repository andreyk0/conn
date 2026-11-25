//! What the poller has read, kept as the builder needs it.
//!
//! Every source is either read, failed or not yet read; the builder shows that, and no source
//! is ever replaced by a guess.

use std::collections::HashMap;

use jiff::SignedDuration;

use crate::events::SessionEvents;
use crate::model::SourceState;
use crate::sessions::launch::LaunchArgs;
use crate::sessions::listing::ListedSession;
use crate::sessions::registry::{Liveness, RegistryEntry};
use crate::sessions::transcripts::TranscriptFile;
use crate::transcript::{Summary, Tailer};
use crate::types::{Instant, SessionId};

/// A source read as a whole: the latest attempt, whatever it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source<T> {
    NeverRead,
    Read { at: Instant, value: T },
    Failed { at: Instant, reason: String },
}

impl<T> Source<T> {
    pub fn state(&self) -> SourceState {
        match self {
            Self::NeverRead => SourceState::NeverRead,
            Self::Read { at, .. } => SourceState::Ok { at: *at },
            Self::Failed { at, reason } => SourceState::Failed {
                at: *at,
                reason: reason.clone(),
            },
        }
    }

    /// The value, when the latest read succeeded.
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Read { value, .. } => Some(value),
            Self::NeverRead | Self::Failed { .. } => None,
        }
    }
}

/// How long a failed listing attempt is covered by the last good listing.
///
/// The listing is read every 10 s with a 10 s timeout per attempt. Two missed polls are 20 s,
/// and the attempt in flight can take its full timeout on top: 30 s. The window doubles that
/// as slack, so a third miss does not blank the inbox, while a listing a minute old is no
/// longer taken for the present.
pub const LISTING_COVER: SignedDuration = SignedDuration::from_secs(60);

/// The latest listing that was read completely, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoodListing {
    pub at: Instant,
    pub sessions: Vec<ListedSession>,
}

/// The listing the builder classifies from, and the health it reports for it.
///
/// The two differ only while a failed attempt is covered by the last good listing: the
/// sessions are classified from that listing and the health still says the attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingForModel {
    pub used: Source<Vec<ListedSession>>,
    pub health: SourceState,
}

impl ListingForModel {
    /// The listing to build the model at `now` from the `latest` attempt and the `last_good`
    /// one. A failure within [`LISTING_COVER`] of the last good listing keeps its sessions and
    /// says how old they are; past it the failure stands alone and every live session is
    /// `Unknown`.
    pub fn at(
        latest: &Source<Vec<ListedSession>>,
        last_good: Option<&GoodListing>,
        now: Instant,
    ) -> Self {
        match (latest, last_good) {
            (Source::Failed { at, reason }, Some(good))
                if now.duration_since(good.at) <= LISTING_COVER =>
            {
                Self {
                    used: Source::Read {
                        at: good.at,
                        value: good.sessions.clone(),
                    },
                    health: SourceState::Failed {
                        at: *at,
                        reason: format!(
                            "{reason}; showing the listing read {} s ago",
                            now.duration_since(good.at).as_secs()
                        ),
                    },
                }
            }
            (Source::Failed { .. } | Source::Read { .. } | Source::NeverRead, _) => Self {
                used: latest.clone(),
                health: latest.state(),
            },
        }
    }
}

/// The registry as read at one moment: each entry with what `/proc` says about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegistrySnapshot {
    pub entries: Vec<RegisteredEntry>,
    /// Entries that could not be read, one line each. The registry counts as partly failed
    /// while there are any.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredEntry {
    pub entry: RegistryEntry,
    pub liveness: Liveness,
    pub launch: LaunchArgs,
}

/// The hook-event log folded so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLogState {
    pub sessions: HashMap<SessionId, SessionEvents>,
    pub health: SourceState,
    /// Lines that carried no usable event, over the whole log.
    pub line_errors: u64,
}

impl EventLogState {
    pub fn unread() -> Self {
        Self {
            sessions: HashMap::new(),
            health: SourceState::NeverRead,
            line_errors: 0,
        }
    }
}

/// A transcript the poller follows.
#[derive(Debug)]
pub struct Tracked {
    pub file: TranscriptFile,
    pub state: TranscriptState,
    /// Present while the file is recent enough to follow.
    pub tailer: Option<Tailer>,
    /// The latest tail failed; the summary may be missing what was appended since.
    pub tail_fault: Option<TailFault>,
}

/// A failed read of the end of a followed transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailFault {
    pub at: Instant,
    pub reason: String,
}

#[derive(Debug)]
pub enum TranscriptState {
    /// Waiting for, or in, its first scan.
    Queued,
    Scanned(Box<Summary>),
    /// The first scan could not read the file.
    Unreadable(String),
}

impl TranscriptState {
    pub fn summary(&self) -> Option<&Summary> {
        match self {
            Self::Scanned(summary) => Some(summary),
            Self::Queued | Self::Unreadable(_) => None,
        }
    }
}

/// Every transcript discovered, and what discovery passed over.
#[derive(Debug, Default)]
pub struct Transcripts {
    pub tracked: HashMap<SessionId, Tracked>,
    pub skipped: usize,
    pub discovery: Option<SourceState>,
}
