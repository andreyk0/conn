//! The open dependencies between Jira tickets: one ticket blocking another, read from a
//! ticket's issue links.
//!
//! `acli jira workitem search` refuses the `issuelinks` field, so each ticket is read on its
//! own with `acli jira workitem view <key> --json --fields key,status,issuelinks`. Only the link
//! types that say one ticket waits for another count; a "Relates" link says as little as a
//! mention. A dependency whose either end is done is over and is left out.

use std::collections::HashSet;

use serde_json::Value;

use super::reads::PerTicket;
use super::{ACLI, ACLI_TIMEOUT, JiraError, classify_failure};
use crate::model::SourceState;
use crate::types::{Instant, JiraKey};
use crate::util::runner::{CommandRunner, run_tool};

/// The link types that make one ticket wait for another, by the name Jira gives them, and
/// which side waits. Jira's own "Blocks" reads outward "blocks", inward "is blocked by"; a
/// "Depends On" type reads outward "depends on", inward "is depended on by".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The outward issue is blocked by this one.
    Blocks,
    /// This one is blocked by the outward issue.
    DependsOn,
}

impl Kind {
    fn of(name: &str) -> Option<Self> {
        match name {
            "Blocks" => Some(Self::Blocks),
            "Depends On" => Some(Self::DependsOn),
            _ => None,
        }
    }
}

/// One ticket waiting for another that is not done.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub blocker: JiraKey,
    pub blocked: JiraKey,
}

/// The open dependencies `key` takes part in, either side.
pub async fn fetch(
    runner: &dyn CommandRunner,
    key: &JiraKey,
) -> Result<Vec<Dependency>, JiraError> {
    let key_text = key.to_string();
    let args = [
        "jira",
        "workitem",
        "view",
        key_text.as_str(),
        "--json",
        "--fields",
        "key,status,issuelinks",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse(key, &output.stdout)
}

/// The open dependencies in one ticket's `view --json` output.
pub fn parse(key: &JiraKey, stdout: &[u8]) -> Result<Vec<Dependency>, JiraError> {
    let unparseable = |detail: &str| JiraError::Unparseable {
        detail: detail.to_owned(),
    };
    let json: Value =
        serde_json::from_slice(stdout).map_err(|_| unparseable("the output is not JSON"))?;
    let fields = json.get("fields").ok_or_else(|| unparseable("no fields"))?;
    if is_done(fields) {
        return Ok(Vec::new());
    }
    let links = match fields.get("issuelinks") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(links) => links
            .as_array()
            .ok_or_else(|| unparseable("issuelinks is not a list"))?,
    };
    let mut dependencies = Vec::new();
    for link in links {
        let Some(kind) = link
            .get("type")
            .and_then(|kind| kind.get("name"))
            .and_then(Value::as_str)
            .and_then(Kind::of)
        else {
            continue;
        };
        // A link names the other issue on exactly one side; "outward" is what this issue does
        // to it ("blocks", "depends on"), "inward" what it does to this issue.
        let (other, outward) = match (link.get("outwardIssue"), link.get("inwardIssue")) {
            (Some(other), _) => (other, true),
            (None, Some(other)) => (other, false),
            (None, None) => continue,
        };
        if is_done(other.get("fields").unwrap_or(&Value::Null)) {
            continue;
        }
        let Some(other_key) = other
            .get("key")
            .and_then(Value::as_str)
            .and_then(JiraKey::parse)
        else {
            continue;
        };
        let this_blocks = match (kind, outward) {
            (Kind::Blocks, true) | (Kind::DependsOn, false) => true,
            (Kind::Blocks, false) | (Kind::DependsOn, true) => false,
        };
        dependencies.push(if this_blocks {
            Dependency {
                blocker: key.clone(),
                blocked: other_key,
            }
        } else {
            Dependency {
                blocker: other_key,
                blocked: key.clone(),
            }
        });
    }
    Ok(dependencies)
}

/// How old a ticket's read may grow before it is read again. Links change when someone edits
/// the ticket, rarely within the hour; they are read for the map's tickets and the work items
/// in [`Linked`], a handful each.
pub const READ_EVERY: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

/// The latest read of each ticket's links.
pub type Reads = PerTicket<Vec<Dependency>>;

impl Reads {
    /// Every open dependency the reads found, each once.
    pub fn dependencies(&self) -> Vec<Dependency> {
        let mut seen = HashSet::new();
        let mut all: Vec<Dependency> = self
            .values()
            .flatten()
            .filter(|dependency| seen.insert((*dependency).clone()))
            .cloned()
            .collect();
        all.sort_by_cached_key(|d| (d.blocked.to_string(), d.blocker.to_string()));
        all
    }
}

/// The JQL of the user's open work items that carry a link saying they wait for another
/// ticket: the side of each [`Kind`] that waits. Jira matches a link type by these names, and
/// one the site does not have matches nothing rather than failing the query.
const LINKED_JQL: &str = concat!(
    mine!(),
    r#" AND issueLinkType in ("is blocked by", "depends on")"#
);

/// The user's open work items that wait on another ticket by a link, open or done: the ones
/// whose links are worth reading. The last good list is kept through a failed read, so a
/// passing failure does not make every blocked item look ready.
#[derive(Debug, Clone, Default)]
pub struct Linked {
    keys: Vec<JiraKey>,
    read_at: Option<Instant>,
    /// The latest attempt, when it failed; cleared by the next success.
    failure: Option<(Instant, String)>,
}

impl Linked {
    /// Whether no attempt was made within [`READ_EVERY`].
    pub fn is_due(&self, now: Instant) -> bool {
        let attempted = self.read_at.max(self.failure.as_ref().map(|(at, _)| *at));
        attempted.is_none_or(|at| now.duration_since(at) >= READ_EVERY)
    }

    pub fn record(&mut self, outcome: Result<Vec<JiraKey>, JiraError>, at: Instant) {
        match outcome {
            Ok(keys) => {
                self.keys = keys;
                self.read_at = Some(at);
                self.failure = None;
            }
            Err(error) => self.failure = Some((at, error.to_string())),
        }
    }

    /// The items of the last good read.
    pub fn keys(&self) -> &[JiraKey] {
        &self.keys
    }

    pub fn state(&self) -> SourceState {
        match (&self.failure, self.read_at) {
            (Some((at, reason)), _) => SourceState::Failed {
                at: *at,
                reason: reason.clone(),
            },
            (None, Some(at)) => SourceState::Ok { at },
            (None, None) => SourceState::NeverRead,
        }
    }
}

/// The user's open work items that wait on another ticket by a link.
pub async fn fetch_linked(runner: &dyn CommandRunner) -> Result<Vec<JiraKey>, JiraError> {
    // `--fields key` alone comes back as nulls; `status` is the cheapest second field.
    let args = [
        "jira",
        "workitem",
        "search",
        "--jql",
        LINKED_JQL,
        "--json",
        "--limit",
        super::LIMIT,
        "--fields",
        "key,status",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse_linked(&output.stdout)
}

/// The keys in a `search --json` output. A key that is not one is logged and left out: it
/// cannot be read with `view` either.
fn parse_linked(stdout: &[u8]) -> Result<Vec<JiraKey>, JiraError> {
    #[derive(serde::Deserialize)]
    struct Row {
        key: String,
    }
    let rows: Vec<Row> = serde_json::from_slice(stdout).map_err(|e| JiraError::Unparseable {
        detail: e.to_string(),
    })?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let key = JiraKey::parse(&row.key);
            if key.is_none() {
                tracing::warn!(key = %row.key, "a linked work item's key is not a Jira key; its links are not read");
            }
            key
        })
        .collect())
}

/// Whether a ticket's status is in Jira's "done" category.
fn is_done(fields: &Value) -> bool {
    fields
        .get("status")
        .and_then(|status| status.get("statusCategory"))
        .and_then(|category| category.get("key"))
        .and_then(Value::as_str)
        == Some("done")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(text: &str) -> JiraKey {
        JiraKey::parse(text).unwrap()
    }

    fn other(text: &str, category: &str) -> Value {
        json!({"key": text, "fields": {"status": {"name": "x", "statusCategory": {"key": category}}}})
    }

    fn link(name: &str, side: &str, issue: Value) -> Value {
        let mut link = json!({"id": "1", "type": {"name": name, "inward": "i", "outward": "o"}});
        if let Value::Object(map) = &mut link {
            map.insert(side.to_owned(), issue);
        }
        link
    }

    /// The search asks for the user's open items with either waiting side of a blocking link.
    /// Mutation: ask for the blocking side ("blocks"), or drop "depends on".
    #[tokio::test]
    async fn the_linked_search_asks_for_the_waiting_side_of_each_link_type() {
        use crate::util::runner::{Call, Exit, FakeRunner, Output};
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Success,
            stdout: br#"[{"key":"ABC-2","fields":{}},{"key":"not a key","fields":{}}]"#.to_vec(),
            stderr: String::new(),
        })]);
        let keys = fetch_linked(&runner).await.unwrap();
        assert_eq!(keys, [key("ABC-2")]);
        assert_eq!(
            runner.calls(),
            [Call {
                program: "acli".to_owned(),
                args: [
                    "jira",
                    "workitem",
                    "search",
                    "--jql",
                    r#"assignee = currentUser() AND resolution = Unresolved AND issueLinkType in ("is blocked by", "depends on")"#,
                    "--json",
                    "--limit",
                    "100",
                    "--fields",
                    "key,status",
                ]
                .map(str::to_owned)
                .to_vec(),
                input: Vec::new(),
                timeout: std::time::Duration::from_secs(20),
            }]
        );
    }

    /// A failed read keeps the last good list and says it failed; the next success clears the
    /// failure; an attempt of either kind makes the next one wait ten minutes.
    /// Mutation: clear the keys on a failure, or time only successes.
    #[test]
    fn a_failed_linked_read_keeps_the_last_good_list() {
        let at = |minutes: i64| Instant::from_second(1_700_000_000 + minutes * 60).unwrap();
        let mut linked = Linked::default();
        assert!(linked.is_due(at(0)));
        assert_eq!(linked.state(), SourceState::NeverRead);
        linked.record(Ok(vec![key("ABC-1")]), at(0));
        linked.record(Err(JiraError::Timeout), at(10));
        assert_eq!(linked.keys(), [key("ABC-1")]);
        assert!(matches!(linked.state(), SourceState::Failed { .. }));
        assert!(!linked.is_due(at(19)));
        assert!(linked.is_due(at(20)));
        linked.record(Ok(vec![]), at(20));
        assert_eq!(linked.keys(), []);
        assert_eq!(linked.state(), SourceState::Ok { at: at(20) });
    }

    /// Each side of each blocking type reads the right way round; a "Relates" link, a link to a
    /// done ticket, and every link of a done ticket are no dependency.
    /// Mutation: read "Blocks" inward as this blocking, drop the done check on the other side,
    /// or count "Relates".
    #[test]
    fn blocking_links_read_the_right_way_round_and_done_ones_drop() {
        let output = json!({"key": "ABC-1", "fields": {
            "status": {"statusCategory": {"key": "indeterminate"}},
            "issuelinks": [
                link("Blocks", "outwardIssue", other("ABC-2", "new")),
                link("Blocks", "inwardIssue", other("ABC-3", "indeterminate")),
                link("Depends On", "outwardIssue", other("ABC-4", "new")),
                link("Depends On", "inwardIssue", other("ABC-5", "new")),
                link("Relates", "outwardIssue", other("ABC-6", "new")),
                link("Blocks", "outwardIssue", other("ABC-7", "done")),
            ],
        }});
        let found = parse(&key("ABC-1"), output.to_string().as_bytes()).unwrap();
        let pairs: Vec<(String, String)> = found
            .iter()
            .map(|d| (d.blocker.to_string(), d.blocked.to_string()))
            .collect();
        let expected = [
            ("ABC-1", "ABC-2"),
            ("ABC-3", "ABC-1"),
            ("ABC-4", "ABC-1"),
            ("ABC-1", "ABC-5"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()));
        assert_eq!(pairs, expected);

        let done = json!({"key": "ABC-1", "fields": {
            "status": {"statusCategory": {"key": "done"}},
            "issuelinks": [link("Blocks", "outwardIssue", other("ABC-2", "new"))],
        }});
        assert_eq!(
            parse(&key("ABC-1"), done.to_string().as_bytes()).unwrap(),
            []
        );
        assert!(parse(&key("ABC-1"), b"not json").is_err());
        let no_links = json!({"key": "ABC-1", "fields": {"status": {}}});
        assert_eq!(
            parse(&key("ABC-1"), no_links.to_string().as_bytes()).unwrap(),
            []
        );
    }
}
