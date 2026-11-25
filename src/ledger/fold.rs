//! The ledger's lines folded into what holds now.
//!
//! The fold trusts the order of the file and the checks the commands made before writing (see
//! [`super::command`]): a claim replaces the owner, a wait replaces the session's wait, a
//! deferral replaces the ticket's. It
//! keeps every entry as well, for the pages' timelines.

use std::collections::HashMap;

use super::{Act, Entry, Note, Outcome, Slug, Subject, WaitOn};
use crate::jira::resolution::{Closed, ClosedTickets};
use crate::types::{Instant, JiraKey, SessionId};

/// Who owns a subject, and since when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub session: SessionId,
    pub since: Instant,
}

/// A session's wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub on: WaitOn,
    pub then: Note,
    pub since: Instant,
    /// The ticket a time or prose wait is about (`conn wait --ticket`).
    pub about: Option<JiraKey>,
}

impl Wait {
    /// The ticket whose closing ends the wait: the one it waits on, or the one it is about.
    pub fn ticket(&self) -> Option<&JiraKey> {
        match &self.on {
            WaitOn::Subject(subject) => subject.ticket(),
            WaitOn::Rollout(_) | WaitOn::Until(_) | WaitOn::For(_) => self.about.as_ref(),
        }
    }
}

/// A rollout that has started and not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub session: SessionId,
    pub since: Instant,
    pub ships: Vec<Subject>,
}

/// How a rollout last ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub session: SessionId,
    pub at: Instant,
    pub outcome: Outcome,
    pub note: Option<Note>,
    /// What the run said it ships; empty when the finish had no recorded start.
    pub ships: Vec<Subject>,
}

/// One deployable's rollouts: the one running, if any, and how the last one ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rollout {
    pub running: Option<Run>,
    pub last_finish: Option<Finish>,
}

/// Where a session's wait stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState<'l> {
    /// Still waiting. For a rollout, `running` is the run under way, if one is.
    Waiting {
        wait: &'l Wait,
        running: Option<&'l Run>,
    },
    /// The rollout it waited on finished after the wait began.
    RolloutFinished { wait: &'l Wait, finish: &'l Finish },
    /// The time it waited for has come.
    TimeReached { wait: &'l Wait },
    /// The ticket or topic it waited on was released after the wait began.
    Released {
        wait: &'l Wait,
        by: &'l SessionId,
        at: Instant,
    },
    /// The ticket it waits on, or is about, has closed in Jira.
    TicketClosed {
        wait: &'l Wait,
        ticket: &'l JiraKey,
        closed: &'l Closed,
    },
}

impl<'l> WaitState<'l> {
    pub const fn wait(self) -> &'l Wait {
        match self {
            Self::Waiting { wait, .. }
            | Self::RolloutFinished { wait, .. }
            | Self::TimeReached { wait }
            | Self::Released { wait, .. }
            | Self::TicketClosed { wait, .. } => wait,
        }
    }

    /// The wait is over and the session has something to do.
    pub const fn is_over(self) -> bool {
        match self {
            Self::Waiting { .. } => false,
            Self::RolloutFinished { .. }
            | Self::TimeReached { .. }
            | Self::Released { .. }
            | Self::TicketClosed { .. } => true,
        }
    }
}

/// Where one deployable's release stands for the sessions whose work waits on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release<'l> {
    /// Under way; its finish ends the waits of `waiting`.
    Running {
        run: &'l Run,
        waiting: Vec<&'l SessionId>,
    },
    /// Not under way while sessions wait on its next finish: their work is held until someone
    /// releases it.
    Pending {
        first: &'l SessionId,
        others: Vec<&'l SessionId>,
        last: Option<&'l Finish>,
    },
    /// Not under way, and no session waits on it.
    Quiet { last: Option<&'l Finish> },
}

impl<'l> Release<'l> {
    /// The sessions still waiting on its next finish, in the order their waits began.
    pub fn waiting(&self) -> Vec<&'l SessionId> {
        match self {
            Self::Running { waiting, .. } => waiting.clone(),
            Self::Pending { first, others, .. } => std::iter::once(*first)
                .chain(others.iter().copied())
                .collect(),
            Self::Quiet { .. } => Vec::new(),
        }
    }
}

/// A shipped ticket's close check put off: not before `until`, and then to check `check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferral {
    pub until: Instant,
    pub check: Note,
    pub by: SessionId,
    pub since: Instant,
}

/// Whether a shipped ticket's close check can be done at a given time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCheck<'l> {
    /// Nobody put it off.
    Open,
    /// It was put off, and that time has come: `check` is what to check now.
    Reached(&'l Deferral),
    /// Not before the deferral's `until`.
    Deferred(&'l Deferral),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ledger {
    claims: HashMap<Subject, Held>,
    waits: HashMap<SessionId, Wait>,
    rollouts: HashMap<Slug, Rollout>,
    deferrals: HashMap<JiraKey, Deferral>,
    entries: Vec<Entry>,
    /// The tickets Jira was last read to have closed; not from the ledger's lines, given by
    /// whoever loaded it ([`Ledger::knowing_closed`]).
    closed: ClosedTickets,
}

impl Ledger {
    pub fn fold(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut ledger = Self::default();
        for entry in entries {
            ledger.apply(entry);
        }
        ledger
    }

    /// The ledger, knowing which tickets have closed: a wait on one of them, or about one, is
    /// over. A ticket not in `closed` is open or not read, and its waits stand.
    #[must_use]
    pub fn knowing_closed(mut self, closed: ClosedTickets) -> Self {
        self.closed = closed;
        self
    }

    /// Every ticket a wait still standing names: the ones whose closing a reader should watch.
    pub fn watched_tickets(&self, now: Instant) -> Vec<JiraKey> {
        let mut tickets: Vec<JiraKey> = self
            .waits
            .values()
            .filter(|wait| !self.state_of(wait, now).is_over())
            .filter_map(Wait::ticket)
            .cloned()
            .collect();
        tickets.sort_by_cached_key(ToString::to_string);
        tickets.dedup();
        tickets
    }

    pub fn apply(&mut self, entry: Entry) {
        let Entry { at, session, act } = &entry;
        match act {
            Act::Claim { subject, .. } => {
                self.claims.insert(
                    subject.clone(),
                    Held {
                        session: session.clone(),
                        since: *at,
                    },
                );
            }
            Act::Release { subject } => {
                if self
                    .claims
                    .get(subject)
                    .is_some_and(|held| held.session == *session)
                {
                    self.claims.remove(subject);
                }
            }
            Act::Wait { on, then, ticket } => {
                self.waits.insert(
                    session.clone(),
                    Wait {
                        on: on.clone(),
                        then: then.clone(),
                        since: *at,
                        about: ticket.clone(),
                    },
                );
            }
            Act::Unwait => {
                self.waits.remove(session);
            }
            Act::RolloutStart { rollout, ships } => {
                self.rollouts.entry(rollout.clone()).or_default().running = Some(Run {
                    session: session.clone(),
                    since: *at,
                    ships: ships.clone(),
                });
            }
            Act::RolloutFinish {
                rollout,
                outcome,
                note,
            } => {
                let state = self.rollouts.entry(rollout.clone()).or_default();
                let ships = state.running.take().map_or_default(|run| run.ships);
                state.last_finish = Some(Finish {
                    session: session.clone(),
                    at: *at,
                    outcome: *outcome,
                    note: note.clone(),
                    ships,
                });
            }
            Act::Defer {
                ticket,
                until,
                check,
            } => {
                self.deferrals.insert(
                    ticket.clone(),
                    Deferral {
                        until: *until,
                        check: check.clone(),
                        by: session.clone(),
                        since: *at,
                    },
                );
            }
        }
        self.entries.push(entry);
    }

    /// Whether `ticket`'s close check can be done at `now`.
    pub fn close_check(&self, ticket: &JiraKey, now: Instant) -> CloseCheck<'_> {
        match self.deferrals.get(ticket) {
            None => CloseCheck::Open,
            Some(deferral) if deferral.until <= now => CloseCheck::Reached(deferral),
            Some(deferral) => CloseCheck::Deferred(deferral),
        }
    }

    /// The close checks still put off at `now`, the soonest first.
    pub fn deferred(&self, now: Instant) -> Vec<(&JiraKey, &Deferral)> {
        let mut deferred: Vec<_> = self
            .deferrals
            .iter()
            .filter(|(_, deferral)| deferral.until > now)
            .collect();
        deferred.sort_by_cached_key(|(ticket, deferral)| (deferral.until, ticket.to_string()));
        deferred
    }

    pub fn owner(&self, subject: &Subject) -> Option<&Held> {
        self.claims.get(subject)
    }

    /// Who owned `subject` at `at`, replaying the claims and releases written by then.
    pub fn owner_at(&self, subject: &Subject, at: Instant) -> Option<&SessionId> {
        let mut owner = None;
        for entry in self.entries.iter().take_while(|entry| entry.at <= at) {
            match &entry.act {
                Act::Claim { subject: s, .. } if s == subject => owner = Some(&entry.session),
                Act::Release { subject: s } if s == subject && owner == Some(&entry.session) => {
                    owner = None;
                }
                Act::Claim { .. }
                | Act::Release { .. }
                | Act::Wait { .. }
                | Act::Unwait
                | Act::RolloutStart { .. }
                | Act::RolloutFinish { .. }
                | Act::Defer { .. } => {}
            }
        }
        owner
    }

    /// Every claim, ordered by subject as written.
    pub fn claims(&self) -> Vec<(&Subject, &Held)> {
        let mut claims: Vec<_> = self.claims.iter().collect();
        claims.sort_by_cached_key(|(subject, _)| subject.to_string());
        claims
    }

    /// What the session owns, ordered by subject as written.
    pub fn claims_of(&self, session: &SessionId) -> Vec<&Subject> {
        self.claims()
            .into_iter()
            .filter(|(_, held)| held.session == *session)
            .map(|(subject, _)| subject)
            .collect()
    }

    pub fn rollout(&self, name: &Slug) -> Option<&Rollout> {
        self.rollouts.get(name)
    }

    /// Every rollout ever started or finished, by name.
    pub fn rollouts(&self) -> Vec<(&Slug, &Rollout)> {
        let mut rollouts: Vec<_> = self.rollouts.iter().collect();
        rollouts.sort_by_key(|(name, _)| *name);
        rollouts
    }

    /// The running rollout that said it ships `subject`, if one does.
    pub fn shipping(&self, subject: &Subject) -> Option<(&Slug, &Run)> {
        self.rollouts().into_iter().find_map(|(name, rollout)| {
            rollout
                .running
                .as_ref()
                .filter(|run| run.ships.contains(subject))
                .map(|run| (name, run))
        })
    }

    /// The session's wait and where it stands at `now`.
    pub fn wait_of(&self, session: &SessionId, now: Instant) -> Option<WaitState<'_>> {
        let wait = self.waits.get(session)?;
        Some(self.state_of(wait, now))
    }

    /// Every session's wait, ordered by when it began.
    pub fn waits(&self, now: Instant) -> Vec<(&SessionId, WaitState<'_>)> {
        let mut waits: Vec<_> = self
            .waits
            .iter()
            .map(|(session, wait)| (session, self.state_of(wait, now)))
            .collect();
        waits.sort_by_key(|(_, state)| state.wait().since);
        waits
    }

    /// The sessions waiting on this rollout, finished or not.
    pub fn waiting_on(&self, rollout: &Slug, now: Instant) -> Vec<(&SessionId, WaitState<'_>)> {
        self.waits(now)
            .into_iter()
            .filter(
                |(_, state)| matches!(&state.wait().on, WaitOn::Rollout(name) if name == rollout),
            )
            .collect()
    }

    /// Where the release of `name` stands at `now`.
    pub fn release(&self, name: &Slug, now: Instant) -> Release<'_> {
        let rollout = self.rollouts.get(name);
        let last = rollout.and_then(|rollout| rollout.last_finish.as_ref());
        let mut waiting = self
            .waiting_on(name, now)
            .into_iter()
            .filter(|(_, state)| !state.is_over())
            .map(|(session, _)| session);
        match rollout.and_then(|rollout| rollout.running.as_ref()) {
            Some(run) => Release::Running {
                run,
                waiting: waiting.collect(),
            },
            None => match waiting.next() {
                Some(first) => Release::Pending {
                    first,
                    others: waiting.collect(),
                    last,
                },
                None => Release::Quiet { last },
            },
        }
    }

    /// Every deployable a session started, finished or waits on, by name, with where its
    /// release stands at `now`. One only waited on is here too: nobody has started it yet.
    pub fn releases(&self, now: Instant) -> Vec<(&Slug, Release<'_>)> {
        let awaited = self.waits.values().filter_map(|wait| match &wait.on {
            WaitOn::Rollout(name) => Some(name),
            WaitOn::Subject(_) | WaitOn::Until(_) | WaitOn::For(_) => None,
        });
        let names: std::collections::BTreeSet<&Slug> =
            self.rollouts.keys().chain(awaited).collect();
        names
            .into_iter()
            .map(|name| (name, self.release(name, now)))
            .collect()
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Where `wait` stands at `now`. A release or a rollout's finish is the reason it is over
    /// when there is one; else a closed ticket ends it, whether it still stood or its time had
    /// come, since the ticket says more than the clock.
    fn state_of<'l>(&'l self, wait: &'l Wait, now: Instant) -> WaitState<'l> {
        let state = self.state_by_ledger(wait, now);
        match state {
            WaitState::Waiting { .. } | WaitState::TimeReached { .. } => wait
                .ticket()
                .and_then(|ticket| {
                    self.closed
                        .get(ticket)
                        .map(|closed| WaitState::TicketClosed {
                            wait,
                            ticket,
                            closed,
                        })
                })
                .unwrap_or(state),
            WaitState::RolloutFinished { .. }
            | WaitState::Released { .. }
            | WaitState::TicketClosed { .. } => state,
        }
    }

    /// Where `wait` stands by the ledger's lines and the clock alone.
    fn state_by_ledger<'l>(&'l self, wait: &'l Wait, now: Instant) -> WaitState<'l> {
        match &wait.on {
            WaitOn::Rollout(name) => {
                let rollout = self.rollouts.get(name);
                match rollout.and_then(|rollout| rollout.last_finish.as_ref()) {
                    Some(finish) if finish.at > wait.since => {
                        WaitState::RolloutFinished { wait, finish }
                    }
                    Some(_) | None => WaitState::Waiting {
                        wait,
                        running: rollout.and_then(|rollout| rollout.running.as_ref()),
                    },
                }
            }
            WaitOn::Subject(subject) => {
                let released = self.entries.iter().find(|entry| {
                    entry.at > wait.since
                        && matches!(&entry.act, Act::Release { subject: s } if s == subject)
                });
                match released {
                    Some(entry) => WaitState::Released {
                        wait,
                        by: &entry.session,
                        at: entry.at,
                    },
                    None => WaitState::Waiting {
                        wait,
                        running: None,
                    },
                }
            }
            WaitOn::Until(when) if *when <= now => WaitState::TimeReached { wait },
            WaitOn::Until(_) | WaitOn::For(_) => WaitState::Waiting {
                wait,
                running: None,
            },
        }
    }
}
