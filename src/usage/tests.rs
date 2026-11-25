//! The endpoint body's parse, the token's read, the `curl` call, and what a reading keeps.

use std::path::Path;

use super::*;
use crate::testutil::{Scratch, epoch, secs};
use crate::util::runner::{Exit, FakeRunner, Output};

/// The shape the endpoint answers with, cut to the fields read here plus a few it is known to
/// carry besides.
const BODY: &str = r#"{
  "five_hour": {"utilization": 7.0, "resets_at": "2026-11-05T00:30:00.123456+00:00", "used_dollars": null},
  "seven_day": {"utilization": 30.0, "resets_at": "2026-11-06T20:00:00.654321+00:00"},
  "seven_day_opus": null,
  "seven_day_sonnet": null,
  "extra_usage": {"is_enabled": true},
  "limits": [
    {"kind": "session", "percent": 8, "resets_at": "2026-11-05T00:30:00+00:00", "scope": null},
    {"kind": "weekly_all", "percent": 30, "resets_at": "2026-11-06T20:00:00+00:00", "scope": null},
    {"kind": "weekly_scoped", "percent": 0, "resets_at": "2026-11-06T20:00:00+00:00",
     "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
  ]
}"#;

fn spans(windows: &[Window]) -> Vec<String> {
    windows.iter().map(|w| w.span.to_string()).collect()
}

/// Mutation: drop the scoped entries, or read `session`/`weekly_all` as model windows -> red.
#[test]
fn the_body_gives_five_hours_the_week_and_each_model_week() {
    let windows = parse(BODY.as_bytes()).unwrap();
    assert_eq!(spans(&windows), ["5h", "7d", "7d Fable"]);
    let five = windows.first().unwrap();
    assert!((five.used_percent - 7.0).abs() < f64::EPSILON);
    assert_eq!(
        five.resets_at,
        Some("2026-11-05T00:30:00.123456Z".parse().unwrap())
    );
}

/// A model with a fixed field is read from it once, not again from `limits`.
/// Mutation: drop the `FIXED_MODELS` check in the scoped filter -> "7d Opus" twice.
#[test]
fn a_model_week_is_shown_once() {
    let body = r#"{
      "seven_day_opus": {"utilization": 12.5, "resets_at": null},
      "limits": [{"kind": "weekly_scoped", "percent": 13, "resets_at": null,
                  "scope": {"model": {"display_name": "Opus"}}}]
    }"#;
    let windows = parse(body.as_bytes()).unwrap();
    assert_eq!(spans(&windows), ["7d Opus"]);
    assert_eq!(windows.first().unwrap().resets_at, None);
}

/// "Not reported" is absent, never zero.
/// Mutation: `unwrap_or(0.0)` on `utilization` -> a "5h" window appears.
#[test]
fn a_window_without_its_use_is_left_out() {
    let body = r#"{"five_hour": {"utilization": null, "resets_at": null}, "seven_day": {"utilization": 1}}"#;
    assert_eq!(spans(&parse(body.as_bytes()).unwrap()), ["7d"]);
}

#[test]
fn a_body_that_is_not_the_shape_is_malformed() {
    let err = parse(b"<html>sign in</html>").unwrap_err();
    assert!(matches!(err, UsageError::Malformed(_)), "{err:?}");
}

const TOKEN: &str = "tok-not-a-real-one";

fn credentials(dir: &Path, expires_at_ms: i64) -> std::path::PathBuf {
    let path = dir.join(CREDENTIALS_FILE);
    let body = serde_json::json!({
        "claudeAiOauth": {"accessToken": TOKEN, "expiresAt": expires_at_ms, "refreshToken": "r"},
        "mcpOAuth": {}
    });
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

fn curl_answer(exit: Exit, stdout: &str, stderr: &str) -> FakeRunner {
    FakeRunner::new([Ok(Output {
        exit,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.to_owned(),
    })])
}

/// `curl` finishing the transfer with `body`, then the status line `--write-out` asks for.
fn http_answer(body: &str, status: u16, retry_after: &str) -> FakeRunner {
    curl_answer(
        Exit::Success,
        &format!("{body}\n{status} {retry_after}"),
        "",
    )
}

/// The token travels on stdin only: never in an argument, where `ps` would show it.
/// Mutation: pass `--header "Authorization: Bearer …"` as an argument -> red.
#[tokio::test]
async fn the_token_is_on_stdin_and_not_in_the_arguments() {
    let dir = Scratch::new("usage-fetch");
    let path = credentials(&dir, 2_000_000_000_000);
    let runner = http_answer(BODY, 200, "");
    let windows = fetch(&runner, &path, epoch(1_000)).await.unwrap();
    assert_eq!(windows.len(), 3);
    let calls = runner.calls();
    let call = calls.first().unwrap();
    assert_eq!(call.program, "curl");
    assert!(call.args.iter().all(|arg| !arg.contains(TOKEN)), "{call:?}");
    assert!(call.args.contains(&"@-".to_owned()), "{:?}", call.args);
    let input = String::from_utf8(call.input.clone()).unwrap();
    assert!(
        input.contains(&format!("Authorization: Bearer {TOKEN}\n")),
        "{input}"
    );
}

/// Mutation: compare the expiry with `>=` the other way round, or skip it -> `curl` is run.
#[tokio::test]
async fn an_expired_sign_in_is_left_for_claude_code_to_renew() {
    let dir = Scratch::new("usage-expired");
    let path = credentials(&dir, 1_000_000);
    let runner = FakeRunner::new([]);
    let err = fetch(&runner, &path, epoch(1_000)).await.unwrap_err();
    assert_eq!(err, UsageError::Expired);
    assert_eq!(runner.calls(), []);
}

#[tokio::test]
async fn no_credentials_file_is_not_signed_in() {
    let dir = Scratch::new("usage-absent");
    let err = fetch(&FakeRunner::new([]), &dir.join(CREDENTIALS_FILE), epoch(0))
        .await
        .unwrap_err();
    assert!(matches!(err, UsageError::NotSignedIn { .. }), "{err:?}");
}

/// A credentials file that does not parse is reported without quoting it.
/// Mutation: put serde's error in the reason -> the token text shows.
#[tokio::test]
async fn an_unparseable_credentials_file_is_not_quoted() {
    let dir = Scratch::new("usage-garbled");
    let path = dir.join(CREDENTIALS_FILE);
    std::fs::write(
        &path,
        format!("{{\"claudeAiOauth\": {{\"accessToken\": {TOKEN}}}}}"),
    )
    .unwrap();
    let err = fetch(&FakeRunner::new([]), &path, epoch(0))
        .await
        .unwrap_err();
    assert!(matches!(err, UsageError::Credentials { .. }), "{err:?}");
    assert!(!err.to_string().contains(TOKEN), "{err}");
}

/// What a fetch comes to when `runner` plays `curl`, with a sign-in that has not expired.
async fn fetched(name: &str, runner: &FakeRunner) -> Result<Vec<Window>, UsageError> {
    let dir = Scratch::new(name);
    let path = credentials(&dir, 2_000_000_000_000);
    fetch(runner, &path, epoch(1_000)).await
}

/// The HTTP status is read from the line after the body, so `--fail` must be off: with it an
/// error status is an exit code and the status and `Retry-After` are lost.
/// Mutation: put `--fail` back, or drop `--write-out` -> red.
#[tokio::test]
async fn curl_prints_the_status_after_the_body() {
    let runner = http_answer(BODY, 200, "");
    fetched("usage-args", &runner).await.unwrap();
    let calls = runner.calls();
    let args = &calls.first().unwrap().args;
    assert!(!args.contains(&"--fail".to_owned()), "{args:?}");
    let at = args.iter().position(|a| a == "--write-out").unwrap();
    assert_eq!(
        args.get(at + 1).map(String::as_str),
        Some("\n%{http_code} %header{retry-after}")
    );
}

/// Mutation: read 429 as `Status`, or ignore the header -> red.
#[tokio::test]
async fn a_429_is_a_refusal_with_the_wait_it_asked_for() {
    let runner = http_answer(r#"{"error": {"type": "rate_limit_error"}}"#, 429, "120");
    let err = fetched("usage-429", &runner).await.unwrap_err();
    assert_eq!(
        err,
        UsageError::Refused {
            retry_after: RetryAfter::Wait(SignedDuration::from_secs(120))
        }
    );
    assert!(err.to_string().contains("HTTP 429"), "{err}");
}

/// RFC 9110 lets `Retry-After` be a date; the wait is counted from the fetch's `now`
/// (1 000 s after the epoch), and a date already past is no wait.
/// Mutation: count from the epoch, or let a past date give a negative wait -> red.
#[test]
fn a_retry_after_date_is_a_wait_from_now() {
    let now = epoch(1_000);
    assert_eq!(
        RetryAfter::parse("Thu, 01 Jan 1970 00:20:00 GMT", now),
        RetryAfter::Wait(SignedDuration::from_secs(200))
    );
    assert_eq!(
        RetryAfter::parse("Thu, 01 Jan 1970 00:00:01 GMT", now),
        RetryAfter::Wait(SignedDuration::ZERO)
    );
    assert_eq!(RetryAfter::parse("", now), RetryAfter::NotSaid);
    assert_eq!(
        RetryAfter::parse("soon", now),
        RetryAfter::Unreadable("soon".to_owned())
    );
}

/// Mutation: parse the body of any status -> a `Malformed` or windows instead of `Status`.
#[tokio::test]
async fn another_status_is_reported_as_itself() {
    let runner = http_answer(BODY, 503, "30");
    let err = fetched("usage-503", &runner).await.unwrap_err();
    assert_eq!(err, UsageError::Status(503));
}

/// A non-zero exit is a transfer that did not happen, not an answer.
/// Mutation: read stdout whatever the exit -> red.
#[tokio::test]
async fn a_failed_transfer_is_a_tool_failure() {
    let runner = curl_answer(Exit::Code(28), "\n000 ", "curl: (28) Operation timed out");
    let err = fetched("usage-timeout", &runner).await.unwrap_err();
    assert!(
        matches!(err, UsageError::Tool(ToolError::NonZeroExit { .. })),
        "{err:?}"
    );
}

/// Mutation: take the whole stdout as the body when no status line is found -> red.
#[tokio::test]
async fn output_without_a_status_line_is_unparseable() {
    for stdout in [BODY.replace('\n', " "), "{}\nOK".to_owned()] {
        let runner = curl_answer(Exit::Success, &stdout, "");
        let err = fetched("usage-no-status", &runner).await.unwrap_err();
        assert!(
            matches!(err, UsageError::Tool(ToolError::Unparseable { .. })),
            "{stdout}: {err:?}"
        );
    }
}

fn window(percent: f64) -> Window {
    Window {
        span: Span::FiveHours,
        used_percent: percent,
        resets_at: None,
    }
}

/// A failed read keeps the last windows read and says it failed.
/// Mutation: clear `last_good` on failure -> red.
#[test]
fn a_failed_read_keeps_the_last_windows_and_reports_the_failure() {
    let mut reading = Reading::default();
    assert_eq!(reading.state(), &SourceState::NeverRead);
    reading.record(Ok(vec![window(7.0)]), secs(0));
    reading.record(Err(UsageError::Expired), secs(60));
    assert_eq!(
        reading.last_good(),
        Some(&Read {
            at: secs(0),
            windows: vec![window(7.0)]
        })
    );
    assert!(
        matches!(reading.state(), SourceState::Failed { at, .. } if *at == secs(60)),
        "{:?}",
        reading.state()
    );
}

/// Due once a minute after the last attempt, whatever it came to.
/// Mutation: `>` for `>=`, or gate on the last success -> red.
#[test]
fn a_read_is_due_a_minute_after_the_last_attempt() {
    let mut reading = Reading::default();
    assert!(reading.is_due(secs(0)));
    reading.started(secs(0));
    reading.record(Err(UsageError::Expired), secs(1));
    assert!(!reading.is_due(secs(59)));
    assert!(reading.is_due(secs(60)));
}

fn refused(retry_after: RetryAfter) -> Result<Vec<Window>, UsageError> {
    Err(UsageError::Refused { retry_after })
}

/// How many seconds after `answered` the next read is first due, found by stepping a second
/// at a time over two hours: past the longest wait, so a read never due fails the test.
fn next_due(reading: &Reading, answered: i64) -> i64 {
    (answered..=answered + 2 * 3_600)
        .find(|&t| reading.is_due(secs(t)))
        .map(|t| t - answered)
        .unwrap()
}

/// Each refusal in a row doubles the wait, counted from the answer: 2, 4, 8, 16, then 16
/// minutes. A success puts the pace back to a minute.
/// Mutation: drop the doubling, the cap, or the reset on success; count from the start of the
/// read -> red.
#[test]
fn refusals_in_a_row_double_the_wait_up_to_sixteen_minutes() {
    let mut reading = Reading::default();
    let mut t = 0;
    for expected_mins in [2, 4, 8, 16, 16] {
        reading.started(secs(t));
        reading.record(refused(RetryAfter::NotSaid), secs(t + 5));
        assert_eq!(next_due(&reading, t + 5), expected_mins * 60, "at {t}");
        assert_eq!(
            reading.backed_off_until(secs(t + 5)),
            Some(secs(t + 5 + expected_mins * 60))
        );
        t += 5 + expected_mins * 60;
    }
    reading.started(secs(t));
    reading.record(Ok(vec![window(7.0)]), secs(t + 1));
    assert_eq!(reading.backed_off_until(secs(t + 1)), None);
    assert_eq!(next_due(&reading, t + 1), 59);
    reading.started(secs(t + 60));
    reading.record(refused(RetryAfter::NotSaid), secs(t + 60));
    assert_eq!(
        next_due(&reading, t + 60),
        2 * 60,
        "the backoff starts over"
    );
}

/// A longer `Retry-After` is honoured, up to an hour; a shorter one does not shorten the
/// backoff.
/// Mutation: ignore `Retry-After`, take it as the wait even when shorter, or drop the hour cap
/// -> red.
#[test]
fn retry_after_lengthens_the_wait_up_to_an_hour() {
    for (asked_secs, expected_secs) in [(600, 600), (30, 120), (2 * 86_400, 3_600)] {
        let mut reading = Reading::default();
        reading.started(secs(0));
        reading.record(
            refused(RetryAfter::Wait(SignedDuration::from_secs(asked_secs))),
            secs(0),
        );
        assert_eq!(next_due(&reading, 0), expected_secs, "asked {asked_secs}");
    }
}

/// Only a refusal moves the pace. Another failure at a steady pace keeps the minute; one after
/// a backoff ran out is still a minute from the next read, and the refusal after it doubles
/// on from the backoff before.
/// Mutation: reset the pace on any outcome, back off on any failure, or let an expired backoff
/// make every moment due -> red.
#[test]
fn another_failure_leaves_the_pace_as_it_is() {
    let mut reading = Reading::default();
    reading.started(secs(0));
    reading.record(Err(UsageError::Status(500)), secs(0));
    assert_eq!(next_due(&reading, 0), 60);
    reading.started(secs(60));
    reading.record(refused(RetryAfter::NotSaid), secs(60));
    assert_eq!(next_due(&reading, 60), 120);
    reading.started(secs(180));
    reading.record(Err(UsageError::Status(500)), secs(180));
    assert_eq!(next_due(&reading, 180), 60);
    reading.started(secs(240));
    reading.record(refused(RetryAfter::NotSaid), secs(240));
    assert_eq!(next_due(&reading, 240), 240);
}
