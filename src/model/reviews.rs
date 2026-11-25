//! Review requests as the inbox shows them: one card per pull request waiting on the user.
//!
//! "Not configured", "never read", "could not read" and "read and these wait" are four
//! values of [`ReviewsView`]; a failed read never replaces a good list, it is shown beside it.

use super::SourceState;
use crate::gh::PullRequestRef;
use crate::reviews::classify::{FlaggedBy, Verdict};
use crate::reviews::github::CommitSha;
use crate::reviews::journal::Entry as JournalEntry;
use crate::reviews::watch::{Reading, Watch};
use crate::reviews::{Request, posted_at};
use crate::types::{Instant, Link, LinkTargets, SafeUrl};
use crate::util::redact::Redacted;

/// What the model knows of the review requests.
#[derive(Debug, Clone)]
pub enum ReviewsView {
    /// The config listens to no channel.
    Off,
    NeverRead,
    /// No read has worked; this is why.
    Failed {
        at: Instant,
        reason: Redacted<String>,
    },
    /// The cards, newest request first. `problems` are channels and pull requests that one
    /// read could not see; `failure` is a later read that did not work at all.
    Listed {
        read_at: Instant,
        cards: Vec<ReviewCard>,
        problems: Vec<Redacted<String>>,
        failure: Option<(Instant, Redacted<String>)>,
    },
}

/// One pull request waiting on the user, with the message that asked.
#[derive(Debug, Clone)]
pub struct ReviewCard {
    pub pr: PullRequestRef,
    /// Absent when the pull request's parts do not form a link.
    pub pr_url: Option<SafeUrl>,
    pub title: Redacted<String>,
    pub author: Redacted<String>,
    pub head: CommitSha,
    pub additions: u64,
    pub deletions: u64,
    pub files: usize,
    pub requester: Redacted<String>,
    /// The request as written, links bare.
    pub request: Redacted<String>,
    pub requested_at: Option<Instant>,
    /// The message in Slack; absent when it does not form a link.
    pub message_url: Option<SafeUrl>,
    pub verdict: CardVerdict,
}

/// What the classifier said about the card's commit, as the card shows it.
#[derive(Debug, Clone)]
pub enum CardVerdict {
    /// Not classified yet: the classifier runs after each read.
    Pending,
    Routine,
    Look(Vec<CardFlag>),
    Unclassified(Redacted<String>),
}

/// One reason to look, with who raised it.
#[derive(Debug, Clone)]
pub struct CardFlag {
    pub by: FlaggedBy,
    pub file: Option<Redacted<String>>,
    pub why: Redacted<String>,
}

impl ReviewsView {
    /// `watch` is `None` where the config listens to no channel.
    pub fn of(watch: Option<&Watch>, targets: &LinkTargets) -> Self {
        let Some(watch) = watch else {
            return Self::Off;
        };
        match watch.reading() {
            Reading::Unread => Self::NeverRead,
            Reading::Failed { at, reason } => Self::Failed {
                at: *at,
                reason: Redacted::of(reason),
            },
            Reading::Read { queue, failure } => Self::Listed {
                read_at: queue.read_at,
                cards: queue
                    .requests
                    .iter()
                    .map(|request| {
                        let verdict = watch.verdict(&request.pr, &request.facts.head);
                        card(request, verdict, targets)
                    })
                    .collect(),
                problems: queue
                    .problems
                    .iter()
                    .map(|problem| Redacted::of(&problem.to_string()))
                    .collect(),
                failure: failure
                    .as_ref()
                    .map(|(at, reason)| (*at, Redacted::of(reason))),
            },
        }
    }

    /// The cards there are; none unless listed.
    pub fn cards(&self) -> &[ReviewCard] {
        match self {
            Self::Listed { cards, .. } => cards,
            Self::Off | Self::NeverRead | Self::Failed { .. } => &[],
        }
    }
}

fn card_verdict(verdict: Option<&Verdict>) -> CardVerdict {
    match verdict {
        None => CardVerdict::Pending,
        Some(Verdict::Routine) => CardVerdict::Routine,
        Some(Verdict::Look(flags)) => CardVerdict::Look(
            flags
                .iter()
                .map(|flag| CardFlag {
                    by: flag.by,
                    file: flag.file.as_deref().map(Redacted::of),
                    why: Redacted::of(&flag.why),
                })
                .collect(),
        ),
        Some(Verdict::Unclassified { reason }) => CardVerdict::Unclassified(Redacted::of(reason)),
    }
}

fn card(request: &Request, verdict: Option<&Verdict>, targets: &LinkTargets) -> ReviewCard {
    let pr_link = Link::PullRequest {
        owner: request.pr.owner().to_owned(),
        repo: request.pr.repo().to_owned(),
        number: request.pr.number().get(),
    };
    let message_link = Link::SlackThread {
        channel: request.message.channel.as_str().to_owned(),
        ts: request.message.ts.clone(),
    };
    let facts = &request.facts;
    ReviewCard {
        pr: request.pr.clone(),
        pr_url: pr_link.url(targets).ok(),
        title: Redacted::of(&facts.title),
        author: Redacted::of(&facts.author),
        head: facts.head.clone(),
        additions: facts.additions,
        deletions: facts.deletions,
        files: facts.files.len(),
        requester: Redacted::of(&request.message.author),
        request: Redacted::of(&request.message.text),
        requested_at: posted_at(&request.message.ts),
        message_url: message_link.url(targets).ok(),
        verdict: card_verdict(verdict),
    }
}

/// One approval made from a review card, as the approvals page shows it.
#[derive(Debug, Clone)]
pub struct ApprovalRow {
    pub at: Instant,
    /// `owner/repo#n`, as recorded; also what a retry posts back.
    pub pr: String,
    pub pr_url: Option<SafeUrl>,
    pub head: String,
    pub title: Redacted<String>,
    pub author: Redacted<String>,
    pub requester: Redacted<String>,
    pub message_url: Option<SafeUrl>,
    pub verdict: Redacted<String>,
    pub flags: Vec<Redacted<String>>,
    pub reaction: Reaction,
}

/// Where the 👍 of an approval stands.
#[derive(Debug, Clone)]
pub enum Reaction {
    /// Asked for; no outcome recorded yet.
    Pending,
    Added {
        at: Instant,
    },
    Failed {
        at: Instant,
        reason: Redacted<String>,
    },
}

/// The journal as rows, newest first: each approval with the latest outcome of its 👍.
pub fn approvals(entries: &[JournalEntry], targets: &LinkTargets) -> Vec<ApprovalRow> {
    let mut rows: Vec<ApprovalRow> = Vec::new();
    for entry in entries {
        match entry {
            JournalEntry::Approved {
                at,
                pr,
                head,
                title,
                author,
                requester,
                channel,
                ts,
                verdict,
                flags,
            } => rows.push(ApprovalRow {
                at: *at,
                pr_url: pr_url(pr, targets),
                pr: pr.clone(),
                head: head.clone(),
                title: Redacted::of(title),
                author: Redacted::of(author),
                requester: Redacted::of(requester),
                message_url: Link::SlackThread {
                    channel: channel.clone(),
                    ts: ts.clone(),
                }
                .url(targets)
                .ok(),
                verdict: Redacted::of(verdict),
                flags: flags.iter().map(|flag| Redacted::of(flag)).collect(),
                reaction: Reaction::Pending,
            }),
            JournalEntry::Reacted { at, pr, head } => {
                if let Some(row) = latest(&mut rows, pr, head) {
                    row.reaction = Reaction::Added { at: *at };
                }
            }
            JournalEntry::ReactionFailed {
                at,
                pr,
                head,
                reason,
            } => {
                if let Some(row) = latest(&mut rows, pr, head) {
                    row.reaction = Reaction::Failed {
                        at: *at,
                        reason: Redacted::of(reason),
                    };
                }
            }
        }
    }
    rows.reverse();
    rows
}

fn latest<'r>(rows: &'r mut [ApprovalRow], pr: &str, head: &str) -> Option<&'r mut ApprovalRow> {
    rows.iter_mut()
        .rev()
        .find(|row| row.pr == pr && row.head == head)
}

/// The link of an `owner/repo#n` the journal recorded.
fn pr_url(pr: &str, targets: &LinkTargets) -> Option<SafeUrl> {
    let (repository, number) = pr.split_once('#')?;
    let (owner, repo) = repository.split_once('/')?;
    Link::PullRequest {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: number.parse().ok()?,
    }
    .url(targets)
    .ok()
}

/// What reading the review queue last did, as the health page states it; `None` where no
/// channel is configured.
pub fn source_state(reading: Option<&Reading>) -> Option<SourceState> {
    reading.map(|reading| match reading {
        Reading::Unread => SourceState::NeverRead,
        Reading::Failed { at, reason }
        | Reading::Read {
            failure: Some((at, reason)),
            ..
        } => SourceState::Failed {
            at: *at,
            reason: reason.clone(),
        },
        Reading::Read {
            queue,
            failure: None,
        } if !queue.problems.is_empty() => SourceState::Failed {
            at: queue.read_at,
            reason: queue
                .problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        },
        Reading::Read {
            queue,
            failure: None,
        } => SourceState::Ok { at: queue.read_at },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{at, link_targets};

    fn approved(pr: &str, head: &str, at_text: &str) -> JournalEntry {
        JournalEntry::Approved {
            at: at(at_text),
            pr: pr.to_owned(),
            head: head.to_owned(),
            title: "t".to_owned(),
            author: "bo".to_owned(),
            requester: "Ada".to_owned(),
            channel: "C0123ABCD".to_owned(),
            ts: "1700000000.000100".to_owned(),
            verdict: "routine".to_owned(),
            flags: Vec::new(),
        }
    }

    /// Rows are newest first, each with the latest outcome of its own 👍: a retry that worked
    /// after a failure reads as added, and an outcome for another commit touches nothing.
    /// Mutation: attach outcomes to the first row instead of the matching one, or keep the
    /// first outcome instead of the latest.
    #[test]
    fn the_journal_folds_into_rows_with_the_latest_reaction() {
        let entries = vec![
            approved("o/a#1", "aaaaaaa", "2026-10-29T10:00:00Z"),
            approved("o/b#2", "bbbbbbb", "2026-10-29T10:05:00Z"),
            JournalEntry::ReactionFailed {
                at: at("2026-10-29T10:00:05Z"),
                pr: "o/a#1".to_owned(),
                head: "aaaaaaa".to_owned(),
                reason: "connector signed out".to_owned(),
            },
            JournalEntry::Reacted {
                at: at("2026-10-29T10:06:00Z"),
                pr: "o/a#1".to_owned(),
                head: "aaaaaaa".to_owned(),
            },
            JournalEntry::ReactionFailed {
                at: at("2026-10-29T10:07:00Z"),
                pr: "o/b#2".to_owned(),
                head: "ccccccc".to_owned(),
                reason: "for another commit".to_owned(),
            },
        ];
        let rows = approvals(&entries, &link_targets());
        let shown: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| {
                let reaction = match row.reaction {
                    Reaction::Pending => "pending",
                    Reaction::Added { .. } => "added",
                    Reaction::Failed { .. } => "failed",
                };
                (row.pr.as_str(), reaction)
            })
            .collect();
        assert_eq!(shown, [("o/b#2", "pending"), ("o/a#1", "added")]);
        assert_eq!(
            rows.first()
                .and_then(|row| row.pr_url.as_ref())
                .map(SafeUrl::as_str),
            Some("https://github.com/o/b/pull/2")
        );
    }
}
