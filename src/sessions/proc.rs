//! The process table: whether a registry entry's process is still the one that wrote it, and
//! what it was launched with.
//!
//! Reads go through [`ProcReader`] so the checks run against a fake `/proc` in tests.

use std::fs;
use std::io;

use crate::types::Pid;

/// The outcome of reading one `/proc` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcRead<T> {
    /// The process does not exist.
    Absent,
    Read(T),
    /// The file exists or may exist, but could not be read; the reason is the I/O error.
    Unreadable(String),
}

pub trait ProcReader {
    /// This process's pid domain, or `None` when it cannot be established.
    fn host_domain(&self) -> Option<PidDomain>;
    /// The text of `/proc/<pid>/stat`.
    fn read_stat(&self, pid: Pid) -> ProcRead<String>;
    /// The raw bytes of `/proc/<pid>/cmdline`: NUL-terminated arguments.
    fn read_cmdline(&self, pid: Pid) -> ProcRead<Vec<u8>>;
}

/// Which pid space a pid belongs to: `linux:<machine id>:pid:[<pid namespace inode>]`, the
/// form the registry records.
///
/// A pid only means something inside the namespace that issued it, so an entry written
/// from another container or machine cannot be checked against this host's `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidDomain(String);

impl PidDomain {
    /// `namespace_link` is what `readlink /proc/<pid>/ns/pid` prints, such as `pid:[4026531836]`.
    pub fn linux(machine_id: &str, namespace_link: &str) -> Self {
        Self(format!("linux:{machine_id}:{namespace_link}"))
    }

    /// A domain as recorded in a registry file. Anything is accepted: the only question asked
    /// of it is whether it equals this host's.
    pub fn recorded(text: &str) -> Self {
        Self(text.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A process start time in clock ticks since boot: field 22 of `/proc/<pid>/stat`.
///
/// Together with a pid it names one process for the life of the machine; a pid alone is
/// reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStart(u64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a process start time: {0:?}")]
pub struct ProcStartError(String);

impl ProcStart {
    pub fn parse(text: &str) -> Result<Self, ProcStartError> {
        text.parse()
            .map(Self)
            .map_err(|_| ProcStartError(text.to_owned()))
    }

    /// The start time in a `/proc/<pid>/stat` line.
    ///
    /// Field 2 is the command name in parentheses and may itself hold spaces and
    /// parentheses, so fields are counted from the last `)`.
    pub fn from_stat(stat: &str) -> Option<Self> {
        /// After the closing parenthesis field 3 comes first, so field 22 is the 20th word.
        const WORDS_BEFORE_START: usize = 22 - 3;
        let (_, after_comm) = stat.rsplit_once(')')?;
        after_comm
            .split_whitespace()
            .nth(WORDS_BEFORE_START)
            .and_then(|word| Self::parse(word).ok())
    }
}

/// Reads the real `/proc`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProc;

impl SystemProc {
    fn path(pid: Pid, file: &str) -> String {
        format!("/proc/{}/{file}", pid.get())
    }

    fn classify<T>(read: io::Result<T>) -> ProcRead<T> {
        /// The kernel's answer for a process that exited mid-read.
        const ESRCH: i32 = 3;
        match read {
            Ok(content) => ProcRead::Read(content),
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ESRCH) => {
                ProcRead::Absent
            }
            Err(e) => ProcRead::Unreadable(e.to_string()),
        }
    }
}

impl ProcReader for SystemProc {
    fn host_domain(&self) -> Option<PidDomain> {
        let machine_id = fs::read_to_string("/etc/machine-id").ok()?;
        let namespace = fs::read_link("/proc/self/ns/pid").ok()?;
        Some(PidDomain::linux(machine_id.trim(), namespace.to_str()?))
    }

    fn read_stat(&self, pid: Pid) -> ProcRead<String> {
        Self::classify(fs::read_to_string(Self::path(pid, "stat")))
    }

    fn read_cmdline(&self, pid: Pid) -> ProcRead<Vec<u8>> {
        Self::classify(fs::read(Self::path(pid, "cmdline")))
    }
}

#[cfg(test)]
pub(super) mod fake {
    use std::collections::HashMap;

    use super::*;

    /// A scripted `/proc`: a pid not in a map reads as absent.
    #[derive(Default)]
    pub(in crate::sessions) struct FakeProc {
        pub(in crate::sessions) domain: Option<PidDomain>,
        pub(in crate::sessions) stats: HashMap<u32, ProcRead<String>>,
        pub(in crate::sessions) cmdlines: HashMap<u32, ProcRead<Vec<u8>>>,
    }

    impl ProcReader for FakeProc {
        fn host_domain(&self) -> Option<PidDomain> {
            self.domain.clone()
        }

        fn read_stat(&self, pid: Pid) -> ProcRead<String> {
            self.stats
                .get(&pid.get())
                .cloned()
                .unwrap_or(ProcRead::Absent)
        }

        fn read_cmdline(&self, pid: Pid) -> ProcRead<Vec<u8>> {
            self.cmdlines
                .get(&pid.get())
                .cloned()
                .unwrap_or(ProcRead::Absent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built stat lines: 52 fields in a real one, but only fields up to 22 matter; field
    /// 22 is `START`, and fields 3..=21 are nineteen filler words.
    fn stat(comm: &str, start: &str) -> String {
        format!(
            "4242 ({comm}) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {start} 1000 50"
        )
    }

    /// Mutation: take the first `)` instead of the last, or an off-by-one on the field index
    /// -> red on the odd names or on all rows.
    #[test]
    fn start_time_is_field_22_even_when_the_command_name_is_hostile() {
        let table = [
            ("claude", "12345678", Some(12_345_678)),
            ("tmux: server", "5", Some(5)),
            ("a) b (c", "99", Some(99)),
            ("x) S 1 2 3 (y", "7", Some(7)),
        ];
        for (comm, start, expected) in table {
            assert_eq!(
                ProcStart::from_stat(&stat(comm, start)),
                expected.map(ProcStart),
                "{comm}"
            );
        }
    }

    /// Mutation: return `Some` for a line with too few fields -> red.
    #[test]
    fn a_truncated_or_garbled_stat_has_no_start_time() {
        for bad in [
            "",
            "4242 no parens at all",
            "4242 (x) S 1 2 3",
            "4242 (x) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 -5",
        ] {
            assert_eq!(ProcStart::from_stat(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_domain_matches_the_registry_spelling() {
        assert_eq!(
            PidDomain::linux("abc123", "pid:[4026531836]"),
            PidDomain::recorded("linux:abc123:pid:[4026531836]")
        );
    }

    /// Reads this machine's real `/proc`; run with `cargo test -- --ignored`.
    /// Mutation: read field 21 instead of 22 -> the live self-check fails.
    #[test]
    #[ignore = "reads the real /proc"]
    fn own_stat_and_domain_are_readable() {
        let me = Pid::new(std::process::id());
        let ProcRead::Read(text) = SystemProc.read_stat(me) else {
            panic!("own stat unreadable");
        };
        assert!(ProcStart::from_stat(&text).is_some());
        assert!(SystemProc.host_domain().is_some());
        assert_eq!(SystemProc.read_stat(Pid::new(u32::MAX)), ProcRead::Absent);
    }
}
