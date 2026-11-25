//! One headless Claude Code run that calls one connector tool, and what that tool answered.
//!
//! The model is only the caller. What the tool returned is read from the run's event stream
//! (`--output-format stream-json`), never from the model's reply: with the tool missing, a
//! model writes a plausible answer of its own. The run is shut in: no built-in tool but tool
//! search, only the named tool allowed and the sending tools denied, hooks off (so `conn hook`
//! does not log it), and no transcript saved (so it never appears as a session).

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::util::runner::{CommandRunner, ToolError, run_tool};

const CLAUDE: &str = "claude";

/// One run reads a channel or adds a reaction: a tool search and a call, which answer in
/// seconds; two minutes covers a slow connector and a token refresh.
const TIMEOUT: Duration = Duration::from_secs(120);

/// The tools a run may never call, whatever the prompt or the text it reads says.
const DENIED: [&str; 3] = [
    "mcp__claude_ai_Slack__slack_send_message",
    "mcp__claude_ai_Slack__slack_send_message_draft",
    "mcp__claude_ai_Slack__slack_schedule_message",
];

/// Why a run's tool answer could not be had. `Tool`, `Unparseable` and `NotCalled` mean
/// nothing is known; `ToolFailed` and `RunFailed` are answers that say the work failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeadlessError {
    #[error(transparent)]
    Tool(ToolError),
    #[error("claude's event stream is not as expected: {detail}")]
    Unparseable { detail: String },
    /// The run ended without calling the tool, twice. `connector` is the tool's MCP server's
    /// status as the run's start reported it (`connected`, `needs-auth`, `pending`, …), or
    /// `None` when the server was not listed; `reply` is what the model said instead.
    #[error(
        "the run did not call {tool} (its connector: {}; the model said: {:?})",
        connector.as_deref().unwrap_or("not listed"),
        reply
    )]
    NotCalled {
        tool: String,
        connector: Option<String>,
        reply: String,
    },
    #[error("{tool} failed: {message}")]
    ToolFailed { tool: String, message: String },
    /// The tool answered, and the answer is not what it answers with: an over-size result
    /// replaced by Claude Code's note about it, for one.
    #[error("{tool} answered with something else than its result: {start:?}")]
    Malformed { tool: String, start: String },
    #[error("the run failed: {detail}")]
    RunFailed { detail: String },
}

/// How many runs one call may take. A run that does not call the tool is usually a connector
/// not ready at that moment, so it is run once more; a second miss is reported.
const ATTEMPTS: usize = 2;

/// Runs `prompt` with only `tool` allowed and returns the text of each answer `tool` gave, in
/// order. A run that did not call the tool is run again, once.
pub async fn call_tool(
    runner: &dyn CommandRunner,
    tool: &str,
    prompt: &str,
) -> Result<Vec<String>, HeadlessError> {
    let mut attempt = 1;
    loop {
        match call_once(runner, tool, prompt).await {
            Err(error @ HeadlessError::NotCalled { .. }) if attempt < ATTEMPTS => {
                tracing::warn!(%error, attempt, "a headless run did not call its tool; running it again");
                attempt += 1;
            }
            outcome => return outcome,
        }
    }
}

async fn call_once(
    runner: &dyn CommandRunner,
    tool: &str,
    prompt: &str,
) -> Result<Vec<String>, HeadlessError> {
    let mut args = vec![
        "-p",
        prompt,
        "--model",
        "haiku",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "ToolSearch",
        "--permission-mode",
        "default",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--allowedTools",
        tool,
        "--disallowedTools",
    ];
    args.extend(DENIED);
    let output = run_tool(runner, CLAUDE, &args, TIMEOUT)
        .await
        .map_err(HeadlessError::Tool)?;
    tool_answers(&output.stdout, tool)
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    message: Option<EventMessage>,
    is_error: Option<bool>,
    result: Option<String>,
    /// On the `system`/`init` event: each MCP server and its status.
    mcp_servers: Option<Vec<McpServer>>,
}

#[derive(Deserialize)]
struct McpServer {
    name: String,
    status: String,
}

/// Whether `server` is the MCP server `tool` belongs to: a tool is named
/// `mcp__<server name with every other character than a letter or digit as _>__<tool>`.
fn serves(server: &str, tool: &str) -> bool {
    let normalized: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    tool.strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(normalized.as_str()))
        .is_some_and(|rest| rest.starts_with("__"))
}

#[derive(Deserialize)]
struct EventMessage {
    content: Option<Vec<Block>>,
}

/// The most of the model's reply a [`HeadlessError::NotCalled`] keeps: enough to say why.
const REPLY_CHARS_MAX: usize = 300;

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    tool_use_id: Option<String>,
    content: Option<serde_json::Value>,
    is_error: Option<bool>,
}

/// The answers `tool` gave in a `stream-json` event stream: each `tool_result` whose call was
/// a `tool_use` of `tool`.
pub fn tool_answers(stream: &[u8], tool: &str) -> Result<Vec<String>, HeadlessError> {
    let text = std::str::from_utf8(stream).map_err(|e| HeadlessError::Unparseable {
        detail: e.to_string(),
    })?;
    let mut calls: HashSet<String> = HashSet::new();
    let mut answers = Vec::new();
    let mut finished = None;
    let mut connector = None;
    let mut reply: Vec<String> = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let event: Event = serde_json::from_str(line).map_err(|e| HeadlessError::Unparseable {
            detail: e.to_string(),
        })?;
        if let Some(servers) = &event.mcp_servers {
            connector = servers
                .iter()
                .find(|server| serves(&server.name, tool))
                .map(|server| server.status.clone());
        }
        let blocks = event
            .message
            .and_then(|message| message.content)
            .unwrap_or_default();
        match event.kind.as_str() {
            "assistant" => {
                reply.extend(
                    blocks
                        .iter()
                        .filter(|b| b.kind == "text")
                        .filter_map(|b| b.text.clone()),
                );
                calls.extend(
                    blocks
                        .into_iter()
                        .filter(|b| b.kind == "tool_use" && b.name.as_deref() == Some(tool))
                        .filter_map(|b| b.id),
                );
            }
            "user" => {
                for block in blocks.into_iter().filter(|b| b.kind == "tool_result") {
                    if !block
                        .tool_use_id
                        .as_ref()
                        .is_some_and(|id| calls.contains(id))
                    {
                        continue;
                    }
                    let content = block.content.as_ref().map_or_default(text_of);
                    if block.is_error == Some(true) {
                        return Err(HeadlessError::ToolFailed {
                            tool: tool.to_owned(),
                            message: content,
                        });
                    }
                    answers.push(content);
                }
            }
            "result" => finished = Some((event.is_error == Some(true), event.result)),
            _ => {}
        }
    }
    match finished {
        None => Err(HeadlessError::Unparseable {
            detail: "the stream has no result event".to_owned(),
        }),
        Some((true, detail)) => Err(HeadlessError::RunFailed {
            detail: detail.unwrap_or_default(),
        }),
        Some((false, _)) if answers.is_empty() => Err(HeadlessError::NotCalled {
            tool: tool.to_owned(),
            connector,
            reply: reply.join(" ").chars().take(REPLY_CHARS_MAX).collect(),
        }),
        Some((false, _)) => Ok(answers),
    }
}

/// A tool result's text: a string, or the text parts of a list of content blocks.
fn text_of(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Object(_)) => other.to_string(),
    }
}

/// What a run prints when it called `tool` once and the tool answered `text`: for the tests of
/// the modules that call tools.
#[cfg(test)]
pub(super) fn answered_run(tool: &str, text: &str) -> crate::util::runner::Output {
    let events = [
        serde_json::json!({"type": "system", "subtype": "init"}),
        serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "t1", "name": tool, "input": {}}]}}),
        serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": false,
             "content": [{"type": "text", "text": text}]}]}}),
        serde_json::json!({"type": "result", "is_error": false, "result": "done"}),
    ];
    let stdout: String = events
        .iter()
        .flat_map(|event| [event.to_string(), "\n".to_owned()])
        .collect();
    crate::util::runner::Output {
        exit: crate::util::runner::Exit::Success,
        stdout: stdout.into_bytes(),
        stderr: String::new(),
    }
}

#[cfg(test)]
mod tests {
    //! The stream parse against hand-written event streams in the shape `claude -p
    //! --output-format stream-json --verbose` printed in a real run (2.1.312): an init event,
    //! the assistant's `tool_use`, the `tool_result`, the reply, the result event.

    use serde_json::json;

    use super::*;

    const TOOL: &str = "mcp__claude_ai_Slack__slack_read_channel";

    fn stream(events: &[serde_json::Value]) -> Vec<u8> {
        let mut lines = String::new();
        for event in events {
            lines.push_str(&event.to_string());
            lines.push('\n');
        }
        lines.into_bytes()
    }

    fn call(id: &str, name: &str) -> serde_json::Value {
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": id, "name": name, "input": {}}]}})
    }

    fn answer(id: &str, text: &str, is_error: bool) -> serde_json::Value {
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error,
             "content": [{"type": "text", "text": text}]}]}})
    }

    fn result(is_error: bool) -> serde_json::Value {
        json!({"type": "result", "is_error": is_error, "result": "done"})
    }

    /// Mutation: take the reply instead of the tool result, or answers of any tool (the
    /// `ToolSearch` answer joins the list).
    #[test]
    fn the_answers_are_the_named_tools_results_only() {
        let events = stream(&[
            json!({"type": "system", "subtype": "init"}),
            call("t0", "ToolSearch"),
            answer("t0", "found it", false),
            call("t1", TOOL),
            answer("t1", "the messages", false),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "invented"}]}}),
            result(false),
        ]);
        assert_eq!(
            tool_answers(&events, TOOL),
            Ok(vec!["the messages".to_owned()])
        );
    }

    /// A model that never called the tool and wrote its own answer is `NotCalled`, not an
    /// empty channel, and says what the run's start reported for the tool's connector and what
    /// the model wrote. Mutation: return `Ok(vec![])` (the run reads as an empty channel),
    /// match the connector by a prefix of the tool name (the `claude.ai Slac` server
    /// is taken), or drop the model's text.
    #[test]
    fn a_run_without_the_call_is_not_called_with_its_connector_and_reply() {
        let events = stream(&[
            json!({"type": "system", "subtype": "init", "mcp_servers": [
                {"name": "claude.ai Slac", "status": "connected"},
                {"name": "claude.ai Slack", "status": "needs-auth"},
                {"name": "claude.ai Gmail", "status": "connected"}]}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "I can't find that tool."}]}}),
            result(false),
        ]);
        assert_eq!(
            tool_answers(&events, TOOL),
            Err(HeadlessError::NotCalled {
                tool: TOOL.to_owned(),
                connector: Some("needs-auth".to_owned()),
                reply: "I can't find that tool.".to_owned(),
            })
        );
    }

    fn printed(events: &[serde_json::Value]) -> crate::util::runner::Output {
        crate::util::runner::Output {
            exit: crate::util::runner::Exit::Success,
            stdout: stream(events),
            stderr: String::new(),
        }
    }

    /// A run that did not call the tool is run once more, and a second miss is reported.
    /// Mutation: never run again (the first case fails), or run again on any error (the
    /// failed tool takes two runs).
    #[tokio::test]
    async fn a_missed_call_is_run_once_more() {
        let missed = || Ok(printed(&[result(false)]));
        let called = Ok(printed(&[
            call("t1", TOOL),
            answer("t1", "messages", false),
            result(false),
        ]));
        let runner = crate::util::runner::FakeRunner::new([missed(), called]);
        assert_eq!(
            call_tool(&runner, TOOL, "read").await,
            Ok(vec!["messages".to_owned()])
        );
        assert_eq!(runner.lines().len(), 2);

        let twice = crate::util::runner::FakeRunner::new([missed(), missed()]);
        assert!(matches!(
            call_tool(&twice, TOOL, "read").await,
            Err(HeadlessError::NotCalled { .. })
        ));
        assert_eq!(twice.lines().len(), 2);

        let failed = Ok(printed(&[
            call("t1", TOOL),
            answer("t1", "nope", true),
            result(false),
        ]));
        let once = crate::util::runner::FakeRunner::new([failed]);
        assert!(matches!(
            call_tool(&once, TOOL, "read").await,
            Err(HeadlessError::ToolFailed { .. })
        ));
        assert_eq!(once.lines().len(), 1);
    }

    #[test]
    fn a_failed_tool_or_run_and_a_cut_stream_are_each_their_own_error() {
        let failed = stream(&[
            call("t1", TOOL),
            answer("t1", "channel_not_found", true),
            result(false),
        ]);
        assert!(matches!(
            tool_answers(&failed, TOOL),
            Err(HeadlessError::ToolFailed { message, .. }) if message == "channel_not_found"
        ));
        let run = stream(&[result(true)]);
        assert!(matches!(
            tool_answers(&run, TOOL),
            Err(HeadlessError::RunFailed { .. })
        ));
        let cut = stream(&[call("t1", TOOL), answer("t1", "x", false)]);
        assert!(matches!(
            tool_answers(&cut, TOOL),
            Err(HeadlessError::Unparseable { .. })
        ));
    }
}
