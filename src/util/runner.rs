//! Running one program: the seam between the tool and the operating system.
//!
//! A run is one call, one deadline, captured output, and optionally bytes written to the
//! program's stdin; a launch starts a program that outlives the call, with no deadline and no
//! output. A caller that needs more (a working directory, an environment) extends this trait
//! rather than bypassing it, because every caller is tested through [`FakeRunner`].

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// The most bytes kept of each of a program's stdout and stderr.
///
/// The programs run here (`claude agents`, `gh pr view`, `niri msg`) print kilobytes; four
/// MiB is three orders of magnitude above that, so reaching it means a runaway or the wrong
/// program, and the run is refused rather than truncated into a plausible-looking answer.
pub const OUTPUT_BYTES_MAX: u64 = 4 * 1024 * 1024;

/// How a finished process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Success,
    Code(i32),
    /// The process was ended by a signal, so it has no exit code.
    Signalled,
}

impl Exit {
    /// The exit code, `None` when a signal ended the process.
    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Success => Some(0),
            Self::Code(code) => Some(code),
            Self::Signalled => None,
        }
    }
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        match (status.success(), status.code()) {
            (true, _) => Self::Success,
            (false, Some(code)) => Self::Code(code),
            (false, None) => Self::Signalled,
        }
    }
}

/// What a finished process left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    /// Lossily decoded: it is only ever shown to a human.
    pub stderr: String,
}

/// The process did not finish: it never started, it was cut off, or it said too much.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("program not found")]
    NotFound,
    #[error("cannot start: {0}")]
    Spawn(io::Error),
    #[error("timed out after {0:?} and was killed")]
    TimedOut(Duration),
    #[error("lost the child while waiting for it: {0}")]
    Wait(io::Error),
    #[error("printed more than {OUTPUT_BYTES_MAX} bytes on one stream and was killed")]
    OutputTooLarge,
}

/// A boxed future, so [`CommandRunner`] stays usable as `&dyn CommandRunner`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait CommandRunner: Send + Sync {
    /// Runs `program` with `args` and no input. A process still running at `timeout` is
    /// killed and the result is [`RunError::TimedOut`].
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>>;

    /// [`run`](Self::run), with `input` written to the program's stdin, which is then closed.
    /// For what must not appear in the process table, such as a credential.
    fn run_with_input<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        input: &'a [u8],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>>;

    /// Starts `program` with `args`, no input and no output, and does not wait for it.
    ///
    /// For a program that may live as long as the person wants, such as a browser that
    /// `xdg-open` starts and waits on: it has no deadline and is never killed. Only a failure
    /// to start is returned; an exit other than success is logged when it happens.
    fn launch(&self, program: &str, args: &[&str]) -> Result<(), RunError>;
}

/// The error a failure to start `program` is, whether running or launching it.
fn spawn_error(error: io::Error) -> RunError {
    if error.kind() == io::ErrorKind::NotFound {
        RunError::NotFound
    } else {
        RunError::Spawn(error)
    }
}

/// Runs real processes through `tokio`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioRunner;

impl CommandRunner for TokioRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(run_process(program, args, None, timeout))
    }

    fn run_with_input<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        input: &'a [u8],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(run_process(program, args, Some(input), timeout))
    }

    fn launch(&self, program: &str, args: &[&str]) -> Result<(), RunError> {
        // The program's output would land on conn's stdout, which carries only what conn
        // prints, so it goes nowhere.
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(spawn_error)?;
        let program = program.to_owned();
        // Waiting reaps the child; the task ends with it, or with the runtime.
        tokio::spawn(async move {
            match child.wait().await.map(Exit::from) {
                Ok(Exit::Success) => {}
                Ok(exit) => tracing::warn!(%program, ?exit, "a launched program failed"),
                Err(error) => tracing::warn!(%program, %error, "lost a launched program"),
            }
        });
        Ok(())
    }
}

/// One run of a real process: `input`, when there is any, is written to its stdin while its
/// output is read, so a program that answers before it has read everything cannot deadlock
/// the run.
async fn run_process(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, RunError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping the child on timeout, or on an early return below, is what kills it;
        // without this it would be left running and orphaned.
        .kill_on_drop(true)
        .spawn()
        .map_err(spawn_error)?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(RunError::Spawn(io::Error::other(
            "the pipes were not set up",
        )));
    };
    let stdin = child.stdin.take();
    let feed = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            stdin.write_all(input).await.map_err(RunError::Wait)?;
            // Dropped here, which closes the pipe: the program sees the end of its input.
        }
        Ok(())
    };
    let finish = async {
        let ((), stdout, stderr, status) =
            tokio::try_join!(feed, read_bounded(stdout), read_bounded(stderr), async {
                child.wait().await.map_err(RunError::Wait)
            },)?;
        Ok(Output {
            exit: status.into(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    };
    tokio::time::timeout(timeout, finish)
        .await
        .map_err(|_elapsed| RunError::TimedOut(timeout))?
}

/// Everything the pipe yields up to its end, or [`RunError::OutputTooLarge`] when that is
/// more than [`OUTPUT_BYTES_MAX`].
async fn read_bounded(pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, RunError> {
    let mut bytes = Vec::new();
    // One byte past the bound tells "exactly the bound" from "more".
    pipe.take(OUTPUT_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(RunError::Wait)?;
    if u64::try_from(bytes.len()).is_ok_and(|len| len > OUTPUT_BYTES_MAX) {
        Err(RunError::OutputTooLarge)
    } else {
        Ok(bytes)
    }
}

/// One call a [`FakeRunner`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub program: String,
    pub args: Vec<String>,
    /// What was written to stdin; empty for a run without input.
    pub input: Vec<u8>,
    pub timeout: Duration,
}

impl Call {
    /// `program arg arg`, for a test that wants to read the command as one line.
    pub fn line(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Answers calls from a script, in order, and records them.
///
/// A call after the script is exhausted fails to spawn, so a test that makes more calls
/// than it scripted fails loudly instead of seeing an invented answer.
#[derive(Debug, Default)]
pub struct FakeRunner {
    script: Mutex<VecDeque<Result<Output, RunError>>>,
    calls: Mutex<Vec<Call>>,
}

impl FakeRunner {
    pub fn new(script: impl IntoIterator<Item = Result<Output, RunError>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: Mutex::default(),
        }
    }

    /// A runner whose one answer is a successful run that printed `stdout`.
    pub fn printing(stdout: &str) -> Self {
        Self::new([Ok(Output {
            exit: Exit::Success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: String::new(),
        })])
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Each call as one `program arg arg` line.
    pub fn lines(&self) -> Vec<String> {
        self.calls().iter().map(Call::line).collect()
    }

    /// Records the call and pops the next scripted answer.
    fn answer(
        &self,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<Output, RunError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call {
                program: program.to_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                input: input.to_vec(),
                timeout,
            });
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| {
                Err(RunError::Spawn(io::Error::other(
                    "FakeRunner: the script has no answer left for this call",
                )))
            })
    }
}

impl CommandRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(std::future::ready(self.answer(program, args, &[], timeout)))
    }

    fn run_with_input<'a>(
        &'a self,
        program: &'a str,
        args: &'a [&'a str],
        input: &'a [u8],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(std::future::ready(
            self.answer(program, args, input, timeout),
        ))
    }

    /// Recorded with a zero timeout, since a launch has none; a scripted `Ok` is a start and
    /// its output is unused.
    fn launch(&self, program: &str, args: &[&str]) -> Result<(), RunError> {
        self.answer(program, args, &[], Duration::ZERO)
            .map(|_output| ())
    }
}

/// Why a tool could not give an answer. Every variant means "could not tell": none of them is
/// an empty result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("{program} is not installed or not on PATH")]
    Missing { program: String },
    #[error("cannot start {program}: {message}")]
    Spawn { program: String, message: String },
    #[error("{program} did not answer within {after:?}")]
    TimedOut { program: String, after: Duration },
    #[error("{program} printed more than it may and was killed")]
    OutputTooLarge { program: String },
    #[error("{program} exited with {code:?}: {stderr}")]
    NonZeroExit {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("{program} printed output that is not what was expected: {reason}")]
    Unparseable { program: String, reason: String },
}

/// The most of a tool's stderr kept in an error; enough for a message, bounded for a log line.
const STDERR_CHARS_MAX: usize = 400;

/// Runs a tool and insists it exits with status 0.
pub async fn run_tool(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, ToolError> {
    tool_outcome(program, runner.run(program, args, timeout).await)
}

/// [`run_tool`], with `input` on the tool's stdin.
pub async fn run_tool_with_input(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
) -> Result<Output, ToolError> {
    tool_outcome(
        program,
        runner.run_with_input(program, args, input, timeout).await,
    )
}

/// A run of the tool `program` as a [`ToolError`] unless it finished with status 0.
fn tool_outcome(program: &str, run: Result<Output, RunError>) -> Result<Output, ToolError> {
    let program_name = || program.to_owned();
    let out = run.map_err(|e| match e {
        RunError::NotFound => ToolError::Missing {
            program: program_name(),
        },
        RunError::Spawn(e) | RunError::Wait(e) => ToolError::Spawn {
            program: program_name(),
            message: e.to_string(),
        },
        RunError::TimedOut(after) => ToolError::TimedOut {
            program: program_name(),
            after,
        },
        RunError::OutputTooLarge => ToolError::OutputTooLarge {
            program: program_name(),
        },
    })?;
    match out.exit {
        Exit::Success => Ok(out),
        Exit::Code(_) | Exit::Signalled => Err(ToolError::NonZeroExit {
            program: program_name(),
            code: out.exit.code(),
            stderr: out.stderr.trim().chars().take(STDERR_CHARS_MAX).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    //! `TokioRunner` against real processes (`sh`), and `FakeRunner`'s own contract.

    use std::path::PathBuf;

    use super::*;

    const GENEROUS: Duration = Duration::from_secs(30);

    fn sh(script: &str) -> [&str; 2] {
        ["-c", script]
    }

    /// Mutation: report `Exit::Success` regardless of the status, or swap stdout and stderr.
    #[tokio::test]
    async fn captures_exit_code_stdout_and_stderr() {
        let out = TokioRunner
            .run("sh", &sh("printf out; printf err >&2; exit 3"), GENEROUS)
            .await
            .unwrap();
        assert_eq!(
            out,
            Output {
                exit: Exit::Code(3),
                stdout: b"out".to_vec(),
                stderr: "err".to_owned(),
            }
        );
    }

    /// Mutation: leave stdin at `null`, or never close it (`cat` then waits until the timeout).
    #[tokio::test]
    async fn input_reaches_stdin_and_its_end_is_seen() {
        let out = TokioRunner
            .run_with_input("cat", &[], b"secret\n", GENEROUS)
            .await
            .unwrap();
        assert_eq!(out.stdout, b"secret\n");
        assert_eq!(out.exit, Exit::Success);
    }

    #[tokio::test]
    async fn success_is_success() {
        let out = TokioRunner.run("sh", &sh("true"), GENEROUS).await.unwrap();
        assert_eq!(out.exit, Exit::Success);
    }

    #[tokio::test]
    async fn a_signalled_process_has_no_exit_code() {
        let out = TokioRunner
            .run("sh", &sh("kill -9 $$"), GENEROUS)
            .await
            .unwrap();
        assert_eq!(out.exit, Exit::Signalled);
        assert_eq!(out.exit.code(), None);
    }

    /// Mutation: map every spawn failure to `Spawn`.
    #[tokio::test]
    async fn a_missing_program_is_reported_as_missing() {
        let result = TokioRunner
            .run("conn-test-no-such-program", &[], GENEROUS)
            .await;
        assert!(matches!(result, Err(RunError::NotFound)), "{result:?}");
    }

    /// Mutation: map every spawn failure to `Spawn` in `launch`.
    #[tokio::test]
    async fn launching_a_missing_program_is_reported_as_missing() {
        let result = TokioRunner.launch("conn-test-no-such-program", &[]);
        assert!(matches!(result, Err(RunError::NotFound)), "{result:?}");
    }

    /// A launch returns while the program still runs, and leaves it running.
    /// Mutation: wait for the child before returning.
    #[tokio::test]
    async fn a_launch_returns_at_once_and_leaves_the_program_running() {
        let pid_file = unique_pid_file();
        let script = format!("echo $$ > {}; exec sleep 3", pid_file.display());
        let started = std::time::Instant::now();
        TokioRunner.launch("sh", &sh(&script)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the launch waited"
        );
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file)
                && !pid.trim().is_empty()
            {
                break pid.trim().to_owned();
            }
            assert!(started.elapsed() < Duration::from_secs(2), "never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        std::fs::remove_file(&pid_file).unwrap();
        assert!(PathBuf::from("/proc").join(&pid).exists(), "{pid} is gone");
        assert!(process_exits(&pid).await, "{pid} outlived its sleep");
    }

    fn unique_pid_file() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("conn-runner-test-{}-{nanos}", std::process::id()))
    }

    /// Whether `/proc/<pid>` is gone within a few seconds. A killed child leaves the table a
    /// moment after the signal; one never killed outlives the wait.
    async fn process_exits(pid: &str) -> bool {
        let proc_dir = PathBuf::from("/proc").join(pid);
        for _ in 0..50 {
            if !proc_dir.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Mutation: drop `kill_on_drop(true)`; the `sleep` stays alive.
    #[tokio::test]
    async fn a_timed_out_process_is_killed() {
        let pid_file = unique_pid_file();
        let script = format!("echo $$ > {}; exec sleep 600", pid_file.display());
        let result = TokioRunner
            .run("sh", &sh(&script), Duration::from_millis(1500))
            .await;
        assert!(matches!(result, Err(RunError::TimedOut(_))), "{result:?}");
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        std::fs::remove_file(&pid_file).unwrap();
        assert!(
            process_exits(&pid).await,
            "process {pid} outlived its timeout"
        );
    }

    /// Mutation: read the pipes without `take`, or compare with `>=` the bound.
    #[tokio::test]
    async fn output_over_the_bound_is_refused_and_exactly_the_bound_is_kept() {
        let bytes = OUTPUT_BYTES_MAX.to_string();
        let over = format!("head -c {} /dev/zero", OUTPUT_BYTES_MAX + 1);
        let result = TokioRunner.run("sh", &sh(&over), GENEROUS).await;
        assert!(
            matches!(result, Err(RunError::OutputTooLarge)),
            "{result:?}"
        );
        let on_stderr = format!("head -c {} /dev/zero >&2", OUTPUT_BYTES_MAX + 1);
        let result = TokioRunner.run("sh", &sh(&on_stderr), GENEROUS).await;
        assert!(
            matches!(result, Err(RunError::OutputTooLarge)),
            "{result:?}"
        );
        let exact = format!("head -c {bytes} /dev/zero");
        let out = TokioRunner.run("sh", &sh(&exact), GENEROUS).await.unwrap();
        assert_eq!(out.stdout.len().to_string(), bytes);
    }

    #[tokio::test]
    async fn the_fake_answers_in_order_records_calls_and_then_refuses() {
        let ok = |text: &str| {
            Ok(Output {
                exit: Exit::Success,
                stdout: text.as_bytes().to_vec(),
                stderr: String::new(),
            })
        };
        let fake = FakeRunner::new([ok("one"), Err(RunError::TimedOut(Duration::from_secs(2)))]);
        let first = fake.run("p", &["a"], Duration::from_secs(1)).await.unwrap();
        assert_eq!(first.stdout, b"one");
        let second = fake.run("p", &["a"], Duration::from_secs(2)).await;
        assert!(matches!(second, Err(RunError::TimedOut(_))));
        let third = fake.run("p", &["a"], Duration::from_secs(3)).await;
        assert!(matches!(third, Err(RunError::Spawn(_))));
        assert_eq!(
            fake.calls()
                .iter()
                .map(|c| c.timeout.as_secs())
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(fake.lines(), ["p a", "p a", "p a"]);
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// Mutation: treat any exit code as success in `run_tool` -> red.
    #[tokio::test]
    async fn run_tool_turns_a_failed_exit_into_an_error() {
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Code(2),
            stdout: b"partial".to_vec(),
            stderr: "  bad flag \n".to_owned(),
        })]);
        let err = run_tool(&runner, "tool", &["--x"], WAIT).await.unwrap_err();
        assert_eq!(
            err,
            ToolError::NonZeroExit {
                program: "tool".into(),
                code: Some(2),
                stderr: "bad flag".into()
            }
        );
        assert_eq!(runner.lines(), ["tool --x"]);
    }

    /// Mutation: map `Signalled` to `Success`.
    #[tokio::test]
    async fn run_tool_reports_a_signalled_tool_with_no_code() {
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Signalled,
            stdout: Vec::new(),
            stderr: String::new(),
        })]);
        let err = run_tool(&runner, "tool", &[], WAIT).await.unwrap_err();
        assert!(
            matches!(err, ToolError::NonZeroExit { code: None, .. }),
            "{err:?}"
        );
    }
}
