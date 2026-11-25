//! What is waiting on the human.
//!
//! **The pending rule.** A session is pending when its latest turn end has no later
//! human-typed prompt in that session, whoever started the turn. The trigger ranks
//! (`Human` above `TaskNotification` above the rest) and never filters: a turn started by
//! a scheduled wake-up or a peer is still waiting on the human once it ends.

use super::{AskUserQuestion, Instant, SessionId, TurnEnd, WaitingFor};

/// One card on the pending page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCard {
    /// The process is stopped on a wait only the human can end, other than a question: a
    /// permission prompt or any other harness-owned wait.
    Blocked {
        session: SessionId,
        on: WaitingFor,
        since: Instant,
    },
    /// The latest turn ended by asking a structured question.
    Asked {
        session: SessionId,
        asked: AskUserQuestion,
        since: Instant,
    },
    /// The latest turn ended with prose.
    Turn {
        session: SessionId,
        turn: TurnEnd,
        since: Instant,
    },
}

impl PendingCard {
    /// Since when the card has waited: what tells one card of a session from its next.
    pub const fn since(&self) -> Instant {
        match self {
            Self::Asked { since, .. } | Self::Blocked { since, .. } | Self::Turn { since, .. } => {
                *since
            }
        }
    }
}
