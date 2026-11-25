//! `conn hook`: append the payload on stdin to the event log, and tell the session its place
//! in the coordination ledger.
//!
//! The hook runs inside every agent turn, so it never blocks or fails the agent: every failure
//! becomes one line on stderr, and the process exits 0. Stdout is the hook's answer: for
//! `SessionStart` and `UserPromptSubmit`, a JSON `hookSpecificOutput` that sets the session's
//! title (what it is about, pinned to its id, so "Open terminal" finds its window) and carries
//! the ledger's reminder (see [`crate::ledger::remind`]) as context; nothing for `Stop`.

use std::fmt::Display;
use std::fs::{OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::log::{LogLine, widen};
use super::{EventsError, HookEventName};
use crate::config::{Config, ConfigError, DEFAULT_STATE_DIR};
use crate::jira::resolution::ClosedTickets;
use crate::ledger::command::{Context, Sessions};
use crate::ledger::remind::{self, Moment};
use crate::ledger::{LEDGER_FILE, store};
use crate::sessions::windows::session_title;
use crate::types::{Instant, SessionId};
use crate::util::fs::{FILE_MODE, OpenRegularError, ensure_private_dir, open_regular};

/// The most stdin the hook keeps. The largest real payload is a Stop with the last assistant
/// message, tens of kilobytes; the cap only stops a runaway writer from filling the disk, and
/// an oversized payload is recorded by size alone.
pub(super) const PAYLOAD_BYTES_MAX: u64 = 16 * 1024 * 1024;

/// The event log, inside the state directory.
pub const LOG_FILE: &str = "events.jsonl";

/// Where this run writes the log, and why.
#[derive(Debug, PartialEq, Eq)]
enum StateDir {
    Configured(PathBuf),
    /// The config could not be had, so the default location is used: the hook keeps logging on
    /// a machine where the console is not configured yet.
    Fallback {
        path: PathBuf,
        config_problem: String,
    },
    /// Neither a config nor a home directory: nowhere to write.
    Unknown {
        config_problem: String,
    },
}

impl StateDir {
    fn choose(config: Result<&Config, &ConfigError>, home: Option<&Path>) -> Self {
        match (config, home) {
            (Ok(config), _) => Self::Configured(config.paths.state_dir.clone()),
            (Err(problem), Some(home)) => Self::Fallback {
                path: home.join(DEFAULT_STATE_DIR),
                config_problem: first_line(&problem),
            },
            (Err(problem), None) => Self::Unknown {
                config_problem: first_line(&problem),
            },
        }
    }
}

/// A config error's first line: the missing-file error goes on to show a whole example, and
/// the hook reports on one line.
fn first_line(error: &impl Display) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Runs `conn hook`. Returns normally whatever happens.
pub fn run_hook() {
    let config = Config::load();
    let chosen = StateDir::choose(config.as_ref(), std::env::home_dir().as_deref());
    let dir = match chosen {
        StateDir::Configured(path) => path,
        StateDir::Fallback {
            path,
            config_problem,
        } => {
            report(format_args!(
                "config unusable ({config_problem}); logging to {}",
                path.display()
            ));
            path
        }
        StateDir::Unknown { config_problem } => {
            report(format_args!(
                "config unusable ({config_problem}) and no home directory; event not logged"
            ));
            return;
        }
    };
    let line = match capture(io::stdin().lock(), Instant::now()) {
        Ok(line) => line,
        Err(error) => {
            report(error);
            return;
        }
    };
    if let Err(error) = append(&dir, &line) {
        report(error);
    }
    // The answer reads the gists and the session registry, which only a usable config locates.
    if let (Ok(config), LogLine::Payload { payload, .. }) = (&config, &line)
        && let Some(answer) = answer(config, payload)
    {
        let _ = writeln!(io::stdout(), "{answer}");
    }
}

/// The answer to a `SessionStart` or `UserPromptSubmit`: the session's title (what it is
/// about, pinned to its id, see [`session_title`]) and the ledger's reminder as context.
fn answer(config: &Config, payload: &serde_json::Value) -> Option<serde_json::Value> {
    let name = payload.get("hook_event_name")?.as_str()?;
    let moment = match name {
        name if name == HookEventName::SessionStart.as_str() => Moment::Start,
        name if name == HookEventName::UserPromptSubmit.as_str() => Moment::Prompt,
        _ => return None,
    };
    let session = SessionId::parse(payload.get("session_id")?.as_str()?).ok()?;
    let now = Instant::now();
    let gists = crate::gist::store::Store::load(&config.paths.state_dir, now);
    let gist = gists
        .get(&session)
        .and_then(crate::gist::store::Entry::written)
        .map(|written| written.gist.text.as_str().to_owned());
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from);
    let title = session_title(
        gist.as_deref().and_then(crate::gist::headline),
        cwd.as_deref(),
        &session,
    );
    serde_json::to_value(Answer {
        hook_specific_output: Specific {
            hook_event_name: name,
            session_title: title,
            additional_context: reminder(config, moment, &session, now),
        },
    })
    .ok()
}

/// The hook's answer, as Claude Code reads it.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Answer<'a> {
    hook_specific_output: Specific<'a>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Specific<'a> {
    hook_event_name: &'a str,
    session_title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_context: Option<String>,
}

/// The ledger's reminder for `session` at this moment; nothing when the ledger has nothing for
/// it or cannot be read.
fn reminder(config: &Config, moment: Moment, session: &SessionId, now: Instant) -> Option<String> {
    let loaded = match store::read(&config.paths.state_dir.join(LEDGER_FILE)) {
        Ok(loaded) => loaded,
        Err(error) => {
            report(error);
            return None;
        }
    };
    // The tickets `conn serve` read as closed: a wait on one is over, and the session is told.
    let ledger = loaded
        .ledger
        .knowing_closed(ClosedTickets::load(&config.paths.state_dir));
    let sessions = Sessions::from_registry(&config.paths.claude_dir);
    let zone = jiff::tz::TimeZone::system();
    let context = Context {
        sessions: &sessions,
        me: Some(session.clone()),
        now,
        zone: &zone,
    };
    remind::text(moment, &ledger, session, &context)
}

/// One line on stderr. A failure to write it has nowhere further to go.
fn report(message: impl Display) {
    let _ = writeln!(io::stderr(), "conn hook: {message}");
}

/// Reads `input` to its end and appends one line to `<state_dir>/events.jsonl`.
#[cfg(test)]
fn append_from(
    state_dir: &Path,
    received_at: Instant,
    input: impl Read,
) -> Result<(), EventsError> {
    append(state_dir, &capture(input, received_at)?)
}

/// Appends the captured line to `<state_dir>/events.jsonl`.
///
/// Stdin is consumed (by [`capture`]) before the disk is touched, so a failing disk never
/// leaves the agent blocked writing into a pipe nobody reads.
fn append(state_dir: &Path, line: &LogLine) -> Result<(), EventsError> {
    let mut bytes = serde_json::to_vec(line).map_err(EventsError::Encode)?;
    bytes.push(b'\n');

    // The log holds what the agent said, so a directory or file that exists with wider modes
    // than this tool creates is tightened.
    ensure_private_dir(state_dir).map_err(|source| EventsError::CreateStateDir {
        path: state_dir.to_path_buf(),
        source,
    })?;
    let path = state_dir.join(LOG_FILE);
    let unwritable = |source| EventsError::Unwritable {
        path: path.clone(),
        source,
    };
    // One write on an `O_APPEND` descriptor: concurrent hooks interleave whole lines.
    let mut log = open_regular(
        OpenOptions::new().append(true).create(true).mode(FILE_MODE),
        &path,
    )
    .map_err(|error| match error {
        OpenRegularError::Io(source) => unwritable(source),
        OpenRegularError::NotRegular(kind) => EventsError::NotRegular {
            path: path.clone(),
            kind,
        },
    })?;
    log.set_permissions(Permissions::from_mode(FILE_MODE))
        .map_err(unwritable)?;
    log.write_all(&bytes).map_err(unwritable)
}

fn capture(mut input: impl Read, received_at: Instant) -> Result<LogLine, EventsError> {
    let mut kept = Vec::new();
    input
        .by_ref()
        .take(PAYLOAD_BYTES_MAX + 1)
        .read_to_end(&mut kept)
        .map_err(EventsError::Stdin)?;
    if widen(kept.len()) > PAYLOAD_BYTES_MAX {
        let rest = io::copy(&mut input, &mut io::sink()).map_err(EventsError::Stdin)?;
        return Ok(LogLine::Oversized {
            received_at,
            oversized: widen(kept.len()) + rest,
        });
    }
    Ok(match serde_json::from_slice(&kept) {
        Ok(payload) => LogLine::Payload {
            received_at,
            payload,
        },
        Err(_) => LogLine::Unparsed {
            received_at,
            unparsed: String::from_utf8_lossy(&kept).into_owned(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testutil::at;
    use crate::testutil::Scratch;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    const NOW: &str = "2026-10-28T12:00:00Z";

    fn run(dir: &Scratch, input: &[u8]) -> Result<(), EventsError> {
        append_from(&dir.join("state"), at(NOW), Cursor::new(input.to_vec()))
    }

    fn log_text(dir: &Scratch) -> String {
        std::fs::read_to_string(dir.join("state").join(LOG_FILE)).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn valid_json_is_appended_as_a_payload_line_with_private_modes() {
        let dir = Scratch::new("hook");
        run(
            &dir,
            br#"{ "session_id": "s1", "hook_event_name": "Stop" }"#,
        )
        .unwrap();
        assert_eq!(
            log_text(&dir),
            "{\"received_at\":\"2026-10-28T12:00:00Z\",\"payload\":{\"hook_event_name\":\"Stop\",\"session_id\":\"s1\"}}\n"
        );
        assert_eq!(mode(&dir.join("state")), 0o700);
        assert_eq!(mode(&dir.join("state").join(LOG_FILE)), 0o600);
    }

    #[test]
    fn each_run_appends_one_line() {
        let dir = Scratch::new("hook");
        run(&dir, br#"{"n":1}"#).unwrap();
        run(&dir, br#"{"n":2}"#).unwrap();
        let text = log_text(&dir);
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().ends_with(r#"{"n":1}}"#));
        assert!(text.lines().nth(1).unwrap().ends_with(r#"{"n":2}}"#));
    }

    #[test]
    fn invalid_json_is_kept_as_unparsed_text() {
        let dir = Scratch::new("hook");
        run(&dir, b"not json {\nsecond line").unwrap();
        assert_eq!(
            log_text(&dir),
            "{\"received_at\":\"2026-10-28T12:00:00Z\",\"unparsed\":\"not json {\\nsecond line\"}\n"
        );
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily() {
        let dir = Scratch::new("hook");
        run(&dir, b"\xff{").unwrap();
        assert_eq!(
            log_text(&dir),
            "{\"received_at\":\"2026-10-28T12:00:00Z\",\"unparsed\":\"\u{fffd}{\"}\n"
        );
    }

    #[test]
    fn empty_stdin_is_an_unparsed_line_not_a_missing_one() {
        let dir = Scratch::new("hook");
        run(&dir, b"").unwrap();
        assert_eq!(
            log_text(&dir),
            "{\"received_at\":\"2026-10-28T12:00:00Z\",\"unparsed\":\"\"}\n"
        );
    }

    #[test]
    fn stdin_over_the_cap_is_recorded_by_size_alone() {
        let dir = Scratch::new("hook");
        let size = PAYLOAD_BYTES_MAX + 10;
        append_from(&dir.join("state"), at(NOW), io::repeat(b'x').take(size)).unwrap();
        assert_eq!(
            log_text(&dir),
            "{\"received_at\":\"2026-10-28T12:00:00Z\",\"oversized\":16777226}\n"
        );
    }

    #[test]
    fn stdin_of_exactly_the_cap_is_kept() {
        let dir = Scratch::new("hook");
        let mut input = br#"{"pad":""#.to_vec();
        let closing = br#""}"#;
        let fill = usize::try_from(PAYLOAD_BYTES_MAX).unwrap() - input.len() - closing.len();
        input.extend(std::iter::repeat_n(b'x', fill));
        input.extend_from_slice(closing);
        assert_eq!(widen(input.len()), PAYLOAD_BYTES_MAX);
        run(&dir, &input).unwrap();
        assert!(log_text(&dir).contains("\"payload\""));
    }

    /// A state directory and a log that exist with loose modes are tightened to 0700 and 0600
    /// before the line is appended, and the earlier lines are kept.
    /// Mutation: skip `ensure_private_dir`'s chmod, or the `set_permissions` on the log.
    #[test]
    fn existing_loose_modes_are_tightened() {
        let dir = Scratch::new("hook");
        let state = dir.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
        let log = state.join(LOG_FILE);
        std::fs::write(&log, "{\"earlier\":1}\n").unwrap();
        std::fs::set_permissions(&log, Permissions::from_mode(0o644)).unwrap();
        run(&dir, br#"{"n":1}"#).unwrap();
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&log), 0o600);
        assert_eq!(log_text(&dir).lines().count(), 2);
    }

    /// A log path that is a FIFO is refused by what it is, with nothing written into it,
    /// whether or not anything is reading it (an unread FIFO fails the non-blocking open, and
    /// must not hang the agent's turn).
    /// Mutation: open without `O_NONBLOCK` (the unread case hangs), or write before checking the
    /// type (the reader sees bytes).
    #[test]
    fn a_fifo_is_refused_and_never_written_or_waited_on() {
        let dir = Scratch::new("hook");
        let state = dir.join("state");
        std::fs::create_dir(&state).unwrap();
        let fifo = state.join(LOG_FILE);
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(matches!(
            run(&dir, b"{}"),
            Err(EventsError::Unwritable { .. })
        ));

        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap())
            .open(&fifo)
            .unwrap();
        assert!(matches!(
            run(&dir, b"{}"),
            Err(EventsError::NotRegular {
                kind: crate::util::fs::NonRegular::Fifo,
                ..
            })
        ));
        let mut received = Vec::new();
        let _ = reader.read_to_end(&mut received);
        assert_eq!(received, b"");
    }

    #[test]
    fn an_unwritable_state_directory_is_an_error_not_a_panic() {
        let dir = Scratch::new("hook");
        std::fs::write(dir.join("state"), "a file where the directory goes").unwrap();
        assert!(matches!(
            run(&dir, b"{}"),
            Err(EventsError::CreateStateDir { .. })
        ));
    }

    fn config() -> Config {
        Config::parse(
            "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n\
             [slack]\nhost = \"example.slack.com\"\n[paths]\nstate_dir = \"/var/conn\"\n",
            Path::new("/cfg.toml"),
            Path::new("/home/dev"),
        )
        .unwrap()
    }

    #[test]
    fn the_configured_state_dir_wins() {
        assert_eq!(
            StateDir::choose(Ok(&config()), Some(Path::new("/home/dev"))),
            StateDir::Configured(PathBuf::from("/var/conn"))
        );
    }

    #[test]
    fn a_missing_config_falls_back_to_the_default_under_home_with_a_one_line_reason() {
        let missing =
            Config::load_from(Path::new("/nonexistent/conn.toml"), Path::new("/home/dev"));
        let chosen = StateDir::choose(missing.as_ref(), Some(Path::new("/home/dev")));
        assert_eq!(
            chosen,
            StateDir::Fallback {
                path: PathBuf::from("/home/dev/.local/state/conn"),
                config_problem: "config file not found: /nonexistent/conn.toml".into(),
            }
        );
    }

    #[test]
    fn no_config_and_no_home_is_unknown() {
        let missing =
            Config::load_from(Path::new("/nonexistent/conn.toml"), Path::new("/home/dev"));
        assert!(matches!(
            StateDir::choose(missing.as_ref(), None),
            StateDir::Unknown { .. }
        ));
    }

    /// The hook tells a session its time wait is over once `conn serve` has saved its ticket
    /// as closed, before the time comes; without the saved file the wait stands.
    /// Mutation: drop `knowing_closed` from `reminder` (red, the wait reads as standing).
    #[test]
    fn a_reminder_says_a_wait_is_over_when_its_ticket_closed() {
        use crate::jira::resolution::{Closed, ClosedTickets, Resolution};
        let dir = Scratch::new("hook-closed");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let session = SessionId::parse("00000000-0000-4000-8000-000000000002").unwrap();
        let line = serde_json::json!({"at": "2026-11-10T20:16:00Z", "session": session.to_string(),
            "act": {"wait": {"on": {"until": "2099-01-01T00:00:00Z"}, "then": "close ABC-7",
                             "ticket": "ABC-7"}}});
        std::fs::write(state.join(LEDGER_FILE), format!("{line}\n")).unwrap();
        let config = Config::parse(
            &format!(
                "[paths]\nstate_dir = \"{}\"\nclaude_dir = \"{}\"\n",
                state.display(),
                dir.join("claude").display()
            ),
            Path::new("/cfg.toml"),
            Path::new("/home/dev"),
        )
        .unwrap();
        let now = at(NOW);
        let standing = reminder(&config, Moment::Prompt, &session, now).unwrap();
        assert!(standing.contains("waiting until"), "{standing}");

        let mut closed = ClosedTickets::default();
        closed.record(
            crate::types::JiraKey::parse("ABC-7").unwrap(),
            Resolution::Closed(Closed {
                resolution: "Done".to_owned(),
                status: "Done".to_owned(),
                at: None,
            }),
        );
        closed.save(&state).unwrap();
        let over = reminder(&config, Moment::Prompt, &session, now).unwrap();
        assert!(over.contains("is over: ABC-7 closed (Done)"), "{over}");
    }
}
