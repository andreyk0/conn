//! What GitHub says about a requested pull request, through `gh`, and whether it still
//! waits on the signed-in user.

use std::time::Duration;

use serde::Deserialize;

use crate::gh::{GhError, PullRequestRef};
use crate::util::runner::{CommandRunner, ToolError, run_tool};

const GH: &str = "gh";

/// One `gh` call is one GraphQL request, well under a second when GitHub is healthy.
const GH_TIMEOUT: Duration = Duration::from_secs(15);

const VIEW_FIELDS: &str = "state,author,title,headRefOid,reviews,additions,deletions,files";

/// A commit, as GitHub names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn parse(text: &str) -> Option<Self> {
        let ok = (7..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit());
        ok.then(|| Self(text.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first seven characters, as GitHub shows a commit.
    pub fn short(&self) -> &str {
        self.0.get(..7).unwrap_or(&self.0)
    }
}

/// Where a pull request is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLife {
    Open,
    Closed,
    Merged,
}

/// One pull request as the review queue needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFacts {
    pub life: PrLife,
    pub author: String,
    pub title: String,
    pub head: CommitSha,
    /// The commits the signed-in user's approving reviews were left on.
    pub approved: Vec<CommitSha>,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<String>,
}

impl PrFacts {
    /// Open, written by someone else, and not approved by `me` at its current commit.
    pub fn awaits(&self, me: &str) -> bool {
        self.life == PrLife::Open
            && !self.author.eq_ignore_ascii_case(me)
            && !self.approved.contains(&self.head)
    }
}

/// The signed-in account's login.
pub async fn login(runner: &dyn CommandRunner) -> Result<String, GhError> {
    let output = run_tool(runner, GH, &["api", "user", "--jq", ".login"], GH_TIMEOUT)
        .await
        .map_err(GhError::Tool)?;
    let login = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if login.is_empty() {
        Err(GhError::Unparseable {
            detail: "gh api user printed no login".to_owned(),
        })
    } else {
        Ok(login)
    }
}

/// The facts of `pr`, with the approvals of `me` picked out of its reviews.
pub async fn facts(
    runner: &dyn CommandRunner,
    pr: &PullRequestRef,
    me: &str,
) -> Result<PrFacts, GhError> {
    let number = pr.number().to_string();
    let repo = format!("{}/{}", pr.owner(), pr.repo());
    let args = [
        "pr",
        "view",
        &number,
        "--repo",
        &repo,
        "--json",
        VIEW_FIELDS,
    ];
    let output = run_tool(runner, GH, &args, GH_TIMEOUT)
        .await
        .map_err(not_found_or_tool)?;
    parse_facts(&output.stdout, me)
}

/// The unified diff of `pr` at its head.
pub async fn diff(runner: &dyn CommandRunner, pr: &PullRequestRef) -> Result<String, GhError> {
    let number = pr.number().to_string();
    let repo = format!("{}/{}", pr.owner(), pr.repo());
    let args = ["pr", "diff", &number, "--repo", &repo];
    let output = run_tool(runner, GH, &args, GH_TIMEOUT)
        .await
        .map_err(not_found_or_tool)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Approves `pr` as the signed-in user, with no comment. `gh` approves whatever the head is
/// when it runs, so a caller that means one commit checks the head just before.
pub async fn approve(runner: &dyn CommandRunner, pr: &PullRequestRef) -> Result<(), GhError> {
    let number = pr.number().to_string();
    let repo = format!("{}/{}", pr.owner(), pr.repo());
    let args = ["pr", "review", &number, "--repo", &repo, "--approve"];
    run_tool(runner, GH, &args, GH_TIMEOUT)
        .await
        .map(drop)
        .map_err(not_found_or_tool)
}

fn not_found_or_tool(error: ToolError) -> GhError {
    match &error {
        ToolError::NonZeroExit { stderr, .. }
            if stderr.contains("Could not resolve to a PullRequest")
                || stderr.contains("Could not resolve to a Repository") =>
        {
            GhError::NotFound
        }
        ToolError::Missing { .. }
        | ToolError::Spawn { .. }
        | ToolError::TimedOut { .. }
        | ToolError::OutputTooLarge { .. }
        | ToolError::NonZeroExit { .. }
        | ToolError::Unparseable { .. } => GhError::Tool(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    state: String,
    author: Login,
    title: String,
    head_ref_oid: String,
    reviews: Vec<WireReview>,
    additions: u64,
    deletions: u64,
    files: Vec<WireFile>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct WireReview {
    author: Option<Login>,
    state: String,
    commit: Option<WireCommit>,
}

#[derive(Deserialize)]
struct WireCommit {
    oid: String,
}

#[derive(Deserialize)]
struct WireFile {
    path: String,
}

pub fn parse_facts(stdout: &[u8], me: &str) -> Result<PrFacts, GhError> {
    let bad = |detail: String| GhError::Unparseable { detail };
    let wire: Wire = serde_json::from_slice(stdout).map_err(|e| bad(e.to_string()))?;
    let life = match wire.state.as_str() {
        "OPEN" => PrLife::Open,
        "CLOSED" => PrLife::Closed,
        "MERGED" => PrLife::Merged,
        other => return Err(bad(format!("unknown pull request state {other:?}"))),
    };
    let head = CommitSha::parse(&wire.head_ref_oid)
        .ok_or_else(|| bad(format!("not a commit: {:?}", wire.head_ref_oid)))?;
    let approved = wire
        .reviews
        .iter()
        .filter(|r| r.state == "APPROVED")
        .filter(|r| {
            r.author
                .as_ref()
                .is_some_and(|a| a.login.eq_ignore_ascii_case(me))
        })
        .filter_map(|r| r.commit.as_ref().and_then(|c| CommitSha::parse(&c.oid)))
        .collect();
    Ok(PrFacts {
        life,
        author: wire.author.login,
        title: wire.title,
        head,
        approved,
        additions: wire.additions,
        deletions: wire.deletions,
        files: wire.files.into_iter().map(|f| f.path).collect(),
    })
}

#[cfg(test)]
mod tests {
    //! The parse against `gh pr view --json` output in the shape `gh` 2.x printed for a real
    //! pull request (placeholder names and commits).

    use serde_json::json;

    use super::*;

    const HEAD: &str = "0123456789aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD: &str = "1111111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn view(state: &str, author: &str, reviews: &serde_json::Value) -> Vec<u8> {
        json!({"state": state, "author": {"login": author}, "title": "Bump x",
            "headRefOid": HEAD, "reviews": reviews, "additions": 2, "deletions": 1,
            "files": [{"path": "a.tf"}, {"path": "b.rs"}]})
        .to_string()
        .into_bytes()
    }

    fn review(author: &str, state: &str, commit: &str) -> serde_json::Value {
        json!({"author": {"login": author}, "state": state, "commit": {"oid": commit}})
    }

    /// An approval by me at the head clears the request; at an older commit, by someone else,
    /// or a comment does not.
    /// Mutation: ignore the commit (the old approval clears it), or the author (Bo's does).
    #[test]
    fn only_my_approval_of_the_head_clears_a_request() {
        let me = "ada";
        let awaits = |reviews: serde_json::Value| {
            parse_facts(&view("OPEN", "bo", &reviews), me)
                .unwrap()
                .awaits(me)
        };
        assert!(awaits(json!([])));
        assert!(awaits(json!([review("ada", "APPROVED", OLD)])));
        assert!(awaits(json!([review("bo", "APPROVED", HEAD)])));
        assert!(awaits(json!([review("ada", "COMMENTED", HEAD)])));
        assert!(!awaits(json!([review("Ada", "APPROVED", HEAD)])));
    }

    /// Mutation: drop the life or the author check.
    #[test]
    fn a_closed_merged_or_own_pull_request_does_not_wait() {
        let me = "ada";
        for (state, author) in [("MERGED", "bo"), ("CLOSED", "bo"), ("OPEN", "ada")] {
            let facts = parse_facts(&view(state, author, &json!([])), me).unwrap();
            assert!(!facts.awaits(me), "{state} {author}");
        }
    }

    #[test]
    fn the_facts_are_read_and_bad_output_is_unparseable() {
        let facts = parse_facts(&view("OPEN", "bo", &json!([])), "ada").unwrap();
        assert_eq!(
            (
                facts.additions,
                facts.deletions,
                facts.files.len(),
                facts.head.short()
            ),
            (2, 1, 2, "0123456")
        );
        assert!(matches!(
            parse_facts(b"{}", "ada"),
            Err(GhError::Unparseable { .. })
        ));
    }
}
