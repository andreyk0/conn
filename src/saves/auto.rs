//! When the automatic save changes.
//!
//! A session that opens joins at once: no crash opens one. A session that stops running leaves
//! only after [`LEAVE_AFTER`], and only while the machine says it is up: a forced reboot stops
//! every session and then conn itself, and in that minute either conn is gone or systemd says
//! `stopping`, so the save keeps the sessions the reboot took. A session that cannot be
//! classified neither joins nor leaves.

use std::collections::{BTreeMap, HashMap, HashSet};

use jiff::SignedDuration;

use super::SavedSession;
use crate::types::{Instant, SessionId};

/// How long a session must have stopped running before it leaves the automatic save. A
/// shutdown stops the user's processes within seconds of each other and conn with them; a
/// minute is well past that and short enough that a closed session soon drops out.
pub const LEAVE_AFTER: SignedDuration = SignedDuration::from_secs(60);

/// The open sessions as one fully read model shows them.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// The sessions with a live process.
    pub live: Vec<SavedSession>,
    /// The sessions whose activity could not be determined.
    pub unknown: HashSet<SessionId>,
}

/// The sessions the automatic save holds, and since when each one not running has been gone.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    members: BTreeMap<SessionId, SavedSession>,
    /// The members seen not running, from when; a member running again is taken out.
    leaving: HashMap<SessionId, Instant>,
}

impl Tracker {
    /// A tracker holding `sessions`, as the automatic save last held them.
    pub fn holding(sessions: Vec<SavedSession>) -> Self {
        Self {
            members: sessions.into_iter().map(|s| (s.id.clone(), s)).collect(),
            leaving: HashMap::new(),
        }
    }

    /// Takes in what is open at `at`. A live session joins or has its details brought up to
    /// date; a member neither live nor unknown starts leaving; a live one stops leaving. True
    /// when what the save says about resuming changed.
    pub fn observe(&mut self, seen: Observation, at: Instant) -> bool {
        let mut changed = false;
        let live: HashSet<SessionId> = seen.live.iter().map(|s| s.id.clone()).collect();
        for session in seen.live {
            self.leaving.remove(&session.id);
            let same = self
                .members
                .get(&session.id)
                .is_some_and(|held| held.resumes_as(&session));
            changed |= !same;
            self.members.insert(session.id.clone(), session);
        }
        for id in self.members.keys() {
            if !live.contains(id) && !seen.unknown.contains(id) {
                self.leaving.entry(id.clone()).or_insert(at);
            }
        }
        changed
    }

    /// Whether a member has been gone for [`LEAVE_AFTER`] at `at`.
    pub fn leaving_due(&self, at: Instant) -> bool {
        self.leaving
            .values()
            .any(|since| at.duration_since(*since) >= LEAVE_AFTER)
    }

    /// Removes the members gone for [`LEAVE_AFTER`]; the caller has checked the machine is
    /// up. True when any left.
    pub fn leave(&mut self, at: Instant) -> bool {
        let gone: Vec<SessionId> = self
            .leaving
            .iter()
            .filter(|(_, since)| at.duration_since(**since) >= LEAVE_AFTER)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &gone {
            self.leaving.remove(id);
            self.members.remove(id);
        }
        !gone.is_empty()
    }

    /// The members, the most recently active first.
    pub fn sessions(&self) -> Vec<SavedSession> {
        super::most_recent_first(self.members.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    //! The tracker driven through observations written by hand, a minute at a time.

    use super::*;
    use crate::saves::Resume;

    fn id(n: u8) -> SessionId {
        SessionId::parse(&format!("00000000-0000-4000-8000-{n:012}")).unwrap()
    }

    fn at(secs: i64) -> Instant {
        Instant::from_second(1_700_000_000 + secs).unwrap()
    }

    fn session(n: u8, title: &str) -> SavedSession {
        SavedSession {
            id: id(n),
            title: Some(title.to_owned()),
            gist: None,
            cwd: None,
            last_activity: Some(at(i64::from(n))),
            resume: Resume::Unavailable {
                why: "test".to_owned(),
            },
        }
    }

    fn live(sessions: &[SavedSession]) -> Observation {
        Observation {
            live: sessions.to_vec(),
            unknown: HashSet::new(),
        }
    }

    fn held(tracker: &Tracker) -> Vec<SessionId> {
        tracker.sessions().into_iter().map(|s| s.id).collect()
    }

    /// A new session is a change at once; the same sessions again are not, whatever their
    /// activity time; a new title is.
    /// Mutation: compare whole sessions (the activity time makes a change), or never report a
    /// joining session.
    #[test]
    fn a_session_joins_at_once_and_only_resume_details_are_a_change() {
        let mut tracker = Tracker::default();
        assert!(tracker.observe(live(&[session(1, "a")]), at(0)));
        let mut later = session(1, "a");
        later.last_activity = Some(at(500));
        later.gist = Some("now doing b".to_owned());
        assert!(!tracker.observe(live(&[later]), at(10)));
        assert!(tracker.observe(live(&[session(1, "renamed")]), at(20)));
        assert_eq!(held(&tracker), [id(1)]);
    }

    /// A session that stops running stays for a minute and leaves after it; one running again
    /// within the minute never leaves; an unknown one never starts leaving.
    /// Mutation: leave at once, compare with `>` (the session is still held at 60 s), keep the
    /// leaving mark when the session runs again, or let an unknown one start leaving.
    #[test]
    fn a_stopped_session_leaves_after_a_minute_unless_it_runs_again() {
        let mut tracker = Tracker::default();
        tracker.observe(
            live(&[session(1, "a"), session(2, "b"), session(3, "c")]),
            at(0),
        );
        let mut seen = live(&[session(2, "b")]);
        seen.unknown.insert(id(3));
        assert!(!tracker.observe(seen, at(10)));
        assert!(!tracker.leaving_due(at(69)));
        assert!(!tracker.leave(at(69)));
        // Session 2 stopped at 30 and came back at 50.
        tracker.observe(
            Observation {
                live: vec![],
                unknown: HashSet::from([id(3)]),
            },
            at(30),
        );
        tracker.observe(
            Observation {
                live: vec![session(2, "b")],
                unknown: HashSet::from([id(3)]),
            },
            at(50),
        );
        assert!(tracker.leaving_due(at(70)));
        assert!(tracker.leave(at(70)));
        assert_eq!(held(&tracker), [id(3), id(2)]);
        assert!(!tracker.leaving_due(at(1000)));
    }
}
