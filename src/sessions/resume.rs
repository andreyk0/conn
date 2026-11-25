//! The command that resumes a session, and what it has to repeat.
//!
//! `claude --resume <id>` restores the model, the agent and the permission mode, so passing a
//! launch flag for any of those would override what the session had moved to since: a session
//! launched with `--model` and switched with `/model` would come back on the launch model. Flags
//! that open or pick a different session (`-w`, `--from-pr`, `-p`, ...) are wrong outright. So
//! every launch flag has a rule in [`RULES`], and the command adds:
//!
//! - bypass mode, which a resume deliberately does not restore, taken from the permission mode
//!   the transcript last recorded: a session that left bypass mode does not re-enter it, and a
//!   session whose process is gone still gets it back;
//! - the launch-time configuration no transcript records: system prompts, tool and MCP
//!   restrictions, settings and plugins (dropping a tool restriction while keeping bypass mode
//!   would resume the session with more reach than it had);
//! - directories added mid-session with `/add-dir`, which a resume does not restore either.
//!
//! A launch flag with no rule is neither carried nor silently dropped: the command reports it
//! by name. Sources: the Claude Code docs, "sessions: what a resumed session restores" and the
//! CLI reference.

use std::iter::Peekable;
use std::path::PathBuf;

use super::launch::LaunchArgs;
use crate::types::{ResumeCommand, ResumeFlag, ResumeWarning, SessionId};
use crate::util::path::NonUtf8Path;

/// Everything the command is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeFacts {
    pub session_id: SessionId,
    /// Where the session works now (the registry's or the transcript's `cwd`, never a decoded
    /// project directory name).
    pub cwd: PathBuf,
    pub launch_args: LaunchArgs,
    /// The permission mode the transcript last recorded.
    pub last_permission_mode: Option<String>,
    /// Directories the transcript confirms were added with `/add-dir`.
    pub added_dirs: Vec<PathBuf>,
}

const BYPASS_MODE: &str = "bypassPermissions";
const BYPASS_FLAG: &str = "--dangerously-skip-permissions";
/// Puts bypass mode back in the Shift+Tab cycle without starting in it.
const ALLOW_BYPASS_FLAG: &str = "--allow-dangerously-skip-permissions";
const PERMISSION_MODE_FLAG: &str = "--permission-mode";

/// How many values follow a flag. A variadic flag takes every following word up to the next
/// flag, which is how the CLI itself reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    None,
    One,
    Variadic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Leave it off: a resume restores its effect, it opens or picks a session, it only
    /// shapes output, or the command derives it from the transcript instead.
    Drop,
    /// `--chrome`: the one carried flag with a typed [`ResumeFlag`] of its own.
    Chrome,
    /// `--add-dir`: each value becomes a typed [`ResumeFlag::AddDir`].
    AddDir,
    /// Repeat it, with its values, because a resume does not restore it.
    Carry(Arity),
}

const RULES: &[(&str, Rule)] = &[
    ("--add-dir", Rule::AddDir),
    ("--mcp-config", Rule::Carry(Arity::Variadic)),
    ("--strict-mcp-config", Rule::Carry(Arity::None)),
    ("--settings", Rule::Carry(Arity::One)),
    ("--setting-sources", Rule::Carry(Arity::One)),
    ("--plugin-dir", Rule::Carry(Arity::One)),
    ("--plugin-url", Rule::Carry(Arity::One)),
    ("--agents", Rule::Carry(Arity::One)),
    ("--fallback-model", Rule::Carry(Arity::One)),
    ("--system-prompt", Rule::Carry(Arity::One)),
    ("--system-prompt-file", Rule::Carry(Arity::One)),
    ("--append-system-prompt", Rule::Carry(Arity::One)),
    ("--append-system-prompt-file", Rule::Carry(Arity::One)),
    ("--tools", Rule::Carry(Arity::Variadic)),
    ("--allowedTools", Rule::Carry(Arity::Variadic)),
    ("--allowed-tools", Rule::Carry(Arity::Variadic)),
    ("--disallowedTools", Rule::Carry(Arity::Variadic)),
    ("--disallowed-tools", Rule::Carry(Arity::Variadic)),
    ("--chrome", Rule::Chrome),
    ("--no-chrome", Rule::Carry(Arity::None)),
    ("--ide", Rule::Carry(Arity::None)),
    (BYPASS_FLAG, Rule::Drop),
    (ALLOW_BYPASS_FLAG, Rule::Drop),
    ("--model", Rule::Drop),
    (PERMISSION_MODE_FLAG, Rule::Drop),
    ("--agent", Rule::Drop),
    ("--resume", Rule::Drop),
    ("-r", Rule::Drop),
    ("--continue", Rule::Drop),
    ("-c", Rule::Drop),
    ("--fork-session", Rule::Drop),
    ("--session-id", Rule::Drop),
    ("--from-pr", Rule::Drop),
    ("--teleport", Rule::Drop),
    ("--worktree", Rule::Drop),
    ("-w", Rule::Drop),
    ("--name", Rule::Drop),
    ("-n", Rule::Drop),
    ("--bg", Rule::Drop),
    ("--background", Rule::Drop),
    ("--print", Rule::Drop),
    ("-p", Rule::Drop),
    ("--output-format", Rule::Drop),
    ("--input-format", Rule::Drop),
    ("--verbose", Rule::Drop),
    ("--debug", Rule::Drop),
    ("-d", Rule::Drop),
    ("--debug-file", Rule::Drop),
    ("--tmux", Rule::Drop),
];

fn rule_for(name: &str) -> Option<Rule> {
    RULES
        .iter()
        .find_map(|(flag, rule)| (*flag == name).then_some(*rule))
}

/// The command that resumes the session, with the reasons it may fall short.
///
/// Fails only when a path in the facts is not valid UTF-8, since the command is text a human
/// pastes and a lossy rendering would name a different directory.
pub fn resume_command(facts: &ResumeFacts) -> Result<ResumeCommand, NonUtf8Path> {
    let argv: &[String] = match &facts.launch_args {
        LaunchArgs::Known(argv) => argv,
        LaunchArgs::Unknown(_) => &[],
    };
    let launch = read_launch(argv);
    let mode = facts.last_permission_mode.as_deref();

    let flags: Vec<ResumeFlag> = bypass_flag(argv, mode)
        .into_iter()
        .chain(launch.flags)
        .collect();
    let flags = with_added_dirs(flags, &facts.added_dirs);

    ResumeCommand::new(facts.cwd.clone(), flags, facts.session_id.clone())
        .map(|command| command.with_warnings(warnings(facts, launch.unclassified)))
}

fn warnings(facts: &ResumeFacts, unclassified: Vec<String>) -> Vec<ResumeWarning> {
    let launch_known = matches!(facts.launch_args, LaunchArgs::Known(_));
    let launch = match &facts.launch_args {
        LaunchArgs::Unknown(why) => Some(ResumeWarning::LaunchFlagsUnknown {
            reason: why.to_string(),
        }),
        LaunchArgs::Known(_) => None,
    };
    let mode = match (&facts.last_permission_mode, launch_known) {
        (Some(_), _) => None,
        (None, true) => Some(ResumeWarning::PermissionModeFollowsLaunch),
        (None, false) => Some(ResumeWarning::PermissionModeUnknown),
    };
    let flags =
        (!unclassified.is_empty()).then_some(ResumeWarning::UnclassifiedFlags(unclassified));
    launch.into_iter().chain(mode).chain(flags).collect()
}

/// Bypass mode is requested when the session was last in it. A session launched able to
/// bypass that has since left it gets bypass back in its Shift+Tab cycle but does not start
/// in it. With no mode recorded, the launch flags decide.
fn bypass_flag(argv: &[String], recorded_mode: Option<&str>) -> Option<ResumeFlag> {
    let launched_in_bypass = argv.iter().any(|a| a == BYPASS_FLAG)
        || launch_permission_mode(argv).is_some_and(|m| m == BYPASS_MODE);
    let launched_able_to_bypass = launched_in_bypass || argv.iter().any(|a| a == ALLOW_BYPASS_FLAG);
    match recorded_mode {
        Some(BYPASS_MODE) => Some(ResumeFlag::SkipPermissions),
        None if launched_in_bypass => Some(ResumeFlag::SkipPermissions),
        Some(_) | None => launched_able_to_bypass.then_some(ResumeFlag::AllowSkipPermissions),
    }
}

/// The mode `--permission-mode` named at launch, in either spelling.
fn launch_permission_mode(argv: &[String]) -> Option<&str> {
    let inline = format!("{PERMISSION_MODE_FLAG}=");
    argv.iter()
        .position(|a| a == PERMISSION_MODE_FLAG)
        .and_then(|at| argv.get(at + 1))
        .map(String::as_str)
        .or_else(|| argv.iter().find_map(|a| a.strip_prefix(&inline)))
}

/// Appends the confirmed directories the launch flags did not already carry.
fn with_added_dirs(mut flags: Vec<ResumeFlag>, added: &[PathBuf]) -> Vec<ResumeFlag> {
    for dir in added {
        let carried = flags
            .iter()
            .any(|f| matches!(f, ResumeFlag::AddDir(existing) if existing == dir));
        if !carried {
            flags.push(ResumeFlag::AddDir(dir.clone()));
        }
    }
    flags
}

struct LaunchFlags {
    /// The carried flags with their values, in launch order.
    flags: Vec<ResumeFlag>,
    /// Flags with no rule, by name.
    unclassified: Vec<String>,
}

/// Sorts a launch argv's flags by rule. `argv[0]` is the program, whatever it was invoked as.
/// Words that are not flags are values or a prompt, and are skipped.
fn read_launch(argv: &[String]) -> LaunchFlags {
    let mut flags = Vec::new();
    let mut unclassified = Vec::new();
    let mut words = argv.iter().skip(1).peekable();
    while let Some(arg) = words.next() {
        if !arg.starts_with('-') {
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg.as_str(), None),
        };
        match rule_for(name) {
            None => unclassified.push(name.to_owned()),
            Some(Rule::Drop) => {}
            Some(Rule::Chrome) if inline.is_none() => flags.push(ResumeFlag::Chrome),
            Some(Rule::Chrome) => flags.push(ResumeFlag::Launch(vec![arg.clone()])),
            Some(Rule::AddDir) => {
                let dirs = match inline {
                    Some(value) => vec![value.to_owned()],
                    None => values(&mut words, Arity::Variadic),
                };
                flags.extend(dirs.into_iter().map(|d| ResumeFlag::AddDir(d.into())));
            }
            Some(Rule::Carry(arity)) => {
                let mut carried = vec![arg.clone()];
                if inline.is_none() {
                    carried.extend(values(&mut words, arity));
                }
                flags.push(ResumeFlag::Launch(carried));
            }
        }
    }
    LaunchFlags {
        flags,
        unclassified,
    }
}

/// The values after a flag. The first is required, so the CLI takes the next word even when
/// it starts with `-` (a system prompt beginning with a bullet); only further values of a
/// variadic flag stop at the next flag.
fn values<'a>(words: &mut Peekable<impl Iterator<Item = &'a String>>, arity: Arity) -> Vec<String> {
    let mut taken = Vec::new();
    if arity == Arity::None {
        return taken;
    }
    let Some(first) = words.next() else {
        return taken;
    };
    taken.push(first.clone());
    if arity == Arity::Variadic {
        while let Some(next) = words.next_if(|w| !w.starts_with('-')) {
            taken.push(next.clone());
        }
    }
    taken
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::sessions::launch::LaunchUnknown;

    const CASES: &str = include_str!("../../tests/fixtures/sessions/resume_cases.json");
    const ID: &str = "00000000-0000-4000-8000-000000000042";

    #[derive(Deserialize)]
    struct Case {
        name: String,
        /// `null`: the process is gone, so the launch flags are unknown.
        launch: Option<Vec<String>>,
        mode: Option<String>,
        added_dirs: Vec<PathBuf>,
        cwd: PathBuf,
        expected: String,
        warnings: Vec<String>,
    }

    impl Case {
        fn facts(&self) -> ResumeFacts {
            ResumeFacts {
                session_id: SessionId::parse(ID).unwrap(),
                cwd: self.cwd.clone(),
                launch_args: self.launch.clone().map_or(
                    LaunchArgs::Unknown(LaunchUnknown::Dormant),
                    LaunchArgs::Known,
                ),
                last_permission_mode: self.mode.clone(),
                added_dirs: self.added_dirs.clone(),
            }
        }
    }

    fn cases() -> Vec<Case> {
        serde_json::from_str(CASES).unwrap()
    }

    /// The table is hand-written in the fixture; each row names the rule it pins. Mutations,
    /// by row family: swap `Drop` for `Carry` on `--model` (model rows); invert the bypass
    /// guard (bypass rows); take `--add-dir` values without the dash stop (add-dir rows);
    /// skip the first-value-is-required rule (bullet row); drop the `unclassified` push
    /// (unknown-flag row); render without quoting (quoting rows).
    #[test]
    fn the_resume_table_holds() {
        let cases = cases();
        assert!(cases.len() >= 20, "the table shrank to {}", cases.len());
        for case in cases {
            let command = resume_command(&case.facts()).unwrap();
            assert_eq!(command.to_string(), case.expected, "{}", case.name);
            let warnings: Vec<String> =
                command.warnings().iter().map(ToString::to_string).collect();
            assert_eq!(warnings, case.warnings, "{} (warnings)", case.name);
        }
    }

    /// Mutation: drop the dedupe in `with_added_dirs` -> `/a` appears twice -> red.
    #[test]
    fn a_confirmed_dir_the_launch_already_carried_is_not_repeated() {
        let facts = ResumeFacts {
            session_id: SessionId::parse(ID).unwrap(),
            cwd: "/p".into(),
            launch_args: LaunchArgs::Known(vec!["claude".into(), "--add-dir".into(), "/a".into()]),
            last_permission_mode: Some("default".into()),
            added_dirs: vec!["/a".into(), "/b".into(), "/b".into()],
        };
        assert_eq!(
            resume_command(&facts).unwrap().flags(),
            [
                ResumeFlag::AddDir("/a".into()),
                ResumeFlag::AddDir("/b".into())
            ]
        );
    }

    /// Mutation: render with `to_string_lossy` -> the bad path prints a replacement char -> red.
    #[test]
    fn a_path_that_is_not_utf8_refuses_a_command() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let facts = ResumeFacts {
            session_id: SessionId::parse(ID).unwrap(),
            cwd: PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")),
            launch_args: LaunchArgs::Unknown(LaunchUnknown::Dormant),
            last_permission_mode: None,
            added_dirs: vec![],
        };
        assert!(resume_command(&facts).is_err());
    }

    /// The table has one row per rule, so a rule added to `RULES` without a row is noticed.
    #[test]
    fn every_carried_rule_has_a_row_in_the_table() {
        let table = CASES;
        for (flag, rule) in RULES {
            if matches!(rule, Rule::Carry(_) | Rule::AddDir | Rule::Chrome) {
                assert!(table.contains(flag), "no table row exercises {flag}");
            }
        }
    }
}
