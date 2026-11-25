//! The pages that are not a page of their own: the inbox, the session lists, one session
//! and health.

use maud::{Markup, html};

use super::card::{cwd_text, inbox_card, link_chips, session_name, session_row};
use super::fmt::{
    activity_detail, ago, badge, clock, collapsible, questions, redact, since_of, trigger_phrase,
};
use super::reach::{Reach, reach};
use super::reply::{Answers, Delivery, answerable, waiting_turn};
use super::{Ctx, NO_HOOK_YET, NotFound, everything_read, link_note, problem_list, problems};
use crate::model::{
    FollowFaults, InboxEntry, Notifications, ScanProgress, Seen, SessionView, SourceState, Tier,
};
use crate::types::SessionId;
use crate::web::route::Page;

/// The most dormant sessions the page lists; the rest are counted, not drawn.
const DORMANT_SHOWN_MAX: usize = 500;

/// What an empty list says, which is calm only if every source was read and every session
/// classified.
fn nothing_here(ctx: &Ctx<'_>, calm: &str) -> Markup {
    html! {
        @if everything_read(ctx.model) {
            p.empty { (calm) }
        } @else {
            p.empty {
                "Nothing is listed, but that is not the same as nothing waiting: "
                (problem_list(&problems(ctx.model))) ". See the health page."
            }
        }
    }
}

// --- the inbox ---

/// The cards in order: unseen blocked sessions, then review requests, then the rest of the
/// session cards. A colleague's review is a click, and waits on nothing but the user.
pub(super) fn inbox(ctx: &Ctx<'_>) -> Markup {
    let entries: &[InboxEntry] = &ctx.model.inbox;
    let blocked = entries
        .iter()
        .take_while(|entry| {
            entry.seen == Seen::Unseen
                && matches!(entry.rank.tier(), Tier::Blocked | Tier::QuotaHold)
        })
        .count();
    let (first, rest) = entries.split_at_checked(blocked).unwrap_or((entries, &[]));
    html! {
        @if entries.is_empty() && ctx.model.reviews.cards().is_empty() {
            (nothing_here(ctx, "Nothing is waiting on you."))
        }
        @for entry in first { (inbox_card(ctx, entry)) }
        (super::reviews::section(ctx))
        @for entry in rest { (inbox_card(ctx, entry)) }
    }
}

// --- sessions ---

/// The box that hides the rows whose `data-filter` text does not contain what is typed (the
/// page script reads it); kept in the URL, so the page's reloads keep it. `placeholder` says
/// what a row's text holds.
pub(super) fn filter_box(placeholder: &str) -> Markup {
    html! {
        p {
            input #filter type="search" placeholder=(placeholder) size="40";
        }
    }
}

const SESSION_FILTER: &str = "filter by title, directory or gist";

pub(super) fn sessions(ctx: &Ctx<'_>) -> Markup {
    html! {
        (super::saves::panel(ctx))
        @if ctx.model.sessions.is_empty() {
            (nothing_here(ctx, "No live sessions."))
        } @else {
            (filter_box(SESSION_FILTER))
        }
        @for kind in super::fmt::Kind::LIVE {
            @let group: Vec<&SessionView> = ctx
                .model
                .sessions
                .iter()
                .filter(|view| super::fmt::Kind::of(&view.session.activity) == kind)
                .collect();
            @if !group.is_empty() {
                h2 { (kind.label()) " (" (group.len()) ")" }
                @for view in group { (session_row(ctx, view)) }
            }
        }
    }
}

/// The dormant sessions: the pinned ones under their own heading, all of them, in pin order;
/// then the rest, newest first, at most [`DORMANT_SHOWN_MAX`].
pub(super) fn dormant(ctx: &Ctx<'_>) -> Markup {
    let all = &ctx.model.dormant;
    let (pinned, rest): (Vec<&SessionView>, Vec<&SessionView>) = all
        .iter()
        .partition(|view| matches!(view.pin, crate::model::Pin::Pinned(_)));
    html! {
        @if all.is_empty() {
            (nothing_here(ctx, "No dormant sessions."))
        } @else {
            (filter_box(SESSION_FILTER))
            @if !pinned.is_empty() {
                h2 { "Pinned (" (pinned.len()) ")" }
                @for view in &pinned { (session_row(ctx, view)) }
                h2 { "Others (" (rest.len()) ")" }
            }
            @if rest.len() > DORMANT_SHOWN_MAX {
                p.muted {
                    "Showing the newest " (DORMANT_SHOWN_MAX) " of " (rest.len())
                    " dormant sessions."
                }
            }
            @for view in rest.iter().take(DORMANT_SHOWN_MAX) { (session_row(ctx, view)) }
        }
    }
}

// --- one session ---

pub(super) fn session(ctx: &Ctx<'_>, id: &SessionId) -> Result<Markup, NotFound> {
    let view = ctx
        .model
        .session(id)
        .ok_or_else(|| NotFound::Session(id.clone()))?;
    let activity = &view.session.activity;
    let since = since_of(activity, view.pending.as_ref());
    let name = session_name(ctx, view.title.as_ref(), id);
    // Only the turn the session waits on takes a reply: an older one was answered already.
    let waiting = waiting_turn(view);
    let delivery = Delivery::of(&Reach::of(view));
    Ok(html! {
        h2 { (name.text()) }
        p {
            (badge(activity, since, ctx.now))
            @if let Some(detail) = activity_detail(activity) { " " span.muted { (detail) } }
        }
        p.cwd { (cwd_text(&view.session)) }
        (reach(view))
        (link_chips(ctx, id, &view.links))
        (super::owners::session_section(ctx, id))
        h2 { "Last turn ends" }
        @if view.last_turn_ends.is_empty() { p.empty { "No turn has ended." } }
        @for (at, end) in view.last_turn_ends.iter().enumerate() {
            @let (text, withheld) = ctx.counting_withheld(|| {
                let body = collapsible(&format!("more-{id}-{at}"), end.text.redacted(), ctx.markdown(end.text.redacted()));
                html! {
                    (answerable(id, end.at, Answers::Turn, delivery.filter(|_| waiting == Some(end.at)), body))
                    @if let Some(asked) = &end.asked { (questions(ctx, asked)) }
                }
            });
            article.panel {
                p.muted { "Ended " (ago(end.at, ctx.now)) " " (trigger_phrase(&end.trigger)) " (" (clock(ctx.zone, end.at)) ")" }
                (text)
                (link_note(withheld))
            }
        }
    })
}

// --- health ---

pub(super) fn health(ctx: &Ctx<'_>) -> Markup {
    let health = &ctx.model.health;
    html! {
        p.muted { "Model built " (clock(ctx.zone, ctx.model.generated_at)) " (" (ago(ctx.model.generated_at, ctx.now)) ")." }
        table {
            tr { th { "Source" } th { "State" } th { "When" } th { "Detail" } }
            (source_row(ctx, "listing (claude agents)", &health.listing, ""))
            (source_row(ctx, "registry and process table", &health.registry, ""))
            (source_row(ctx, "event log", &health.event_log,
                &format!("{} lines without a usable event", health.event_log_line_errors)))
            (source_row(ctx, "transcript discovery", &health.transcripts.discovery, ""))
            (count_row("transcripts found", &health.transcripts.files.to_string(), false))
            (count_row("transcript entries passed over", &health.transcripts.skipped.to_string(), false))
            (count_row("transcript lines without a record", &health.transcripts.line_errors.to_string(), false))
            (count_row("transcripts unreadable", &health.transcripts.unreadable.to_string(), health.transcripts.unreadable > 0))
            (tail_faults_row(ctx, &health.follow_faults))
            (unclassified_row(ctx.model.unknown_sessions))
            (scan_row(&health.transcripts.progress))
            (count_row("pull requests read", &health.gh.fresh.to_string(), false))
            (count_row("pull requests not found", &health.gh.not_found.to_string(), false))
            (count_row("pull requests stale", &health.gh.stale.to_string(), health.gh.stale > 0))
            (count_row("pull requests failed", &health.gh.failed.to_string(), health.gh.failed > 0))
            (count_row("pull requests not asked yet", &health.gh.unread.to_string(), false))
            (source_row(ctx, "seen.json", &health.seen, ""))
            @match &health.work_items {
                Some(state) => (source_row(ctx, "Jira work items (acli)", state, "")),
                None => tr { td { "Jira work items (acli)" } td { "not configured" } td {} td { "no [jira] section in the config" } },
            }
            @if let Some(state) = &health.work_item_comments {
                (source_row(ctx, "Jira comments of the work items (acli)", state, "changed items asked every two minutes, and read, while a page was viewed in the last hour"))
            }
            @match &health.reviews {
                Some(state) => (source_row(ctx, "review requests (Slack, gh)", state, "read while a page was viewed in the last hour")),
                None => tr { td { "review requests (Slack, gh)" } td { "not configured" } td {} td { "no [reviews] section in the config" } },
            }
            (source_row(ctx, "card gists (claude haiku)", &health.gists, "written while a page was viewed in the last hour"))
            (source_row(ctx, "plan usage (curl)", ctx.model.usage.state(), "read once a minute while a page was viewed in the last hour, slower after a refusal as too frequent"))
            (source_row(ctx, "coordination ledger", &ctx.model.coordination.state, "written by the sessions' conn claim, wait and rollout"))
            @if let Some(linked) = &health.work_items_linked {
                (source_row(ctx, "Jira work items with a blocking link (acli)", linked, "asked every ten minutes while a page was viewed in the last hour"))
            }
            @if let Some(links) = &ctx.model.coordination.links {
                (source_row(ctx, "Jira links of the map's tickets and the linked work items (acli)", links, "each read every ten minutes while a page was viewed in the last hour"))
            }
            @if let Some(children) = &health.epic_children {
                (source_row(ctx, "Jira children of the open epics (acli)", children, "each epic read every ten minutes while a page was viewed in the last hour"))
            }
            (count_row("coordination ledger lines that could not be read", &ctx.model.coordination.bad_lines.len().to_string(), !ctx.model.coordination.bad_lines.is_empty()))
            (count_row("links that cannot be rendered", &health.links_unrenderable.to_string(), health.links_unrenderable > 0))
            (count_row("listed sessions without an id yet", &health.unidentified_listed.to_string(), false))
            (notifications_row(ctx, &health.notifications))
        }
    }
}

fn source_row(ctx: &Ctx<'_>, name: &str, state: &SourceState, extra: &str) -> Markup {
    let (word, bad, when, detail) = match state {
        SourceState::Ok { at } => ("ok", false, Some(*at), String::new()),
        SourceState::Failed { at, reason } => ("failed", true, Some(*at), reason.clone()),
        SourceState::NeverRead => ("not read yet", true, None, String::new()),
        SourceState::NeverWritten { at, path } => (
            "never written",
            true,
            Some(*at),
            format!("no file at {}: {NO_HOOK_YET}", path.display()),
        ),
    };
    let detail = [detail.as_str(), extra]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    html! {
        tr {
            td { (name) }
            td class=[bad.then_some("bad")] { (word) }
            td {
                @if let Some(at) = when { (clock(ctx.zone, at)) " (" (ago(at, ctx.now)) ")" }
            }
            td { (redact(&detail)) }
        }
    }
}

/// The transcripts that could not be followed on the latest poll, with the latest reason.
fn tail_faults_row(ctx: &Ctx<'_>, faults: &FollowFaults) -> Markup {
    html! {
        tr {
            td { "transcript tails not followed" }
            td class=[(faults.transcripts > 0).then_some("bad")] { (faults.transcripts) }
            td {
                @if let Some(fault) = &faults.latest { (clock(ctx.zone, fault.at)) " (" (ago(fault.at, ctx.now)) ")" }
            }
            td { @if let Some(fault) = &faults.latest { (redact(&fault.reason)) } }
        }
    }
}

/// The sessions whose activity could not be told, each listed with its reason on the sessions
/// page.
fn unclassified_row(sessions: u32) -> Markup {
    html! {
        tr {
            td { "sessions that could not be classified" }
            td class=[(sessions > 0).then_some("bad")] { (sessions) }
            td {}
            td { @if sessions > 0 { a href=(Page::Sessions.to_string()) { "see each one's reason" } } }
        }
    }
}

fn count_row(name: &str, value: &str, bad: bool) -> Markup {
    html! {
        tr {
            td { (name) }
            td class=[bad.then_some("bad")] { (value) }
            td {}
            td {}
        }
    }
}

fn scan_row(progress: &ScanProgress) -> Markup {
    let (text, bad) = match progress {
        ScanProgress::Complete => ("complete".to_owned(), false),
        ScanProgress::InProgress { scanned, total } => {
            (format!("still scanning: {scanned} of {total}"), true)
        }
    };
    html! {
        tr { td { "transcript scan" } td class=[bad.then_some("bad")] { (text) } td {} td {} }
    }
}

fn notifications_row(ctx: &Ctx<'_>, state: &Notifications) -> Markup {
    let (word, bad, when, detail) = match state {
        Notifications::Untried => ("none needed yet", false, None, String::new()),
        Notifications::Sent { at } => ("sent", false, Some(*at), String::new()),
        Notifications::Missing { at } => (
            "notify-send is not installed",
            true,
            Some(*at),
            "desktop notifications are off".to_owned(),
        ),
        Notifications::Failed { at, reason } => ("failed", true, Some(*at), reason.clone()),
    };
    html! {
        tr {
            td { "desktop notification" }
            td class=[bad.then_some("bad")] { (word) }
            td { @if let Some(at) = when { (clock(ctx.zone, at)) " (" (ago(at, ctx.now)) ")" } }
            td { (redact(&detail)) }
        }
    }
}
