//! One tool-less model run: a prompt in, the model's JSON answer out.
//!
//! The run has no tools and no MCP servers, keeps no transcript and runs no hooks, so it can
//! read untrusted text and do nothing but answer. The answer is parsed strictly into the
//! caller's wire type; a reply that is not that JSON is [`OneshotError::Malformed`].

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::runner::{CommandRunner, ToolError, run_tool};

const CLAUDE: &str = "claude";

/// Why a run gave no answer. `Tool` and `Unparseable` mean nothing is known; `RunFailed` is
/// the run saying it failed; `Malformed` is a reply that is not the asked JSON.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OneshotError {
    #[error(transparent)]
    Tool(ToolError),
    #[error("claude's output is not JSON: {detail}")]
    Unparseable { detail: String },
    #[error("the model run failed: {detail}")]
    RunFailed { detail: String },
    #[error("the model's answer is not the asked JSON: {detail}")]
    Malformed { detail: String },
}

/// Runs `prompt` on Haiku with no tools and parses its reply as `T`.
pub async fn answer<T: DeserializeOwned>(
    runner: &dyn CommandRunner,
    prompt: &str,
    timeout: Duration,
) -> Result<T, OneshotError> {
    let args = [
        "-p",
        prompt,
        "--model",
        "haiku",
        "--output-format",
        "json",
        "--tools",
        "",
        "--strict-mcp-config",
        "--permission-mode",
        "default",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ];
    let output = run_tool(runner, CLAUDE, &args, timeout)
        .await
        .map_err(OneshotError::Tool)?;
    parse(&output.stdout)
}

#[derive(Deserialize)]
struct RunResult {
    is_error: Option<bool>,
    result: Option<String>,
}

/// The reply in `claude -p --output-format json` output, parsed as `T`. A reply fenced as a
/// code block is read inside the fence.
pub fn parse<T: DeserializeOwned>(stdout: &[u8]) -> Result<T, OneshotError> {
    let run: RunResult = serde_json::from_slice(stdout).map_err(|e| OneshotError::Unparseable {
        detail: e.to_string(),
    })?;
    let text = run.result.unwrap_or_default();
    if run.is_error == Some(true) {
        return Err(OneshotError::RunFailed { detail: text });
    }
    let json = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(json).map_err(|e| OneshotError::Malformed {
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    //! The parse against output in the shape `claude -p --output-format json` printed in a
    //! real run: one object with `is_error` and the reply as `result`.

    use serde_json::json;

    use super::*;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    struct Wire {
        word: String,
    }

    fn run(result: &str, is_error: bool) -> Vec<u8> {
        json!({"type": "result", "is_error": is_error, "result": result})
            .to_string()
            .into_bytes()
    }

    /// Mutation: drop the fence stripping (the fenced reply is `Malformed`), or ignore
    /// `is_error` (the failed run's text is parsed and reads as `Malformed`).
    #[test]
    fn each_way_a_run_ends_is_its_own_answer() {
        let word = || Wire {
            word: "ok".to_owned(),
        };
        assert_eq!(parse(&run(r#"{"word":"ok"}"#, false)), Ok(word()));
        assert_eq!(
            parse(&run("```json\n{\"word\":\"ok\"}\n```", false)),
            Ok(word())
        );
        assert!(matches!(
            parse::<Wire>(&run("Sure! Here it is.", false)),
            Err(OneshotError::Malformed { .. })
        ));
        assert_eq!(
            parse::<Wire>(&run(r#"{"word":"ok"}"#, true)),
            Err(OneshotError::RunFailed {
                detail: r#"{"word":"ok"}"#.to_owned()
            })
        );
        assert!(matches!(
            parse::<Wire>(b"not json"),
            Err(OneshotError::Unparseable { .. })
        ));
    }
}
