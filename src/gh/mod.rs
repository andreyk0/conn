//! Pull-request state from the `gh` CLI.
//!
//! `gh` is driven as a subprocess whose JSON output is parsed into types at this boundary.
//! "Could not ask" and "asked and the answer was X" are separate: only the latter is a
//! value; every [`GhError`] variant is the former, and each says which way it could not.

mod cache;
mod state;

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use crate::types::Link;
use crate::util::runner::{CommandRunner, ToolError, run_tool};

pub use cache::{Cache, Failed, Observed, Reading};
pub use state::{Checks, PrLifecycle, PrState, Readiness, ReviewDecision, parse_pr_state};

const GH: &str = "gh";

/// How long one `gh pr view` may take. A card is rendered from the cache, so this bounds
/// how long a refresh task lives, not how long a page takes; `gh` makes one GraphQL call,
/// which answers in well under a second when GitHub is healthy.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// `gh` exits with this code when it has no usable credentials.
const GH_EXIT_AUTH_REQUIRED: i32 = 4;

/// A pull request, named so that it can be passed to `gh --repo` and used as a cache key.
///
/// Witnesses that the owner and repository are GitHub names (so the `--repo` argument
/// is well formed) and the number is not zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PullRequestRef {
    owner: String,
    repo: String,
    number: NonZeroU32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PullRequestRefError {
    #[error("not a GitHub owner name: {0:?}")]
    Owner(String),
    #[error("not a GitHub repository name: {0:?}")]
    Repo(String),
    #[error("pull request numbers start at 1")]
    Zero,
    #[error("not a pull request link")]
    NotAPullRequest,
}

impl PullRequestRef {
    pub fn new(owner: &str, repo: &str, number: u32) -> Result<Self, PullRequestRefError> {
        let name_char = |c: char| c.is_ascii_alphanumeric() || c == '-';
        let owner_ok = !owner.is_empty() && !owner.starts_with('-') && owner.chars().all(name_char);
        let repo_ok = !repo.is_empty()
            && !repo.starts_with(['-', '.'])
            && repo.chars().all(|c| name_char(c) || c == '.' || c == '_');
        if !owner_ok {
            return Err(PullRequestRefError::Owner(owner.to_owned()));
        }
        if !repo_ok {
            return Err(PullRequestRefError::Repo(repo.to_owned()));
        }
        let number = NonZeroU32::new(number).ok_or(PullRequestRefError::Zero)?;
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub const fn number(&self) -> NonZeroU32 {
        self.number
    }

    /// The arguments after `gh`.
    fn view_args(&self) -> Vec<String> {
        [
            "pr",
            "view",
            &self.number.to_string(),
            "--repo",
            &format!("{}/{}", self.owner, self.repo),
            "--json",
            state::JSON_FIELDS,
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

impl TryFrom<&Link> for PullRequestRef {
    type Error = PullRequestRefError;

    fn try_from(link: &Link) -> Result<Self, Self::Error> {
        match link {
            Link::PullRequest {
                owner,
                repo,
                number,
            } => Self::new(owner, repo, *number),
            Link::Jira { .. }
            | Link::SlackThread { .. }
            | Link::Doc { .. }
            | Link::Other { .. } => Err(PullRequestRefError::NotAPullRequest),
        }
    }
}

impl fmt::Display for PullRequestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// What GitHub answered about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Found(PrState),
    /// The pull request (or its repository) does not exist, or is not visible to the
    /// signed-in account; a placeholder link in session text is the common case.
    NotFound,
}

/// Why `gh` could not do what it was asked. [`GhError::NotFound`] is GitHub's answer, and only
/// an action ([`crate::reviews`]) fails with it: [`pr_state`] returns the same answer as
/// [`Answer::NotFound`]. The rest are failures to find out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GhError {
    /// GitHub answered that the pull request (or its repository) does not exist, or is not
    /// visible to the signed-in account.
    #[error("pull request not found")]
    NotFound,
    #[error("gh has no usable credentials; run `gh auth login`")]
    AuthRequired,
    #[error("gh output is not the expected JSON: {detail}")]
    Unparseable { detail: String },
    /// `gh` could not be run, did not answer in time, or failed in a way not classified above.
    #[error(transparent)]
    Tool(ToolError),
}

/// The state of one pull request, asked of GitHub through `gh`.
pub async fn pr_state(runner: &dyn CommandRunner, pr: &PullRequestRef) -> Result<Answer, GhError> {
    let args = pr.view_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run_tool(runner, GH, &args, GH_TIMEOUT).await {
        Ok(output) => parse_pr_state(&output.stdout, pr).map(Answer::Found),
        Err(error) => classify_failure(error),
    }
}

/// What `gh`'s failure says: no such pull request, which is an answer; no credentials; and
/// anything else is the tool's own error.
fn classify_failure(error: ToolError) -> Result<Answer, GhError> {
    let ToolError::NonZeroExit { code, stderr, .. } = &error else {
        return Err(GhError::Tool(error));
    };
    let says = |needles: &[&str]| needles.iter().any(|n| stderr.contains(n));
    if *code == Some(GH_EXIT_AUTH_REQUIRED) || says(&["gh auth login", "HTTP 401"]) {
        Err(GhError::AuthRequired)
    } else if says(&[
        "Could not resolve to a PullRequest",
        "Could not resolve to a Repository",
        "HTTP 404",
    ]) {
        Ok(Answer::NotFound)
    } else {
        Err(GhError::Tool(error))
    }
}

#[cfg(test)]
mod tests {
    //! `pr_state` driven through `FakeRunner`: what is asked, and how each way of not
    //! getting an answer is told apart. The argument vector and the error mapping are
    //! written out by hand from `gh`'s documented behaviour and its observed messages.

    use super::*;
    use crate::util::runner::{Call, Exit, FakeRunner, Output, RunError};

    const OPEN_PASSING: &[u8] = include_bytes!("../../tests/fixtures/gh/open_passing.json");

    fn pr() -> PullRequestRef {
        PullRequestRef::new("example-org", "example-repo", 42).unwrap()
    }

    fn finished(exit: Exit, stdout: &[u8], stderr: &str) -> Output {
        Output {
            exit,
            stdout: stdout.to_vec(),
            stderr: stderr.to_owned(),
        }
    }

    async fn ask(answer: Result<Output, RunError>) -> (Result<Answer, GhError>, FakeRunner) {
        let runner = FakeRunner::new([answer]);
        let result = pr_state(&runner, &pr()).await;
        (result, runner)
    }

    #[tokio::test]
    async fn asks_gh_for_exactly_these_fields_with_a_ten_second_deadline() {
        let (result, runner) = ask(Ok(finished(Exit::Success, OPEN_PASSING, ""))).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            runner.calls(),
            [Call {
                program: "gh".to_owned(),
                args: [
                    "pr",
                    "view",
                    "42",
                    "--repo",
                    "example-org/example-repo",
                    "--json",
                    "number,title,state,isDraft,mergedAt,reviewDecision,statusCheckRollup,updatedAt,url",
                ]
                .map(str::to_owned)
                .to_vec(),
                input: Vec::new(),
                timeout: Duration::from_secs(10),
            }]
        );
    }

    #[tokio::test]
    async fn a_timeout_is_a_timeout() {
        let (result, _) = ask(Err(RunError::TimedOut(GH_TIMEOUT))).await;
        assert!(
            matches!(result, Err(GhError::Tool(ToolError::TimedOut { .. }))),
            "{result:?}"
        );
    }

    /// Mutation: map `NotFound` to `Answer::NotFound`, which would say the pull request is gone.
    #[tokio::test]
    async fn a_missing_gh_is_not_a_missing_pull_request() {
        let (result, _) = ask(Err(RunError::NotFound)).await;
        assert!(
            matches!(result, Err(GhError::Tool(ToolError::Missing { .. }))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_spawn_error() {
        let (result, _) = ask(Err(RunError::Spawn(std::io::Error::other("no gh")))).await;
        assert!(
            matches!(result, Err(GhError::Tool(ToolError::Spawn { .. }))),
            "{result:?}"
        );
    }

    /// Mutation: return `Err(GhError::NotFound)` from `classify_failure`, which counts a
    /// placeholder link as a failed read for as long as a session mentions it.
    #[tokio::test]
    async fn failures_are_told_apart() {
        let long = "x".repeat(500);
        let cases = [
            (
                Exit::Code(1),
                "GraphQL: Could not resolve to a PullRequest with the number of 42. (repository.pullRequest)",
                "NotFound",
            ),
            (
                Exit::Code(1),
                "GraphQL: Could not resolve to a Repository with the name 'example-org/example-repo'.",
                "NotFound",
            ),
            (Exit::Code(1), "HTTP 404: Not Found", "NotFound"),
            (
                Exit::Code(4),
                "To get started with GitHub CLI, please run:  gh auth login",
                "AuthRequired",
            ),
            (Exit::Code(1), "HTTP 401: Bad credentials", "AuthRequired"),
            (Exit::Code(4), "", "AuthRequired"),
            (
                Exit::Code(1),
                "dial tcp: lookup api.github.com: no such host",
                "NonZero(Some(1))",
            ),
            (Exit::Signalled, "", "NonZero(None)"),
            (Exit::Code(2), &long, "NonZero(Some(2))"),
        ];
        for (exit, stderr, expected) in cases {
            let (result, _) = ask(Ok(finished(exit, b"", stderr))).await;
            let shown = match &result {
                Ok(Answer::NotFound) => "NotFound".to_owned(),
                Err(GhError::AuthRequired) => "AuthRequired".to_owned(),
                Err(GhError::Tool(ToolError::NonZeroExit {
                    code, stderr: kept, ..
                })) => {
                    assert!(kept.chars().count() <= 400, "{kept}");
                    if stderr.len() > 400 {
                        assert_eq!(kept.chars().count(), 400);
                    } else {
                        assert_eq!(kept, stderr);
                    }
                    format!("NonZero({code:?})")
                }
                other => format!("{other:?}"),
            };
            assert_eq!(shown, expected, "{exit:?} {stderr:?}");
        }
    }

    #[tokio::test]
    async fn success_with_unreadable_output_is_unparseable_not_not_found() {
        for stdout in [&b""[..], b"null", b"{}"] {
            let (result, _) = ask(Ok(finished(Exit::Success, stdout, ""))).await;
            assert!(
                matches!(result, Err(GhError::Unparseable { .. })),
                "{stdout:?}: {result:?}"
            );
        }
    }

    #[test]
    fn a_pull_request_ref_is_built_only_from_well_formed_names() {
        assert!(PullRequestRef::new("example-org", "example-repo.rs_2", 1).is_ok());
        for (owner, repo, number) in [
            ("", "r", 1),
            ("a b", "r", 1),
            ("-x", "r", 1),
            ("o/x", "r", 1),
            ("o", "", 1),
            ("o", "..", 1),
            ("o", "-r", 1),
            ("o", "r/x", 1),
            ("o", "r x", 1),
            ("o", "r", 0),
        ] {
            assert!(
                PullRequestRef::new(owner, repo, number).is_err(),
                "accepted {owner:?} {repo:?} {number}"
            );
        }
    }

    #[test]
    fn only_a_pull_request_link_converts() {
        let link = Link::PullRequest {
            owner: "example-org".to_owned(),
            repo: "example-repo".to_owned(),
            number: 42,
        };
        assert_eq!(PullRequestRef::try_from(&link).unwrap(), pr());
        let other = Link::Other {
            url: crate::types::SafeUrl::parse("https://example.com/").unwrap(),
        };
        assert_eq!(
            PullRequestRef::try_from(&other),
            Err(PullRequestRefError::NotAPullRequest)
        );
        let zero = Link::PullRequest {
            owner: "o".to_owned(),
            repo: "r".to_owned(),
            number: 0,
        };
        assert_eq!(
            PullRequestRef::try_from(&zero),
            Err(PullRequestRefError::Zero)
        );
    }
}
