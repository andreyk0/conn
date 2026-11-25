//! `conn snapshot`: a markdown file of sessions and their resume commands, so a crash costs a
//! few pastes.
//!
//! The file exists to outlive the machine's memory, so a path on a memory-backed filesystem
//! is refused before anything is read.
#![allow(
    clippy::print_stdout,
    reason = "snapshot reports the written path on stdout"
)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::tz::TimeZone;
use rustix::fs::statfs;

use crate::config::{Config, ConfigError};
use crate::model::{Model, ScanProgress, SessionView, SourceState};
use crate::state::{StartError, read_once};
use crate::types::{Activity, ActivityKind, Instant};
use crate::util::fs::write_atomic;
use crate::util::runner::TokioRunner;

/// `statfs` `f_type` of a tmpfs.
const TMPFS_MAGIC: u64 = 0x0102_1994;
/// `statfs` `f_type` of a ramfs.
const RAMFS_MAGIC: u64 = 0x8584_58f6;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(
        "REFUSED: {} is on a {kind} filesystem, which does not survive a reboot or a crash; \
         choose a path on disk",
        path.display()
    )]
    MemoryBacked { path: PathBuf, kind: &'static str },
    #[error("cannot tell what filesystem {} is on: {source}", path.display())]
    Filesystem {
        path: PathBuf,
        source: rustix::io::Errno,
    },
    #[error("cannot write snapshot {}: {source}", path.display())]
    Unwritable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Start(#[from] StartError),
}

/// Reads every source once and writes the snapshot to `out`, atomically.
pub async fn run(out: &Path) -> Result<(), SnapshotError> {
    refuse_memory_backed(out)?;
    let config = Config::load()?;
    let model = read_once(config, Arc::new(TokioRunner)).await?;
    let text = render(&model, &TimeZone::system());
    write_atomic(out, text.as_bytes()).map_err(|source| SnapshotError::Unwritable {
        path: out.to_path_buf(),
        source,
    })?;
    println!(
        "wrote {} ({} sessions, {} warnings)",
        out.display(),
        model.sessions.len() + model.dormant.len(),
        warnings(&model).len()
    );
    Ok(())
}

/// Fails when the directory `out` would be created in is on a tmpfs or a ramfs. The file
/// itself need not exist; a directory that does not exist is an error of its own.
pub fn refuse_memory_backed(out: &Path) -> Result<(), SnapshotError> {
    let dir = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        Some(_) | None => Path::new("."),
    };
    let found = statfs(dir).map_err(|source| SnapshotError::Filesystem {
        path: dir.to_path_buf(),
        source,
    })?;
    match u64::try_from(found.f_type) {
        Ok(TMPFS_MAGIC) => Err(SnapshotError::MemoryBacked {
            path: out.to_path_buf(),
            kind: "tmpfs",
        }),
        Ok(RAMFS_MAGIC) => Err(SnapshotError::MemoryBacked {
            path: out.to_path_buf(),
            kind: "ramfs",
        }),
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Everything in the model that makes the snapshot less than complete.
fn warnings(model: &Model) -> Vec<String> {
    let health = &model.health;
    let mut out = Vec::new();
    let mut source = |name: &str, state: &SourceState| match state {
        SourceState::Ok { .. } => {}
        SourceState::Failed { reason, .. } => out.push(format!("{name}: {reason}")),
        SourceState::NeverRead => out.push(format!("{name}: was not read")),
        SourceState::NeverWritten { path, .. } => {
            out.push(format!("{name}: nothing has written {}", path.display()));
        }
    };
    source("the supervisor's listing", &health.listing);
    source("the registry", &health.registry);
    source("transcript discovery", &health.transcripts.discovery);
    // The event log is not needed to list sessions and resume them.
    if let ScanProgress::InProgress { scanned, total } = health.transcripts.progress {
        out.push(format!(
            "transcripts: only {scanned} of {total} were read, so sessions are missing"
        ));
    }
    if health.follow_faults.transcripts > 0 {
        out.push(format!(
            "transcripts: {} could not be followed; their sessions are listed as unknown",
            health.follow_faults.transcripts
        ));
    }
    if health.transcripts.unreadable > 0 {
        out.push(format!(
            "transcripts: {} could not be read; their sessions are listed as unknown",
            health.transcripts.unreadable
        ));
    }
    out
}

fn local(at: Instant, zone: &TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The markdown of a model: what is running, dormant and unplaced, each with a
/// resume command to paste.
pub fn render(model: &Model, zone: &TimeZone) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ = writeln!(
        out,
        "# Claude Code sessions, {}\n",
        local(model.generated_at, zone)
    );
    let _ = writeln!(
        out,
        "A resume restores the conversation, not a command that was running; this is a point in time.\n"
    );
    let warned = warnings(model);
    if !warned.is_empty() {
        let _ = writeln!(out, "## Warnings\n");
        for warning in &warned {
            let _ = writeln!(out, "- {warning}");
        }
        out.push('\n');
    }
    let kind_of = |view: &&SessionView| view.session.activity.kind();
    let running = |view: &&SessionView| kind_of(view).is_live();
    let unknown = |view: &&SessionView| kind_of(view) == ActivityKind::Unknown;
    section(
        &mut out,
        zone,
        "Running",
        model.sessions.iter().filter(running),
    );
    section(
        &mut out,
        zone,
        "Could not tell",
        model.sessions.iter().filter(unknown),
    );
    section(&mut out, zone, "Dormant", model.dormant.iter());
    out
}

fn section<'a>(
    out: &mut String,
    zone: &TimeZone,
    heading: &str,
    views: impl Iterator<Item = &'a SessionView>,
) {
    let views: Vec<&SessionView> = views.collect();
    if views.is_empty() {
        return;
    }
    let _ = writeln!(out, "## {heading} ({})\n", views.len());
    for view in views {
        entry(out, view, zone);
    }
}

fn entry(out: &mut String, view: &SessionView, zone: &TimeZone) {
    let title = view
        .title
        .as_ref()
        .map(|title| one_line(title.as_str()))
        .filter(|title| !title.is_empty());
    let _ = writeln!(out, "### {}\n", title.as_deref().unwrap_or("(no title)"));
    let _ = writeln!(
        out,
        "- {}, session `{}`",
        describe(&view.session.activity, zone),
        view.session.id
    );
    if let Some(last) = view.last_activity {
        let _ = writeln!(out, "- last activity {}", local(last, zone));
    }
    if let Some(branch_dir) = view
        .session
        .cwd
        .as_deref()
        .and_then(Path::to_str)
        .filter(|dir| !dir.is_empty())
    {
        let _ = writeln!(out, "- directory `{branch_dir}`");
    }
    match &view.resume {
        Ok(command) => {
            let _ = writeln!(out, "\n```sh\n{command}\n```");
            for warning in command.warnings() {
                let _ = writeln!(out, "- warning: {warning}");
            }
        }
        Err(why) => {
            let _ = writeln!(out, "\nNo resume command: {why}.");
        }
    }
    out.push('\n');
}

fn describe(activity: &Activity, zone: &TimeZone) -> String {
    match activity {
        Activity::Blocked { on } => format!("blocked ({on:?})"),
        Activity::Running { since } => format!("running since {}", local(*since, zone)),
        Activity::Watching { ended_at, .. } => {
            format!(
                "turn ended {}, background work continues",
                local(*ended_at, zone)
            )
        }
        Activity::Idle { ended_at, .. } => format!("idle since {}", local(*ended_at, zone)),
        Activity::Dormant {
            last_seen,
            background,
        } => {
            let ended = background.map_or(String::new(), |end| {
                format!(" (background session {})", end.words())
            });
            format!("dormant, last seen {}{ended}", local(*last_seen, zone))
        }
        Activity::Gone { background } => {
            let ended = background.map_or(String::new(), |end| {
                format!(" (background session {})", end.words())
            });
            format!("gone: its transcript is gone, nothing to resume{ended}")
        }
        Activity::Unknown { reason } => {
            format!("{}: {}", activity.kind().name(), one_line(reason.as_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    //! The refusal is checked against real mounts; the rendering against a hand-built model.

    use super::*;

    /// A tmpfs that is certainly mounted on a Linux machine with systemd or a /run/user.
    fn some_tmpfs() -> Option<PathBuf> {
        ["/dev/shm", "/run", "/tmp"]
            .into_iter()
            .map(PathBuf::from)
            .find(|dir| statfs(dir).is_ok_and(|s| u64::try_from(s.f_type) == Ok(TMPFS_MAGIC)))
    }

    /// Mutation: remove the `TMPFS_MAGIC` arm, or check the file instead of its directory
    /// (a file that does not exist yet fails `statfs` with ENOENT instead).
    #[test]
    fn a_path_on_tmpfs_is_refused_by_name() {
        let Some(dir) = some_tmpfs() else {
            // No tmpfs on this machine to test against; the home directory test below still ran.
            return;
        };
        let error = refuse_memory_backed(&dir.join("not-yet-written.md")).unwrap_err();
        let shown = error.to_string();
        assert!(
            matches!(error, SnapshotError::MemoryBacked { kind: "tmpfs", .. }),
            "{shown}"
        );
        assert!(shown.starts_with("REFUSED: "), "{shown}");
        assert!(shown.contains("choose a path on disk"), "{shown}");
    }

    #[test]
    fn a_path_on_a_disk_is_accepted_and_a_missing_directory_is_an_error_of_its_own() {
        let on_disk = Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshot.md");
        assert!(refuse_memory_backed(&on_disk).is_ok());
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-dir/snapshot.md");
        assert!(matches!(
            refuse_memory_backed(&missing),
            Err(SnapshotError::Filesystem { .. })
        ));
        assert!(refuse_memory_backed(Path::new("snapshot.md")).is_ok());
    }

    /// The markdown for a model with one dormant, one blocked and one unreadable session,
    /// in UTC; the expected text is written out by hand.
    /// Mutation: drop the warnings section, file `Unknown` under `Running`, or print the
    /// resume command without its fence.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the model and the expected text are the test"
    )]
    fn the_snapshot_lists_each_kind_of_session_and_every_warning() {
        use crate::model::{Pin, SourceState};
        use crate::types::{ResumeCommand, Session, SessionId, SessionKind, WaitingFor};
        use crate::util::redact::Redacted;
        let at = |text: &str| -> Instant { text.parse().unwrap() };
        let id = |n: u8| {
            SessionId::parse(&format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
        };
        let view = |n: u8, title: Option<&str>, activity: Activity| {
            let resume = ResumeCommand::new("/home/dev/my proj".into(), vec![], id(n)).unwrap();
            SessionView {
                session: Session {
                    id: id(n),
                    cwd: Some("/home/dev/my proj".into()),
                    ai_title: None,
                    window_title: None,
                    name: None,
                    kind: SessionKind::Interactive,
                    activity,
                },
                title: title.map(Redacted::of),
                pending: None,
                links: Vec::new(),
                resume: Ok(resume),
                last_turn_ends: Vec::new(),
                last_activity: Some(at("2026-10-28T09:30:00Z")),
                pin: Pin::Unpinned,
                gist: None,
            }
        };
        let mut model = Model::unread(at("2026-10-28T10:00:00Z"));
        model.health.listing = SourceState::Failed {
            at: at("2026-10-28T10:00:00Z"),
            reason: "timed out".into(),
        };
        model.health.registry = SourceState::Ok {
            at: at("2026-10-28T10:00:00Z"),
        };
        model.health.transcripts.discovery = SourceState::Ok {
            at: at("2026-10-28T10:00:00Z"),
        };
        model.health.transcripts.progress = ScanProgress::Complete;
        model.sessions = vec![
            view(
                1,
                Some("Plan the\nmigration"),
                Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
            ),
            view(
                2,
                None,
                Activity::Unknown {
                    reason: Redacted::of("the listing is unavailable"),
                },
            ),
        ];
        model.dormant = vec![view(
            3,
            Some("Old work"),
            Activity::Dormant {
                last_seen: at("2026-10-27T08:00:00Z"),
                background: None,
            },
        )];
        let expected = "\
# Claude Code sessions, 2026-10-28 10:00

A resume restores the conversation, not a command that was running; this is a point in time.

## Warnings

- the supervisor's listing: timed out

## Running (1)

### Plan the migration

- blocked (InputNeeded), session `00000000-0000-4000-8000-000000000001`
- last activity 2026-10-28 09:30
- directory `/home/dev/my proj`

```sh
cd '/home/dev/my proj' && claude --resume 00000000-0000-4000-8000-000000000001
```

## Could not tell (1)

### (no title)

- unknown: the listing is unavailable, session `00000000-0000-4000-8000-000000000002`
- last activity 2026-10-28 09:30
- directory `/home/dev/my proj`

```sh
cd '/home/dev/my proj' && claude --resume 00000000-0000-4000-8000-000000000002
```

## Dormant (1)

### Old work

- dormant, last seen 2026-10-27 08:00, session `00000000-0000-4000-8000-000000000003`
- last activity 2026-10-28 09:30
- directory `/home/dev/my proj`

```sh
cd '/home/dev/my proj' && claude --resume 00000000-0000-4000-8000-000000000003
```

";
        assert_eq!(render(&model, &TimeZone::UTC), expected);
    }

    #[test]
    fn the_magic_numbers_are_the_kernels() {
        // include/uapi/linux/magic.h: TMPFS_MAGIC 0x01021994, RAMFS_MAGIC 0x858458f6.
        assert_eq!(TMPFS_MAGIC, 16_914_836);
        assert_eq!(RAMFS_MAGIC, 2_240_043_254);
    }
}
