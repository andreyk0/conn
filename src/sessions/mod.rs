//! Finding sessions: raw facts from four sources, each with its provenance.
//!
//! - [`listing`]: `claude agents --json --all`, the supervisor's view of every session.
//! - [`registry`]: `~/.claude/sessions/<pid>.json`, with [`proc`] to tell a live process from a
//!   stale file.
//! - [`launch`]: the arguments a live process was started with, which no file records.
//! - [`transcripts`]: the `.jsonl` files on disk, which outlive every process.
//! - [`windows`]: the niri window that shows a session's terminal.
//!
//! None of it classifies [`crate::types::Activity`]: that needs transcript and hook-event facts
//! this module does not read. [`resume`] turns the facts it does gather into the command a human
//! pastes to continue a dormant session.

pub mod facts;
pub mod launch;
pub mod listing;
pub mod proc;
pub mod registry;
pub mod resume;
pub mod transcripts;
pub mod windows;
