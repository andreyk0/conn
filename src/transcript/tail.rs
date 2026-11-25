//! Reading what was appended to a transcript since the last look.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::TranscriptError;
use super::lines::Feed;
use super::parse::LineError;
use super::record::Record;

/// The most bytes one [`Tailer::poll`] reads; the rest waits for the next poll.
///
/// It bounds the memory one poll holds in parsed records. Sixty-four MiB is far above what a
/// session appends between polls seconds apart, so reaching it means catching up on a file
/// that already existed, which several polls do in sequence.
pub(super) const POLL_BYTES_MAX: u64 = 64 * 1024 * 1024;

pub(super) const CHUNK_BYTES: usize = 64 * 1024;

/// One read that a signal does not turn into a failure. Zero means end of file.
pub(super) fn read_retrying(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match file.read(buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// One thing a poll found.
#[derive(Debug, Clone, PartialEq)]
pub enum TailEvent {
    Record(Record),
    /// A line that yielded no record. Counted, not dropped.
    Malformed(LineError),
    /// The file is shorter than what was already read, so it was rewritten: everything derived
    /// from the old bytes is void, and the events after this one describe the file from its
    /// start.
    Truncated,
}

impl From<Result<Record, LineError>> for TailEvent {
    fn from(line: Result<Record, LineError>) -> Self {
        match line {
            Ok(record) => Self::Record(record),
            Err(error) => Self::Malformed(error),
        }
    }
}

/// Follows one transcript file.
///
/// A line is read once, when its newline arrives; a line being written stays in the tailer
/// until it ends. The file is opened afresh on every poll, so a rename or a deletion shows up as
/// [`TranscriptError::Unreadable`] or as a truncation rather than as reads from an unlinked file.
#[derive(Debug)]
pub struct Tailer {
    path: PathBuf,
    /// Bytes of the file already read, the unterminated tail included.
    offset: u64,
    feed: Feed,
}

impl Tailer {
    /// Follows `path` from its first byte.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::starting_at(path, 0)
    }

    /// Follows `path` from `offset`, which must be a line boundary: the
    /// [`bytes_read`](super::Summary::bytes_read) of a scan of the same file.
    pub fn starting_at(path: impl Into<PathBuf>, offset: u64) -> Self {
        Self {
            path: path.into(),
            offset,
            feed: Feed::starting_at(offset),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes that belong to lines already returned; what a summary's `bytes_read` should be.
    pub fn consumed(&self) -> u64 {
        self.feed.consumed()
    }

    /// Bytes of a line that has not ended yet.
    pub fn unterminated(&self) -> u64 {
        self.feed.unterminated()
    }

    /// Reads the bytes appended since the last poll and returns what they hold, oldest first.
    pub fn poll(&mut self) -> Result<Vec<TailEvent>, TranscriptError> {
        let unreadable = |source| TranscriptError::Unreadable {
            path: self.path.clone(),
            source,
        };
        let mut file = File::open(&self.path).map_err(unreadable)?;
        let len = file.metadata().map_err(unreadable)?.len();
        let mut events = Vec::new();
        if len < self.offset {
            events.push(TailEvent::Truncated);
            self.offset = 0;
            self.feed = Feed::new();
        }
        if len == self.offset {
            return Ok(events);
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(unreadable)?;
        let mut remaining = (len - self.offset).min(POLL_BYTES_MAX);
        let mut chunk = vec![0_u8; CHUNK_BYTES];
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK_BYTES, |n| n.min(CHUNK_BYTES));
            let Some(buffer) = chunk.get_mut(..want) else {
                break;
            };
            let read = read_retrying(&mut file, buffer).map_err(unreadable)?;
            if read == 0 {
                break;
            }
            let Some(filled) = buffer.get(..read) else {
                break;
            };
            events.extend(self.feed.push(filled).into_iter().map(TailEvent::from));
            let read = u64::try_from(read).unwrap_or(u64::MAX);
            self.offset = self.offset.saturating_add(read);
            remaining = remaining.saturating_sub(read);
        }
        Ok(events)
    }
}
