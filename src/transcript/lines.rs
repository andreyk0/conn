//! Bytes to records, with a bound on how much of one line is ever held.
//!
//! `scan` and `Tailer` both read through [`Feed`], so the line boundaries, the size bound and
//! the parse are one implementation. What a chunk boundary does to a line is the only thing
//! that differs between them, and [`Feed`] is built so that it does nothing.

use super::parse::{LineError, parse_line};
use super::record::Record;

/// The longest line, newline excluded, that is parsed. A longer one is one counted
/// [`LineError::TooLong`] and its bytes are dropped as they arrive.
///
/// Derivation: the largest legitimate records are inline base64 images and tool results,
/// which Claude Code spills to disk beyond a few tens of KiB; 4 MiB leaves two orders of
/// magnitude of headroom while capping what one hostile or runaway line can cost.
pub const LINE_BYTES_MAX: usize = 4 * 1024 * 1024;

const NEWLINE: u8 = b'\n';

/// What the splitter holds of the line it has not yet seen the end of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Partial {
    /// The bytes so far, at most [`LINE_BYTES_MAX`] of them.
    Held(Vec<u8>),
    /// The line already exceeded the bound; only its length so far is kept.
    Overflowed { bytes_seen: u64 },
}

/// Splits a byte stream into lines of at most [`LINE_BYTES_MAX`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Feed {
    partial: Partial,
    /// Every byte ever pushed.
    pushed: u64,
}

impl Feed {
    pub(super) const fn new() -> Self {
        Self {
            partial: Partial::Held(Vec::new()),
            pushed: 0,
        }
    }

    /// A feed whose stream is known to have been consumed up to `offset`, a line boundary.
    pub(super) const fn starting_at(offset: u64) -> Self {
        Self {
            partial: Partial::Held(Vec::new()),
            pushed: offset,
        }
    }

    /// Bytes of the line still waiting for its newline.
    pub(super) fn unterminated(&self) -> u64 {
        match &self.partial {
            Partial::Held(bytes) => len_u64(bytes.len()),
            Partial::Overflowed { bytes_seen } => *bytes_seen,
        }
    }

    /// Bytes pushed that belong to lines already read: the offset a later reader can resume at.
    pub(super) fn consumed(&self) -> u64 {
        self.pushed.saturating_sub(self.unterminated())
    }

    /// Takes the next bytes of the stream and returns the lines they completed, in order.
    ///
    /// The result depends only on the concatenation of everything pushed, never on where the
    /// chunks were cut. A blank line is not a record and yields nothing.
    pub(super) fn push(&mut self, chunk: &[u8]) -> Vec<Result<Record, LineError>> {
        self.pushed = self.pushed.saturating_add(len_u64(chunk.len()));
        let mut lines = Vec::new();
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|byte| *byte == NEWLINE) {
            let (head, tail) = rest.split_at_checked(newline).unwrap_or((rest, &[]));
            rest = tail.get(1..).unwrap_or_default();
            if let Some(line) = self.finish_line(head) {
                lines.push(line);
            }
        }
        self.extend_partial(rest);
        lines
    }

    fn finish_line(&mut self, last_bytes: &[u8]) -> Option<Result<Record, LineError>> {
        let partial = std::mem::replace(&mut self.partial, Partial::Held(Vec::new()));
        match partial {
            Partial::Overflowed { .. } => Some(Err(LineError::TooLong)),
            Partial::Held(held) if held.len().saturating_add(last_bytes.len()) > LINE_BYTES_MAX => {
                Some(Err(LineError::TooLong))
            }
            Partial::Held(held) if held.is_empty() => decode(last_bytes),
            Partial::Held(mut held) => {
                held.extend_from_slice(last_bytes);
                decode(&held)
            }
        }
    }

    fn extend_partial(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let overflow = match &mut self.partial {
            Partial::Overflowed { bytes_seen } => {
                *bytes_seen = bytes_seen.saturating_add(len_u64(bytes.len()));
                return;
            }
            Partial::Held(held) if held.len().saturating_add(bytes.len()) <= LINE_BYTES_MAX => {
                held.extend_from_slice(bytes);
                return;
            }
            Partial::Held(held) => len_u64(held.len()).saturating_add(len_u64(bytes.len())),
        };
        self.partial = Partial::Overflowed {
            bytes_seen: overflow,
        };
    }
}

fn decode(line: &[u8]) -> Option<Result<Record, LineError>> {
    if line.is_empty() {
        return None;
    }
    Some(
        std::str::from_utf8(line)
            .map_err(|_| LineError::NotUtf8)
            .and_then(parse_line),
    )
}

pub(super) fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}
