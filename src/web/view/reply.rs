//! The reply composer: the text of the turn a session waits on, marked so that the page's
//! script can hang a reply under any block of it, and the bar that turns the replies into one
//! message.
//!
//! Composing is a mode the human switches a card into with its Reply button, never something
//! a click on the text starts: until then the text reads, selects and follows links as any
//! other. In the mode each block shows a ↩ that opens a reply under it; Discard, or sending,
//! leaves the mode.
//!
//! The message reaches the session the way a typed one does: the script copies it to the
//! clipboard and raises the session's terminal, where the human pastes it and presses Enter.
//! The server never sends it anywhere, so a reply changes nothing about who can make a
//! session act: the human's own keystroke submits it.
//!
//! A reply answers one turn, or one open question: then each option is a block, and a reply
//! left empty under it picks it. Its draft is kept per tab under the session and the instant
//! the turn ended or the question was asked, so a draft written for one is never offered under
//! the next.

use maud::{Markup, html};

use super::reach::Reach;
use crate::model::SessionView;
use crate::types::{Instant, PendingCard, SessionId};

/// How a finished reply leaves the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Copied, and the session's terminal raised to paste it into.
    Terminal,
    /// Copied only: no live terminal runs the session, so it is pasted after resuming it.
    Copy,
}

impl Delivery {
    /// How a reply reaches the session `reach` leads to; none for a session with nothing to
    /// resume.
    pub(super) const fn of(reach: &Reach<'_>) -> Option<Self> {
        match reach {
            Reach::Terminal { .. } => Some(Self::Terminal),
            Reach::Resume(_) => Some(Self::Copy),
            Reach::Gone => None,
        }
    }
}

/// What a reply answers, which decides where in the terminal it is pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answers {
    /// A finished turn: pasted at the prompt.
    Turn,
    /// An open structured question: pasted as the dialog's own free-text answer.
    Question,
}

impl Answers {
    const fn attribute(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Question => "question",
        }
    }
}

/// The instant the turn `view`'s session waits on ended: the one turn a reply can answer on
/// the session's page. An open question is answered from its card; a harness prompt (a
/// permission, a sandbox request) only in the terminal.
pub(super) const fn waiting_turn(view: &SessionView) -> Option<Instant> {
    match &view.pending {
        Some(PendingCard::Turn { turn, .. }) => Some(turn.at),
        Some(PendingCard::Asked { .. } | PendingCard::Blocked { .. }) | None => None,
    }
}

/// `body`, the text of the turn that ended at `turn_at`, made answerable block by block, with
/// the composer under it; `body` as it is when no reply can reach the session.
pub(super) fn answerable(
    session: &SessionId,
    turn_at: Instant,
    answers: Answers,
    delivery: Option<Delivery>,
    body: Markup,
) -> Markup {
    let Some(delivery) = delivery else {
        return body;
    };
    let send = match delivery {
        Delivery::Terminal => "Copy & open terminal",
        Delivery::Copy => "Copy reply",
    };
    html! {
        div.replyable data-reply-key={ (session.as_str()) "@" (turn_at) } data-answers=(answers.attribute()) {
            (body)
            div.composer {
                button.reply-open type="button" title="Compose a reply: each line gets a ↩ that answers it" { "Reply" }
                div.composer-body hidden {
                    textarea.reply-general rows="2" placeholder="Anything else to say" {}
                    div.actions {
                        button.primary.reply-send type="button"
                            data-delivery=(match delivery { Delivery::Terminal => "terminal", Delivery::Copy => "copy" })
                        { (send) }
                        button.reply-discard type="button" { "Discard" }
                        span.reply-note.muted {}
                    }
                }
            }
        }
    }
}
