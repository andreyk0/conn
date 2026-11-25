//! One `conn serve` per state directory.
//!
//! Two servers over one state directory would each send the desktop notification, each pay for
//! the gist and review runs, and each save its own copy of `seen.json` over the other's. So
//! `serve` takes an exclusive `flock` on `serve.lock` before anything else, and a second one is
//! refused with the first one's process id. The kernel drops the lock when the
//! process exits, however it exits, so a lock is never left behind. `conn hook` never takes it:
//! the hooks of many sessions append to the event log at once by design.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

use crate::util::fs::{FILE_MODE, OpenRegularError, ensure_private_dir, open_regular};

pub const FILE: &str = "serve.lock";

/// The lock on a state directory, held while this value lives.
#[must_use = "the lock is released when this is dropped"]
#[derive(Debug)]
pub struct ServeLock {
    _file: File,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock; `holder` is its process id, when the file says.
    #[error(
        "another conn serve{} is using {}; stop it, or give this one another state_dir",
        holder.map_or_default(|pid| format!(" (pid {pid})")),
        state_dir.display()
    )]
    Held {
        state_dir: PathBuf,
        holder: Option<u32>,
    },
    #[error("cannot open {}: {source}", path.display())]
    Open {
        path: PathBuf,
        source: OpenRegularError,
    },
    #[error("cannot lock {}: {source}", path.display())]
    Lock { path: PathBuf, source: io::Error },
}

/// Takes the lock on `state_dir`, creating the directory and the file, and records this
/// process's id in it; refuses at once, never waits, when another process holds it.
pub fn acquire(state_dir: &Path) -> Result<ServeLock, LockError> {
    let path = state_dir.join(FILE);
    let lock_error = |source: io::Error| LockError::Lock {
        path: path.clone(),
        source,
    };
    ensure_private_dir(state_dir).map_err(lock_error)?;
    // Not truncated on open: the holder's id must survive a refused second open.
    let mut file = open_regular(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE),
        &path,
    )
    .map_err(|source| LockError::Open {
        path: path.clone(),
        source,
    })?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => {
            return Err(LockError::Held {
                state_dir: state_dir.to_path_buf(),
                holder: holder(&mut file),
            });
        }
        Err(errno) => return Err(lock_error(errno.into())),
    }
    record_holder(&mut file, std::process::id()).map_err(lock_error)?;
    Ok(ServeLock { _file: file })
}

fn record_holder(file: &mut File, pid: u32) -> io::Result<()> {
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(format!("{pid}\n").as_bytes())
}

/// The process id the holder wrote; `None` when the file holds none yet (it is between its open
/// and its write) or holds something else.
fn holder(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    //! Two locks in one process: `flock` belongs to the open file, so a second open of the same
    //! file conflicts with the first exactly as a second process would.

    use super::*;
    use crate::testutil::Scratch;

    /// Mutation: skip the `flock` (the second lock is taken), or truncate the file on open
    /// (the refusal names no holder).
    #[test]
    fn a_second_lock_is_refused_naming_the_holder() {
        let dir = Scratch::new("lock-held");
        let first = acquire(&dir).unwrap();
        match acquire(&dir) {
            Err(LockError::Held { state_dir, holder }) => {
                assert_eq!(state_dir, dir.to_path_buf());
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("expected Held, got {other:?}"),
        }
        drop(first);
    }

    /// Mutation: hold the lock somewhere that outlives the value (the second acquire fails).
    #[test]
    fn a_dropped_lock_is_released() {
        let dir = Scratch::new("lock-drop");
        drop(acquire(&dir).unwrap());
        let again = acquire(&dir).unwrap();
        drop(again);
    }

    /// A state directory that does not exist yet is created private; a lock file that is not
    /// a regular file is refused, not followed.
    #[test]
    fn the_directory_is_created_and_a_non_file_refused() {
        let dir = Scratch::new("lock-mkdir");
        let state = dir.join("state");
        let lock = acquire(&state).unwrap();
        assert!(state.join(FILE).is_file());
        drop(lock);

        let odd = dir.join("odd");
        std::fs::create_dir_all(odd.join(FILE)).unwrap();
        assert!(matches!(acquire(&odd), Err(LockError::Open { .. })));
    }
}
