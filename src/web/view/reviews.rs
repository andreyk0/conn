//! Review requests as inbox cards: each pull request waiting on the user, the message that
//! asked, and what reading the queue could not see.

use maud::{Markup, html};

use super::Ctx;
use super::card::post_button;
use super::fmt::{ago, clock, external_link, redact};
use crate::model::{CardVerdict, Reaction, ReviewCard, ReviewsView};
use crate::reviews::classify::FlaggedBy;
use crate::types::SafeUrl;
use crate::web::route::PostRoute;

/// The review part of the inbox: what could not be read, then a card per request.
pub(super) fn section(ctx: &Ctx<'_>) -> Markup {
    match &ctx.model.reviews {
        ReviewsView::Off | ReviewsView::NeverRead => html! {},
        ReviewsView::Failed { at, reason } => html! {
            p.problem {
                "Review requests could not be read " (ago(*at, ctx.now)) " (" (clock(ctx.zone, *at)) "): " (reason)
            }
        },
        ReviewsView::Listed {
            cards,
            problems,
            failure,
            read_at,
        } => html! {
            @if let Some((at, reason)) = failure {
                p.problem {
                    "Review requests could not be read again " (ago(*at, ctx.now)) ": " (reason)
                    "; the cards below are from " (ago(*read_at, ctx.now)) "."
                }
            }
            @for problem in problems {
                p.problem { "Review requests: " (problem) }
            }
            @for card in cards { (review_card(ctx, card)) }
        },
    }
}

/// What the classifier said: a highlighted list of reasons to look, or a quiet line.
fn verdict(card: &ReviewCard) -> Markup {
    match &card.verdict {
        CardVerdict::Pending => html! { p.muted.verdict { "Not classified yet." } },
        CardVerdict::Routine => html! { p.muted.verdict { "Routine: nothing flagged." } },
        CardVerdict::Unclassified(reason) => html! {
            p.verdict.unclassified { "Not classified: " (reason) }
        },
        CardVerdict::Look(flags) => html! {
            div.verdict.look {
                strong { "Look here" }
                @if let Some(url) = &card.pr_url {
                    " · " (external_link(&files_url(url), &html! { "files changed" }))
                }
                ul {
                    @for flag in flags {
                        li {
                            span.flag-by { @match flag.by { FlaggedBy::Rule => "rule", FlaggedBy::Model => "model" } }
                            " "
                            @if let Some(file) = &flag.file { code { (file) } ": " }
                            (flag.why)
                        }
                    }
                }
            }
        },
    }
}

/// The pull request's "Files changed" tab.
fn files_url(pr: &SafeUrl) -> SafeUrl {
    SafeUrl::parse(&format!("{}/files", pr.as_str())).unwrap_or_else(|_| pr.clone())
}

fn review_card(ctx: &Ctx<'_>, card: &ReviewCard) -> Markup {
    let pr_text = redact(&card.pr.to_string());
    html! {
        article.card.review {
            div.card-head {
                span.badge.review { "Review" }
                h2 {
                    @if let Some(url) = &card.pr_url {
                        (external_link(url, &html! { (pr_text) }))
                    } @else {
                        (pr_text)
                    }
                }
                span.muted { "by " (card.author) }
                span.muted {
                    "+" (card.additions) " −" (card.deletions) " · " (card.files)
                    @if card.files == 1 { " file" } @else { " files" }
                    " · " (card.head.short())
                }
            }
            p.review-title { (card.title) }
            (verdict(card))
            div.body { (ctx.markdown(&card.request)) }
            p.muted {
                (card.requester) " asked"
                @if let Some(at) = card.requested_at { " " (ago(at, ctx.now)) }
                @if let Some(url) = &card.message_url {
                    " in " (external_link(url, &html! { "Slack" }))
                }
            }
            div.actions {
                (post_button(
                    PostRoute::ApproveReview,
                    &[("pr", &card.pr.to_string()), ("head", card.head.as_str())],
                    "Approve",
                    "primary",
                ))
            }
        }
    }
}

/// The approvals made from review cards, newest first, with how each 👍 went.
pub(super) fn approvals_page(ctx: &Ctx<'_>) -> Markup {
    let Some(rows) = &ctx.model.approvals else {
        return html! {
            p.empty { "No [reviews] section is configured, so there are no review cards to approve." }
        };
    };
    html! {
        @if rows.is_empty() {
            p.empty { "Nothing approved from a review card yet." }
        } @else {
            table.items {
                tr { th { "When" } th { "Pull request" } th { "Asked by" } th { "Verdict" } th { "👍" } }
                @for row in rows {
                    tr {
                        td { (clock(ctx.zone, row.at)) }
                        td {
                            @if let Some(url) = &row.pr_url {
                                (external_link(url, &html! { (redact(&row.pr)) }))
                            } @else {
                                (redact(&row.pr))
                            }
                            " " span.muted { (row.title) " · by " (row.author) }
                        }
                        td {
                            @if let Some(url) = &row.message_url {
                                (external_link(url, &html! { (row.requester) }))
                            } @else {
                                (row.requester)
                            }
                        }
                        td {
                            (row.verdict)
                            @for flag in &row.flags { br; span.muted { (flag) } }
                        }
                        td {
                            @match &row.reaction {
                                Reaction::Pending => span.muted { "adding…" },
                                Reaction::Added { at } => span { "added " (ago(*at, ctx.now)) },
                                Reaction::Failed { reason, .. } => {
                                    span.problem { "failed: " (reason) }
                                    " "
                                    (post_button(PostRoute::RetryReaction, &[("pr", &row.pr), ("head", &row.head)], "Retry", ""))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
