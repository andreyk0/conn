//! One line of JSON into a [`Record`], leniently.
//!
//! Required fields are the ones a derivation cannot do without (a timestamp, a tool-use id);
//! everything else is optional and an unknown value is carried, never refused. A refusal is a
//! [`LineError`] that the caller counts.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::types::{
    AskUserQuestion, Instant, MultiSelect, NoQuestions, PromptFacts, Question, QuestionOption,
    SafeUrl, SessionId, TurnTrigger,
};

use super::lines::LINE_BYTES_MAX;
use super::record::{
    Answers, AssistantRecord, Attachment, AttachmentRecord, Block, Chain, Envelope, Meta, PrLink,
    Prompt, Record, StopReason, SystemEvent, SystemRecord, ToolCall, ToolOutcome, ToolResult,
    ToolResultContent, ToolUse, UserContent, UserRecord,
};
use crate::types::outward::Outward;
use crate::util::redact::Redacted;

/// A line that yielded no [`Record`]. Counted by the summary, never fatal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("line is not valid UTF-8")]
    NotUtf8,
    #[error("line is longer than the {} byte limit", LINE_BYTES_MAX)]
    TooLong,
    /// Not JSON, or JSON of the wrong shape. Only the error's class and column are kept, so
    /// transcript text never reaches a log through this.
    #[error("not the JSON a transcript line is: {0}")]
    Json(String),
    #[error("{kind} record: `{field}` is {problem}")]
    Field {
        kind: &'static str,
        field: &'static str,
        problem: FieldProblem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FieldProblem {
    #[error("missing")]
    Missing,
    #[error("malformed")]
    Malformed,
    /// A URL that may not become an `href`; the URL itself is not repeated, because it is
    /// transcript text.
    #[error("not a safe https URL")]
    UnsafeUrl,
}

/// The tag every line is routed on.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Every key of the four envelope record kinds that this reader looks at.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    timestamp: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    is_sidechain: Option<bool>,
    entrypoint: Option<String>,
    message: Option<RawMessage>,
    origin: Option<RawOrigin>,
    tool_use_result: Option<Value>,
    is_meta: Option<bool>,
    is_compact_summary: Option<bool>,
    prompt_source: Option<String>,
    turn_origin: Option<String>,
    subtype: Option<String>,
    duration_ms: Option<u64>,
    attachment: Option<RawAttachment>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    stop_reason: Option<String>,
    content: Option<Content>,
}

/// `message.content` is a string or an array of blocks. Blocks stay JSON values: their set
/// is open and a block this code does not read must not fail the line.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

#[derive(Deserialize)]
struct RawOrigin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawAttachment {
    #[serde(rename = "type")]
    kind: Option<String>,
    prompt: Option<Value>,
    origin: Option<RawOrigin>,
    #[serde(rename = "commandMode")]
    command_mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMeta {
    ai_title: Option<String>,
    custom_title: Option<String>,
    last_prompt: Option<String>,
    pr_number: Option<u64>,
    pr_url: Option<String>,
    pr_repository: Option<String>,
    permission_mode: Option<String>,
}

pub fn parse_line(line: &str) -> Result<Record, LineError> {
    let head: Head = serde_json::from_str(line)?;
    let kind = head.kind.ok_or_else(|| missing("record", "type"))?;
    match kind.as_str() {
        "user" => parse_user(line),
        "assistant" => parse_assistant(line),
        "system" => parse_system(line),
        "attachment" => parse_attachment(line),
        "ai-title" => parse_meta(line, MetaKind::AiTitle),
        "custom-title" => parse_meta(line, MetaKind::CustomTitle),
        "last-prompt" => parse_meta(line, MetaKind::LastPrompt),
        "pr-link" => parse_meta(line, MetaKind::PrLink),
        "permission-mode" => parse_meta(line, MetaKind::PermissionMode),
        _ => Ok(Record::Unknown { kind }),
    }
}

impl From<serde_json::Error> for LineError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(format!(
            "{:?} at column {}",
            error.classify(),
            error.column()
        ))
    }
}

fn missing(kind: &'static str, field: &'static str) -> LineError {
    LineError::Field {
        kind,
        field,
        problem: FieldProblem::Missing,
    }
}

fn malformed(kind: &'static str, field: &'static str) -> LineError {
    LineError::Field {
        kind,
        field,
        problem: FieldProblem::Malformed,
    }
}

fn raw(line: &str) -> Result<Raw, LineError> {
    Ok(serde_json::from_str(line)?)
}

fn envelope(kind: &'static str, raw: &Raw) -> Result<Envelope, LineError> {
    let at = raw
        .timestamp
        .as_deref()
        .ok_or_else(|| missing(kind, "timestamp"))?
        .parse::<Instant>()
        .map_err(|_| malformed(kind, "timestamp"))?;
    let session_id = raw
        .session_id
        .as_deref()
        .map(SessionId::parse)
        .transpose()
        .map_err(|_| malformed(kind, "sessionId"))?;
    let chain = if raw.is_sidechain.unwrap_or(false) {
        Chain::Sidechain
    } else {
        Chain::Main
    };
    Ok(Envelope {
        at,
        chain,
        session_id,
        cwd: raw.cwd.as_deref().map(PathBuf::from),
        git_branch: raw.git_branch.clone(),
        entrypoint: raw.entrypoint.clone(),
    })
}

fn message<'a>(kind: &'static str, raw: &'a Raw) -> Result<&'a RawMessage, LineError> {
    raw.message.as_ref().ok_or_else(|| missing(kind, "message"))
}

// --- user ------------------------------------------------------------------------------------

fn parse_user(line: &str) -> Result<Record, LineError> {
    let raw = raw(line)?;
    let env = envelope("user", &raw)?;
    let content = message("user", &raw)?
        .content
        .as_ref()
        .ok_or_else(|| missing("user", "message.content"))?;
    Ok(Record::User(UserRecord {
        env,
        content: classify_user(&raw, content)?,
    }))
}

fn classify_user(raw: &Raw, content: &Content) -> Result<UserContent, LineError> {
    if let Content::Blocks(blocks) = content {
        let results: Vec<&Value> = blocks
            .iter()
            .filter(|block| block_kind(block) == Some("tool_result"))
            .collect();
        if !results.is_empty() {
            // `toolUseResult` describes the record's one result; with several it cannot be
            // attributed to any of them.
            let details = match results.as_slice() {
                [_] => raw.tool_use_result.as_ref(),
                _ => None,
            };
            return results
                .into_iter()
                .map(|block| tool_result(block, details))
                .collect::<Result<Vec<_>, _>>()
                .map(UserContent::ToolResults);
        }
    }
    let text = content_text(content);
    Ok(match prompt_trigger(raw, &text) {
        Some(trigger) => UserContent::Prompt(Prompt { trigger, text }),
        None => UserContent::Injected,
    })
}

/// What started the turn this user text opens, or `None` when the text starts nothing.
///
/// The shared classifier ([`TurnTrigger::of_prompt`]) decides what a prompt is. The records
/// Claude Code writes without `origin` add what only a transcript has: compaction summaries,
/// injected context that starts no turn, and scheduled wake-ups.
fn prompt_trigger(raw: &Raw, text: &str) -> Option<TurnTrigger> {
    let origin_kind = raw
        .origin
        .as_ref()
        .and_then(|origin| origin.kind.as_deref());
    let sdk = [raw.prompt_source.as_deref(), raw.turn_origin.as_deref()].contains(&Some("sdk"));
    let source = if sdk {
        Some("sdk")
    } else {
        raw.prompt_source.as_deref()
    };
    let facts = PromptFacts {
        origin_kind,
        source,
    };
    if origin_kind.is_some() {
        return Some(TurnTrigger::of_prompt(text, facts));
    }
    if raw.is_compact_summary.unwrap_or(false) {
        return Some(TurnTrigger::CompactionSummary);
    }
    let body = text.trim_start();
    let local_command = TurnTrigger::of_prompt(text, facts) == TurnTrigger::LocalCommand;
    let injected = raw.is_meta.unwrap_or(false)
        || body.is_empty()
        || body.starts_with("<local-command-")
        || body.starts_with("[Request interrupted");
    if injected && !local_command {
        return None;
    }
    if raw.turn_origin.as_deref() == Some("scheduled") {
        return Some(TurnTrigger::Scheduled);
    }
    Some(TurnTrigger::of_prompt(text, facts))
}

fn block_kind(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// The text of a content value: a string as is, an array as its `text` blocks joined by
/// newlines, anything else as empty.
fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks_text(blocks),
    }
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block_kind(block) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks_text(blocks),
        Some(_) | None => String::new(),
    }
}

fn tool_result(block: &Value, details: Option<&Value>) -> Result<ToolResult, LineError> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("user", "tool_use_id"))?
        .to_owned();
    let outcome = match block.get("is_error").and_then(Value::as_bool) {
        Some(true) => ToolOutcome::Error,
        Some(false) | None => ToolOutcome::Ok,
    };
    let persisted = details
        .and_then(|d| d.get("persistedOutputPath"))
        .and_then(Value::as_str);
    let content = match persisted {
        Some(path) => ToolResultContent::Persisted {
            path: PathBuf::from(path),
            size: details
                .and_then(|d| d.get("persistedOutputSize"))
                .and_then(Value::as_u64),
        },
        None => ToolResultContent::Inline(value_text(block.get("content"))),
    };
    let answers = details
        .and_then(|d| d.get("answers"))
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(question, answer)| {
                    let answer = match answer {
                        Value::String(text) => text.clone(),
                        Value::Null
                        | Value::Bool(_)
                        | Value::Number(_)
                        | Value::Array(_)
                        | Value::Object(_) => answer.to_string(),
                    };
                    (question.clone(), answer)
                })
                .collect::<Answers>()
        });
    Ok(ToolResult {
        tool_use_id,
        outcome,
        content,
        answers,
    })
}

// --- assistant -------------------------------------------------------------------------------

fn parse_assistant(line: &str) -> Result<Record, LineError> {
    let raw = raw(line)?;
    let env = envelope("assistant", &raw)?;
    let message = message("assistant", &raw)?;
    let blocks = match message.content.as_ref() {
        Some(Content::Blocks(blocks)) => blocks.iter().map(block).collect::<Result<Vec<_>, _>>()?,
        Some(Content::Text(text)) => vec![Block::Text(text.clone())],
        None => return Err(missing("assistant", "message.content")),
    };
    Ok(Record::Assistant(AssistantRecord {
        env,
        message_id: message.id.clone(),
        stop_reason: message.stop_reason.as_deref().map(stop_reason),
        blocks,
    }))
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn block(value: &Value) -> Result<Block, LineError> {
    let kind = block_kind(value).ok_or_else(|| missing("assistant", "content.type"))?;
    match kind {
        "text" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Block::Text(text.to_owned()))
            .ok_or_else(|| missing("assistant", "content.text")),
        "thinking" => Ok(Block::Thinking),
        "tool_use" => tool_use(value).map(Block::ToolUse),
        other => Ok(Block::Other {
            kind: other.to_owned(),
        }),
    }
}

fn tool_use(value: &Value) -> Result<ToolUse, LineError> {
    let text = |key: &'static str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| missing("assistant", key))
    };
    let id = text("id")?.to_owned();
    let name = text("name")?;
    let call = if name == "AskUserQuestion" {
        ToolCall::AskUserQuestion(ask_user_question(value.get("input"))?)
    } else if let Some(write) = Outward::of_call(name, value.get("input")) {
        ToolCall::Outward(write)
    } else {
        ToolCall::Other {
            name: name.to_owned(),
        }
    };
    Ok(ToolUse { id, call })
}

fn ask_user_question(input: Option<&Value>) -> Result<AskUserQuestion, LineError> {
    let bad = || malformed("assistant", "input.questions");
    input
        .and_then(|input| input.get("questions"))
        .and_then(Value::as_array)
        .ok_or_else(bad)?
        .iter()
        .map(|question| self::question(question).ok_or_else(bad))
        .collect::<Result<Vec<_>, _>>()
        .and_then(|questions| AskUserQuestion::new(questions).map_err(|NoQuestions| bad()))
}

fn question(value: &Value) -> Option<Question> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(Redacted::<String>::of)
    };
    let options = value
        .get("options")
        .and_then(Value::as_array)
        .map_or_default(Vec::as_slice)
        .iter()
        .map(|option| {
            Some(QuestionOption {
                label: text(option, "label")?,
                description: text(option, "description"),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let multi_select = match value.get("multiSelect").and_then(Value::as_bool) {
        Some(true) => MultiSelect::Many,
        Some(false) | None => MultiSelect::One,
    };
    Some(Question {
        header: text(value, "header"),
        question: text(value, "question")?,
        multi_select,
        options,
    })
}

// --- system, attachment, metadata ------------------------------------------------------------

fn parse_system(line: &str) -> Result<Record, LineError> {
    let raw = raw(line)?;
    let env = envelope("system", &raw)?;
    let subtype = raw
        .subtype
        .as_deref()
        .ok_or_else(|| missing("system", "subtype"))?;
    let event = match subtype {
        "turn_duration" => SystemEvent::TurnDuration {
            duration_ms: raw.duration_ms,
        },
        other => SystemEvent::Other {
            subtype: other.to_owned(),
        },
    };
    Ok(Record::System(SystemRecord { env, event }))
}

fn parse_attachment(line: &str) -> Result<Record, LineError> {
    let raw = raw(line)?;
    let env = envelope("attachment", &raw)?;
    let body = raw
        .attachment
        .as_ref()
        .ok_or_else(|| missing("attachment", "attachment"))?;
    let kind = body
        .kind
        .as_deref()
        .ok_or_else(|| missing("attachment", "attachment.type"))?;
    let attachment = match kind {
        "queued_command" => Attachment::QueuedCommand(queued_prompt(body)?),
        other => Attachment::Other {
            kind: other.to_owned(),
        },
    };
    Ok(Record::Attachment(AttachmentRecord { env, attachment }))
}

/// A queued prompt carries `origin` like a user record, except that task notifications are
/// often queued with only `commandMode` to say what they are.
fn queued_prompt(body: &RawAttachment) -> Result<Prompt, LineError> {
    let text = match body.prompt.as_ref() {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks_text(blocks),
        Some(_) | None => return Err(malformed("attachment", "attachment.prompt")),
    };
    let trigger = match (
        body.origin
            .as_ref()
            .and_then(|origin| origin.kind.as_deref()),
        body.command_mode.as_deref(),
    ) {
        (Some(kind), _) => TurnTrigger::of_origin_kind(kind),
        (None, Some("prompt")) => TurnTrigger::Human,
        (None, Some("task-notification")) => TurnTrigger::TaskNotification,
        (None, Some(mode)) => TurnTrigger::Unknown(mode.to_owned()),
        (None, None) => TurnTrigger::Unknown("queued_command".to_owned()),
    };
    Ok(Prompt { trigger, text })
}

/// The metadata record kinds this reader keeps, named by their `type` tag.
#[derive(Clone, Copy)]
enum MetaKind {
    AiTitle,
    CustomTitle,
    LastPrompt,
    PermissionMode,
    PrLink,
}

fn parse_meta(line: &str, kind: MetaKind) -> Result<Record, LineError> {
    let raw: RawMeta = serde_json::from_str(line)?;
    let meta = match kind {
        MetaKind::AiTitle => {
            Meta::AiTitle(raw.ai_title.ok_or_else(|| missing("ai-title", "aiTitle"))?)
        }
        MetaKind::CustomTitle => Meta::CustomTitle(
            raw.custom_title
                .ok_or_else(|| missing("custom-title", "customTitle"))?,
        ),
        MetaKind::LastPrompt => Meta::LastPrompt(raw.last_prompt),
        MetaKind::PermissionMode => Meta::PermissionMode(
            raw.permission_mode
                .ok_or_else(|| missing("permission-mode", "permissionMode"))?,
        ),
        MetaKind::PrLink => {
            let kind = "pr-link";
            let number = raw.pr_number.ok_or_else(|| missing(kind, "prNumber"))?;
            Meta::PrLink(PrLink {
                number: u32::try_from(number).map_err(|_| malformed(kind, "prNumber"))?,
                url: SafeUrl::parse(&raw.pr_url.ok_or_else(|| missing(kind, "prUrl"))?).map_err(
                    |_| LineError::Field {
                        kind,
                        field: "prUrl",
                        problem: FieldProblem::UnsafeUrl,
                    },
                )?,
                repo: raw
                    .pr_repository
                    .ok_or_else(|| missing(kind, "prRepository"))?,
            })
        }
    };
    Ok(Record::Meta(meta))
}
