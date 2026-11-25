//! The pages, as functions from the read model to HTML.
//!
//! Every page is a pure function of a [`Ctx`]: the model, the time it is viewed at, and what
//! is needed to write a link. None reads a file or asks a process; what the human sees is the
//! model, and the model's own account of what it could not read. A local file's page is
//! handed the file's text by its handler.

mod card;
mod comments;
mod fmt;
mod followups;
mod map;
mod markdown;
mod owners;
mod pages;
mod pam;
mod reach;
mod reply;
mod reviews;
mod saves;
mod search;
mod usage;
mod work_items;

use std::cell::Cell;

use jiff::tz::TimeZone;
use maud::{DOCTYPE, Markup, html};

use super::auth::{Token, Write};
use super::route::{Asset, Page};
use crate::links::Extractor;
use crate::model::{Model, Notifications, ScanProgress, Seen, SourceState};
use crate::types::secs_since;
use crate::types::{Instant, LinkTargets, LocalFile, SessionId};
use crate::util::redact::Redacted;
use fmt::ago;

/// What a page is rendered from.
pub(super) struct Ctx<'a> {
    pub model: &'a Model,
    /// When the page is viewed, for every "n min ago".
    pub now: Instant,
    pub zone: &'a TimeZone,
    pub extractor: &'a Extractor,
    pub targets: &'a LinkTargets,
    /// The GitHub owner a pull request chip leaves out of its name.
    pub default_owner: Option<&'a str>,
    /// The token the page's script sends with a change.
    pub write_token: &'a Token<Write>,
    /// The links the markdown drawn since the innermost [`Ctx::counting_withheld`] began left
    /// out.
    withheld: Cell<u32>,
}

impl<'a> Ctx<'a> {
    pub(super) const fn new(
        model: &'a Model,
        now: Instant,
        zone: &'a TimeZone,
        extractor: &'a Extractor,
        targets: &'a LinkTargets,
        default_owner: Option<&'a str>,
        write_token: &'a Token<Write>,
    ) -> Self {
        Self {
            model,
            now,
            zone,
            extractor,
            targets,
            default_owner,
            write_token,
            withheld: Cell::new(0),
        }
    }

    /// Third-party markdown as markup; the links it had to leave out are added to the count of
    /// the enclosing [`Ctx::counting_withheld`].
    pub(super) fn markdown(&self, source: &Redacted<String>) -> Markup {
        let rendered = markdown::render(self.extractor, self.targets, source);
        self.withheld
            .set(self.withheld.get().saturating_add(rendered.withheld));
        rendered.markup
    }

    /// What `draw` draws, and how many links the markdown inside it left out: the count a card
    /// reports in its footnote. Nested calls each count their own.
    pub(super) fn counting_withheld(&self, draw: impl FnOnce() -> Markup) -> (Markup, u32) {
        let outer = self.withheld.replace(0);
        let drawn = draw();
        (drawn, self.withheld.replace(outer))
    }
}

/// The footnote under a body whose text had `withheld` anchors left out; nothing when none.
pub(super) fn link_note(withheld: u32) -> Markup {
    html! {
        @if withheld > 0 {
            p.muted {
                (withheld) @if withheld == 1 { " link" } @else { " links" } " not clickable: not https or local http"
            }
        }
    }
}

/// What a header or an empty list says about a hook that has never run, and the health page
/// repeats: the event log is written by the hook alone.
const NO_HOOK_YET: &str = "no hook has run yet (register it with conn hook --print-settings)";

/// What a page was asked for and the model does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum NotFound {
    #[error("no session {0} in the model")]
    Session(SessionId),
    #[error("there is no save {0}")]
    Save(crate::saves::SaveId),
}

/// A model older than this is shown as stale: the poller rebuilds it every two seconds, so
/// this is several missed rebuilds, not a slow one.
pub(super) const STALE_AFTER_SECS: u64 = 15;

/// The page for `page`.
pub(super) fn render(ctx: &Ctx<'_>, page: &Page) -> Result<Markup, NotFound> {
    let (nav, title, body) = match page {
        Page::Inbox => (Nav::Inbox, "Inbox", pages::inbox(ctx)),
        Page::FollowUps => (Nav::FollowUps, "Follow-ups", followups::page(ctx)),
        Page::Sessions => (Nav::Sessions, "Sessions", pages::sessions(ctx)),
        Page::Dormant => (Nav::Dormant, "Dormant", pages::dormant(ctx)),
        Page::Map => (Nav::Map, "Map", map::page(ctx)),
        Page::Owners => (Nav::Owners, "Who owns what", owners::page(ctx)),
        Page::WorkItems => (Nav::WorkItems, "Work items", work_items::page(ctx)),
        Page::Approvals => (Nav::Approvals, "Approvals", reviews::approvals_page(ctx)),
        Page::Pam => (Nav::Pam, "PAM", pam::page(ctx)),
        Page::Search(query) => (Nav::Search, "Search", search::page(ctx, query.as_ref())),
        Page::Health => (Nav::Health, "Health", pages::health(ctx)),
        Page::Session(id) => (Nav::Sessions, "Session", pages::session(ctx, id)?),
        Page::Save(id) => (Nav::Sessions, "Saved sessions", saves::page(ctx, *id)?),
    };
    Ok(layout(ctx, nav, title, &body))
}

/// The header's navigation entries; a detail page lights the list it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Inbox,
    FollowUps,
    Owners,
    Sessions,
    Dormant,
    Map,
    WorkItems,
    Approvals,
    Pam,
    Search,
    Health,
}

impl Nav {
    const ALL: [Self; 11] = [
        Self::Inbox,
        Self::FollowUps,
        Self::Owners,
        Self::Sessions,
        Self::Dormant,
        Self::Map,
        Self::WorkItems,
        Self::Approvals,
        Self::Pam,
        Self::Search,
        Self::Health,
    ];

    /// Whether the header lists it: PAM only where it is configured.
    const fn shown(self, model: &Model) -> bool {
        match self {
            Self::Pam => model.pam.is_some(),
            Self::Inbox
            | Self::FollowUps
            | Self::Owners
            | Self::Sessions
            | Self::Dormant
            | Self::Map
            | Self::WorkItems
            | Self::Approvals
            | Self::Search
            | Self::Health => true,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::FollowUps => "Follow-ups",
            Self::Owners => "Owners",
            Self::Sessions => "Sessions",
            Self::Dormant => "Dormant",
            Self::Map => "Map",
            Self::WorkItems => "Work items",
            Self::Approvals => "Approvals",
            Self::Pam => "PAM",
            Self::Search => "Search",
            Self::Health => "Health",
        }
    }

    const fn page(self) -> Page {
        match self {
            Self::Inbox => Page::Inbox,
            Self::FollowUps => Page::FollowUps,
            Self::Owners => Page::Owners,
            Self::Sessions => Page::Sessions,
            Self::Dormant => Page::Dormant,
            Self::Map => Page::Map,
            Self::WorkItems => Page::WorkItems,
            Self::Approvals => Page::Approvals,
            Self::Pam => Page::Pam,
            Self::Search => Page::Search(None),
            Self::Health => Page::Health,
        }
    }
}

fn layout(ctx: &Ctx<'_>, current: Nav, title: &str, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) " · conn" }
                link rel="stylesheet" href=(Asset::Style.path());
                script src=(Asset::Script.path()) defer {}
            }
            body data-write-token=(ctx.write_token.as_str()) data-reload {
                (header(ctx, current))
                main {
                    div #flash.flash role="status" hidden {}
                    h1 { (title) }
                    (body)
                }
                footer {
                    p {
                        "Redaction is best effort: text shown here has known secret patterns "
                        "masked, and a secret in any other shape is shown as written."
                    }
                    p { "conn reads and shows. It never writes to a session." }
                }
                (file_dialog())
            }
        }
    }
}

/// A local markdown file's page: the file rendered under its name and path. It does not
/// reload itself, and the pop-up the script opens shows its `article`.
pub(super) fn file(ctx: &Ctx<'_>, file: &LocalFile, text: &str) -> Markup {
    let name = fmt::redact(&file.file_name());
    let (body, withheld) = ctx.counting_withheld(|| ctx.markdown(&fmt::redact(text)));
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (name) " · conn" }
                link rel="stylesheet" href=(Asset::Style.path());
                script src=(Asset::Script.path()) defer {}
            }
            body {
                main {
                    article.file-view {
                        header {
                            h1 { (name) }
                            p.muted { code { (fmt::redact(&file.to_string())) } }
                        }
                        div.body { (body) }
                        (link_note(withheld))
                    }
                }
                (file_dialog())
            }
        }
    }
}

/// The pop-up a file link opens: the script fills in the name, the link to the file's own
/// page and the body.
fn file_dialog() -> Markup {
    html! {
        dialog #file-dialog.file-dialog aria-label="Local file" {
            div.file-dialog-bar {
                strong.file-dialog-name {}
                a.file-dialog-open href="" target="_blank" rel="noopener" { "Open in new window" }
                button.file-dialog-close type="button" { "Close" }
            }
            div.file-dialog-body {}
        }
    }
}

fn header(ctx: &Ctx<'_>, current: Nav) -> Markup {
    let model = ctx.model;
    let age = secs_since(model.generated_at, ctx.now);
    let problems = problems(model);
    let unseen = model
        .inbox
        .iter()
        .filter(|entry| entry.seen == Seen::Unseen)
        .count()
        + model.reviews.cards().len();
    let due = match &model.followups {
        crate::model::followups::FollowUpsView::On { due, .. } => due.len(),
        crate::model::followups::FollowUpsView::Off => 0,
    };
    html! {
        div.bar {
            div.bar-inner {
                span.brand { "conn" }
                nav {
                    @for entry in Nav::ALL.into_iter().filter(|entry| entry.shown(model)) {
                        a href=(entry.page().to_string()) class=[(entry == current).then_some("active")] {
                            (entry.label())
                            @if entry == Nav::Inbox && unseen > 0 { " (" (unseen) ")" }
                            @if entry == Nav::FollowUps && due > 0 { " (" (due) ")" }
                        }
                    }
                }
                div.status {
                    // A dot for how fresh the cards are: green within STALE_AFTER_SECS, yellow
                    // past it, red while "hold" is ticked. The script keeps it current from
                    // `data-generated-at`, so a page that has not reloaded shows its real age;
                    // the age and any reason the page is not reloading are its title.
                    @let age_text = format!("as of {}", ago(model.generated_at, ctx.now));
                    span #freshness
                        class={ "dot " @if age > STALE_AFTER_SECS { "stale" } @else { "fresh" } }
                        data-generated-at=(model.generated_at.as_millisecond())
                        data-fresh-secs=(STALE_AFTER_SECS)
                        role="img"
                        title=(age_text)
                        aria-label=(age_text) {}
                    @if let ScanProgress::InProgress { scanned, total } = model.health.transcripts.progress {
                        span.badge.scan { "still scanning " (scanned) " of " (total) }
                    }
                    (usage::gauges(ctx))
                    label { input #hold type="checkbox"; " hold" }
                }
                @if !problems.is_empty() {
                    div.healthline {
                        "Not everything was read: " (problem_list(&problems)) ". "
                        a href=(Page::Health.to_string()) { "Details on the health page." }
                    }
                }
            }
        }
    }
}

/// How a source that is not fine reads in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    Failed,
    NotReadYet,
    NeverWritten,
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Failed => "failed",
            Self::NotReadYet => "not read yet",
            Self::NeverWritten => "never written",
        })
    }
}

fn trouble(state: &SourceState) -> Option<Trouble> {
    match state {
        SourceState::Ok { .. } => None,
        SourceState::Failed { .. } => Some(Trouble::Failed),
        SourceState::NeverRead => Some(Trouble::NotReadYet),
        SourceState::NeverWritten { .. } => Some(Trouble::NeverWritten),
    }
}

/// Something that stops a page from saying calm: a source not read, a transcript tail that
/// could not be followed, a session that could not be classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    Source {
        name: &'static str,
        trouble: Trouble,
    },
    TailFaults {
        transcripts: usize,
    },
    /// The first read of the transcripts is not finished.
    Scanning {
        scanned: usize,
        total: usize,
    },
    Unclassified {
        sessions: u32,
    },
    /// The event log does not exist: the hook has not written to it.
    NoHookYet,
}

impl Problem {
    fn markup(self) -> Markup {
        match self {
            Self::Source { name, trouble } => html! { (name) " (" (trouble) ")" },
            Self::TailFaults { transcripts } => {
                html! { "transcript tails (" (transcripts) " could not be read)" }
            }
            Self::Scanning { scanned, total } => {
                html! { "transcript scan (" (scanned) " of " (total) " read)" }
            }
            Self::Unclassified { sessions } => html! {
                a href=(Page::Sessions.to_string()) {
                    (sessions) @if sessions == 1 { " session" } @else { " sessions" }
                    " could not be classified"
                }
            },
            Self::NoHookYet => html! { "event log: " (NO_HOOK_YET) },
        }
    }
}

fn problem_list(problems: &[Problem]) -> Markup {
    html! {
        @for (at, problem) in problems.iter().enumerate() {
            @if at > 0 { ", " }
            (problem.markup())
        }
    }
}

/// What the model could not read or tell, in the order the header lists it.
fn problems(model: &Model) -> Vec<Problem> {
    let health = &model.health;
    let source =
        |name, state: &SourceState| trouble(state).map(|trouble| Problem::Source { name, trouble });
    let event_log = match &health.event_log {
        SourceState::NeverWritten { .. } => Some(Problem::NoHookYet),
        state @ (SourceState::Ok { .. } | SourceState::Failed { .. } | SourceState::NeverRead) => {
            source("event log", state)
        }
    };
    let mut found: Vec<Problem> = [
        source("listing", &health.listing),
        source("registry", &health.registry),
        event_log,
        source("transcript discovery", &health.transcripts.discovery),
        source("seen.json", &health.seen),
    ]
    .into_iter()
    .flatten()
    .collect();
    let failed = |name| Problem::Source {
        name,
        trouble: Trouble::Failed,
    };
    if health.transcripts.unreadable > 0 {
        found.push(failed("transcripts"));
    }
    if health.follow_faults.transcripts > 0 {
        found.push(Problem::TailFaults {
            transcripts: health.follow_faults.transcripts,
        });
    }
    if let ScanProgress::InProgress { scanned, total } = health.transcripts.progress {
        found.push(Problem::Scanning { scanned, total });
    }
    if health.gh.failed > 0 {
        found.push(failed("gh"));
    }
    if matches!(
        health.notifications,
        Notifications::Missing { .. } | Notifications::Failed { .. }
    ) {
        found.push(failed("notifications"));
    }
    if model.unknown_sessions > 0 {
        found.push(Problem::Unclassified {
            sessions: model.unknown_sessions,
        });
    }
    found
}

/// Whether every source was read, every session classified and the scan is complete: the only
/// state in which an empty list means nothing is there.
fn everything_read(model: &Model) -> bool {
    problems(model).is_empty()
}
