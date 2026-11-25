//! What is due, and who is best placed to act on it: the Follow-ups page.
//!
//! Every row is a rule over facts conn already holds, never a model's reading: a comment on a
//! work item after the user's latest one, a session's wait that is over while the session sits
//! still, a release sessions wait on that nobody runs, a claim whose owner is gone, the work
//! items in a status the user named as waiting for their close check. A row names who acts (the
//! session that holds the context, a new session, or the user) and the prompt to hand it; the
//! page only shows them, and the human's own paste or Enter starts anything.
//!
//! The close checks are one row, a sweep, because one session checking each item's exit
//! criteria in turn costs less than a session per item. An item not ready for its check stays
//! out of it: one a running rollout ships, one a session that may be running owns, and one
//! whose check a session put off with `conn defer` until a time not yet come. Checking those
//! only finds out again that it is too soon, or bounces off their owner.

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use crate::config::FollowUpsConfig;
use crate::jira::WorkItemKey;
use crate::jira::comments::Unseen;
use crate::ledger::{
    CloseCheck, Deferral, Ledger, Note, Outcome, Release, Run, Slug, Subject, WaitOn, WaitState,
};
use crate::model::{SessionView, StatusSection, WorkItemRow, WorkItemsView};
use crate::types::{ActivityKind, Instant, JiraKey, SafeUrl, SessionId};
use crate::util::redact::Redacted;

/// The page's content: off without its config section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUpsView {
    Off,
    On {
        /// The most pressing first.
        due: Vec<FollowUp>,
        /// Items waiting for their close check that are not ready for it: the soonest due
        /// first, then those with no date.
        not_yet: Vec<NotYet>,
    },
}

/// A work item in a close status that is not ready for its check, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotYet {
    pub item: ClosingItem,
    pub hold: Hold,
}

/// Why an item is not ready for its close check, the first that holds of these.
///
/// `deferred` on the first two is a check also put off until a time not yet come, so the
/// reader is told when it is due even while something else holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// A running rollout said it ships it: it is still going out.
    RollingOut {
        rollout: Slug,
        run: Run,
        deferred: Option<Deferral>,
    },
    /// A session that may be running owns it: it holds the context and closes it itself, and
    /// the guard would refuse another session's writes to it.
    WithOwner {
        owner: SessionId,
        presence: Presence,
        deferred: Option<Deferral>,
    },
    /// A session put its check off until a time not yet come.
    Deferred(Deferral),
}

impl Hold {
    /// The check put off, when it was.
    pub const fn deferral(&self) -> Option<&Deferral> {
        match self {
            Self::RollingOut { deferred, .. } | Self::WithOwner { deferred, .. } => {
                deferred.as_ref()
            }
            Self::Deferred(deferral) => Some(deferral),
        }
    }

    /// The list's order: by the time it is put off until, the soonest first; then the undated,
    /// rolling out before with the owner.
    fn rank(&self) -> (Undated, Option<Instant>, u8) {
        let group = match self {
            Self::RollingOut { .. } => 0,
            Self::WithOwner { .. } => 1,
            Self::Deferred(_) => 2,
        };
        match self.deferral() {
            Some(deferral) => (Undated::No, Some(deferral.until), group),
            None => (Undated::Yes, None, group),
        }
    }
}

/// Whether a held item has no date it is due: those sort after every dated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Undated {
    No,
    Yes,
}

/// A listed work item in a status the user named as shipped and waiting for its close check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingItem {
    pub key: WorkItemKey,
    pub url: Option<SafeUrl>,
    pub summary: Redacted<String>,
    pub status: Redacted<String>,
}

/// An item due its close check, and what to check when a session put it off until now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseCheckDue {
    pub item: ClosingItem,
    pub reached: Option<Deferral>,
}

/// One thing due, who should act on it, and since when it is due (when known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub due: Due,
    pub handler: Handler,
    pub since: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// One of the user's open work items, with every reason it is due; never without one.
    Ticket {
        key: WorkItemKey,
        url: Option<SafeUrl>,
        summary: Redacted<String>,
        reasons: Vec<TicketReason>,
        /// The sessions whose text names it, active first.
        mentioned_in: Vec<SessionId>,
    },
    /// A topic whose owner is gone.
    Topic { subject: Subject, owner: SessionId },
    /// A session's wait is over and the session is not at work on it.
    Wait {
        session: SessionId,
        on: WaitOn,
        then: Note,
        over: Over,
    },
    /// Sessions wait on the next finish of a rollout that nobody is running.
    Release {
        rollout: Slug,
        waiting: Vec<SessionId>,
    },
    /// Work items whose close check can be done now, for one session to sweep; never empty.
    CloseChecks { items: Vec<CloseCheckDue> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketReason {
    /// Someone wrote `count` comments after the user's latest, the newest at `newest`.
    Asked {
        count: NonZeroUsize,
        newest: Instant,
    },
    /// The session that owns it is gone: nothing is left to resume. Not said of an item
    /// waiting for its close check, which the check takes over.
    OwnerGone { owner: SessionId },
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Over {
    Rollout {
        outcome: Outcome,
        at: Instant,
    },
    Time {
        at: Instant,
    },
    Released {
        by: SessionId,
        at: Instant,
    },
    /// Jira resolved the ticket the wait was on or about, with `resolution`, at `at` (when
    /// Jira gave no time, when it was found closed).
    TicketClosed {
        ticket: JiraKey,
        resolution: String,
        at: Instant,
    },
}

/// Who should act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handler {
    /// The session that owns the thing or whose wait it is: it holds the context.
    Session { id: SessionId, presence: Presence },
    /// No session can pick it up: a new one, in `dir` when a session that named it worked in
    /// one.
    NewSession { dir: Option<PathBuf> },
    /// Only the user can: a release is started by a person or the session they ask.
    You,
}

/// Where a handling session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// A turn or its background work is running.
    Working,
    /// A process, with nothing running.
    Live,
    /// No process; its transcript can be resumed.
    Dormant,
    /// Its process could not be told; it may be running, so it is not resumed.
    Unknown,
}

/// How pressing a row is: a person waiting on the user first, housekeeping last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Asked,
    WaitOver,
    Release,
    OwnerGone,
    Closing,
}

impl TicketReason {
    const fn tier(&self) -> Tier {
        match self {
            Self::Asked { .. } => Tier::Asked,
            Self::OwnerGone { .. } => Tier::OwnerGone,
        }
    }
}

impl FollowUp {
    pub fn tier(&self) -> Tier {
        match &self.due {
            Due::Ticket { reasons, .. } => reasons
                .iter()
                .map(TicketReason::tier)
                .min()
                .unwrap_or(Tier::OwnerGone),
            Due::Topic { .. } => Tier::OwnerGone,
            Due::Wait { .. } => Tier::WaitOver,
            Due::Release { .. } => Tier::Release,
            Due::CloseChecks { .. } => Tier::Closing,
        }
    }
}

/// Where a session is, or `None` when it is gone: no process and nothing to resume, or older
/// than anything the model lists.
fn presence(views: &[&SessionView], id: &SessionId) -> Option<Presence> {
    let view = views.iter().find(|view| view.session.id == *id)?;
    match view.session.activity.kind() {
        ActivityKind::Running | ActivityKind::Watching => Some(Presence::Working),
        ActivityKind::Blocked | ActivityKind::Idle => Some(Presence::Live),
        ActivityKind::Dormant => Some(Presence::Dormant),
        ActivityKind::Unknown => Some(Presence::Unknown),
        ActivityKind::Gone => None,
    }
}

/// The directory of the first of `sessions` that worked in one.
fn dir_of(views: &[&SessionView], sessions: &[SessionId]) -> Option<PathBuf> {
    sessions.iter().find_map(|id| {
        views
            .iter()
            .find(|view| view.session.id == *id)
            .and_then(|view| view.session.cwd.clone())
    })
}

/// What is due at `now`, the most pressing first, then the longest due, and the items waiting
/// for a close check they are not ready for.
pub fn build(
    config: &FollowUpsConfig,
    work_items: &WorkItemsView,
    ledger: &Ledger,
    sessions: &[SessionView],
    dormant: &[SessionView],
    now: Instant,
) -> FollowUpsView {
    let views: Vec<&SessionView> = sessions.iter().chain(dormant).collect();
    let listed = match work_items {
        WorkItemsView::Listed { sections, .. } => sections.as_slice(),
        WorkItemsView::NoJira | WorkItemsView::NeverRead | WorkItemsView::Failed(_) => &[],
    };
    let closing = Closing::sort(config, listed, ledger, &views, now);
    let mut due: Vec<FollowUp> = tickets(config, listed, ledger, &views)
        .into_iter()
        .chain(close_checks(closing.due, &views))
        .chain(topics(ledger, &views))
        .chain(waits(ledger, &views, now))
        .chain(releases(ledger, now))
        .collect();
    due.sort_by_key(|follow_up| (follow_up.tier(), follow_up.since.is_none(), follow_up.since));
    FollowUpsView::On {
        due,
        not_yet: closing.not_yet,
    }
}

/// Where a listed work item is, by the status the user's config names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Being worked on.
    Open,
    /// Shipped and waiting for its close check.
    Closing,
}

fn stage(config: &FollowUpsConfig, section: &StatusSection) -> Stage {
    if config
        .close_statuses
        .iter()
        .any(|status| status == section.status.as_str())
    {
        Stage::Closing
    } else {
        Stage::Open
    }
}

/// The listed items waiting for their close check, by whether they are ready for it.
struct Closing<'w> {
    /// Each with the sessions whose text names it.
    due: Vec<(CloseCheckDue, &'w [SessionId])>,
    not_yet: Vec<NotYet>,
}

/// Whether an item waiting for its close check can have it now.
enum Readiness {
    /// It can; `reached` is the deferral whose time has come, if it was put off.
    Ready {
        reached: Option<Deferral>,
    },
    Held(Hold),
}

/// An item is held while a running rollout ships it, while a session that may be running owns
/// it, and while its deferral's time has not come; else it is ready. Only a configured key is
/// in the ledger.
fn readiness(
    key: &WorkItemKey,
    ledger: &Ledger,
    views: &[&SessionView],
    now: Instant,
) -> Readiness {
    let WorkItemKey::Configured(key) = key else {
        return Readiness::Ready { reached: None };
    };
    let subject = Subject::Ticket(key.clone());
    let (reached, deferred) = match ledger.close_check(key, now) {
        CloseCheck::Open => (None, None),
        CloseCheck::Reached(deferral) => (Some(deferral.clone()), None),
        CloseCheck::Deferred(deferral) => (None, Some(deferral.clone())),
    };
    if let Some((rollout, run)) = ledger.shipping(&subject) {
        return Readiness::Held(Hold::RollingOut {
            rollout: rollout.clone(),
            run: run.clone(),
            deferred,
        });
    }
    if let Some(held) = ledger.owner(&subject) {
        match presence(views, &held.session) {
            Some(presence @ (Presence::Working | Presence::Live | Presence::Unknown)) => {
                return Readiness::Held(Hold::WithOwner {
                    owner: held.session.clone(),
                    presence,
                    deferred,
                });
            }
            Some(Presence::Dormant) | None => {}
        }
    }
    match deferred {
        Some(deferral) => Readiness::Held(Hold::Deferred(deferral)),
        None => Readiness::Ready { reached },
    }
}

impl<'w> Closing<'w> {
    fn sort(
        config: &FollowUpsConfig,
        sections: &'w [StatusSection],
        ledger: &Ledger,
        views: &[&SessionView],
        now: Instant,
    ) -> Self {
        let mut closing = Self {
            due: Vec::new(),
            not_yet: Vec::new(),
        };
        for section in sections
            .iter()
            .filter(|section| stage(config, section) == Stage::Closing)
        {
            for row in section.rows() {
                let item = ClosingItem {
                    key: row.key.clone(),
                    url: row.url.clone(),
                    summary: row.summary.clone(),
                    status: section.status.clone(),
                };
                match readiness(&row.key, ledger, views, now) {
                    Readiness::Ready { reached } => closing
                        .due
                        .push((CloseCheckDue { item, reached }, &row.sessions)),
                    Readiness::Held(hold) => closing.not_yet.push(NotYet { item, hold }),
                }
            }
        }
        closing.not_yet.sort_by_key(|not_yet| not_yet.hold.rank());
        closing
    }
}

/// The one sweep over the items whose close check can be done now, in a new session in the
/// directory of the first session that named one of them; nothing when none can.
fn close_checks(
    due: Vec<(CloseCheckDue, &[SessionId])>,
    views: &[&SessionView],
) -> Option<FollowUp> {
    if due.is_empty() {
        return None;
    }
    let mentions: Vec<SessionId> = due
        .iter()
        .flat_map(|(_, mentions)| mentions.iter().cloned())
        .collect();
    let items: Vec<CloseCheckDue> = due.into_iter().map(|(item, _)| item).collect();
    Some(FollowUp {
        since: items
            .iter()
            .filter_map(|item| item.reached.as_ref().map(|deferral| deferral.until))
            .min(),
        due: Due::CloseChecks { items },
        handler: Handler::NewSession {
            dir: dir_of(views, &mentions),
        },
    })
}

/// The follow-ups of the listed items for anything but their close check.
fn tickets(
    config: &FollowUpsConfig,
    sections: &[StatusSection],
    ledger: &Ledger,
    views: &[&SessionView],
) -> Vec<FollowUp> {
    sections
        .iter()
        .flat_map(|section| {
            let stage = stage(config, section);
            section
                .rows()
                .filter_map(move |row| ticket(row, stage, ledger, views))
        })
        .collect()
}

/// The row's follow-up, when anything about it besides its close check is due.
fn ticket(
    row: &WorkItemRow,
    stage: Stage,
    ledger: &Ledger,
    views: &[&SessionView],
) -> Option<FollowUp> {
    let owner = match &row.key {
        WorkItemKey::Configured(key) => ledger
            .owner(&Subject::Ticket(key.clone()))
            .map(|held| held.session.clone()),
        WorkItemKey::Other(_) => None,
    };
    let owner_presence = owner.as_ref().map(|id| (id, presence(views, id)));
    let mut reasons = Vec::new();
    let mut since = None;
    if let Some(Unseen::New { count, newest }) = row.unanswered {
        reasons.push(TicketReason::Asked { count, newest });
        since = Some(newest);
    }
    if let (Some((id, None)), Stage::Open) = (owner_presence, stage) {
        reasons.push(TicketReason::OwnerGone { owner: id.clone() });
    }
    if reasons.is_empty() {
        return None;
    }
    let handler = match owner_presence {
        Some((id, Some(presence))) => Handler::Session {
            id: id.clone(),
            presence,
        },
        Some((_, None)) | None => Handler::NewSession {
            dir: dir_of(views, &row.sessions),
        },
    };
    Some(FollowUp {
        due: Due::Ticket {
            key: row.key.clone(),
            url: row.url.clone(),
            summary: row.summary.clone(),
            reasons,
            mentioned_in: row.sessions.clone(),
        },
        handler,
        since,
    })
}

/// Topics whose owner is gone. A ticket's is a reason on the ticket's own row, and a rollout's
/// claim says nothing once it is not running.
fn topics(ledger: &Ledger, views: &[&SessionView]) -> Vec<FollowUp> {
    ledger
        .claims()
        .into_iter()
        .filter(|(subject, held)| {
            matches!(subject, Subject::Topic(_)) && presence(views, &held.session).is_none()
        })
        .map(|(subject, held)| FollowUp {
            due: Due::Topic {
                subject: subject.clone(),
                owner: held.session.clone(),
            },
            handler: Handler::NewSession {
                dir: dir_of(views, std::slice::from_ref(&held.session)),
            },
            since: Some(held.since),
        })
        .collect()
}

/// Waits that are over while their session is not at work.
fn waits(ledger: &Ledger, views: &[&SessionView], now: Instant) -> Vec<FollowUp> {
    ledger
        .waits(now)
        .into_iter()
        .filter_map(|(session, state)| {
            let over = match state {
                WaitState::Waiting { .. } => return None,
                WaitState::RolloutFinished { finish, .. } => Over::Rollout {
                    outcome: finish.outcome,
                    at: finish.at,
                },
                WaitState::TimeReached { wait } => match &wait.on {
                    WaitOn::Until(at) => Over::Time { at: *at },
                    WaitOn::Rollout(_) | WaitOn::Subject(_) | WaitOn::For(_) => {
                        Over::Time { at: now }
                    }
                },
                WaitState::Released { by, at, .. } => Over::Released { by: by.clone(), at },
                WaitState::TicketClosed { ticket, closed, .. } => Over::TicketClosed {
                    ticket: ticket.clone(),
                    resolution: closed.resolution.clone(),
                    at: closed.at.unwrap_or(now),
                },
            };
            let handler = match presence(views, session) {
                Some(Presence::Working) => return None,
                Some(presence) => Handler::Session {
                    id: session.clone(),
                    presence,
                },
                None => Handler::NewSession {
                    dir: dir_of(views, std::slice::from_ref(session)),
                },
            };
            let wait = state.wait();
            Some(FollowUp {
                since: Some(over.at()),
                due: Due::Wait {
                    session: session.clone(),
                    on: wait.on.clone(),
                    then: wait.then.clone(),
                    over,
                },
                handler,
            })
        })
        .collect()
}

/// Releases sessions wait on that nobody runs.
fn releases(ledger: &Ledger, now: Instant) -> Vec<FollowUp> {
    ledger
        .releases(now)
        .into_iter()
        .filter_map(|(name, release)| match release {
            Release::Pending { .. } => Some(FollowUp {
                since: release
                    .waiting()
                    .first()
                    .and_then(|first| ledger.wait_of(first, now))
                    .map(|state| state.wait().since),
                due: Due::Release {
                    rollout: name.clone(),
                    waiting: release.waiting().into_iter().cloned().collect(),
                },
                handler: Handler::You,
            }),
            Release::Running { .. } | Release::Quiet { .. } => None,
        })
        .collect()
}

impl Over {
    pub const fn at(&self) -> Instant {
        match self {
            Self::Rollout { at, .. }
            | Self::Time { at }
            | Self::Released { at, .. }
            | Self::TicketClosed { at, .. } => *at,
        }
    }
}

impl FollowUp {
    /// What to tell the session that acts: the owner, which knows the work, or a new one, which
    /// is told to claim it first. `None` for what only the user does.
    pub fn prompt(&self) -> Option<String> {
        let fresh = matches!(self.handler, Handler::NewSession { .. });
        match &self.due {
            Due::Ticket { key, reasons, .. } => {
                let mut lines = Vec::new();
                if fresh {
                    lines.push(format!(
                        "Take over {key}: run `conn claim {key}` first, then read the ticket and what earlier sessions did on it."
                    ));
                }
                for reason in reasons {
                    lines.push(match reason {
                        TicketReason::Asked { count, .. } => format!(
                            "{count} comment(s) on {key} came after my latest one. Read them and draft a reply for me to approve before you post it."
                        ),
                        TicketReason::OwnerGone { .. } => format!(
                            "The session that owned {key} is gone. Find out where it stands and tell me what is left."
                        ),
                    });
                }
                Some(lines.join("\n\n"))
            }
            Due::Topic { subject, .. } => Some(format!(
                "Take over {subject}: run `conn claim {subject}` first. The session that owned it is gone; find out from the ledger (`conn who`) and the work it names where it stands, and tell me what is left."
            )),
            Due::Wait { on, then, .. } => {
                let what = on_words(on);
                Some(if fresh {
                    format!(
                        "A session that has ended was waiting on {what}, which is over now. It meant to do this next: {}. Do it, and tell me what you did.",
                        then.as_str()
                    )
                } else {
                    format!(
                        "Your wait on {what} is over. You said you would then: {}. Go ahead.",
                        then.as_str()
                    )
                })
            }
            Due::Release { .. } => None,
            Due::CloseChecks { items } => Some(sweep_prompt(items)),
        }
    }
}

/// One session checks each item's exit criteria in turn: it closes what holds, records with
/// `conn defer` what can only be checked later, and leaves an item another running session owns
/// to that session.
fn sweep_prompt(items: &[CloseCheckDue]) -> String {
    let list: Vec<String> = items
        .iter()
        .map(|due| {
            let mut line = format!("- {} ({})", due.item.key, due.item.status.as_str());
            if let Some(deferral) = &due.reached {
                let _ = write!(
                    line,
                    ": deferred until {}, to check then: {}",
                    deferral.until,
                    deferral.check.as_str()
                );
            }
            line
        })
        .collect();
    format!(
        "Close-check sweep. These work items are shipped and waiting for their close check:\n{}\n\n\
         For each one, in turn: run `conn claim <key>` first; if a running session owns it, send that session what you find (SendMessage) and leave the ticket to it. \
         Read the ticket and check whether its exit criteria hold in production. If they hold, close it. \
         If one can only be checked after a date or a period (\"a week after the deploy\"), run `conn defer <key> --until <time> --check \"<what to check then>\"`, which keeps it out of this sweep until then. \
         Otherwise, find what is missing. `conn release <key>` when you are done with it.\n\n\
         End with one line per item: closed, deferred until when, or what is missing.",
        list.join("\n")
    )
}

/// A wait's subject in words, as a prompt says it.
pub fn on_words(on: &WaitOn) -> String {
    match on {
        WaitOn::Rollout(name) => format!("rollout:{}", name.as_str()),
        WaitOn::Subject(subject) => subject.to_string(),
        WaitOn::Until(at) => format!("the time {at}"),
        WaitOn::For(note) => note.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    //! Goal: each rule lists what it should and nothing else, names the right handler, and the
    //! rows come most pressing first.
    //!
    //! Method: a hand-written ledger, work item list and set of sessions at fixed instants;
    //! every expected row is written out from the rules.

    use std::num::NonZeroU32;

    use super::*;
    use crate::jira::comments::Unseen;
    use crate::ledger::{Act, Entry};
    use crate::model::StatusSection;
    use crate::testutil::{secs, session_id as sid, session_view};
    use crate::types::{Activity, JiraKey, JiraPrefix, ListingStatus};

    fn key(n: u32) -> JiraKey {
        JiraKey::new(
            JiraPrefix::parse("ABC").unwrap(),
            NonZeroU32::new(n).unwrap(),
        )
    }

    fn row(n: u32, unanswered: Option<Unseen>, mentioned_in: Vec<SessionId>) -> WorkItemRow {
        WorkItemRow {
            key: WorkItemKey::Configured(key(n)),
            url: None,
            summary: Redacted::of("a ticket"),
            creator: None,
            description: None,
            sessions: mentioned_in,
            comments: None,
            unanswered,
            kind: crate::model::RowKind::Item,
            waits_on: Vec::new(),
            epic: None,
        }
    }

    fn section(status: &str, rows: Vec<WorkItemRow>) -> StatusSection {
        StatusSection::new(Redacted::of(status), rows)
    }

    fn entry(session: u8, at: i64, act: Act) -> Entry {
        Entry {
            at: secs(at),
            session: sid(session),
            act,
        }
    }

    fn idle() -> Activity {
        Activity::Idle {
            ended_at: secs(0),
            listing: ListingStatus::Seen,
        }
    }

    fn dormant() -> Activity {
        Activity::Dormant {
            last_seen: secs(0),
            background: None,
        }
    }

    fn asked(count: usize, newest: i64) -> Unseen {
        Unseen::New {
            count: NonZeroUsize::new(count).unwrap(),
            newest: secs(newest),
        }
    }

    /// Sessions: 1 idle, 2 dormant, 3 working; 9 is in no list, so gone.
    fn sessions() -> Vec<SessionView> {
        vec![
            session_view(1, None, idle()),
            session_view(2, None, dormant()),
            session_view(3, None, Activity::Running { since: secs(0) }),
        ]
    }

    fn ledger() -> Ledger {
        let checkout = Slug::parse("checkout").unwrap();
        let ticket = |n| Subject::Ticket(key(n));
        Ledger::fold(
            vec![
                // ABC-1 owned by the idle session; ABC-3 by a gone one; ABC-4 by the dormant one.
                entry(
                    1,
                    10,
                    Act::Claim {
                        subject: ticket(1),
                        took_from: None,
                    },
                ),
                entry(
                    9,
                    11,
                    Act::Claim {
                        subject: ticket(3),
                        took_from: None,
                    },
                ),
                entry(
                    2,
                    12,
                    Act::Claim {
                        subject: ticket(4),
                        took_from: None,
                    },
                ),
                entry(
                    9,
                    13,
                    Act::Claim {
                        subject: Subject::parse("topic:handover").unwrap(),
                        took_from: None,
                    },
                ),
                entry(
                    2,
                    14,
                    Act::Claim {
                        subject: Subject::parse("topic:parked").unwrap(),
                        took_from: None,
                    },
                ),
                // The dormant session and the working one wait on checkout, which then ships.
                entry(
                    2,
                    20,
                    Act::Wait {
                        on: WaitOn::Rollout(checkout.clone()),
                        then: Note::parse("re-run the pilot").unwrap(),
                        ticket: None,
                    },
                ),
                entry(
                    3,
                    21,
                    Act::Wait {
                        on: WaitOn::Rollout(checkout.clone()),
                        then: Note::parse("check it").unwrap(),
                        ticket: None,
                    },
                ),
                entry(
                    4,
                    22,
                    Act::RolloutStart {
                        rollout: checkout.clone(),
                        ships: Vec::new(),
                    },
                ),
                entry(
                    4,
                    30,
                    Act::RolloutFinish {
                        rollout: checkout,
                        outcome: Outcome::Shipped,
                        note: None,
                    },
                ),
                // The idle session waits on portal, which nobody runs.
                entry(
                    1,
                    40,
                    Act::Wait {
                        on: WaitOn::Rollout(Slug::parse("portal").unwrap()),
                        then: Note::parse("verify").unwrap(),
                        ticket: None,
                    },
                ),
            ]
            .into_iter()
            .chain(closing_entries()),
        )
    }

    /// ABC-7's close check was put off until 50, which has come; ABC-8's until 200; ABC-9's
    /// owner is gone, ABC-10's is the idle session, ABC-11's the dormant one; the nightly
    /// rollout, under way, ships ABC-12.
    fn closing_entries() -> Vec<Entry> {
        vec![
            entry(
                1,
                41,
                Act::Defer {
                    ticket: key(7),
                    until: secs(50),
                    check: Note::parse("a week of clean error rates").unwrap(),
                },
            ),
            entry(
                1,
                42,
                Act::Defer {
                    ticket: key(8),
                    until: secs(200),
                    check: Note::parse("the November invoices match").unwrap(),
                },
            ),
            entry(
                9,
                43,
                Act::Claim {
                    subject: Subject::Ticket(key(9)),
                    took_from: None,
                },
            ),
            entry(
                1,
                44,
                Act::Claim {
                    subject: Subject::Ticket(key(10)),
                    took_from: None,
                },
            ),
            entry(
                2,
                45,
                Act::Claim {
                    subject: Subject::Ticket(key(11)),
                    took_from: None,
                },
            ),
            entry(
                4,
                46,
                Act::RolloutStart {
                    rollout: Slug::parse("nightly").unwrap(),
                    ships: vec![Subject::Ticket(key(12))],
                },
            ),
            entry(
                1,
                47,
                Act::Defer {
                    ticket: key(10),
                    until: secs(300),
                    check: Note::parse("the export ran twice").unwrap(),
                },
            ),
        ]
    }

    fn work_items() -> WorkItemsView {
        WorkItemsView::Listed {
            read_at: secs(0),
            sections: vec![
                section(
                    "In Progress",
                    vec![
                        row(1, Some(asked(2, 50)), vec![sid(1)]),
                        row(3, None, Vec::new()),
                        row(4, None, vec![sid(2)]),
                        row(6, Some(Unseen::Caught), vec![sid(2)]),
                    ],
                ),
                section(
                    "Prod",
                    vec![
                        row(5, None, vec![sid(2), sid(1)]),
                        row(7, None, vec![sid(1)]),
                        row(8, None, Vec::new()),
                        row(9, None, Vec::new()),
                        row(10, None, Vec::new()),
                        row(11, None, Vec::new()),
                        row(12, None, Vec::new()),
                    ],
                ),
            ],
            failure: None,
        }
    }

    fn view_now() -> FollowUpsView {
        let config = FollowUpsConfig {
            close_statuses: vec!["Prod".to_owned()],
        };
        build(
            &config,
            &work_items(),
            &ledger(),
            &sessions(),
            &[],
            secs(100),
        )
    }

    fn build_now() -> Vec<FollowUp> {
        match view_now() {
            FollowUpsView::On { due, .. } => due,
            FollowUpsView::Off => panic!("the config turns the page on"),
        }
    }

    fn label(follow_up: &FollowUp) -> String {
        match &follow_up.due {
            Due::Ticket { key, .. } => key.to_string(),
            Due::Topic { subject, .. } => subject.to_string(),
            Due::Wait { session, .. } => format!(
                "wait of {}",
                session.as_str().rsplit('-').next().unwrap_or_default()
            ),
            Due::Release { rollout, .. } => format!("release {}", rollout.as_str()),
            Due::CloseChecks { items } => {
                let keys: Vec<String> = items.iter().map(|due| due.item.key.to_string()).collect();
                format!("close checks {}", keys.join(" "))
            }
        }
    }

    /// The rows and their order: a question first, then a wait that is over, the release
    /// nobody runs, what a gone owner left, and the one sweep over the close checks that can be
    /// done now. Nothing else is due: ABC-4's owner is dormant and nobody asked; ABC-6's
    /// comments are answered; the working session's wait is over but it is at work; the
    /// dormant session's topic can be resumed; ABC-8, ABC-10 and ABC-12 are not ready for their
    /// check; ABC-9's gone owner is the sweep's to take over, not a row of its own. ABC-11's
    /// owner is dormant, so its claim passes to the sweep.
    /// Mutation: list a working session's wait (red, "wait of 000000000003" appears); call a
    /// dormant owner gone (red, ABC-4 appears); sort by kind of row in reverse (red, order);
    /// sweep a deferred item (red, ABC-8 in the sweep); say a closing item's gone owner (red,
    /// ABC-9 row); hold an item with a dormant owner (red, ABC-11 missing).
    /// An idle session's time wait about a ticket is due as soon as the ticket closes, before
    /// its time, and says the ticket closed; without the closed ticket it is not due yet.
    /// Mutation: map `TicketClosed` to `Over::Time` (red, the row says the time).
    #[test]
    fn a_wait_about_a_closed_ticket_is_due_as_closed() {
        use crate::jira::resolution::{Closed, ClosedTickets, Resolution};
        let ledger = Ledger::fold([entry(
            1,
            10,
            Act::Wait {
                on: WaitOn::Until(secs(5000)),
                then: Note::parse("re-check the reaper; close ABC-20").unwrap(),
                ticket: Some(key(20)),
            },
        )]);
        let views = sessions();
        let views: Vec<&SessionView> = views.iter().collect();
        assert_eq!(waits(&ledger, &views, secs(100)), []);
        let mut closed = ClosedTickets::default();
        closed.record(
            key(20),
            Resolution::Closed(Closed {
                resolution: "Done".to_owned(),
                status: "Done".to_owned(),
                at: Some(secs(90)),
            }),
        );
        let rows = waits(&ledger.knowing_closed(closed), &views, secs(100));
        let [row] = rows.as_slice() else {
            panic!("one row: {rows:?}")
        };
        assert!(
            matches!(
                &row.due,
                Due::Wait { over: Over::TicketClosed { ticket, resolution, at }, .. }
                    if *ticket == key(20) && resolution == "Done" && *at == secs(90)
            ),
            "{row:?}"
        );
    }

    #[test]
    fn each_rule_lists_its_rows_most_pressing_first() {
        let labels: Vec<String> = build_now().iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "ABC-1",
                "wait of 000000000002",
                "release portal",
                "topic:handover",
                "ABC-3",
                "close checks ABC-5 ABC-7 ABC-9 ABC-11",
            ]
        );
    }

    /// An item not ready for its close check is listed apart, with why: a running rollout
    /// ships it, an owner that may be running holds it, or its check is put off past now. One
    /// whose deferral's time has come is in the sweep, with what to check. One held by its
    /// owner and also put off keeps its deferral. The list runs by date, the soonest first,
    /// the undated last.
    /// Mutation: sweep an item a running rollout ships (red, ABC-12 moves); sweep an item whose
    /// owner is idle (red, ABC-10 moves); drop the not-yet list (red); sweep a reached item
    /// without its deferral (red); drop the deferral of an item its owner holds (red); sort
    /// by why before when (red).
    #[test]
    fn an_item_not_ready_waits_with_why_and_a_reached_one_says_what_to_check() {
        let FollowUpsView::On { due, not_yet } = view_now() else {
            panic!("the config turns the page on");
        };
        let held: Vec<(String, Hold)> = not_yet
            .into_iter()
            .map(|not_yet| (not_yet.item.key.to_string(), not_yet.hold))
            .collect();
        let deferral = Deferral {
            until: secs(200),
            check: Note::parse("the November invoices match").unwrap(),
            by: sid(1),
            since: secs(42),
        };
        assert_eq!(
            held,
            [
                ("ABC-8".to_owned(), Hold::Deferred(deferral)),
                (
                    "ABC-10".to_owned(),
                    Hold::WithOwner {
                        owner: sid(1),
                        presence: Presence::Live,
                        deferred: Some(Deferral {
                            until: secs(300),
                            check: Note::parse("the export ran twice").unwrap(),
                            by: sid(1),
                            since: secs(47),
                        }),
                    }
                ),
                (
                    "ABC-12".to_owned(),
                    Hold::RollingOut {
                        rollout: Slug::parse("nightly").unwrap(),
                        run: crate::ledger::Run {
                            session: sid(4),
                            since: secs(46),
                            ships: vec![Subject::Ticket(key(12))],
                        },
                        deferred: None,
                    }
                ),
            ]
        );
        let Some(Due::CloseChecks { items }) = due.last().map(|row| &row.due) else {
            panic!("the sweep is the last row");
        };
        let reached: Vec<Option<&str>> = items
            .iter()
            .map(|due| due.reached.as_ref().map(|d| d.check.as_str()))
            .collect();
        assert_eq!(
            reached,
            [None, Some("a week of clean error rates"), None, None]
        );
    }

    /// No item waits for its close check, or every one is put off: no sweep row.
    /// Mutation: return the sweep row when it has no items (red).
    #[test]
    fn no_sweep_without_an_item_to_check() {
        let config = FollowUpsConfig {
            close_statuses: vec!["Done-ish".to_owned()],
        };
        let FollowUpsView::On { due, not_yet } = build(
            &config,
            &work_items(),
            &ledger(),
            &sessions(),
            &[],
            secs(100),
        ) else {
            panic!("the config turns the page on");
        };
        assert_eq!(not_yet, []);
        assert!(
            !due.iter()
                .any(|row| matches!(row.due, Due::CloseChecks { .. }))
        );
    }

    /// Who acts: the owner that holds the context while it can be reached, a new session in a
    /// directory a session that named the ticket worked in otherwise, the user for a release.
    /// Mutation: hand an owned ticket to a new session (red, ABC-1's handler); take the
    /// directory from the last mention instead of the first (red, proj-2).
    #[test]
    fn the_owner_acts_while_it_can_else_a_new_session_or_the_user() {
        let rows = build_now();
        let handler = |label_text: &str| {
            rows.iter()
                .find(|row| label(row) == label_text)
                .map(|row| row.handler.clone())
                .unwrap()
        };
        assert_eq!(
            handler("ABC-1"),
            Handler::Session {
                id: sid(1),
                presence: Presence::Live
            }
        );
        assert_eq!(
            handler("wait of 000000000002"),
            Handler::Session {
                id: sid(2),
                presence: Presence::Dormant
            }
        );
        assert_eq!(handler("release portal"), Handler::You);
        assert_eq!(handler("ABC-3"), Handler::NewSession { dir: None });
        assert_eq!(
            handler("close checks ABC-5 ABC-7 ABC-9 ABC-11"),
            Handler::NewSession {
                dir: Some(PathBuf::from("/home/dev/proj-2"))
            }
        );
    }

    /// A new session is told to claim what it takes over; the owner is not. The wait's own
    /// words are carried to the session that said them. The sweep lists every item, says what
    /// to check for the one whose deferral is over, and says how to put off one that is early.
    /// Mutation: drop the claim line (red); word the owner's prompt as a new session's (red);
    /// drop the reached deferral from the sweep's list (red).
    #[test]
    fn a_new_session_claims_first_and_the_owner_hears_its_own_plan() {
        let rows = build_now();
        let prompt = |label_text: &str| {
            rows.iter()
                .find(|row| label(row) == label_text)
                .and_then(FollowUp::prompt)
                .unwrap()
        };
        assert_eq!(
            prompt("ABC-1"),
            "2 comment(s) on ABC-1 came after my latest one. Read them and draft a reply for me to approve before you post it."
        );
        assert!(prompt("ABC-3").starts_with("Take over ABC-3: run `conn claim ABC-3` first,"));
        assert_eq!(
            prompt("close checks ABC-5 ABC-7 ABC-9 ABC-11"),
            "Close-check sweep. These work items are shipped and waiting for their close check:\n\
             - ABC-5 (Prod)\n\
             - ABC-7 (Prod): deferred until 2027-01-15T08:00:50Z, to check then: a week of clean error rates\n\
             - ABC-9 (Prod)\n\
             - ABC-11 (Prod)\n\n\
             For each one, in turn: run `conn claim <key>` first; if a running session owns it, send that session what you find (SendMessage) and leave the ticket to it. \
             Read the ticket and check whether its exit criteria hold in production. If they hold, close it. \
             If one can only be checked after a date or a period (\"a week after the deploy\"), run `conn defer <key> --until <time> --check \"<what to check then>\"`, which keeps it out of this sweep until then. \
             Otherwise, find what is missing. `conn release <key>` when you are done with it.\n\n\
             End with one line per item: closed, deferred until when, or what is missing."
        );
        assert_eq!(
            prompt("wait of 000000000002"),
            "Your wait on rollout:checkout is over. You said you would then: re-run the pilot. Go ahead."
        );
        assert!(
            rows.iter()
                .any(|row| label(row) == "release portal" && row.prompt().is_none())
        );
    }
}
