//! What a session is doing, and the one card it may show: the pending rule.
//!
//! The rule is stated on [`crate::types::PendingCard`]: a session is pending when its latest
//! turn end has no later human-typed prompt, whoever started the turn; the trigger ranks and
//! never filters. The derivation is pure and cannot fail, so the module has no error type.
//!
//! The facts about a session come from three sources, each possibly absent: the supervisor's
//! listing, the registry with `/proc`, and the hook-event log; the transcript's
//! [`Summary`](crate::transcript::Summary) is always present for a known session. The builder
//! joins them into one [`Evidence`]; [`classify`] is the only function that turns that into an
//! [`Activity`] and a [`PendingCard`], and [`join_alive`] the only one that decides liveness.

use crate::sessions::facts::ProcessStatus;
use crate::sessions::listing::BackgroundState;
use crate::types::{
    Activity, AskUserQuestion, BackgroundTasks, BgState, Instant, ListingStatus, PendingCard,
    ResumeCommand, ResumeUnavailable, SessionId, SessionKind, TaskStatus, TurnEnd, WaitingFor,
};
use crate::util::redact::Redacted;

/// Whether the session's process is alive, as far as the sources can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alive {
    Yes,
    No,
    /// The sources could not say; the reason is shown, never folded into `No`.
    Unknown(String),
}

/// What the registry and the process table say about the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryOpinion {
    /// An entry whose process is the one that wrote it.
    Alive,
    /// An entry whose process is gone or whose pid was reused.
    Gone,
    /// The registry was read and has no entry for the session.
    Absent,
    /// The registry or the process table could not be read for the session.
    Cannot(String),
}

/// What the supervisor's listing says about the session's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingOpinion {
    /// An entry with a process.
    Process,
    /// No entry, or an entry without a process.
    NoProcess,
    /// The listing failed or was never read.
    Cannot(String),
}

/// Joins the two opinions about liveness.
///
/// | registry          | listing        | result                                        |
/// | ----------------- | -------------- | --------------------------------------------- |
/// | `Alive`           | any            | `Yes`: `/proc` is the direct observation      |
/// | `Gone`            | any            | `No`: likewise, even if the listing lags      |
/// | `Absent`          | `Process`      | `Yes`: the supervisor knows a process         |
/// | `Absent`          | `NoProcess`    | `No`: both were read and neither has one      |
/// | `Absent`          | `Cannot`       | `Unknown`: only one source was readable       |
/// | `Cannot`          | `Process`      | `Yes`                                         |
/// | `Cannot`          | `NoProcess`    | `No`                                          |
/// | `Cannot`          | `Cannot`       | `Unknown`                                     |
pub fn join_alive(registry: &RegistryOpinion, listing: &ListingOpinion) -> Alive {
    match (registry, listing) {
        (RegistryOpinion::Alive, _) | (RegistryOpinion::Cannot(_), ListingOpinion::Process) => {
            Alive::Yes
        }
        (RegistryOpinion::Gone, _) => Alive::No,
        (RegistryOpinion::Absent, ListingOpinion::Process) => Alive::Yes,
        (RegistryOpinion::Absent | RegistryOpinion::Cannot(_), ListingOpinion::NoProcess) => {
            Alive::No
        }
        (RegistryOpinion::Absent, ListingOpinion::Cannot(why)) => Alive::Unknown(format!(
            "no registry entry and the listing is unavailable: {why}"
        )),
        (RegistryOpinion::Cannot(why), ListingOpinion::Cannot(also)) => {
            Alive::Unknown(format!("{why}; listing: {also}"))
        }
    }
}

/// What the listing says about a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// The listing could not be had.
    Failed(String),
    /// The listing was read and the session is not in it.
    Absent,
    Listed(Listed),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Present while the supervisor sees a process.
    pub process: Option<ProcessFacts>,
    /// Present for a background session.
    pub bg_state: Option<BackgroundState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub status: Option<ProcessStatus>,
    pub waiting_for: Option<WaitingFor>,
}

/// Everything [`classify`] reads. The builder fills it from the sources; nothing else
/// reaches the rule.
#[derive(Debug, Clone)]
pub struct Evidence<'a> {
    pub kind: SessionKind,
    pub alive: Alive,
    pub listing: Listing,
    /// When the process started, from the listing or the registry.
    pub started_at: Option<Instant>,
    /// The `AskUserQuestion` no tool result has answered.
    pub pending_ask: Option<&'a AskUserQuestion>,
    /// The later of the event log's and the transcript's last human-typed prompt.
    pub last_human_prompt_at: Option<Instant>,
    /// The later of the event log's last Stop and the transcript's last turn end, as one
    /// turn end.
    pub latest_turn_end: Option<&'a TurnEnd>,
    /// The latest main-chain record of the transcript.
    pub last_record_at: Option<Instant>,
    pub transcript: TranscriptOnDisk,
    pub resume: &'a Result<ResumeCommand, ResumeUnavailable>,
}

/// Whether the session's transcript is on disk, as far as the walks of the projects directory
/// tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptOnDisk {
    Found,
    /// The latest walk was complete and found none.
    Missing,
    /// None was found, and no complete walk has run yet or the latest failed: it may be there.
    NotFoundYet,
}

/// A session's activity and the card that goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub activity: Activity,
    pub card: Option<PendingCard>,
}

impl Classified {
    const fn without_card(activity: Activity) -> Self {
        Self {
            activity,
            card: None,
        }
    }

    /// `reason` quotes strings from outside (a failed tool's message, a state name), so it is
    /// redacted here, where it becomes a value the pages can hold.
    fn unknown(reason: impl AsRef<str>) -> Self {
        Self::without_card(Activity::Unknown {
            reason: Redacted::of(reason.as_ref()),
        })
    }
}

/// The project's central rule: what a session is doing, and whether a card waits on the human.
///
/// The first line that applies decides. "Live" is [`Evidence::alive`]; a session whose
/// liveness is [`Alive::Unknown`] is `Unknown` before any line is read.
///
/// 1. Live, interactive or background, with an unanswered `AskUserQuestion`:
///    `Blocked { on: InputNeeded }`, card `Asked`. Needs no listing, so it holds when the
///    listing failed. The card's `since` is the transcript's latest record: the question is the
///    last main-chain record while it waits.
/// 2. Live and the listing says waiting (status `waiting` or a `waitingFor`):
///    `Blocked { on: <waitingFor> }`, card `Blocked`. Its `since` is the transcript's latest
///    record, else the process start: the session has been stopped at least since then.
/// 3. Live and the latest turn end is after the last human prompt (or there is no human
///    prompt): `Watching { ended_at }` when that turn end's background tasks are
///    `Known` with one `Running`, or the listing says `busy`, else `Idle { ended_at }`; card
///    `Turn`. A turn end at the same instant as the prompt is not after it. When the listing
///    failed but the registry and `/proc` say the process is alive, only this line applies of
///    those that read the listing: the card stays, the activity carries
///    [`ListingStatus::Unavailable`], and `busy` is not consulted.
/// 4. Live otherwise (a human prompt after the last turn end, or no turn end):
///    `Running { since: last human prompt, else process start }`, no card.
/// 5. Not live, and the listing shows a background session without a process:
///    `Dormant { last_seen: last record, background: Some(its state) }`, no card. It resumes
///    like any other dormant session; the state is said beside it.
/// 6. Not live, and absent from the listing or the listing failed: `Dormant { last_seen: last
///    record, background: None }`, no card (the last turn end is context on the session, not a
///    card).
///    In both 5 and 6, when a complete walk of the projects directory found no transcript, the
///    session is `Gone { background }` instead: there is nothing to resume.
/// 7. Anything the rules above cannot place is `Unknown { reason }` with no card: the listing
///    failed (for 2 to 4), the process table could not be read, the listing and `/proc`
///    disagree, a background state this code does not know, a transcript with no records, no
///    transcript found while the projects directory has not been walked completely.
///    Each is a visible failure; none is folded into another state.
///
/// Lines 3 and 4 read two times from two sources, which the builder has already merged: the
/// event log sees a turn end the moment the Stop hook fires, the transcript a moment later.
pub fn classify(session: &SessionId, evidence: &Evidence<'_>) -> Classified {
    match &evidence.alive {
        Alive::Unknown(why) => Classified::unknown(format!("cannot tell if it is running: {why}")),
        Alive::Yes => live(session, evidence),
        Alive::No => not_live(evidence),
    }
}

fn live(session: &SessionId, evidence: &Evidence<'_>) -> Classified {
    if let Some(asked) = evidence.pending_ask
        && can_ask(evidence.kind)
    {
        return match evidence.last_record_at {
            Some(since) => Classified {
                activity: Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
                card: Some(PendingCard::Asked {
                    session: session.clone(),
                    asked: asked.clone(),
                    since,
                }),
            },
            None => Classified::unknown("a question is open but the transcript has no record time"),
        };
    }
    let listed = match &evidence.listing {
        Listing::Failed(why) => return live_without_listing(session, evidence, why),
        Listing::Absent => None,
        Listing::Listed(listed) => Some(listed),
    };
    let process = listed.and_then(|l| l.process.as_ref());
    if let Some(on) = process.and_then(waiting_on) {
        return match evidence.last_record_at.or(evidence.started_at) {
            Some(since) => Classified {
                activity: Activity::Blocked { on: on.clone() },
                card: Some(PendingCard::Blocked {
                    session: session.clone(),
                    on,
                    since,
                }),
            },
            None => Classified::unknown(format!(
                "blocked on {on:?} with no record and no process start time to date it"
            )),
        };
    }
    if let Some(end) = pending_turn_end(evidence) {
        return turn_card(
            session,
            end,
            has_background_work(end, process),
            ListingStatus::Seen,
        );
    }
    match evidence.last_human_prompt_at.or(evidence.started_at) {
        Some(since) => Classified::without_card(Activity::Running { since }),
        None => Classified::unknown("running, with no prompt and no process start time to date it"),
    }
}

/// A live session whose listing could not be had. Whatever needs the listing (a wait the
/// process reports, `busy`) is unknown; the turn-end line does not, so a finished turn still
/// has its card, and anything else cannot be placed.
fn live_without_listing(session: &SessionId, evidence: &Evidence<'_>, why: &str) -> Classified {
    match pending_turn_end(evidence) {
        Some(end) => turn_card(
            session,
            end,
            has_listed_tasks(end),
            ListingStatus::Unavailable,
        ),
        None => Classified::unknown(format!("the listing is unavailable: {why}")),
    }
}

/// The latest turn end, when no human prompt follows it.
fn pending_turn_end<'e>(evidence: &Evidence<'e>) -> Option<&'e TurnEnd> {
    evidence
        .latest_turn_end
        .filter(|end| turn_end_is_pending(end.at, evidence.last_human_prompt_at))
}

fn turn_card(
    session: &SessionId,
    end: &TurnEnd,
    background_work: bool,
    listing: ListingStatus,
) -> Classified {
    Classified {
        activity: if background_work {
            Activity::Watching {
                ended_at: end.at,
                listing,
            }
        } else {
            Activity::Idle {
                ended_at: end.at,
                listing,
            }
        },
        card: Some(PendingCard::Turn {
            session: session.clone(),
            turn: end.clone(),
            since: end.at,
        }),
    }
}

/// The pending rule's one comparison: a turn end waits on the human when it is strictly later
/// than the last human prompt, or there is no prompt. A turn end at the prompt's own instant is
/// not after it.
pub fn turn_end_is_pending(end_at: Instant, last_human_prompt_at: Option<Instant>) -> bool {
    last_human_prompt_at.is_none_or(|prompt| end_at > prompt)
}

/// Interactive and background sessions can wait on a question; a headless run cannot be
/// answered.
const fn can_ask(kind: SessionKind) -> bool {
    match kind {
        SessionKind::Interactive | SessionKind::Background => true,
        SessionKind::Headless => false,
    }
}

fn waiting_on(process: &ProcessFacts) -> Option<WaitingFor> {
    match (&process.status, &process.waiting_for) {
        (_, Some(reason)) => Some(reason.clone()),
        (Some(ProcessStatus::Waiting), None) => Some(WaitingFor::Other("waiting".to_owned())),
        (
            Some(ProcessStatus::Busy | ProcessStatus::Idle | ProcessStatus::Other(_)) | None,
            None,
        ) => None,
    }
}

/// Whether work continues past the turn end: the Stop hook listed a task that is still
/// running, or the listing says the process is busy while no turn runs.
fn has_background_work(end: &TurnEnd, process: Option<&ProcessFacts>) -> bool {
    let busy = process.is_some_and(|p| p.status == Some(ProcessStatus::Busy));
    has_listed_tasks(end) || busy
}

/// Whether the Stop hook listed a task still running. A completed or failed one is over, and
/// "the source cannot say" is not a task.
fn has_listed_tasks(end: &TurnEnd) -> bool {
    match &end.background_tasks {
        BackgroundTasks::Known(tasks) => tasks.iter().any(|t| t.status == TaskStatus::Running),
        BackgroundTasks::Unknown => false,
    }
}

fn not_live(evidence: &Evidence<'_>) -> Classified {
    match &evidence.listing {
        // The process is gone, so the session is dormant whatever the listing would say; all
        // it could add is a background state.
        Listing::Failed(_) | Listing::Absent => dormant(evidence, None),
        Listing::Listed(Listed {
            process: None,
            bg_state: Some(state),
        }) => match bg_state(state) {
            Some(bg_state) => dormant(evidence, Some(bg_state)),
            None => Classified::unknown(format!(
                "a background state this code does not know: {state:?}"
            )),
        },
        Listing::Listed(Listed {
            process: Some(_), ..
        }) => Classified::unknown("the listing shows a process and /proc shows none"),
        Listing::Listed(Listed {
            process: None,
            bg_state: None,
        }) => Classified::unknown("listed with no process and no background state"),
    }
}

/// A session with no process: dormant when its transcript can resume it, gone when there is no
/// transcript to resume.
fn dormant(evidence: &Evidence<'_>, background: Option<BgState>) -> Classified {
    match (
        evidence.transcript,
        evidence.last_record_at,
        evidence.resume,
    ) {
        (TranscriptOnDisk::Missing, _, _) => {
            Classified::without_card(Activity::Gone { background })
        }
        (TranscriptOnDisk::NotFoundYet, _, _) => {
            Classified::unknown("no transcript has been found for it yet")
        }
        (TranscriptOnDisk::Found, Some(last_seen), Ok(_)) => {
            Classified::without_card(Activity::Dormant {
                last_seen,
                background,
            })
        }
        (TranscriptOnDisk::Found, None, _) => Classified::unknown("the transcript has no records"),
        (TranscriptOnDisk::Found, Some(_), Err(why)) => {
            Classified::unknown(format!("cannot form a resume command: {why}"))
        }
    }
}

fn bg_state(state: &BackgroundState) -> Option<BgState> {
    match state {
        BackgroundState::Working => Some(BgState::Working),
        BackgroundState::Blocked => Some(BgState::Blocked),
        BackgroundState::Done => Some(BgState::Done),
        BackgroundState::Failed => Some(BgState::Failed),
        BackgroundState::Stopped => Some(BgState::Stopped),
        BackgroundState::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    //! The activity table: one row per distinguished combination of (liveness, listing,
    //! pending question, turn end against prompt, background tasks), each with the exact
    //! `Activity` and card written out by hand from the rule's text, plus two sweeps of the
    //! whole product that pin relations between the columns.

    use super::*;
    use crate::testutil::secs as at;
    use crate::types::{
        AssistantText, BackgroundTask, Question, TaskKind, TaskStatus, TurnTrigger,
    };

    const T_PROMPT: i64 = 1_000;
    const T_START: i64 = 500;

    fn session() -> SessionId {
        SessionId::parse("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d").unwrap()
    }

    fn resume() -> ResumeCommand {
        ResumeCommand::new("/home/dev/proj".into(), vec![], session()).unwrap()
    }

    fn ask() -> AskUserQuestion {
        AskUserQuestion::new(vec![Question {
            header: None,
            question: Redacted::of("which one?"),
            multi_select: crate::types::MultiSelect::One,
            options: vec![],
        }])
        .unwrap()
    }

    fn task() -> BackgroundTask {
        BackgroundTask {
            id: "t1".into(),
            status: TaskStatus::Running,
            description: "watch".into(),
            kind: TaskKind::Shell {
                command: "tail -f".into(),
            },
        }
    }

    fn finished_task() -> BackgroundTask {
        BackgroundTask {
            status: TaskStatus::Completed,
            ..task()
        }
    }

    fn turn_end(secs: i64, tasks: BackgroundTasks) -> TurnEnd {
        TurnEnd {
            at: at(secs),
            trigger: TurnTrigger::Human,
            text: AssistantText::of("done"),
            background_tasks: tasks,
            asked: None,
        }
    }

    fn proc(status: Option<ProcessStatus>, waiting_for: Option<WaitingFor>) -> Listing {
        Listing::Listed(Listed {
            process: Some(ProcessFacts {
                status,
                waiting_for,
            }),
            bg_state: None,
        })
    }

    fn listed_background(state: BackgroundState) -> Listing {
        Listing::Listed(Listed {
            process: None,
            bg_state: Some(state),
        })
    }

    /// One row of the table.
    struct Row {
        name: &'static str,
        kind: SessionKind,
        alive: Alive,
        listing: Listing,
        ask: bool,
        prompt: Option<i64>,
        end: Option<(i64, BackgroundTasks)>,
        transcript: TranscriptOnDisk,
        expect: Activity,
        /// `Some(since)` for a `Turn` card with this turn end; the ask card is `Asked`.
        card: Card,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Card {
        None,
        Asked(i64),
        /// A `Blocked` card on the row's expected wait, since this time.
        Blocked(i64),
        Turn(i64),
    }

    fn row(name: &'static str) -> Row {
        Row {
            name,
            kind: SessionKind::Interactive,
            alive: Alive::Yes,
            listing: Listing::Absent,
            ask: false,
            prompt: Some(T_PROMPT),
            end: None,
            transcript: TranscriptOnDisk::Found,
            expect: Activity::Running {
                since: at(T_PROMPT),
            },
            card: Card::None,
        }
    }

    fn evaluate(row: &Row) -> Classified {
        let end = row
            .end
            .as_ref()
            .map(|(secs, tasks)| turn_end(*secs, tasks.clone()));
        let asked = ask();
        let resume = Ok(resume());
        let evidence = Evidence {
            kind: row.kind,
            alive: row.alive.clone(),
            listing: row.listing.clone(),
            started_at: Some(at(T_START)),
            pending_ask: row.ask.then_some(&asked),
            last_human_prompt_at: row.prompt.map(at),
            latest_turn_end: end.as_ref(),
            last_record_at: Some(at(2_000)),
            transcript: row.transcript,
            resume: &resume,
        };
        classify(&session(), &evidence)
    }

    fn expected_card(row: &Row) -> Option<PendingCard> {
        match row.card {
            Card::None => None,
            Card::Asked(secs) => Some(PendingCard::Asked {
                session: session(),
                asked: ask(),
                since: at(secs),
            }),
            Card::Blocked(secs) => match &row.expect {
                Activity::Blocked { on } => Some(PendingCard::Blocked {
                    session: session(),
                    on: on.clone(),
                    since: at(secs),
                }),
                Activity::Running { .. }
                | Activity::Watching { .. }
                | Activity::Idle { .. }
                | Activity::Dormant { .. }
                | Activity::Gone { .. }
                | Activity::Unknown { .. } => None,
            },
            Card::Turn(secs) => {
                let tasks = row
                    .end
                    .as_ref()
                    .map_or(BackgroundTasks::Unknown, |(_, t)| t.clone());
                Some(PendingCard::Turn {
                    session: session(),
                    turn: turn_end(secs, tasks),
                    since: at(secs),
                })
            }
        }
    }

    #[allow(clippy::too_many_lines, reason = "the table is the test")]
    fn table() -> Vec<Row> {
        let after = T_PROMPT + 100;
        let before = T_PROMPT - 100;
        let none = BackgroundTasks::Unknown;
        let empty = BackgroundTasks::Known(vec![]);
        let some = BackgroundTasks::Known(vec![task()]);
        let completed = BackgroundTasks::Known(vec![finished_task()]);
        let completed_and_running = BackgroundTasks::Known(vec![finished_task(), task()]);
        let waiting = |why: WaitingFor| proc(Some(ProcessStatus::Waiting), Some(why));
        vec![
            // Rule 1: a question outranks everything the listing says, in both live kinds.
            Row {
                ask: true,
                expect: Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
                card: Card::Asked(2_000),
                ..row("live interactive with an open question")
            },
            Row {
                ask: true,
                kind: SessionKind::Background,
                listing: waiting(WaitingFor::PermissionPrompt),
                expect: Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
                card: Card::Asked(2_000),
                ..row("live background with an open question beats a permission wait")
            },
            Row {
                ask: true,
                end: Some((after, none.clone())),
                expect: Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
                card: Card::Asked(2_000),
                ..row("an open question beats an ended turn")
            },
            Row {
                ask: true,
                listing: Listing::Failed("timed out".into()),
                expect: Activity::Blocked {
                    on: WaitingFor::InputNeeded,
                },
                card: Card::Asked(2_000),
                ..row("an open question needs no listing")
            },
            Row {
                ask: true,
                kind: SessionKind::Headless,
                expect: Activity::Running {
                    since: at(T_PROMPT),
                },
                ..row("a headless run cannot be answered, so a question is not blocked")
            },
            Row {
                ask: true,
                alive: Alive::No,
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: None,
                },
                ..row("a dead process with an open question is dormant, with no card")
            },
            // Rule 2: the listing says waiting.
            Row {
                listing: waiting(WaitingFor::PermissionPrompt),
                expect: Activity::Blocked {
                    on: WaitingFor::PermissionPrompt,
                },
                card: Card::Blocked(2_000),
                ..row("waiting on a permission prompt")
            },
            Row {
                listing: proc(Some(ProcessStatus::Waiting), None),
                expect: Activity::Blocked {
                    on: WaitingFor::Other("waiting".into()),
                },
                card: Card::Blocked(2_000),
                ..row("waiting with no reason given")
            },
            Row {
                listing: proc(Some(ProcessStatus::Busy), Some(WaitingFor::DialogOpen)),
                expect: Activity::Blocked {
                    on: WaitingFor::DialogOpen,
                },
                card: Card::Blocked(2_000),
                ..row("a reason without the status still blocks")
            },
            Row {
                listing: waiting(WaitingFor::SandboxRequest),
                end: Some((after, none.clone())),
                expect: Activity::Blocked {
                    on: WaitingFor::SandboxRequest,
                },
                card: Card::Blocked(2_000),
                ..row("waiting beats an ended turn, and is a Blocked card, not a Turn card")
            },
            Row {
                listing: waiting(WaitingFor::Other("hibernating".into())),
                expect: Activity::Blocked {
                    on: WaitingFor::Other("hibernating".into()),
                },
                card: Card::Blocked(2_000),
                ..row("an unknown reason is kept verbatim")
            },
            Row {
                listing: waiting(WaitingFor::QuotaHold),
                expect: Activity::Blocked {
                    on: WaitingFor::QuotaHold,
                },
                card: Card::Blocked(2_000),
                ..row("a quota hold is a Blocked card like any wait")
            },
            // Rule 3: the turn ended after the last prompt.
            Row {
                end: Some((after, none.clone())),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("turn ended after the prompt, nothing known of tasks")
            },
            Row {
                end: Some((after, empty.clone())),
                listing: proc(Some(ProcessStatus::Idle), None),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("turn ended, no tasks, listing idle")
            },
            Row {
                end: Some((after, some.clone())),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("turn ended with background tasks listed")
            },
            Row {
                end: Some((after, none.clone())),
                listing: proc(Some(ProcessStatus::Busy), None),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("turn ended and the listing says busy")
            },
            Row {
                end: Some((after, empty.clone())),
                listing: proc(Some(ProcessStatus::Busy), None),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("an empty task list does not cancel busy")
            },
            Row {
                end: Some((after, completed.clone())),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("a completed task is over, so it is not background work")
            },
            Row {
                end: Some((after, completed_and_running.clone())),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("one running task among completed ones is background work")
            },
            Row {
                end: Some((after, some.clone())),
                listing: proc(Some(ProcessStatus::Idle), None),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("tasks listed beat an idle listing")
            },
            Row {
                end: Some((after, none.clone())),
                listing: proc(Some(ProcessStatus::Other("hibernating".into())), None),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("an unknown status is not busy")
            },
            Row {
                end: Some((after, none.clone())),
                prompt: None,
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("no human prompt at all and a turn end is pending")
            },
            Row {
                end: Some((after, none.clone())),
                kind: SessionKind::Headless,
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("a headless run's finished turn is a card")
            },
            Row {
                end: Some((after, none.clone())),
                listing: listed_background(BackgroundState::Working),
                kind: SessionKind::Background,
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Seen,
                },
                card: Card::Turn(after),
                ..row("a live background session is classified like any live one")
            },
            // Rule 4: running.
            Row {
                end: Some((before, none.clone())),
                expect: Activity::Running {
                    since: at(T_PROMPT),
                },
                ..row("a prompt after the turn end")
            },
            Row {
                end: Some((T_PROMPT, none.clone())),
                expect: Activity::Running {
                    since: at(T_PROMPT),
                },
                ..row("a turn end at the instant of the prompt is not after it")
            },
            Row {
                end: Some((before, some.clone())),
                listing: proc(Some(ProcessStatus::Busy), None),
                expect: Activity::Running {
                    since: at(T_PROMPT),
                },
                ..row("busy with a prompt after the end is running, not watching")
            },
            Row {
                prompt: None,
                expect: Activity::Running { since: at(T_START) },
                ..row("no prompt and no turn end: running since the process started")
            },
            Row {
                prompt: None,
                listing: proc(Some(ProcessStatus::Idle), None),
                expect: Activity::Running { since: at(T_START) },
                ..row("an idle listing does not make a never-prompted session idle")
            },
            // Rule 5: a listed background session with no process is dormant, with its state.
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Done),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: Some(BgState::Done),
                },
                ..row("a finished background session")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Failed),
                end: Some((after, none.clone())),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: Some(BgState::Failed),
                },
                ..row("a failed background session gets no card for its last turn")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Stopped),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: Some(BgState::Stopped),
                },
                ..row("a stopped background session")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Blocked),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: Some(BgState::Blocked),
                },
                ..row("a blocked background session whose process is gone")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Working),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: Some(BgState::Working),
                },
                ..row("a working background session whose process is gone")
            },
            // Rule 6: dormant.
            Row {
                alive: Alive::No,
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: None,
                },
                ..row("no process and not listed")
            },
            Row {
                alive: Alive::No,
                end: Some((after, none.clone())),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: None,
                },
                ..row("dormant shows its last turn end as context, not as a card")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: None,
                },
                ..row("a background session that is unlisted and dead is dormant")
            },
            // Rules 5 and 6 with no transcript on disk: gone, or not known yet.
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Done),
                transcript: TranscriptOnDisk::Missing,
                expect: Activity::Gone {
                    background: Some(BgState::Done),
                },
                ..row("a listed background session whose transcript is gone")
            },
            Row {
                alive: Alive::No,
                transcript: TranscriptOnDisk::Missing,
                expect: Activity::Gone { background: None },
                ..row("an unlisted dead session whose transcript is gone")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Done),
                transcript: TranscriptOnDisk::NotFoundYet,
                expect: Activity::Unknown {
                    reason: Redacted::of("no transcript has been found for it yet"),
                },
                ..row("no transcript found before a complete walk is not yet known")
            },
            Row {
                alive: Alive::Yes,
                transcript: TranscriptOnDisk::Missing,
                ..row("a live session needs no transcript to be running")
            },
            // Rule 7: unknown, never folded.
            Row {
                alive: Alive::Unknown("pid domain differs".into()),
                expect: Activity::Unknown {
                    reason: Redacted::of("cannot tell if it is running: pid domain differs"),
                },
                ..row("liveness unreadable")
            },
            Row {
                alive: Alive::Unknown("x".into()),
                ask: true,
                end: Some((after, none.clone())),
                expect: Activity::Unknown {
                    reason: Redacted::of("cannot tell if it is running: x"),
                },
                ..row("liveness unreadable beats an open question and an ended turn")
            },
            Row {
                listing: Listing::Failed("timed out".into()),
                end: Some((after, none.clone())),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Unavailable,
                },
                card: Card::Turn(after),
                ..row("a live session whose turn ended keeps its card with the listing down")
            },
            Row {
                listing: Listing::Failed("timed out".into()),
                end: Some((after, some.clone())),
                expect: Activity::Watching {
                    ended_at: at(after),
                    listing: ListingStatus::Unavailable,
                },
                card: Card::Turn(after),
                ..row("running tasks make it Watching with the listing down")
            },
            Row {
                listing: Listing::Failed("timed out".into()),
                end: Some((after, completed.clone())),
                expect: Activity::Idle {
                    ended_at: at(after),
                    listing: ListingStatus::Unavailable,
                },
                card: Card::Turn(after),
                ..row("completed tasks are not work with the listing down either")
            },
            Row {
                listing: Listing::Failed("timed out".into()),
                end: Some((before, none.clone())),
                expect: Activity::Unknown {
                    reason: Redacted::of("the listing is unavailable: timed out"),
                },
                ..row("a prompt after the turn end needs the listing to tell running from blocked")
            },
            Row {
                listing: Listing::Failed("timed out".into()),
                prompt: None,
                expect: Activity::Unknown {
                    reason: Redacted::of("the listing is unavailable: timed out"),
                },
                ..row("no turn end and the listing down is unknown")
            },
            Row {
                alive: Alive::No,
                listing: Listing::Failed("timed out".into()),
                expect: Activity::Dormant {
                    last_seen: at(2_000),
                    background: None,
                },
                ..row("a dead session with the listing down is dormant, its background unsaid")
            },
            Row {
                alive: Alive::No,
                kind: SessionKind::Background,
                listing: listed_background(BackgroundState::Other("hibernating".into())),
                expect: Activity::Unknown {
                    reason: Redacted::of(
                        "a background state this code does not know: Other(\"hibernating\")",
                    ),
                },
                ..row("an unknown background state")
            },
            Row {
                alive: Alive::No,
                listing: proc(Some(ProcessStatus::Busy), None),
                expect: Activity::Unknown {
                    reason: Redacted::of("the listing shows a process and /proc shows none"),
                },
                ..row("the listing and /proc disagree")
            },
            Row {
                alive: Alive::No,
                listing: Listing::Listed(Listed {
                    process: None,
                    bg_state: None,
                }),
                expect: Activity::Unknown {
                    reason: Redacted::of("listed with no process and no background state"),
                },
                ..row("listed with nothing to say")
            },
        ]
    }

    /// Mutation: swap any two of rules 1 to 4 in `live`, make `>` `>=` in the turn-end
    /// comparison, drop the `can_ask` check, return `Idle` for `Busy`, or give a card to a
    /// `Dormant`/`Unknown` result: each reddens at least one named row.
    #[test]
    fn the_activity_table() {
        for row in table() {
            let got = evaluate(&row);
            assert_eq!(got.activity, row.expect, "activity: {}", row.name);
            assert_eq!(got.card, expected_card(&row), "card: {}", row.name);
        }
    }

    /// Dormancy needs a transcript with a record and a resume command.
    /// Mutation: fabricate `last_seen` from the process start.
    #[test]
    fn dormant_needs_a_record_and_a_resume_command() {
        let no_record = Evidence {
            kind: SessionKind::Interactive,
            alive: Alive::No,
            listing: Listing::Absent,
            started_at: None,
            pending_ask: None,
            last_human_prompt_at: None,
            latest_turn_end: None,
            last_record_at: None,
            transcript: TranscriptOnDisk::Found,
            resume: &Ok(resume()),
        };
        assert_eq!(
            classify(&session(), &no_record).activity,
            Activity::Unknown {
                reason: Redacted::of("the transcript has no records")
            }
        );
        let unavailable = Err(ResumeUnavailable::NoWorkingDirectory);
        let no_command = Evidence {
            last_record_at: Some(at(1)),
            resume: &unavailable,
            ..no_record
        };
        assert_eq!(
            classify(&session(), &no_command).activity,
            Activity::Unknown {
                reason: Redacted::of(
                    "cannot form a resume command: the session's working directory is unknown"
                )
            }
        );
    }

    /// A live session with nothing to date it is `Unknown`, not `Running` since a made-up time.
    #[test]
    fn running_without_any_time_is_unknown() {
        let resume = Ok(resume());
        let evidence = Evidence {
            kind: SessionKind::Interactive,
            alive: Alive::Yes,
            listing: Listing::Absent,
            started_at: None,
            pending_ask: None,
            last_human_prompt_at: None,
            latest_turn_end: None,
            last_record_at: None,
            transcript: TranscriptOnDisk::Found,
            resume: &resume,
        };
        assert!(matches!(
            classify(&session(), &evidence).activity,
            Activity::Unknown { .. }
        ));
        let open_question = ask();
        let asking = Evidence {
            pending_ask: Some(&open_question),
            ..evidence
        };
        assert!(matches!(
            classify(&session(), &asking).activity,
            Activity::Unknown { .. }
        ));
    }

    fn all_listings() -> Vec<Listing> {
        let statuses = [
            None,
            Some(ProcessStatus::Busy),
            Some(ProcessStatus::Idle),
            Some(ProcessStatus::Waiting),
        ];
        let reasons = [None, Some(WaitingFor::PermissionPrompt)];
        let mut listings = vec![Listing::Failed("down".into()), Listing::Absent];
        for status in &statuses {
            for reason in &reasons {
                listings.push(proc(status.clone(), reason.clone()));
            }
        }
        listings.push(listed_background(BackgroundState::Done));
        listings.push(listed_background(BackgroundState::Other("x".into())));
        listings
    }

    /// Every combination of the five columns, three liveness values and three kinds.
    fn product() -> Vec<Row> {
        let ends = [
            None,
            Some((T_PROMPT + 1, BackgroundTasks::Unknown)),
            Some((T_PROMPT + 1, BackgroundTasks::Known(vec![task()]))),
            Some((T_PROMPT, BackgroundTasks::Unknown)),
            Some((T_PROMPT - 1, BackgroundTasks::Unknown)),
        ];
        let alives = [Alive::Yes, Alive::No, Alive::Unknown("u".into())];
        let kinds = [
            SessionKind::Interactive,
            SessionKind::Background,
            SessionKind::Headless,
        ];
        let mut rows = Vec::new();
        for alive in &alives {
            for listing in all_listings() {
                for ask in [false, true] {
                    for prompt in [None, Some(T_PROMPT)] {
                        for end in &ends {
                            for kind in kinds {
                                rows.push(Row {
                                    kind,
                                    alive: alive.clone(),
                                    listing: listing.clone(),
                                    ask,
                                    prompt,
                                    end: end.clone(),
                                    ..row("product")
                                });
                            }
                        }
                    }
                }
            }
        }
        rows
    }

    /// A question has an `Asked` card, any other wait a `Blocked` card on the same wait, a
    /// finished turn a `Turn` card, nothing else a card.
    fn card_matches_activity(got: &Classified) -> bool {
        if let (Activity::Blocked { on }, Some(PendingCard::Blocked { on: card_on, .. })) =
            (&got.activity, &got.card)
        {
            return on == card_on;
        }
        matches!(
            (&got.activity, &got.card),
            (
                Activity::Blocked {
                    on: WaitingFor::InputNeeded
                },
                Some(PendingCard::Asked { .. })
            ) | (
                Activity::Watching { .. } | Activity::Idle { .. },
                Some(PendingCard::Turn { .. })
            ) | (
                Activity::Running { .. } | Activity::Dormant { .. } | Activity::Unknown { .. },
                None
            )
        )
    }

    /// Live states need a live process, dead-process states a dead one, and a process that
    /// could not be read is `Unknown` and nothing else.
    fn activity_matches_liveness(activity: &Activity, alive: &Alive) -> bool {
        match alive {
            Alive::Yes => !matches!(activity, Activity::Dormant { .. }),
            Alive::No => matches!(
                activity,
                Activity::Dormant { .. } | Activity::Unknown { .. }
            ),
            Alive::Unknown(_) => matches!(activity, Activity::Unknown { .. }),
        }
    }

    /// The relations the rule states, over the whole product: a card exists exactly for the
    /// card-bearing activities and matches them, and activity agrees with liveness.
    /// Mutation: give `Running` a card, let `Dormant` through for a live session, or have
    /// `Watching` carry an `Asked` card.
    #[test]
    fn the_whole_product_obeys_the_rule_s_relations() {
        let rows = product();
        assert_eq!(rows.len(), 3 * 12 * 2 * 2 * 5 * 3);
        for row in rows {
            let got = evaluate(&row);
            let label = format!(
                "{:?} {:?} ask={} prompt={:?} end={:?} {:?}: {got:?}",
                row.alive, row.listing, row.ask, row.prompt, row.end, row.kind
            );
            assert!(card_matches_activity(&got), "{label}");
            assert!(
                activity_matches_liveness(&got.activity, &row.alive),
                "{label}"
            );
        }
    }

    /// A blocked process that cannot be dated gets no card and says why, never a made-up time.
    /// Mutation: fall back to `at(0)` when both times are absent.
    #[test]
    fn a_wait_with_no_time_to_date_it_is_unknown() {
        let resume = Ok(resume());
        let evidence = Evidence {
            kind: SessionKind::Interactive,
            alive: Alive::Yes,
            listing: proc(
                Some(ProcessStatus::Waiting),
                Some(WaitingFor::PermissionPrompt),
            ),
            started_at: None,
            pending_ask: None,
            last_human_prompt_at: None,
            latest_turn_end: None,
            last_record_at: None,
            transcript: TranscriptOnDisk::Found,
            resume: &resume,
        };
        let got = classify(&session(), &evidence);
        assert!(matches!(got.activity, Activity::Unknown { .. }), "{got:?}");
        assert_eq!(got.card, None);
    }

    /// Mutation: swap two arms of `join_alive`, or let `Cannot` plus `NoProcess` answer `Unknown`.
    #[test]
    fn liveness_joins_the_registry_and_the_listing() {
        use ListingOpinion as L;
        use RegistryOpinion as R;
        let cannot = || String::from("c");
        let rows: Vec<(R, L, &str)> = vec![
            (R::Alive, L::Process, "Yes"),
            (R::Alive, L::NoProcess, "Yes"),
            (R::Alive, L::Cannot(cannot()), "Yes"),
            (R::Gone, L::Process, "No"),
            (R::Gone, L::NoProcess, "No"),
            (R::Gone, L::Cannot(cannot()), "No"),
            (R::Absent, L::Process, "Yes"),
            (R::Absent, L::NoProcess, "No"),
            (R::Absent, L::Cannot(cannot()), "Unknown"),
            (R::Cannot(cannot()), L::Process, "Yes"),
            (R::Cannot(cannot()), L::NoProcess, "No"),
            (R::Cannot(cannot()), L::Cannot(cannot()), "Unknown"),
        ];
        for (registry, listing, expected) in rows {
            let shown = match join_alive(&registry, &listing) {
                Alive::Yes => "Yes",
                Alive::No => "No",
                Alive::Unknown(_) => "Unknown",
            };
            assert_eq!(shown, expected, "{registry:?} / {listing:?}");
        }
    }
}
