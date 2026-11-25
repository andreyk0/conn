//! `conn guard`: the `PreToolUse` hook that refuses a Jira write to a ticket another running
//! session owns.
//!
//! Registered only for the Jira tools that change a ticket (see
//! [`crate::events::settings`]). The refusal goes back to the agent as the hook's
//! `permissionDecisionReason`, naming the owner, so the agent sends its news there instead.
//! Anything the guard cannot read or decide lets the call through: the guard stops a known
//! collision, and must never become the reason a session cannot work.

use std::io::{self, Read, Write};

use jiff::tz::TimeZone;
use serde_json::{Value, json};

use super::command::{Life, Refusal, Sessions, local_time};
use super::{LEDGER_FILE, Ledger, Subject, store};
use crate::config::Config;
use crate::types::SessionId;

/// The Jira tools that change a ticket, by the last part of their name; the server's part of
/// the name differs between connections (`mcp__<server>__<tool>`).
pub const JIRA_WRITES: [&str; 4] = [
    "addCommentToJiraIssue",
    "transitionJiraIssue",
    "editJiraIssue",
    "addWorklogToJiraIssue",
];

/// The largest payload read. A tool call's input is a comment at most, kilobytes.
const PAYLOAD_BYTES_MAX: u64 = 4 * 1024 * 1024;

/// What the guard answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Say nothing: Claude Code goes on as it would have.
    Allow,
    Deny(String),
}

/// Runs `conn guard`. Returns normally whatever happens.
pub fn run() {
    let mut bytes = Vec::new();
    if let Err(error) = io::stdin()
        .lock()
        .take(PAYLOAD_BYTES_MAX)
        .read_to_end(&mut bytes)
    {
        report(format_args!("cannot read the payload: {error}"));
        return;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        report(format_args!("the payload is not JSON; allowed"));
        return;
    };
    let Some(ticket) = written_ticket(&payload) else {
        return;
    };
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            report(format_args!("config unusable ({error}); allowed"));
            return;
        }
    };
    let loaded = match store::read(&config.paths.state_dir.join(LEDGER_FILE)) {
        Ok(loaded) => loaded,
        Err(error) => {
            report(format_args!("{error}; allowed"));
            return;
        }
    };
    let me = payload
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| SessionId::parse(id).ok());
    let sessions = Sessions::from_registry(&config.paths.claude_dir);
    let zone = TimeZone::system();
    let verdict = decide(&loaded.ledger, &ticket, me.as_ref(), &sessions, &zone);
    if let Verdict::Deny(reason) = verdict {
        let answer = json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }});
        let _ = writeln!(io::stdout(), "{answer}");
    }
}

/// The ticket a Jira write names, when the payload is one.
pub fn written_ticket(payload: &Value) -> Option<Subject> {
    let tool = payload.get("tool_name")?.as_str()?;
    let (_, last) = tool.rsplit_once("__")?;
    if !JIRA_WRITES.contains(&last) {
        return None;
    }
    let key = payload.get("tool_input")?.get("issueIdOrKey")?.as_str()?;
    Subject::parse(key.trim())
        .ok()
        .filter(|s| s.ticket().is_some())
}

/// Refuses the write when another session that may be running owns the ticket.
pub fn decide(
    ledger: &Ledger,
    ticket: &Subject,
    me: Option<&SessionId>,
    sessions: &Sessions,
    zone: &TimeZone,
) -> Verdict {
    let Some(held) = ledger.owner(ticket) else {
        return Verdict::Allow;
    };
    if Some(&held.session) == me || sessions.life(&held.session) == Life::Ended {
        return Verdict::Allow;
    }
    Verdict::Deny(
        Refusal::Owned {
            subject: ticket.clone(),
            owner: sessions.describe(&held.session),
            since: local_time(held.since, zone),
        }
        .to_string(),
    )
}

/// One line on stderr. A failure to write it has nowhere further to go.
fn report(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr(), "conn guard: {message}");
}
