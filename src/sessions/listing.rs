//! `claude agents --json --all`: the supervisor's list of every session it knows.
//!
//! The output is undocumented, so it is parsed field by field ([`super::json`]): unknown
//! fields are ignored and unknown values are kept verbatim as `Other`. An entry that lacks a
//! field its kind cannot do without fails the whole listing, because a console that silently
//! lacks one session would show a calm it has not observed.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use super::facts::{JobId, ProcessStatus, waiting_for};
use crate::types::{Instant, Pid, SessionId, WaitingFor};
use crate::util::json::{FieldError, Fields};
use crate::util::runner::{CommandRunner, ToolError, run_tool};

pub type ListingError = ToolError;

const PROGRAM: &str = "claude";
const ARGS: [&str; 3] = ["agents", "--json", "--all"];
/// Observed latency is about 0.2 s; the bound is for a wedged supervisor, not a slow one.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What every entry carries, whatever its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedCommon {
    pub cwd: PathBuf,
    pub started_at: Instant,
    /// Absent until Claude Code has assigned one ("when set").
    pub session_id: Option<SessionId>,
    pub name: Option<String>,
}

/// The facts that exist only while the session's process is alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: Pid,
    pub status: Option<ProcessStatus>,
    /// Present while the process waits for the human.
    pub waiting_for: Option<WaitingFor>,
}

/// The state a background session's supervisor reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundState {
    Working,
    Blocked,
    Done,
    Failed,
    Stopped,
    /// A state this code does not know, verbatim.
    Other(String),
}

impl BackgroundState {
    fn parse(text: &str) -> Self {
        match text {
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// A terminal session; always has a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedInteractive {
    pub common: ListedCommon,
    pub process: LiveProcess,
}

/// A background session. `process` is `None` once it has exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedBackground {
    pub common: ListedCommon,
    pub job_id: JobId,
    pub state: BackgroundState,
    pub process: Option<LiveProcess>,
}

/// An entry of a kind this code does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedOther {
    pub kind: String,
    pub common: ListedCommon,
    pub process: Option<LiveProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedSession {
    Interactive(ListedInteractive),
    Background(ListedBackground),
    Other(ListedOther),
}

impl ListedSession {
    pub const fn common(&self) -> &ListedCommon {
        match self {
            Self::Interactive(s) => &s.common,
            Self::Background(s) => &s.common,
            Self::Other(s) => &s.common,
        }
    }
}

/// Every session the supervisor lists, or the reason it could not be asked.
pub async fn listing(runner: &dyn CommandRunner) -> Result<Vec<ListedSession>, ListingError> {
    let out = run_tool(runner, PROGRAM, &ARGS, TIMEOUT).await?;
    parse_listing(&out.stdout).map_err(|reason| ToolError::Unparseable {
        program: PROGRAM.to_owned(),
        reason,
    })
}

pub(super) fn parse_listing(stdout: &[u8]) -> Result<Vec<ListedSession>, String> {
    let json: Value = serde_json::from_slice(stdout).map_err(|e| e.to_string())?;
    let entries = json
        .as_array()
        .ok_or_else(|| "the top level is not an array".to_owned())?;
    entries
        .iter()
        .enumerate()
        .map(|(at, entry)| parse_entry(entry).map_err(|e| format!("entry {at}: {e}")))
        .collect()
}

fn parse_entry(entry: &Value) -> Result<ListedSession, FieldError> {
    let fields = Fields::of(entry)?;
    let common = ListedCommon {
        cwd: fields.required_path("cwd")?,
        started_at: fields.required_millis("startedAt")?,
        session_id: fields.optional_session_id("sessionId")?,
        name: fields.optional_str("name")?.map(str::to_owned),
    };
    let process = live_process(&fields)?;
    match fields.required_str("kind")? {
        "interactive" => Ok(ListedSession::Interactive(ListedInteractive {
            common,
            process: process.ok_or(FieldError::missing("pid"))?,
        })),
        "background" => Ok(ListedSession::Background(ListedBackground {
            common,
            job_id: JobId::parse(fields.required_str("id")?)
                .map_err(|e| FieldError::invalid("id", e.to_string()))?,
            state: BackgroundState::parse(fields.required_str("state")?),
            process,
        })),
        other => Ok(ListedSession::Other(ListedOther {
            kind: other.to_owned(),
            common,
            process,
        })),
    }
}

/// `status` and `waitingFor` describe a process, so they are meaningless without a `pid`.
fn live_process(fields: &Fields<'_>) -> Result<Option<LiveProcess>, FieldError> {
    let status = fields.optional_str("status")?.map(ProcessStatus::parse);
    let waiting = fields.optional_str("waitingFor")?.map(waiting_for);
    match fields.optional_pid("pid")? {
        Some(pid) => Ok(Some(LiveProcess {
            pid,
            status,
            waiting_for: waiting,
        })),
        None if status.is_none() && waiting.is_none() => Ok(None),
        None => Err(FieldError::invalid(
            "status",
            "a status or waitingFor without a pid".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testutil::{epoch as at, session_id as id};
    use crate::util::runner::{Exit, FakeRunner, Output, RunError};

    const FIXTURE: &str = include_str!("../../tests/fixtures/sessions/listing.json");

    fn listed() -> Vec<ListedSession> {
        parse_listing(FIXTURE.as_bytes()).unwrap()
    }

    fn live(pid: u32, status: Option<ProcessStatus>, waiting: Option<WaitingFor>) -> LiveProcess {
        LiveProcess {
            pid: Pid::new(pid),
            status,
            waiting_for: waiting,
        }
    }

    /// Hand-written from the fixture. Mutation: parse every entry as `Other`, or read
    /// `status` into the wrong slot -> red.
    #[test]
    fn both_entry_shapes_and_an_unknown_kind_parse_into_their_variants() {
        let sessions = listed();
        assert_eq!(sessions.len(), 8);

        assert_eq!(
            sessions[0],
            ListedSession::Interactive(ListedInteractive {
                common: ListedCommon {
                    cwd: "/home/dev/alpha".into(),
                    started_at: at(1_761_645_600),
                    session_id: Some(id(1)),
                    name: Some("alpha-01".into()),
                },
                process: live(4101, Some(ProcessStatus::Busy), None),
            })
        );
        assert_eq!(
            sessions[1],
            ListedSession::Interactive(ListedInteractive {
                common: ListedCommon {
                    cwd: "/home/dev/beta".into(),
                    started_at: at(1_761_649_200),
                    session_id: Some(id(2)),
                    name: Some("beta-01".into()),
                },
                process: live(
                    4102,
                    Some(ProcessStatus::Waiting),
                    Some(WaitingFor::PermissionPrompt)
                ),
            })
        );
    }

    /// Mutation: map every unknown status to `Idle`, or fail on it -> red.
    #[test]
    fn unknown_values_are_kept_verbatim() {
        let sessions = listed();
        let ListedSession::Interactive(third) = &sessions[2] else {
            panic!("entry 2 is not interactive: {:?}", sessions[2]);
        };
        assert_eq!(
            third.process.status,
            Some(ProcessStatus::Other("hibernating".into()))
        );
        let ListedSession::Interactive(fourth) = &sessions[3] else {
            panic!("entry 3 is not interactive");
        };
        assert_eq!(fourth.process.waiting_for, Some(WaitingFor::QuotaHold));
        let ListedSession::Background(paused) = &sessions[6] else {
            panic!("entry 6 is not background");
        };
        assert_eq!(paused.state, BackgroundState::Other("paused".into()));
        let ListedSession::Other(other) = &sessions[7] else {
            panic!("entry 7 is not of an unknown kind");
        };
        assert_eq!(other.kind, "scheduled");
        assert_eq!(other.process, None);
    }

    /// Mutation: require `pid` for background entries -> the exited entry fails -> red.
    #[test]
    fn a_live_and_an_exited_background_entry_differ_in_process_only() {
        let sessions = listed();
        let ListedSession::Background(blocked) = &sessions[4] else {
            panic!("entry 4 is not background");
        };
        assert_eq!(blocked.job_id.as_str(), "abcd0001");
        assert_eq!(blocked.state, BackgroundState::Blocked);
        assert_eq!(
            blocked.process,
            Some(live(
                4201,
                Some(ProcessStatus::Waiting),
                Some(WaitingFor::InputNeeded)
            ))
        );
        let ListedSession::Background(done) = &sessions[5] else {
            panic!("entry 5 is not background");
        };
        assert_eq!(done.state, BackgroundState::Done);
        assert_eq!(done.process, None);
        assert_eq!(done.common.session_id, None);
    }

    /// Mutation: ignore the `Err` from `live_process` -> red.
    #[test]
    fn an_entry_missing_what_its_kind_needs_fails_the_listing_and_names_it() {
        let cases = [
            (
                r#"[{"kind":"interactive","cwd":"/a","startedAt":1000}]"#,
                "entry 0: \"pid\" is missing",
            ),
            (
                r#"[{"kind":"background","cwd":"/a","startedAt":1000,"state":"done"}]"#,
                "entry 0: \"id\" is missing",
            ),
            (
                r#"[{"kind":"background","id":"x1","cwd":"/a","startedAt":1000,"status":"idle","state":"done"}]"#,
                "entry 0: \"status\" is not valid: a status or waitingFor without a pid",
            ),
            (
                r#"[{"kind":"interactive","pid":1,"startedAt":1000}]"#,
                "entry 0: \"cwd\" is missing",
            ),
            (
                r#"[{"kind":"interactive","pid":1,"cwd":"/a","startedAt":1000,"sessionId":"nope"}]"#,
                "entry 0: \"sessionId\" is not valid: not a UUID-shaped session id: \"nope\"",
            ),
            (r#"{"not":"an array"}"#, "the top level is not an array"),
            (r"", "EOF while parsing a value at line 1 column 0"),
        ];
        for (json, expected) in cases {
            assert_eq!(
                parse_listing(json.as_bytes()).unwrap_err(),
                expected,
                "{json}"
            );
        }
    }

    fn reply(exit: Exit, stdout: &str) -> Output {
        Output {
            exit,
            stdout: stdout.as_bytes().to_vec(),
            stderr: "oops".to_owned(),
        }
    }

    /// The four failures are four variants and none is an empty list.
    /// Mutation: map any error to `Ok(vec![])` -> red on every row.
    #[tokio::test]
    async fn each_failure_is_its_own_variant() {
        let cases: Vec<(Result<Output, RunError>, ListingError)> = vec![
            (
                Err(RunError::NotFound),
                ToolError::Missing {
                    program: "claude".into(),
                },
            ),
            (
                Ok(reply(Exit::Code(1), "")),
                ToolError::NonZeroExit {
                    program: "claude".into(),
                    code: Some(1),
                    stderr: "oops".into(),
                },
            ),
            (
                Err(RunError::TimedOut(Duration::from_secs(10))),
                ToolError::TimedOut {
                    program: "claude".into(),
                    after: Duration::from_secs(10),
                },
            ),
            (
                Ok(reply(Exit::Success, "Loading agents...")),
                ToolError::Unparseable {
                    program: "claude".into(),
                    reason: "expected value at line 1 column 1".into(),
                },
            ),
        ];
        for (scripted, expected) in cases {
            let runner = FakeRunner::new([scripted]);
            assert_eq!(listing(&runner).await, Err(expected));
        }
    }

    /// Mutation: change an argument (drop `--all`) -> red.
    #[tokio::test]
    async fn asks_for_every_session_as_json() {
        let runner = FakeRunner::printing("[]");
        assert_eq!(listing(&runner).await, Ok(vec![]));
        assert_eq!(runner.lines(), ["claude agents --json --all"]);
    }

    /// Reads this machine's real listing; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the claude CLI and a running supervisor"]
    #[allow(
        clippy::print_stdout,
        reason = "an ignored smoke test reports counts to its runner"
    )]
    async fn real_listing_parses() {
        let sessions = listing(&crate::util::runner::TokioRunner).await.unwrap();
        let interactive = sessions
            .iter()
            .filter(|s| matches!(s, ListedSession::Interactive(_)))
            .count();
        let background = sessions
            .iter()
            .filter(|s| matches!(s, ListedSession::Background(_)))
            .count();
        println!(
            "listing: {} total, {interactive} interactive, {background} background",
            sessions.len()
        );
    }
}
