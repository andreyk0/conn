//! What the map is drawn from: the sessions of the last week with what each is doing
//! ([`Motion`]), the tickets, pull requests, threads and directories their text links with
//! where each stands ([`Progress`]), and the dependency graph the page draws ([`deps`]).
//!
//! The anchors are what the sessions' text links, joined where two sessions share one or it is
//! one of the user's own open work items. The page no longer draws them as edges, since a
//! mention says little; it reads a ticket's progress from them.

pub mod deps;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use jiff::SignedDuration;

use crate::gh::{Answer, Checks, PrLifecycle, PrState, Readiness, Reading, ReviewDecision};
use crate::gist::Ask;
use crate::jira::WorkItemKey;
use crate::model::{InboxEntry, LinkView, Pin, ReleaseRun, SessionView, Waiting, WorkItemsView};
use crate::types::{Activity, Identifier, Instant, Link, SafeUrl, SessionId};
use crate::util::redact::Redacted;

/// How far back a session without a live process or a card is still on the map, unless it is
/// pinned.
pub const WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);

/// The most sessions drawn, the active and then the most recent first. A week of work is a
/// few dozen; the layout runs in the page and stays quick well past this.
pub const SESSIONS_MAX: usize = 150;

/// How many sessions an anchor must join to be drawn, unless it is one of the user's work
/// items: one session's own links are on its card already.
const SHARED_BY: usize = 2;

/// Whether a session is in play now or only within [`WINDOW`]; the page fades the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Recency {
    /// Anything but [`Motion::Dormant`].
    Active,
    Recent,
}

/// What a session is doing now: the one thing the map colours it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// A turn, or the session's background work, is running.
    Moving,
    /// It waits on the user: a question or a prompt, a card asking a decision or a move (or
    /// a card whose ask is not known yet, since a card is the user's by the pending rule), or
    /// a background run that stopped on something or failed.
    Blocked,
    /// Its card waits on other people.
    WaitingOnOthers,
    /// Its card waits on a release nobody runs: the user's move, but one that can wait.
    AwaitingRelease,
    /// A process with nothing going on, or a card that only reports.
    Idle,
    /// Its state could not be told.
    Unknown,
    /// No process and no card.
    Dormant,
}

impl Motion {
    pub const fn recency(self) -> Recency {
        match self {
            Self::Dormant => Recency::Recent,
            Self::Moving
            | Self::Blocked
            | Self::WaitingOnOthers
            | Self::AwaitingRelease
            | Self::Idle
            | Self::Unknown => Recency::Active,
        }
    }
}

/// Where the ticket or pull request an anchor stands for is, as far as conn read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// A pull request whose checks fail or that has changes requested; a work item whose
    /// status names a block.
    Blocking,
    /// A pull request waiting on review or on its checks; a work item past in progress and
    /// not done (review, QA, a deploy stage).
    Waiting,
    /// A draft, or an open pull request nothing holds up; a work item in progress.
    Moving,
    /// A work item not started.
    Queued,
    /// Merged or closed; a work item done.
    Done,
    /// Nothing read: a thread, a directory, a pull request not read or not found, a key that
    /// is not one of the user's work items.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct SessionNode {
    pub id: SessionId,
    pub title: Option<Redacted<String>>,
    /// The latest gist written for the session.
    pub gist: Option<Redacted<String>>,
    pub motion: Motion,
}

/// What an anchor stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    Link {
        identifier: Identifier,
        url: SafeUrl,
        /// A work item's summary, where the Jira read has the key.
        summary: Option<Redacted<String>>,
    },
    /// The working directory of the sessions; the home directory never is one, since a
    /// session started there says nothing about its work.
    Directory(PathBuf),
}

#[derive(Debug, Clone)]
pub struct AnchorNode {
    pub anchor: Anchor,
    /// The sessions joined to it, in the order of [`WorkMap::sessions`]; never empty.
    pub sessions: Vec<SessionId>,
    /// `Active` when any of its sessions is.
    pub recency: Recency,
    pub progress: Progress,
}

#[derive(Debug, Clone, Default)]
pub struct WorkMap {
    /// Active first, then the most recent first.
    pub sessions: Vec<SessionNode>,
    /// Work items first, then the most widely shared first.
    pub anchors: Vec<AnchorNode>,
}

/// What the map is drawn from: the model's views, built at `now`.
#[derive(Debug, Clone, Copy)]
pub struct Sources<'a> {
    pub now: Instant,
    /// Live and unclassifiable sessions, then the dormant ones.
    pub sessions: &'a [SessionView],
    pub dormant: &'a [SessionView],
    pub inbox: &'a [InboxEntry],
    pub work_items: &'a WorkItemsView,
    /// Never a directory anchor.
    pub home: Option<&'a Path>,
}

/// Whether a session has a card in the inbox, and what the card asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Card {
    /// `ask` is `None` until the card or its gist says.
    Pending {
        ask: Option<Ask>,
    },
    /// The card's session waits on a release nobody runs: the user's to run, but not now.
    ForRelease,
    None,
}

impl Card {
    /// The card of `entry`. The wait its session declared in the ledger says what it waits
    /// for when that wait places the card, as on the card itself; otherwise the gist does.
    fn of(entry: &InboxEntry) -> Self {
        match entry.standing.waiting() {
            Some(Waiting::Release {
                run: ReleaseRun::NotStarted,
                ..
            }) => Self::ForRelease,
            Some(
                Waiting::Release {
                    run: ReleaseRun::Running,
                    ..
                }
                | Waiting::Other { .. },
            ) => Self::Pending {
                ask: Some(Ask::Others),
            },
            None => Self::Pending { ask: entry.ask },
        }
    }
}

pub fn build(sources: &Sources<'_>) -> WorkMap {
    let cards: HashMap<&SessionId, Card> = sources
        .inbox
        .iter()
        .map(|entry| (&entry.session, Card::of(entry)))
        .collect();
    let card_of = |id: &SessionId| cards.get(id).copied().unwrap_or(Card::None);
    let mut chosen: Vec<(&SessionView, Motion)> = sources
        .sessions
        .iter()
        .chain(sources.dormant)
        .filter_map(|view| {
            let motion = motion_of(&view.session.activity, card_of(&view.session.id));
            on_map(view, motion, sources.now).then_some((view, motion))
        })
        .collect();
    chosen.sort_by(|(a, a_motion), (b, b_motion)| {
        a_motion
            .recency()
            .cmp(&b_motion.recency())
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    chosen.truncate(SESSIONS_MAX);

    let sessions = chosen
        .iter()
        .map(|&(view, motion)| SessionNode {
            id: view.session.id.clone(),
            title: view.title.clone(),
            gist: view.gist.clone(),
            motion,
        })
        .collect();
    WorkMap {
        sessions,
        anchors: anchors(&chosen, sources),
    }
}

/// What the session is doing. A wait on the user outranks the card, the card outranks what
/// the process is doing, since a card that asks something is the reason to look at it.
fn motion_of(activity: &Activity, card: Card) -> Motion {
    let process = match activity {
        Activity::Blocked { .. } => return Motion::Blocked,
        Activity::Running { .. } | Activity::Watching { .. } => Motion::Moving,
        Activity::Idle { .. } => Motion::Idle,
        Activity::Unknown { .. } => Motion::Unknown,
        Activity::Dormant { .. } | Activity::Gone { .. } => Motion::Dormant,
    };
    match card {
        Card::Pending {
            ask: Some(Ask::Decide | Ask::Act) | None,
        } => Motion::Blocked,
        Card::Pending {
            ask: Some(Ask::Others),
        } => Motion::WaitingOnOthers,
        Card::ForRelease => Motion::AwaitingRelease,
        // A card that only reports still sits in the inbox, so its session is not dormant.
        Card::Pending {
            ask: Some(Ask::Nothing),
        } => match process {
            Motion::Unknown | Motion::Dormant => Motion::Idle,
            Motion::Moving
            | Motion::Blocked
            | Motion::WaitingOnOthers
            | Motion::AwaitingRelease
            | Motion::Idle => process,
        },
        Card::None => process,
    }
}

/// Whether the session is drawn: anything in play, and a dormant one within the window.
/// An active session is on the map; a dormant one while it is within [`WINDOW`], or for as
/// long as it is pinned.
fn on_map(view: &SessionView, motion: Motion, now: Instant) -> bool {
    match motion.recency() {
        Recency::Active => true,
        Recency::Recent => {
            matches!(view.pin, Pin::Pinned(_))
                || view
                    .last_activity
                    .is_some_and(|at| now.duration_since(at) <= WINDOW)
        }
    }
}

/// What an anchor is keyed by while the sessions are collected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Link(Identifier),
    Directory(PathBuf),
}

/// An anchor while its sessions are collected: the first link's URL and pull-request reading.
struct Joined<'v> {
    link: Option<&'v LinkView>,
    sessions: Vec<SessionId>,
    recency: Recency,
}

fn anchors(chosen: &[(&SessionView, Motion)], sources: &Sources<'_>) -> Vec<AnchorNode> {
    let mine = work_items_by_key(sources.work_items);
    let mut order: Vec<Key> = Vec::new();
    let mut joined: HashMap<Key, Joined<'_>> = HashMap::new();
    for &(view, motion) in chosen {
        let links = view
            .links
            .iter()
            .filter_map(|link| Identifier::of(&link.link).map(|id| (Key::Link(id), Some(link))));
        let directory = view
            .session
            .cwd
            .as_ref()
            .filter(|cwd| Some(cwd.as_path()) != sources.home)
            .map(|cwd| (Key::Directory(cwd.clone()), None));
        let mut seen_here = HashSet::new();
        for (key, link) in links.chain(directory) {
            if !seen_here.insert(key.clone()) {
                continue;
            }
            let entry = joined.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                Joined {
                    link,
                    sessions: Vec::new(),
                    recency: Recency::Recent,
                }
            });
            entry.sessions.push(view.session.id.clone());
            entry.recency = entry.recency.min(motion.recency());
        }
    }
    let mut anchors: Vec<(bool, AnchorNode)> = order
        .into_iter()
        .filter_map(|key| {
            let Joined {
                link,
                sessions,
                recency,
            } = joined.remove(&key)?;
            let item = match &key {
                Key::Link(id) => match id.link() {
                    Link::Jira { key } => mine.get(key.to_string().as_str()),
                    Link::SlackThread { .. }
                    | Link::PullRequest { .. }
                    | Link::Doc { .. }
                    | Link::Other { .. } => None,
                },
                Key::Directory(_) => None,
            };
            let is_mine = item.is_some();
            if sessions.len() < SHARED_BY && !is_mine {
                return None;
            }
            let progress = match (item, link.and_then(|link| link.pr.as_ref())) {
                (Some(item), _) => item.progress,
                (None, Some(reading)) => pr_progress(reading),
                (None, None) => Progress::Unknown,
            };
            let anchor = match key {
                Key::Link(identifier) => Anchor::Link {
                    identifier,
                    url: link?.url.clone(),
                    summary: item.map(|item| item.summary.clone()),
                },
                Key::Directory(path) => Anchor::Directory(path),
            };
            Some((
                is_mine,
                AnchorNode {
                    anchor,
                    sessions,
                    recency,
                    progress,
                },
            ))
        })
        .collect();
    // Stable: anchors that tie keep the order their first session gave them.
    anchors.sort_by(|(a_mine, a), (b_mine, b)| {
        b_mine
            .cmp(a_mine)
            .then_with(|| b.sessions.len().cmp(&a.sessions.len()))
    });
    anchors.into_iter().map(|(_, anchor)| anchor).collect()
}

/// Where a pull request stands, from the latest state read, stale or not.
fn pr_progress(reading: &Reading) -> Progress {
    match reading {
        Reading::Value { observed, .. } => match &observed.answer {
            Answer::Found(state) => found_pr_progress(state),
            Answer::NotFound => Progress::Unknown,
        },
        Reading::Unread | Reading::Failure(_) => Progress::Unknown,
    }
}

/// A failure or a change request holds an open pull request up; a draft is still being
/// written; a pending check or a missing review waits on someone else.
fn found_pr_progress(state: &PrState) -> Progress {
    match state.state {
        PrLifecycle::Merged { .. } | PrLifecycle::Closed => return Progress::Done,
        PrLifecycle::Open => {}
    }
    let failing = matches!(state.checks, Checks::Failing { .. });
    let changes = state.review == ReviewDecision::ChangesRequested;
    let pending = matches!(state.checks, Checks::Pending { .. });
    let unreviewed = state.review == ReviewDecision::ReviewRequired;
    if failing || changes {
        Progress::Blocking
    } else if state.readiness == Readiness::Draft {
        Progress::Moving
    } else if pending || unreviewed {
        Progress::Waiting
    } else {
        Progress::Moving
    }
}

/// One of the user's work items as the map uses it.
struct MyItem {
    summary: Redacted<String>,
    progress: Progress,
}

/// The user's work items, by key text (`ABC-123`).
fn work_items_by_key(view: &WorkItemsView) -> BTreeMap<String, MyItem> {
    match view {
        WorkItemsView::Listed { sections, .. } => sections
            .iter()
            .flat_map(|section| {
                let progress = status_progress(section.status.as_str());
                section.rows().filter_map(move |row| match &row.key {
                    WorkItemKey::Configured(key) => Some((
                        key.to_string(),
                        MyItem {
                            summary: row.summary.clone(),
                            progress,
                        },
                    )),
                    WorkItemKey::Other(_) => None,
                })
            })
            .collect(),
        WorkItemsView::NoJira | WorkItemsView::NeverRead | WorkItemsView::Failed(_) => {
            BTreeMap::new()
        }
    }
}

/// Where a work item stands, from its status name. Workflows name their statuses freely, so
/// the name is folded and read for the few words every workflow shares; a status past
/// "in progress" that is not "done" is a hand-off (review, QA, a deploy stage) and waits.
fn status_progress(status: &str) -> Progress {
    let folded: String = status
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let says = |words: &[&str]| words.iter().any(|word| folded.contains(word));
    if says(&["block", "onhold"]) {
        Progress::Blocking
    } else if says(&["inprogress", "indevelopment"]) {
        Progress::Moving
    } else if says(&["todo", "backlog", "open", "new", "selected"]) {
        Progress::Queued
    } else if says(&["done", "closed", "resolved", "cancel", "wontdo"]) {
        Progress::Done
    } else {
        Progress::Waiting
    }
}

#[cfg(test)]
mod tests;
