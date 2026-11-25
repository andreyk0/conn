//! When the review queue is read, and what the last reads gave.
//!
//! A read costs model runs, so it happens only while someone looks: at most every
//! [`READ_EVERY`], and only while a page was viewed in the last [`VIEWED_WITHIN`]. A failed
//! read never replaces a good queue; it is held beside it. A pull request is classified once
//! per commit: a verdict is kept while its pull request is in the queue at that commit.

use std::collections::{HashMap, HashSet};

use jiff::SignedDuration;

use super::classify::Verdict;
use super::github::CommitSha;
use super::{NoLogin, Queue, Request};
use crate::gh::PullRequestRef;
use crate::types::Instant;

/// How old the queue may get while someone is looking. A review request waits on a person
/// for minutes at least; five is well inside that.
pub const READ_EVERY: SignedDuration = SignedDuration::from_mins(5);

/// How long after the last page view reads go on.
pub const VIEWED_WITHIN: SignedDuration = SignedDuration::from_hours(1);

/// What the reads have given.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Reading {
    #[default]
    Unread,
    /// No read has worked; this is the latest failure.
    Failed { at: Instant, reason: String },
    /// The latest good queue, and a later failure if there was one.
    Read {
        queue: Queue,
        failure: Option<(Instant, String)>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Watch {
    reading: Reading,
    /// When the last read was started, whatever it gave.
    tried: Option<Instant>,
    viewed: Option<Instant>,
    /// The classifier's verdicts, by pull request and the commit they were reached on; only
    /// those still in the queue are kept.
    verdicts: HashMap<(PullRequestRef, CommitSha), Verdict>,
}

impl Watch {
    pub const fn reading(&self) -> &Reading {
        &self.reading
    }

    /// The verdict on `pr` at `head`, once the classifier has reached one.
    pub fn verdict(&self, pr: &PullRequestRef, head: &CommitSha) -> Option<&Verdict> {
        self.verdicts.get(&(pr.clone(), head.clone()))
    }

    /// The requests of the current queue the classifier has not seen at their head.
    pub fn unclassified(&self) -> Vec<Request> {
        let Reading::Read { queue, .. } = &self.reading else {
            return Vec::new();
        };
        queue
            .requests
            .iter()
            .filter(|request| self.verdict(&request.pr, &request.facts.head).is_none())
            .cloned()
            .collect()
    }

    /// The queued request for `pr`, and the login it waits on.
    pub fn request(&self, pr: &PullRequestRef) -> Option<(Request, String)> {
        let Reading::Read { queue, .. } = &self.reading else {
            return None;
        };
        queue
            .requests
            .iter()
            .find(|request| request.pr == *pr)
            .map(|request| (request.clone(), queue.me.clone()))
    }

    /// Takes `pr` out of the queue: it was approved, or no longer waits. The next read
    /// confirms it.
    pub fn resolved(&mut self, pr: &PullRequestRef) {
        if let Reading::Read { queue, .. } = &mut self.reading {
            queue.requests.retain(|request| request.pr != *pr);
        }
    }

    pub fn classified(&mut self, request: &Request, verdict: Verdict) {
        self.verdicts
            .insert((request.pr.clone(), request.facts.head.clone()), verdict);
    }

    /// Records a page view at `now`; true when that makes a read due, so the reader is woken.
    pub fn viewed(&mut self, now: Instant) -> bool {
        self.viewed = Some(now);
        self.is_due(now)
    }

    /// A page was viewed within [`VIEWED_WITHIN`] and no read started within [`READ_EVERY`].
    pub fn is_due(&self, now: Instant) -> bool {
        let watched = self
            .viewed
            .is_some_and(|at| now.duration_since(at) <= VIEWED_WITHIN);
        let stale = self
            .tried
            .is_none_or(|at| now.duration_since(at) >= READ_EVERY);
        watched && stale
    }

    pub const fn started(&mut self, now: Instant) {
        self.tried = Some(now);
    }

    pub fn record(&mut self, outcome: Result<Queue, NoLogin>, now: Instant) {
        if let Ok(queue) = &outcome {
            let current: HashSet<(PullRequestRef, CommitSha)> = queue
                .requests
                .iter()
                .map(|request| (request.pr.clone(), request.facts.head.clone()))
                .collect();
            self.verdicts.retain(|key, _| current.contains(key));
        }
        self.reading = match (outcome, std::mem::take(&mut self.reading)) {
            (Ok(queue), _) => Reading::Read {
                queue,
                failure: None,
            },
            (Err(error), Reading::Read { queue, .. }) => Reading::Read {
                queue,
                failure: Some((now, error.to_string())),
            },
            (Err(error), Reading::Unread | Reading::Failed { .. }) => Reading::Failed {
                at: now,
                reason: error.to_string(),
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gh::GhError;
    use crate::testutil::at;

    fn queue(at: Instant) -> Queue {
        Queue {
            read_at: at,
            me: "ada".to_owned(),
            requests: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Nothing is read until a page is viewed, then at most every five minutes, and not an
    /// hour after the last view. Mutation: drop the view check (due before any view), or the
    /// staleness check (due right after a read).
    #[test]
    fn a_read_is_due_only_while_someone_looks_and_the_queue_is_stale() {
        let start = at("2026-10-29T10:00:00Z");
        let mut watch = Watch::default();
        assert!(!watch.is_due(start), "nobody has looked");
        assert!(watch.viewed(start));
        watch.started(start);
        assert!(!watch.is_due(at("2026-10-29T10:04:59Z")));
        assert!(watch.is_due(at("2026-10-29T10:05:00Z")));
        assert!(
            !watch.is_due(at("2026-10-29T11:00:01Z")),
            "nobody looked for an hour"
        );
    }

    fn request(head: &str) -> Request {
        Request {
            pr: PullRequestRef::new("example-org", "a", 1).unwrap(),
            message: super::super::slack::Message {
                channel: super::super::slack::ChannelId::parse("D0ABC123").unwrap(),
                ts: "1.000001".to_owned(),
                author: "Ada".to_owned(),
                author_id: "U1".to_owned(),
                text: String::new(),
            },
            facts: super::super::github::PrFacts {
                life: super::super::github::PrLife::Open,
                author: "bo".to_owned(),
                title: "t".to_owned(),
                head: CommitSha::parse(head).unwrap(),
                approved: Vec::new(),
                additions: 1,
                deletions: 0,
                files: Vec::new(),
            },
        }
    }

    /// A verdict holds for its commit: a new commit is unclassified again, and the old verdict
    /// is dropped. Mutation: keep verdicts across reads (the old one is still there), or key
    /// them by pull request alone (the new commit reads as classified).
    #[test]
    fn a_new_commit_is_classified_again() {
        let mut watch = Watch::default();
        let now = at("2026-10-29T10:00:00Z");
        let first = request("aaaaaaa");
        watch.record(
            Ok(Queue {
                read_at: now,
                me: "ada".to_owned(),
                requests: vec![first.clone()],
                problems: Vec::new(),
            }),
            now,
        );
        assert_eq!(watch.unclassified(), vec![first.clone()]);
        watch.classified(&first, Verdict::Routine);
        assert_eq!(watch.unclassified(), vec![]);

        let pushed = request("bbbbbbb");
        watch.record(
            Ok(Queue {
                read_at: now,
                me: "ada".to_owned(),
                requests: vec![pushed.clone()],
                problems: Vec::new(),
            }),
            now,
        );
        assert_eq!(watch.unclassified(), vec![pushed]);
        assert_eq!(watch.verdict(&first.pr, &first.facts.head), None);
    }

    /// Mutation: let a failure replace the good queue.
    #[test]
    fn a_failure_is_held_beside_the_last_good_queue() {
        let mut watch = Watch::default();
        let no_login = || Err(NoLogin(GhError::AuthRequired));
        watch.record(no_login(), at("2026-10-29T10:00:00Z"));
        assert!(matches!(watch.reading(), Reading::Failed { .. }));
        watch.record(
            Ok(queue(at("2026-10-29T10:05:00Z"))),
            at("2026-10-29T10:05:00Z"),
        );
        watch.record(no_login(), at("2026-10-29T10:10:00Z"));
        assert!(matches!(
            watch.reading(),
            Reading::Read { queue, failure: Some(_) } if queue.read_at == at("2026-10-29T10:05:00Z")
        ));
    }
}
