//! What the machine says about itself: which boot this is, and whether it is shutting down.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util::runner::{CommandRunner, Exit};

/// Where Linux keeps the id it draws anew at every boot.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// The id of one boot of the machine: two saves with the same one were made without a reboot
/// between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BootId(String);

impl BootId {
    /// The boot id in the file at `path`, trimmed; an empty file has none.
    pub fn read(path: &Path) -> Result<Self, std::io::Error> {
        let text = std::fs::read_to_string(path)?;
        let id = text.trim();
        if id.is_empty() {
            Err(std::io::Error::other(format!(
                "{} is empty",
                path.display()
            )))
        } else {
            Ok(Self(id.to_owned()))
        }
    }

    #[cfg(test)]
    pub fn of(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// Whether the machine is up, as `systemctl is-system-running` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemState {
    /// Up: `running`, or `degraded` (up with a failed unit).
    Up,
    /// Any other answer, as systemd wrote it: `stopping` during a shutdown, `starting` and
    /// `initializing` during a boot, `maintenance`, `offline`.
    Other(String),
    /// `systemctl` could not be asked or gave no answer; `reason` says which.
    Unreadable { reason: String },
}

/// How long `systemctl is-system-running` may take: it answers from systemd's memory at once,
/// and a machine too busy to answer in five seconds is not one to trust as up.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Asks systemd whether the machine is up. It exits non-zero for every state but `running`,
/// so the state is read from stdout whatever the exit.
pub async fn state(runner: &dyn CommandRunner) -> SystemState {
    let output = match runner
        .run("systemctl", &["is-system-running"], SYSTEMCTL_TIMEOUT)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            return SystemState::Unreadable {
                reason: format!("systemctl: {error}"),
            };
        }
    };
    let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    match answer.as_str() {
        "running" | "degraded" => SystemState::Up,
        "" => SystemState::Unreadable {
            reason: format!(
                "systemctl printed nothing (exit {:?}): {}",
                output.exit.code(),
                output.stderr
            ),
        },
        _ if matches!(output.exit, Exit::Signalled) => SystemState::Unreadable {
            reason: "systemctl was killed".to_owned(),
        },
        _ => SystemState::Other(answer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::runner::{FakeRunner, Output, RunError};

    async fn ask(answer: Result<Output, RunError>) -> SystemState {
        state(&FakeRunner::new([answer])).await
    }

    async fn printed(code: Exit, stdout: &str) -> SystemState {
        ask(Ok(Output {
            exit: code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: String::new(),
        }))
        .await
    }

    /// The answers are read from stdout whatever the exit: `degraded` and `stopping` exit 1.
    /// Mutation: treat a non-zero exit as unreadable (`degraded` stops counting as up), or
    /// count `stopping` as up.
    #[tokio::test]
    async fn the_state_is_read_from_stdout_whatever_the_exit() {
        assert_eq!((printed(Exit::Success, "running\n")).await, SystemState::Up);
        assert_eq!(
            (printed(Exit::Code(1), "degraded\n")).await,
            SystemState::Up
        );
        assert_eq!(
            (printed(Exit::Code(1), "stopping\n")).await,
            SystemState::Other("stopping".to_owned())
        );
        assert!(matches!(
            (printed(Exit::Code(1), "")).await,
            SystemState::Unreadable { .. }
        ));
        assert!(matches!(
            ask(Err(RunError::NotFound)).await,
            SystemState::Unreadable { .. }
        ));
    }

    #[test]
    fn a_boot_id_is_the_trimmed_file_and_an_empty_one_is_none() {
        let dir = crate::testutil::Scratch::new("boot-id");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("boot_id");
        std::fs::write(&path, "abc-123\n").unwrap();
        assert_eq!(BootId::read(&path).unwrap(), BootId::of("abc-123"));
        std::fs::write(&path, "\n").unwrap();
        assert!(BootId::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
