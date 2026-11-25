//! Where the sessions' actual writes say the ledger was not followed: a ticket or a Slack
//! thread several sessions wrote to, or a ticket written to by a session that did not own it.
//!
//! The ledger is what sessions declare; their transcripts say what they did. Comparing the two
//! is how a collision shows up even when no session claimed anything, and how a missed claim
//! or a release session moving an owned ticket shows up at all.

use std::collections::{BTreeSet, HashMap};

use super::{Ledger, Subject};
use crate::types::outward::{Target, WriteKind, Written};
use crate::types::{Instant, SessionId};

/// Who owned a ticket when a session wrote to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerThen {
    /// No session had claimed it; for a Slack thread, always.
    Unowned,
    /// The writer itself.
    Writer,
    /// Another session.
    Other(SessionId),
}

/// One write to a shared target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedWrite {
    pub at: Instant,
    pub session: SessionId,
    pub kind: WriteKind,
    pub excerpt: Option<String>,
    pub owner_then: OwnerThen,
}

/// A target whose writes say coordination slipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub target: Target,
    /// Every write to it in the window, oldest first.
    pub writes: Vec<SharedWrite>,
}

impl Shared {
    /// The distinct sessions that wrote, in order of their first write.
    pub fn writers(&self) -> Vec<&SessionId> {
        let mut seen = BTreeSet::new();
        self.writes
            .iter()
            .map(|write| &write.session)
            .filter(|session| seen.insert(*session))
            .collect()
    }

    /// Writes made while another session owned the target.
    pub fn foreign(&self) -> impl Iterator<Item = &SharedWrite> {
        self.writes
            .iter()
            .filter(|write| matches!(write.owner_then, OwnerThen::Other(_)))
    }

    pub fn latest(&self) -> Option<Instant> {
        self.writes.last().map(|write| write.at)
    }
}

/// The targets written to since `since` by two sessions or more, or by a session that did not
/// own them; the most recently written first. A post to a channel outside any thread is left
/// out: announcements in a team channel are many sessions' normal business.
pub fn find<'w>(
    writes: impl IntoIterator<Item = (&'w SessionId, &'w Written)>,
    ledger: &Ledger,
    since: Instant,
) -> Vec<Shared> {
    let mut by_target: HashMap<Target, Vec<SharedWrite>> = HashMap::new();
    for (session, written) in writes {
        if written.at < since {
            continue;
        }
        let target = &written.write.target;
        let owner_then = match target {
            Target::Ticket(key) => {
                match ledger.owner_at(&Subject::Ticket(key.clone()), written.at) {
                    None => OwnerThen::Unowned,
                    Some(owner) if owner == session => OwnerThen::Writer,
                    Some(owner) => OwnerThen::Other(owner.clone()),
                }
            }
            Target::Slack { thread: None, .. } => continue,
            Target::Slack {
                thread: Some(_), ..
            } => OwnerThen::Unowned,
        };
        by_target
            .entry(target.clone())
            .or_default()
            .push(SharedWrite {
                at: written.at,
                session: session.clone(),
                kind: written.write.kind,
                excerpt: written.write.excerpt.clone(),
                owner_then,
            });
    }
    let mut shared: Vec<Shared> = by_target
        .into_iter()
        .map(|(target, mut writes)| {
            writes.sort_by_key(|write| write.at);
            Shared { target, writes }
        })
        .filter(|shared| shared.writers().len() >= 2 || shared.foreign().next().is_some())
        .collect();
    shared.sort_by_key(|shared| std::cmp::Reverse(shared.latest()));
    shared
}
