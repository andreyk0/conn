//! The read model the pages render: everything the tool knows, derived, at one moment.
//!
//! Nothing here is a store. The poller ([`crate::state`]) rebuilds the whole model from the
//! event log, the transcripts, the supervisor's listing, the process table and `gh`, and
//! swaps it in; the only file the model reads back that the user's actions wrote is
//! `seen.json`.
//!
//! **Calm is never shown by default.** An empty [`Model::inbox`] means nothing is pending
//! only if [`Health`] says every source was read: a source that failed is
//! [`SourceState::Failed`], a source not yet read is [`SourceState::NeverRead`], a file nothing
//! has written is [`SourceState::NeverWritten`], and a transcript set still being scanned is
//! [`ScanProgress::InProgress`]. A page that shows an empty inbox while any of these holds shows
//! calm it has not observed.
//!
//! **A blocked session is a card.** A session stopped on a question, a permission prompt or any
//! other wait the harness owns has a [`PendingCard::Blocked`] or [`PendingCard::Asked`] card, and
//! the inbox ranks it first.

pub mod followups;
mod reviews;
mod search;
mod work_items;

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

pub use reviews::{
    ApprovalRow, CardFlag, CardVerdict, Reaction, ReviewCard, ReviewsView, approvals,
    source_state as reviews_state,
};
pub use search::{
    CONTEXT_CHARS, Found, Hit, Matched, QUERY_CHARS_MAX, Query, QueryError, RESULTS_MAX, Results,
    Snippet, search,
};
pub use work_items::{
    Blocker, Children, CommentMarks, DESCRIPTION_CHARS, Entry, EpicRef, Head, ItemRelations,
    RowKind, StatusSection, Teaser, WorkItemRow, WorkItemsFailure, WorkItemsFault, WorkItemsView,
    source_state as work_items_state,
};

use crate::gh::Reading;
use crate::gist::{Ask, GistView};
use crate::ledger::Slug;
use crate::map::WorkMap;
use crate::types::{
    ActivityKind, Instant, Link, PendingCard, ResumeCommand, ResumeUnavailable, SafeUrl, Session,
    SessionId, TurnEnd, TurnTrigger, WaitingFor,
};
use crate::util::redact::Redacted;

/// The model at one moment.
#[derive(Debug, Clone)]
pub struct Model {
    pub generated_at: Instant,
    pub health: Health,
    /// One entry per pending card, the one to look at first first.
    pub inbox: Vec<InboxEntry>,
    /// Live, parked and unclassifiable sessions, the most urgent first.
    pub sessions: Vec<SessionView>,
    /// Sessions with a transcript and no process, the most recent first.
    pub dormant: Vec<SessionView>,
    /// Sessions whose activity is `Unknown`, which no page may leave out of its summary: a
    /// session this code could not classify is not calm.
    pub unknown_sessions: u32,
    /// The human prompts each session holds, oldest first, for search; a session with none
    /// has no entry.
    pub prompts: HashMap<SessionId, Arc<[HeldPrompt]>>,
    /// The user's open Jira work items, joined to the sessions that mention them.
    pub work_items: WorkItemsView,
    /// Pull requests someone asked the user to approve; they are cards in the inbox.
    pub reviews: ReviewsView,
    /// The approvals made from review cards, newest first; `None` where no channel is
    /// configured.
    pub approvals: Option<Vec<ApprovalRow>>,
    /// The sessions of the last week with what each is doing, and where the tickets and pull
    /// requests they link stand.
    pub map: WorkMap,
    /// The plan's rate-limit windows. Their failure is not something the inbox's calm depends
    /// on, so the header does not list it; the gauges say it themselves.
    pub usage: crate::usage::Reading,
    /// Who owns which ticket, rollout and topic, and what each session waits for, as the
    /// sessions declared it.
    pub coordination: Coordination,
    /// What is due and who should act on it; off without the config's `[followups]`.
    pub followups: followups::FollowUpsView,
    /// The saved sets of open sessions, to resume after a crash or a reboot.
    pub saves: crate::saves::SavesView,
    /// The PAM entitlements and grants as last read; `None` without the config's `[pam]`.
    /// Their failures are the PAM page's own, like the usage gauges'.
    pub pam: Option<crate::pam::Reading>,
}

/// The coordination ledger as last read. Its failure is shown on its own page and the health
/// page; the inbox's calm does not depend on it, since no card comes from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordination {
    pub state: SourceState,
    /// The latest good read; empty before one.
    pub ledger: crate::ledger::Ledger,
    /// Lines of the file that held no entry.
    pub bad_lines: Vec<crate::ledger::store::BadLine>,
    /// What the transcripts say slipped: tickets and threads several sessions wrote to, or a
    /// non-owner did, over [`SHARED_WINDOW`]; the most recent first.
    pub shared: Vec<crate::ledger::shared::Shared>,
    /// The open dependencies of the map's tickets and of the work items with a blocking link,
    /// read from their Jira links; the map draws those with an end on it.
    pub dependencies: Vec<crate::jira::links::Dependency>,
    /// Where those reads stand; `None` where no Jira is configured.
    pub links: Option<SourceState>,
}

/// How far back the writes of [`Coordination::shared`] are compared: the week the map shows.
pub const SHARED_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_hours(7 * 24);

impl Coordination {
    pub fn unread() -> Self {
        Self {
            state: SourceState::NeverRead,
            ledger: crate::ledger::Ledger::default(),
            bad_lines: Vec::new(),
            shared: Vec::new(),
            dependencies: Vec::new(),
            links: None,
        }
    }
}

/// A prompt the human typed, redacted where the model took it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldPrompt {
    pub at: Instant,
    pub text: Redacted<String>,
}

/// Where every source stands. Each failure is here with its time; none is only logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// `claude agents --json --all`.
    pub listing: SourceState,
    /// `~/.claude/sessions` and the process table.
    pub registry: SourceState,
    pub event_log: SourceState,
    /// Log lines that carried no usable event, over the whole log.
    pub event_log_line_errors: u64,
    pub transcripts: Scanned,
    /// Followed transcripts whose latest read failed.
    pub follow_faults: FollowFaults,
    pub gh: GhHealth,
    /// `seen.json`; a corrupt file is `Failed` and is never replaced.
    pub seen: SourceState,
    /// Links that were extracted but could not be rendered as a URL.
    pub links_unrenderable: u32,
    /// Sessions the listing shows that have no session id yet (just launched).
    pub unidentified_listed: u32,
    /// The desktop notification when cards need the human.
    pub notifications: Notifications,
    /// `acli jira workitem search`, the source of the work items page; `None` where there is
    /// no Jira. Its failure is not something the inbox's calm depends on, so the header does
    /// not list it.
    pub work_items: Option<SourceState>,
    /// `acli jira workitem search` and `view`, the source of the comment badges; `None` where
    /// there is no Jira.
    pub work_item_comments: Option<SourceState>,
    /// `acli jira workitem search` of the work items with a blocking link, whose links are
    /// then read; `None` where there is no Jira.
    pub work_items_linked: Option<SourceState>,
    /// `acli jira workitem search` of each open epic's children; `None` where there is no Jira.
    pub epic_children: Option<SourceState>,
    /// The Slack channels and `gh` reads behind the review cards; `None` where no channel is
    /// configured.
    pub reviews: Option<SourceState>,
    /// The runs that write the cards' gists, and `gists.json`.
    pub gists: SourceState,
}

/// The followed transcripts that could not be read on their latest poll: their sessions are
/// `Unknown`, since what was appended since is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowFaults {
    /// How many transcripts are at fault now.
    pub transcripts: usize,
    /// The most recent failure among them.
    pub latest: Option<FollowFault>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowFault {
    pub at: Instant,
    pub reason: String,
}

/// What sending a desktop notification has done so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notifications {
    /// None has been needed yet.
    Untried,
    /// The latest one was sent at `at`.
    Sent { at: Instant },
    /// `notify-send` is not installed; no further attempt is made.
    Missing { at: Instant },
    /// The latest attempt failed at `at`.
    Failed { at: Instant, reason: String },
}

/// What reading one source last did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// Read completely at `at`.
    Ok { at: Instant },
    /// The latest read failed, or was partial, at `at`.
    Failed { at: Instant, reason: String },
    /// The file at `path` does not exist as of `at`: nothing has written it yet. A source whose
    /// writer has not run is not broken, and is still not read.
    NeverWritten { at: Instant, path: PathBuf },
    /// Not read yet.
    NeverRead,
}

/// What the transcript scan has covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    /// Transcripts discovered on disk.
    pub files: usize,
    /// Entries of the projects directory passed over, with a reason each.
    pub skipped: usize,
    /// Lines of all transcripts that yielded no record.
    pub line_errors: u64,
    /// Transcripts that could not be read; their sessions are `Unknown`.
    pub unreadable: usize,
    /// Whether the projects directory could be walked.
    pub discovery: SourceState,
    pub progress: ScanProgress,
}

/// Whether the transcripts discovered have all been read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanProgress {
    Complete,
    /// Pages say "still scanning", never an empty inbox.
    InProgress {
        scanned: usize,
        total: usize,
    },
}

/// How current the pull-request states in the links are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GhHealth {
    /// The latest read succeeded with a state.
    pub fresh: usize,
    /// The latest read succeeded: GitHub has no such pull request, or the account cannot see
    /// it. An answer, so it never stops a page from saying calm.
    pub not_found: usize,
    /// A good value is shown beside a later failed read.
    pub stale: usize,
    /// No read has ever succeeded.
    pub failed: usize,
    /// Not asked yet.
    pub unread: usize,
}

/// A session with everything a page shows about it.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: Session,
    /// The name the human gave the session, else the title Claude Code derived.
    pub title: Option<Redacted<String>>,
    pub pending: Option<PendingCard>,
    pub links: Vec<LinkView>,
    /// Absent when the command cannot be formed; the reason is the error.
    pub resume: Result<ResumeCommand, ResumeUnavailable>,
    /// Newest first, at most [`LAST_TURN_ENDS`].
    pub last_turn_ends: Vec<TurnEnd>,
    /// The latest sign of life in any source.
    pub last_activity: Option<Instant>,
    pub pin: Pin,
    /// The latest gist written for the session, whichever of its cards it was for.
    pub gist: Option<Redacted<String>>,
}

/// Whether the human pinned the session. Pinned sessions sort first, in the order they were
/// pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pin {
    Pinned(PinRank),
    Unpinned,
}

/// A pin's place in the order pins were made: the earliest pin is rank 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PinRank(usize);

impl PinRank {
    /// The pin at `index` in pin order.
    pub const fn at(index: usize) -> Self {
        Self(index)
    }
}

/// How many turn ends a [`SessionView`] keeps.
pub const LAST_TURN_ENDS: NonZeroUsize = NonZeroUsize::new(5).unwrap();

/// A link a card shows: where it goes, and what GitHub says about it when it is a pull request.
#[derive(Debug, Clone)]
pub struct LinkView {
    pub link: Link,
    pub url: SafeUrl,
    pub pr: Option<Reading>,
}

/// Whether the human has marked a card's session seen since the card appeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Seen {
    Unseen,
    Seen,
}

/// How urgently a card asks for a look: lower sorts first.
///
/// A blocked session comes before any finished turn, a quota hold after every other blocked
/// one; of finished turns, one the human
/// started comes before one a background task's notification started, and both before any
/// other trigger. The trigger ranks and never filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Blocked,
    /// A wait for the usage quota to reset: blocked, but nothing the human does ends it.
    QuotaHold,
    Human,
    TaskNotification,
    Other,
}

/// The sort key of an inbox entry: its [`Tier`], then the oldest `since` first.
///
/// Built only by [`Rank::of`], from the card, so a rank always matches its card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    tier: Tier,
    since: Instant,
}

/// Where a card's session stands in the coordination ledger, for the inbox's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitStanding {
    /// It declared no wait.
    None,
    /// It waits on something that has not come.
    Waiting(Waiting),
    /// What it waited on has come: it has something to do.
    Over,
}

/// What a session waits on while the wait lasts, as its own `conn wait` declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The next finish of a rollout: a release, the user's to run and often not today.
    Release { rollout: Slug, run: ReleaseRun },
    /// Another session's ticket or topic, a time, or someone, in the ledger's words.
    Other { on: String },
}

/// Whether the release a session waits on is being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseRun {
    /// Nobody runs it: the work is held until someone does.
    NotStarted,
    Running,
}

/// What a card needs from the human now, which orders the inbox before [`Rank`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Band {
    /// Blocked on the human, or a finished turn whose session's wait is over.
    Now,
    Normal,
    /// A finished turn of a session that waits on something other than the human now.
    Waiting,
}

/// A card's [`Band`], with what its session waits on when that is what places it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    Now,
    Normal,
    Waiting(Waiting),
}

impl Standing {
    /// A blocked card needs the human whatever the session waits on; a wait moves only a
    /// finished turn, up when it is over and down while it lasts.
    pub fn of(rank: Rank, wait: WaitStanding) -> Self {
        match (rank.tier, wait) {
            (Tier::Blocked, _)
            | (Tier::Human | Tier::TaskNotification | Tier::Other, WaitStanding::Over) => Self::Now,
            (Tier::QuotaHold, _)
            | (Tier::Human | Tier::TaskNotification | Tier::Other, WaitStanding::None) => {
                Self::Normal
            }
            (Tier::Human | Tier::TaskNotification | Tier::Other, WaitStanding::Waiting(on)) => {
                Self::Waiting(on)
            }
        }
    }

    pub const fn band(&self) -> Band {
        match self {
            Self::Now => Band::Now,
            Self::Normal => Band::Normal,
            Self::Waiting(_) => Band::Waiting,
        }
    }

    /// What the session waits on, when its card is placed by the wait.
    pub const fn waiting(&self) -> Option<&Waiting> {
        match self {
            Self::Waiting(on) => Some(on),
            Self::Now | Self::Normal => None,
        }
    }
}

impl Rank {
    pub fn of(card: &PendingCard) -> Self {
        match card {
            PendingCard::Blocked {
                on: WaitingFor::QuotaHold,
                since,
                ..
            } => Self {
                tier: Tier::QuotaHold,
                since: *since,
            },
            PendingCard::Asked { since, .. } | PendingCard::Blocked { since, .. } => Self {
                tier: Tier::Blocked,
                since: *since,
            },
            PendingCard::Turn { turn, since, .. } => Self {
                tier: match turn.trigger {
                    TurnTrigger::Human => Tier::Human,
                    TurnTrigger::TaskNotification => Tier::TaskNotification,
                    TurnTrigger::Peer
                    | TurnTrigger::Scheduled
                    | TurnTrigger::Coordinator
                    | TurnTrigger::AutoContinuation
                    | TurnTrigger::LocalCommand
                    | TurnTrigger::CompactionSummary
                    | TurnTrigger::Unknown(_) => Tier::Other,
                },
                since: *since,
            },
        }
    }

    pub const fn tier(&self) -> Tier {
        self.tier
    }

    pub const fn since(&self) -> Instant {
        self.since
    }
}

/// One card in the inbox.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub session: SessionId,
    pub title: Option<Redacted<String>>,
    pub card: PendingCard,
    pub rank: Rank,
    /// Set from the ledger when the inbox is ordered; [`Standing::Normal`] before.
    pub standing: Standing,
    pub seen: Seen,
    /// The turn end before the pending one (for a question: the latest turn end), redacted
    /// and then cut to [`NOW_LINE_CHARS`], as context for what the card is about.
    pub now_line: Option<Redacted<String>>,
    pub links: Vec<LinkView>,
    /// What the session is about, as its gist says.
    pub gist: GistView,
    /// What the card waits for; `None` until the card or its gist says.
    pub ask: Option<Ask>,
}

/// How much of the earlier turn end the inbox shows.
pub const NOW_LINE_CHARS: usize = 200;

impl Model {
    /// The model before anything has been read: every source `NeverRead`, the scan not
    /// begun. It shows no calm, because [`Health`] says nothing was observed.
    pub fn unread(now: Instant) -> Self {
        Self {
            generated_at: now,
            health: Health {
                listing: SourceState::NeverRead,
                registry: SourceState::NeverRead,
                event_log: SourceState::NeverRead,
                event_log_line_errors: 0,
                transcripts: Scanned {
                    files: 0,
                    skipped: 0,
                    line_errors: 0,
                    unreadable: 0,
                    discovery: SourceState::NeverRead,
                    progress: ScanProgress::InProgress {
                        scanned: 0,
                        total: 0,
                    },
                },
                gh: GhHealth::default(),
                seen: SourceState::NeverRead,
                links_unrenderable: 0,
                unidentified_listed: 0,
                notifications: Notifications::Untried,
                follow_faults: FollowFaults {
                    transcripts: 0,
                    latest: None,
                },
                work_items: Some(SourceState::NeverRead),
                work_item_comments: None,
                work_items_linked: None,
                epic_children: None,
                reviews: None,
                gists: SourceState::NeverRead,
            },
            inbox: Vec::new(),
            sessions: Vec::new(),
            dormant: Vec::new(),
            unknown_sessions: 0,
            prompts: HashMap::new(),
            work_items: WorkItemsView::NeverRead,
            reviews: ReviewsView::Off,
            approvals: None,
            map: WorkMap::default(),
            usage: crate::usage::Reading::default(),
            coordination: Coordination::unread(),
            followups: followups::FollowUpsView::Off,
            saves: crate::saves::SavesView::default(),
            pam: None,
        }
    }

    pub fn session(&self, id: &SessionId) -> Option<&SessionView> {
        self.sessions
            .iter()
            .chain(&self.dormant)
            .find(|view| view.session.id == *id)
    }

    /// Sessions by the kind of their activity, most urgent kind first, for a page's counts.
    pub fn activity_counts(&self) -> BTreeMap<ActivityKind, usize> {
        let mut counts = BTreeMap::new();
        for view in self.sessions.iter().chain(&self.dormant) {
            *counts.entry(view.session.activity.kind()).or_insert(0) += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    //! The inbox order, with hand-picked instants: blocked first; then `Human`, then
    //! `TaskNotification`, then everything else; then oldest first; and equal ranks tie.

    use super::*;
    use crate::testutil::{asked as the_question, secs as at, session_id as id};
    use crate::types::{AssistantText, BackgroundTasks};

    fn turn(n: u8, trigger: TurnTrigger, secs: i64) -> PendingCard {
        PendingCard::Turn {
            session: id(n),
            turn: TurnEnd {
                at: at(secs),
                trigger,
                text: AssistantText::of(""),
                background_tasks: BackgroundTasks::Unknown,
                asked: None,
            },
            since: at(secs),
        }
    }

    fn asked(n: u8, secs: i64) -> PendingCard {
        PendingCard::Asked {
            session: id(n),
            asked: the_question(),
            since: at(secs),
        }
    }

    /// Mutation: swap `Human` and `TaskNotification` in `Tier`, put `Blocked` last, or sort
    /// `since` descending.
    #[test]
    fn ranks_order_blocked_then_trigger_then_oldest() {
        let cards = [
            turn(1, TurnTrigger::Scheduled, 10),
            turn(2, TurnTrigger::TaskNotification, 20),
            turn(3, TurnTrigger::Human, 300),
            asked(4, 400),
            turn(5, TurnTrigger::Human, 100),
            turn(6, TurnTrigger::Unknown("x".into()), 5),
            turn(7, TurnTrigger::TaskNotification, 15),
        ];
        let mut ranked: Vec<_> = cards.iter().map(|c| (Rank::of(c), c)).collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        let order: Vec<u8> = ranked
            .iter()
            .map(|(_, card)| match card {
                PendingCard::Asked { session, .. }
                | PendingCard::Blocked { session, .. }
                | PendingCard::Turn { session, .. } => session
                    .as_str()
                    .chars()
                    .last()
                    .and_then(|c| c.to_digit(16))
                    .map_or(0, |d| u8::try_from(d).unwrap_or(0)),
            })
            .collect();
        // blocked(4); human oldest first (5, 3); notifications oldest first (7, 2); others
        // oldest first (6, 1).
        assert_eq!(order, [4, 5, 3, 7, 2, 6, 1]);
    }

    /// Mutation: derive the tier from something other than the trigger, e.g. make `Peer`
    /// `Human`.
    /// Mutation: give `Blocked` the `Other` tier, or let it fall through to `Human`.
    #[test]
    fn a_permission_wait_ranks_with_a_question_before_any_finished_turn() {
        let wait = PendingCard::Blocked {
            session: id(1),
            on: WaitingFor::PermissionPrompt,
            since: at(900),
        };
        assert_eq!(Rank::of(&wait).tier(), Tier::Blocked);
        assert!(Rank::of(&wait) < Rank::of(&turn(2, TurnTrigger::Human, 0)));
    }

    /// A quota hold is a blocked card ranked after every other blocked one, whatever their
    /// ages, and still ahead of a finished turn.
    /// Mutation: give `QuotaHold` the `Blocked` tier (the old wait sorts first by age), or put
    /// it after `Human` in `Tier`.
    #[test]
    fn a_quota_hold_ranks_last_among_the_blocked() {
        let hold = PendingCard::Blocked {
            session: id(1),
            on: WaitingFor::QuotaHold,
            since: at(1),
        };
        let wait = PendingCard::Blocked {
            session: id(2),
            on: WaitingFor::PermissionPrompt,
            since: at(900),
        };
        assert_eq!(Rank::of(&hold).tier(), Tier::QuotaHold);
        assert!(Rank::of(&wait) < Rank::of(&hold));
        assert!(Rank::of(&asked(3, 5_000)) < Rank::of(&hold));
        assert!(Rank::of(&hold) < Rank::of(&turn(4, TurnTrigger::Human, 0)));
    }

    #[test]
    fn every_non_human_non_notification_trigger_is_the_same_tier() {
        for trigger in [
            TurnTrigger::Peer,
            TurnTrigger::Scheduled,
            TurnTrigger::Coordinator,
            TurnTrigger::AutoContinuation,
            TurnTrigger::LocalCommand,
            TurnTrigger::CompactionSummary,
            TurnTrigger::Unknown("anything".into()),
        ] {
            assert_eq!(
                Rank::of(&turn(1, trigger.clone(), 0)).tier(),
                Tier::Other,
                "{trigger:?}"
            );
        }
        assert_eq!(
            Rank::of(&turn(1, TurnTrigger::Human, 0)).tier(),
            Tier::Human
        );
    }

    /// Equal tier and time are equal ranks: the sort is stable, so ties keep input order.
    #[test]
    fn equal_ranks_tie() {
        let a = Rank::of(&turn(1, TurnTrigger::Human, 50));
        let b = Rank::of(&turn(2, TurnTrigger::Human, 50));
        assert_eq!(a, b);
        assert_eq!(a.since(), at(50));
    }
}
