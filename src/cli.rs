//! The command line, and the dispatch from a subcommand to the module that implements it.
//!
//! The one place (with `main.rs`) where errors become `anyhow`.

use std::ffi::OsString;
use std::io::Write;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use crate::config::Config;
use crate::events::settings::SettingsPatch;
use crate::ledger::command::{Context as LedgerContext, IfOwned, Request, Sessions};
use crate::ledger::{Note, Outcome, Slug, Subject, WaitOn};
use crate::types::{Instant, JiraKey, SessionId};
use crate::util::runner::{CommandRunner, TokioRunner};
use crate::web::SignIn;
use crate::{events, ledger, replay, snapshot, state, web};

#[derive(Debug, Parser)]
#[command(version, about = "Local control surface for many Claude Code sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve the pending page on 127.0.0.1.
    Serve {
        #[arg(long, default_value = "7357")]
        port: NonZeroU16,
        /// Also open the sign-in URL in the default browser (`xdg-open`).
        #[arg(long)]
        open: bool,
    },
    /// Claude Code hook entry point: append the payload on stdin to the event log.
    Hook {
        /// Print the settings.json fragment that registers this binary, and exit.
        #[arg(long)]
        print_settings: bool,
    },
    /// Run the derivations over historical transcripts and print hit-rate tables.
    Replay {
        /// Only consider activity on or after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<jiff::civil::Date>,
    },
    /// Write a markdown file of sessions and resume commands.
    Snapshot {
        #[arg(long)]
        out: PathBuf,
    },
    /// Own a ticket (ABC-123), a rollout (rollout:<name>) or a topic (topic:<name>): other
    /// sessions are told to send you what they have instead of acting on it.
    Claim {
        #[arg(required = true, value_parser = Subject::parse)]
        subjects: Vec<Subject>,
        /// Take it over from a session that may still be running. Only when the user said so.
        #[arg(long)]
        take: bool,
        #[command(flatten)]
        actor: Actor,
    },
    /// Give up what you own.
    Release {
        #[arg(required = true, value_parser = Subject::parse)]
        subjects: Vec<Subject>,
        #[command(flatten)]
        actor: Actor,
    },
    /// Who owns a subject, or, with none, everything owned, rolling out and waited on.
    Who {
        #[arg(value_parser = Subject::parse)]
        subject: Option<Subject>,
    },
    /// Say what this session is idle for and what it does once that is over.
    #[command(group(clap::ArgGroup::new("target").required(true)
        .args(["waited", "until", "for_", "clear"])))]
    Wait {
        /// rollout:<name>: wait for that rollout's next finish. ABC-123 or topic:<name>: wait
        /// until the session that owns it releases it.
        #[arg(value_parser = Subject::parse)]
        waited: Option<Subject>,
        /// Wait until this time: 2026-11-06T09:00 (local) or an RFC 3339 timestamp.
        #[arg(long, value_parser = parse_until)]
        until: Option<Instant>,
        /// Wait for something only words can say ("the vendor to pick a time").
        #[arg(long = "for", value_parser = Note::parse)]
        for_: Option<Note>,
        /// With --until or --for: the ticket the wait is about (ABC-123). The wait is over as
        /// soon as Jira resolves it, whoever does, even before the time comes.
        #[arg(long, value_parser = parse_ticket, conflicts_with_all = ["waited", "clear"])]
        ticket: Option<JiraKey>,
        /// What this session does when the wait is over.
        #[arg(long, value_parser = Note::parse, required_unless_present = "clear")]
        then: Option<Note>,
        /// This session is no longer waiting.
        #[arg(long, conflicts_with = "then")]
        clear: bool,
        #[command(flatten)]
        actor: Actor,
    },
    /// Record a rollout's start and finish, so sessions waiting on it are told.
    Rollout {
        #[command(subcommand)]
        step: RolloutStep,
    },
    /// Put off a shipped ticket's close check until its exit criteria can be checked: the
    /// Follow-ups page does not offer it before then.
    Defer {
        #[arg(value_parser = parse_ticket)]
        ticket: JiraKey,
        /// Not before this time: 2026-11-06T09:00 (local) or an RFC 3339 timestamp.
        #[arg(long, value_parser = parse_until)]
        until: Instant,
        /// What to check then ("a week of error rates under 0.1%").
        #[arg(long, value_parser = Note::parse)]
        check: Note,
        #[command(flatten)]
        actor: Actor,
    },
    /// Claude Code `PreToolUse` hook: refuse a Jira write to a ticket another running session
    /// owns.
    Guard,
}

#[derive(Debug, Subcommand)]
pub enum RolloutStep {
    /// A rollout of <name> begins in this session.
    Start {
        #[arg(value_parser = Slug::parse)]
        name: Slug,
        /// A ticket this rollout ships; repeat for each.
        #[arg(long = "ships", value_parser = Subject::parse)]
        ships: Vec<Subject>,
        #[command(flatten)]
        actor: Actor,
    },
    /// The rollout of <name> this session ran has ended.
    Finish {
        #[arg(value_parser = Slug::parse)]
        name: Slug,
        #[arg(long, value_enum)]
        outcome: Outcome,
        #[arg(long, value_parser = Note::parse)]
        note: Option<Note>,
        #[command(flatten)]
        actor: Actor,
    },
}

/// The session a ledger command acts for.
#[derive(Debug, Clone, clap::Args)]
pub struct Actor {
    /// The session to act for; defaults to the Claude Code session running this command.
    #[arg(long, value_parser = SessionId::parse)]
    session: Option<SessionId>,
}

impl Actor {
    /// The flag, else the session Claude Code runs this command in.
    fn session(self) -> Option<SessionId> {
        self.session.or_else(|| {
            std::env::var(SESSION_ENV)
                .ok()
                .and_then(|id| SessionId::parse(&id).ok())
        })
    }
}

/// Set by Claude Code in the environment of every command a session runs.
const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// `--until`: an RFC 3339 timestamp, or a civil date and time in the local zone.
fn parse_until(text: &str) -> Result<Instant, String> {
    text.parse::<Instant>()
        .or_else(|_| {
            text.replace(' ', "T")
                .parse::<jiff::civil::DateTime>()
                .and_then(|civil| civil.to_zoned(jiff::tz::TimeZone::system()))
                .map(|zoned| zoned.timestamp())
        })
        .map_err(|_| {
            format!("{text:?} is not a time: write 2026-11-06T09:00 or 2026-11-06T14:00:00Z")
        })
}

fn parse_ticket(text: &str) -> Result<JiraKey, String> {
    JiraKey::parse(text).ok_or_else(|| format!("{text:?} is not a ticket key: write ABC-123"))
}

/// What `conn hook` does, decided once from the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    Log,
    PrintSettings,
}

impl From<bool> for HookMode {
    /// The `--print-settings` flag.
    fn from(print_settings: bool) -> Self {
        if print_settings {
            Self::PrintSettings
        } else {
            Self::Log
        }
    }
}

/// The subcommands Claude Code runs as hooks, whose failures must never reach the agent: a
/// `PreToolUse` hook that exits 2 blocks the tool call.
const HOOKS: [&str; 2] = ["hook", "guard"];

/// What the command line asked for, or why it asked for nothing.
#[derive(Debug)]
pub enum Invocation {
    Run(Command),
    /// `conn hook` or `conn guard` was given arguments it does not take. A hook runs inside
    /// agent turns and must never fail one, so this is a line on stderr and a normal exit, not
    /// the usage error (exit 2) every other subcommand gets.
    HookRefused {
        hook: &'static str,
        problem: String,
    },
    /// Anything else the parser stopped on: usage errors, `--help`, `--version`. Printed and
    /// exited by the parser with its own code.
    Parser(clap::Error),
}

/// Parses `args` (the program name first).
pub fn invocation(args: &[OsString]) -> Invocation {
    let hook = args
        .get(1)
        .and_then(|arg| HOOKS.into_iter().find(|hook| arg == *hook));
    match (Cli::try_parse_from(args), hook) {
        (Ok(cli), _) => Invocation::Run(cli.command),
        (Err(error), Some(hook)) => {
            let problem = error
                .to_string()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            Invocation::HookRefused { hook, problem }
        }
        (Err(error), None) => Invocation::Parser(error),
    }
}

pub async fn run() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let command = match invocation(&args) {
        Invocation::Run(command) => command,
        Invocation::HookRefused { hook, problem } => {
            // A failure to write the line has nowhere further to go.
            let _ = writeln!(std::io::stderr(), "conn {hook}: {problem}; did nothing");
            return Ok(());
        }
        Invocation::Parser(error) => error.exit(),
    };
    // The hooks run inside agent turns and never fail them, so they must not depend on
    // `RUST_LOG` being valid; they report on stderr themselves.
    if !is_hook(&command) {
        init_tracing()?;
    }
    dispatch(command).await
}

fn is_hook(command: &Command) -> bool {
    match command {
        Command::Hook { print_settings } => HookMode::from(*print_settings) == HookMode::Log,
        Command::Guard => true,
        Command::Serve { .. }
        | Command::Replay { .. }
        | Command::Snapshot { .. }
        | Command::Claim { .. }
        | Command::Release { .. }
        | Command::Who { .. }
        | Command::Wait { .. }
        | Command::Rollout { .. }
        | Command::Defer { .. } => false,
    }
}

pub async fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Serve { port, open } => serve(port, sign_in(open)).await?,
        Command::Hook { print_settings } => match HookMode::from(print_settings) {
            HookMode::Log => events::run_hook(),
            HookMode::PrintSettings => print_settings_fragment()?,
        },
        Command::Replay { since } => replay::run(since)?,
        Command::Snapshot { out } => snapshot::run(&out).await?,
        Command::Guard => ledger::guard::run(),
        Command::Claim { .. }
        | Command::Release { .. }
        | Command::Who { .. }
        | Command::Wait { .. }
        | Command::Rollout { .. }
        | Command::Defer { .. } => run_ledger(command)?,
    }
    Ok(())
}

/// A ledger command, from the parsed flags to the printed answer.
fn run_ledger(command: Command) -> anyhow::Result<()> {
    let (request, actor) = match command {
        Command::Claim {
            subjects,
            take,
            actor,
        } => {
            let if_owned = if take { IfOwned::Take } else { IfOwned::Refuse };
            (Request::Claim { subjects, if_owned }, Some(actor))
        }
        Command::Release { subjects, actor } => (Request::Release { subjects }, Some(actor)),
        Command::Who { subject } => (Request::Who { subject }, None),
        Command::Wait {
            waited,
            until,
            for_,
            ticket,
            then,
            clear,
            actor,
        } => (
            wait_request(waited, until, for_, ticket, then, clear)?,
            Some(actor),
        ),
        Command::Rollout { step } => match step {
            RolloutStep::Start { name, ships, actor } => (
                Request::RolloutStart {
                    rollout: name,
                    ships,
                },
                Some(actor),
            ),
            RolloutStep::Finish {
                name,
                outcome,
                note,
                actor,
            } => (
                Request::RolloutFinish {
                    rollout: name,
                    outcome,
                    note,
                },
                Some(actor),
            ),
        },
        Command::Defer {
            ticket,
            until,
            check,
            actor,
        } => (
            Request::Defer {
                ticket,
                until,
                check,
            },
            Some(actor),
        ),
        Command::Serve { .. }
        | Command::Hook { .. }
        | Command::Replay { .. }
        | Command::Snapshot { .. }
        | Command::Guard => anyhow::bail!("not a ledger command"),
    };
    let config = Config::load()?;
    let sessions = Sessions::from_registry(&config.paths.claude_dir);
    let zone = jiff::tz::TimeZone::system();
    let context = LedgerContext {
        sessions: &sessions,
        me: actor.and_then(Actor::session),
        now: Instant::now(),
        zone: &zone,
    };
    let path = config.paths.state_dir.join(ledger::LEDGER_FILE);
    let answer = ledger::command::run(&request, &path, &context)?;
    writeln!(std::io::stdout(), "{answer}")?;
    Ok(())
}

/// The one wait `conn wait`'s flags name; clap has already required exactly one target.
fn wait_request(
    waited: Option<Subject>,
    until: Option<Instant>,
    for_: Option<Note>,
    ticket: Option<JiraKey>,
    then: Option<Note>,
    clear: bool,
) -> anyhow::Result<Request> {
    if clear {
        return Ok(Request::Unwait);
    }
    let then = then.context("--then is required: say what this session does next")?;
    let on = match (waited, until, for_) {
        (Some(Subject::Rollout(name)), None, None) => WaitOn::Rollout(name),
        (Some(subject @ (Subject::Ticket(_) | Subject::Topic(_))), None, None) => {
            WaitOn::Subject(subject)
        }
        (None, Some(at), None) => WaitOn::Until(at),
        (None, None, Some(what)) => WaitOn::For(what),
        (_, _, _) => anyhow::bail!("name one thing to wait on"),
    };
    Ok(Request::Wait { on, then, ticket })
}

/// Starts the poller and serves its model, and the changes the human may make to what they
/// own, until interrupted.
async fn serve(port: NonZeroU16, sign_in: SignIn) -> anyhow::Result<()> {
    let config = Config::load()?;
    // Before anything reads or writes the state directory: one server per directory.
    let lock = state::lock::acquire(&config.paths.state_dir)?;
    let runner: Arc<dyn CommandRunner> = Arc::new(TokioRunner);
    let handle = state::start_with(config.clone(), Arc::clone(&runner))?;
    web::serve(port, sign_in, handle, &config, runner).await?;
    drop(lock);
    Ok(())
}

/// The `--open` flag of `conn serve`.
const fn sign_in(open: bool) -> SignIn {
    if open {
        SignIn::PrintAndOpen
    } else {
        SignIn::Print
    }
}

/// Logs go to stderr: stdout belongs to the command's own output, and a hook's stdout is
/// read by Claude Code.
fn init_tracing() -> anyhow::Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()
        .context("invalid RUST_LOG")?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    Ok(())
}

fn print_settings_fragment() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine this binary's path")?;
    let patch = SettingsPatch::registering(&exe)?;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &patch)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("conn").chain(args.iter().copied()))
    }

    fn invoke(args: &[&str]) -> Invocation {
        let args: Vec<OsString> = std::iter::once("conn")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        invocation(&args)
    }

    /// Whatever the hook is given, it parses to a command or to a refusal that exits 0; every
    /// other subcommand keeps the parser's usage error.
    /// Mutation: parse with `Cli::parse` (the refusals become parser exits), or test for the
    /// hook anywhere but the first argument.
    #[test]
    fn a_hook_that_cannot_parse_is_refused_on_one_line_not_a_usage_error() {
        assert!(matches!(
            invoke(&["hook"]),
            Invocation::Run(Command::Hook { .. })
        ));
        for args in [
            &["hook", "--nonsense"][..],
            &["hook", "extra"],
            &["hook", "--print-settings=yes"],
            &["hook", "--help"],
            &["guard", "--nonsense"],
            &["guard", "extra"],
        ] {
            let Invocation::HookRefused { hook, problem } = invoke(args) else {
                panic!("{args:?} was not refused");
            };
            assert_eq!(args.first(), Some(&hook));
            assert!(
                !problem.is_empty() && !problem.contains('\n'),
                "{problem:?}"
            );
        }
        assert!(matches!(
            invoke(&["serve", "--port", "0"]),
            Invocation::Parser(_)
        ));
        assert!(matches!(invoke(&["nonsense"]), Invocation::Parser(_)));
        assert!(matches!(invoke(&[]), Invocation::Parser(_)));
    }

    #[test]
    fn serve_defaults_to_the_documented_port() {
        let command = parse(&["serve"]).unwrap().command;
        assert!(matches!(command, Command::Serve { port, open: false } if port.get() == 7357));
    }

    /// Mutation: invert `sign_in`, or drop the flag's `#[arg(long)]`.
    #[test]
    fn serve_opens_the_browser_only_when_asked() {
        let sign_in_of = |args: &[&str]| match parse(args).unwrap().command {
            Command::Serve { open, .. } => Some(sign_in(open)),
            Command::Hook { .. }
            | Command::Replay { .. }
            | Command::Snapshot { .. }
            | Command::Claim { .. }
            | Command::Release { .. }
            | Command::Who { .. }
            | Command::Wait { .. }
            | Command::Rollout { .. }
            | Command::Defer { .. }
            | Command::Guard => None,
        };
        assert_eq!(sign_in_of(&["serve"]), Some(SignIn::Print));
        assert_eq!(sign_in_of(&["serve", "--open"]), Some(SignIn::PrintAndOpen));
    }

    #[test]
    fn serve_refuses_port_zero() {
        assert!(parse(&["serve", "--port", "0"]).is_err());
    }

    #[test]
    fn hook_flag_selects_the_mode() {
        let mode = |args: &[&str]| {
            let command = parse(args).unwrap().command;
            if let Command::Hook { print_settings } = command {
                Some(HookMode::from(print_settings))
            } else {
                None
            }
        };
        assert_eq!(mode(&["hook"]), Some(HookMode::Log));
        assert_eq!(
            mode(&["hook", "--print-settings"]),
            Some(HookMode::PrintSettings)
        );
    }

    #[test]
    fn only_the_hooks_skip_tracing_setup() {
        let skips = |args: &[&str]| is_hook(&parse(args).unwrap().command);
        assert!(skips(&["hook"]));
        assert!(skips(&["guard"]));
        assert!(!skips(&["hook", "--print-settings"]));
        assert!(!skips(&["serve"]));
        assert!(!skips(&["who"]));
    }

    /// Each wait names exactly one thing and says what follows it: a rollout, a ticket or
    /// topic, a time (local or a timestamp) or words.
    /// Mutation: drop the `target` group (a bare `wait --then x` parses), drop
    /// `required_unless_present` (a wait with no `--then` parses), or read a ticket as a rollout.
    #[test]
    fn a_wait_names_one_thing_and_what_comes_next() {
        let request = |args: &[&str]| match parse(args).map(|cli| cli.command) {
            Ok(Command::Wait {
                waited,
                until,
                for_,
                ticket,
                then,
                clear,
                ..
            }) => wait_request(waited, until, for_, ticket, then, clear).map_err(|e| e.to_string()),
            Ok(_) => Err("not a wait".to_owned()),
            Err(error) => Err(error.to_string()),
        };
        assert!(matches!(
            request(&["wait", "rollout:checkout", "--then", "re-run E2E"]),
            Ok(Request::Wait {
                on: WaitOn::Rollout(_),
                ..
            })
        ));
        assert!(matches!(
            request(&[
                "wait",
                "--until",
                "2026-11-06T09:00",
                "--then",
                "read results"
            ]),
            Ok(Request::Wait {
                on: WaitOn::Until(_),
                ..
            })
        ));
        assert!(matches!(
            request(&["wait", "--until", "2026-11-06T14:00:00Z", "--then", "x"]),
            Ok(Request::Wait {
                on: WaitOn::Until(_),
                ..
            })
        ));
        assert!(matches!(
            request(&["wait", "--for", "the vendor to pick a time", "--then", "x"]),
            Ok(Request::Wait {
                on: WaitOn::For(_),
                ..
            })
        ));
        for waited in ["ABC-123", "topic:vendor-follow-up"] {
            assert!(matches!(
                request(&["wait", waited, "--then", "x"]),
                Ok(Request::Wait { on: WaitOn::Subject(s), .. }) if s.to_string() == waited
            ));
        }
        assert_eq!(request(&["wait", "--clear"]), Ok(Request::Unwait));
        assert!(matches!(
            request(&["wait", "--until", "2026-11-06T09:00", "--ticket", "ABC-12", "--then", "x"]),
            Ok(Request::Wait { on: WaitOn::Until(_), ticket: Some(t), .. }) if t.to_string() == "ABC-12"
        ));
        for refused in [
            &["wait", "--then", "x"][..],
            &["wait", "rollout:checkout"],
            &["wait", "ABC-1", "--ticket", "ABC-2", "--then", "x"],
            &[
                "wait",
                "--until",
                "2026-11-06T09:00",
                "--ticket",
                "nope",
                "--then",
                "x",
            ],
            &[
                "wait",
                "rollout:checkout",
                "--until",
                "2026-11-06T09:00",
                "--then",
                "x",
            ],
            &["wait", "--until", "tomorrow", "--then", "x"],
            &["wait", "--clear", "--then", "x"],
        ] {
            assert!(request(refused).is_err(), "{refused:?} was accepted");
        }
    }

    #[test]
    fn replay_since_is_a_date_and_snapshot_needs_out() {
        assert!(parse(&["replay", "--since", "2026-10-01"]).is_ok());
        assert!(parse(&["replay", "--since", "yesterday"]).is_err());
        assert!(parse(&["snapshot"]).is_err());
        assert!(parse(&["snapshot", "--out", "/tmp/s.md"]).is_ok());
    }

    /// A deferral names one ticket, a time and what to check then; anything else is a usage
    /// error.
    /// Mutation: make `--check` optional (red), or accept a topic as the ticket (red).
    #[test]
    fn a_deferral_names_a_ticket_a_time_and_the_check() {
        let ok = parse(&[
            "defer",
            "ABC-1",
            "--until",
            "2026-11-20T09:00:00Z",
            "--check",
            "a week of clean error rates",
        ]);
        assert!(
            matches!(
                ok,
                Ok(Cli { command: Command::Defer { ref ticket, until, ref check, .. } })
                    if ticket.to_string() == "ABC-1"
                        && until == "2026-11-20T09:00:00Z".parse::<Instant>().unwrap()
                        && check.as_str() == "a week of clean error rates"
            ),
            "{ok:?}"
        );
        assert!(parse(&["defer", "ABC-1", "--until", "2026-11-20T09:00:00Z"]).is_err());
        assert!(
            parse(&[
                "defer",
                "topic:x",
                "--until",
                "2026-11-20T09:00:00Z",
                "--check",
                "x"
            ])
            .is_err()
        );
        assert!(parse(&["defer", "ABC-1", "--until", "next week", "--check", "x"]).is_err());
    }
}
