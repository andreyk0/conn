//! Reading the append-only event log, whole or from an offset.
//!
//! Every line is accounted for: a line that carries no usable event is yielded as a
//! [`LogLineError`] in its place, never skipped, so a caller can say how many events it
//! could not use and why.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::RawEvent;
use super::hook::PAYLOAD_BYTES_MAX;
use crate::types::Instant;
use crate::util::fs::{NonRegular, OpenRegularError, open_regular};

/// The longest line a reader accepts.
///
/// A line is at most the payload cap re-encoded. JSON in and JSON out differ little, but the
/// `unparsed` form can turn one stdin byte into a six-byte `\u00XX` escape, so the bound is
/// six times the cap plus room for the keys and the timestamp.
const LINE_BYTES_MAX: u64 = 6 * PAYLOAD_BYTES_MAX + 64 * 1024;

/// One line as `conn hook` writes it; the keys select the variant.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum LogLine {
    Payload {
        received_at: Instant,
        payload: serde_json::Value,
    },
    /// Stdin was not JSON; kept so the failure is visible.
    Unparsed {
        received_at: Instant,
        unparsed: String,
    },
    /// Stdin exceeded the cap; only its size is kept.
    Oversized {
        received_at: Instant,
        oversized: u64,
    },
}

/// A line of the log that is not an event, or the log itself being unreadable.
///
/// `Unreadable`, `NotRegular`, `Shrunk` and `Truncated` mean the log could not be read in full; the rest
/// mean a line was read and holds no usable event. Offsets are the line's first byte.
#[derive(Debug, thiserror::Error)]
pub enum LogLineError {
    #[error("cannot read event log {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The log path is something other than a file; it was not read.
    #[error("event log {} is not a regular file: it is {kind}", path.display())]
    NotRegular { path: PathBuf, kind: NonRegular },
    /// The log is shorter than the offset a reader had reached, which an append-only file
    /// never is.
    #[error("event log {} is {len} bytes, shorter than offset {offset}", path.display())]
    Shrunk {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
    #[error("event log line at byte {offset}: not a log line: {source}")]
    Malformed {
        offset: u64,
        source: serde_json::Error,
    },
    #[error("event log line at byte {offset} is longer than {LINE_BYTES_MAX} bytes")]
    TooLong { offset: u64 },
    /// The last line has no terminating newline: a write in progress, or a torn one.
    #[error("event log line at byte {offset} has no terminating newline")]
    Truncated { offset: u64 },
    /// The hook received stdin that was not JSON; the text is in the file, not here.
    #[error("event log line at byte {offset} ({received_at}): hook stdin was not JSON")]
    Unparsed { offset: u64, received_at: Instant },
    #[error(
        "event log line at byte {offset} ({received_at}): hook stdin was {bytes} bytes, over the cap"
    )]
    Oversized {
        offset: u64,
        received_at: Instant,
        bytes: u64,
    },
}

/// What one incremental read found.
#[derive(Debug)]
pub struct ReadBatch {
    pub events: Vec<Result<RawEvent, LogLineError>>,
    /// Where the next read starts: after the last complete line, so a line still being
    /// written is read whole next time.
    pub next_offset: u64,
}

/// The event log file. Holding one touches nothing on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every line, streamed. A missing file is an `Unreadable` item with kind `NotFound`: no
    /// hook has run yet, or the state directory is not the one the hook writes to, and the
    /// reader cannot tell which.
    pub fn read(&self) -> impl Iterator<Item = Result<RawEvent, LogLineError>> {
        LogLines::open(&self.path, 0, PartialLine::Report, LINE_BYTES_MAX)
    }

    /// The complete lines from `offset` on. Start at 0, then pass each `next_offset` back.
    pub fn read_from(&self, offset: u64) -> ReadBatch {
        let mut lines = LogLines::open(&self.path, offset, PartialLine::Defer, LINE_BYTES_MAX);
        let events = lines.by_ref().collect();
        ReadBatch {
            events,
            next_offset: lines.offset,
        }
    }
}

/// What to do with a last line that has no newline yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialLine {
    /// Yield [`LogLineError::Truncated`]: a whole-file read has nothing to wait for.
    Report,
    /// Leave it for the next read, which starts at its first byte.
    Defer,
}

enum Source {
    Reading(BufReader<File>),
    /// The error to yield once, then the iterator ends.
    Failed(Option<LogLineError>),
    Finished,
}

struct LogLines {
    path: PathBuf,
    source: Source,
    /// The first byte not yet consumed; only complete lines advance it.
    offset: u64,
    partial: PartialLine,
    line_bytes_max: u64,
}

impl LogLines {
    fn open(path: &Path, offset: u64, partial: PartialLine, line_bytes_max: u64) -> Self {
        let source = match open_at(path, offset) {
            Ok(reader) => Source::Reading(reader),
            Err(error) => Source::Failed(Some(error)),
        };
        Self {
            path: path.to_path_buf(),
            source,
            offset,
            partial,
            line_bytes_max,
        }
    }

    fn unreadable(&self, source: std::io::Error) -> LogLineError {
        LogLineError::Unreadable {
            path: self.path.clone(),
            source,
        }
    }

    /// Reads one line from `reader`; `None` ends the iteration.
    fn next_line(
        &mut self,
        reader: &mut BufReader<File>,
    ) -> Result<Option<Result<RawEvent, LogLineError>>, LogLineError> {
        let start = self.offset;
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(self.line_bytes_max + 1)
            .read_until(b'\n', &mut line)
            .map_err(|source| self.unreadable(source))?;
        if read == 0 {
            return Ok(None);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            self.offset += widen(read);
            return Ok(Some(decode(&line, start)));
        }
        if widen(read) > self.line_bytes_max {
            let rest = reader
                .skip_until(b'\n')
                .map_err(|source| self.unreadable(source))?;
            self.offset += widen(read) + widen(rest);
            return Ok(Some(Err(LogLineError::TooLong { offset: start })));
        }
        match self.partial {
            PartialLine::Report => Ok(Some(Err(LogLineError::Truncated { offset: start }))),
            PartialLine::Defer => Ok(None),
        }
    }
}

impl Iterator for LogLines {
    type Item = Result<RawEvent, LogLineError>;

    fn next(&mut self) -> Option<Self::Item> {
        match std::mem::replace(&mut self.source, Source::Finished) {
            Source::Finished => None,
            Source::Failed(error) => error.map(Err),
            Source::Reading(mut reader) => match self.next_line(&mut reader) {
                Ok(Some(item)) => {
                    // A reported partial line is the last one; every other item continues.
                    let ends_here = matches!(item, Err(LogLineError::Truncated { .. }));
                    if !ends_here {
                        self.source = Source::Reading(reader);
                    }
                    Some(item)
                }
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            },
        }
    }
}

fn open_at(path: &Path, offset: u64) -> Result<BufReader<File>, LogLineError> {
    let unreadable = |source| LogLineError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let mut file =
        open_regular(OpenOptions::new().read(true), path).map_err(|error| match error {
            OpenRegularError::Io(source) => unreadable(source),
            OpenRegularError::NotRegular(kind) => LogLineError::NotRegular {
                path: path.to_path_buf(),
                kind,
            },
        })?;
    let len = file.metadata().map_err(unreadable)?.len();
    if len < offset {
        return Err(LogLineError::Shrunk {
            path: path.to_path_buf(),
            offset,
            len,
        });
    }
    file.seek(SeekFrom::Start(offset)).map_err(unreadable)?;
    Ok(BufReader::new(file))
}

fn decode(line: &[u8], offset: u64) -> Result<RawEvent, LogLineError> {
    match serde_json::from_slice::<LogLine>(line) {
        Ok(LogLine::Payload {
            received_at,
            payload,
        }) => Ok(RawEvent {
            received_at,
            payload,
        }),
        Ok(LogLine::Unparsed { received_at, .. }) => Err(LogLineError::Unparsed {
            offset,
            received_at,
        }),
        Ok(LogLine::Oversized {
            received_at,
            oversized,
        }) => Err(LogLineError::Oversized {
            offset,
            received_at,
            bytes: oversized,
        }),
        Err(source) => Err(LogLineError::Malformed { offset, source }),
    }
}

/// A byte count as `u64`; a platform where it does not fit is not one this tool runs on, and
/// saturating keeps the offset from wrapping to a smaller one.
pub(super) fn widen(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testutil::at;
    use crate::testutil::Scratch;
    use std::io::Write;

    const GOOD_A: &str = r#"{"received_at":"2026-10-28T12:00:00Z","payload":{"n":1}}"#;
    const GOOD_B: &str = r#"{"received_at":"2026-10-28T12:00:01Z","payload":{"n":2}}"#;

    fn log_with(dir: &Scratch, content: &str) -> EventLog {
        let path = dir.join("events.jsonl");
        std::fs::write(&path, content).unwrap();
        EventLog::open(path)
    }

    fn append(log: &EventLog, content: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn n(event: &RawEvent) -> i64 {
        event.payload["n"].as_i64().unwrap()
    }

    #[test]
    fn every_line_is_accounted_for_in_order() {
        let dir = Scratch::new("log");
        let log = log_with(
            &dir,
            &format!(
                "{GOOD_A}\nnot json\n{}\n{}\n{GOOD_B}\n",
                r#"{"received_at":"2026-10-28T12:00:02Z","unparsed":"x"}"#,
                r#"{"received_at":"2026-10-28T12:00:03Z","oversized":17000000}"#,
            ),
        );
        let items: Vec<_> = log.read().collect();
        assert_eq!(items.len(), 5);
        assert_eq!(n(items[0].as_ref().unwrap()), 1);
        assert!(matches!(
            items[1],
            Err(LogLineError::Malformed { offset: 57, .. })
        ));
        assert!(matches!(
            &items[2],
            Err(LogLineError::Unparsed { received_at, .. }) if *received_at == at("2026-10-28T12:00:02Z")
        ));
        assert!(matches!(
            items[3],
            Err(LogLineError::Oversized {
                bytes: 17_000_000,
                ..
            })
        ));
        assert_eq!(n(items[4].as_ref().unwrap()), 2);
    }

    #[test]
    fn read_from_resumes_at_the_offset_across_a_partial_trailing_line() {
        let dir = Scratch::new("log");
        let (head, tail) = GOOD_B.split_at(20);
        let log = log_with(&dir, &format!("{GOOD_A}\n{head}"));

        let first = log.read_from(0);
        assert_eq!(first.events.len(), 1);
        assert_eq!(n(first.events[0].as_ref().unwrap()), 1);
        assert_eq!(first.next_offset, u64::try_from(GOOD_A.len() + 1).unwrap());

        // Nothing new is complete yet: no events, the offset stays at the partial line.
        let idle = log.read_from(first.next_offset);
        assert!(idle.events.is_empty());
        assert_eq!(idle.next_offset, first.next_offset);

        append(&log, &format!("{tail}\n"));
        let second = log.read_from(first.next_offset);
        assert_eq!(second.events.len(), 1);
        assert_eq!(n(second.events[0].as_ref().unwrap()), 2);
        assert_eq!(
            second.next_offset,
            u64::try_from(GOOD_A.len() + 1 + GOOD_B.len() + 1).unwrap()
        );
    }

    #[test]
    fn a_whole_read_reports_a_trailing_partial_line() {
        let dir = Scratch::new("log");
        let log = log_with(&dir, &format!("{GOOD_A}\n{}", GOOD_B.split_at(10).0));
        let items: Vec<_> = log.read().collect();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(matches!(
            items[1],
            Err(LogLineError::Truncated { offset: 57 })
        ));
    }

    #[test]
    fn an_unreadable_log_is_an_error_not_an_empty_log() {
        let dir = Scratch::new("log");
        let log = EventLog::open(dir.join("absent.jsonl"));
        let items: Vec<_> = log.read().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            Err(LogLineError::Unreadable { source, .. }) if source.kind() == std::io::ErrorKind::NotFound
        ));
        let batch = log.read_from(0);
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.next_offset, 0);
    }

    #[test]
    fn an_offset_past_the_end_is_reported_and_not_advanced() {
        let dir = Scratch::new("log");
        let log = log_with(&dir, &format!("{GOOD_A}\n"));
        let batch = log.read_from(1_000);
        assert!(matches!(
            batch.events[..],
            [Err(LogLineError::Shrunk { offset: 1_000, .. })]
        ));
        assert_eq!(batch.next_offset, 1_000);
    }

    #[test]
    fn a_line_over_the_bound_is_one_error_and_the_next_line_still_reads() {
        let dir = Scratch::new("log");
        let overlong = format!(
            r#"{{"received_at":"2026-10-28T12:00:00Z","payload":{{"n":"{}"}}}}"#,
            "x".repeat(100)
        );
        let log = log_with(&dir, &format!("{overlong}\n{GOOD_B}\n"));
        let mut lines = LogLines::open(log.path(), 0, PartialLine::Defer, 64);
        assert!(matches!(
            lines.next(),
            Some(Err(LogLineError::TooLong { offset: 0 }))
        ));
        assert_eq!(n(&lines.next().unwrap().unwrap()), 2);
        assert!(lines.next().is_none());
        assert_eq!(
            lines.offset,
            u64::try_from(overlong.len() + 1 + GOOD_B.len() + 1).unwrap()
        );
    }

    /// A log path that is a FIFO is reported as what it is, and is never waited on: a plain
    /// open for reading would block until a writer came.
    /// Mutation: open with `File::open` (the test hangs).
    #[test]
    fn a_fifo_log_is_reported_and_not_waited_on() {
        let dir = Scratch::new("log");
        let fifo = dir.join("events.jsonl");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        let batch = EventLog::open(&fifo).read_from(0);
        assert!(matches!(
            batch.events.as_slice(),
            [Err(LogLineError::NotRegular {
                kind: NonRegular::Fifo,
                ..
            })]
        ));
    }
}
