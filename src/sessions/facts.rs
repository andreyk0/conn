//! Vocabulary shared by the listing and the registry.

use std::fmt;

use crate::types::WaitingFor;

/// What a live Claude Code process reports about itself.
///
/// `Busy` means something is running: a turn, a background shell or a monitor. It does not
/// mean a turn is running, so it cannot by itself tell "working" from "idle with background
/// work"; that needs the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Busy,
    Waiting,
    Idle,
    /// A status this code does not know, verbatim.
    Other(String),
}

impl ProcessStatus {
    pub fn parse(text: &str) -> Self {
        match text {
            "busy" => Self::Busy,
            "waiting" => Self::Waiting,
            "idle" => Self::Idle,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Reads the `waitingFor` text of a session that stopped for the human.
pub(super) fn waiting_for(text: &str) -> WaitingFor {
    match text {
        "permission prompt" => WaitingFor::PermissionPrompt,
        "input needed" => WaitingFor::InputNeeded,
        "sandbox request" => WaitingFor::SandboxRequest,
        "worker request" => WaitingFor::WorkerRequest,
        "dialog open" => WaitingFor::DialogOpen,
        "quota hold" => WaitingFor::QuotaHold,
        other => WaitingFor::Other(other.to_owned()),
    }
}

/// The handle of a background session (`claude attach|logs|stop <id>`).
///
/// Witnesses that the text is non-empty and a single word, so it can be pasted into a command.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a job id: {0:?}")]
pub struct JobIdError(String);

impl JobId {
    pub fn parse(text: &str) -> Result<Self, JobIdError> {
        let plain = !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if plain {
            Ok(Self(text.to_owned()))
        } else {
            Err(JobIdError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutation: make `parse` return `Busy` for every input -> red on all three rows.
    #[test]
    fn statuses_parse_and_unknown_ones_survive() {
        assert_eq!(ProcessStatus::parse("busy"), ProcessStatus::Busy);
        assert_eq!(ProcessStatus::parse("waiting"), ProcessStatus::Waiting);
        assert_eq!(ProcessStatus::parse("idle"), ProcessStatus::Idle);
        assert_eq!(
            ProcessStatus::parse("hibernating"),
            ProcessStatus::Other("hibernating".into())
        );
    }

    /// Mutation: swap two arms of `waiting_for` -> red.
    #[test]
    fn wait_reasons_map_one_to_one() {
        let table = [
            ("permission prompt", WaitingFor::PermissionPrompt),
            ("input needed", WaitingFor::InputNeeded),
            ("sandbox request", WaitingFor::SandboxRequest),
            ("worker request", WaitingFor::WorkerRequest),
            ("dialog open", WaitingFor::DialogOpen),
            ("quota hold", WaitingFor::QuotaHold),
        ];
        for (text, expected) in table {
            assert_eq!(waiting_for(text), expected, "{text}");
        }
    }

    /// Mutation: accept the empty string -> red.
    #[test]
    fn job_ids_are_one_plain_word() {
        assert!(JobId::parse("abcd1234").is_ok());
        for bad in ["", "a b", "a;b", "$(x)"] {
            assert!(JobId::parse(bad).is_err(), "{bad:?}");
        }
    }
}
