//! One transcript line, as a sum type.
//!
//! The shapes keep what the tool derives facts from and drop the rest (usage counters, request
//! ids, hook output). Every open set is carried verbatim in an `Unknown`/`Other` variant.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::types::{AskUserQuestion, Instant, SafeUrl, SessionId, TurnTrigger};

/// A transcript line that parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    User(UserRecord),
    Assistant(AssistantRecord),
    System(SystemRecord),
    Attachment(AttachmentRecord),
    /// A record without a uuid that Claude Code re-appends many times.
    Meta(Meta),
    /// A record kind this code does not read (`file-history-delta`, `api-request`, ...).
    Unknown {
        kind: String,
    },
}

/// Whether a record belongs to the conversation the human sees or to a subagent's.
///
/// Turn logic reads the main chain only; a subagent's turns are not the session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Main,
    Sidechain,
}

/// What every `user`, `assistant`, `system` and `attachment` record carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub at: Instant,
    pub chain: Chain,
    pub session_id: Option<SessionId>,
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    /// `cli`, `sdk-cli`, ...: how the session was started. Open set.
    pub entrypoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub env: Envelope,
    pub content: UserContent,
}

/// What a `user` record is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserContent {
    /// Text that starts (or restarts) a turn.
    Prompt(Prompt),
    /// Tool results handed back to the model: the turn continues, nothing new starts.
    ToolResults(Vec<ToolResult>),
    /// Text the model reads that starts nothing: skill bodies, interrupt markers, the output
    /// of a local command.
    Injected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub trigger: TurnTrigger,
    pub text: String,
}

/// One `tool_result` content block of a carrier record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub outcome: ToolOutcome,
    pub content: ToolResultContent,
    /// The `AskUserQuestion` answers, keyed by question text; `None` for every other tool.
    pub answers: Option<Answers>,
}

pub type Answers = BTreeMap<String, String>;

/// `is_error` is absent on many successes, so absent means [`ToolOutcome::Ok`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultContent {
    Inline(String),
    /// A large result Claude Code replaced with a `<persisted-output>` stub; the full text is
    /// in the file at `path`. `size` is absent when the record does not say.
    Persisted {
        path: PathBuf,
        size: Option<u64>,
    },
}

/// One content block of an assistant message, which is one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantRecord {
    pub env: Envelope,
    /// Blocks of one API message share this id.
    pub message_id: Option<String>,
    /// `None` while the message is still streaming (`stop_reason: null`) or when absent.
    pub stop_reason: Option<StopReason>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    StopSequence,
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Thinking,
    ToolUse(ToolUse),
    Other { kind: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    pub id: String,
    pub call: ToolCall,
}

/// The tools the transcript reader looks into; every other call is only named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCall {
    AskUserQuestion(AskUserQuestion),
    /// A Jira or Slack write: what other people will read.
    Outward(crate::types::outward::Outward),
    Other {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemRecord {
    pub env: Envelope,
    pub event: SystemEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemEvent {
    /// The end of an assistant turn in an interactive session.
    TurnDuration {
        duration_ms: Option<u64>,
    },
    Other {
        subtype: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub env: Envelope,
    pub attachment: Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// A prompt entered while a turn was running.
    QueuedCommand(Prompt),
    Other {
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    AiTitle(String),
    /// The session's name, set by `-n`, `/rename` or a hook's `sessionTitle`.
    CustomTitle(String),
    /// `lastPrompt` is absent on some records.
    LastPrompt(Option<String>),
    PrLink(PrLink),
    PermissionMode(String),
}

/// A pull request the session announced. Its URL is already safe to use as an `href`: a
/// record whose URL is not is a [`LineError`](super::LineError), not a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLink {
    pub number: u32,
    pub url: SafeUrl,
    /// `org/repo`.
    pub repo: String,
}
