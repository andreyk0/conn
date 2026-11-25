//! The `settings.json` fragment that registers this binary as a Claude Code hook.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::{EventsError, HookEventName};
use crate::ledger::guard::JIRA_WRITES;
use crate::util::path::utf8;
use crate::util::shell::quote;

/// Seconds Claude Code waits for a hook. The hook appends a line and the guard reads a small
/// file, so a slow disk is the only way to need more, and a late hook must not hold the agent
/// up.
const TIMEOUT_SECS: u8 = 5;

/// The hook handler kind Claude Code runs as a subprocess.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum HandlerKind {
    Command,
}

/// One hook command. There is deliberately no `onFailure` field: a logging hook must never
/// block the agent.
#[derive(Debug, Serialize)]
struct Handler {
    #[serde(rename = "type")]
    kind: HandlerKind,
    command: String,
    timeout: u8,
}

#[derive(Debug, Serialize)]
struct MatcherGroup {
    /// The tool names the group runs for, as a regular expression; every event when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<String>,
    hooks: Vec<Handler>,
}

/// The object to merge into `~/.claude/settings.json`.
#[derive(Debug, Serialize)]
pub struct SettingsPatch {
    hooks: BTreeMap<&'static str, Vec<MatcherGroup>>,
}

/// The event `conn guard` answers; it is not logged, so it is not a [`HookEventName`].
const PRE_TOOL_USE: &str = "PreToolUse";

impl SettingsPatch {
    /// Registers `exe hook` for every event in [`HookEventName::ALL`], and `exe guard` before
    /// the Jira tools that change a ticket.
    pub fn registering(exe: &Path) -> Result<Self, EventsError> {
        let exe = quote(utf8(exe)?);
        let group = |matcher: Option<String>, subcommand: &str| MatcherGroup {
            matcher,
            hooks: vec![Handler {
                kind: HandlerKind::Command,
                command: format!("{exe} {subcommand}"),
                timeout: TIMEOUT_SECS,
            }],
        };
        let mut hooks: BTreeMap<_, _> = HookEventName::ALL
            .into_iter()
            .map(|event| (event.as_str(), vec![group(None, "hook")]))
            .collect();
        let jira_writes = format!("^mcp__.*__({})$", JIRA_WRITES.join("|"));
        hooks.insert(PRE_TOOL_USE, vec![group(Some(jira_writes), "guard")]);
        Ok(Self { hooks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(command: &str) -> serde_json::Value {
        json!([{ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }])
    }

    /// Mutation: drop the matcher (the guard would run before every tool), or misspell a tool.
    #[test]
    fn registers_the_three_events_and_the_guard_without_on_failure() {
        let patch = SettingsPatch::registering(Path::new("/opt/conn/bin/conn")).unwrap();
        assert_eq!(
            serde_json::to_value(patch).unwrap(),
            json!({ "hooks": {
                "SessionStart": entry("/opt/conn/bin/conn hook"),
                "UserPromptSubmit": entry("/opt/conn/bin/conn hook"),
                "Stop": entry("/opt/conn/bin/conn hook"),
                "PreToolUse": [{
                    "matcher": "^mcp__.*__(addCommentToJiraIssue|transitionJiraIssue|editJiraIssue|addWorklogToJiraIssue)$",
                    "hooks": [{ "type": "command", "command": "/opt/conn/bin/conn guard", "timeout": 5 }],
                }],
            }})
        );
    }

    #[test]
    fn quotes_an_executable_path_with_spaces() {
        let patch = SettingsPatch::registering(Path::new("/opt/my tools/conn")).unwrap();
        let value = serde_json::to_value(patch).unwrap();
        assert_eq!(value["hooks"]["Stop"], entry("'/opt/my tools/conn' hook"));
    }
}
