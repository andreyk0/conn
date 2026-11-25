//! The Follow-ups page: what is due, who should act on it, and the prompt to hand them.
//!
//! The prompt reaches a session the way the reply composer's message does: copied, with the
//! session's terminal raised, or as a command that resumes the session or starts a new one with
//! it. The human pastes and runs it; the server starts nothing.

use maud::{Markup, html};

use super::Ctx;
use super::fmt::{ago, ahead, external_link, moment, redact};
use super::owners::{subject, who};
use super::reach::Reach;
use crate::ledger::Deferral;
use crate::ledger::command::local_time;
use crate::model::followups::{
    ClosingItem, Due, FollowUp, FollowUpsView, Handler, Hold, NotYet, Over, Presence, TicketReason,
    on_words,
};
use crate::model::{StatusSection, WorkItemsView};
use crate::util::shell::quote;

/// How many of the sessions that named a ticket a row lists, the active first; the rest are
/// counted. A ticket worked on for weeks is named in dozens.
const MENTIONS_SHOWN: usize = 3;

pub(super) fn page(ctx: &Ctx<'_>) -> Markup {
    let (rows, not_yet) = match &ctx.model.followups {
        FollowUpsView::Off => {
            return html! {
                p.empty { "Follow-ups are off: add a [followups] section to the config to list what is due here." }
            };
        }
        FollowUpsView::On { due, not_yet } => (due, not_yet),
    };
    html! {
        p.muted {
            "What is due and who is best placed to act: comments on your work items after your \
             latest one, sessions whose wait is over, releases sessions wait on that nobody runs, \
             claims whose owner is gone, and one sweep over the work items ready for their close \
             check. Each comes with a prompt for the session that should act; you paste it \
             and press Enter."
        }
        (comments_note(ctx))
        @if rows.is_empty() { p.empty { "Nothing is due." } }
        @for row in rows { (follow_up(ctx, row)) }
        @if !not_yet.is_empty() {
            h2 { "Not yet" }
            p.muted {
                "Waiting for a close check they are not ready for: still rolling out (conn rollout \
                 --ships), with an owner that may be running, or put off (conn defer). Each joins \
                 the sweep once that is over."
            }
            table.not-yet {
                thead { tr { th { "Deferred until" } th { "In" } th { "Item" } th { "Held by" } th { "To check then" } } }
                tbody {
                    @for item in not_yet { (not_yet_row(ctx, item)) }
                }
            }
        }
    }
}

/// One item not ready for its check: until when it is put off and how far off that is (blank
/// when it is not), the item, what holds it, and what to check then.
fn not_yet_row(ctx: &Ctx<'_>, item: &NotYet) -> Markup {
    let deferral = item.hold.deferral();
    html! {
        tr {
            td.when {
                @if let Some(deferral) = deferral { strong { (moment(ctx.zone, deferral.until, ctx.now)) } }
                @else { span.muted { "—" } }
            }
            td.when.muted { @if let Some(deferral) = deferral { (ahead(deferral.until, ctx.now)) } }
            td { (item_link(&item.item)) " " span.muted { (item.item.summary) } }
            td { (hold(ctx, &item.hold)) }
            td.muted { @if let Some(deferral) = deferral { (redact(deferral.check.as_str())) } }
        }
    }
}

/// What holds the item: the rollout or owner, and the session that put it off where one did.
/// Each session links to its page, running or not.
fn hold(ctx: &Ctx<'_>, hold: &Hold) -> Markup {
    let also = |deferred: &Option<Deferral>| {
        html! { @if let Some(deferral) = deferred { br; (put_off(ctx, deferral)) } }
    };
    match hold {
        Hold::RollingOut {
            rollout,
            run,
            deferred,
        } => html! {
            "rolling out in " code { "rollout:" (rollout.as_str()) } ", run by " (who(ctx, &run.session))
            " since " (local_time(run.since, ctx.zone)) (also(deferred))
        },
        Hold::WithOwner {
            owner,
            presence,
            deferred,
        } => html! {
            "its owner " (who(ctx, owner)) ", which " (presence_words(*presence)) (also(deferred))
        },
        Hold::Deferred(deferral) => put_off(ctx, deferral),
    }
}

/// `deferred by <session>`.
fn put_off(ctx: &Ctx<'_>, deferral: &Deferral) -> Markup {
    html! { "deferred by " (who(ctx, &deferral.by)) }
}

/// The item's key, opening it in Jira when it has a page.
fn item_link(item: &ClosingItem) -> Markup {
    html! {
        @match &item.url {
            Some(url) => (external_link(url, &html! { (redact(&item.key.to_string())) })),
            None => code { (redact(&item.key.to_string())) },
        }
    }
}

/// Whether the comments a "someone asked" row is read from have been read: until they are,
/// an empty page says nothing about them.
fn comments_note(ctx: &Ctx<'_>) -> Markup {
    let read = match &ctx.model.work_items {
        WorkItemsView::Listed { sections, .. } => sections
            .iter()
            .flat_map(StatusSection::rows)
            .any(|row| row.unanswered.is_some()),
        WorkItemsView::NoJira | WorkItemsView::NeverRead | WorkItemsView::Failed(_) => false,
    };
    html! {
        @if !read {
            p.muted { "No work item's comments have been read yet, so nobody's question is listed." }
        }
    }
}

fn follow_up(ctx: &Ctx<'_>, row: &FollowUp) -> Markup {
    html! {
        article.row.followup {
            h3 { (what(ctx, &row.due)) }
            (why(ctx, row))
            (act(ctx, row))
        }
    }
}

fn what(ctx: &Ctx<'_>, due: &Due) -> Markup {
    match due {
        Due::Ticket {
            key, url, summary, ..
        } => html! {
            @match url {
                Some(url) => (external_link(url, &html! { (redact(&key.to_string())) })),
                None => code { (redact(&key.to_string())) },
            }
            " " span.muted { (summary) }
        },
        Due::CloseChecks { items } => html! {
            "Close checks: " (items.len()) @if items.len() == 1 { " work item" } @else { " work items" }
        },
        Due::Topic { subject: s, .. } => subject(ctx, s),
        Due::Wait { session, .. } => html! { "The wait of " (who(ctx, session)) },
        Due::Release { rollout, .. } => {
            html! { code { "rollout:" (rollout.as_str()) } " release pending" }
        }
    }
}

fn why(ctx: &Ctx<'_>, row: &FollowUp) -> Markup {
    match &row.due {
        Due::Ticket {
            key,
            reasons,
            mentioned_in,
            ..
        } => html! {
            ul.reasons {
                @for reason in reasons {
                    li {
                        @match reason {
                            TicketReason::Asked { count, newest } => {
                                strong.over { (count) " comment(s) after your latest" }
                                ", the newest " (ago(*newest, ctx.now))
                            }
                            TicketReason::OwnerGone { owner } => {
                                "Owned by " (who(ctx, owner)) ", which is gone: nobody can resume it"
                            }
                        }
                    }
                }
            }
            @if !mentioned_in.is_empty() {
                p.muted {
                    (redact(&key.to_string())) " is named in "
                    @for (i, id) in mentioned_in.iter().take(MENTIONS_SHOWN).enumerate() { @if i > 0 { ", " } (who(ctx, id)) }
                    @if mentioned_in.len() > MENTIONS_SHOWN {
                        " and " (mentioned_in.len().saturating_sub(MENTIONS_SHOWN)) " more"
                    }
                }
            }
        },
        Due::Topic { owner, .. } => html! {
            p { "Owned by " (who(ctx, owner)) ", which is gone: nobody can resume it." }
        },
        Due::Wait { on, then, over, .. } => html! {
            p {
                strong.over { "Wait over" } ": " code { (redact(&on_words(on))) } " "
                @match over {
                    Over::Rollout { outcome, at } => { (outcome.words()) " " (ago(*at, ctx.now)) }
                    Over::Time { at } => { "reached " (ago(*at, ctx.now)) }
                    Over::Released { by, at } => { "released by " (who(ctx, by)) " " (ago(*at, ctx.now)) }
                    Over::TicketClosed { ticket, resolution, at } => {
                        "— " (super::owners::subject(ctx, &crate::ledger::Subject::Ticket(ticket.clone())))
                        " closed (" (redact(resolution)) ") " (ago(*at, ctx.now))
                    }
                }
                " → next: " (redact(then.as_str()))
            }
        },
        Due::Release { waiting, .. } => html! {
            p.clash {
                (super::owners::pending_words(waiting.len())) " Waiting: "
                @for (i, id) in waiting.iter().enumerate() { @if i > 0 { ", " } (who(ctx, id)) }
            }
        },
        Due::CloseChecks { items } => html! {
            p.muted { "Shipped and waiting for their close check, which can be done now; one session sweeps them all." }
            ul.reasons {
                @for due in items {
                    li {
                        (item_link(&due.item)) " " span.muted { (due.item.summary) } " (" (due.item.status) ")"
                        @if let Some(deferral) = &due.reached {
                            br;
                            "Deferred until " (moment(ctx.zone, deferral.until, ctx.now)) " by " (who(ctx, &deferral.by))
                            ", which has come: " (redact(deferral.check.as_str()))
                        }
                    }
                }
            }
        },
    }
}

/// Who acts, and the one way to hand them the prompt.
fn act(ctx: &Ctx<'_>, row: &FollowUp) -> Markup {
    // The prompt is the session's words as much as conn's: a wait's plan is what it wrote.
    let prompt = row
        .prompt()
        .map(|prompt| redact(&prompt).as_str().to_owned());
    match (&row.handler, prompt) {
        (Handler::You, _) | (_, None) => {
            html! { p { strong { "You" } ": start the release, or ask the session that runs it." } }
        }
        (Handler::NewSession { dir }, Some(prompt)) => new_session(dir.as_deref(), &prompt),
        (Handler::Session { id, presence }, Some(prompt)) => {
            let Some(view) = ctx.model.session(id) else {
                return new_session(None, &prompt);
            };
            let reach = Reach::of(view);
            html! {
                p { strong { "Its session" } " " (who(ctx, id)) " " span.muted { (presence_words(*presence)) } }
                @match &reach {
                    Reach::Terminal { .. } => {
                        div.cmd.prompt {
                            pre { (prompt) }
                            button.copy-open.primary type="button" { "Copy & open terminal" }
                        }
                        div.actions { (reach.button()) }
                        (reach.below())
                    }
                    Reach::Resume(view) => {
                        @match &view.resume {
                            Ok(command) => (command_block(&format!("{command} {}", quote(&prompt)))),
                            Err(why) => {
                                p.muted { "No resume command: " (why.to_string()) }
                                (command_block(&prompt))
                            }
                        }
                    }
                    Reach::Gone => (new_session(None, &prompt)),
                }
            }
        }
    }
}

const fn presence_words(presence: Presence) -> &'static str {
    match presence {
        Presence::Working => "is at work",
        Presence::Live => "is idle",
        Presence::Dormant => "is dormant: resume it with the prompt",
        Presence::Unknown => "could not be told apart from running",
    }
}

fn new_session(dir: Option<&std::path::Path>, prompt: &str) -> Markup {
    let command = match dir {
        Some(dir) => format!(
            "cd {} && claude {}",
            quote(&dir.to_string_lossy()),
            quote(prompt)
        ),
        None => format!("claude {}", quote(prompt)),
    };
    html! {
        p {
            strong { "A new session" }
            @if dir.is_none() { " " span.muted { "(run it in the right repository: no session that named it worked in one)" } }
        }
        (command_block(&command))
    }
}

fn command_block(command: &str) -> Markup {
    html! {
        div.cmd {
            pre { (command) }
            button.copy type="button" { "Copy" }
        }
    }
}
