//! The latest read of something about each of a set of tickets, one `acli` call per ticket:
//! a ticket's links, an epic's children.
//!
//! A failed read is kept as its own outcome, never as an empty answer; a ticket that leaves the
//! set is forgotten, so what it last said stops counting.

use std::collections::{HashMap, HashSet};

use jiff::SignedDuration;

use super::JiraError;
use crate::model::SourceState;
use crate::types::{Instant, JiraKey};

/// The latest read of each ticket, and when it was made.
#[derive(Debug, Clone)]
pub struct PerTicket<T> {
    every: SignedDuration,
    by_key: HashMap<JiraKey, (Instant, Result<T, String>)>,
}

impl<T> PerTicket<T> {
    /// Reads that fall due `every` after the last one.
    pub fn new(every: SignedDuration) -> Self {
        Self {
            every,
            by_key: HashMap::new(),
        }
    }

    /// Whether `key` has no read younger than the period.
    pub fn is_due(&self, key: &JiraKey, now: Instant) -> bool {
        self.by_key
            .get(key)
            .is_none_or(|(at, _)| now.duration_since(*at) >= self.every)
    }

    pub fn record(&mut self, key: JiraKey, outcome: Result<T, JiraError>, at: Instant) {
        self.by_key
            .insert(key, (at, outcome.map_err(|error| error.to_string())));
    }

    /// Forgets every ticket outside `wanted`.
    pub fn retain(&mut self, wanted: &HashSet<&JiraKey>) {
        self.by_key.retain(|key, _| wanted.contains(key));
    }

    /// The latest outcome for `key`; `None` before its first read.
    pub fn get(&self, key: &JiraKey) -> Option<&Result<T, String>> {
        self.by_key.get(key).map(|(_, outcome)| outcome)
    }

    /// Every good read.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.by_key
            .values()
            .filter_map(|(_, outcome)| outcome.as_ref().ok())
    }

    /// Where the reads stand: the latest failure, else the latest read, else none yet.
    pub fn state(&self) -> SourceState {
        let failed = self
            .by_key
            .iter()
            .filter_map(|(key, (at, outcome))| outcome.as_ref().err().map(|e| (key, at, e)))
            .max_by_key(|(_, at, _)| **at);
        if let Some((key, at, reason)) = failed {
            return SourceState::Failed {
                at: *at,
                reason: format!("{key}: {reason}"),
            };
        }
        self.by_key
            .values()
            .map(|(at, _)| *at)
            .max()
            .map_or(SourceState::NeverRead, |at| SourceState::Ok { at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> JiraKey {
        JiraKey::parse(text).unwrap()
    }

    fn at(minutes: i64) -> Instant {
        Instant::from_second(1_700_000_000 + minutes * 60).unwrap()
    }

    /// Mutation: compare with `>` instead of `>=`, or make `retain` keep everything.
    #[test]
    fn a_read_falls_due_after_the_period_and_a_dropped_ticket_is_forgotten() {
        let mut reads: PerTicket<u8> = PerTicket::new(SignedDuration::from_mins(10));
        assert!(reads.is_due(&key("ABC-1"), at(0)));
        reads.record(key("ABC-1"), Ok(1), at(0));
        reads.record(key("ABC-2"), Err(JiraError::Timeout), at(1));
        assert!(!reads.is_due(&key("ABC-1"), at(9)));
        assert!(reads.is_due(&key("ABC-1"), at(10)));
        assert_eq!(reads.values().copied().collect::<Vec<_>>(), [1]);
        assert!(matches!(reads.state(), SourceState::Failed { .. }));

        let keep = key("ABC-1");
        reads.retain(&HashSet::from([&keep]));
        assert!(reads.get(&key("ABC-2")).is_none());
        assert_eq!(reads.state(), SourceState::Ok { at: at(0) });
    }
}
