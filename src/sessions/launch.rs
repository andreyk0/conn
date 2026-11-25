//! How a live session was launched, from `/proc/<pid>/cmdline`.
//!
//! A dormant session has no process to ask, so its launch flags are unknown rather than
//! empty: "launched with nothing" and "cannot tell" lead to different resume commands.

use super::proc::{ProcRead, ProcReader};
use super::registry::{Liveness, RegistryEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchArgs {
    /// The program and its arguments, as the process was started; the program is first.
    Known(Vec<String>),
    Unknown(LaunchUnknown),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchUnknown {
    /// No live process to read it from.
    #[error("the session has no live process")]
    Dormant,
    #[error("the process could not be checked: {0}")]
    LivenessUnknown(String),
    #[error("/proc/{pid}/cmdline: {message}")]
    Unreadable { pid: u32, message: String },
    #[error("/proc/{pid}/cmdline is empty")]
    Empty { pid: u32 },
    #[error("/proc/{pid}/cmdline is not valid UTF-8")]
    NotUtf8 { pid: u32 },
}

impl LaunchArgs {
    /// The launch arguments of the process behind a registry entry. The process is read only
    /// when [`Liveness::check`] says it is the one that wrote the entry: a reused pid has
    /// someone else's command line.
    pub fn of_entry(entry: &RegistryEntry, procs: &dyn ProcReader) -> Self {
        match Liveness::check(entry, procs) {
            Liveness::Gone { .. } => Self::Unknown(LaunchUnknown::Dormant),
            Liveness::Unknown(reason) => {
                Self::Unknown(LaunchUnknown::LivenessUnknown(reason.to_string()))
            }
            Liveness::Live => {
                let pid = entry.pid.get();
                match procs.read_cmdline(entry.pid) {
                    ProcRead::Absent => Self::Unknown(LaunchUnknown::Dormant),
                    ProcRead::Unreadable(message) => {
                        Self::Unknown(LaunchUnknown::Unreadable { pid, message })
                    }
                    ProcRead::Read(raw) => Self::from_cmdline(pid, &raw),
                }
            }
        }
    }

    /// Splits a `cmdline`: arguments are NUL-terminated.
    pub fn from_cmdline(pid: u32, raw: &[u8]) -> Self {
        let Some(body) = raw.strip_suffix(&[0]) else {
            return Self::Unknown(LaunchUnknown::Empty { pid });
        };
        body.split(|b| *b == 0)
            .map(|word| String::from_utf8(word.to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_or(Self::Unknown(LaunchUnknown::NotUtf8 { pid }), Self::Known)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::sessions::proc::PidDomain;
    use crate::sessions::proc::fake::FakeProc;

    fn known(words: &[&str]) -> LaunchArgs {
        LaunchArgs::Known(words.iter().map(|w| (*w).to_owned()).collect())
    }

    /// Mutation: split on whitespace instead of NUL -> the argument with a space splits -> red.
    #[test]
    fn cmdline_splits_on_nul_and_keeps_spaces_inside_an_argument() {
        assert_eq!(
            LaunchArgs::from_cmdline(1, b"claude\0--system-prompt\0be brief\0--chrome\0"),
            known(&["claude", "--system-prompt", "be brief", "--chrome"])
        );
        assert_eq!(
            LaunchArgs::from_cmdline(1, b"claude\0\0x\0"),
            known(&["claude", "", "x"])
        );
    }

    /// Mutation: map empty or invalid input to `Known(vec![])` -> red.
    #[test]
    fn an_unusable_cmdline_is_unknown_not_empty() {
        assert_eq!(
            LaunchArgs::from_cmdline(7, b""),
            LaunchArgs::Unknown(LaunchUnknown::Empty { pid: 7 })
        );
        assert_eq!(
            LaunchArgs::from_cmdline(7, b"claude\0\xff\0"),
            LaunchArgs::Unknown(LaunchUnknown::NotUtf8 { pid: 7 })
        );
    }

    const DOMAIN: &str = RegistryEntry::TEST_DOMAIN;

    fn entry() -> RegistryEntry {
        RegistryEntry::for_proc_test(500, Some("777"), Some(DOMAIN))
    }

    fn procs(start: &str) -> FakeProc {
        FakeProc {
            domain: Some(PidDomain::recorded(DOMAIN)),
            stats: HashMap::from([(
                500,
                ProcRead::Read(format!(
                    "500 (claude) S 1 500 500 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {start} 1 2"
                )),
            )]),
            cmdlines: HashMap::from([(500, ProcRead::Read(b"claude\0--chrome\0".to_vec()))]),
        }
    }

    /// Mutation: skip the liveness check -> the reused pid's command line is returned as
    /// this session's -> red.
    #[test]
    fn only_the_process_that_wrote_the_entry_is_read() {
        assert_eq!(
            LaunchArgs::of_entry(&entry(), &procs("777")),
            known(&["claude", "--chrome"])
        );
        assert_eq!(
            LaunchArgs::of_entry(&entry(), &procs("778")),
            LaunchArgs::Unknown(LaunchUnknown::Dormant)
        );
        assert_eq!(
            LaunchArgs::of_entry(&entry(), &FakeProc::default()),
            LaunchArgs::Unknown(LaunchUnknown::LivenessUnknown(
                "this host's pid domain could not be read".into()
            ))
        );
    }
}
