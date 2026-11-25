//! One finished assistant turn and what it asked of the human.

use super::Instant;
use crate::util::redact::Redacted;

/// What started a turn, as far as the transcript says.
///
/// The set is open: Claude Code's record format is internal and grows, so a value this
/// code does not know is carried as [`TurnTrigger::Unknown`], never rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnTrigger {
    /// The human typed a prompt.
    Human,
    /// A background task reported completion.
    TaskNotification,
    /// Another session or agent sent a message.
    Peer,
    /// A scheduled wake-up or cron-style prompt.
    Scheduled,
    /// A coordinator agent dispatched the work.
    Coordinator,
    /// The agent continued itself without a new prompt.
    AutoContinuation,
    /// Output of a local slash command.
    LocalCommand,
    /// The synthetic prompt that carries a compaction summary.
    CompactionSummary,
    /// A trigger this code does not recognise, verbatim.
    Unknown(String),
}

/// Assistant prose with the credentials of known shapes masked.
///
/// Redacted where it is read from the transcript or the hook, so nothing downstream holds the
/// unmasked text. Never HTML-escaped here: escaping happens once, at render, by `maud`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantText(Redacted<String>);

impl AssistantText {
    pub fn of(text: &str) -> Self {
        Self(Redacted::of(text))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub const fn redacted(&self) -> &Redacted<String> {
        &self.0
    }
}

/// Background tasks still running when a turn ended.
///
/// The Stop hook payload may list them; the transcript does not, so "none are running"
/// and "the source cannot say" are different values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTasks {
    /// The source carries no such information.
    Unknown,
    /// The source listed these tasks; empty means none are running.
    Known(Vec<BackgroundTask>),
}

/// One task Claude Code was still tracking when the turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    pub status: TaskStatus,
    pub description: String,
    pub kind: TaskKind,
}

/// Where a background task stands. Open: Claude Code adds states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    /// A status this code does not recognise, verbatim.
    Other(String),
}

/// What kind of task it is, carrying the one field that kind has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    Shell {
        command: String,
    },
    Subagent {
        agent_type: String,
    },
    /// A task type this code does not model (monitor, workflow, ...), by its label.
    Other(String),
}

/// The end of one assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEnd {
    pub at: Instant,
    pub trigger: TurnTrigger,
    pub text: AssistantText,
    pub background_tasks: BackgroundTasks,
    /// The structured question the turn ended on, if it ended by asking one.
    pub asked: Option<AskUserQuestion>,
}

/// A structured question (the `AskUserQuestion` tool) the agent is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserQuestion {
    questions: Vec<Question>,
}

/// The tool call carried no question, so there is nothing to wait on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the question list is empty")]
pub struct NoQuestions;

impl AskUserQuestion {
    /// At least one question: a call without any cannot be answered, so it is not a question.
    pub fn new(questions: Vec<Question>) -> Result<Self, NoQuestions> {
        if questions.is_empty() {
            Err(NoQuestions)
        } else {
            Ok(Self { questions })
        }
    }

    pub fn questions(&self) -> &[Question] {
        &self.questions
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub header: Option<Redacted<String>>,
    pub question: Redacted<String>,
    pub multi_select: MultiSelect,
    pub options: Vec<QuestionOption>,
}

/// Whether a question accepts one answer or several.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiSelect {
    One,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: Redacted<String>,
    pub description: Option<Redacted<String>>,
}
