//! The map page: the active sessions and what ties them, as a dependency graph.
//!
//! Three columns: the sessions, the rollouts, then the tickets, topics and Slack threads. An
//! edge is something a session declared (owns, runs, waits on), a ticket a rollout ships, or a
//! collision its writes made in the last day (see [`crate::map::deps`]). The graph is drawn
//! here as SVG, with no script: each node links to its detail card, which the style sheet shows
//! while the URL's fragment names it, so a click brings up the details and the next click
//! another node's.

use std::collections::HashMap;

use maud::{Markup, html};

use super::Ctx;
use super::fmt::{badge, redact, since_of};
use super::owners::{name, owned_by, rollout_block, session_line, subject, target, writes_table};
use super::reach::reach;
use crate::ledger::{Release, Subject};
use crate::map::deps::{self, Deps, Edge, From, Relation, Thing};
use crate::map::{Anchor, Motion, Progress};
use crate::model::{Pin, SessionView, StatusSection, WorkItemRow, WorkItemsView};
use crate::types::outward::Target;
use crate::types::{JiraKey, Link, SessionId};

/// The columns' left edges and widths, in the drawing's own units.
const SESSIONS_X: u32 = 0;
const SESSIONS_W: u32 = 330;
const ROLLOUTS_X: u32 = 410;
const ROLLOUTS_W: u32 = 200;
const TARGETS_X: u32 = 690;
const TARGETS_W: u32 = 310;
/// How far an arc between two tickets of the last column reaches past it.
const ARC_W: u32 = 70;
const WIDTH: u32 = TARGETS_X + TARGETS_W + ARC_W;
/// A node's height, and the distance from one row to the next.
const NODE_H: u32 = 42;
const ROW_H: u32 = 54;
/// How many characters of a label fit a column.
const SESSION_CHARS: usize = 40;
const ROLLOUT_CHARS: usize = 22;
const TARGET_CHARS: usize = 38;

pub(super) fn page(ctx: &Ctx<'_>) -> Markup {
    // The live sessions, and the dormant ones the human pinned to come back to.
    let active: Vec<SessionId> = ctx
        .model
        .sessions
        .iter()
        .chain(ctx.model.dormant.iter().filter(|view| is_pinned(view)))
        .map(|view| view.session.id.clone())
        .collect();
    let coordination = &ctx.model.coordination;
    let graph = deps::build(
        &active,
        &coordination.ledger,
        &coordination.shared,
        &coordination.dependencies,
        ctx.now,
    );
    html! {
        p.muted {
            "The active sessions, and the dormant ones you pinned, and what ties them: what each owns, the rollouts and tickets they \
             run or wait on, what those ship, the tickets Jira says block others, and where two \
             sessions wrote to the same ticket or thread in the last day. Click a node for its \
             details."
        }
        (legend())
        @if graph.sessions.is_empty() {
            p.empty {
                "No active or pinned session has claimed anything, runs or waits on a rollout, or collided \
                 with another. Sessions declare these with conn claim, wait and rollout."
            }
        } @else {
            (drawing(ctx, &graph))
        }
        @if !graph.alone.is_empty() {
            h2 { "On their own" }
            p.muted { "Active and pinned sessions tied to nothing above." }
            ul.map-alone {
                @for id in &graph.alone {
                    li { a href={ "#" (node_id_session(id)) } { (name(ctx, id)) } " " (session_badge(ctx, id)) }
                }
            }
        }
        div.map-details {
            @for id in graph.sessions.iter().chain(&graph.alone) { (session_detail(ctx, id)) }
            @for thing in graph.rollouts.iter().chain(&graph.targets) { (thing_detail(ctx, thing, &graph)) }
        }
    }
}

fn legend() -> Markup {
    html! {
        div.map-legend {
            span.key { svg viewBox="0 0 30 8" { line.e-owns x1="0" y1="4" x2="30" y2="4" {} } "owns" }
            span.key { svg viewBox="0 0 30 8" { line.e-runs x1="0" y1="4" x2="30" y2="4" {} } "runs" }
            span.key { svg viewBox="0 0 30 8" { line.e-waits x1="0" y1="4" x2="30" y2="4" {} } "waits on" }
            span.key { svg viewBox="0 0 30 8" { line.e-waits.over x1="0" y1="4" x2="30" y2="4" {} } "wait over" }
            span.key { svg viewBox="0 0 30 8" { line.e-ships x1="0" y1="4" x2="30" y2="4" {} } "ships" }
            span.key { svg viewBox="0 0 30 8" { line.e-collided x1="0" y1="4" x2="30" y2="4" {} } "collided" }
            span.key { svg viewBox="0 0 30 8" { line.e-blocks x1="0" y1="4" x2="30" y2="4" {} } "blocks (Jira)" }
            span.key { svg viewBox="0 0 30 8" { rect.n-rollout.pending x="1" y="1" width="28" height="6" rx="2" {} } "release pending: sessions wait, nobody runs it" }
            span.key { svg viewBox="0 0 30 8" { rect.n-session.awaiting-release x="1" y="1" width="28" height="6" rx="2" {} } "session waits on your release" }
        }
    }
}

/// Where a node sits: its column's left edge and width, and its row's top.
#[derive(Debug, Clone, Copy)]
struct Place {
    x: u32,
    w: u32,
    y: u32,
}

impl Place {
    const fn mid_y(self) -> u32 {
        self.y + NODE_H / 2
    }
}

fn column(len: usize, x: u32, w: u32) -> impl Iterator<Item = Place> {
    (0_u32..).take(len).map(move |row| Place {
        x,
        w,
        y: row * ROW_H,
    })
}

fn drawing(ctx: &Ctx<'_>, graph: &Deps) -> Markup {
    let sessions: HashMap<&SessionId, Place> = graph
        .sessions
        .iter()
        .zip(column(graph.sessions.len(), SESSIONS_X, SESSIONS_W))
        .collect();
    let things: HashMap<&Thing, Place> = graph
        .rollouts
        .iter()
        .zip(column(graph.rollouts.len(), ROLLOUTS_X, ROLLOUTS_W))
        .chain(
            graph
                .targets
                .iter()
                .zip(column(graph.targets.len(), TARGETS_X, TARGETS_W)),
        )
        .collect();
    let rows = graph
        .sessions
        .len()
        .max(graph.rollouts.len())
        .max(graph.targets.len());
    let height = u32::try_from(rows)
        .unwrap_or(u32::MAX)
        .saturating_mul(ROW_H);
    html! {
        svg.deps viewBox={ "0 0 " (WIDTH) " " (height) } role="img" aria-label="Dependency graph" {
            defs {
                marker #arrow viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse" {
                    path d="M0,0L10,5L0,10z" {}
                }
            }
            @for edge in &graph.edges {
                @let from = match &edge.from {
                    From::Session(id) => sessions.get(id).copied(),
                    From::Rollout(name) => things.get(&Thing::Rollout(name.clone())).copied(),
                    From::Ticket(key) => things.get(&Thing::Subject(key.clone())).copied(),
                };
                @if let (Some(from), Some(to)) = (from, things.get(&edge.to).copied()) {
                    path class=(edge_class(edge)) d=(curve(from, to)) marker-end="url(#arrow)" {}
                }
            }
            @for id in &graph.sessions {
                @if let Some(place) = sessions.get(id) { (session_node(ctx, id, *place)) }
            }
            @for thing in graph.rollouts.iter().chain(&graph.targets) {
                @if let Some(place) = things.get(thing) {
                    (thing_node(ctx, thing, *place, graph.orphaned.contains(thing)))
                }
            }
        }
    }
}

/// From the right edge of one node to the left edge of the other, bending halfway; between two
/// nodes of one column, an arc out to the right and back.
fn curve(from: Place, to: Place) -> String {
    if from.x == to.x {
        let x = from.x + from.w;
        let out = x + ARC_W;
        let (y1, y2) = (from.mid_y(), to.mid_y());
        return format!("M{x},{y1} C{out},{y1} {out},{y2} {x},{y2}");
    }
    let (x1, y1) = (from.x + from.w, from.mid_y());
    let (x2, y2) = (to.x, to.mid_y());
    let bend = x1 + (x2.saturating_sub(x1)) / 2;
    format!("M{x1},{y1} C{bend},{y1} {bend},{y2} {x2},{y2}")
}

fn edge_class(edge: &Edge) -> &'static str {
    match edge.relation {
        Relation::Owns => "e-owns",
        Relation::Runs => "e-runs",
        Relation::WaitsOn { over: false } => "e-waits",
        Relation::WaitsOn { over: true } => "e-waits over",
        Relation::Ships => "e-ships",
        Relation::Collided => "e-collided",
        Relation::Blocks => "e-blocks",
    }
}

/// `label` cut to `chars` characters, with an ellipsis when it was longer.
fn cut(label: &str, chars: usize) -> String {
    match label.char_indices().nth(chars) {
        Some((at, _)) => format!("{}…", label.get(..at).unwrap_or_default()),
        None => label.to_owned(),
    }
}

fn motion(ctx: &Ctx<'_>, id: &SessionId) -> Motion {
    ctx.model
        .map
        .sessions
        .iter()
        .find(|node| node.id == *id)
        .map_or(Motion::Unknown, |node| node.motion)
}

const fn motion_names(motion: Motion) -> (&'static str, &'static str) {
    match motion {
        Motion::Moving => ("moving", "moving"),
        Motion::Blocked => ("blocked", "on you"),
        Motion::WaitingOnOthers => ("waiting", "waiting on others"),
        Motion::AwaitingRelease => ("awaiting-release", "waits on your release"),
        Motion::Idle => ("idle", "idle"),
        Motion::Unknown => ("unknown", "state unknown"),
        Motion::Dormant => ("dormant", "dormant"),
    }
}

fn is_pinned(view: &SessionView) -> bool {
    matches!(view.pin, Pin::Pinned(_))
}

fn session_node(ctx: &Ctx<'_>, id: &SessionId, place: Place) -> Markup {
    let (class, words) = motion_names(motion(ctx, id));
    let pinned = ctx.model.session(id).is_some_and(is_pinned);
    let wait = ctx
        .model
        .coordination
        .ledger
        .wait_of(id, ctx.now)
        .map(|state| {
            if state.is_over() {
                "wait over"
            } else {
                "waiting"
            }
        });
    html! {
        a href={ "#" (node_id_session(id)) } {
            rect class={ "n-session " (class) } x=(place.x) y=(place.y) width=(place.w) height=(NODE_H) rx="6" {}
            text x=(place.x + 10) y=(place.y + 17) { (cut(name(ctx, id).as_str(), SESSION_CHARS)) }
            text.sub x=(place.x + 10) y=(place.y + 33) {
                (words) @if pinned { " · pinned" } @if let Some(wait) = wait { " · " (wait) }
            }
        }
    }
}

fn thing_node(ctx: &Ctx<'_>, thing: &Thing, place: Place, orphaned: bool) -> Markup {
    let (label, sub, class) = match thing {
        Thing::Rollout(name) => {
            let release = ctx.model.coordination.ledger.release(name, ctx.now);
            let waiting = release.waiting().len();
            let (class, sub) = match &release {
                Release::Running { .. } if waiting > 0 => {
                    ("running", format!("running · {waiting} waiting"))
                }
                Release::Running { .. } => ("running", "running".to_owned()),
                Release::Pending { .. } => {
                    ("pending", format!("release pending · {waiting} waiting"))
                }
                Release::Quiet { last: Some(_) } => ("finished", "finished".to_owned()),
                Release::Quiet { last: None } => ("not-started", "not started".to_owned()),
            };
            (
                cut(&format!("rollout:{name}"), ROLLOUT_CHARS),
                sub,
                format!("n-rollout {class}"),
            )
        }
        Thing::Subject(text) => {
            let item = JiraKey::parse(text).and_then(|key| work_item(ctx, &key));
            let blockers = blockers_of(ctx, text);
            let sub = match blockers.first() {
                Some(first) => format!("blocked by {first}"),
                None => item.map_or_else(String::new, |(_, row)| row.summary.as_str().to_owned()),
            };
            let class = if blockers.is_empty() {
                progress_class(progress(ctx, text))
            } else {
                "blocking"
            };
            let label = match super::comments::label(ctx, text) {
                Some((badge, _)) => format!("{text} {badge}"),
                None => text.clone(),
            };
            (label, cut(&sub, TARGET_CHARS), format!("n-target {class}"))
        }
        Thing::Thread(channel, _) => (
            "Slack thread".to_owned(),
            cut(channel, TARGET_CHARS),
            "n-target thread".to_owned(),
        ),
    };
    html! {
        a href={ "#" (node_id_thing(thing)) } {
            rect class={ (class) @if orphaned { " orphaned" } } x=(place.x) y=(place.y) width=(place.w) height=(NODE_H) rx="6" {}
            text x=(place.x + 10) y=(place.y + 17) { (redact(&label)) }
            text.sub x=(place.x + 10) y=(place.y + 33) {
                @if orphaned { "owner ended · " }
                (redact(&sub))
            }
        }
    }
}

/// The open tickets Jira says block `key`, as written.
fn blockers_of(ctx: &Ctx<'_>, key: &str) -> Vec<String> {
    ctx.model
        .coordination
        .dependencies
        .iter()
        .filter(|dependency| dependency.blocked.to_string() == key)
        .map(|dependency| dependency.blocker.to_string())
        .collect()
}

/// The open tickets Jira says `key` blocks, as written.
fn blocked_by(ctx: &Ctx<'_>, key: &str) -> Vec<String> {
    ctx.model
        .coordination
        .dependencies
        .iter()
        .filter(|dependency| dependency.blocker.to_string() == key)
        .map(|dependency| dependency.blocked.to_string())
        .collect()
}

/// A ticket written as text, linked to its node's card.
fn ticket_link(key: &str) -> Markup {
    html! { a href={ "#" (node_id_thing(&Thing::Subject(key.to_owned()))) } { (redact(key)) } }
}

/// Where the ticket stands, as the map's anchors read it from Jira and GitHub.
fn progress(ctx: &Ctx<'_>, key: &str) -> Progress {
    ctx.model
        .map
        .anchors
        .iter()
        .find(|anchor| match &anchor.anchor {
            Anchor::Link { identifier, .. } => {
                matches!(identifier.link(), Link::Jira { key: k } if k.to_string() == key)
            }
            Anchor::Directory(_) => false,
        })
        .map_or(Progress::Unknown, |anchor| anchor.progress)
}

const fn progress_class(progress: Progress) -> &'static str {
    match progress {
        Progress::Blocking => "blocking",
        Progress::Waiting => "waiting",
        Progress::Moving => "moving",
        Progress::Queued => "queued",
        Progress::Done => "done",
        Progress::Unknown => "unknown",
    }
}

fn work_item<'m>(ctx: &'m Ctx<'_>, key: &JiraKey) -> Option<(&'m StatusSection, &'m WorkItemRow)> {
    let WorkItemsView::Listed { sections, .. } = &ctx.model.work_items else {
        return None;
    };
    let text = key.to_string();
    sections.iter().find_map(|section| {
        section
            .rows()
            .find(|row| row.key.to_string() == text)
            .map(|row| (section, row))
    })
}

fn node_id_session(id: &SessionId) -> String {
    format!("n-s-{id}")
}

fn node_id_thing(thing: &Thing) -> String {
    match thing {
        Thing::Rollout(name) => format!("n-r-{name}"),
        Thing::Subject(text) => format!("n-t-{text}"),
        Thing::Thread(channel, ts) => format!("n-th-{channel}-{ts}"),
    }
}

fn session_badge(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    html! {
        @if let Some(view) = ctx.model.session(id) {
            (badge(&view.session.activity, since_of(&view.session.activity, view.pending.as_ref()), ctx.now))
        }
    }
}

/// A detail card: shown while the URL's fragment names it.
fn card(id: &str, title: &Markup, body: &Markup) -> Markup {
    html! {
        section.detail id=(id) {
            div.detail-head {
                h3 { (title) }
                a.detail-close href="#map" aria-label="Close" { "×" }
            }
            (body)
        }
    }
}

fn session_detail(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    let view = ctx.model.session(id);
    card(
        &node_id_session(id),
        &html! { (super::owners::who(ctx, id)) " " (session_badge(ctx, id)) },
        &html! {
            @if let Some(gist) = view.and_then(|view| view.gist.as_ref()) { p.gist { span.text { (gist.as_str()) } } }
            (session_line(ctx, id))
            @if let Some(view) = view { (reach(view)) }
        },
    )
}

fn thing_detail(ctx: &Ctx<'_>, thing: &Thing, graph: &Deps) -> Markup {
    let id = node_id_thing(thing);
    match thing {
        Thing::Rollout(name) => card(
            &id,
            &html! { code { "rollout:" (name.as_str()) } },
            &rollout_block(ctx, name),
        ),
        Thing::Subject(text) => {
            let parsed = Subject::parse(text).ok();
            let item = JiraKey::parse(text).and_then(|key| work_item(ctx, &key));
            let shipped_by: Vec<&Edge> = graph
                .edges
                .iter()
                .filter(|edge| edge.relation == Relation::Ships && edge.to == *thing)
                .collect();
            card(
                &id,
                &html! { @match &parsed { Some(s) => (subject(ctx, s)), None => (redact(text)) } },
                &html! {
                    @if let Some((section, row)) = item {
                        p { (row.summary) " " span.muted { "(" (section.status) ")" } (super::work_items::epic_marks(row)) }
                    }
                    @if let Some(s) = &parsed { p { (owned_by(ctx, s)) } }
                    @if let Some(Subject::Ticket(key)) = &parsed { p { (super::owners::deferral(ctx, key)) } }
                    @if graph.orphaned.contains(thing) {
                        p.clash { "Its owner is no longer running: nobody is on it." }
                    }
                    @let blockers = blockers_of(ctx, text);
                    @if !blockers.is_empty() {
                        p.clash {
                            "Blocked by "
                            @for (i, key) in blockers.iter().enumerate() { @if i > 0 { ", " } (ticket_link(key)) }
                            " (Jira)"
                        }
                    }
                    @let blocks = blocked_by(ctx, text);
                    @if !blocks.is_empty() {
                        p {
                            "Blocks "
                            @for (i, key) in blocks.iter().enumerate() { @if i > 0 { ", " } (ticket_link(key)) }
                            " (Jira)"
                        }
                    }
                    @for edge in shipped_by {
                        @if let From::Rollout(name) = &edge.from {
                            p { "Shipped by " code { "rollout:" (name.as_str()) } }
                        }
                    }
                    (collision_writes(ctx, thing))
                },
            )
        }
        Thing::Thread(channel, ts) => card(
            &id,
            &target(
                ctx,
                &Target::Slack {
                    channel: channel.clone(),
                    thread: Some(ts.clone()),
                },
            ),
            &collision_writes(ctx, thing),
        ),
    }
}

/// The writes of a collision on this thing, when there is one.
fn collision_writes(ctx: &Ctx<'_>, thing: &Thing) -> Markup {
    let shared = ctx
        .model
        .coordination
        .shared
        .iter()
        .find(|item| match (&item.target, thing) {
            (Target::Ticket(key), Thing::Subject(text)) => key.to_string() == *text,
            (
                Target::Slack {
                    channel,
                    thread: Some(ts),
                },
                Thing::Thread(c, t),
            ) => channel == c && ts == t,
            (Target::Ticket(_) | Target::Slack { .. }, _) => false,
        });
    html! {
        @if let Some(item) = shared {
            h4 { "Writes" }
            (writes_table(ctx, &item.writes))
        }
    }
}
