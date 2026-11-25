//! The user's open Jira work items, from the Atlassian CLI.
//!
//! `acli` is driven as a subprocess whose JSON output is parsed into types at this boundary.
//! "Could not ask" and "asked and the answer was X" are separate: an empty list is a value
//! (nothing is open); every [`JiraError`] variant is a failure to find out, and each says which
//! way.

/// What is still open and mine. A macro so the queries built on it are `concat!`ed constants;
/// defined before the modules so theirs can use it too.
///
/// "Mine" is the assignee, since that is the list a person works from; `Unresolved` is the
/// one resolution test that holds across every workflow, where a status list would name each
/// project's own "done" states.
macro_rules! mine {
    () => {
        "assignee = currentUser() AND resolution = Unresolved"
    };
}

mod cache;
pub mod children;
pub mod comments;
mod item;
pub mod links;
pub mod reads;
pub mod resolution;

use std::fmt;
use std::time::Duration;

use crate::types::JiraPrefix;
use crate::util::runner::{CommandRunner, ToolError, run_tool};

pub use cache::{Cache, CacheFileError, FILE_NAME, Failed, Observed, Reading};
pub use item::{
    DESCRIPTION_CHARS_MAX, EpicOf, Level, Person, Status, StatusCategory, WorkItem, WorkItemKey,
    by_key, parse_work_items,
};

const ACLI: &str = "acli";

/// How long one `acli jira workitem search` may take. `acli` makes one authenticated REST call
/// that answers in a second or two; 20 s covers a slow link and a token refresh, and a refresh
/// task is never what a page waits on.
const ACLI_TIMEOUT: Duration = Duration::from_secs(20);

/// The format Jira writes its times in: `2026-11-06T10:00:00.100-0500`.
const JIRA_TIME: &str = "%Y-%m-%dT%H:%M:%S%.f%z";

/// A time as Jira writes it; `None` when it is not in [`JIRA_TIME`].
fn jira_time(text: &str) -> Option<crate::types::Instant> {
    jiff::fmt::strtime::parse(JIRA_TIME, text)
        .and_then(|parsed| parsed.to_timestamp())
        .ok()
}

/// The most items one search returns. A person's open assignments are a few dozen; a hundred is
/// well above that, and a result of exactly this size is logged as possibly cut off.
const LIMIT: &str = "100";
const LIMIT_ITEMS: usize = 100;

/// The fields asked for. `acli` whitelists `--fields` and refuses `updated`, `created`,
/// `duedate`, `statuscategorychangedate`, `parent`, `comment`, `issuelinks`, `*all` and
/// `*navigable` with exit 1, empty stdout and `field '<name>' is not allowed` on stderr, so this
/// is the whole usable set the parse in [`item`] relies on. `acli jira workitem view` takes
/// those fields; JQL can still filter and order on them.
const FIELDS: &str = "key,summary,status,issuetype,priority,creator,assignee,description";

/// What `acli` prints on stderr when it has no credentials is matched against these
/// (lower case): its observed text is `unauthorized: use 'acli [product] auth login' to
/// authenticate`, exit code 1.
const AUTH_MARKERS: [&str; 2] = ["unauthorized", "auth login"];

const AUTH_HINT: &str = "run `acli jira auth login`";

const MINE: &str = mine!();

/// What is read about tickets beyond the list of work items, one `acli` call per ticket: what
/// waits on what, and how far each epic has come.
#[derive(Debug, Clone)]
pub struct Relations {
    /// The links of the map's tickets and of the [`links::Linked`] work items.
    pub links: links::Reads,
    pub linked: links::Linked,
    /// The children of each open epic among the work items.
    pub children: reads::PerTicket<children::Progress>,
}

impl Default for Relations {
    fn default() -> Self {
        Self {
            links: reads::PerTicket::new(links::READ_EVERY),
            linked: links::Linked::default(),
            children: reads::PerTicket::new(children::READ_EVERY),
        }
    }
}

/// The default query: [`MINE`], the most recently touched first.
pub const DEFAULT_JQL: &str = concat!(mine!(), " order by updated DESC");

/// A JQL query, passed to `acli --jql` as one argument.
///
/// Witnesses that the text holds something besides whitespace: `acli` answers an empty query
/// with a usage error, which would otherwise surface as a failure of the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jql(String);

/// The query text was empty or only whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a JQL query cannot be empty")]
pub struct JqlError;

impl Jql {
    pub fn new(text: &str) -> Result<Self, JqlError> {
        let text = text.trim();
        if text.is_empty() {
            Err(JqlError)
        } else {
            Ok(Self(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Jql {
    fn default() -> Self {
        Self(DEFAULT_JQL.to_owned())
    }
}

impl fmt::Display for Jql {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the work items could not be had. None of these is "there are no work items": the empty
/// list is `Ok(vec![])`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JiraError {
    #[error("acli is not installed or not on PATH")]
    NotInstalled,
    /// `acli` has no usable credentials; `hint` is what to run.
    #[error("acli is not signed in to Jira: {hint}")]
    NotAuthenticated { hint: &'static str },
    #[error("acli did not answer within {} s", ACLI_TIMEOUT.as_secs())]
    Timeout,
    /// `acli` ran and failed in a way not classified above; `stderr_head` is the start of its
    /// stderr.
    #[error("acli exited with {code:?}: {stderr_head}")]
    NonZero {
        code: Option<i32>,
        stderr_head: String,
    },
    #[error("acli output is not the expected JSON: {detail}")]
    Unparseable { detail: String },
    /// `acli` could not be started, was killed for printing too much, or failed in a way not
    /// classified above.
    #[error(transparent)]
    Tool(ToolError),
    /// The saved copy of the last read could not be read, or is not what this tool wrote.
    #[error(transparent)]
    CacheFile(CacheFileError),
}

/// The open work items matching `query`, asked of Jira through `acli`.
///
/// Keys are classified against `prefixes`: a key of another project is kept as
/// [`WorkItemKey::Other`]. A result of exactly 100 items may be cut off by the limit; that is
/// logged, since the list cannot say so itself.
pub async fn fetch(
    runner: &dyn CommandRunner,
    query: &Jql,
    prefixes: &[JiraPrefix],
) -> Result<Vec<WorkItem>, JiraError> {
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
        FIELDS,
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    let items = parse_work_items(&output.stdout, prefixes)?;
    if items.len() >= LIMIT_ITEMS {
        tracing::warn!(
            limit = LIMIT_ITEMS,
            "acli returned a full page; later items may be cut off"
        );
    }
    Ok(items)
}

/// What `acli`'s failure says: no credentials, not installed, out of time; anything else is
/// the tool's own error.
fn classify_failure(error: ToolError) -> JiraError {
    match error {
        ToolError::Missing { .. } => JiraError::NotInstalled,
        ToolError::TimedOut { .. } => JiraError::Timeout,
        ToolError::NonZeroExit { code, stderr, .. } => {
            let lower = stderr.to_lowercase();
            if AUTH_MARKERS.iter().any(|marker| lower.contains(marker)) {
                JiraError::NotAuthenticated { hint: AUTH_HINT }
            } else {
                JiraError::NonZero {
                    code,
                    stderr_head: stderr,
                }
            }
        }
        ToolError::Spawn { .. }
        | ToolError::OutputTooLarge { .. }
        | ToolError::Unparseable { .. } => JiraError::Tool(error),
    }
}

#[cfg(test)]
mod tests {
    //! `fetch` driven through `FakeRunner`: what is asked, and how each way of not getting an
    //! answer is told apart. The argument vector is written out by hand from the task's
    //! command line; the unauthenticated case is the exit code and stderr observed from
    //! `acli` 1.3.13 and 1.3.40 with no login (`tests/fixtures/jira/unauthenticated.stderr`).

    use super::*;
    use crate::util::runner::{Call, Exit, FakeRunner, Output, RunError};

    const SEARCH: &[u8] = include_bytes!("../../tests/fixtures/jira/search_result.json");
    const UNAUTHENTICATED: &str = include_str!("../../tests/fixtures/jira/unauthenticated.stderr");

    fn prefixes() -> Vec<JiraPrefix> {
        vec![JiraPrefix::parse("EX").unwrap()]
    }

    fn finished(exit: Exit, stdout: &[u8], stderr: &str) -> Output {
        Output {
            exit,
            stdout: stdout.to_vec(),
            stderr: stderr.to_owned(),
        }
    }

    async fn ask(
        answer: Result<Output, RunError>,
    ) -> (Result<Vec<WorkItem>, JiraError>, FakeRunner) {
        let runner = FakeRunner::new([answer]);
        let result = fetch(&runner, &Jql::default(), &prefixes()).await;
        (result, runner)
    }

    /// Mutation: change the limit, drop `--json`, or pass a different timeout.
    #[tokio::test]
    async fn asks_acli_for_exactly_this_search_with_a_twenty_second_deadline() {
        let (result, runner) = ask(Ok(finished(Exit::Success, SEARCH, ""))).await;
        assert_eq!(result.unwrap().len(), 3);
        assert_eq!(
            runner.calls(),
            [Call {
                program: "acli".to_owned(),
                args: [
                    "jira",
                    "workitem",
                    "search",
                    "--jql",
                    "assignee = currentUser() AND resolution = Unresolved order by updated DESC",
                    "--json",
                    "--limit",
                    "100",
                    "--fields",
                    "key,summary,status,issuetype,priority,creator,assignee,description",
                ]
                .map(str::to_owned)
                .to_vec(),
                input: Vec::new(),
                timeout: Duration::from_secs(20),
            }]
        );
    }

    /// `acli` refuses a field outside its whitelist with exit 1, so the request asks for
    /// exactly the allowed set.
    /// Mutation: add `updated` (or `created`) back to `FIELDS`.
    #[tokio::test]
    async fn the_requested_fields_are_exactly_the_allowed_set() {
        let (_, runner) = ask(Ok(finished(Exit::Success, b"[]", ""))).await;
        let call = runner.calls().remove(0);
        let at = call.args.iter().position(|a| a == "--fields").unwrap();
        let fields: Vec<&str> = call.args.get(at + 1).unwrap().split(',').collect();
        assert_eq!(
            fields,
            [
                "key",
                "summary",
                "status",
                "issuetype",
                "priority",
                "creator",
                "assignee",
                "description"
            ]
        );
    }

    /// The observed refusal of a field is a failure of the tool, not an auth problem or an
    /// empty list.
    #[tokio::test]
    async fn a_refused_field_is_a_non_zero_exit_with_the_stderr_head() {
        let stderr = "✗ Error: fields 'updated, created' are not allowed";
        let (result, _) = ask(Ok(finished(Exit::Code(1), b"", stderr))).await;
        assert_eq!(
            result.unwrap_err(),
            JiraError::NonZero {
                code: Some(1),
                stderr_head: stderr.to_owned()
            }
        );
    }

    /// Mutation: map a missing `acli` to `Tool(..)`, or a timeout to `NonZero`.
    #[tokio::test]
    async fn not_installed_and_timeout_are_their_own_errors() {
        let (missing, _) = ask(Err(RunError::NotFound)).await;
        assert_eq!(missing.unwrap_err(), JiraError::NotInstalled);
        let (slow, _) = ask(Err(RunError::TimedOut(ACLI_TIMEOUT))).await;
        assert_eq!(slow.unwrap_err(), JiraError::Timeout);
    }

    /// Mutation: replace both markers (the observed text then falls to `NonZero`).
    #[tokio::test]
    async fn the_observed_unauthenticated_output_is_not_authenticated() {
        let (result, _) = ask(Ok(finished(Exit::Code(1), b"", UNAUTHENTICATED))).await;
        assert_eq!(
            result.unwrap_err(),
            JiraError::NotAuthenticated {
                hint: "run `acli jira auth login`"
            }
        );
    }

    /// Each marker alone is enough, in any case; an exit code alone is not.
    /// Mutation: drop either marker from `AUTH_MARKERS`, or compare without lower-casing.
    #[tokio::test]
    async fn each_auth_marker_alone_is_not_authenticated() {
        for stderr in [
            "Error: Unauthorized",
            "session expired; run acli jira auth login",
        ] {
            let (result, _) = ask(Ok(finished(Exit::Code(1), b"", stderr))).await;
            assert!(
                matches!(result, Err(JiraError::NotAuthenticated { .. })),
                "{stderr}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn other_failures_keep_their_exit_code_and_the_head_of_stderr() {
        let long = "x".repeat(500);
        let cases = [
            (
                Exit::Code(1),
                "Error: dial tcp: lookup example.atlassian.net: no such host",
                Some(1),
                "Error: dial tcp: lookup example.atlassian.net: no such host",
            ),
            (Exit::Code(2), "", Some(2), ""),
            (Exit::Signalled, "", None, ""),
        ];
        for (exit, stderr, code, head) in cases {
            let (result, _) = ask(Ok(finished(exit, b"", stderr))).await;
            assert_eq!(
                result.unwrap_err(),
                JiraError::NonZero {
                    code,
                    stderr_head: head.to_owned()
                },
                "{exit:?} {stderr:?}"
            );
        }
        let (result, _) = ask(Ok(finished(Exit::Code(1), b"", &long))).await;
        let Err(JiraError::NonZero { stderr_head, .. }) = result else {
            panic!("a failing exit is NonZero");
        };
        assert_eq!(stderr_head.chars().count(), 400);
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_tool_error() {
        let (result, _) = ask(Err(RunError::Spawn(std::io::Error::other("no acli")))).await;
        assert!(
            matches!(result, Err(JiraError::Tool(ToolError::Spawn { .. }))),
            "{result:?}"
        );
    }

    /// Mutation: parse failure mapped to `Ok(vec![])`.
    #[tokio::test]
    async fn success_with_unreadable_output_is_unparseable_not_empty() {
        for stdout in [&b""[..], b"null", b"{}", b"not json"] {
            let (result, _) = ask(Ok(finished(Exit::Success, stdout, ""))).await;
            assert!(
                matches!(result, Err(JiraError::Unparseable { .. })),
                "{stdout:?}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_open_items_is_an_empty_list() {
        let (result, _) = ask(Ok(finished(Exit::Success, b"[]", ""))).await;
        assert_eq!(result.unwrap(), []);
    }

    /// Mutation: `Jql::new` accepts the empty string, or stops trimming.
    #[test]
    fn a_query_is_not_empty() {
        assert_eq!(Jql::new(""), Err(JqlError));
        assert_eq!(Jql::new(" \t\n"), Err(JqlError));
        assert_eq!(Jql::new("  project = X ").unwrap().as_str(), "project = X");
        assert_eq!(Jql::default().as_str(), DEFAULT_JQL);
    }
}
