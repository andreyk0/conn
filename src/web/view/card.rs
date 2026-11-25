//! The pieces pages are built from: an inbox card, a session row, a link chip, and the forms
//! that ask for a change. The way to a session (its terminal or its resume command) is
//! [`super::reach`]'s.

use jiff::tz::TimeZone;
use maud::{Markup, html};

use super::fmt::{
    activity_detail, ago, badge, collapsible, date, dimmed, external_link, first_line, local_link,
    pr_age, pr_badge, pr_glyphs, pr_reading_text, questions, redact, since_of, trigger_phrase,
    waiting_for,
};
use super::reach::{Reach, reach};
use super::reply::{Answers, Delivery, answerable};
use super::{Ctx, link_note};
use crate::gh::{Answer, Reading};
use crate::gist::{Ask, GistView};
use crate::model::{Band, InboxEntry, LinkView, Pin, ReleaseRun, SessionView, Waiting};
use crate::types::{Instant, JiraKey, Link, PendingCard, SafeUrl, Session, SessionId};
use crate::util::redact::Redacted;
use crate::web::route::{Page, PostRoute};

/// The longest a one-line summary of a turn end gets.
const FIRST_LINE_CHARS_MAX: usize = 160;
/// The longest the path of a document link's chip gets before it is cut.
const DOC_PATH_CHARS_MAX: usize = 40;
/// How much of an ordinary URL's path its chip shows.
const OTHER_PATH_CHARS_MAX: usize = 32;

/// The most characters of a session's first prompt that stand in for its missing title.
const PROMPT_TITLE_CHARS_MAX: usize = 60;
/// How many characters of a session's id name it where nothing better does.
const SHORT_ID_CHARS: usize = 8;

/// The name a session goes by on a page.
pub(super) enum SessionName {
    /// The title the person or Claude Code gave it.
    Titled(Redacted<String>),
    /// No title: the start of the first prompt that reads as words (a slash command reads as `/name args`), then the short id to tell
    /// sessions that began alike apart.
    Prompted {
        prompt: Redacted<String>,
        short_id: Redacted<String>,
    },
    /// No title and no prompt held: the start of its id.
    IdOnly(Redacted<String>),
}

/// How `id` is named: its title, else the start of its first held prompt that reads as words,
/// else its id.
pub(super) fn session_name(
    ctx: &Ctx<'_>,
    title: Option<&Redacted<String>>,
    id: &SessionId,
) -> SessionName {
    let short_id = redact(&id.as_str().chars().take(SHORT_ID_CHARS).collect::<String>());
    if let Some(title) = title.filter(|title| !title.as_str().trim().is_empty()) {
        return SessionName::Titled(title.clone());
    }
    let prompt = ctx
        .model
        .prompts
        .get(id)
        .and_then(|prompts| prompt_title(prompts.iter().map(|held| &held.text)));
    match prompt {
        Some(prompt) => SessionName::Prompted { prompt, short_id },
        None => SessionName::IdOnly(short_id),
    }
}

/// The wrapper Claude Code records around a typed slash command: the command's name.
const COMMAND_MESSAGE_TAG: &str = "command-message";
/// The wrapper around the arguments typed after a slash command.
const COMMAND_ARGS_TAG: &str = "command-args";

/// The text between `<tag>` and `</tag>`, trimmed, where `text` holds both; `None` when either
/// is missing.
fn tag_content<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let (_, after_open) = text.split_once(&format!("<{tag}>"))?;
    let (content, _) = after_open.split_once(&format!("</{tag}>"))?;
    Some(content.trim())
}

/// What a prompt reads as in a title: a slash command's `/name args`, else its first line. `None`
/// when the prompt is not words a person typed: blank, or opening with a tag that is not a command
/// wrapper (a task notification, a local command's output), or with a command wrapper that names
/// no command.
fn prompt_reading(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if !line.starts_with('<') {
        return Some(line.to_owned());
    }
    if !line.starts_with(&format!("<{COMMAND_MESSAGE_TAG}>")) {
        return None;
    }
    let name = tag_content(line, COMMAND_MESSAGE_TAG).filter(|name| !name.is_empty())?;
    Some(
        match tag_content(text, COMMAND_ARGS_TAG).filter(|args| !args.is_empty()) {
            Some(args) => format!("/{name} {args}"),
            None => format!("/{name}"),
        },
    )
}

/// The title the first of `prompts` that reads as words gives, cut to [`PROMPT_TITLE_CHARS_MAX`].
fn prompt_title<'a>(
    prompts: impl IntoIterator<Item = &'a Redacted<String>>,
) -> Option<Redacted<String>> {
    prompts
        .into_iter()
        .find_map(|prompt| prompt_reading(prompt.as_str()))
        .map(|reading| first_line(&redact(&reading), PROMPT_TITLE_CHARS_MAX))
}

impl SessionName {
    /// The words that name the session: what a link says and a filter matches.
    pub(super) const fn label(&self) -> &Redacted<String> {
        match self {
            Self::Titled(title) => title,
            Self::Prompted { prompt, .. } => prompt,
            Self::IdOnly(short_id) => short_id,
        }
    }

    /// The short id that follows a name taken from a prompt.
    pub(super) const fn short_id_after(&self) -> Option<&Redacted<String>> {
        match self {
            Self::Prompted { short_id, .. } => Some(short_id),
            Self::Titled(_) | Self::IdOnly(_) => None,
        }
    }

    /// Every word the name shows, for a filter to match.
    pub(super) fn words(&self) -> String {
        match self.short_id_after() {
            Some(short_id) => format!("{} {}", self.label().as_str(), short_id.as_str()),
            None => self.label().as_str().to_owned(),
        }
    }

    /// The name as plain text, then the muted short id when it follows.
    pub(super) fn text(&self) -> Markup {
        html! {
            (self.label())
            @if let Some(short_id) = self.short_id_after() { " " span.muted { (short_id) } }
        }
    }

    /// The name as a link to the session's page, then the muted short id when it follows.
    pub(super) fn link(&self, id: &SessionId) -> Markup {
        html! {
            (local_link(&Page::Session(id.clone()), self.label()))
            @if let Some(short_id) = self.short_id_after() { " " span.muted { (short_id) } }
        }
    }
}

/// The working directory as text; empty when the session has none.
pub(super) fn cwd_text(session: &Session) -> Redacted<String> {
    redact(
        &session
            .cwd
            .as_ref()
            .map_or_else(String::new, |cwd| cwd.to_string_lossy().into_owned()),
    )
}

// --- forms ---

/// A button that POSTs `fields` to `route` through the page's script.
pub(super) fn post_button(
    route: PostRoute,
    fields: &[(&str, &str)],
    label: &str,
    class: &str,
) -> Markup {
    post_form(
        route,
        fields,
        &html! { button type="submit" class=(class) { (label) } },
    )
}

/// The form that POSTs `fields` to `route` through the page's script, submitted by `button`,
/// which may hold the form's own controls before the button.
pub(super) fn post_form(route: PostRoute, fields: &[(&str, &str)], button: &Markup) -> Markup {
    html! {
        form.act method="post" action=(route.path()) data-kind=(kind_of(route)) {
            @for (name, value) in fields {
                input type="hidden" name=(name) value=(value);
            }
            (button)
        }
    }
}

const fn kind_of(route: PostRoute) -> &'static str {
    match route {
        PostRoute::Focus => "focus",
        PostRoute::Seen
        | PostRoute::Pin
        | PostRoute::Unpin
        | PostRoute::RefreshLink
        | PostRoute::RefreshWorkItems
        | PostRoute::ApproveReview
        | PostRoute::RetryReaction
        | PostRoute::SaveSessions
        | PostRoute::DeleteSave
        | PostRoute::RequestGrant
        | PostRoute::RefreshPam => "change",
    }
}

// --- links ---

/// What kind of artefact a chip stands for. The declaration order is the order chips sort in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChipKind {
    PullRequest,
    Jira,
    Slack,
    Doc,
    Other,
}

impl ChipKind {
    const fn of(link: &Link) -> Self {
        match link {
            Link::PullRequest { .. } => Self::PullRequest,
            Link::Jira { .. } => Self::Jira,
            Link::SlackThread { .. } => Self::Slack,
            Link::Doc { .. } => Self::Doc,
            Link::Other { .. } => Self::Other,
        }
    }

    const fn class(self) -> &'static str {
        match self {
            Self::PullRequest => "pr",
            Self::Jira => "jira",
            Self::Slack => "slack",
            Self::Doc => "doc",
            Self::Other => "link",
        }
    }
}

/// Whose a chip's link is, seen from the session whose card or page it sits on, so a question
/// about it goes to the session that owns it. The ledger claims Jira tickets only, so a Jira
/// chip is its ticket's owner's, and a pull request is the owner's of the first ticket its
/// title names that someone claimed; anything else has no owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership<'a> {
    /// The session the chip is shown for claimed it.
    Viewer,
    /// Another session claimed it.
    Other(&'a SessionId),
    /// No session claims it, or it is no kind of link a session can claim.
    Unclaimed,
}

impl<'a> Ownership<'a> {
    fn of(ctx: &Ctx<'a>, viewer: &SessionId, view: &LinkView) -> Self {
        let ledger = &ctx.model.coordination.ledger;
        let owner = |key: JiraKey| ledger.owner(&crate::ledger::Subject::Ticket(key));
        let held = match (&view.link, pr_title(view)) {
            (Link::Jira { key }, _) => owner(key.clone()),
            (Link::PullRequest { .. }, Some(title)) => title_keys(title).find_map(owner),
            (
                Link::PullRequest { .. }
                | Link::SlackThread { .. }
                | Link::Doc { .. }
                | Link::Other { .. },
                _,
            ) => None,
        };
        match held {
            Some(held) if held.session == *viewer => Self::Viewer,
            Some(held) => Self::Other(&held.session),
            None => Self::Unclaimed,
        }
    }

    const fn class(self) -> &'static str {
        match self {
            Self::Viewer => " mine",
            Self::Other(_) => " theirs",
            Self::Unclaimed => "",
        }
    }

    /// What goes before the link: a mark on what the viewer owns.
    fn before(self) -> Markup {
        html! {
            @match self {
                Self::Viewer => { span.own-mark title="This session owns it" { "●" } " " }
                Self::Other(_) | Self::Unclaimed => {}
            }
        }
    }

    /// What goes after the link: the way to the session that owns it instead.
    fn after(self, ctx: &Ctx<'_>) -> Markup {
        html! {
            @match self {
                Self::Other(owner) => { " " span.owner title="Owned by another session" { "→ " (super::owners::who(ctx, owner)) } }
                Self::Viewer | Self::Unclaimed => {}
            }
        }
    }
}

/// A pull request's title, when a reading of it found one.
fn pr_title(view: &LinkView) -> Option<&str> {
    match &view.pr {
        Some(Reading::Value { observed, .. }) => match &observed.answer {
            Answer::Found(state) => Some(state.title.as_str()),
            Answer::NotFound => None,
        },
        Some(Reading::Unread | Reading::Failure(_)) | None => None,
    }
}

/// The ticket keys a title names, in order: its words that read as `ABC-123`.
fn title_keys(title: &str) -> impl Iterator<Item = JiraKey> {
    title
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter_map(JiraKey::parse)
}

/// What a chip says its link is, derived from the link alone and the default owner: the Jira
/// key, `owner/repo#n` (`repo#n` when the owner is the default one), a Slack thread by its
/// channel id and the date of its timestamp (the permalink holds nothing more), a document by
/// its host and the start of its path, any other URL by [`other_label`].
fn chip_label(zone: &TimeZone, default_owner: Option<&str>, link: &Link) -> Redacted<String> {
    redact(&match link {
        Link::Jira { .. } => link.to_string(),
        Link::PullRequest {
            owner,
            repo,
            number,
        } => pull_request_label(default_owner, owner, repo, *number),
        Link::SlackThread { channel, ts } => match slack_date(zone, ts) {
            Some(day) => format!("Slack thread {channel} · {day}"),
            None => format!("Slack thread {channel}"),
        },
        Link::Doc { url } => {
            let (host, path) = host_and_path(url);
            format!("{host}{}", clipped(path, DOC_PATH_CHARS_MAX))
        }
        Link::Other { url } => other_label(url),
    })
}

/// `owner/repo#number`, without the owner when it is the default one (GitHub names are
/// case-insensitive).
fn pull_request_label(default_owner: Option<&str>, owner: &str, repo: &str, number: u32) -> String {
    if default_owner.is_some_and(|default| default.eq_ignore_ascii_case(owner)) {
        format!("{repo}#{number}")
    } else {
        format!("{owner}/{repo}#{number}")
    }
}

/// The first `max` characters of `path`, and an ellipsis when there were more.
fn clipped(path: &str, max: usize) -> String {
    let shown: String = path.chars().take(max).collect();
    if path.chars().count() > max {
        format!("{shown}…")
    } else {
        shown
    }
}

/// What a chip for a URL that is no known kind of link says, so chips to one host differ.
///
/// A GitHub URL is read as `owner/repo` and what it points at there (a run, a commit, an
/// issue, a compare, a file or directory, else the first path segment); nothing is decoded.
/// Any other URL is its host and the start of its path.
fn other_label(url: &SafeUrl) -> String {
    let (host, path) = host_and_path(url);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match (host.eq_ignore_ascii_case("github.com"), segments.as_slice()) {
        (true, [owner, repo, rest @ ..]) => {
            let repository = format!("{owner}/{repo}");
            match rest {
                [] => repository,
                ["actions", "runs", run, ..] => format!("{repository} · actions run {run}"),
                ["commit", sha, ..] => {
                    format!(
                        "{repository} · commit {}",
                        sha.chars().take(7).collect::<String>()
                    )
                }
                ["issues", number, ..] => format!("{repository} · issue #{number}"),
                ["compare", ..] => format!("{repository} · compare"),
                ["tree" | "blob", .., last] => format!("{repository} · {last}"),
                [first, ..] => format!("{repository} · {first}"),
            }
        }
        _ if path.is_empty() => host.to_owned(),
        _ => format!("{host} · {}", clipped(path, OTHER_PATH_CHARS_MAX)),
    }
}

/// The day a Slack message was posted, in the viewer's zone, from its `1700000000.123456`
/// timestamp; `None` when the seconds are not a time.
fn slack_date(zone: &TimeZone, ts: &str) -> Option<String> {
    let seconds = ts.split_once('.').map_or(ts, |(seconds, _)| seconds);
    let at = Instant::from_second(seconds.parse().ok()?).ok()?;
    Some(date(zone, at))
}

/// The authority of a URL and its path without query or fragment; the path is empty for a bare
/// host or `/`.
fn host_and_path(url: &SafeUrl) -> (&str, &str) {
    let rest = url
        .as_str()
        .strip_prefix("https://")
        .or_else(|| url.as_str().strip_prefix("http://"))
        .unwrap_or(url.as_str());
    let (host, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    (host, if path == "/" { "" } else { path })
}

/// A link as a chip. A pull request's chip is compact: its name, state badge, checks and
/// review glyphs, its age when the reading is stale or failed, and a button that reads it
/// again; the words are its `title`. A chip says whose its link is, seen from `viewer`
/// ([`Ownership`]).
fn link_chip(ctx: &Ctx<'_>, viewer: &SessionId, view: &LinkView) -> Markup {
    let kind = ChipKind::of(&view.link);
    let ownership = Ownership::of(ctx, viewer, view);
    let label = html! { (chip_label(ctx.zone, ctx.default_owner, &view.link)) };
    let Some(reading) = &view.pr else {
        return html! {
            span class={ "chip " (kind.class()) (ownership.class()) } {
                (ownership.before())
                (external_link(&view.url, &label))
                @if let Link::Jira { key } = &view.link { (super::comments::badge(ctx, &key.to_string())) }
                (ownership.after(ctx))
            }
        };
    };
    let age = pr_age(reading, ctx.now);
    html! {
        span class={ "chip " (kind.class()) (ownership.class()) @if age.is_some() { " stale" } }
            title=(pr_reading_text(reading, ctx.now)) {
            (ownership.before())
            (external_link(&view.url, &label))
            " " (pr_badge(reading))
            (pr_glyphs(reading))
            @if let Some(age) = age { " " span.age { (age) } }
            (ownership.after(ctx))
            " " (post_form(
                PostRoute::RefreshLink,
                &[("link", view.url.as_str())],
                &html! { button.icon type="submit" title="Read again" aria-label="Refresh" { "↻" } },
            ))
        }
    }
}

/// The chips of a list of links: pull requests, Jira issues, Slack threads, documents, then
/// the rest; in order of appearance within a kind. `viewer` is the session they were mentioned
/// in, which the chips measure ownership against.
pub(super) fn link_chips(ctx: &Ctx<'_>, viewer: &SessionId, links: &[LinkView]) -> Markup {
    let mut sorted: Vec<&LinkView> = links.iter().collect();
    sorted.sort_by_key(|view| ChipKind::of(&view.link));
    html! {
        @if !sorted.is_empty() {
            div.links { @for view in sorted { (link_chip(ctx, viewer, view)) } }
        }
    }
}

// --- the inbox card ---

/// One card of the inbox.
pub(super) fn inbox_card(ctx: &Ctx<'_>, entry: &InboxEntry) -> Markup {
    let view = ctx.model.session(&entry.session);
    let name = session_name(ctx, entry.title.as_ref(), &entry.session);
    let id = entry.session.as_str();
    let activity = view.map(|v| &v.session.activity);
    let since = activity.and_then(|a| since_of(a, Some(&entry.card)));
    let card_since_text = entry.card.since().to_string();
    let ticket_marks = super::comments::seen_marks(ctx, &entry.session);
    let pin = view.map_or(Pin::Unpinned, |v| v.pin);
    let pinned = matches!(pin, Pin::Pinned(_));
    let reach = Reach::of_card(&entry.session, view);
    let (text, withheld) = ctx.counting_withheld(|| {
        html! {
            div.body { (card_body(ctx, entry, Delivery::of(&reach))) }
            @if let Some(line) = &entry.now_line {
                details.earlier {
                    summary { "earlier" }
                    (ctx.markdown(line))
                }
            }
        }
    });
    html! {
        article class={ "card" @if dimmed(entry.seen) { " seen" } @if entry.standing.band() == Band::Waiting { " waiting" } @if pinned { " pinned" } } id=(id) {
            (pin_toggle(id, pin))
            div.card-head {
                h2 { (name.link(&entry.session)) }
                @if let Some(activity) = activity {
                    (badge(activity, since, ctx.now))
                } @else {
                    span.badge.unknown { "Unknown" }
                }
                span.muted { "since " (ago(entry.card.since(), ctx.now)) }
            }
            (gist_line(ask_badge(entry), &entry.gist))
            (super::owners::session_line(ctx, &entry.session))
            (text)
            (link_chips(ctx, &entry.session, &entry.links))
            div.actions {
                (reach.button())
                (post_button(
                    PostRoute::Seen,
                    &[("session", id), ("since", &card_since_text), ("tickets", &ticket_marks)],
                    "Seen",
                    "",
                ))
            }
            (link_note(withheld))
            (reach.below())
        }
    }
}

/// The pin in the corner of a card or a session row, which pins the session or unpins it:
/// grey while it is not pinned, yellow while it is.
fn pin_toggle(session: &str, pin: Pin) -> Markup {
    let (route, label) = match pin {
        Pin::Pinned(_) => (PostRoute::Unpin, "Unpin"),
        Pin::Unpinned => (PostRoute::Pin, "Pin"),
    };
    let button = html! {
        button type="submit" title=(label) aria-label=(label)
            aria-pressed=(if matches!(pin, Pin::Pinned(_)) { "true" } else { "false" }) {
            svg viewBox="0 0 24 24" aria-hidden="true" {
                line x1="12" y1="14" x2="12" y2="22" {}
                circle cx="12" cy="9" r="6" {}
            }
        }
    };
    html! { div.pin-toggle { (post_form(route, &[("session", session)], &button)) } }
}

/// What the card waits for. The wait the session declared in the ledger says it when it is
/// what places the card, since the session said so itself; otherwise the gist's reading of
/// the card does.
fn ask_badge(entry: &InboxEntry) -> Option<Markup> {
    if let Some(waiting) = entry.standing.waiting() {
        return Some(match waiting {
            Waiting::Release {
                rollout,
                run: ReleaseRun::NotStarted,
            } => html! {
                span.badge.ask-release title="the session waits for you to run this release" {
                    "Waits on release: " (rollout.as_str())
                }
            },
            Waiting::Release {
                rollout,
                run: ReleaseRun::Running,
            } => html! { span.badge.ask-others { "Release running: " (rollout.as_str()) } },
            Waiting::Other { on } => html! {
                span.badge.ask-others { "Waiting on: " (redact(on)) }
            },
        });
    }
    entry
        .ask
        .map(|ask| html! { span class={ "badge ask-" (ask_class(ask)) } { (ask_words(ask)) } })
}

/// What the card waits for, and the session's gist: the reminder of what it is about. A gist
/// written for an earlier card is shown while this card's is written; a failure is said.
fn gist_line(ask: Option<Markup>, gist: &GistView) -> Markup {
    let (text, note) = match gist {
        GistView::Pending => (None, None),
        GistView::Current(gist)
        | GistView::Earlier {
            gist,
            failure: None,
        } => (Some(&gist.text), None),
        GistView::Earlier {
            gist,
            failure: Some(error),
        } => (Some(&gist.text), Some(format!("not updated: {error}"))),
        GistView::Failed(error) => (None, Some(format!("no gist: {error}"))),
    };
    if ask.is_none() && text.is_none() && note.is_none() {
        return html! {};
    }
    html! {
        p.gist {
            @if let Some(ask) = ask { (ask) }
            @if let Some(text) = text { span.text { (text.as_str()) } }
            @if let Some(note) = note { span.note { (redact(&note).as_str()) } }
        }
    }
}

const fn ask_words(ask: Ask) -> &'static str {
    match ask {
        Ask::Decide => "Your decision",
        Ask::Act => "Your move",
        Ask::Others => "Waiting on others",
        Ask::Nothing => "FYI",
    }
}

const fn ask_class(ask: Ask) -> &'static str {
    match ask {
        Ask::Decide => "decide",
        Ask::Act => "act",
        Ask::Others => "others",
        Ask::Nothing => "nothing",
    }
}

/// What the card says; the text of a finished turn is answerable when `delivery` says how a
/// reply reaches the session.
fn card_body(ctx: &Ctx<'_>, entry: &InboxEntry, delivery: Option<Delivery>) -> Markup {
    match &entry.card {
        PendingCard::Asked { asked, since, .. } => answerable(
            &entry.session,
            *since,
            Answers::Question,
            delivery,
            questions(ctx, asked),
        ),
        PendingCard::Blocked { on, .. } => html! {
            p { "Waiting for " (waiting_for(on)) ": open the terminal." }
        },
        PendingCard::Turn { turn, .. } => {
            let id = format!("more-{}", entry.session);
            let text = collapsible(
                &id,
                turn.text.redacted(),
                ctx.markdown(turn.text.redacted()),
            );
            html! {
                (answerable(&entry.session, turn.at, Answers::Turn, delivery, text))
                @if let Some(asked) = &turn.asked { (questions(ctx, asked)) }
                p.muted { "Turn ended " (trigger_phrase(&turn.trigger)) "." }
            }
        }
    }
}

// --- session rows ---

/// One session as a row: title, working directory, age, the last turn end's first line and
/// its resume command.
pub(super) fn session_row(ctx: &Ctx<'_>, view: &SessionView) -> Markup {
    let session = &view.session;
    let name = session_name(ctx, view.title.as_ref(), &session.id);
    let cwd = cwd_text(session);
    let mut filter = format!("{} {}", name.words(), cwd.as_str());
    if let Some(gist) = &view.gist {
        filter.push(' ');
        filter.push_str(gist.as_str());
    }
    let filter = filter.to_lowercase();
    let since = since_of(&session.activity, view.pending.as_ref());
    let last = view
        .last_turn_ends
        .first()
        .map(|end| first_line(end.text.redacted(), FIRST_LINE_CHARS_MAX))
        .filter(|line| !line.as_str().is_empty());
    let pinned = matches!(view.pin, Pin::Pinned(_));
    html! {
        div class={ "row" @if pinned { " pinned" } } data-filter=(filter) {
            (pin_toggle(session.id.as_str(), view.pin))
            div {
                strong { (name.link(&session.id)) }
                " " (badge(&session.activity, since, ctx.now))
                @if let Some(detail) = activity_detail(&session.activity) {
                    " " span.muted { (detail) }
                }
            }
            @if let Some(gist) = &view.gist { p.gist { span.text { (gist.as_str()) } } }
            (super::owners::session_line(ctx, &session.id))
            div.cwd { (cwd) }
            @if let Some(last) = last { div.first { (last) } }
            @if let Some(at) = view.last_activity {
                div.muted { "last activity " (ago(at, ctx.now)) }
            }
            (reach(view))
        }
    }
}

#[cfg(test)]
mod tests {
    //! Goal: the chip label of a URL that is no known kind of link tells chips to one host
    //! apart. Method: a hand-written table of URL and label, one row per branch of
    //! `other_label`; every expected label is written out, none is built by the code.

    use super::*;

    /// The owner is dropped when it is the default one, whatever its case, and kept otherwise
    /// or when there is no default.
    /// Mutation, each ran red on its row: compare case-sensitively; never drop; drop always.
    #[test]
    fn a_pull_request_name_leaves_out_only_the_default_owner() {
        for (default, expected) in [
            (Some("example-org"), "repo#7"),
            (Some("Example-Org"), "repo#7"),
            (Some("other-org"), "example-org/repo#7"),
            (None, "example-org/repo#7"),
        ] {
            assert_eq!(
                pull_request_label(default, "example-org", "repo", 7),
                expected,
                "{default:?}"
            );
        }
    }

    fn label(url: &str) -> String {
        other_label(&SafeUrl::parse(url).unwrap())
    }

    /// Mutation, each ran red on its row: drop `actions` from the run wording; keep 9 sha
    /// characters; drop the `#` of an issue; add words to compare; read the ref rather than
    /// the last segment of a tree; add a separator to the bare repo; replace the first-segment
    /// fallback; clip other paths at 40; drop the bare-host branch; never treat the host as GitHub.
    #[test]
    fn a_url_that_is_no_known_link_is_labelled_by_what_it_points_at() {
        for (url, expected) in [
            (
                "https://github.com/example-org/example-repo/actions/runs/123456",
                "example-org/example-repo · actions run 123456",
            ),
            (
                "https://github.com/example-org/example-repo/actions/runs/123456/job/789",
                "example-org/example-repo · actions run 123456",
            ),
            (
                "https://github.com/example-org/example-repo/commit/0123456789abcdef",
                "example-org/example-repo · commit 0123456",
            ),
            (
                "https://github.com/example-org/example-repo/issues/17",
                "example-org/example-repo · issue #17",
            ),
            (
                "https://github.com/example-org/example-repo/compare/main...feature",
                "example-org/example-repo · compare",
            ),
            (
                "https://github.com/example-org/example-repo/tree/main/src/web",
                "example-org/example-repo · web",
            ),
            (
                "https://github.com/example-org/example-repo/blob/main/src/lib.rs#L10",
                "example-org/example-repo · lib.rs",
            ),
            (
                "https://github.com/example-org/example-repo",
                "example-org/example-repo",
            ),
            (
                "https://github.com/example-org/example-repo/",
                "example-org/example-repo",
            ),
            (
                "https://github.com/example-org/example-repo/releases/tag/v1",
                "example-org/example-repo · releases",
            ),
            (
                "https://github.com/example-org",
                "github.com · /example-org",
            ),
            ("https://example.com", "example.com"),
            ("https://example.com/", "example.com"),
            ("https://example.com/a/b?q=1#frag", "example.com · /a/b"),
            (
                "https://example.com/0123456789012345678901234567890123456789",
                "example.com · /0123456789012345678901234567890…",
            ),
            ("http://localhost:5173/app", "localhost:5173 · /app"),
        ] {
            assert_eq!(label(url), expected, "{url}");
        }
    }

    fn prompts(texts: &[&str]) -> Option<String> {
        let held: Vec<Redacted<String>> = texts.iter().map(|text| redact(text)).collect();
        prompt_title(&held).map(|title| title.as_str().to_owned())
    }

    /// A typed slash command is named `/name`, with its arguments when there are any, in each
    /// shape Claude Code records it: the message alone, then the name tag, then the skill body.
    /// Mutation, each ran red on its row: drop the `/` prefix; read the name from `command-name`;
    /// drop the `command-args` branch; leave the wrapper text in.
    #[test]
    fn a_slash_command_prompt_is_named_by_the_command() {
        for (prompt, expected) in [
            ("<command-message>sessions</command-message>", "/sessions"),
            (
                "<command-message>sessions</command-message>\n<command-name>/sessions</command-name>",
                "/sessions",
            ),
            (
                "<command-message>sessions</command-message>\n<command-name>/sessions</command-name>\n\nBase directory for this skill: /x\n\nbody",
                "/sessions",
            ),
            (
                "<command-message>plan</command-message>\n<command-name>/plan</command-name>\n<command-args>build the thing</command-args>\n\nBase directory for this skill: /x",
                "/plan build the thing",
            ),
            (
                "<command-message>plan</command-message>\n<command-name>/plan</command-name>\n<command-args></command-args>",
                "/plan",
            ),
        ] {
            assert_eq!(prompts(&[prompt]).as_deref(), Some(expected), "{prompt}");
        }
    }

    /// A prompt that opens with a tag other than a command wrapper is not words a person typed:
    /// the next prompt names the session, and with none left nothing does.
    /// Mutation, each ran red: treat every `<` prompt as plain text; stop at the first prompt
    /// instead of falling through; accept a command wrapper anywhere in the first line, not only at its start.
    #[test]
    fn a_prompt_opening_with_another_tag_gives_way_to_the_next() {
        let notice = "<task-notification><task-id>t1</task-id></task-notification>";
        let output = "<local-command-stdout>done</local-command-stdout>";
        assert_eq!(
            prompts(&[notice, output, "fix the bug"]).as_deref(),
            Some("fix the bug")
        );
        assert_eq!(prompts(&[notice, output]), None);
        let quoting =
            "<task-notification>ran <command-message>x</command-message></task-notification>";
        assert_eq!(prompts(&[quoting]), None);
        assert_eq!(prompts(&[]), None);
    }

    /// A prompt with no tag is its first non-blank line, cut at 60 characters with an ellipsis,
    /// and a command's name is cut the same way.
    /// Mutation, each ran red: skip blank lines no longer; cut at 61; skip the cut for commands.
    #[test]
    fn a_plain_prompt_is_its_first_line_cut_to_the_title_length() {
        assert_eq!(
            prompts(&["\n  fix the bug  \nsecond line"]).as_deref(),
            Some("fix the bug")
        );
        let long = "x".repeat(70);
        assert_eq!(
            prompts(&[&long]).as_deref(),
            Some(format!("{}…", "x".repeat(60)).as_str())
        );
        let args = "y".repeat(70);
        assert_eq!(
            prompts(&[&format!(
                "<command-message>plan</command-message>\n<command-args>{args}</command-args>"
            )])
            .as_deref(),
            Some(format!("/plan {}…", "y".repeat(54)).as_str())
        );
    }
}
