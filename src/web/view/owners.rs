//! "Who owns what": the coordination ledger as a page, and the line a card or session row
//! carries about its session's claims and wait.
//!
//! Everything here is what the sessions declared with `conn claim`, `conn wait` and `conn
//! rollout`; whether an owner still runs is the model's own view of its sessions.

use std::collections::BTreeMap;

use maud::{Markup, html};

use super::Ctx;
use super::card::session_name;
use super::fmt::{external_link, redact};
use crate::ledger::Held;
use crate::ledger::command::local_time;
use crate::ledger::shared::{OwnerThen, Shared, SharedWrite};
use crate::ledger::{
    Act, CloseCheck, Entry, Finish, Outcome, Release, Slug, Subject, WaitOn, WaitState,
};
use crate::model::SourceState;
use crate::types::outward::Target;
use crate::types::{JiraKey, Link, SessionId};
use crate::util::redact::Redacted;

/// How many of the ledger's latest entries the page lists.
const RECENT_ENTRIES: usize = 30;

/// Whether a session the ledger names still runs, as far as the model knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// Live, parked or unclassified: a process the registry vouches for.
    Running,
    /// Dormant: a transcript and no process.
    Ended,
    /// Not in the model at all.
    Unknown,
}

fn presence(ctx: &Ctx<'_>, id: &SessionId) -> Presence {
    if ctx.model.sessions.iter().any(|view| view.session.id == *id) {
        Presence::Running
    } else if ctx.model.dormant.iter().any(|view| view.session.id == *id) {
        Presence::Ended
    } else {
        Presence::Unknown
    }
}

/// Who owns `subject`, as a muted "owned by <session>"; nothing when no one does. For the pages
/// that show a ticket or a rollout: the work items, the map's anchors.
pub(super) fn owned_by(ctx: &Ctx<'_>, subject: &Subject) -> Markup {
    html! {
        @if let Some(held) = ctx.model.coordination.ledger.owner(subject) {
            span.owned-by { "owned by " (who(ctx, &held.session)) }
        }
    }
}

/// The session's own part of the ledger, for its page: its line, then what it declared, newest
/// first, and the way to the whole picture. Nothing when it declared nothing.
pub(super) fn session_section(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    let ledger = &ctx.model.coordination.ledger;
    let mine: Vec<&Entry> = ledger
        .entries()
        .iter()
        .rev()
        .filter(|entry| entry.session == *id)
        .take(RECENT_ENTRIES)
        .collect();
    html! {
        @if !mine.is_empty() || ledger.wait_of(id, ctx.now).is_some() || !ledger.claims_of(id).is_empty() {
            h2 { "Coordination" }
            (session_line(ctx, id))
            @if !mine.is_empty() {
                ol.ledger-log reversed {
                    @for entry in &mine {
                        li { span.muted { (when(ctx, entry.at)) } " " (act_words(ctx, entry)) }
                    }
                }
            }
            p.muted { "Every session's claims, rollouts and waits: " a href=(crate::web::route::Page::Owners.to_string()) { "Who owns what" } "." }
        }
    }
}

/// What a session is called here: its gist's headline when it has one, since the titles
/// sessions give themselves too often only repeat their id; else its usual name.
pub(super) fn name(ctx: &Ctx<'_>, id: &SessionId) -> Redacted<String> {
    let view = ctx
        .model
        .sessions
        .iter()
        .chain(&ctx.model.dormant)
        .find(|view| view.session.id == *id);
    view.and_then(|view| view.gist.as_ref())
        .and_then(|gist| crate::gist::headline(gist.as_str()))
        .map_or_else(
            || {
                session_name(ctx, view.and_then(|view| view.title.as_ref()), id)
                    .label()
                    .clone()
            },
            redact,
        )
}

/// A session as a link under its [`name`], marked when it no longer runs.
pub(super) fn who(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    let presence = presence(ctx, id);
    html! {
        span class={ "who " (presence_class(presence)) } {
            (super::fmt::local_link(&crate::web::route::Page::Session(id.clone()), &name(ctx, id)))
            @match presence {
                Presence::Running => {}
                Presence::Ended => { " " span.muted { "(ended)" } }
                Presence::Unknown => { " " span.muted { "(not seen)" } }
            }
        }
    }
}

const fn presence_class(presence: Presence) -> &'static str {
    match presence {
        Presence::Running => "running",
        Presence::Ended => "ended",
        Presence::Unknown => "unseen",
    }
}

/// A subject as written, a ticket linked to Jira where one is configured.
pub(super) fn subject(ctx: &Ctx<'_>, subject: &Subject) -> Markup {
    let text = redact(&subject.to_string());
    let url = subject
        .ticket()
        .and_then(|key| Link::Jira { key: key.clone() }.url(ctx.targets).ok());
    let badge = subject
        .ticket()
        .map(|key| super::comments::badge(ctx, &key.to_string()));
    html! {
        @if let Some(url) = url {
            (external_link(&url, &html! { (text) }))
        } @else {
            code { (text) }
        }
        @if let Some(badge) = badge { (badge) }
    }
}

fn subjects(ctx: &Ctx<'_>, list: &[&Subject]) -> Markup {
    html! {
        @for (i, item) in list.iter().enumerate() {
            @if i > 0 { ", " }
            (subject(ctx, item))
        }
    }
}

fn when(ctx: &Ctx<'_>, at: crate::types::Instant) -> String {
    local_time(at, ctx.zone)
}

fn wait_on(on: &WaitOn, ctx: &Ctx<'_>) -> Markup {
    match on {
        WaitOn::Rollout(name) => html! { code { "rollout:" (name.as_str()) } },
        WaitOn::Subject(waited) => subject(ctx, waited),
        WaitOn::Until(at) => html! { "until " (when(ctx, *at)) },
        WaitOn::For(what) => html! { (redact(what.as_str())) },
    }
}

fn finish_words(ctx: &Ctx<'_>, finish: &Finish) -> Markup {
    html! {
        span class={ "outcome " (outcome_class(finish.outcome)) } { (finish.outcome.words()) }
        " " (when(ctx, finish.at)) " by " (who(ctx, &finish.session))
        @if let Some(note) = &finish.note { " (" (redact(note.as_str())) ")" }
    }
}

const fn outcome_class(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Shipped => "shipped",
        Outcome::RolledBack => "rolled-back",
        Outcome::Abandoned => "abandoned",
    }
}

/// A wait, as a card or a table says it: what it is on, where that stands, and what follows.
fn wait_words(ctx: &Ctx<'_>, state: WaitState<'_>) -> Markup {
    let wait = state.wait();
    html! {
        @match state {
            WaitState::Waiting { running, .. } => {
                "waiting " @if matches!(wait.on, WaitOn::Rollout(_) | WaitOn::Subject(_)) { "on " } (wait_on(&wait.on, ctx))
                @if let WaitOn::Rollout(_) = wait.on {
                    @match running {
                        Some(run) => { " (running in " (who(ctx, &run.session)) ")" }
                        None => { " (not started)" }
                    }
                }
                @if let WaitOn::Subject(waited) = &wait.on {
                    @match ctx.model.coordination.ledger.owner(waited) {
                        Some(held) => { " (" (who(ctx, &held.session)) " has it)" }
                        None => { " (no owner)" }
                    }
                }
                " → then: " (redact(wait.then.as_str()))
            }
            WaitState::Released { by, at, .. } => {
                strong.over { "wait over" } ": " (wait_on(&wait.on, ctx)) " released " (when(ctx, at))
                " by " (who(ctx, by))
                " → next: " (redact(wait.then.as_str()))
            }
            WaitState::RolloutFinished { finish, .. } => {
                strong.over { "wait over" } ": " (wait_on(&wait.on, ctx)) " " (finish_words(ctx, finish))
                " → next: " (redact(wait.then.as_str()))
            }
            WaitState::TimeReached { .. } => {
                strong.over { "wait over" } ": " (wait_on(&wait.on, ctx))
                " → next: " (redact(wait.then.as_str()))
            }
            WaitState::TicketClosed { ticket, closed, .. } => {
                strong.over { "wait over" } ": " (subject(ctx, &Subject::Ticket(ticket.clone())))
                " closed (" (redact(&closed.resolution))
                @if let Some(at) = closed.at { ", " (when(ctx, at)) }
                ") → next: " (redact(wait.then.as_str()))
            }
        }
    }
}

/// How recent a collision has to be for a session's line to warn about it; older ones stay on
/// the Owners page.
const CLASH_WARNS_FOR: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// What a card or a session row says about its session in the ledger, and any recent
/// collision its writes were part of; nothing when there is none of that.
pub(super) fn session_line(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    let ledger = &ctx.model.coordination.ledger;
    let owned = ledger.claims_of(id);
    let wait = ledger.wait_of(id, ctx.now);
    let clashes = clashes_of(ctx, id);
    html! {
        @if !owned.is_empty() || wait.is_some() || !clashes.is_empty() {
            p.coord {
                @if !owned.is_empty() { "owns " (subjects(ctx, &owned)) }
                @if !owned.is_empty() && wait.is_some() { " · " }
                @if let Some(state) = wait { (wait_words(ctx, state)) }
                @for shared in &clashes {
                    " " (clash_words(ctx, id, shared))
                }
            }
        }
    }
}

/// The recent shared targets this session wrote to.
fn clashes_of<'c>(ctx: &'c Ctx<'_>, id: &SessionId) -> Vec<&'c Shared> {
    ctx.model
        .coordination
        .shared
        .iter()
        .filter(|shared| {
            shared
                .latest()
                .is_some_and(|at| at >= ctx.now - CLASH_WARNS_FOR)
        })
        .filter(|shared| shared.writes.iter().any(|write| write.session == *id))
        .collect()
}

/// `⚠ ABC-1 also written by <other>`, or `⚠ wrote ABC-1 while <owner> owned it`.
fn clash_words(ctx: &Ctx<'_>, id: &SessionId, shared: &Shared) -> Markup {
    let foreign_owner = shared
        .writes
        .iter()
        .filter(|write| write.session == *id)
        .find_map(|write| match &write.owner_then {
            OwnerThen::Other(owner) => Some(owner),
            OwnerThen::Unowned | OwnerThen::Writer => None,
        });
    let others: Vec<&SessionId> = shared
        .writers()
        .into_iter()
        .filter(|writer| *writer != id)
        .collect();
    html! {
        span.clash {
            "⚠ "
            @if let Some(owner) = foreign_owner {
                "wrote " (target(ctx, &shared.target)) " while " (who(ctx, owner)) " owned it"
            } @else {
                (target(ctx, &shared.target)) " also written by "
                @for (i, other) in others.iter().enumerate() {
                    @if i > 0 { ", " }
                    (who(ctx, other))
                }
            }
        }
    }
}

/// Where a write landed: a ticket as [`subject`] draws it, a Slack thread as a link to it.
pub(super) fn target(ctx: &Ctx<'_>, target: &Target) -> Markup {
    match target {
        Target::Ticket(key) => subject(ctx, &Subject::Ticket(key.clone())),
        Target::Slack { channel, thread } => {
            let url = thread.as_ref().and_then(|ts| {
                Link::SlackThread {
                    channel: channel.clone(),
                    ts: ts.clone(),
                }
                .url(ctx.targets)
                .ok()
            });
            let label = redact(&format!("Slack thread in {channel}"));
            if let Some(url) = url {
                external_link(&url, &html! { (label) })
            } else {
                html! { code { (label) } }
            }
        }
    }
}

/// How many of a shared target's latest writes are shown open; the earlier ones fold away.
const WRITES_SHOWN: usize = 8;

pub(super) fn writes_table(ctx: &Ctx<'_>, writes: &[SharedWrite]) -> Markup {
    html! {
        table.writes {
            @for write in writes {
                tr class=@if matches!(write.owner_then, OwnerThen::Other(_)) { "foreign" } {
                    td.muted { (when(ctx, write.at)) }
                    td { (who(ctx, &write.session)) }
                    td { (write.kind.words()) }
                    td {
                        @if let Some(excerpt) = &write.excerpt { span.excerpt { (redact(excerpt)) } }
                        @if let OwnerThen::Other(owner) = &write.owner_then {
                            " " span.clash { "while " (who(ctx, owner)) " owned it" }
                        }
                    }
                }
            }
        }
    }
}

/// The collisions section of the Owners page: each shared target with its writes.
fn collisions(ctx: &Ctx<'_>) -> Markup {
    let shared = &ctx.model.coordination.shared;
    html! {
        @if !shared.is_empty() {
            h2 { "Where the work collided" }
            p.muted {
                "From the sessions' transcripts over the last week: tickets and Slack threads more \
                 than one session wrote to, and tickets written to by a session that did not own \
                 them. Claiming a ticket (conn claim) keeps the next write with its owner."
            }
            @for item in shared {
                section.collision {
                    h3 {
                        (target(ctx, &item.target)) " "
                        span.muted {
                            (item.writers().len()) @if item.writers().len() == 1 { " session" } @else { " sessions" }
                            @let foreign = item.foreign().count();
                            @if foreign > 0 { ", " (foreign) " write(s) by a non-owner" }
                        }
                        @if let Target::Ticket(key) = &item.target { " " (owned_by(ctx, &Subject::Ticket(key.clone()))) }
                    }
                    @let split = item.writes.len().saturating_sub(WRITES_SHOWN);
                    @let (earlier, latest) = item.writes.split_at(split);
                    @if !earlier.is_empty() {
                        details.earlier-writes {
                            summary { (earlier.len()) " earlier" }
                            (writes_table(ctx, earlier))
                        }
                    }
                    (writes_table(ctx, latest))
                }
            }
        }
    }
}

pub(super) fn page(ctx: &Ctx<'_>) -> Markup {
    let coordination = &ctx.model.coordination;
    let ledger = &coordination.ledger;
    let claims = ledger.claims();
    let releases = ledger.releases(ctx.now);
    let waits = ledger.waits(ctx.now);
    let deferred = ledger.deferred(ctx.now);
    let empty = claims.is_empty() && releases.is_empty() && waits.is_empty() && deferred.is_empty();
    html! {
        p.muted {
            "What the sessions declared: who owns each shared ticket, rollout and topic (conn \
             claim), what each idle session waits for and will do next (conn wait), the \
             rollouts (conn rollout), and the shipped tickets whose close check cannot be done \
             yet (conn defer). A Jira write to a ticket another running session owns is \
             refused by the guard."
        }
        @match &coordination.state {
            SourceState::Failed { reason, .. } => p.error { "The ledger could not be read: " (redact(reason)) },
            SourceState::NeverRead => p.muted { "Not read yet." },
            SourceState::Ok { .. } | SourceState::NeverWritten { .. } => {}
        }
        @if !coordination.bad_lines.is_empty() {
            p.error { (coordination.bad_lines.len()) " ledger line(s) could not be read; see the health page." }
        }
        @if empty {
            p.empty { "Nothing is owned, rolling out, waited on or deferred." }
        }
        @if !waits.is_empty() {
            h2 { "Waiting" }
            ul.waits {
                @for (session, state) in &waits {
                    li class=@if state.is_over() { "over" } @else { "waiting" } {
                        (who(ctx, session)) ": " (wait_words(ctx, *state))
                        span.muted { " since " (when(ctx, state.wait().since)) }
                    }
                }
            }
        }
        @if !releases.is_empty() {
            h2 { "Rollouts" }
            @for (name, _) in &releases { (rollout_block(ctx, name)) }
        }
        @if !claims.is_empty() {
            h2 { "Owned" }
            (owned(ctx, &claims))
        }
        (collisions(ctx))
        @if !deferred.is_empty() {
            h2 { "Close checks deferred" }
            ul.deferrals {
                @for (ticket, deferral) in &deferred {
                    li {
                        (subject(ctx, &Subject::Ticket((*ticket).clone()))) " until " (when(ctx, deferral.until))
                        ": " (redact(deferral.check.as_str()))
                        span.muted { " (" (who(ctx, &deferral.by)) ")" }
                    }
                }
            }
        }
        @if !ledger.entries().is_empty() {
            h2 { "Recent" }
            ol.ledger-log reversed {
                @for entry in ledger.entries().iter().rev().take(RECENT_ENTRIES) {
                    li { span.muted { (when(ctx, entry.at)) } " " (who(ctx, &entry.session)) " " (act_words(ctx, entry)) }
                }
            }
        }
    }
}

/// The claims as one block per owner, what it owns as chips: owners still running first, the
/// one that claimed most recently first; then, dimmed, the owners that ended, whose claims pass
/// to the next session to claim them. An owner's wait is only marked here, since "Waiting"
/// above words it in full.
fn owned(ctx: &Ctx<'_>, claims: &[(&Subject, &Held)]) -> Markup {
    let mut by_owner: BTreeMap<&SessionId, Vec<(&Subject, &Held)>> = BTreeMap::new();
    for (owned, held) in claims {
        by_owner
            .entry(&held.session)
            .or_default()
            .push((owned, held));
    }
    let mut owners: Vec<(&SessionId, Vec<(&Subject, &Held)>)> = by_owner.into_iter().collect();
    owners.sort_by_cached_key(|(session, held)| {
        let latest = held.iter().map(|(_, held)| held.since).max();
        (
            presence(ctx, session) != Presence::Running,
            std::cmp::Reverse(latest),
        )
    });
    let (running, ended): (Vec<_>, Vec<_>) = owners
        .iter()
        .partition(|(session, _)| presence(ctx, session) == Presence::Running);
    html! {
        div.owner-groups {
            @for (session, held) in &running { (owner_block(ctx, session, held)) }
        }
        @if !ended.is_empty() {
            p.muted.owners-ended { "Owned by sessions that ended: the next session to claim one takes it over." }
            div.owner-groups.ended {
                @for (session, held) in &ended { (owner_block(ctx, session, held)) }
            }
        }
    }
}

fn owner_block(ctx: &Ctx<'_>, session: &SessionId, held: &[(&Subject, &Held)]) -> Markup {
    let ledger = &ctx.model.coordination.ledger;
    html! {
        div class={ "owner-group " (presence_class(presence(ctx, session))) } {
            div.owner-head {
                (who(ctx, session))
                @if let Some(state) = ledger.wait_of(session, ctx.now) {
                    " "
                    @if state.is_over() {
                        span.badge.over { "wait over" }
                    } @else {
                        span.badge.waits-on { "waiting" }
                    }
                }
            }
            ul.owned-chips {
                @for (owned, held) in held {
                    li title={ "claimed " (when(ctx, held.since)) } { (subject(ctx, owned)) }
                }
            }
        }
    }
}

/// What a pending release with `waiting` sessions held on it says.
pub(super) fn pending_words(waiting: usize) -> String {
    let sessions = if waiting == 1 {
        "session waits"
    } else {
        "sessions wait"
    };
    format!("Release pending: {waiting} {sessions} on its next finish, and nobody is running it.")
}

/// One deployable's rollouts: the release its waiters are held on, or the run under way; how
/// it last ended; and every session that waits on it.
pub(super) fn rollout_block(ctx: &Ctx<'_>, name: &Slug) -> Markup {
    let ledger = &ctx.model.coordination.ledger;
    let waiting = ledger.waiting_on(name, ctx.now);
    let release = ledger.release(name, ctx.now);
    let last = ledger.rollout(name).and_then(|r| r.last_finish.as_ref());
    html! {
        section.rollout {
            h3 { code { "rollout:" (name.as_str()) } }
            @if let Release::Pending { .. } = release {
                p.clash { (pending_words(release.waiting().len())) }
            }
            @if let Release::Running { run, .. } = release {
                p {
                    span.outcome.running { "running" } " in " (who(ctx, &run.session)) " since " (when(ctx, run.since))
                }
                @if !run.ships.is_empty() {
                    ul.ships {
                        @for shipped in &run.ships {
                            li {
                                (subject(ctx, shipped))
                                @if let Some(held) = ledger.owner(shipped) {
                                    " owned by " (who(ctx, &held.session)) ": the rollout reports to it"
                                }
                            }
                        }
                    }
                }
            }
            @if let Some(finish) = last {
                p { "Last: " (finish_words(ctx, finish)) }
            }
            @if !waiting.is_empty() {
                ul.waits {
                    @for (session, state) in &waiting {
                        li { (who(ctx, session)) ": " (wait_words(ctx, *state)) }
                    }
                }
            }
        }
    }
}

fn act_words(ctx: &Ctx<'_>, entry: &Entry) -> Markup {
    match &entry.act {
        Act::Claim {
            subject: s,
            took_from,
        } => html! {
            "claimed " (subject(ctx, s))
            @if let Some(from) = took_from { " from " (who(ctx, from)) }
        },
        Act::Release { subject: s } => html! { "released " (subject(ctx, s)) },
        Act::Wait { on, then, ticket } => html! {
            "waits " @if matches!(on, WaitOn::Rollout(_) | WaitOn::Subject(_)) { "on " } (wait_on(on, ctx))
            @if let Some(ticket) = ticket { " about " (subject(ctx, &Subject::Ticket(ticket.clone()))) }
            " → then: " (redact(then.as_str()))
        },
        Act::Unwait => html! { "stopped waiting" },
        Act::RolloutStart { rollout, ships } => {
            let shipped: Vec<&Subject> = ships.iter().collect();
            html! {
                "started " code { "rollout:" (rollout.as_str()) }
                @if !shipped.is_empty() { " shipping " (subjects(ctx, &shipped)) }
            }
        }
        Act::RolloutFinish {
            rollout,
            outcome,
            note,
        } => html! {
            "finished " code { "rollout:" (rollout.as_str()) } ": "
            span class={ "outcome " (outcome_class(*outcome)) } { (outcome.words()) }
            @if let Some(note) = note { " (" (redact(note.as_str())) ")" }
        },
        Act::Defer {
            ticket,
            until,
            check,
        } => html! {
            "deferred the close check of " (subject(ctx, &Subject::Ticket(ticket.clone())))
            " until " (when(ctx, *until)) ": " (redact(check.as_str()))
        },
    }
}

/// "Close check deferred until <when>: <check>" for a ticket whose check is put off; nothing
/// otherwise. For the pages that show a ticket: the work items, the map's ticket card.
pub(super) fn deferral(ctx: &Ctx<'_>, ticket: &JiraKey) -> Markup {
    html! {
        @if let CloseCheck::Deferred(deferral) = ctx.model.coordination.ledger.close_check(ticket, ctx.now) {
            span.deferred {
                "close check deferred until " (when(ctx, deferral.until)) ": " (redact(deferral.check.as_str()))
            }
        }
    }
}
