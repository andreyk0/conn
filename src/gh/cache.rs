//! The last answers `gh` gave, with the age that lets a card show a stale value as stale.
//!
//! A failed read never erases the last good value and never passes for one: the entry
//! keeps both, and [`Reading`] hands both to the renderer. Time is passed in, so the
//! cache makes no clock reads of its own.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use jiff::SignedDuration;

use super::{Answer, GhError, PullRequestRef};
use crate::types::{Age, Instant};

/// How long an answer, good or not, is served before it is asked for again.
const DEFAULT_TTL: SignedDuration = SignedDuration::from_secs(120);

/// How many pull requests the cache remembers.
///
/// A day's distinct pull requests: a few dozen sessions a day each mention a handful, about a
/// hundred in all. Twice that leaves room for a heavy day, and 256 entries are some hundred
/// kilobytes, so the bound is never what limits a card.
const DEFAULT_ENTRIES_MAX: NonZeroUsize = NonZeroUsize::new(256).unwrap();

/// An answer and when it was read. "No such pull request" is an answer like a state is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub answer: Answer,
    pub read_at: Instant,
}

/// A failed read and when it happened.
#[derive(Debug, Clone)]
pub struct Failed {
    pub error: GhError,
    pub at: Instant,
}

/// What the cache knows about one pull request.
#[derive(Debug, Clone)]
pub enum Reading {
    /// Never asked.
    Unread,
    /// A good answer; if a later attempt failed, that failure is `failure` and is shown
    /// beside the value, whose age is how stale it is.
    Value {
        observed: Observed,
        age: Age,
        failure: Option<Failed>,
    },
    /// Asked, and no attempt has succeeded.
    Failure(Failed),
}

#[derive(Debug)]
struct Entry {
    good: Option<Observed>,
    /// Set by a failed attempt; cleared by the next success.
    failure: Option<Failed>,
}

impl Entry {
    /// When the latest attempt, successful or not, was made.
    fn attempted_at(&self) -> Option<Instant> {
        self.failure
            .as_ref()
            .map(|f| f.at)
            .or_else(|| self.good.as_ref().map(|g| g.read_at))
    }
}

#[derive(Debug)]
pub struct Cache {
    ttl: SignedDuration,
    /// Past this many entries, the one whose latest attempt is oldest is forgotten.
    entries_max: NonZeroUsize,
    entries: HashMap<PullRequestRef, Entry>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_ENTRIES_MAX)
    }
}

impl Cache {
    pub fn new(ttl: SignedDuration, entries_max: NonZeroUsize) -> Self {
        Self {
            ttl,
            entries_max,
            entries: HashMap::new(),
        }
    }

    /// Whether the next read should ask `gh` again: never asked, or the latest attempt is
    /// at least the TTL old. A clock that ran backwards makes the entry due, since its age
    /// cannot be trusted.
    pub fn is_due(&self, pr: &PullRequestRef, now: Instant) -> bool {
        let Some(attempted) = self.entries.get(pr).and_then(Entry::attempted_at) else {
            return true;
        };
        let elapsed = now.duration_since(attempted);
        elapsed.is_negative() || elapsed >= self.ttl
    }

    /// Files the outcome of one `gh` call made at `now`.
    pub fn record(&mut self, pr: &PullRequestRef, outcome: Result<Answer, GhError>, now: Instant) {
        let entry = self.entries.entry(pr.clone()).or_insert(Entry {
            good: None,
            failure: None,
        });
        match outcome {
            Ok(answer) => {
                entry.good = Some(Observed {
                    answer,
                    read_at: now,
                });
                entry.failure = None;
            }
            Err(error) => {
                entry.failure = Some(Failed { error, at: now });
            }
        }
        self.forget_oldest_over_the_bound();
    }

    fn forget_oldest_over_the_bound(&mut self) {
        while self.entries.len() > self.entries_max.get() {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.attempted_at())
                .map(|(pr, _)| pr.clone());
            let Some(oldest) = oldest else { break };
            self.entries.remove(&oldest);
        }
    }

    pub fn reading(&self, pr: &PullRequestRef, now: Instant) -> Reading {
        match self.entries.get(pr) {
            // `record` always sets one of the two, so an empty entry is never stored.
            None
            | Some(Entry {
                good: None,
                failure: None,
            }) => Reading::Unread,
            Some(Entry {
                good: Some(observed),
                failure,
            }) => Reading::Value {
                age: Age::between(observed.read_at, now),
                observed: observed.clone(),
                failure: failure.clone(),
            },
            Some(Entry {
                good: None,
                failure: Some(failed),
            }) => Reading::Failure(failed.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The cache is driven with explicit instants, so every age and every due-time is a
    //! hand-written number of seconds after `T0`.

    use super::*;
    use std::time::Duration;

    use crate::util::runner::ToolError;

    fn timeout() -> GhError {
        GhError::Tool(ToolError::TimedOut {
            program: "gh".to_owned(),
            after: Duration::from_secs(10),
        })
    }
    use crate::gh::{PullRequestRef, parse_pr_state};

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/gh/open_passing.json");
    const T0_SECS: i64 = 1_800_000_000;

    fn pr() -> PullRequestRef {
        PullRequestRef::new("example-org", "example-repo", 42).unwrap()
    }

    fn found() -> Answer {
        Answer::Found(parse_pr_state(FIXTURE, &pr()).unwrap())
    }

    fn t(secs_after_t0: i64) -> Instant {
        Instant::from_second(T0_SECS + secs_after_t0).unwrap()
    }

    fn cache_with_good_at_t0() -> Cache {
        let mut cache = Cache::default();
        cache.record(&pr(), Ok(found()), t(0));
        cache
    }

    #[test]
    fn nothing_recorded_is_unread_and_due() {
        let cache = Cache::default();
        assert!(matches!(cache.reading(&pr(), t(0)), Reading::Unread));
        assert!(cache.is_due(&pr(), t(0)));
    }

    #[test]
    fn a_good_answer_is_served_with_its_age_until_the_ttl_passes() {
        let cache = cache_with_good_at_t0();
        assert!(!cache.is_due(&pr(), t(119)));
        assert!(cache.is_due(&pr(), t(120)));
        let Reading::Value {
            observed,
            age,
            failure,
        } = cache.reading(&pr(), t(300))
        else {
            panic!("a recorded good answer reads as a value");
        };
        assert_eq!(observed.answer, found());
        assert_eq!(age, Age::Minutes(5));
        assert!(failure.is_none());
    }

    #[test]
    fn a_failure_after_a_good_answer_is_shown_beside_the_stale_value() {
        let mut cache = cache_with_good_at_t0();
        cache.record(&pr(), Err(timeout()), t(130));
        let Reading::Value {
            observed,
            age,
            failure: Some(failed),
        } = cache.reading(&pr(), t(140))
        else {
            panic!("the good answer survives a failed refresh, with the failure");
        };
        assert_eq!(observed.read_at, t(0));
        assert_eq!(age, Age::Minutes(2));
        assert!(matches!(
            failed.error,
            GhError::Tool(ToolError::TimedOut { .. })
        ));
        assert_eq!(failed.at, t(130));
    }

    #[test]
    fn a_failed_attempt_is_not_retried_before_the_ttl() {
        let mut cache = cache_with_good_at_t0();
        cache.record(&pr(), Err(timeout()), t(130));
        assert!(!cache.is_due(&pr(), t(249)));
        assert!(cache.is_due(&pr(), t(250)));
    }

    #[test]
    fn no_such_pull_request_is_an_answer_and_replaces_the_state_it_had() {
        let mut cache = cache_with_good_at_t0();
        cache.record(&pr(), Ok(Answer::NotFound), t(130));
        let Reading::Value {
            observed, failure, ..
        } = cache.reading(&pr(), t(131))
        else {
            panic!("an answer reads as a value");
        };
        assert_eq!(observed.answer, Answer::NotFound);
        assert_eq!(observed.read_at, t(130));
        assert!(failure.is_none());
    }

    #[test]
    fn a_later_success_clears_the_failure() {
        let mut cache = cache_with_good_at_t0();
        cache.record(&pr(), Err(timeout()), t(130));
        cache.record(&pr(), Ok(found()), t(260));
        let Reading::Value {
            observed, failure, ..
        } = cache.reading(&pr(), t(261))
        else {
            panic!("a recorded good answer reads as a value");
        };
        assert_eq!(observed.read_at, t(260));
        assert!(failure.is_none());
    }

    #[test]
    fn a_failure_with_no_good_answer_is_a_failure_not_a_blank() {
        let mut cache = Cache::default();
        cache.record(&pr(), Err(GhError::AuthRequired), t(0));
        let Reading::Failure(failed) = cache.reading(&pr(), t(5)) else {
            panic!("only a failure was recorded");
        };
        assert!(matches!(failed.error, GhError::AuthRequired));
    }

    #[test]
    fn a_clock_that_ran_backwards_makes_the_entry_due() {
        let cache = cache_with_good_at_t0();
        assert!(cache.is_due(&pr(), t(-60)));
    }

    fn pr_number(number: u32) -> PullRequestRef {
        PullRequestRef::new("example-org", "example-repo", number).unwrap()
    }

    /// Mutation: skip `forget_oldest_over_the_bound`, or evict the newest (`max_by_key`).
    #[test]
    fn the_entry_with_the_oldest_attempt_is_forgotten_past_the_bound() {
        let mut cache = Cache::new(DEFAULT_TTL, NonZeroUsize::new(2).unwrap());
        cache.record(&pr_number(1), Ok(found()), t(10));
        cache.record(&pr_number(2), Ok(found()), t(20));
        // Refreshing 1 makes 2 the oldest attempt.
        cache.record(&pr_number(1), Err(timeout()), t(30));
        cache.record(&pr_number(3), Ok(found()), t(40));
        assert!(matches!(
            cache.reading(&pr_number(2), t(41)),
            Reading::Unread
        ));
        assert!(matches!(
            cache.reading(&pr_number(1), t(41)),
            Reading::Value { .. }
        ));
        assert!(matches!(
            cache.reading(&pr_number(3), t(41)),
            Reading::Value { .. }
        ));
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn entries_are_per_pull_request() {
        let cache = cache_with_good_at_t0();
        let other = PullRequestRef::new("example-org", "example-repo", 43).unwrap();
        assert!(matches!(cache.reading(&other, t(1)), Reading::Unread));
    }
}
