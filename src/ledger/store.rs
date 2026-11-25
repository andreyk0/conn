//! Reading `ledger.jsonl`, and changing it under a lock.
//!
//! A command reads the ledger, decides, and appends, all while holding an exclusive `flock`
//! on the file, so two sessions claiming one ticket at once are decided one after the other
//! and the second sees the first's claim. Readers (the server, the hooks) take no lock: every
//! write is one `write` on an `O_APPEND` descriptor, so they see whole lines, and a line still
//! being written is reported as unreadable rather than guessed at.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use super::{Entry, Ledger};
use crate::util::fs::{FILE_MODE, NonRegular, OpenRegularError, ensure_private_dir, open_regular};

/// The largest ledger read. A line is a few hundred bytes and a busy day writes dozens, so
/// this is years; a file past it is not this tool's ledger.
const BYTES_MAX: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("cannot read the ledger {}: {source}", path.display())]
    Unreadable { path: PathBuf, source: io::Error },
    #[error("the ledger {} is not a regular file: it is {kind}", path.display())]
    NotRegular { path: PathBuf, kind: NonRegular },
    #[error("the ledger {} is over {BYTES_MAX} bytes; it was not read", path.display())]
    TooLarge { path: PathBuf },
    #[error("cannot lock the ledger {}: {source}", path.display())]
    Lock { path: PathBuf, source: io::Error },
    #[error("cannot write the ledger {}: {source}", path.display())]
    Unwritable { path: PathBuf, source: io::Error },
    #[error("cannot encode a ledger line: {0}")]
    Encode(serde_json::Error),
}

/// A line of the ledger that holds no entry. `line` counts from 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ledger line {line}: {reason}")]
pub struct BadLine {
    pub line: usize,
    pub reason: String,
}

/// The ledger as read: what the good lines say, and every line that said nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub ledger: Ledger,
    pub bad_lines: Vec<BadLine>,
}

/// A change the command refused, or the ledger failing under it.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError<E> {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Refused(E),
}

/// Reads the ledger at `path`. A missing file is an empty ledger: nothing was written yet.
pub fn read(path: &Path) -> Result<Loaded, LedgerError> {
    let mut file = match open_regular(OpenOptions::new().read(true), path) {
        Ok(file) => file,
        Err(OpenRegularError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Loaded::default());
        }
        Err(error) => return Err(open_error(path, error)),
    };
    let bytes = read_bounded(&mut file, path)?;
    Ok(parse(&bytes))
}

/// Folds the ledger's bytes; a line that is not an entry becomes a [`BadLine`] in its place.
pub fn parse(bytes: &[u8]) -> Loaded {
    let mut entries = Vec::new();
    let mut bad_lines = Vec::new();
    for (index, line) in (1_usize..).zip(bytes.split_inclusive(|byte| *byte == b'\n')) {
        let Some(body) = line.strip_suffix(b"\n") else {
            bad_lines.push(BadLine {
                line: index,
                reason: "no terminating newline: a write in progress, or a torn one".to_owned(),
            });
            continue;
        };
        if body.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Entry>(body) {
            Ok(entry) => entries.push(entry),
            Err(error) => bad_lines.push(BadLine {
                line: index,
                reason: error.to_string(),
            }),
        }
    }
    Loaded {
        ledger: Ledger::fold(entries),
        bad_lines,
    }
}

/// Locks the ledger at `path`, reads it, and appends what `decide` returns; or, when `decide`
/// refuses, appends nothing. The lock is held from the read to the end of the write.
///
/// `decide` sees only the good lines. A bad line is no reason to refuse a change, but a
/// command says it was there.
pub fn update<R, E>(
    path: &Path,
    decide: impl FnOnce(&Loaded) -> Result<(Vec<Entry>, R), E>,
) -> Result<R, UpdateError<E>> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir).map_err(|source| LedgerError::Unwritable {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let mut file = open_regular(
        OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(FILE_MODE),
        path,
    )
    .map_err(|error| open_error(path, error))?;
    flock(&file, FlockOperation::LockExclusive).map_err(|errno| LedgerError::Lock {
        path: path.to_path_buf(),
        source: errno.into(),
    })?;
    let bytes = read_bounded(&mut file, path)?;
    let read = parse(&bytes);
    let (entries, result) = decide(&read).map_err(UpdateError::Refused)?;
    let mut out = Vec::new();
    for entry in &entries {
        serde_json::to_writer(&mut out, entry).map_err(LedgerError::Encode)?;
        out.push(b'\n');
    }
    // One write: a reader without the lock sees all of the lines or none.
    file.write_all(&out)
        .map_err(|source| LedgerError::Unwritable {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(result)
}

fn read_bounded(file: &mut File, path: &Path) -> Result<Vec<u8>, LedgerError> {
    let mut bytes = Vec::new();
    file.take(BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LedgerError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > BYTES_MAX) {
        return Err(LedgerError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}

fn open_error(path: &Path, error: OpenRegularError) -> LedgerError {
    match error {
        OpenRegularError::Io(source) => LedgerError::Unreadable {
            path: path.to_path_buf(),
            source,
        },
        OpenRegularError::NotRegular(kind) => LedgerError::NotRegular {
            path: path.to_path_buf(),
            kind,
        },
    }
}
