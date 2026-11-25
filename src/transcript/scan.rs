//! One pass over a whole transcript.

use std::fs::File;
use std::path::Path;

use super::TranscriptError;
use super::lines::Feed;
use super::summary::{Summary, fold_event};
use super::tail::{CHUNK_BYTES, TailEvent, read_retrying};

/// Reads `path` once, in fixed-size chunks, and folds every line.
///
/// Memory is bounded by the chunk, one line at most [`LINE_BYTES_MAX`](super::LINE_BYTES_MAX)
/// long, and the summary: a longer line is a counted error and its bytes are never kept. An
/// unreadable file is the only failure; a file full of garbage is a summary with a line-error
/// count.
pub fn scan(path: &Path) -> Result<Summary, TranscriptError> {
    let unreadable = |source| TranscriptError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(unreadable)?;
    let mut feed = Feed::new();
    let mut summary = Summary::default();
    let mut chunk = vec![0_u8; CHUNK_BYTES];
    loop {
        let read = read_retrying(&mut file, &mut chunk).map_err(unreadable)?;
        let Some(filled) = chunk.get(..read).filter(|filled| !filled.is_empty()) else {
            break;
        };
        for line in feed.push(filled) {
            fold_event(&mut summary, &TailEvent::from(line));
        }
    }
    summary.set_extent(feed.consumed(), feed.unterminated());
    Ok(summary)
}
