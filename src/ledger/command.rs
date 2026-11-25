//! `conn claim`, `release`, `who`, `wait`, `rollout` and `defer`: what each decides, and the
//! words it answers with.
//!
//! The decisions are pure: they take the ledger, which sessions are running, and the time,
//! and return the entries to append or a [`Refusal`]. The answers are written for the agent
//! that ran the command, so a refusal says what to do instead.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use jiff::tz::TimeZone;

use super::store::{self, LedgerError, Loaded, UpdateError};
use super::{
    Act, CloseCheck, Deferral, Entry, Finish, Ledger, Note, Outcome, Run, Slug, Subject, WaitOn,
    WaitState,
};
use crate::jira::resolution::ClosedTickets;
use crate::sessions::proc::SystemProc;
use crate::sessions::registry::{Liveness, registry};
use crate::types::{Instant, JiraKey, SessionId};

/// Whether a session's process is running, as far as this machine can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Life {
    Running,
    /// No registry entry, or its process is gone.
    Ended,
    /// The registry entry could not be checked. Not a softer "running": it is treated as one
    /// only so that nothing is taken from a session that may still be working.
    Unknown(String),
}

/// What is known about the sessions the ledger names: whether each runs, and a name for it.
#[derive(Debug, Clone, Default)]
pub struct Sessions {
    known: HashMap<SessionId, Known>,
}

#[derive(Debug, Clone)]
struct Known {
    life: Life,
    /// The session's title, or the last part of its working directory.
    name: Option<String>,
}

impl Sessions {
    /// Reads Claude Code's session registry and checks each entry's process.
    pub fn from_registry(claude_dir: &Path) -> Self {
        let mut sessions = Self::default();
        for entry in registry(claude_dir).into_iter().flatten() {
            let life = match Liveness::check(&entry, &SystemProc) {
                Liveness::Live => Life::Running,
                Liveness::Gone { .. } => Life::Ended,
                Liveness::Unknown(reason) => Life::Unknown(reason.to_string()),
            };
            let name = session_name(entry.name.as_deref(), &entry.session_id, &entry.cwd);
            sessions.add(entry.session_id, life, name);
        }
        sessions
    }

    /// Records a session; a running entry wins over an ended one for the same session (a
    /// resumed session leaves its earlier entry behind).
    pub fn add(&mut self, session: SessionId, life: Life, name: Option<String>) {
        let rank = |life: &Life| match life {
            Life::Running => 2,
            Life::Unknown(_) => 1,
            Life::Ended => 0,
        };
        match self.known.get(&session) {
            Some(known) if rank(&known.life) >= rank(&life) => {}
            Some(_) | None => {
                self.known.insert(session, Known { life, name });
            }
        }
    }

    pub fn life(&self, session: &SessionId) -> Life {
        self.known
            .get(session)
            .map_or(Life::Ended, |known| known.life.clone())
    }

    /// `0a1b2c3d "title", running`: the id's first block, the name, and whether it runs.
    pub fn describe(&self, session: &SessionId) -> String {
        let short = short_id(session);
        let life = match self.life(session) {
            Life::Running => "running".to_owned(),
            Life::Ended => "no longer running".to_owned(),
            Life::Unknown(reason) => format!("could not tell whether it runs: {reason}"),
        };
        match self
            .known
            .get(session)
            .and_then(|known| known.name.as_deref())
        {
            Some(name) => format!("{short} \"{name}\", {life}"),
            None => format!("{short}, {life}"),
        }
    }
}

/// What to call a session: its title, unless the title only repeats its id ("Claude: <id>",
/// which names nothing), else the last part of the directory it works in.
pub fn session_name(title: Option<&str>, session: &SessionId, cwd: &Path) -> Option<String> {
    title
        .filter(|title| !title.contains(session.as_str()))
        .map(str::to_owned)
        .or_else(|| {
            cwd.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
}

/// The first block of a session id: enough to tell today's sessions apart, and what the pages
/// and `/sessions/<id>` URLs start with.
pub fn short_id(session: &SessionId) -> &str {
    session.as_str().split('-').next().unwrap_or_default()
}

/// What a command was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Claim {
        subjects: Vec<Subject>,
        if_owned: IfOwned,
    },
    Release {
        subjects: Vec<Subject>,
    },
    Who {
        subject: Option<Subject>,
    },
    Wait {
        on: WaitOn,
        then: Note,
        /// The ticket a time or prose wait is about.
        ticket: Option<JiraKey>,
    },
    Unwait,
    RolloutStart {
        rollout: Slug,
        ships: Vec<Subject>,
    },
    RolloutFinish {
        rollout: Slug,
        outcome: Outcome,
        note: Option<Note>,
    },
    Defer {
        ticket: JiraKey,
        until: Instant,
        check: Note,
    },
}

/// What a claim does to a subject another session that may still be running owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfOwned {
    Refuse,
    /// Take it over: the human said so.
    Take,
}

/// Why a command changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "{subject} is owned by session {owner} since {since}. Send what you have to that session \
         (SendMessage) instead of acting on {subject} yourself. Only if the user told you to take \
         it over, run `conn claim --take {subject}`."
    )]
    Owned {
        subject: Subject,
        owner: String,
        since: String,
    },
    #[error("{subject} is owned by session {owner}; only its owner releases it.")]
    NotYours { subject: Subject, owner: String },
    #[error(
        "rollout:{rollout} is already running in session {runner} since {since}{ships}. Wait for \
         it (`conn wait rollout:{rollout} --then \"...\"`) or ask that session; don't start a \
         second one."
    )]
    RolloutRunning {
        rollout: Slug,
        runner: String,
        since: String,
        ships: String,
    },
    #[error("rollout:{rollout} is being run by session {runner}; only that session finishes it.")]
    NotYourRollout { rollout: Slug, runner: String },
    #[error(
        "{until} has already passed: {ticket}'s close check can be done now. Check it instead of \
         deferring it."
    )]
    DeferPast { ticket: JiraKey, until: String },
}

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no session to act for: run this from a Claude Code session, or pass --session <id>")]
    NoSession,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

impl From<UpdateError<Refusal>> for CommandError {
    fn from(error: UpdateError<Refusal>) -> Self {
        match error {
            UpdateError::Ledger(error) => Self::Ledger(error),
            UpdateError::Refused(refusal) => Self::Refused(refusal),
        }
    }
}

/// Who acts, when, and what is known about the other sessions: what a decision and its words
/// depend on besides the ledger.
pub struct Context<'c> {
    pub sessions: &'c Sessions,
    /// The session acting, when there is one; `who` needs none.
    pub me: Option<SessionId>,
    pub now: Instant,
    pub zone: &'c TimeZone,
}

/// Runs the request against the ledger file at `ledger_path` and returns what to print.
pub fn run(
    request: &Request,
    ledger_path: &Path,
    context: &Context<'_>,
) -> Result<String, CommandError> {
    if let Request::Who { subject } = request {
        let loaded = store::read(ledger_path)?;
        // The tickets `conn serve` read as closed sit beside the ledger; without them a wait
        // on a closed ticket only reads as still waiting.
        let closed = ledger_path
            .parent()
            .map_or_else(ClosedTickets::default, ClosedTickets::load);
        let loaded = Loaded {
            ledger: loaded.ledger.knowing_closed(closed),
            ..loaded
        };
        return Ok(who(&loaded, subject.as_ref(), context));
    }
    let me = context.me.clone().ok_or(CommandError::NoSession)?;
    let answer = store::update(ledger_path, |loaded| {
        let (entries, mut answer) = decide(request, &loaded.ledger, &me, context)?;
        if !loaded.bad_lines.is_empty() {
            let _ = write!(
                answer,
                "\n(The ledger has {} unreadable line(s); `conn who` lists them.)",
                loaded.bad_lines.len()
            );
        }
        Ok::<_, Refusal>((entries, answer))
    })?;
    Ok(answer)
}

/// The entries a request appends and the answer to print, or why it appends nothing.
pub fn decide(
    request: &Request,
    ledger: &Ledger,
    me: &SessionId,
    context: &Context<'_>,
) -> Result<(Vec<Entry>, String), Refusal> {
    let mut draft = Draft {
        ledger,
        me,
        context,
        entries: Vec::new(),
        answer: String::new(),
    };
    match request {
        Request::Claim { subjects, if_owned } => draft.claim(subjects, *if_owned)?,
        Request::Release { subjects } => draft.release(subjects)?,
        Request::Who { .. } => {}
        Request::Wait { on, then, ticket } => draft.wait(on, then, ticket.as_ref()),
        Request::Unwait => draft.unwait(),
        Request::RolloutStart { rollout, ships } => draft.start(rollout, ships)?,
        Request::RolloutFinish {
            rollout,
            outcome,
            note,
        } => draft.finish(rollout, *outcome, note.as_ref())?,
        Request::Defer {
            ticket,
            until,
            check,
        } => draft.defer(ticket, *until, check)?,
    }
    Ok((draft.entries, draft.answer.trim_end().to_owned()))
}

/// One request being decided: what it will append, and what it will say.
struct Draft<'d> {
    ledger: &'d Ledger,
    me: &'d SessionId,
    context: &'d Context<'d>,
    entries: Vec<Entry>,
    answer: String,
}

impl Draft<'_> {
    fn push(&mut self, act: Act) {
        self.entries.push(Entry {
            at: self.context.now,
            session: self.me.clone(),
            act,
        });
    }

    fn say(&mut self, line: impl std::fmt::Display) {
        let _ = writeln!(self.answer, "{line}");
    }

    fn describe(&self, session: &SessionId) -> String {
        self.context.sessions.describe(session)
    }

    fn may_be_running(&self, session: &SessionId) -> bool {
        self.context.sessions.life(session) != Life::Ended
    }

    /// Every subject is claimed, or none is.
    fn claim(&mut self, subjects: &[Subject], if_owned: IfOwned) -> Result<(), Refusal> {
        for subject in subjects {
            let took_from = match self.ledger.owner(subject) {
                None => None,
                Some(held) if held.session == *self.me => {
                    self.say(format_args!("{subject} is already yours."));
                    continue;
                }
                Some(held) => {
                    if if_owned == IfOwned::Refuse && self.may_be_running(&held.session) {
                        return Err(Refusal::Owned {
                            subject: subject.clone(),
                            owner: self.describe(&held.session),
                            since: when(held.since, self.context),
                        });
                    }
                    Some(held.session.clone())
                }
            };
            match &took_from {
                None => self.say(format_args!("{subject} is yours.")),
                Some(owner) => self.say(format_args!(
                    "{subject} was owned by session {}; it is yours now. Tell that session if it \
                     still runs.",
                    self.describe(owner)
                )),
            }
            self.push(Act::Claim {
                subject: subject.clone(),
                took_from,
            });
        }
        Ok(())
    }

    fn release(&mut self, subjects: &[Subject]) -> Result<(), Refusal> {
        for subject in subjects {
            match self.ledger.owner(subject) {
                None => self.say(format_args!("{subject} has no owner.")),
                Some(held) if held.session == *self.me => {
                    self.say(format_args!("{subject} released."));
                    self.push(Act::Release {
                        subject: subject.clone(),
                    });
                }
                Some(held) => {
                    return Err(Refusal::NotYours {
                        subject: subject.clone(),
                        owner: self.describe(&held.session),
                    });
                }
            }
        }
        Ok(())
    }

    fn wait(&mut self, on: &WaitOn, then: &Note, ticket: Option<&JiraKey>) {
        self.say(format_args!(
            "Waiting {}; then: {then}",
            wait_words(on, self.context)
        ));
        if let Some(ticket) = ticket {
            self.say(format_args!(
                "The wait is about {ticket}: it is over as soon as {ticket} is resolved in Jira, \
                 as `conn serve` reads it."
            ));
        }
        match on {
            WaitOn::Rollout(name) => self.say(rollout_line(self.ledger, name, self.context)),
            WaitOn::Subject(subject) => match self.ledger.owner(subject) {
                Some(held) => self.say(format_args!(
                    "{subject} is owned by session {}; the wait is over when it releases it.",
                    self.describe(&held.session)
                )),
                None => self.say(format_args!(
                    "{subject} has no owner yet; the wait is over when whoever claims it releases \
                     it."
                )),
            },
            WaitOn::For(what) => {
                let known = self.ledger.rollouts();
                for rollout in named_rollouts(what.as_str(), known.iter().map(|(name, _)| *name)) {
                    self.say(format_args!(
                        "This names rollout:{name}. If you are waiting for that release, wait on \
                         the rollout instead, so the user's pages show a release waiting on \
                         them: conn wait rollout:{name} --then {then}",
                        name = rollout.as_str(),
                        then = crate::util::shell::quote(then.as_str()),
                    ));
                }
            }
            WaitOn::Until(_) => {}
        }
        self.push(Act::Wait {
            on: on.clone(),
            then: then.clone(),
            ticket: ticket.cloned(),
        });
    }

    fn unwait(&mut self) {
        if self.ledger.wait_of(self.me, self.context.now).is_none() {
            self.say("No wait to clear.");
        } else {
            self.say("Wait cleared.");
            self.push(Act::Unwait);
        }
    }

    /// Refused while a run of the same rollout is under way in a session that may be running,
    /// this one included.
    fn start(&mut self, rollout: &Slug, ships: &[Subject]) -> Result<(), Refusal> {
        let running = self
            .ledger
            .rollout(rollout)
            .and_then(|r| r.running.as_ref());
        if let Some(run) = running {
            if run.session == *self.me || self.may_be_running(&run.session) {
                return Err(Refusal::RolloutRunning {
                    rollout: rollout.clone(),
                    runner: self.describe(&run.session),
                    since: when(run.since, self.context),
                    ships: ships_words(&run.ships),
                });
            }
            self.say(format_args!(
                "The run started by session {} never finished; this one replaces it.",
                self.describe(&run.session)
            ));
        }
        self.say(format_args!(
            "rollout:{rollout} started{}.",
            ships_words(ships)
        ));
        self.owners_to_tell(ships);
        self.push(Act::RolloutStart {
            rollout: rollout.clone(),
            ships: ships.to_vec(),
        });
        Ok(())
    }

    /// Only the run's own session finishes it, unless that session has ended.
    fn finish(
        &mut self,
        rollout: &Slug,
        outcome: Outcome,
        note: Option<&Note>,
    ) -> Result<(), Refusal> {
        let running = self
            .ledger
            .rollout(rollout)
            .and_then(|r| r.running.as_ref());
        let ships = match running {
            Some(run) if run.session != *self.me && self.may_be_running(&run.session) => {
                return Err(Refusal::NotYourRollout {
                    rollout: rollout.clone(),
                    runner: self.describe(&run.session),
                });
            }
            Some(run) => run.ships.clone(),
            None => {
                self.say(format_args!(
                    "(No start of rollout:{rollout} was recorded.)"
                ));
                Vec::new()
            }
        };
        self.say(format_args!("rollout:{rollout} {}.", outcome.words()));
        self.owners_to_tell(&ships);
        let waiting = self.ledger.waiting_on(rollout, self.context.now);
        if !waiting.is_empty() {
            self.say("These sessions wait on it:");
            for (session, state) in waiting {
                self.say(format_args!(
                    "- {}: then {}",
                    self.describe(session),
                    state.wait().then
                ));
            }
        }
        self.push(Act::RolloutFinish {
            rollout: rollout.clone(),
            outcome,
            note: note.cloned(),
        });
        Ok(())
    }

    /// Refused for a time already past: the check is due, and putting it off says otherwise.
    fn defer(&mut self, ticket: &JiraKey, until: Instant, check: &Note) -> Result<(), Refusal> {
        if until <= self.context.now {
            return Err(Refusal::DeferPast {
                ticket: ticket.clone(),
                until: when(until, self.context),
            });
        }
        match self.ledger.close_check(ticket, self.context.now) {
            CloseCheck::Deferred(earlier) | CloseCheck::Reached(earlier) => self.say(format_args!(
                "This replaces the earlier deferral to {}: {}.",
                when(earlier.until, self.context),
                earlier.check
            )),
            CloseCheck::Open => {}
        }
        self.say(format_args!(
            "{ticket}'s close check is deferred until {}; then check: {check}",
            when(until, self.context)
        ));
        self.push(Act::Defer {
            ticket: ticket.clone(),
            until,
            check: check.clone(),
        });
        Ok(())
    }

    /// For a rollout's tickets that another session owns: the rule that the rollout reports
    /// to the owner and leaves the ticket to it.
    fn owners_to_tell(&mut self, ships: &[Subject]) {
        for subject in ships {
            if let Some(held) = self.ledger.owner(subject).filter(|h| h.session != *self.me) {
                let owner = self.describe(&held.session);
                self.say(format_args!(
                    "{subject} is owned by session {owner}: report this rollout to it \
                     (SendMessage) and leave the ticket's comments and transitions to it."
                ));
            }
        }
    }
}

/// `conn who`: one subject, or everything the ledger holds.
fn who(loaded: &Loaded, subject: Option<&Subject>, context: &Context<'_>) -> String {
    let ledger = &loaded.ledger;
    let mut out = String::new();
    if let Some(subject) = subject {
        match ledger.owner(subject) {
            Some(held) => {
                let _ = writeln!(
                    out,
                    "{subject}: owned by session {} since {}.",
                    context.sessions.describe(&held.session),
                    when(held.since, context)
                );
            }
            None => {
                let _ = writeln!(out, "{subject}: no owner.");
            }
        }
        match subject {
            Subject::Rollout(name) => {
                let _ = writeln!(out, "{}", rollout_line(ledger, name, context));
                waiters(&mut out, ledger, name, context);
            }
            Subject::Ticket(key) => match ledger.close_check(key, context.now) {
                CloseCheck::Deferred(deferral) => {
                    let _ = writeln!(out, "{}", deferral_line(key, deferral, context));
                }
                CloseCheck::Reached(deferral) => {
                    let _ = writeln!(
                        out,
                        "Its close check was deferred until {}, which has come: check {}.",
                        when(deferral.until, context),
                        deferral.check
                    );
                }
                CloseCheck::Open => {}
            },
            Subject::Topic(_) => {}
        }
        return out.trim_end().to_owned();
    }
    let claims = ledger.claims();
    let rollouts = ledger.rollouts();
    let waits = ledger.waits(context.now);
    let deferred = ledger.deferred(context.now);
    if claims.is_empty() && rollouts.is_empty() && waits.is_empty() && deferred.is_empty() {
        out.push_str(
            "The ledger is empty: nothing is claimed, waited on, rolling out or deferred.\n",
        );
    }
    if !claims.is_empty() {
        out.push_str("Owned:\n");
        for (subject, held) in claims {
            let _ = writeln!(
                out,
                "- {subject}: {} since {}",
                context.sessions.describe(&held.session),
                when(held.since, context)
            );
        }
    }
    if !rollouts.is_empty() {
        out.push_str("Rollouts:\n");
        for (name, _) in rollouts {
            let _ = writeln!(out, "- {}", rollout_line(ledger, name, context));
        }
    }
    if !waits.is_empty() {
        out.push_str("Waiting:\n");
        for (session, state) in waits {
            let _ = writeln!(
                out,
                "- {}: {}",
                context.sessions.describe(session),
                wait_state_words(state, context)
            );
        }
    }
    if !deferred.is_empty() {
        out.push_str("Close checks deferred:\n");
        for (key, deferral) in deferred {
            let _ = writeln!(out, "- {}", deferral_line(key, deferral, context));
        }
    }
    for bad in &loaded.bad_lines {
        let _ = writeln!(out, "Unreadable: {bad}");
    }
    out.trim_end().to_owned()
}

/// `ABC-1's close check is deferred until Nov 20 09:00 by session …: <check>`.
fn deferral_line(key: &JiraKey, deferral: &Deferral, context: &Context<'_>) -> String {
    format!(
        "{key}'s close check is deferred until {} by session {}: {}",
        when(deferral.until, context),
        context.sessions.describe(&deferral.by),
        deferral.check
    )
}

fn waiters(out: &mut String, ledger: &Ledger, name: &Slug, context: &Context<'_>) {
    for (session, state) in ledger.waiting_on(name, context.now) {
        let _ = writeln!(
            out,
            "- waited on by {}: then {}",
            context.sessions.describe(session),
            state.wait().then
        );
    }
}

/// `rollout:checkout running in session …since …, ships ABC-1` or how it last ended.
pub fn rollout_line(ledger: &Ledger, name: &Slug, context: &Context<'_>) -> String {
    let rollout = ledger.rollout(name);
    let running = rollout.and_then(|r| r.running.as_ref());
    let finish = rollout.and_then(|r| r.last_finish.as_ref());
    let mut line = format!("rollout:{name} ");
    match running {
        Some(Run {
            session,
            since,
            ships,
        }) => {
            let _ = write!(
                line,
                "running in session {} since {}{}",
                context.sessions.describe(session),
                when(*since, context),
                ships_words(ships)
            );
        }
        None => line.push_str("not running"),
    }
    if let Some(finish) = finish {
        let _ = write!(line, "; last {}", finish_words(finish, context));
    }
    line
}

fn finish_words(finish: &Finish, context: &Context<'_>) -> String {
    let mut words = format!(
        "{} {} by session {}",
        finish.outcome.words(),
        when(finish.at, context),
        context.sessions.describe(&finish.session)
    );
    if let Some(note) = &finish.note {
        let _ = write!(words, " ({note})");
    }
    words
}

fn ships_words(ships: &[Subject]) -> String {
    if ships.is_empty() {
        return String::new();
    }
    let names: Vec<String> = ships.iter().map(ToString::to_string).collect();
    format!(", shipping {}", names.join(", "))
}

/// The rollouts of `known` that `text` names as a word of its own, in any case: "ship CHECKOUT to
/// Prod" names `checkout`, "checkoutx" does not. A word runs over letters, digits, `-`, `_` and `.`, the
/// characters of a rollout's name, less the dots that end a sentence.
fn named_rollouts<'k>(text: &str, known: impl Iterator<Item = &'k Slug>) -> Vec<&'k Slug> {
    let words: std::collections::HashSet<String> = text
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .map(|word| word.trim_end_matches('.'))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    known.filter(|name| words.contains(name.as_str())).collect()
}

fn wait_words(on: &WaitOn, context: &Context<'_>) -> String {
    match on {
        WaitOn::Rollout(name) => format!("on rollout:{name}"),
        WaitOn::Subject(subject) => format!("on {subject}"),
        WaitOn::Until(at) => format!("until {}", when(*at, context)),
        WaitOn::For(what) => format!("for {what}"),
    }
}

/// A wait in words, for `who` and for the hook's reminders.
pub fn wait_state_words(state: WaitState<'_>, context: &Context<'_>) -> String {
    let wait = state.wait();
    let waiting = wait_words(&wait.on, context);
    match state {
        WaitState::Waiting { running, .. } => {
            let mut words = format!("waiting {waiting} since {}", when(wait.since, context));
            if let (WaitOn::Rollout(_), Some(run)) = (&wait.on, running) {
                let _ = write!(
                    words,
                    " (running in session {})",
                    context.sessions.describe(&run.session)
                );
            } else if matches!(wait.on, WaitOn::Rollout(_)) {
                words.push_str(" (not started)");
            }
            let _ = write!(words, "; then: {}", wait.then);
            words
        }
        WaitState::RolloutFinished { finish, .. } => format!(
            "the wait {waiting} is over: {}; next: {}",
            finish_words(finish, context),
            wait.then
        ),
        WaitState::TimeReached { .. } => {
            format!("the wait {waiting} is over; next: {}", wait.then)
        }
        WaitState::Released { by, at, .. } => format!(
            "the wait {waiting} is over: released {} by session {}; next: {}",
            when(at, context),
            context.sessions.describe(by),
            wait.then
        ),
        WaitState::TicketClosed { ticket, closed, .. } => format!(
            "the wait {waiting} is over: {ticket} closed ({}{}); next: {}",
            closed.resolution,
            closed
                .at
                .map_or_else(String::new, |at| format!(", {}", when(at, context))),
            wait.then
        ),
    }
}

fn when(at: Instant, context: &Context<'_>) -> String {
    local_time(at, context.zone)
}

/// An instant as the person reading it would write it: `Nov 5 15:05`.
pub fn local_time(at: Instant, zone: &TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%b %-d %H:%M")
        .to_string()
}
