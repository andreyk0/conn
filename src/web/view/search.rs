//! The search page: a form that keeps the query, what is searched, the count, and each
//! session found with the text that matched.
//!
//! The search itself is [`crate::model::search`]; this only draws its results.

use maud::{Markup, html};

use super::Ctx;
use super::card::{cwd_text, session_name};
use super::fmt::{ago, badge, redact, since_of};
use super::reach::reach;
use crate::model::{
    Found, Hit, LAST_TURN_ENDS, Matched, QUERY_CHARS_MAX, Query, Results, Snippet, search,
};
use crate::transcript::HUMAN_PROMPTS_KEPT;
use crate::web::route::Page;

pub(super) fn page(ctx: &Ctx<'_>, query: Option<&Query>) -> Markup {
    html! {
        form method="get" action=(Page::Search(None).to_string()) {
            input type="search" name="q" value=(redact(query.map_or("", Query::as_str)))
                maxlength=(QUERY_CHARS_MAX) size="50" placeholder="title, key, #pull request, text";
            " "
            button type="submit" { "Search" }
        }
        p.muted {
            "Searches the titles, working directories and links of every session, and its last "
            (HUMAN_PROMPTS_KEPT) " prompts and last " (LAST_TURN_ENDS) " replies; not whole transcripts."
        }
        @if let Some(query) = query {
            (results(ctx, &search(ctx.model, query)))
        }
    }
}

fn results(ctx: &Ctx<'_>, results: &Results) -> Markup {
    html! {
        p {
            strong { (results.matched()) @if results.matched() == 1 { " session matches" } @else { " sessions match" } }
            @if results.truncated() {
                " " span.problem { "Showing the best " (results.found().len()) " of them; narrow the query to see the rest." }
            }
        }
        @for found in results.found() { (result(ctx, found)) }
    }
}

fn result(ctx: &Ctx<'_>, found: &Found) -> Markup {
    let id = found.session();
    let Some(view) = ctx.model.session(id) else {
        return html! { div.row { "Session " (id.as_str()) " is not in the current model." } };
    };
    let activity = &view.session.activity;
    html! {
        article.card id=(id.as_str()) {
            div.card-head {
                h2 { (session_name(ctx, view.title.as_ref(), id).link(id)) }
                (badge(activity, since_of(activity, view.pending.as_ref()), ctx.now))
                @if let Some(at) = view.last_activity { span.muted { "last activity " (ago(at, ctx.now)) } }
            }
            p.cwd { (cwd_text(&view.session)) }
            (super::owners::session_line(ctx, id))
            ul.hits {
                @for hit in found.hits() { li { (hit_line(hit)) } }
            }
            (reach(view))
        }
    }
}

fn hit_line(hit: &Hit) -> Markup {
    let label = match &hit.field {
        Matched::Title => "title",
        Matched::Cwd => "directory",
        Matched::Link(_) => "link",
        Matched::Prompt => "prompt",
        Matched::Reply => "reply",
    };
    html! { span.tag { (label) } " " (snippet(&hit.snippet)) }
}

fn snippet(snippet: &Snippet) -> Markup {
    html! {
        span.snippet {
            @if snippet.cut_before { "…" }
            (snippet.before) mark { (snippet.matched) } (snippet.after)
            @if snippet.cut_after { "…" }
        }
    }
}
