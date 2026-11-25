//! The work items page: the user's open Jira items as a plain table per status, each item
//! with the session that owns it in the coordination ledger, or, for one no session owns, the
//! sessions that mention its key. An epic is tinted and says how many of its children are
//! done, and within a status its items are nested under it; an item waiting on an open ticket
//! recedes and names it.
//!
//! Never blank: a list never read says it is being read, a failed read says what failed and
//! when, and a list with a later failure is shown beside the failure with its age.

use maud::{Markup, html};

use super::Ctx;
use super::card::{post_button, session_name};
use super::fmt::{ago, badge, clock, external_link, redact, since_of};
use crate::jira::WorkItemKey;
use crate::ledger::Subject;
use crate::model::{
    Children, Entry, EpicRef, Head, RowKind, Teaser, WorkItemRow, WorkItemsFailure, WorkItemsFault,
    WorkItemsView,
};
use crate::types::{Instant, SessionId};
use crate::util::redact::Redacted;
use crate::web::route::PostRoute;

pub(super) fn page(ctx: &Ctx<'_>) -> Markup {
    match &ctx.model.work_items {
        WorkItemsView::NoJira => html! {
            p.empty { "No Jira is configured: add a [jira] section with host and prefixes to the config to list your work items here." }
        },
        WorkItemsView::NeverRead => html! {
            (header(ctx, None))
            p.empty { "reading…" }
        },
        WorkItemsView::Failed(failure) => html! {
            (header(ctx, None))
            (failure_line(ctx, failure))
        },
        WorkItemsView::Listed {
            read_at,
            sections,
            failure,
        } => html! {
            (header(ctx, Some(*read_at)))
            @if let Some(failure) = failure {
                (failure_line(ctx, failure))
                p.muted { "The list below is from the last read that worked, " (ago(*read_at, ctx.now)) "." }
            }
            @if sections.is_empty() {
                p.empty { "No open work items are assigned to you." }
            }
            @for section in sections {
                h2.items-section { (section.status) " (" (section.rows().count()) ")" }
                (table(ctx, section.entries()))
            }
        },
    }
}

/// How old the list is, and the button that asks for a new one.
fn header(ctx: &Ctx<'_>, read_at: Option<Instant>) -> Markup {
    html! {
        div.actions {
            @if let Some(at) = read_at {
                span.muted { "Read " (ago(at, ctx.now)) " (" (clock(ctx.zone, at)) ")" }
            } @else {
                span.muted { "Not read yet" }
            }
            (post_button(PostRoute::RefreshWorkItems, &[], "Refresh", ""))
        }
    }
}

fn failure_line(ctx: &Ctx<'_>, failure: &WorkItemsFailure) -> Markup {
    html! {
        p.problem {
            @match &failure.fault {
                WorkItemsFault::NotAuthenticated { hint } => {
                    "acli is not logged in: " (hint) " once"
                }
                WorkItemsFault::Failed { reason } => {
                    "Reading the work items failed " (ago(failure.at, ctx.now))
                    " (" (clock(ctx.zone, failure.at)) "): " (reason)
                }
            }
        }
    }
}

/// The section's entries, each epic's items under it like files under a directory: `├` before
/// each but the last, `└` before the last.
fn table(ctx: &Ctx<'_>, entries: &[Entry]) -> Markup {
    html! {
        table.items {
            tr {
                th { "Key" } th { "Summary" } th { "Creator" }
                th { "Description" } th { "Owner" }
            }
            @for entry in entries {
                @match &entry.head {
                    Head::Row(row) => (item_row(ctx, row, Nesting::Top)),
                    Head::Epic(epic) => (epic_head(epic)),
                }
                @for (at, child) in entry.children.iter().enumerate() {
                    @let nesting = if at + 1 == entry.children.len() { Nesting::Last } else { Nesting::Child };
                    (item_row(ctx, child, nesting))
                }
            }
        }
    }
}

/// Where a row sits in its section's tree.
#[derive(Clone, Copy)]
enum Nesting {
    Top,
    /// Under an epic, with more of its items below.
    Child,
    /// The last item under an epic.
    Last,
}

fn item_row(ctx: &Ctx<'_>, row: &WorkItemRow, nesting: Nesting) -> Markup {
    let branch = match nesting {
        Nesting::Top => None,
        Nesting::Child => Some("├ "),
        Nesting::Last => Some("└ "),
    };
    html! {
        tr class=[row_class(row, nesting)] {
            td.key {
                @if let Some(branch) = branch { span.branch aria-hidden="true" { (branch) } }
                (key(row)) (super::comments::badge(ctx, &row.key.to_string()))
                (epic_marks(row))
            }
            td { (row.summary) (waits_line(row)) }
            td.creator { @if let Some(creator) = &row.creator { (creator) } }
            td.desc {
                @if let Some(teaser) = &row.description {
                    div.clamp title=(teaser_text(teaser)) { (teaser_text(teaser)) }
                }
            }
            td.sessions { (owner_cell(ctx, row)) }
        }
    }
}

/// The line naming an epic outside the section, over the section's items in it.
fn epic_head(epic: &EpicRef) -> Markup {
    let text = redact(&epic.key.to_string());
    html! {
        tr.epic.epic-head {
            td.key {
                @match &epic.url {
                    Some(url) => (external_link(url, &html! { (text) })),
                    None => (text),
                }
                " " span.tag.epic { "epic" }
            }
            td colspan="4" { (epic.summary) " " span.muted { "(not in this status)" } }
        }
    }
}

/// How a row stands out: an epic is tinted, an item waiting on an open ticket recedes, an item
/// under an epic is indented.
fn row_class(row: &WorkItemRow, nesting: Nesting) -> Option<String> {
    let classes: Vec<&str> = [
        matches!(row.kind, RowKind::Epic(_)).then_some("epic"),
        row.is_waiting().then_some("waiting"),
        matches!(nesting, Nesting::Child | Nesting::Last).then_some("child"),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!classes.is_empty()).then(|| classes.join(" "))
}

/// An epic's tag and how many of its children are done; nothing for any other item.
pub(super) fn epic_marks(row: &WorkItemRow) -> Markup {
    let RowKind::Epic(children) = row.kind else {
        return html! {};
    };
    html! {
        " " span.tag.epic { "epic" } " "
        @match children {
            Children::NotRead => span.muted { "children not read yet" },
            Children::Unreadable => span.muted title="see the health page" { "children could not be read" },
            Children::Counted(progress) if progress.total() == 0 => span.muted { "no children" },
            Children::Counted(progress) => {
                @let over = if progress.may_be_cut() { "≥" } else { "" };
                span.epic-progress title={ (progress.done()) " of " (over) (progress.total()) " children done" } {
                    meter value=(progress.done()) max=(progress.total()) {}
                    (progress.done()) "/" (over) (progress.total())
                }
            }
        }
    }
}

/// The open tickets the item waits for, each opening in Jira; nothing when it waits for none.
pub(super) fn waits_line(row: &WorkItemRow) -> Markup {
    html! {
        @if row.is_waiting() {
            div.waits-on {
                "waits on "
                @for (at, blocker) in row.waits_on.iter().enumerate() {
                    @if at > 0 { ", " }
                    @let text = redact(&blocker.key.to_string());
                    @match &blocker.url {
                        Some(url) => (external_link(url, &html! { (text) })),
                        None => (text),
                    }
                }
            }
        }
    }
}

/// The session that owns the item in the coordination ledger. An item no session owns says
/// so, with the sessions that mention it folded away: the ones that might take it.
fn owner_cell(ctx: &Ctx<'_>, row: &WorkItemRow) -> Markup {
    let owner = Subject::parse(&row.key.to_string())
        .ok()
        .and_then(|subject| ctx.model.coordination.ledger.owner(&subject).cloned());
    let count = row.sessions.len();
    html! {
        @if let WorkItemKey::Configured(key) = &row.key { (super::owners::deferral(ctx, key)) }
        @if let Some(held) = owner {
            div.chips { (session_chip(ctx, &held.session)) }
        } @else {
            span.muted { "no owner" }
            @if count > 0 {
                details.mentions {
                    summary { "mentioned in " (count) @if count == 1 { " session" } @else { " sessions" } }
                    div.chips { @for session in &row.sessions { (session_chip(ctx, session)) } }
                }
            }
        }
    }
}

/// The key, opening the item in Jira in a new tab; plain text for a key with no page to go to.
fn key(row: &WorkItemRow) -> Markup {
    let text = redact(&row.key.to_string());
    if let Some(url) = &row.url {
        external_link(url, &html! { (text) })
    } else {
        html! { (text) }
    }
}

fn teaser_text(teaser: &Teaser) -> Redacted<String> {
    let cut = if teaser.cut { "…" } else { "" };
    redact(&format!("{}{cut}", teaser.text.as_str()))
}

/// A session: its page, its activity, and whether it has a card.
fn session_chip(ctx: &Ctx<'_>, id: &SessionId) -> Markup {
    let Some(view) = ctx.model.session(id) else {
        return html! { span.muted { "session " (id.as_str()) " is not in the current model" } };
    };
    let activity = &view.session.activity;
    html! {
        span.chip {
            (session_name(ctx, view.title.as_ref(), id).link(id))
            " "
            (badge(activity, since_of(activity, view.pending.as_ref()), ctx.now))
            @if view.pending.is_some() { " " span.tag { "pending" } }
        }
    }
}
