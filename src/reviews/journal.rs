//! The approvals journal: what the user approved from a review card, and whether the 👍 landed.
//!
//! An append-only file of JSON lines in the state directory, written before the outcome is
//! shown, so a crash after `gh` approved leaves the approval on record. Each line is one
//! [`Entry`]; a line that does not parse is skipped and counted, never dropped silently.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::Instant;
use crate::util::fs::{FILE_MODE, ensure_private_dir, open_regular};

pub const FILE: &str = "reviews-journal.jsonl";

/// The most entries held in memory and shown; the file keeps every one.
const SHOWN_MAX: usize = 500;

/// One thing that happened to an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Entry {
    Approved {
        at: Instant,
        /// `owner/repo#n`.
        pr: String,
        head: String,
        title: String,
        author: String,
        requester: String,
        channel: String,
        ts: String,
        /// `routine`, `look`, `unclassified` or `pending`: what the card said when approved.
        verdict: String,
        flags: Vec<String>,
    },
    Reacted {
        at: Instant,
        pr: String,
        head: String,
    },
    ReactionFailed {
        at: Instant,
        pr: String,
        head: String,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("cannot write the approvals journal {}: {source}", path.display())]
pub struct JournalError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// The journal's file and its newest entries, oldest first.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
    entries: Vec<Entry>,
    /// Lines of the file that did not parse.
    pub unreadable: usize,
}

impl Journal {
    /// The journal in `state_dir`; a missing file is an empty journal, an unreadable one is
    /// empty with the reason.
    pub fn load(state_dir: &Path) -> (Self, Option<String>) {
        let path = state_dir.join(FILE);
        let mut journal = Self {
            path,
            entries: Vec::new(),
            unreadable: 0,
        };
        let file = match std::fs::File::open(&journal.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (journal, None),
            Err(error) => {
                let reason = format!("cannot read {}: {error}", journal.path.display());
                return (journal, Some(reason));
            }
        };
        for line in BufReader::new(file).lines() {
            match line.map(|line| serde_json::from_str::<Entry>(&line)) {
                Ok(Ok(entry)) => journal.push(entry),
                Ok(Err(_)) | Err(_) => journal.unreadable = journal.unreadable.saturating_add(1),
            }
        }
        if journal.unreadable > 0 {
            tracing::warn!(lines = journal.unreadable, path = %journal.path.display(), "approvals journal lines that do not parse are skipped");
        }
        (journal, None)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Writes `entry` as one line, then holds it.
    pub fn append(&mut self, entry: Entry) -> Result<(), JournalError> {
        let error = |source| JournalError {
            path: self.path.clone(),
            source,
        };
        let mut line = serde_json::to_vec(&entry).map_err(|e| error(e.into()))?;
        line.push(b'\n');
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir).map_err(error)?;
        }
        let mut file = open_regular(
            OpenOptions::new().append(true).create(true).mode(FILE_MODE),
            &self.path,
        )
        .map_err(|e| error(std::io::Error::other(e.to_string())))?;
        // One write on an `O_APPEND` descriptor: a line is never torn.
        file.write_all(&line).map_err(error)?;
        self.push(entry);
        Ok(())
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        let excess = self.entries.len().saturating_sub(SHOWN_MAX);
        if excess > 0 {
            self.entries.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Scratch, at};

    fn reacted(pr: &str) -> Entry {
        Entry::Reacted {
            at: at("2026-10-29T10:00:00Z"),
            pr: pr.to_owned(),
            head: "aaaaaaa".to_owned(),
        }
    }

    /// What is appended is what a later load reads, and a line that does not parse is
    /// counted, not fatal. Mutation: write without the newline (two entries read as one bad
    /// line), or stop at the first bad line.
    #[test]
    fn appended_entries_load_back_and_bad_lines_are_counted() {
        let dir = Scratch::new("roundtrip");
        let (mut journal, problem) = Journal::load(&dir);
        assert_eq!(problem, None);
        journal.append(reacted("o/a#1")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(FILE))
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        journal.append(reacted("o/b#2")).unwrap();
        let (loaded, problem) = Journal::load(&dir);
        assert_eq!(problem, None);
        assert_eq!(loaded.entries(), [reacted("o/a#1"), reacted("o/b#2")]);
        assert_eq!(loaded.unreadable, 1);
    }

    #[test]
    fn the_json_names_the_event() {
        let line = serde_json::to_string(&reacted("o/a#1")).unwrap();
        assert!(line.starts_with(r#"{"event":"reacted","#), "{line}");
    }
}
