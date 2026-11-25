//! The replay's arithmetic and its tables.

use std::collections::BTreeMap;
use std::fmt;

use crate::pending::turn_end_is_pending;
use crate::transcript::Summary;
use crate::types::{Age, Instant, TurnTrigger, secs_since};

/// One session's history, reduced to what the replay reads.
#[derive(Debug, Clone)]
pub struct Timeline {
    /// Every turn end, oldest first.
    turn_ends: Vec<(Instant, TurnTrigger)>,
    /// Every human prompt, oldest first.
    prompts: Vec<Instant>,
    last_record_at: Option<Instant>,
    line_errors: u32,
    asks_made: u32,
    asks_answered: u32,
}

impl Timeline {
    pub fn of(summary: &Summary) -> Self {
        Self {
            turn_ends: summary
                .turn_ends
                .iter()
                .map(|end| (end.at, end.trigger.clone()))
                .collect(),
            prompts: summary.human_prompt_times.clone(),
            last_record_at: summary.last_record_at,
            line_errors: summary.line_errors,
            asks_made: summary.asks_made,
            asks_answered: summary.asks_answered,
        }
    }
}

/// What the page would have shown just before a human prompt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Before {
    /// A card, started by this trigger.
    Card(String),
    /// A turn was running: the human prompted after the latest turn end, before the next.
    Running,
    /// Nothing had ended in the session yet.
    NothingYet,
}

/// The replay's results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    since: Option<Instant>,
    sessions: usize,
    turn_ends: BTreeMap<String, u32>,
    prompts: BTreeMap<Before, u32>,
    /// Turn ends no human prompt ever followed, by trigger.
    noise: BTreeMap<String, u32>,
    /// Seconds from each of those to the next event, ascending.
    noise_ages_secs: Vec<u64>,
    asks_made: u32,
    asks_answered: u32,
    /// Sessions by their line-error count, in [`ERROR_BUCKETS`].
    errors: [u32; ERROR_BUCKETS.len()],
    unreadable: Vec<String>,
}

/// The line-error counts a bucket holds, inclusive; the last is open-ended.
const ERROR_BUCKETS: [(&str, u32, u32); 5] = [
    ("0", 0, 0),
    ("1", 1, 1),
    ("2-9", 2, 9),
    ("10-99", 10, 99),
    ("100+", 100, u32::MAX),
];

fn label(trigger: &TurnTrigger) -> String {
    match trigger {
        TurnTrigger::Human => "human".to_owned(),
        TurnTrigger::TaskNotification => "task-notification".to_owned(),
        TurnTrigger::Peer => "peer".to_owned(),
        TurnTrigger::Scheduled => "scheduled".to_owned(),
        TurnTrigger::Coordinator => "coordinator".to_owned(),
        TurnTrigger::AutoContinuation => "auto-continuation".to_owned(),
        TurnTrigger::LocalCommand => "local-command".to_owned(),
        TurnTrigger::CompactionSummary => "compaction-summary".to_owned(),
        TurnTrigger::Unknown(other) => format!("unknown:{other}"),
    }
}

/// What the page showed just before the prompt at index `at` of the session's prompts: the
/// latest turn end before it is a card unless a human prompt came after that turn end.
fn before_prompt(timeline: &Timeline, at: usize) -> Before {
    let Some(&prompt) = timeline.prompts.get(at) else {
        return Before::NothingYet;
    };
    let latest_end = timeline
        .turn_ends
        .iter()
        .rev()
        .find(|(ended, _)| *ended < prompt);
    let previous_prompt = at.checked_sub(1).and_then(|i| timeline.prompts.get(i));
    match latest_end {
        None => Before::NothingYet,
        Some((ended, trigger)) => {
            if turn_end_is_pending(*ended, previous_prompt.copied()) {
                Before::Card(label(trigger))
            } else {
                Before::Running
            }
        }
    }
}

/// Seconds from a turn end to the next event of the session: the next turn end or prompt, or
/// the end of the file when nothing follows.
fn age_to_next_event(timeline: &Timeline, ended: Instant) -> u64 {
    let next_end = timeline
        .turn_ends
        .iter()
        .map(|(at, _)| *at)
        .find(|at| *at > ended);
    let next_prompt = timeline.prompts.iter().copied().find(|at| *at > ended);
    let next = [next_end, next_prompt]
        .into_iter()
        .flatten()
        .min()
        .or(timeline.last_record_at)
        .unwrap_or(ended);
    secs_since(ended, next)
}

/// Nearest-rank percentile of an ascending list: the value at rank `ceil(q * n)`, `q` being
/// `numerator / 100`.
fn percentile(sorted: &[u64], numerator: usize) -> Option<u64> {
    let rank = (numerator * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// The tables for `timelines`, counting only what happened on or after `since`.
///
/// A prompt is judged against the whole history before it, so a prompt just after `since`
/// still sees the card an earlier turn end left; only the events counted start at `since`.
pub fn analyse(timelines: &[Timeline], since: Option<Instant>, unreadable: Vec<String>) -> Report {
    let counted = |at: Instant| since.is_none_or(|since| at >= since);
    let mut report = Report {
        since,
        sessions: timelines.len(),
        turn_ends: BTreeMap::new(),
        prompts: BTreeMap::new(),
        noise: BTreeMap::new(),
        noise_ages_secs: Vec::new(),
        asks_made: 0,
        asks_answered: 0,
        errors: [0; ERROR_BUCKETS.len()],
        unreadable,
    };
    for timeline in timelines {
        for (at, trigger) in &timeline.turn_ends {
            if !counted(*at) {
                continue;
            }
            *report.turn_ends.entry(label(trigger)).or_insert(0) += 1;
            if !timeline.prompts.iter().any(|prompt| prompt > at) {
                *report.noise.entry(label(trigger)).or_insert(0) += 1;
                report
                    .noise_ages_secs
                    .push(age_to_next_event(timeline, *at));
            }
        }
        for (index, at) in timeline.prompts.iter().enumerate() {
            if counted(*at) {
                *report
                    .prompts
                    .entry(before_prompt(timeline, index))
                    .or_insert(0) += 1;
            }
        }
        report.asks_made += timeline.asks_made;
        report.asks_answered += timeline.asks_answered;
        if let Some(bucket) = ERROR_BUCKETS
            .iter()
            .position(|(_, low, high)| (*low..=*high).contains(&timeline.line_errors))
            && let Some(count) = report.errors.get_mut(bucket)
        {
            *count += 1;
        }
    }
    report.noise_ages_secs.sort_unstable();
    report
}

/// Rows as columns padded to their widest cell; the last column is not padded.
fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|row| row.get(c))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(c, cell)| {
                if c == last {
                    cell.clone()
                } else {
                    format!(
                        "{cell:<width$}",
                        width = widths.get(c).copied().unwrap_or(0)
                    )
                }
            })
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

fn row<const N: usize>(cells: [String; N]) -> Vec<String> {
    cells.into()
}

fn counted(map: &BTreeMap<String, u32>, header: [&str; 2]) -> String {
    let total: u32 = map.values().sum();
    let mut rows = vec![row(header.map(str::to_owned))];
    rows.extend(map.iter().map(|(k, v)| row([k.clone(), v.to_string()])));
    rows.push(row(["total".to_owned(), total.to_string()]));
    table(&rows)
}

/// `part` of `whole` as a whole percentage, rounded down; `-` when there is no whole.
fn percent(part: u32, whole: u32) -> String {
    if whole == 0 {
        "-".to_owned()
    } else {
        format!("{}%", u64::from(part) * 100 / u64::from(whole))
    }
}

fn age(secs: Option<u64>) -> String {
    secs.map_or_else(|| "-".to_owned(), |secs| Age::from_secs(secs).to_string())
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.since {
            Some(since) => writeln!(
                f,
                "conn replay: {} sessions with a record on or after {since}",
                self.sessions
            )?,
            None => writeln!(f, "conn replay: {} sessions, all history", self.sessions)?,
        }

        writeln!(f, "\nTurn ends by trigger")?;
        f.write_str(&counted(&self.turn_ends, ["trigger", "count"]))?;

        let total: u32 = self.prompts.values().sum();
        let hits: u32 = self
            .prompts
            .iter()
            .filter(|(before, _)| matches!(before, Before::Card(_)))
            .map(|(_, n)| n)
            .sum();
        writeln!(f, "\nHuman prompts: what the page showed just before each")?;
        let mut rows = vec![row(["before the prompt".to_owned(), "prompts".to_owned()])];
        for (before, n) in &self.prompts {
            let name = match before {
                Before::Card(trigger) => format!("a card, started by {trigger}"),
                Before::Running => "no card: typed while a turn was running".to_owned(),
                Before::NothingYet => "no card: no turn had ended yet".to_owned(),
            };
            rows.push(row([name, n.to_string()]));
        }
        rows.push(row(["total".to_owned(), total.to_string()]));
        f.write_str(&table(&rows))?;
        writeln!(f, "hit rate (a card was showing): {}", percent(hits, total))?;

        writeln!(
            f,
            "\nTurn ends no human prompt followed in the file (the noise)"
        )?;
        f.write_str(&counted(&self.noise, ["trigger", "count"]))?;
        writeln!(
            f,
            "age until the next event: median {}, p90 {}",
            age(percentile(&self.noise_ages_secs, 50)),
            age(percentile(&self.noise_ages_secs, 90))
        )?;

        writeln!(f, "\nAskUserQuestion calls (whole files)")?;
        writeln!(
            f,
            "made {}, answered {}, unanswered {}",
            self.asks_made,
            self.asks_answered,
            self.asks_made.saturating_sub(self.asks_answered)
        )?;

        writeln!(f, "\nLine errors per session")?;
        let mut rows = vec![row(["errors".to_owned(), "sessions".to_owned()])];
        rows.extend(
            ERROR_BUCKETS
                .iter()
                .zip(self.errors)
                .map(|((name, _, _), n)| row([(*name).to_owned(), n.to_string()])),
        );
        f.write_str(&table(&rows))?;

        if !self.unreadable.is_empty() {
            writeln!(f, "\nNot read ({}):", self.unreadable.len())?;
            for line in &self.unreadable {
                writeln!(f, "  {line}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The report on `tests/fixtures/replay`, whose five sessions are written so the tables
    //! can be derived by hand (see the comments on each expectation), and the pieces that
    //! the fixture cannot reach.

    use std::path::Path;

    use super::*;
    use crate::replay::read_timelines;

    fn since() -> Instant {
        "2026-10-01T00:00:00Z".parse().unwrap()
    }

    fn fixture_report() -> Report {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay/claude");
        let read = read_timelines(&dir, Some(since()));
        assert_eq!(read.unreadable, Vec::<String>::new());
        analyse(&read.timelines, Some(since()), read.unreadable)
    }

    /// The fixture's five sessions, as written in `tests/fixtures/replay/claude/projects`:
    ///
    /// - `a1` (counted): prompts 10:00, 10:05, 10:30; turn ends 10:01 human, 10:06 human, 10:08
    ///   task-notification, 10:31 human; an ask at 10:35 never answered; last record 10:40.
    /// - `b2` (counted, one garbage line): prompts 11:00:00, 11:00:30, 11:02:00, 11:02:10; turn
    ///   ends 11:01 and 11:03 human; one ask answered; last record 11:13.
    /// - `c3`: every record on 2026-09-30: not counted at all.
    /// - `d4` (counted): human prompt 09-30 21:00 and peer prompt 22:00, the peer's turn end 23:00 (all
    ///   before `since`, so none counted, but the prompt below still sees the card);
    ///   human prompt 10-01 00:10, turn end 00:20 human; last record 00:50.
    /// - `e5` (counted): one human prompt at 12:00 and nothing else.
    const EXPECTED: &str = "\
conn replay: 4 sessions with a record on or after 2026-10-01T00:00:00Z\n\
\n\
Turn ends by trigger\n\
trigger            count\n\
human              6\n\
task-notification  1\n\
total              7\n\
\n\
Human prompts: what the page showed just before each\n\
before the prompt                        prompts\n\
a card, started by human                 2\n\
a card, started by peer                  1\n\
a card, started by task-notification     1\n\
no card: typed while a turn was running  1\n\
no card: no turn had ended yet           4\n\
total                                    9\n\
hit rate (a card was showing): 44%\n\
\n\
Turn ends no human prompt followed in the file (the noise)\n\
trigger  count\n\
human    3\n\
total    3\n\
age until the next event: median 10 min, p90 30 min\n\
\n\
AskUserQuestion calls (whole files)\n\
made 2, answered 1, unanswered 1\n\
\n\
Line errors per session\n\
errors  sessions\n\
0       3\n\
1       1\n\
2-9     0\n\
10-99   0\n\
100+    0\n\
";

    /// Mutation: judge a prompt against the *latest* turn end instead of the latest before it,
    /// drop the `previous_prompt` comparison, count turn ends before `since`, or take the
    /// median as the upper middle.
    #[test]
    fn the_tables_for_the_fixture_are_the_ones_derived_by_hand() {
        let shown = fixture_report().to_string();
        // The derivation, so a mismatch can be located:
        // turn ends  human 3 (a1) + 2 (b2) + 1 (d4) = 6, task-notification 1 (a1); d4's 23:00
        //            peer end is before `since`.
        // prompts    a1: 10:00 nothing yet; 10:05 card(human, E1 10:01); 10:30 card(task-notification,
        //            E3 10:08).  b2: 11:00:00 and 11:00:30 nothing yet; 11:02:00 card(human, E1 11:01);
        //            11:02:10 running (E1 < 11:02:00 prompt).  d4: 00:10 card(peer, E 23:00; no
        //            earlier human prompt).  e5: nothing yet.  Cards 4 of 9 = 44%.
        // noise      a1's 10:31 (to 10:40: 540 s), b2's 11:03 (to 11:13: 600 s), d4's 00:20 (to 00:50:
        //            1800 s); nearest-rank median ceil(1.5)=2 -> 600 s, p90 ceil(2.7)=3 -> 1800 s.
        assert_eq!(shown, EXPECTED);
    }

    /// A turn end and a prompt at the same instant: the end is not before the prompt and the
    /// prompt does not follow the end, as in the live rule, which needs a strictly later time.
    /// Mutation: `<` to `<=` in `before_prompt`, `>` to `>=` there, or `>` to `>=` in `analyse`.
    #[test]
    fn simultaneous_events_are_not_ordered() {
        let at = |text: &str| -> Instant { text.parse().unwrap() };
        let timeline = |ends: &[&str], prompts: &[&str]| Timeline {
            turn_ends: ends.iter().map(|t| (at(t), TurnTrigger::Human)).collect(),
            prompts: prompts.iter().map(|t| at(t)).collect(),
            last_record_at: Some(at("2026-10-01T09:00:00Z")),
            line_errors: 0,
            asks_made: 0,
            asks_answered: 0,
        };
        // An end at the very instant of the first prompt is not before it.
        let first = timeline(&["2026-10-01T08:00:00Z"], &["2026-10-01T08:00:00Z"]);
        assert_eq!(before_prompt(&first, 0), Before::NothingYet);
        // An end at the instant of the previous prompt is not after it: the turn was running.
        let second = timeline(
            &["2026-10-01T08:00:00Z"],
            &["2026-10-01T08:00:00Z", "2026-10-01T08:30:00Z"],
        );
        assert_eq!(before_prompt(&second, 1), Before::Running);
        // A prompt at the instant of the end does not follow it: the end is noise.
        let report = analyse(
            &[timeline(
                &["2026-10-01T08:00:00Z"],
                &["2026-10-01T08:00:00Z"],
            )],
            None,
            Vec::new(),
        );
        assert_eq!(report.noise.get("human"), Some(&1));
    }

    /// Mutation: round the rank down (`n * q / 100`) or index from the end.
    #[test]
    fn percentiles_are_nearest_rank() {
        let ages = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&ages, 50), Some(50));
        assert_eq!(percentile(&ages, 90), Some(90));
        assert_eq!(percentile(&ages[..3], 50), Some(20));
        assert_eq!(percentile(&ages[..3], 90), Some(30));
        assert_eq!(percentile(&ages[..1], 90), Some(10));
        assert_eq!(percentile(&[], 50), None);
    }

    /// Mutation: bucket `10` with `2-9` (change the bounds), or drop the open end.
    #[test]
    fn line_error_buckets_cover_every_count_once() {
        for count in [0, 1, 2, 9, 10, 99, 100, u32::MAX] {
            let hits = ERROR_BUCKETS
                .iter()
                .filter(|(_, low, high)| (*low..=*high).contains(&count))
                .count();
            assert_eq!(hits, 1, "{count}");
        }
    }
}
