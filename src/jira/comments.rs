//! The comments on the user's open work items, and how many of them they have not seen.
//!
//! `acli jira workitem search` refuses the `comment` and `updated` fields, so comments come
//! from `acli jira workitem view <key> --json --fields key,comment,parent`, one ticket per call. To keep
//! that to the tickets that changed, a pass first asks the search which of the open items were
//! updated since the last pass (JQL filters on `updated` though it will not return it), and
//! reads only those, plus any open item never read. The reads are saved, so a restart does not
//! read every ticket again.
//!
//! A comment is unseen when someone else wrote it after the later of two marks: the user's own
//! latest comment on the ticket (replying means they read the thread) and the mark the inbox's
//! "seen" made on the card of the session that owns the ticket.
//!
//! The same view names the epic the ticket belongs to (its `parent`), which the search refuses
//! as well; a ticket moves between epics rarely, and moving it updates it, so the reads that
//! follow comments follow epics too.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ACLI, ACLI_TIMEOUT, EpicOf, JiraError, LIMIT, LIMIT_ITEMS, MINE, classify_failure};
use crate::types::Instant;
use crate::util::fs::write_private;
use crate::util::runner::{CommandRunner, run_tool};

/// The file in the state directory that holds the reads.
pub const FILE_NAME: &str = "jira-comments.json";

/// How often the open items are asked which of them changed. Each ask is one search; a comment
/// shows up on the page within this, plus the minute the reader sleeps between passes.
pub const CHANGES_EVERY: SignedDuration = SignedDuration::from_mins(2);

/// How far before the last ask the next one looks. JQL compares `updated` to the minute, and an
/// update made while a pass ran must be caught by the next one.
const OVERLAP: SignedDuration = SignedDuration::from_mins(2);

/// The widest window asked about; a reader that slept longer (no page open for days) reads
/// every open item again instead, which costs the same as one backfill.
const WINDOW_MAX: SignedDuration = SignedDuration::from_hours(7 * 24);

/// A Jira user's account id: never empty. Only the user's own is kept; another author is
/// told apart from the user's and then forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);

impl AccountId {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self(text.to_owned()))
    }
}

impl TryFrom<String> for AccountId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or("an account id cannot be empty")
    }
}

impl From<AccountId> for String {
    fn from(id: AccountId) -> Self {
        id.0
    }
}

/// Who wrote a comment, as far as counting needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Author {
    Me,
    Other,
}

/// One comment: who wrote it and when. The text is never kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: Author,
    pub created: Instant,
}

/// A ticket's comments, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Thread {
    /// Every comment the ticket has.
    Whole(Vec<Comment>),
    /// Jira returned fewer comments than it says the ticket has, so which are the latest is not
    /// known.
    Partial { returned: usize, total: usize },
}

/// How many comments on a ticket the user has not seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unseen {
    /// None: every comment is the user's, older than their marks, or there are none.
    Caught,
    /// `count` comments by others after the user's marks; `newest` is when the latest was written,
    /// the mark that seeing them makes.
    New {
        count: NonZeroUsize,
        newest: Instant,
    },
    /// The thread was cut short, so the latest comments were not read.
    Unknown,
}

/// What the user has not seen of `thread`: comments by others after both their own latest
/// comment and `seen_through`.
pub fn unseen(thread: &Thread, seen_through: Option<Instant>) -> Unseen {
    let comments = match thread {
        Thread::Whole(comments) => comments,
        Thread::Partial { .. } => return Unseen::Unknown,
    };
    let replied = comments
        .iter()
        .filter(|comment| comment.author == Author::Me)
        .map(|comment| comment.created)
        .max();
    // The watermark is never before the user's own latest comment, so every comment after it
    // is someone else's.
    let watermark = replied.max(seen_through);
    let new: Vec<Instant> = comments
        .iter()
        .filter(|comment| watermark.is_none_or(|mark| comment.created > mark))
        .map(|comment| comment.created)
        .collect();
    match (NonZeroUsize::new(new.len()), new.iter().max()) {
        (Some(count), Some(&newest)) => Unseen::New { count, newest },
        (None, _) | (Some(_), None) => Unseen::Caught,
    }
}

/// What one view of a ticket says: its comments, and the epic it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewed {
    pub thread: Thread,
    pub epic: EpicOf,
}

/// The comments and the epic of the ticket `key`, asked of Jira through `acli`; `me` tells the
/// user's own comments apart.
pub async fn fetch(
    runner: &dyn CommandRunner,
    key: &str,
    me: &AccountId,
) -> Result<Viewed, JiraError> {
    let args = [
        "jira",
        "workitem",
        "view",
        key,
        "--json",
        "--fields",
        "key,comment,parent",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse(&output.stdout, me)
}

/// The comments and the epic in one ticket's `view --json` output.
pub fn parse(stdout: &[u8], me: &AccountId) -> Result<Viewed, JiraError> {
    let unparseable = |detail: &str| JiraError::Unparseable {
        detail: detail.to_owned(),
    };
    let json: Value =
        serde_json::from_slice(stdout).map_err(|_| unparseable("the output is not JSON"))?;
    let fields = json.get("fields").ok_or_else(|| unparseable("no fields"))?;
    Ok(Viewed {
        thread: parse_thread(fields, me)?,
        epic: EpicOf::parse(fields)?,
    })
}

/// The comments in a view's `fields`.
fn parse_thread(fields: &Value, me: &AccountId) -> Result<Thread, JiraError> {
    let unparseable = |detail: &str| JiraError::Unparseable {
        detail: detail.to_owned(),
    };
    let field = fields
        .get("comment")
        .ok_or_else(|| unparseable("no comment field"))?;
    let listed = field
        .get("comments")
        .and_then(Value::as_array)
        .ok_or_else(|| unparseable("comment.comments is not a list"))?;
    let total = field
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|total| usize::try_from(total).ok())
        .ok_or_else(|| unparseable("comment.total is not a count"))?;
    if listed.len() < total {
        tracing::warn!(
            returned = listed.len(),
            total,
            "Jira returned part of a ticket's comments"
        );
        return Ok(Thread::Partial {
            returned: listed.len(),
            total,
        });
    }
    let mut comments = listed
        .iter()
        .map(|comment| {
            parse_comment(comment, me).ok_or_else(|| unparseable("a comment has no author or time"))
        })
        .collect::<Result<Vec<Comment>, JiraError>>()?;
    comments.sort_by_key(|comment| comment.created);
    Ok(Thread::Whole(comments))
}

fn parse_comment(comment: &Value, me: &AccountId) -> Option<Comment> {
    let author = comment
        .get("author")
        .and_then(|author| author.get("accountId"))
        .and_then(Value::as_str)
        .and_then(AccountId::parse)?;
    let created = comment.get("created").and_then(Value::as_str)?;
    let created = super::jira_time(created)?;
    Some(Comment {
        author: if author == *me {
            Author::Me
        } else {
            Author::Other
        },
        created,
    })
}

/// What an ask for changed items found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// The items' keys; `None` when the answer filled a page, so some may be missing.
    pub keys: Option<Vec<String>>,
    /// The user's account id, the assignee of every item a `currentUser()` query returns;
    /// `None` when it returned none.
    pub me: Option<AccountId>,
}

/// The open items updated within `window` of now, by Jira's clock; every open item when
/// `window` is `None`.
pub async fn changed(
    runner: &dyn CommandRunner,
    window: Option<SignedDuration>,
) -> Result<Changed, JiraError> {
    let query = match window {
        Some(window) => format!("{MINE} AND updated >= -{}m", window_minutes(window)),
        None => MINE.to_owned(),
    };
    let args = [
        "jira",
        "workitem",
        "search",
        "--jql",
        query.as_str(),
        "--json",
        "--limit",
        LIMIT,
        "--fields",
        "key,assignee",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse_changed(&output.stdout)
}

/// The keys and the assignee in a `search --json --fields key,assignee` output.
pub fn parse_changed(stdout: &[u8]) -> Result<Changed, JiraError> {
    let unparseable = |detail: &str| JiraError::Unparseable {
        detail: detail.to_owned(),
    };
    let json: Value =
        serde_json::from_slice(stdout).map_err(|_| unparseable("the output is not JSON"))?;
    let rows = json
        .as_array()
        .ok_or_else(|| unparseable("the output is not a list"))?;
    let keys = rows
        .iter()
        .map(|row| {
            row.get("key")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| unparseable("an item has no key"))
        })
        .collect::<Result<Vec<String>, JiraError>>()?;
    let me = rows.iter().find_map(|row| {
        row.get("fields")
            .and_then(|fields| fields.get("assignee"))
            .and_then(|assignee| assignee.get("accountId"))
            .and_then(Value::as_str)
            .and_then(AccountId::parse)
    });
    let full = keys.len() >= LIMIT_ITEMS;
    if full {
        tracing::warn!(
            limit = LIMIT_ITEMS,
            "the changed items filled a page; every open item is read again"
        );
    }
    Ok(Changed {
        keys: (!full).then_some(keys),
        me,
    })
}

/// `window` in whole minutes, rounded up, at least one: JQL's relative times take minutes.
fn window_minutes(window: SignedDuration) -> i64 {
    let seconds = window.as_secs().max(1);
    seconds.div_euclid(60) + i64::from(seconds.rem_euclid(60) > 0)
}

/// Why the saved reads could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommentsFileError {
    #[error("cannot read {}: {detail}", path.display())]
    Unreadable { path: PathBuf, detail: String },
    /// The file was read and is not what this tool wrote. Everything in it can be read from
    /// Jira again, so the reads start over and the next save replaces it.
    #[error("{} is not the comment reads file: {detail}", path.display())]
    Corrupt { path: PathBuf, detail: String },
    #[error("cannot save {}: {detail}", path.display())]
    Write { path: PathBuf, detail: String },
}

/// The reads of every open item's comments, and how far the asks for changed items reach.
#[derive(Debug)]
pub struct Reads {
    path: PathBuf,
    threads: BTreeMap<String, Read>,
    /// When the latest ask for changed items was made; every item changed before it, less the
    /// overlap, is read or in `due`. `None` before the first pass, or after a file that could
    /// not be used.
    asked: Option<Instant>,
    /// Items known to have changed since their last read.
    due: BTreeSet<String>,
    /// The user's account id, from the latest ask that returned an item.
    me: Option<AccountId>,
    /// The latest pass's failure, cleared by a pass that worked.
    failure: Option<(Instant, String)>,
}

/// One item's latest good read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read {
    pub at: Instant,
    pub thread: Thread,
    /// `NotAsked` in a file written before the epic was read, until the item is read again.
    #[serde(default)]
    pub epic: EpicOf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    asked: Option<Instant>,
    due: BTreeSet<String>,
    me: Option<AccountId>,
    threads: BTreeMap<String, Read>,
}

/// What a pass should do about changed items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Nothing: the last ask is recent.
    Not,
    /// Ask which items changed within this window of now.
    Within(SignedDuration),
    /// Read every open item: no ask has ever been made, or it was too long ago to trust.
    Everything,
}

impl Reads {
    /// The reads saved under `state_dir`. A missing file is no reads; a file that cannot be
    /// used is logged and the reads start over, reading every open item again.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let stored = match read_stored(&path) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::error!(%error, "the saved Jira comment reads cannot be used");
                None
            }
        };
        let (asked, due, me, threads) =
            stored.map_or_else(Default::default, |s| (s.asked, s.due, s.me, s.threads));
        Self {
            path,
            threads,
            asked,
            due,
            me,
            failure: None,
        }
    }

    /// The user's account id, once an ask has returned an item.
    pub const fn me(&self) -> Option<&AccountId> {
        self.me.as_ref()
    }

    /// What this pass should ask, at `now`.
    pub fn ask(&self, now: Instant) -> Ask {
        let Some(asked) = self.asked else {
            return Ask::Everything;
        };
        let since = now.duration_since(asked);
        if since.is_negative() || since > WINDOW_MAX {
            Ask::Everything
        } else if since < CHANGES_EVERY {
            Ask::Not
        } else {
            Ask::Within(since + OVERLAP)
        }
    }

    /// Files an ask made at `at`: the items it found are due, all of `open` when it could not
    /// list them all.
    pub fn asked(&mut self, at: Instant, changed: Changed, open: &[String]) {
        self.due
            .extend(changed.keys.unwrap_or_else(|| open.to_vec()));
        if let Some(me) = changed.me {
            self.me = Some(me);
        }
        self.asked = Some(at);
        self.failure = None;
    }

    /// Up to `limit` of the `open` items to read now: the changed ones, then those never read
    /// or read without their epic.
    pub fn to_read(&self, open: &[String], limit: usize) -> Vec<String> {
        let changed = open.iter().filter(|key| self.due.contains(*key));
        let unread = open.iter().filter(|key| {
            !self.due.contains(*key)
                && self
                    .threads
                    .get(*key)
                    .is_none_or(|read| read.epic == EpicOf::NotAsked)
        });
        changed.chain(unread).take(limit).cloned().collect()
    }

    pub fn record(&mut self, key: String, viewed: Viewed, at: Instant) {
        self.due.remove(&key);
        let Viewed { thread, epic } = viewed;
        self.threads.insert(key, Read { at, thread, epic });
    }

    /// Files a failure of the pass at `at`. A failed ticket stays due.
    pub fn failed(&mut self, at: Instant, error: &JiraError) {
        self.failure = Some((at, error.to_string()));
    }

    /// Forgets every item that is no longer open, and saves.
    pub fn keep_and_save(&mut self, open: &[String]) -> Result<(), CommentsFileError> {
        let open: HashSet<&String> = open.iter().collect();
        self.threads.retain(|key, _| open.contains(key));
        self.due.retain(|key| open.contains(key));
        save(
            &self.path,
            &Stored {
                asked: self.asked,
                due: self.due.clone(),
                me: self.me.clone(),
                threads: self.threads.clone(),
            },
        )
    }

    pub fn thread(&self, key: &str) -> Option<&Read> {
        self.threads.get(key)
    }

    /// Where the reads stand: the latest failure, the latest read, or none yet.
    pub fn state(&self) -> crate::model::SourceState {
        use crate::model::SourceState;
        if let Some((at, reason)) = &self.failure {
            return SourceState::Failed {
                at: *at,
                reason: reason.clone(),
            };
        }
        self.threads
            .values()
            .map(|read| read.at)
            .chain(self.asked)
            .max()
            .map_or(SourceState::NeverRead, |at| SourceState::Ok { at })
    }
}

fn read_stored(path: &Path) -> Result<Option<Stored>, CommentsFileError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CommentsFileError::Unreadable {
                path: path.to_owned(),
                detail: e.to_string(),
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| CommentsFileError::Corrupt {
            path: path.to_owned(),
            detail: e.to_string(),
        })
}

fn save(path: &Path, stored: &Stored) -> Result<(), CommentsFileError> {
    let write = |detail: String| CommentsFileError::Write {
        path: path.to_owned(),
        detail,
    };
    let mut bytes = serde_json::to_vec_pretty(stored).map_err(|e| write(e.to_string()))?;
    bytes.push(b'\n');
    write_private(path, &bytes).map_err(|e| write(e.to_string()))
}

#[cfg(test)]
mod tests {
    //! The parse against a masked `view --json --fields key,comment,parent` output of `acli`
    //! 1.3.40 (`tests/fixtures/jira/view_comments.json`, newest comment first as written there;
    //! the parent's shape is from a real view of an item in an epic, masked), the
    //! count against threads written out by hand, and the reads' bookkeeping driven by hand
    //! with the clock passed in. Expected times are converted from Jira's `-0500` by hand.

    use super::*;
    use crate::testutil::{Scratch, at};
    use crate::util::runner::{Call, Exit, FakeRunner, Output};

    const VIEW: &[u8] = include_bytes!("../../tests/fixtures/jira/view_comments.json");
    const ONE: &str = "000000000000000000000001";

    fn me() -> AccountId {
        AccountId::parse(ONE).unwrap()
    }

    fn comment(author: Author, created: &str) -> Comment {
        Comment {
            author,
            created: at(created),
        }
    }

    fn ok(stdout: &[u8]) -> Output {
        Output {
            exit: Exit::Success,
            stdout: stdout.to_vec(),
            stderr: String::new(),
        }
    }

    fn viewed(thread: Thread) -> Viewed {
        Viewed {
            thread,
            epic: EpicOf::NoEpic,
        }
    }

    fn thread_of(stdout: &[u8]) -> Thread {
        parse(stdout, &me()).unwrap().thread
    }

    /// Mutation: compare `author` to the wrong id, drop the sort, read `created` as UTC, or
    /// read the parent's own key or summary from the wrong place.
    #[test]
    fn the_fixture_parses_into_two_comments_oldest_first_mine_told_apart_and_its_epic() {
        assert_eq!(
            parse(VIEW, &me()).unwrap(),
            Viewed {
                thread: Thread::Whole(vec![
                    comment(Author::Me, "2026-11-06T15:00:00.100Z"),
                    comment(Author::Other, "2026-11-06T16:30:00.200Z"),
                ]),
                epic: EpicOf::Epic {
                    key: crate::types::JiraKey::parse("EX-7").unwrap(),
                    summary: "Placeholder epic".to_owned(),
                },
            }
        );
    }

    /// No parent, or a parent below the epic level, is no epic; a parent without a key, or an
    /// epic without a summary, is a bad shape.
    /// Mutation: take any parent for an epic, or read a missing parent as an error.
    #[test]
    fn only_an_epic_parent_is_an_epic() {
        let with_parent = |parent: Value| {
            serde_json::json!({"fields": {"comment": {"comments": [], "total": 0}, "parent": parent}})
                .to_string()
        };
        let story = serde_json::json!({"key": "EX-5", "fields": {"summary": "s", "issuetype": {"hierarchyLevel": 0}}});
        for no_epic in [Value::Null, story] {
            assert_eq!(
                parse(with_parent(no_epic).as_bytes(), &me()).unwrap().epic,
                EpicOf::NoEpic
            );
        }
        let absent = serde_json::json!({"fields": {"comment": {"comments": [], "total": 0}}});
        assert_eq!(
            parse(absent.to_string().as_bytes(), &me()).unwrap().epic,
            EpicOf::NoEpic
        );
        for bad in [
            serde_json::json!({"fields": {"issuetype": {"hierarchyLevel": 1}, "summary": "e"}}),
            serde_json::json!({"key": "EX-7", "fields": {"issuetype": {"hierarchyLevel": 1}}}),
        ] {
            assert!(matches!(
                parse(with_parent(bad).as_bytes(), &me()),
                Err(JiraError::Unparseable { .. })
            ));
        }
    }

    /// Mutation: treat a short page as whole, or fail it instead of saying "partial".
    #[test]
    fn fewer_comments_than_the_total_is_a_partial_thread_and_a_bad_shape_an_error() {
        let short = serde_json::json!({"fields": {"comment": {"comments": [], "total": 3}}});
        assert_eq!(
            thread_of(short.to_string().as_bytes()),
            Thread::Partial {
                returned: 0,
                total: 3
            }
        );
        let none = serde_json::json!({"fields": {"comment": {"comments": [], "total": 0}}});
        assert_eq!(
            thread_of(none.to_string().as_bytes()),
            Thread::Whole(Vec::new())
        );
        for bad in [
            serde_json::json!({"fields": {}}),
            serde_json::json!({"fields": {"comment": {"comments": [{"created": "x"}], "total": 1}}}),
        ] {
            assert!(matches!(
                parse(bad.to_string().as_bytes(), &me()),
                Err(JiraError::Unparseable { .. })
            ));
        }
        assert!(parse(b"not json", &me()).is_err());
    }

    /// Others' comments after the later of the user's own last comment and the mark count;
    /// the user's own never do; a partial thread is not known.
    /// Mutation: take the earlier of the two marks, or count a comment made exactly at the
    /// mark.
    #[test]
    fn unseen_counts_others_after_the_later_of_reply_and_mark() {
        let thread = Thread::Whole(vec![
            comment(Author::Other, "2026-11-06T09:00:00Z"),
            comment(Author::Me, "2026-11-06T10:00:00Z"),
            comment(Author::Other, "2026-11-06T11:00:00Z"),
            comment(Author::Other, "2026-11-06T12:00:00Z"),
            comment(Author::Me, "2026-11-06T12:30:00Z"),
            comment(Author::Other, "2026-11-06T13:00:00Z"),
            comment(Author::Other, "2026-11-06T14:00:00Z"),
        ]);
        let new = |count: usize, newest: &str| Unseen::New {
            count: NonZeroUsize::new(count).unwrap(),
            newest: at(newest),
        };
        assert_eq!(unseen(&thread, None), new(2, "2026-11-06T14:00:00Z"));
        assert_eq!(
            unseen(&thread, Some(at("2026-11-06T11:00:00Z"))),
            new(2, "2026-11-06T14:00:00Z"),
            "a mark before the reply changes nothing"
        );
        assert_eq!(
            unseen(&thread, Some(at("2026-11-06T13:00:00Z"))),
            new(1, "2026-11-06T14:00:00Z")
        );
        assert_eq!(
            unseen(&thread, Some(at("2026-11-06T14:00:00Z"))),
            Unseen::Caught
        );
        let never_replied = Thread::Whole(vec![comment(Author::Other, "2026-11-06T09:00:00Z")]);
        assert_eq!(unseen(&never_replied, None), new(1, "2026-11-06T09:00:00Z"));
        assert_eq!(unseen(&Thread::Whole(Vec::new()), None), Unseen::Caught);
        assert_eq!(
            unseen(
                &Thread::Partial {
                    returned: 50,
                    total: 60
                },
                None
            ),
            Unseen::Unknown
        );
    }

    /// Mutation: change the view's fields, or the changed query's window or fields.
    #[tokio::test]
    async fn asks_acli_for_exactly_these_commands() {
        let runner = FakeRunner::new([Ok(ok(VIEW)), Ok(ok(b"[]")), Ok(ok(b"[]"))]);
        fetch(&runner, "EX-1", &me()).await.unwrap();
        changed(&runner, Some(SignedDuration::from_secs(7 * 60 + 1)))
            .await
            .unwrap();
        changed(&runner, None).await.unwrap();
        let args: Vec<Vec<String>> = runner.calls().into_iter().map(|c: Call| c.args).collect();
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let search = |jql: &str| {
            owned(&[
                "jira",
                "workitem",
                "search",
                "--jql",
                jql,
                "--json",
                "--limit",
                "100",
                "--fields",
                "key,assignee",
            ])
        };
        assert_eq!(
            args,
            [
                owned(&[
                    "jira",
                    "workitem",
                    "view",
                    "EX-1",
                    "--json",
                    "--fields",
                    "key,comment,parent"
                ]),
                search("assignee = currentUser() AND resolution = Unresolved AND updated >= -8m"),
                search("assignee = currentUser() AND resolution = Unresolved"),
            ]
        );
    }

    /// Mutation: take the id from a row without an assignee, or keep the keys of a full page.
    #[test]
    fn a_changed_answer_names_its_keys_and_the_user() {
        let rows = serde_json::json!([
            {"key": "EX-1", "fields": {"assignee": null}},
            {"key": "EX-2", "fields": {"assignee": {"accountId": ONE, "displayName": "Person One"}}},
        ]);
        assert_eq!(
            parse_changed(rows.to_string().as_bytes()).unwrap(),
            Changed {
                keys: Some(vec!["EX-1".to_owned(), "EX-2".to_owned()]),
                me: Some(me()),
            }
        );
        let full: Vec<Value> = (1..=100)
            .map(|n| serde_json::json!({"key": format!("EX-{n}"), "fields": {}}))
            .collect();
        let full = parse_changed(Value::Array(full).to_string().as_bytes()).unwrap();
        assert_eq!(full.keys, None);
        assert_eq!(full.me, None);
        assert!(parse_changed(br#"[{"fields": {}}]"#).is_err());
    }

    /// Mutation: round the window down, or let a zero window through as `-0m`.
    #[test]
    fn a_window_is_whole_minutes_rounded_up() {
        let minutes = |secs| window_minutes(SignedDuration::from_secs(secs));
        assert_eq!(minutes(0), 1);
        assert_eq!(minutes(60), 1);
        assert_eq!(minutes(61), 2);
        assert_eq!(minutes(240), 4);
    }

    /// The first pass reads everything; an ask comes every two minutes and looks back two
    /// minutes further than the last; a clock gone back or a week asleep reads everything.
    /// Mutation: drop the overlap, compare with `<=`, or trust a negative age.
    #[test]
    fn what_a_pass_asks_follows_the_last_ask() {
        let dir = Scratch::new("comments-ask");
        let mut reads = Reads::load(&dir);
        let start = at("2026-11-06T12:00:00Z");
        assert_eq!(reads.ask(start), Ask::Everything);
        reads.asked(
            start,
            Changed {
                keys: Some(Vec::new()),
                me: None,
            },
            &[],
        );
        let after = |mins: i64| start + SignedDuration::from_mins(mins);
        assert_eq!(reads.ask(after(1)), Ask::Not);
        assert_eq!(
            reads.ask(after(2)),
            Ask::Within(SignedDuration::from_mins(4))
        );
        assert_eq!(reads.ask(after(-1)), Ask::Everything);
        assert_eq!(reads.ask(after(7 * 24 * 60 + 1)), Ask::Everything);
    }

    /// Changed items come first, then those never read, up to the limit; a read clears its
    /// item; a full answer makes every open item due; the save drops what is no longer open
    /// and a load reads it back.
    /// Mutation: read unread items before changed ones, keep a read item due, keep a closed
    /// item on save, or leave `me` out of the file.
    #[test]
    fn reads_track_what_is_due_and_survive_a_restart() {
        let dir = Scratch::new("comments-reads");
        let open: Vec<String> = ["EX-1", "EX-2", "EX-3"].map(str::to_owned).to_vec();
        let now = at("2026-11-06T12:00:00Z");
        let mut reads = Reads::load(&dir);
        reads.record("EX-1".to_owned(), viewed(Thread::Whole(Vec::new())), now);
        reads.record("EX-9".to_owned(), viewed(Thread::Whole(Vec::new())), now);
        reads.asked(
            now,
            Changed {
                keys: Some(vec!["EX-3".to_owned()]),
                me: Some(me()),
            },
            &open,
        );
        assert_eq!(reads.to_read(&open, 10), ["EX-3", "EX-2"]);
        assert_eq!(reads.to_read(&open, 1), ["EX-3"]);
        reads.record("EX-3".to_owned(), viewed(Thread::Whole(Vec::new())), now);
        assert_eq!(reads.to_read(&open, 10), ["EX-2"]);
        reads.keep_and_save(&open).unwrap();

        let loaded = Reads::load(&dir);
        assert_eq!(loaded.me(), Some(&me()));
        assert!(loaded.thread("EX-1").is_some());
        assert!(loaded.thread("EX-9").is_none(), "no longer open");
        assert_eq!(loaded.to_read(&open, 10), ["EX-2"]);
        assert_eq!(loaded.ask(now), Ask::Not);

        let mut reads = loaded;
        reads.asked(
            now,
            Changed {
                keys: None,
                me: None,
            },
            &open,
        );
        assert_eq!(reads.to_read(&open, 10), ["EX-1", "EX-2", "EX-3"]);
        assert_eq!(
            reads.me(),
            Some(&me()),
            "an ask with no rows keeps the known id"
        );
    }

    /// A read saved before the epic was asked for loads, keeps its thread, and is read again;
    /// once read with its epic it is not.
    /// Mutation: refuse the old file, or leave a read without its epic out of `to_read`.
    #[test]
    fn a_read_saved_without_its_epic_is_read_again() {
        let dir = Scratch::new("comments-old-file");
        let old = serde_json::json!({
            "asked": "2026-11-06T12:00:00Z",
            "due": [],
            "me": ONE,
            "threads": {"EX-1": {"at": "2026-11-06T12:00:00Z", "thread": {"Whole": []}}},
        });
        std::fs::write(dir.join(FILE_NAME), old.to_string()).unwrap();
        let mut reads = Reads::load(&dir);
        let open = ["EX-1".to_owned()];
        assert_eq!(
            reads.thread("EX-1").map(|read| &read.epic),
            Some(&EpicOf::NotAsked)
        );
        assert_eq!(reads.to_read(&open, 10), ["EX-1"]);
        reads.record(
            "EX-1".to_owned(),
            viewed(Thread::Whole(Vec::new())),
            at("2026-11-06T12:05:00Z"),
        );
        assert_eq!(reads.to_read(&open, 10), Vec::<String>::new());
    }

    /// Mutation: turn a corrupt file into an error that stops the reads.
    #[test]
    fn a_corrupt_file_starts_the_reads_over() {
        let dir = Scratch::new("comments-corrupt");
        std::fs::write(dir.join(FILE_NAME), "{").unwrap();
        let reads = Reads::load(&dir);
        assert_eq!(reads.ask(at("2026-11-06T12:00:00Z")), Ask::Everything);
        assert!(reads.thread("EX-1").is_none());
    }
}
