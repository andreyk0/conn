//! Reading Claude Code transcripts (`~/.claude/projects/<dir>/<session>.jsonl`).
//!
//! A transcript line is an internal record: unknown record kinds and trigger values are
//! carried as `Unknown`, never rejected, because the format changes between releases. A line
//! that cannot be read at all is a counted [`LineError`], never a silent skip and never fatal.
//!
//! Two ways in, one result. [`scan`] reads a whole file once; [`Tailer`] reads what was
//! appended since the last [`Tailer::poll`]. Both split bytes into lines with the same bounded
//! splitter and fold the same events with [`fold_event`], so the same bytes give the same
//! [`Summary`] however they arrive.

mod lines;
mod parse;
mod record;
mod scan;
mod summary;
mod tail;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use lines::LINE_BYTES_MAX;
pub use parse::{FieldProblem, LineError, parse_line};
pub use record::{
    Answers, AssistantRecord, Attachment, AttachmentRecord, Block, Chain, Envelope, Meta, PrLink,
    Prompt, Record, StopReason, SystemEvent, SystemRecord, ToolCall, ToolOutcome, ToolResult,
    ToolResultContent, ToolUse, UserContent, UserRecord,
};
pub use scan::scan;
pub use summary::{HUMAN_PROMPTS_KEPT, HumanPrompt, Summary, fold, fold_event};
pub use tail::{TailEvent, Tailer};

/// The file could not be read, so nothing is known about the session, or about what a
/// [`Tailer`] poll found. Every other problem is in the [`Summary`] the read would have
/// produced.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("cannot read transcript {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}
