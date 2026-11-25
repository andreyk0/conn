//! The comment badge a ticket wears wherever a page shows it, and the marks a card's "seen"
//! makes on the tickets its session owns.
//!
//! The badge counts the comments by others since the later of the user's own latest comment
//! and the ticket's "seen" mark ([`crate::jira::comments::unseen`]). Only a listed work item
//! has its comments read, so any other ticket wears none.

use maud::{Markup, html};

use super::Ctx;
use crate::jira::comments::Unseen;
use crate::types::SessionId;

/// The badge of the ticket Jira writes as `key`: the count of comments the user has not seen,
/// a question mark when Jira returned only part of the thread, and nothing when they have seen
/// them all or they are not read yet.
pub(super) fn badge(ctx: &Ctx<'_>, key: &str) -> Markup {
    let Some((label, title)) = label(ctx, key) else {
        return html! {};
    };
    html! { " " span.tag.comments title=(title) { (label) } }
}

/// The badge's text and its explanation, where the ticket wears one; the map's drawing, which
/// holds no markup, writes the text alone.
pub(super) fn label(ctx: &Ctx<'_>, key: &str) -> Option<(String, String)> {
    match ctx.model.work_items.row(key)?.comments? {
        Unseen::New { count, .. } => {
            let plural = if count.get() > 1 { "s" } else { "" };
            Some((
                format!("💬 {count}"),
                format!("{count} comment{plural} you have not seen"),
            ))
        }
        Unseen::Unknown => Some((
            "💬 ?".to_owned(),
            "Jira returned only part of this ticket's comments, so the latest are not known"
                .to_owned(),
        )),
        Unseen::Caught => None,
    }
}

/// The unseen comments on the tickets `session` owns, as the card's "seen" posts them:
/// `KEY=instant` pairs, each the ticket and its newest unseen comment's time. Posting the time
/// the page showed, not the time of the click, leaves a comment that arrived since unseen.
pub(super) fn seen_marks(ctx: &Ctx<'_>, session: &SessionId) -> String {
    ctx.model
        .coordination
        .ledger
        .claims_of(session)
        .into_iter()
        .filter_map(|subject| subject.ticket())
        .filter_map(|key| {
            let key = key.to_string();
            match ctx.model.work_items.row(&key)?.comments? {
                Unseen::New { newest, .. } => Some(format!("{key}={newest}")),
                Unseen::Caught | Unseen::Unknown => None,
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}
