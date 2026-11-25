//! The user's open Jira work items as the work items page shows them: the list the cache
//! holds, each item joined to the sessions that mention its key.
//!
//! "Never read", "could not read" and "read and it was this list" are three different
//! values of [`WorkItemsView`]; a failed read never replaces a good list, it is shown beside
//! it.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{SessionView, SourceState};
use crate::jira::children::Progress;
use crate::jira::comments::Unseen;
use crate::jira::links::Dependency;
use crate::jira::reads::PerTicket;
use crate::jira::{self, EpicOf, JiraError, Level, WorkItem, WorkItemKey};
use crate::types::{Instant, JiraKey, Link, LinkTargets, SafeUrl, SessionId};
use crate::util::redact::Redacted;

/// How many characters of a description a row shows.
pub const DESCRIPTION_CHARS: usize = 200;

/// What the model knows of the work items.
#[derive(Debug, Clone)]
pub enum WorkItemsView {
    /// The config names no Jira, so nothing is read.
    NoJira,
    /// No read has been tried and none is saved.
    NeverRead,
    /// No read has succeeded; this is why.
    Failed(WorkItemsFailure),
    /// A list, in sections by status. `failure` is a later read that did not succeed, which
    /// makes the list stale by the age of `read_at`.
    Listed {
        read_at: Instant,
        sections: Vec<StatusSection>,
        failure: Option<WorkItemsFailure>,
    },
}

/// The work items in one status, never none, as a tree one level deep: each item of an epic
/// is nested under it.
///
/// Sections come `In Progress` first, then `To Do`, then every other status by name. Within
/// a section the items that can be picked up come before the ones waiting on another ticket,
/// and otherwise the rows keep the query's order, which is the most recently updated first:
/// `acli` refuses to return `updated` itself, so that order is the only activity time there is.
/// An entry stands where its first row would: an epic of the section where the epic is, an
/// epic outside it where its first item is.
#[derive(Debug, Clone)]
pub struct StatusSection {
    pub status: Redacted<String>,
    entries: Vec<Entry>,
}

/// A top-level line of a section, with the items of the section nested under it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub head: Head,
    /// In the section's order; only an epic has any.
    pub children: Vec<WorkItemRow>,
}

/// What heads an entry.
#[derive(Debug, Clone)]
pub enum Head {
    /// An item of the section; boxed, as a row is several times the size of an epic's name.
    Row(Box<WorkItemRow>),
    /// An epic that is not in the section, named for the section's items in it: it is in
    /// another status, or not among the user's work items. Never without children.
    Epic(EpicRef),
}

impl StatusSection {
    /// The section of `rows`, all of one status, ordered and nested.
    pub fn new(status: Redacted<String>, mut rows: Vec<WorkItemRow>) -> Self {
        // Stable: within ready and within waiting, the query's order holds.
        rows.sort_by_key(WorkItemRow::is_waiting);
        Self {
            status,
            entries: tree(rows),
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Every item of the section, each epic followed by its items.
    pub fn rows(&self) -> impl Iterator<Item = &WorkItemRow> {
        self.entries.iter().flat_map(|entry| {
            let head = match &entry.head {
                Head::Row(row) => Some(&**row),
                Head::Epic(_) => None,
            };
            head.into_iter().chain(&entry.children)
        })
    }
}

/// `rows` nested under their epics: under the epic's own row where the epic is among them,
/// under a head naming the epic where it is not.
fn tree(rows: Vec<WorkItemRow>) -> Vec<Entry> {
    let epics_here: HashSet<JiraKey> = rows
        .iter()
        .filter(|row| matches!(row.kind, RowKind::Epic(_)))
        .filter_map(WorkItemRow::jira_key)
        .collect();
    let mut entries: Vec<Entry> = Vec::new();
    let mut under_rows: HashMap<JiraKey, Vec<WorkItemRow>> = HashMap::new();
    let mut under_heads: HashMap<JiraKey, usize> = HashMap::new();
    for row in rows {
        let Some(epic) = row.epic.clone() else {
            entries.push(Entry {
                head: Head::Row(Box::new(row)),
                children: Vec::new(),
            });
            continue;
        };
        if epics_here.contains(&epic.key) {
            under_rows.entry(epic.key).or_default().push(row);
        } else if let Some(entry) = under_heads
            .get(&epic.key)
            .and_then(|&at| entries.get_mut(at))
        {
            entry.children.push(row);
        } else {
            under_heads.insert(epic.key.clone(), entries.len());
            entries.push(Entry {
                head: Head::Epic(epic),
                children: vec![row],
            });
        }
    }
    for entry in &mut entries {
        if let Head::Row(row) = &entry.head
            && let Some(children) = row.jira_key().and_then(|key| under_rows.remove(&key))
        {
            entry.children = children;
        }
    }
    entries
}

/// Where a status's section goes. Names are compared folded (`In Progress`, `in progress`
/// and `InProgress` are one place), since workflows spell them differently; the exact name
/// breaks ties, so two spellings stay two sections in a fixed order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SectionPlace {
    InProgress,
    ToDo,
    Other { folded: String },
}

impl SectionPlace {
    fn of(status: &str) -> Self {
        let folded: String = status
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        match folded.as_str() {
            "inprogress" => Self::InProgress,
            "todo" => Self::ToDo,
            _ => Self::Other { folded },
        }
    }
}

/// `items` in sections by status name, each section in the order the items came.
fn sections<'i>(
    items: impl Iterator<Item = &'i WorkItem>,
    mut row: impl FnMut(&WorkItem) -> WorkItemRow,
) -> Vec<StatusSection> {
    let mut by_place: BTreeMap<(SectionPlace, &str), Vec<WorkItemRow>> = BTreeMap::new();
    for item in items {
        let name = item.status.name.as_str();
        by_place
            .entry((SectionPlace::of(name), name))
            .or_default()
            .push(row(item));
    }
    by_place
        .into_iter()
        .map(|((_, name), rows)| StatusSection::new(Redacted::of(name), rows))
        .collect()
}

/// A read that failed, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemsFailure {
    pub at: Instant,
    pub fault: WorkItemsFault,
}

/// Why the work items could not be read, by what the person can do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemsFault {
    /// `acli` has no credentials; `hint` is what to run.
    NotAuthenticated { hint: &'static str },
    /// Any other failure, as `acli` or the cache said it.
    Failed { reason: Redacted<String> },
}

impl WorkItemsFailure {
    fn of(failure: &jira::Failed) -> Self {
        Self {
            at: failure.at,
            fault: match &failure.error {
                &JiraError::NotAuthenticated { hint } => WorkItemsFault::NotAuthenticated { hint },
                error @ (JiraError::NotInstalled
                | JiraError::Timeout
                | JiraError::NonZero { .. }
                | JiraError::Unparseable { .. }
                | JiraError::Tool(_)
                | JiraError::CacheFile(_)) => WorkItemsFault::Failed {
                    reason: Redacted::of(&error.to_string()),
                },
            },
        }
    }
}

/// A description cut to [`DESCRIPTION_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teaser {
    pub text: Redacted<String>,
    /// The description goes on beyond `text`.
    pub cut: bool,
}

/// One work item with what the page shows of it.
#[derive(Debug, Clone)]
pub struct WorkItemRow {
    pub key: WorkItemKey,
    /// Where the item is in Jira; absent when the key is not one a link can be made of.
    pub url: Option<SafeUrl>,
    pub summary: Redacted<String>,
    pub creator: Option<Redacted<String>>,
    pub description: Option<Teaser>,
    /// Every session whose links include the key: the active ones in the order of the
    /// sessions page, then the dormant ones, newest first.
    pub sessions: Vec<SessionId>,
    /// The comments the user has not seen; `None` until the item's comments are read.
    pub comments: Option<Unseen>,
    /// The comments by others since the user's own latest one, whatever they marked seen: what
    /// still waits on their reply. `None` until the item's comments are read.
    pub unanswered: Option<Unseen>,
    pub kind: RowKind,
    /// The open tickets Jira says this one waits for, by a "Blocks" or "Depends On" link;
    /// empty when it waits for none, or its links are not read.
    pub waits_on: Vec<Blocker>,
    /// The epic the item belongs to; `None` for an epic, an item in none, or one whose view
    /// is not read yet.
    pub epic: Option<EpicRef>,
}

impl WorkItemRow {
    /// Whether the item waits for an open ticket: it cannot be picked up yet.
    pub fn is_waiting(&self) -> bool {
        !self.waits_on.is_empty()
    }

    /// The row's key as a Jira key, whatever its project; `None` for text that is not one.
    fn jira_key(&self) -> Option<JiraKey> {
        JiraKey::parse(&self.key.to_string())
    }
}

/// An epic an item belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpicRef {
    pub key: JiraKey,
    pub summary: Redacted<String>,
    pub url: Option<SafeUrl>,
}

/// What kind of item a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Item,
    /// An epic, with how far its children have come.
    Epic(Children),
}

/// An epic's children as last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Children {
    NotRead,
    /// The latest read failed; the health page says why.
    Unreadable,
    Counted(Progress),
}

/// An open ticket a row waits for, with its page when the key makes a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub key: JiraKey,
    pub url: Option<SafeUrl>,
}

/// What is read about tickets beyond the list, joined to the rows: the open dependencies, and
/// the epics' children.
pub struct ItemRelations<'a> {
    pub dependencies: &'a [Dependency],
    pub children: &'a PerTicket<Progress>,
}

/// What the comment badges are made of: the comment reads, and the latest "seen" mark of each
/// configured ticket.
pub struct CommentMarks<'a> {
    pub reads: &'a jira::comments::Reads,
    pub seen_through: &'a dyn Fn(&JiraKey) -> Option<Instant>,
}

impl WorkItemsView {
    /// The view of what the cache read, joined to `sessions`, `dormant`, the comments and the
    /// relations; `reading` is `None` where there is no Jira.
    pub fn of(
        reading: Option<&jira::Reading>,
        targets: &LinkTargets,
        sessions: &[SessionView],
        dormant: &[SessionView],
        comments: Option<&CommentMarks<'_>>,
        relations: Option<&ItemRelations<'_>>,
    ) -> Self {
        let Some(reading) = reading else {
            return Self::NoJira;
        };
        match reading {
            jira::Reading::Unread => Self::NeverRead,
            jira::Reading::Failure(failure) => Self::Failed(WorkItemsFailure::of(failure)),
            jira::Reading::Value {
                observed, failure, ..
            } => {
                let mentions = mentions(sessions.iter().chain(dormant));
                Self::Listed {
                    read_at: observed.read_at,
                    sections: sections(observed.items.iter(), |item| {
                        row(item, targets, &mentions, comments, relations)
                    }),
                    failure: failure.as_ref().map(WorkItemsFailure::of),
                }
            }
        }
    }
}

impl WorkItemsView {
    /// The listed item whose key Jira wrote as `key`.
    pub fn row(&self, key: &str) -> Option<&WorkItemRow> {
        match self {
            Self::Listed { sections, .. } => sections
                .iter()
                .flat_map(StatusSection::rows)
                .find(|row| row.key.to_string() == key),
            Self::NoJira | Self::NeverRead | Self::Failed(_) => None,
        }
    }
}

/// What reading the work items last did, as the health page states it: a list read at some
/// time and not failed since is `Ok`; any failure, with or without an older list, is `Failed`;
/// `None` where there is no Jira, which is no source rather than a source not read.
pub fn source_state(reading: Option<&jira::Reading>) -> Option<SourceState> {
    reading.map(read_state)
}

fn read_state(reading: &jira::Reading) -> SourceState {
    let failed = |failure: &jira::Failed| SourceState::Failed {
        at: failure.at,
        reason: failure.error.to_string(),
    };
    match reading {
        jira::Reading::Unread => SourceState::NeverRead,
        jira::Reading::Failure(failure)
        | jira::Reading::Value {
            failure: Some(failure),
            ..
        } => failed(failure),
        jira::Reading::Value {
            observed,
            failure: None,
            ..
        } => SourceState::Ok {
            at: observed.read_at,
        },
    }
}

/// The Jira link a work item's key is, when it is one.
///
/// A key of a configured project is the link the extractor makes of it in a session's text;
/// any other key is a link only if it reads as an issue key, so a key of another project
/// still has its page. Nothing else is a link: text that is a URL is not a key.
fn jira_link(key: &WorkItemKey) -> Option<Link> {
    match key {
        WorkItemKey::Configured(key) => Some(Link::Jira { key: key.clone() }),
        WorkItemKey::Other(text) => text
            .parse::<Link>()
            .ok()
            .filter(|link| matches!(link, Link::Jira { .. })),
    }
}

/// The sessions that mention each Jira link, in the order given.
fn mentions<'a>(views: impl Iterator<Item = &'a SessionView>) -> HashMap<&'a Link, Vec<SessionId>> {
    let mut mentions: HashMap<&Link, Vec<SessionId>> = HashMap::new();
    for view in views {
        for link in view
            .links
            .iter()
            .map(|view| &view.link)
            .filter(|link| matches!(link, Link::Jira { .. }))
        {
            let sessions = mentions.entry(link).or_default();
            // A session lists a link once; the model's links are distinct.
            if !sessions.contains(&view.session.id) {
                sessions.push(view.session.id.clone());
            }
        }
    }
    mentions
}

/// What the user has not seen of the item's comments, and what still waits on their reply;
/// both `None` until the comments are read. Only a configured key can carry a "seen" mark,
/// since only those are owned by a session.
fn comments_of(
    item: &WorkItem,
    comments: Option<&CommentMarks<'_>>,
) -> (Option<Unseen>, Option<Unseen>) {
    let Some(marks) = comments else {
        return (None, None);
    };
    let Some(read) = marks.reads.thread(&item.key.to_string()) else {
        return (None, None);
    };
    let seen_through = match &item.key {
        WorkItemKey::Configured(key) => (marks.seen_through)(key),
        WorkItemKey::Other(_) => None,
    };
    (
        Some(jira::comments::unseen(&read.thread, seen_through)),
        Some(jira::comments::unseen(&read.thread, None)),
    )
}

/// The epic `item` belongs to, from the view the comments are read with. An epic belongs to
/// none here, whatever its own parent, so the tree stays one level deep.
fn epic_of(
    item: &WorkItem,
    targets: &LinkTargets,
    comments: Option<&CommentMarks<'_>>,
) -> Option<EpicRef> {
    if item.level == Level::Epic {
        return None;
    }
    let read = comments?.reads.thread(&item.key.to_string())?;
    match &read.epic {
        EpicOf::NotAsked | EpicOf::NoEpic => None,
        EpicOf::Epic { key, summary } => Some(EpicRef {
            key: key.clone(),
            summary: Redacted::of(summary),
            url: url_of(&Link::Jira { key: key.clone() }, targets),
        }),
    }
}

/// What kind of row `item` is: an epic carries its children's count as last read.
fn kind_of(item: &WorkItem, relations: Option<&ItemRelations<'_>>) -> RowKind {
    match item.level {
        Level::Item => RowKind::Item,
        Level::Epic => {
            let read = JiraKey::parse(&item.key.to_string())
                .zip(relations)
                .and_then(|(key, relations)| relations.children.get(&key));
            RowKind::Epic(match read {
                None => Children::NotRead,
                Some(Err(_)) => Children::Unreadable,
                Some(Ok(progress)) => Children::Counted(*progress),
            })
        }
    }
}

/// The open tickets `item` waits for, in the order the dependencies are listed.
fn waits_on(
    item: &WorkItem,
    targets: &LinkTargets,
    relations: Option<&ItemRelations<'_>>,
) -> Vec<Blocker> {
    let Some((key, relations)) = JiraKey::parse(&item.key.to_string()).zip(relations) else {
        return Vec::new();
    };
    relations
        .dependencies
        .iter()
        .filter(|dependency| dependency.blocked == key)
        .map(|dependency| Blocker {
            key: dependency.blocker.clone(),
            url: url_of(
                &Link::Jira {
                    key: dependency.blocker.clone(),
                },
                targets,
            ),
        })
        .collect()
}

/// The page of `link`; a link that cannot be rendered is logged and has none.
fn url_of(link: &Link, targets: &LinkTargets) -> Option<SafeUrl> {
    match link.url(targets) {
        Ok(url) => Some(url),
        Err(error) => {
            tracing::warn!(%link, %error, "a Jira key cannot be rendered as a URL");
            None
        }
    }
}

fn row(
    item: &WorkItem,
    targets: &LinkTargets,
    mentions: &HashMap<&Link, Vec<SessionId>>,
    comments: Option<&CommentMarks<'_>>,
    relations: Option<&ItemRelations<'_>>,
) -> WorkItemRow {
    let link = jira_link(&item.key);
    let sessions = link
        .as_ref()
        .and_then(|link| mentions.get(link))
        .cloned()
        .unwrap_or_default();
    let (unseen, unanswered) = comments_of(item, comments);
    WorkItemRow {
        key: item.key.clone(),
        url: link.as_ref().and_then(|link| url_of(link, targets)),
        kind: kind_of(item, relations),
        waits_on: waits_on(item, targets, relations),
        epic: epic_of(item, targets, comments),
        summary: Redacted::of(&item.summary),
        creator: item
            .creator
            .as_ref()
            .map(|person| Redacted::of(&person.display_name)),
        description: item.description.as_deref().map(teaser),
        sessions,
        comments: unseen,
        unanswered,
    }
}

/// The first [`DESCRIPTION_CHARS`] characters of the description, cut after redaction so a
/// credential that straddles the cut is masked whole.
fn teaser(description: &str) -> Teaser {
    let redacted = Redacted::of(description);
    Teaser {
        cut: redacted.as_str().chars().count() > DESCRIPTION_CHARS,
        text: redacted.prefix(DESCRIPTION_CHARS),
    }
}

#[cfg(test)]
mod tests {
    //! Goal: the join from a work item's key to the sessions that mention it, and the view
    //! each state of the cache becomes.
    //!
    //! Method: a hand-written model of five sessions and a hand-written list of items; every
    //! expected session list is written out from who mentions what.

    use std::num::NonZeroU32;

    use super::*;
    use crate::jira::{Failed, Observed, Person, Status, StatusCategory};
    use crate::testutil::{ago, link_targets, session_id as id, session_view};
    use crate::types::{Activity, Age, JiraKey, JiraPrefix};

    fn key(prefix: &str, n: u32) -> JiraKey {
        JiraKey::new(
            JiraPrefix::parse(prefix).unwrap(),
            NonZeroU32::new(n).unwrap(),
        )
    }

    fn mentioning(n: u8, activity: Activity, keys: &[(&str, u32)]) -> SessionView {
        let mut view = session_view(n, Some("a session"), activity);
        for (prefix, number) in keys {
            let link = Link::Jira {
                key: key(prefix, *number),
            };
            view.links.push(crate::model::LinkView {
                url: link.url(&link_targets()).unwrap(),
                link,
                pr: None,
            });
        }
        view
    }

    fn item(key: WorkItemKey, description: Option<&str>) -> WorkItem {
        WorkItem {
            key,
            summary: "Fix the parser".to_owned(),
            status: Status {
                name: "In Progress".to_owned(),
                category: StatusCategory::InProgress,
            },
            issue_type: "Task".to_owned(),
            level: Level::Item,
            priority: None,
            creator: Some(Person {
                display_name: "Ada Example".to_owned(),
            }),
            assignee: None,
            updated: Some(ago(60)),
            created: None,
            description: description.map(str::to_owned),
        }
    }

    fn good(items: Vec<WorkItem>, failure: Option<Failed>) -> jira::Reading {
        jira::Reading::Value {
            observed: Observed {
                items,
                read_at: ago(300),
            },
            age: Age::Minutes(5),
            failure,
        }
    }

    fn running() -> Activity {
        Activity::Running { since: ago(5) }
    }

    fn dormant() -> Activity {
        Activity::Dormant {
            last_seen: ago(500),
            background: None,
        }
    }

    /// Every row, section after section.
    fn rows(view: &WorkItemsView) -> Vec<&WorkItemRow> {
        let WorkItemsView::Listed { sections, .. } = view else {
            panic!("a read list is `Listed`: {view:?}");
        };
        sections.iter().flat_map(StatusSection::rows).collect()
    }

    /// Sections come `In Progress`, then `To Do`, then the rest by name, whatever order or
    /// spelling the statuses arrive in; each holds its items in the order they came, and no
    /// status without items has one.
    /// Mutation: order the places by name only (`Dev` comes first), drop the fold (`to do`
    /// is no longer second), or sort rows within a section by key (`ABC-5` before `ABC-9`).
    #[test]
    fn sections_are_in_progress_then_to_do_then_the_rest_by_name() {
        let with_status = |n, status: &str| WorkItem {
            status: Status {
                name: status.to_owned(),
                category: StatusCategory::Other("unspecified".to_owned()),
            },
            ..item(WorkItemKey::Configured(key("ABC", n)), None)
        };
        let items = vec![
            with_status(9, "Prod"),
            with_status(8, "to do"),
            with_status(7, "Dev"),
            with_status(9, "In Progress"),
            with_status(5, "In Progress"),
            with_status(4, "Code Review"),
        ];
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            None,
            None,
        );
        let WorkItemsView::Listed { sections, .. } = &view else {
            panic!("a read list is `Listed`");
        };
        let shown: Vec<(&str, Vec<String>)> = sections
            .iter()
            .map(|section| {
                (
                    section.status.as_str(),
                    section.rows().map(|row| row.key.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("In Progress", vec!["ABC-9".to_owned(), "ABC-5".to_owned()]),
                ("to do", vec!["ABC-8".to_owned()]),
                ("Code Review", vec!["ABC-4".to_owned()]),
                ("Dev", vec!["ABC-7".to_owned()]),
                ("Prod", vec!["ABC-9".to_owned()]),
            ]
        );
    }

    /// Within a status, an item of an epic listed there is nested under the epic's row; items
    /// of an epic listed elsewhere, or not listed, are nested under one head naming it, placed
    /// where its first item is; an epic never nests, whatever its own parent says; an item in
    /// no epic stays at the top.
    /// Mutation, each ran red: nest under a head even when the epic is in the section, give
    /// each item of an outside epic its own head, or nest an epic under its parent.
    #[test]
    fn a_section_nests_each_item_under_its_epic_one_level_deep() {
        use crate::jira::comments::{Reads, Thread, Viewed};
        let in_status = |n, status: &str, level| WorkItem {
            status: Status {
                name: status.to_owned(),
                category: StatusCategory::InProgress,
            },
            level,
            ..item(WorkItemKey::Configured(key("ABC", n)), None)
        };
        let items = vec![
            in_status(1, "In Progress", Level::Item),
            in_status(5, "In Progress", Level::Epic),
            in_status(2, "In Progress", Level::Item),
            in_status(3, "In Progress", Level::Item),
            in_status(4, "In Progress", Level::Item),
            in_status(7, "In Progress", Level::Epic),
            in_status(8, "In Progress", Level::Item),
            in_status(6, "To Do", Level::Item),
        ];
        let dir = crate::testutil::Scratch::new("wi-epics");
        let mut reads = Reads::load(&dir);
        let viewed = |epic: EpicOf| Viewed {
            thread: Thread::Whole(Vec::new()),
            epic,
        };
        let in_epic = |n| {
            viewed(EpicOf::Epic {
                key: key("ABC", n),
                summary: format!("Epic {n}"),
            })
        };
        for (n, epic) in [(1, 5), (3, 9), (4, 5), (7, 9), (6, 5), (8, 9)] {
            reads.record(format!("ABC-{n}"), in_epic(epic), ago(10));
        }
        reads.record("ABC-2".to_owned(), viewed(EpicOf::NoEpic), ago(10));
        let marks = CommentMarks {
            reads: &reads,
            seen_through: &|_: &JiraKey| None,
        };
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            Some(&marks),
            None,
        );
        let WorkItemsView::Listed { sections, .. } = &view else {
            panic!("a read list is `Listed`: {view:?}");
        };
        let shape: Vec<Vec<(String, Vec<String>)>> = sections
            .iter()
            .map(|section| {
                section
                    .entries()
                    .iter()
                    .map(|entry| {
                        let head = match &entry.head {
                            Head::Row(row) => row.key.to_string(),
                            Head::Epic(epic) => {
                                format!("head {} {}", epic.key, epic.summary.as_str())
                            }
                        };
                        let children = entry.children.iter().map(|row| row.key.to_string());
                        (head, children.collect())
                    })
                    .collect()
            })
            .collect();
        let entry = |head: &str, children: &[&str]| {
            (
                head.to_owned(),
                children.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            shape,
            [
                vec![
                    entry("ABC-5", &["ABC-1", "ABC-4"]),
                    entry("ABC-2", &[]),
                    entry("head ABC-9 Epic 9", &["ABC-3", "ABC-8"]),
                    entry("ABC-7", &[]),
                ],
                vec![entry("head ABC-5 Epic 5", &["ABC-6"])],
            ]
        );
        let rows: Vec<String> = sections
            .first()
            .map_or_default(|section| section.rows().map(|row| row.key.to_string()).collect());
        assert_eq!(
            rows,
            [
                "ABC-5", "ABC-1", "ABC-4", "ABC-2", "ABC-3", "ABC-8", "ABC-7"
            ],
            "rows() walks the tree, skipping heads"
        );
    }

    /// An epic carries its children's count as last read, an item never does; an item waits on
    /// exactly the open tickets that block it, each linked; within a section the waiting items
    /// come after the ready ones, each group in the query's order.
    /// Mutation: give an `Item` a count, map a failed read to `NotRead`, match the dependency
    /// on its blocker instead of its blocked side, or drop the sort.
    #[test]
    fn rows_join_the_epics_children_and_the_open_blockers() {
        use crate::jira::children::Progress;
        use crate::jira::links::Dependency;
        let epic = |n| WorkItem {
            level: Level::Epic,
            ..item(WorkItemKey::Configured(key("ABC", n)), None)
        };
        let plain = |n| item(WorkItemKey::Configured(key("ABC", n)), None);
        let items = vec![plain(1), epic(2), plain(3), epic(4), epic(5), plain(6)];
        let mut children = PerTicket::new(jira::children::READ_EVERY);
        let three_of_four = [
            StatusCategory::Done,
            StatusCategory::Done,
            StatusCategory::Done,
            StatusCategory::InProgress,
        ];
        children.record(
            key("ABC", 2),
            Ok(Progress::count(three_of_four.iter())),
            ago(5),
        );
        children.record(key("ABC", 4), Err(JiraError::Timeout), ago(5));
        // ABC-1 waits on ABC-9 and ABC-8; ABC-3 blocks ABC-6, which waits on it.
        let dependencies = [(9, 1), (3, 6), (8, 1)].map(|(blocker, blocked)| Dependency {
            blocker: key("ABC", blocker),
            blocked: key("ABC", blocked),
        });
        let relations = ItemRelations {
            dependencies: &dependencies,
            children: &children,
        };
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            None,
            Some(&relations),
        );
        let shown: Vec<(String, RowKind, Vec<String>)> = rows(&view)
            .iter()
            .map(|row| {
                (
                    row.key.to_string(),
                    row.kind,
                    row.waits_on.iter().map(|b| b.key.to_string()).collect(),
                )
            })
            .collect();
        let none = Vec::<String>::new;
        assert_eq!(
            shown,
            [
                (
                    "ABC-2".to_owned(),
                    RowKind::Epic(Children::Counted(Progress::count(three_of_four.iter()))),
                    none()
                ),
                ("ABC-3".to_owned(), RowKind::Item, none()),
                (
                    "ABC-4".to_owned(),
                    RowKind::Epic(Children::Unreadable),
                    none()
                ),
                ("ABC-5".to_owned(), RowKind::Epic(Children::NotRead), none()),
                (
                    "ABC-1".to_owned(),
                    RowKind::Item,
                    vec!["ABC-9".to_owned(), "ABC-8".to_owned()]
                ),
                ("ABC-6".to_owned(), RowKind::Item, vec!["ABC-3".to_owned()]),
            ]
        );
        let first_blocker = rows(&view)
            .iter()
            .find(|row| row.key.to_string() == "ABC-1")
            .and_then(|row| row.waits_on.first().cloned())
            .unwrap();
        assert_eq!(
            first_blocker.url.unwrap().as_str(),
            "https://example.atlassian.net/browse/ABC-9"
        );
    }

    /// Without the relations nothing is known of them: an epic's children are not read and
    /// nothing waits.
    /// Mutation: map a missing relation read to `Counted` with no children.
    #[test]
    fn without_relations_an_epic_is_not_read_and_nothing_waits() {
        let epic = WorkItem {
            level: Level::Epic,
            ..item(WorkItemKey::Configured(key("ABC", 2)), None)
        };
        let plain = item(WorkItemKey::Configured(key("ABC", 1)), None);
        let bare = WorkItemsView::of(
            Some(&good(vec![epic, plain], None)),
            &link_targets(),
            &[],
            &[],
            None,
            None,
        );
        let kinds: Vec<(RowKind, bool)> = rows(&bare)
            .iter()
            .map(|row| (row.kind, row.is_waiting()))
            .collect();
        assert_eq!(
            kinds,
            [
                (RowKind::Epic(Children::NotRead), false),
                (RowKind::Item, false)
            ]
        );
    }

    /// Mutation: look only at active sessions (ran red).
    #[test]
    fn a_row_lists_every_session_that_mentions_its_key_active_ones_first() {
        let sessions = [
            mentioning(1, running(), &[("ABC", 1), ("ABC", 2)]),
            mentioning(2, running(), &[("ABC", 1)]),
            mentioning(3, running(), &[("DEF", 2)]),
        ];
        let dormant_ones = [
            mentioning(4, dormant(), &[("ABC", 1)]),
            mentioning(5, dormant(), &[("XYZ", 9)]),
        ];
        let items = vec![
            item(WorkItemKey::Configured(key("ABC", 1)), None),
            item(WorkItemKey::Configured(key("ABC", 2)), None),
            item(WorkItemKey::Configured(key("ABC", 4)), None),
            item(WorkItemKey::Other("XYZ-9".to_owned()), None),
        ];
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &sessions,
            &dormant_ones,
            None,
            None,
        );
        let joined: Vec<(String, Vec<SessionId>)> = rows(&view)
            .iter()
            .map(|row| (row.key.to_string(), row.sessions.clone()))
            .collect();
        assert_eq!(
            joined,
            vec![
                ("ABC-1".to_owned(), vec![id(1), id(2), id(4)]),
                ("ABC-2".to_owned(), vec![id(1)]),
                ("ABC-4".to_owned(), vec![]),
                ("XYZ-9".to_owned(), vec![id(5)]),
            ]
        );
    }

    /// The rows keep the order the query returned, and each key links to the configured host.
    /// No mutation run on this test by itself; the URL format is pinned again through the
    /// session page's chip.
    #[test]
    fn rows_keep_the_query_order_and_link_to_the_configured_host() {
        let items = vec![
            item(WorkItemKey::Configured(key("ABC", 7)), None),
            item(WorkItemKey::Other("XYZ-9".to_owned()), None),
            item(WorkItemKey::Configured(key("ABC", 1)), None),
            item(
                WorkItemKey::Other("https://evil.example/x".to_owned()),
                None,
            ),
        ];
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            None,
            None,
        );
        let urls: Vec<(String, Option<String>)> = rows(&view)
            .iter()
            .map(|row| {
                (
                    row.key.to_string(),
                    row.url.as_ref().map(|url| url.as_str().to_owned()),
                )
            })
            .collect();
        assert_eq!(
            urls,
            vec![
                (
                    "ABC-7".to_owned(),
                    Some("https://example.atlassian.net/browse/ABC-7".to_owned())
                ),
                (
                    "XYZ-9".to_owned(),
                    Some("https://example.atlassian.net/browse/XYZ-9".to_owned())
                ),
                (
                    "ABC-1".to_owned(),
                    Some("https://example.atlassian.net/browse/ABC-1".to_owned())
                ),
                ("https://evil.example/x".to_owned(), None),
            ]
        );
    }

    /// The description is redacted and then cut at 200 characters, and says it was cut.
    /// Mutation: cut before redacting (ran red).
    #[test]
    fn a_description_is_redacted_then_cut_at_two_hundred_characters() {
        let secret = "ghp_aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let straddling = format!("{} {secret} tail", "x".repeat(174));
        let exact = "y".repeat(200);
        let long = "z".repeat(201);
        let items = vec![
            item(WorkItemKey::Configured(key("ABC", 1)), Some(&straddling)),
            item(WorkItemKey::Configured(key("ABC", 2)), Some(&exact)),
            item(WorkItemKey::Configured(key("ABC", 3)), Some(&long)),
            item(WorkItemKey::Configured(key("ABC", 4)), None),
        ];
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            None,
            None,
        );
        let teasers: Vec<Option<Teaser>> = rows(&view)
            .iter()
            .map(|row| row.description.clone())
            .collect();
        assert_eq!(
            teasers,
            vec![
                Some(Teaser {
                    text: Redacted::of(&format!("{} [redacted] tail", "x".repeat(174))),
                    cut: false,
                }),
                Some(Teaser {
                    text: Redacted::of(&exact),
                    cut: false,
                }),
                Some(Teaser {
                    text: Redacted::of(&"z".repeat(200)),
                    cut: true,
                }),
                None,
            ]
        );
    }

    /// Each state of the cache is its own view, and a failure keeps the list beside it.
    /// No mutation run on this test.
    #[test]
    fn each_state_of_the_cache_is_its_own_view() {
        let unauthenticated = JiraError::NotAuthenticated {
            hint: "run `acli jira auth login`",
        };
        let failed = |error| Failed { error, at: ago(30) };
        let view = |reading: &jira::Reading| {
            WorkItemsView::of(Some(reading), &link_targets(), &[], &[], None, None)
        };

        assert!(matches!(
            WorkItemsView::of(None, &link_targets(), &[], &[], None, None),
            WorkItemsView::NoJira
        ));
        assert!(matches!(
            view(&jira::Reading::Unread),
            WorkItemsView::NeverRead
        ));

        let WorkItemsView::Failed(failure) =
            view(&jira::Reading::Failure(failed(unauthenticated.clone())))
        else {
            panic!("a failure with no list is `Failed`");
        };
        assert_eq!(
            failure,
            WorkItemsFailure {
                at: ago(30),
                fault: WorkItemsFault::NotAuthenticated {
                    hint: "run `acli jira auth login`"
                },
            }
        );

        let WorkItemsView::Failed(failure) =
            view(&jira::Reading::Failure(failed(JiraError::Timeout)))
        else {
            panic!("a failure with no list is `Failed`");
        };
        assert_eq!(
            failure.fault,
            WorkItemsFault::Failed {
                reason: Redacted::of("acli did not answer within 20 s")
            }
        );

        let stale = view(&good(
            vec![item(WorkItemKey::Configured(key("ABC", 1)), None)],
            Some(failed(unauthenticated)),
        ));
        assert_eq!(rows(&stale).len(), 1);
        let WorkItemsView::Listed {
            read_at,
            failure: Some(failure),
            ..
        } = stale
        else {
            panic!("a list with a later failure is `Listed` with the failure");
        };
        assert_eq!(read_at, ago(300));
        assert!(matches!(
            failure.fault,
            WorkItemsFault::NotAuthenticated { .. }
        ));

        let fresh = view(&good(vec![], None));
        assert!(matches!(fresh, WorkItemsView::Listed { failure: None, .. }));
    }

    /// The health row: a list with no later failure is read at its time; a failure, with or
    /// without an older list, is failed at its own time.
    /// Mutation: report `Ok` for a list that a later failure followed (ran red).
    #[test]
    fn the_health_state_is_failed_whenever_the_latest_read_failed() {
        let failure = || Failed {
            error: JiraError::NotInstalled,
            at: ago(30),
        };
        assert_eq!(source_state(None), None);
        assert_eq!(read_state(&jira::Reading::Unread), SourceState::NeverRead);
        assert_eq!(
            read_state(&good(vec![], None)),
            SourceState::Ok { at: ago(300) }
        );
        let failed = SourceState::Failed {
            at: ago(30),
            reason: "acli is not installed or not on PATH".to_owned(),
        };
        assert_eq!(read_state(&good(vec![], Some(failure()))), failed);
        assert_eq!(read_state(&jira::Reading::Failure(failure())), failed);
    }

    /// A row's count joins its read thread to its own ticket's "seen" mark; an item whose
    /// comments are not read has none; a key outside the configured projects has no mark.
    /// Mutation: look the thread up under another key, pass every row the same mark, or give
    /// an unread item `Caught`.
    #[test]
    fn a_row_counts_its_comments_after_its_own_seen_mark() {
        use crate::jira::comments::{Author, Comment, Reads, Thread, Viewed};
        let dir = crate::testutil::Scratch::new("wi-comments");
        let mut reads = Reads::load(&dir);
        let others = |times: &[i64]| Viewed {
            thread: Thread::Whole(
                times
                    .iter()
                    .map(|secs| Comment {
                        author: Author::Other,
                        created: ago(*secs),
                    })
                    .collect(),
            ),
            epic: EpicOf::NoEpic,
        };
        reads.record("ABC-1".to_owned(), others(&[300, 200]), ago(10));
        reads.record("ABC-2".to_owned(), others(&[300, 200]), ago(10));
        reads.record("XYZ-1".to_owned(), others(&[300]), ago(10));
        let marked = key("ABC", 1);
        let seen_through = |ticket: &JiraKey| (*ticket == marked).then(|| ago(250));
        let marks = CommentMarks {
            reads: &reads,
            seen_through: &seen_through,
        };
        let abc = [JiraPrefix::parse("ABC").unwrap()];
        let items = ["ABC-1", "ABC-2", "ABC-3", "XYZ-1"]
            .map(|text| item(WorkItemKey::classify(text, &abc), None))
            .to_vec();
        let view = WorkItemsView::of(
            Some(&good(items, None)),
            &link_targets(),
            &[],
            &[],
            Some(&marks),
            None,
        );
        let new = |count: usize| {
            Some(Unseen::New {
                count: std::num::NonZeroUsize::new(count).unwrap(),
                newest: ago(200),
            })
        };
        let counts: Vec<Option<Unseen>> = rows(&view).iter().map(|row| row.comments).collect();
        assert_eq!(
            counts,
            [
                new(1),
                new(2),
                None,
                Some(Unseen::New {
                    count: std::num::NonZeroUsize::MIN,
                    newest: ago(300),
                }),
            ]
        );
    }
}
