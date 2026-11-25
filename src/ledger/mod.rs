//! The coordination ledger: who owns which shared thing, what each idle session waits for,
//! which rollouts are running, and which shipped tickets cannot have their close check yet.
//!
//! Sessions write it themselves through `conn claim`, `conn wait`, `conn rollout` and `conn
//! defer`, because none of it can be read back out of a transcript: whether a session waits on
//! a rollout, and what it means to do once the rollout lands, is prose. The pages, the hooks
//! and `conn who` read it.
//!
//! `ledger.jsonl` in the state directory, one [`Entry`] per line, appended under a lock by the
//! commands that check the ledger before they write (see [`store`]).

pub mod command;
pub mod fold;
pub mod guard;
pub mod remind;
pub mod shared;
pub mod store;

#[cfg(test)]
mod tests;

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::{Instant, JiraKey, SessionId};

pub use fold::{
    CloseCheck, Deferral, Finish, Held, Ledger, Release, Rollout, Run, Wait, WaitState,
};

/// The ledger file, inside the state directory.
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// A thing one session owns at a time: a ticket, a rollout of one deployable, or a named piece
/// of follow-up work with no ticket of its own.
///
/// Written `ABC-123`, `rollout:<slug>` or `topic:<slug>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Subject {
    Ticket(JiraKey),
    Rollout(Slug),
    Topic(Slug),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not something to own: write a ticket key (ABC-123), rollout:<name> or topic:<name>, \
     a name being lowercase letters, digits, '-', '_' or '.'"
)]
pub struct SubjectError(String);

const ROLLOUT_PREFIX: &str = "rollout:";
const TOPIC_PREFIX: &str = "topic:";

impl Subject {
    pub fn parse(text: &str) -> Result<Self, SubjectError> {
        let refuse = || SubjectError(text.to_owned());
        if let Some(name) = text.strip_prefix(ROLLOUT_PREFIX) {
            return Slug::parse(name).map(Self::Rollout).map_err(|_| refuse());
        }
        if let Some(name) = text.strip_prefix(TOPIC_PREFIX) {
            return Slug::parse(name).map(Self::Topic).map_err(|_| refuse());
        }
        JiraKey::parse(text).map(Self::Ticket).ok_or_else(refuse)
    }

    /// The ticket, when this is one.
    pub const fn ticket(&self) -> Option<&JiraKey> {
        match self {
            Self::Ticket(key) => Some(key),
            Self::Rollout(_) | Self::Topic(_) => None,
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ticket(key) => write!(f, "{key}"),
            Self::Rollout(name) => write!(f, "{ROLLOUT_PREFIX}{name}"),
            Self::Topic(name) => write!(f, "{TOPIC_PREFIX}{name}"),
        }
    }
}

impl TryFrom<String> for Subject {
    type Error = SubjectError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Subject> for String {
    fn from(subject: Subject) -> Self {
        subject.to_string()
    }
}

/// The name of a rollout or a topic.
///
/// Witnesses 1 to 64 characters, each a lowercase ASCII letter, a digit, `-`, `_` or `.`,
/// starting with a letter or digit: safe in a URL path, a page and a terminal as it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Slug(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not a name: 1 to 64 lowercase letters, digits, '-', '_' or '.', starting with a \
     letter or digit"
)]
pub struct SlugError(String);

impl Slug {
    const LEN: std::ops::RangeInclusive<usize> = 1..=64;

    pub fn parse(text: &str) -> Result<Self, SlugError> {
        let first_ok = text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok = text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
        if first_ok && rest_ok && Self::LEN.contains(&text.len()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(SlugError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Slug {
    type Error = SlugError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Slug> for String {
    fn from(slug: Slug) -> Self {
        slug.0
    }
}

/// A sentence a session wrote for a person to read: what it waits for, what it will do next,
/// how a rollout ended.
///
/// Witnesses non-blank text of at most [`Note::CHARS_MAX`] characters, trimmed. It is the
/// session's own words, so a page shows it redacted like any other session text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Note(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoteError {
    #[error("the text is empty")]
    Blank,
    #[error("the text is {chars} characters; at most {} fit", Note::CHARS_MAX)]
    TooLong { chars: usize },
}

impl Note {
    /// A line or two: the ledger says what happens next, the transcript says why.
    pub const CHARS_MAX: usize = 400;

    pub fn parse(text: &str) -> Result<Self, NoteError> {
        let text = text.trim();
        let chars = text.chars().count();
        if chars == 0 {
            Err(NoteError::Blank)
        } else if chars > Self::CHARS_MAX {
            Err(NoteError::TooLong { chars })
        } else {
            Ok(Self(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Note {
    type Error = NoteError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Note> for String {
    fn from(note: Note) -> Self {
        note.0
    }
}

/// What an idle session waits for. A rollout, a ticket or topic, and a time end the wait by
/// themselves; anything else (a person, tomorrow's data run, a reply) is said in words and ended
/// by the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitOn {
    /// The next finish of this rollout after the wait began.
    Rollout(Slug),
    /// A ticket or topic another session works on: over once it is released after the wait
    /// began, which its owner does when its part is done.
    Subject(Subject),
    Until(Instant),
    For(Note),
}

/// How a rollout ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Shipped,
    RolledBack,
    Abandoned,
}

impl Outcome {
    pub const fn words(self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::RolledBack => "rolled back",
            Self::Abandoned => "abandoned",
        }
    }
}

/// One thing a session did to the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Act {
    /// The session owns the subject from now; `took_from` is the owner it replaced, if any.
    Claim {
        subject: Subject,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        took_from: Option<SessionId>,
    },
    Release {
        subject: Subject,
    },
    /// The session's wait, replacing any earlier one. `ticket` is a ticket a time or prose wait
    /// is about: the wait is over once it closes. Absent from lines written before it existed,
    /// and from every wait that names no such ticket.
    Wait {
        on: WaitOn,
        then: Note,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<JiraKey>,
    },
    /// The session is no longer waiting.
    Unwait,
    RolloutStart {
        rollout: Slug,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ships: Vec<Subject>,
    },
    RolloutFinish {
        rollout: Slug,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<Note>,
    },
    /// The shipped ticket's close check cannot be done before `until`, when `check` is what to
    /// check; replaces any earlier deferral of the ticket.
    Defer {
        ticket: JiraKey,
        until: Instant,
        check: Note,
    },
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub at: Instant,
    pub session: SessionId,
    pub act: Act,
}
