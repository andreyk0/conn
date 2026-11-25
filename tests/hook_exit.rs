//! The built binary, run as the agent runs it: `conn hook` and `conn guard` never fail a turn.
//!
//! Arguments a hook does not take are one line on stderr, nothing on stdout (Claude Code reads
//! stdout as the hook's answer) and exit 0; the other subcommands keep the parser's exit 2. For
//! the guard, a `PreToolUse` hook, exit 2 would block the tool call.

use std::process::{Command, Output};

fn conn(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_conn")).args(args).output()
}

/// Mutation: parse with `Cli::parse` in `cli::run`: the first row exits 2.
#[test]
fn a_hook_with_bad_arguments_exits_zero_with_one_line_on_stderr() {
    for args in [
        &["hook", "--nonsense"][..],
        &["hook", "extra", "arguments"],
        &["guard", "--nonsense"],
    ] {
        let output = conn(args).unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1, "{args:?}: {stderr:?}");
        let prefix = format!("conn {}: ", args.first().unwrap_or(&""));
        assert!(stderr.starts_with(&prefix), "{stderr:?}");
    }
}

/// The guard lets through what it cannot read: a payload that is not JSON, and a tool it does
/// not guard, answer nothing on stdout and exit 0.
#[test]
fn the_guard_says_nothing_about_what_it_does_not_guard() {
    use std::io::Write;
    for payload in [
        "not json",
        r#"{"tool_name":"mcp__x__getJiraIssue","tool_input":{"issueIdOrKey":"ABC-1"}}"#,
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_conn"))
            .arg("guard")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{payload}");
        assert!(output.stdout.is_empty(), "{payload}");
    }
}

#[test]
fn other_subcommands_keep_the_usage_error() {
    assert_eq!(
        conn(&["serve", "--port", "0"]).unwrap().status.code(),
        Some(2)
    );
    assert_eq!(conn(&["nonsense"]).unwrap().status.code(), Some(2));
}
