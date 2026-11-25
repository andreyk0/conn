//! Review requests: pull requests linked in the Slack channels the config names, that still
//! wait on the signed-in GitHub user.
//!
//! A request is a pull request some message in the last [`LOOKBACK_DAYS`] links, open, written
//! by someone else, and not yet approved by the user at its current commit. Its message is
//! the newest one linking it. One read of the queue is a run per channel through the Slack
//! connector (see [`headless`]) and a `gh pr view` per pull request; a channel or a pull
//! request that cannot be read is a [`Problem`] beside what could, never a silent gap.

pub mod classify;
pub mod github;
pub mod headless;
pub mod journal;
pub mod slack;
pub mod watch;

use std::collections::HashMap;

use crate::gh::{GhError, PullRequestRef};
use crate::links::{Extractor, LinkContext};
use crate::types::{Instant, Link};
use crate::util::runner::CommandRunner;
use github::{CommitSha, PrFacts};
use headless::HeadlessError;
use slack::{ChannelId, Message};

/// How far back a message still asks. Review requests are immediate: one older than this has
/// been answered, given to someone else, or asked again, and the new ask is a new message.
pub const LOOKBACK_DAYS: i64 = 2;

/// The most pull requests one read asks GitHub about, newest request first: a few a day is
/// the norm, and each costs a `gh` call.
const REQUESTS_MAX: usize = 40;

/// The `[reviews]` section: where requests are listened for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewsConfig {
    pub channels: Vec<ChannelId>,
}

/// One pull request waiting on the user, and the message that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub pr: PullRequestRef,
    pub message: Message,
    pub facts: PrFacts,
}

/// Something one read could not see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Channel {
        channel: ChannelId,
        error: HeadlessError,
    },
    PullRequest {
        pr: PullRequestRef,
        error: GhError,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Channel { channel, error } => write!(f, "Slack channel {channel}: {error}"),
            Self::PullRequest { pr, error } => write!(f, "{pr}: {error}"),
        }
    }
}

/// One read of the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    pub read_at: Instant,
    /// The GitHub login the requests wait on.
    pub me: String,
    /// Newest request first.
    pub requests: Vec<Request>,
    pub problems: Vec<Problem>,
}

/// The read could not start: who the user is on GitHub is unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot tell who is signed in to gh: {0}")]
pub struct NoLogin(pub GhError);

/// Reads every channel, then asks GitHub about each pull request they link.
pub async fn read_queue(
    runner: &dyn CommandRunner,
    config: &ReviewsConfig,
    extractor: &Extractor,
    now: Instant,
) -> Result<Queue, NoLogin> {
    let me = github::login(runner).await.map_err(NoLogin)?;
    let mut problems = Vec::new();
    let mut messages = Vec::new();
    for channel in &config.channels {
        match slack::read_channel(runner, channel, lookback_start(now)).await {
            Ok(read) => messages.extend(read),
            Err(error) => problems.push(Problem::Channel {
                channel: channel.clone(),
                error,
            }),
        }
    }
    let mut requests = Vec::new();
    for (pr, message) in newest_per_pull_request(messages, extractor, now) {
        match github::facts(runner, &pr, &me).await {
            Ok(facts) if facts.awaits(&me) => requests.push(Request { pr, message, facts }),
            Ok(_) => {}
            Err(error) => problems.push(Problem::PullRequest { pr, error }),
        }
    }
    Ok(Queue {
        read_at: now,
        me,
        requests,
        problems,
    })
}

/// Why a click on Approve did not approve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApproveError {
    #[error("{0} is not in the review queue; reload the page")]
    NotInQueue(PullRequestRef),
    #[error("{pr} has new commits since its card was drawn (now at {}); reload the page", now.short())]
    Moved { pr: PullRequestRef, now: CommitSha },
    #[error("{0} no longer waits on you: merged, closed or already approved")]
    NoLongerWaits(PullRequestRef),
    #[error("could not approve {pr}: {error}")]
    GitHub { pr: PullRequestRef, error: GhError },
}

/// Approves `request` if it is still at `head` and still waits on `me`: a click approves
/// what the card showed, nothing newer.
pub async fn approve(
    runner: &dyn CommandRunner,
    request: &Request,
    head: &CommitSha,
    me: &str,
) -> Result<(), ApproveError> {
    let pr = &request.pr;
    let gh = |error| ApproveError::GitHub {
        pr: pr.clone(),
        error,
    };
    let now = github::facts(runner, pr, me).await.map_err(gh)?;
    if now.head != *head {
        return Err(ApproveError::Moved {
            pr: pr.clone(),
            now: now.head,
        });
    }
    if !now.awaits(me) {
        return Err(ApproveError::NoLongerWaits(pr.clone()));
    }
    github::approve(runner, pr).await.map_err(gh)
}

/// Each pull request linked in a message of the last [`LOOKBACK_DAYS`], with the newest such
/// message, newest first, at most [`REQUESTS_MAX`].
pub fn newest_per_pull_request(
    messages: Vec<Message>,
    extractor: &Extractor,
    now: Instant,
) -> Vec<(PullRequestRef, Message)> {
    let since = lookback_start(now).as_second();
    let mut newest: HashMap<PullRequestRef, Message> = HashMap::new();
    for message in messages {
        if seconds_of(&message.ts).is_none_or(|at| at < since) {
            continue;
        }
        let prs: Vec<PullRequestRef> = extractor
            .extract(&message.text, &LinkContext::default())
            .found
            .iter()
            .filter(|found| matches!(found.link, Link::PullRequest { .. }))
            .filter_map(|found| PullRequestRef::try_from(&found.link).ok())
            .collect();
        for pr in prs {
            let newer = newest
                .get(&pr)
                .is_none_or(|held| ts_key(&held.ts) < ts_key(&message.ts));
            if newer {
                newest.insert(pr, message.clone());
            }
        }
    }
    let mut ordered: Vec<(PullRequestRef, Message)> = newest.into_iter().collect();
    ordered.sort_by(|(a_pr, a), (b_pr, b)| {
        ts_key(&b.ts)
            .cmp(&ts_key(&a.ts))
            .then_with(|| a_pr.to_string().cmp(&b_pr.to_string()))
    });
    ordered.truncate(REQUESTS_MAX);
    ordered
}

/// The oldest instant a message may be from and still ask: [`LOOKBACK_DAYS`] before `now`.
fn lookback_start(now: Instant) -> Instant {
    now.saturating_sub(jiff::SignedDuration::from_hours(LOOKBACK_DAYS * 24))
        .unwrap_or(Instant::MIN)
}

/// When a message was posted, from its Slack timestamp.
pub fn posted_at(ts: &str) -> Option<Instant> {
    let (seconds, micros) = ts_key(ts)?;
    Instant::new(seconds, i32::try_from(micros.saturating_mul(1000)).ok()?).ok()
}

fn seconds_of(ts: &str) -> Option<i64> {
    ts_key(ts).map(|(seconds, _)| seconds)
}

/// A Slack timestamp as numbers, so `9.000001` sorts before `10.000000`.
fn ts_key(ts: &str) -> Option<(i64, i64)> {
    let (seconds, micros) = ts.split_once('.')?;
    Some((seconds.parse().ok()?, micros.parse().ok()?))
}

#[cfg(test)]
mod tests {
    //! The request list from hand-written messages, with placeholder repositories.

    use super::*;
    use crate::config::Config;

    fn extractor() -> Extractor {
        let config = Config::parse(
            "[slack]\nhost = \"example.slack.com\"\n",
            std::path::Path::new("/cfg.toml"),
            std::path::Path::new("/home/dev"),
        )
        .unwrap();
        Extractor::from_config(&config).unwrap()
    }

    fn message(ts: &str, text: &str) -> Message {
        Message {
            channel: ChannelId::parse("D0ABC123").unwrap(),
            ts: ts.to_owned(),
            author: "Ada".to_owned(),
            author_id: "U1".to_owned(),
            text: text.to_owned(),
        }
    }

    const NOW: i64 = 1_793_300_000;
    const PR_A: &str = "https://github.com/example-org/a/pull/1";
    const PR_B: &str = "https://github.com/example-org/b/pull/2";

    /// Each pull request keeps its newest message, the list is newest first, and a message
    /// older than the lookback asks nothing. Mutation: keep the first message instead of the
    /// newest, compare timestamps as text (`99…` after `100…`), or drop the lookback.
    #[test]
    fn each_pull_request_keeps_its_newest_message_within_the_lookback() {
        let day = 24 * 60 * 60;
        let messages = vec![
            message(
                &format!("{}.000001", NOW - day - day / 2),
                &format!("first ask {PR_A}"),
            ),
            message(
                &format!("{}.000001", NOW - day),
                &format!("again {PR_A} and {PR_B}"),
            ),
            message(&format!("{}.000002", NOW - day), &format!("bump {PR_B}")),
            message(
                &format!("{}.000001", NOW - 2 * day - 1),
                "old https://github.com/example-org/c/pull/3",
            ),
            message(
                "99999999.000001",
                "ancient https://github.com/example-org/d/pull/4",
            ),
        ];
        let now = Instant::from_second(NOW).unwrap();
        let got: Vec<(String, String)> = newest_per_pull_request(messages, &extractor(), now)
            .into_iter()
            .map(|(pr, m)| (pr.to_string(), m.text))
            .collect();
        assert_eq!(
            got,
            vec![
                ("example-org/b#2".to_owned(), format!("bump {PR_B}")),
                (
                    "example-org/a#1".to_owned(),
                    format!("again {PR_A} and {PR_B}")
                ),
            ]
        );
    }

    /// Reads the real queue from the configured channels: Slack through the connector, then
    /// `gh`. Run by hand with `cargo test -- --ignored the_real_queue`.
    #[tokio::test]
    #[ignore = "reads the real Slack channels and GitHub"]
    #[allow(
        clippy::print_stdout,
        reason = "an ignored smoke test reports what it read to its runner"
    )]
    async fn the_real_queue_reads() {
        let config = Config::load().unwrap();
        let reviews = config.reviews.clone().unwrap();
        let extractor = Extractor::from_config(&config).unwrap();
        let queue = read_queue(
            &crate::util::runner::TokioRunner,
            &reviews,
            &extractor,
            Instant::now(),
        )
        .await
        .unwrap();
        for request in &queue.requests {
            println!(
                "{} by {} (+{} -{}, {} files) asked by {} at {}",
                request.pr,
                request.facts.author,
                request.facts.additions,
                request.facts.deletions,
                request.facts.files.len(),
                request.message.author,
                request.message.ts
            );
        }
        for problem in &queue.problems {
            println!("problem: {problem}");
        }
    }

    fn request(head: &str) -> Request {
        Request {
            pr: PullRequestRef::new("example-org", "infra", 4242).unwrap(),
            message: message("1793300000.000001", "please"),
            facts: github::parse_facts(&view("OPEN", head), "ada").unwrap(),
        }
    }

    fn view(state: &str, head: &str) -> Vec<u8> {
        serde_json::json!({"state": state, "author": {"login": "bo"}, "title": "t",
            "headRefOid": head, "reviews": [], "additions": 1, "deletions": 0, "files": []})
        .to_string()
        .into_bytes()
    }

    fn printed(stdout: &[u8]) -> crate::util::runner::Output {
        crate::util::runner::Output {
            exit: crate::util::runner::Exit::Success,
            stdout: stdout.to_vec(),
            stderr: String::new(),
        }
    }

    const SHOWN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PUSHED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A click approves the commit the card showed and nothing newer: the head is read again
    /// first, and a moved head or a pull request that no longer waits is refused before `gh pr
    /// review` runs. Mutation: skip the head check (the pushed commit is approved), or the
    /// waits check (the merged one is).
    #[tokio::test]
    async fn approve_approves_only_the_commit_the_card_showed() {
        let head = CommitSha::parse(SHOWN).unwrap();
        let runner = crate::util::runner::FakeRunner::new([
            Ok(printed(&view("OPEN", SHOWN))),
            Ok(printed(b"")),
        ]);
        assert_eq!(
            approve(&runner, &request(SHOWN), &head, "ada").await,
            Ok(())
        );
        assert_eq!(
            runner.lines(),
            [
                "gh pr view 4242 --repo example-org/infra --json state,author,title,headRefOid,reviews,additions,deletions,files",
                "gh pr review 4242 --repo example-org/infra --approve",
            ]
        );

        let moved = crate::util::runner::FakeRunner::new([Ok(printed(&view("OPEN", PUSHED)))]);
        assert!(matches!(
            approve(&moved, &request(SHOWN), &head, "ada").await,
            Err(ApproveError::Moved { .. })
        ));
        assert_eq!(moved.lines().len(), 1, "no approval after a moved head");

        let merged = crate::util::runner::FakeRunner::new([Ok(printed(&view("MERGED", SHOWN)))]);
        assert!(matches!(
            approve(&merged, &request(SHOWN), &head, "ada").await,
            Err(ApproveError::NoLongerWaits(_))
        ));
        assert_eq!(
            merged.lines().len(),
            1,
            "no approval of a merged pull request"
        );
    }

    #[test]
    fn a_slack_timestamp_is_a_time() {
        assert_eq!(
            posted_at("1793290000.000100").map(|at| at.to_string()),
            Some("2026-10-29T16:06:40.0001Z".to_owned())
        );
        assert_eq!(posted_at("nope"), None);
    }
}
