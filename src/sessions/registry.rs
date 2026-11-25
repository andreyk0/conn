//! `~/.claude/sessions/<pid>.json`: one file per Claude Code process, written by the process.
//!
//! The files are undocumented, so only the fields a resume cannot do without are required;
//! every other field is optional and a field of the wrong type is an error that names it. The
//! `<pid>.<hex>.key` files beside them hold credentials and are never opened.
//!
//! A file can outlive its process, so an entry says nothing about whether the session is
//! running: [`Liveness::check`] answers that.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::facts::{JobId, ProcessStatus};
use super::proc::{PidDomain, ProcRead, ProcReader, ProcStart};
use crate::types::{Instant, Pid, SessionId};
use crate::util::json::{FieldError, Fields};

/// How the session was started, as the registry spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryKind {
    Interactive,
    /// The registry writes `bg`; the listing writes `background`.
    Background,
    Other(String),
}

impl RegistryKind {
    fn parse(text: &str) -> Self {
        match text {
            "interactive" => Self::Interactive,
            "bg" | "background" => Self::Background,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub pid: Pid,
    pub session_id: SessionId,
    /// Where the session works now; Claude Code updates it when the session moves.
    pub cwd: PathBuf,
    pub started_at: Option<Instant>,
    pub proc_start: Option<ProcStart>,
    pub pid_domain: Option<PidDomain>,
    pub version: Option<String>,
    pub kind: Option<RegistryKind>,
    pub entrypoint: Option<String>,
    pub name: Option<String>,
    /// `derived`, `auto`, ...; kept as text because only the human-given case matters and the
    /// vocabulary is not documented.
    pub name_source: Option<String>,
    pub status: Option<ProcessStatus>,
    pub status_updated_at: Option<Instant>,
    /// Background sessions only.
    pub job_id: Option<JobId>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The directory or a file in it could not be read: "could not tell", not "no sessions".
    #[error("cannot read {}: {source}", path.display())]
    Unreadable { path: PathBuf, source: io::Error },
    #[error("{} is not a session record: {reason}", path.display())]
    Malformed { path: PathBuf, reason: String },
    /// The `pid` inside the file is not the pid in its name, so it describes neither process.
    #[error("{} is named for pid {} but records pid {}", path.display(), file_pid.get(), entry_pid.get())]
    PidMismatch {
        path: PathBuf,
        file_pid: Pid,
        entry_pid: Pid,
    },
    /// A file the registry directory is not known to contain.
    #[error("{} is not a registry entry or key file", path.display())]
    Unrecognized { path: PathBuf },
}

/// What a file name in the registry directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileName {
    Entry(Pid),
    Key,
    Unknown,
}

impl FileName {
    fn classify(name: &str) -> Self {
        let digits = |text: &str| -> Option<u32> {
            if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        };
        if let Some(pid) = name.strip_suffix(".json").and_then(digits) {
            return Self::Entry(Pid::new(pid));
        }
        let is_key = name.strip_suffix(".key").is_some_and(|stem| {
            stem.split_once('.').is_some_and(|(pid, hex)| {
                digits(pid).is_some()
                    && !hex.is_empty()
                    && hex.chars().all(|c| c.is_ascii_hexdigit())
            })
        });
        if is_key { Self::Key } else { Self::Unknown }
    }
}

/// Every entry in `<claude_dir>/sessions`, in file-name order, each parsed on its own so one
/// bad file does not hide the others.
pub fn registry(claude_dir: &Path) -> Vec<Result<RegistryEntry, RegistryError>> {
    let dir = claude_dir.join("sessions");
    let listed = fs::read_dir(&dir).and_then(Iterator::collect::<Result<Vec<_>, _>>);
    let mut files = match listed {
        Ok(files) => files,
        Err(source) => return vec![Err(RegistryError::Unreadable { path: dir, source })],
    };
    files.sort_by_key(fs::DirEntry::file_name);

    files
        .iter()
        .filter_map(|file| {
            let path = file.path();
            let name = file.file_name();
            match name.to_str().map(FileName::classify) {
                Some(FileName::Key) => None,
                Some(FileName::Entry(file_pid)) => Some(read_entry(&path, file_pid)),
                Some(FileName::Unknown) | None => Some(Err(RegistryError::Unrecognized { path })),
            }
        })
        .collect()
}

fn read_entry(path: &Path, file_pid: Pid) -> Result<RegistryEntry, RegistryError> {
    let text = fs::read_to_string(path).map_err(|source| RegistryError::Unreadable {
        path: path.to_owned(),
        source,
    })?;
    let malformed = |reason: String| RegistryError::Malformed {
        path: path.to_owned(),
        reason,
    };
    let json: Value = serde_json::from_str(&text).map_err(|e| malformed(e.to_string()))?;
    let entry = parse_entry(&json).map_err(|e| malformed(e.to_string()))?;
    if entry.pid == file_pid {
        Ok(entry)
    } else {
        Err(RegistryError::PidMismatch {
            path: path.to_owned(),
            file_pid,
            entry_pid: entry.pid,
        })
    }
}

fn parse_entry(json: &Value) -> Result<RegistryEntry, FieldError> {
    let fields = Fields::of(json)?;
    let text = |key| fields.optional_str(key).map(|v| v.map(str::to_owned));
    Ok(RegistryEntry {
        pid: fields
            .optional_pid("pid")?
            .ok_or(FieldError::missing("pid"))?,
        session_id: fields.required_session_id("sessionId")?,
        cwd: fields.required_path("cwd")?,
        started_at: fields.optional_millis("startedAt")?,
        proc_start: fields
            .optional_str("procStart")?
            .map(|s| {
                ProcStart::parse(s).map_err(|e| FieldError::invalid("procStart", e.to_string()))
            })
            .transpose()?,
        pid_domain: fields.optional_str("pidDomain")?.map(PidDomain::recorded),
        version: text("version")?,
        kind: fields.optional_str("kind")?.map(RegistryKind::parse),
        entrypoint: text("entrypoint")?,
        name: text("name")?,
        name_source: text("nameSource")?,
        status: fields.optional_str("status")?.map(ProcessStatus::parse),
        status_updated_at: fields.optional_millis("statusUpdatedAt")?,
        job_id: fields
            .optional_str("jobId")?
            .map(|s| JobId::parse(s).map_err(|e| FieldError::invalid("jobId", e.to_string())))
            .transpose()?,
    })
}

/// Whether the process that wrote a registry entry is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Gone {
        reason: GoneReason,
    },
    /// This machine could not check. It is not a softer "running".
    Unknown(UnknownReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneReason {
    /// No process has that pid.
    NoProcess,
    /// A process has that pid, but it started after the entry's did.
    PidReused,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnknownReason {
    #[error("this host's pid domain could not be read")]
    HostDomainUnreadable,
    #[error("the entry records no pid domain")]
    EntryHasNoDomain,
    #[error("registered in pid domain {recorded}, not this host's {host}")]
    OtherDomain { recorded: String, host: String },
    #[error("the entry records no process start time")]
    EntryHasNoStart,
    #[error("/proc/{pid}/stat: {message}")]
    StatUnreadable { pid: u32, message: String },
    #[error("/proc/{pid}/stat has no start time")]
    StatMalformed { pid: u32 },
}

impl Liveness {
    /// The one place a registry entry becomes a verdict. The pid-domain check comes first
    /// because `/proc` here says nothing about a pid issued elsewhere.
    pub fn check(entry: &RegistryEntry, procs: &dyn ProcReader) -> Self {
        let Some(host) = procs.host_domain() else {
            return Self::Unknown(UnknownReason::HostDomainUnreadable);
        };
        let Some(recorded) = &entry.pid_domain else {
            return Self::Unknown(UnknownReason::EntryHasNoDomain);
        };
        if *recorded != host {
            return Self::Unknown(UnknownReason::OtherDomain {
                recorded: recorded.as_str().to_owned(),
                host: host.as_str().to_owned(),
            });
        }
        let Some(recorded_start) = entry.proc_start else {
            return Self::Unknown(UnknownReason::EntryHasNoStart);
        };
        let pid = entry.pid.get();
        match procs.read_stat(entry.pid) {
            ProcRead::Absent => Self::Gone {
                reason: GoneReason::NoProcess,
            },
            ProcRead::Unreadable(message) => {
                Self::Unknown(UnknownReason::StatUnreadable { pid, message })
            }
            ProcRead::Read(line) => match ProcStart::from_stat(&line) {
                None => Self::Unknown(UnknownReason::StatMalformed { pid }),
                Some(start) if start == recorded_start => Self::Live,
                Some(_) => Self::Gone {
                    reason: GoneReason::PidReused,
                },
            },
        }
    }
}

#[cfg(test)]
impl RegistryEntry {
    /// The pid domain of the fake host in tests.
    pub(super) const TEST_DOMAIN: &'static str =
        "linux:0123456789abcdef0123456789abcdef:pid:[4026531836]";

    /// An entry with only what the process-table checks read.
    pub(super) fn for_proc_test(pid: u32, start: Option<&str>, domain: Option<&str>) -> Self {
        Self {
            pid: Pid::new(pid),
            session_id: SessionId::parse("00000000-0000-4000-8000-000000000020").unwrap(),
            cwd: "/home/dev/alpha".into(),
            started_at: None,
            proc_start: start.map(|s| ProcStart::parse(s).unwrap()),
            pid_domain: domain.map(PidDomain::recorded),
            version: None,
            kind: None,
            entrypoint: None,
            name: None,
            name_source: None,
            status: None,
            status_updated_at: None,
            job_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::sessions::proc::fake::FakeProc;
    use crate::testutil::{epoch as at, session_id as id};

    const DOMAIN: &str = RegistryEntry::TEST_DOMAIN;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sessions/claude")
    }

    fn file_of(result: &Result<RegistryEntry, RegistryError>) -> String {
        let path = match result {
            Ok(_) => return "entry".to_owned(),
            Err(
                RegistryError::Unreadable { path, .. }
                | RegistryError::Malformed { path, .. }
                | RegistryError::PidMismatch { path, .. }
                | RegistryError::Unrecognized { path },
            ) => path,
        };
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// The fixture directory holds a `.key` file that must yield nothing: not an entry, not an
    /// error. Mutation: classify the key file as unrecognised -> an extra error -> red; drop the
    /// sort by file name -> order differs -> red.
    #[test]
    fn the_directory_yields_two_entries_and_one_error_per_bad_file_in_name_order() {
        let results = registry(&fixture_dir());
        let shape: Vec<String> = results.iter().map(file_of).collect();
        assert_eq!(
            shape,
            [
                "entry",     // 4301.json
                "entry",     // 4302.json
                "4303.json", // not an object
                "4304.json", // names pid 9999
                "4305.json", // no sessionId
                "4306.json", // procStart is not numeric
                "notes.txt", // unrecognised, reported
            ]
        );
    }

    /// Mutation: drop `Fields::optional_millis` for `startedAt` -> `started_at` is `None` -> red.
    #[test]
    fn a_full_interactive_entry_is_read_field_by_field() {
        let results = registry(&fixture_dir());
        let entry = results[0].as_ref().unwrap();
        assert_eq!(
            *entry,
            RegistryEntry {
                pid: Pid::new(4301),
                session_id: id(0x11),
                cwd: "/home/dev/alpha".into(),
                started_at: Some(at(1_761_645_600)),
                proc_start: Some(ProcStart::parse("12345678").unwrap()),
                pid_domain: Some(PidDomain::recorded(
                    "linux:0123456789abcdef0123456789abcdef:pid:[4026531836]"
                )),
                version: Some("2.1.312".into()),
                kind: Some(RegistryKind::Interactive),
                entrypoint: Some("cli".into()),
                name: Some("alpha-01".into()),
                name_source: Some("derived".into()),
                status: Some(ProcessStatus::Idle),
                status_updated_at: Some(at(1_761_649_200)),
                job_id: None,
            }
        );
    }

    /// The second file is a background session with almost nothing but the required fields,
    /// an unknown status and an unknown kind spelling.
    /// Mutation: make `procStart` or `status` required -> the second entry errors -> red.
    #[test]
    fn missing_optional_fields_and_unknown_values_are_tolerated() {
        let results = registry(&fixture_dir());
        let entry = results[1].as_ref().unwrap();
        assert_eq!(
            *entry,
            RegistryEntry {
                pid: Pid::new(4302),
                session_id: id(0x12),
                cwd: "/home/dev/beta".into(),
                started_at: None,
                proc_start: None,
                pid_domain: None,
                version: None,
                kind: Some(RegistryKind::Background),
                entrypoint: None,
                name: None,
                name_source: None,
                status: Some(ProcessStatus::Other("napping".into())),
                status_updated_at: None,
                job_id: Some(JobId::parse("abcd0009").unwrap()),
            }
        );
    }

    /// Mutation: drop the pid comparison in `read_entry` -> `4304.json` is accepted -> red.
    #[test]
    fn a_file_naming_another_pid_is_a_distinct_error() {
        let results = registry(&fixture_dir());
        assert!(matches!(
            &results[3],
            Err(RegistryError::PidMismatch { file_pid, entry_pid, .. })
                if file_pid.get() == 4304 && entry_pid.get() == 9999
        ));
        assert!(matches!(&results[2], Err(RegistryError::Malformed { .. })));
        assert!(
            matches!(&results[4], Err(RegistryError::Malformed { reason, .. }) if reason == "\"sessionId\" is missing")
        );
        assert!(
            matches!(&results[5], Err(RegistryError::Malformed { reason, .. }) if reason.starts_with("\"procStart\" is not valid"))
        );
        assert!(matches!(
            &results[6],
            Err(RegistryError::Unrecognized { .. })
        ));
    }

    /// Mutation: return `vec![]` for an unreadable directory -> red.
    #[test]
    fn a_missing_registry_directory_is_an_error_not_an_empty_list() {
        let results = registry(&fixture_dir().join("no-such-dir"));
        assert!(matches!(
            results.as_slice(),
            [Err(RegistryError::Unreadable { .. })]
        ));
    }

    #[test]
    fn file_names_are_classified() {
        let table = [
            ("4301.json", FileName::Entry(Pid::new(4301))),
            ("4301.0f9a.key", FileName::Key),
            ("4301.key", FileName::Unknown),
            ("4301.xyz.key", FileName::Unknown),
            ("x4301.json", FileName::Unknown),
            (".json", FileName::Unknown),
            ("99999999999.json", FileName::Unknown),
            ("notes.txt", FileName::Unknown),
        ];
        for (name, expected) in table {
            assert_eq!(FileName::classify(name), expected, "{name}");
        }
    }

    fn proc_with(stat: Option<ProcRead<String>>) -> FakeProc {
        FakeProc {
            domain: Some(PidDomain::recorded(DOMAIN)),
            stats: stat
                .into_iter()
                .map(|s| (500, s))
                .collect::<HashMap<_, _>>(),
            cmdlines: HashMap::new(),
        }
    }

    fn stat_line(start: &str) -> ProcRead<String> {
        ProcRead::Read(format!(
            "500 (claude) S 1 500 500 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {start} 1000 50"
        ))
    }

    /// One row per verdict. Mutation per row: `Live` <- compare against a constant;
    /// `NoProcess` <- map `Absent` to `PidReused`; `PidReused` <- treat any present process as
    /// `Live`; the `Unknown` rows <- map each to `Gone`.
    #[test]
    fn liveness_has_a_verdict_for_every_state_of_the_process_table() {
        let live = RegistryEntry::for_proc_test(500, Some("777"), Some(DOMAIN));
        let cases: Vec<(&str, RegistryEntry, FakeProc, Liveness)> = vec![
            (
                "live",
                live.clone(),
                proc_with(Some(stat_line("777"))),
                Liveness::Live,
            ),
            (
                "gone",
                live.clone(),
                proc_with(None),
                Liveness::Gone {
                    reason: GoneReason::NoProcess,
                },
            ),
            (
                "reused",
                live.clone(),
                proc_with(Some(stat_line("778"))),
                Liveness::Gone {
                    reason: GoneReason::PidReused,
                },
            ),
            (
                "unreadable",
                live.clone(),
                proc_with(Some(ProcRead::Unreadable("Permission denied".into()))),
                Liveness::Unknown(UnknownReason::StatUnreadable {
                    pid: 500,
                    message: "Permission denied".into(),
                }),
            ),
            (
                "garbled stat",
                live.clone(),
                proc_with(Some(ProcRead::Read("500 (x) S".into()))),
                Liveness::Unknown(UnknownReason::StatMalformed { pid: 500 }),
            ),
            (
                "other pid namespace",
                RegistryEntry::for_proc_test(
                    500,
                    Some("777"),
                    Some("linux:0123456789abcdef0123456789abcdef:pid:[4026532999]"),
                ),
                proc_with(Some(stat_line("777"))),
                Liveness::Unknown(UnknownReason::OtherDomain {
                    recorded: "linux:0123456789abcdef0123456789abcdef:pid:[4026532999]".into(),
                    host: DOMAIN.into(),
                }),
            ),
            (
                "entry without domain",
                RegistryEntry::for_proc_test(500, Some("777"), None),
                proc_with(Some(stat_line("777"))),
                Liveness::Unknown(UnknownReason::EntryHasNoDomain),
            ),
            (
                "entry without start",
                RegistryEntry::for_proc_test(500, None, Some(DOMAIN)),
                proc_with(Some(stat_line("777"))),
                Liveness::Unknown(UnknownReason::EntryHasNoStart),
            ),
            (
                "host domain unreadable",
                live.clone(),
                FakeProc {
                    domain: None,
                    ..proc_with(Some(stat_line("777")))
                },
                Liveness::Unknown(UnknownReason::HostDomainUnreadable),
            ),
        ];
        for (name, entry, procs, expected) in cases {
            assert_eq!(Liveness::check(&entry, &procs), expected, "{name}");
        }
    }

    /// Reads this machine's registry and checks each entry's liveness; prints counts only.
    /// Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "reads ~/.claude/sessions and the real /proc"]
    #[allow(
        clippy::print_stdout,
        reason = "an ignored smoke test reports counts to its runner"
    )]
    fn real_registry_reads() {
        let claude_dir = Path::new(&std::env::var_os("HOME").unwrap()).join(".claude");
        let results = registry(&claude_dir);
        let (mut live, mut gone, mut unknown, mut errors) = (0, 0, 0, 0);
        for result in &results {
            match result {
                Ok(entry) => match Liveness::check(entry, &crate::sessions::proc::SystemProc) {
                    Liveness::Live => live += 1,
                    Liveness::Gone { .. } => gone += 1,
                    Liveness::Unknown(_) => unknown += 1,
                },
                Err(_) => errors += 1,
            }
        }
        println!(
            "registry: {} results: {live} live, {gone} gone, {unknown} unknown, {errors} errors",
            results.len()
        );
    }
}
