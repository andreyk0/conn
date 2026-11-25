//! `conn replay`: the live derivations run over historical transcripts, as hit-rate tables.
//!
//! The pending rule is simulated on each transcript alone (no event log): a turn end is a
//! card from the moment it ends until the next human prompt. At each human prompt the replay
//! asks what the page would have shown just before it. A high hit rate for a trigger says its
//! cards are what the human then acted on; turn ends that no prompt ever followed are the
//! noise the rule would have put on the page.
#![allow(
    clippy::print_stdout,
    reason = "replay's product is a report on stdout"
)]

mod report;

use std::path::Path;

use jiff::tz::TimeZone;

use crate::config::{Config, ConfigError};
use crate::sessions::transcripts::transcripts;
use crate::transcript::scan;
use crate::types::Instant;

pub use report::{Report, Timeline, analyse};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("--since {0} is not a date this machine's time zone can place")]
    Since(jiff::civil::Date),
}

/// Replays the transcripts of the configured Claude directory whose last record is on or
/// after the start of `since` in this machine's time zone (every transcript when `None`),
/// and prints the tables.
pub fn run(since: Option<jiff::civil::Date>) -> Result<(), ReplayError> {
    let config = Config::load()?;
    let since = since.map(start_of_day).transpose()?;
    let timelines = read_timelines(&config.paths.claude_dir, since);
    print!(
        "{}",
        analyse(&timelines.timelines, since, timelines.unreadable)
    );
    Ok(())
}

fn start_of_day(date: jiff::civil::Date) -> Result<Instant, ReplayError> {
    date.to_zoned(TimeZone::system())
        .map(|zoned| zoned.timestamp())
        .map_err(|_| ReplayError::Since(date))
}

/// What reading the transcripts gave.
pub struct Timelines {
    pub timelines: Vec<Timeline>,
    /// Transcripts that could not be read: their sessions are missing from the tables.
    pub unreadable: Vec<String>,
}

/// A timeline for every transcript that was touched on or after `since` and whose last
/// record is too. A file last modified before `since` cannot hold a later record.
pub fn read_timelines(claude_dir: &Path, since: Option<Instant>) -> Timelines {
    let found = transcripts(claude_dir);
    let mut out = Timelines {
        timelines: Vec::new(),
        unreadable: found
            .skipped
            .iter()
            .map(|(path, reason)| format!("{}: {reason}", path.display()))
            .collect(),
    };
    for file in found
        .files
        .iter()
        .filter(|f| since.is_none_or(|since| f.modified >= since))
    {
        match scan(&file.path) {
            Ok(summary) if since.is_none_or(|since| summary.last_record_at >= Some(since)) => {
                out.timelines.push(Timeline::of(&summary));
            }
            Ok(_) => {}
            Err(error) => out.unreadable.push(error.to_string()),
        }
    }
    out
}
