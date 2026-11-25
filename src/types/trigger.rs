//! The one reading of "who started this turn", shared by the transcript and the hook log.

use super::TurnTrigger;

const TASK_NOTIFICATION_PREFIX: &str = "<task-notification";
const PEER_PREFIX: &str = "Another Claude session sent a message";
const PEER_TAG: &str = "<cross-session-message";
const LOCAL_COMMAND_PREFIX: &str = "<command-name>";
const COMPACTION_PREFIX: &str = "This session is being continued";
const SDK_SOURCE: &str = "sdk";

/// What a source says about a prompt besides its text. Each is `None` when the source does
/// not carry it: a hook payload has neither, a transcript record has what Claude Code wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptFacts<'a> {
    /// `origin.kind`: Claude Code's own statement of who started the turn.
    pub origin_kind: Option<&'a str>,
    /// `promptSource`: where the prompt came from; `sdk` for a headless caller.
    pub source: Option<&'a str>,
}

impl TurnTrigger {
    /// The trigger of the turn that `text` starts.
    ///
    /// `origin.kind` decides when present; otherwise the text's own leading marker does, after
    /// leading whitespace. Anything that matches no machine marker is a human prompt, because
    /// the pending rule keys on `Human` and a misread typed prompt would hide a waiting
    /// session. A typed skill invocation opens with `<command-message>`, which is a human
    /// prompt; a built-in slash command opens with `<command-name>`, which is not. A prompt from
    /// the SDK is never `Human`.
    pub fn of_prompt(text: &str, facts: PromptFacts<'_>) -> Self {
        let trigger = match facts.origin_kind {
            Some(kind) => Self::of_origin_kind(kind),
            None => Self::of_text(text),
        };
        match (&trigger, facts.source) {
            (Self::Human, Some(SDK_SOURCE)) => Self::Unknown(SDK_SOURCE.to_owned()),
            _ => trigger,
        }
    }

    /// The trigger Claude Code's `origin.kind` names.
    pub fn of_origin_kind(kind: &str) -> Self {
        match kind {
            "human" => Self::Human,
            "task-notification" => Self::TaskNotification,
            "peer" => Self::Peer,
            "coordinator" => Self::Coordinator,
            "auto-continuation" => Self::AutoContinuation,
            other => Self::Unknown(other.to_owned()),
        }
    }

    fn of_text(text: &str) -> Self {
        let body = text.trim_start();
        if body.starts_with(TASK_NOTIFICATION_PREFIX) {
            Self::TaskNotification
        } else if body.starts_with(PEER_PREFIX) || body.contains(PEER_TAG) {
            Self::Peer
        } else if body.starts_with(LOCAL_COMMAND_PREFIX) {
            Self::LocalCommand
        } else if body.starts_with(COMPACTION_PREFIX) {
            Self::CompactionSummary
        } else {
            Self::Human
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: PromptFacts<'static> = PromptFacts {
        origin_kind: None,
        source: None,
    };

    fn origin(kind: &str) -> PromptFacts<'_> {
        PromptFacts {
            origin_kind: Some(kind),
            source: None,
        }
    }

    fn sdk() -> PromptFacts<'static> {
        PromptFacts {
            origin_kind: None,
            source: Some("sdk"),
        }
    }

    /// The whole table of prompt shapes (observed in Claude Code 2.1.312; bodies are
    /// placeholders, only the markers are real) against the trigger each must get.
    /// Mutation: drop the `trim_start`, drop the sdk demotion, put `<command-message>` back
    /// among the local-command markers, or let the text win over `origin.kind`: each reddens
    /// a named row.
    #[test]
    #[allow(clippy::too_many_lines, reason = "the table is the test")]
    fn the_prompt_table() {
        let table: Vec<(&str, PromptFacts<'_>, TurnTrigger)> = vec![
            ("fix the failing test", NONE, TurnTrigger::Human),
            ("", NONE, TurnTrigger::Human),
            ("   \n", NONE, TurnTrigger::Human),
            (
                "<task-notification><task-id>t1</task-id></task-notification>",
                NONE,
                TurnTrigger::TaskNotification,
            ),
            (
                "\n  <task-notification>x</task-notification>",
                NONE,
                TurnTrigger::TaskNotification,
            ),
            (
                "Another Claude session sent a message: placeholder",
                NONE,
                TurnTrigger::Peer,
            ),
            (
                "Please read <cross-session-message from=\"x\">p</cross-session-message>",
                NONE,
                TurnTrigger::Peer,
            ),
            (
                "<command-name>/placeholder</command-name>",
                NONE,
                TurnTrigger::LocalCommand,
            ),
            (
                "  <command-name>/placeholder</command-name>",
                NONE,
                TurnTrigger::LocalCommand,
            ),
            (
                "<command-message>skill</command-message><command-name>/skill</command-name>",
                NONE,
                TurnTrigger::Human,
            ),
            (
                "This session is being continued from a previous conversation.",
                NONE,
                TurnTrigger::CompactionSummary,
            ),
            // A marker that is not at the start is the human quoting it, except the peer tag.
            (
                "why does <task-notification> show up?",
                NONE,
                TurnTrigger::Human,
            ),
            ("what is a <command-name> tag?", NONE, TurnTrigger::Human),
            // origin.kind decides when present, whatever the text says.
            ("fix it", origin("peer"), TurnTrigger::Peer),
            (
                "<task-notification>x</task-notification>",
                origin("human"),
                TurnTrigger::Human,
            ),
            (
                "fix it",
                origin("task-notification"),
                TurnTrigger::TaskNotification,
            ),
            ("fix it", origin("coordinator"), TurnTrigger::Coordinator),
            (
                "fix it",
                origin("auto-continuation"),
                TurnTrigger::AutoContinuation,
            ),
            (
                "fix it",
                origin("teleport"),
                TurnTrigger::Unknown("teleport".into()),
            ),
            // An SDK prompt is never Human, and does not demote the machine triggers.
            ("fix it", sdk(), TurnTrigger::Unknown("sdk".into())),
            (
                "fix it",
                PromptFacts {
                    origin_kind: Some("human"),
                    source: Some("sdk"),
                },
                TurnTrigger::Unknown("sdk".into()),
            ),
            (
                "<task-notification>x</task-notification>",
                sdk(),
                TurnTrigger::TaskNotification,
            ),
            (
                "fix it",
                PromptFacts {
                    origin_kind: None,
                    source: Some("keyboard"),
                },
                TurnTrigger::Human,
            ),
        ];
        for (text, facts, expected) in table {
            assert_eq!(
                TurnTrigger::of_prompt(text, facts),
                expected,
                "{text:?} {facts:?}"
            );
        }
    }
}
