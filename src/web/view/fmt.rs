//! How the model's values read on a page: ages, badges, phrases, and the one place text from
//! outside is made safe to show.
//!
//! Third-party text (assistant prose, titles, questions) is a [`Redacted`] before a template
//! sees it: the helpers here and in the sibling modules take `&Redacted<String>`, so a forgotten
//! redaction does not compile. The text the model still holds as a plain `String` becomes
//! `Redacted` through [`redact`], the one call of the constructor in the view. `maud` escapes
//! whatever a template interpolates. External `href`s are written only by [`external_link`],
//! from a [`SafeUrl`]; the site's own by [`local_link`], from a [`Page`], and by
//! [`file_link`], from a [`LocalFile`].

use jiff::tz::TimeZone;
use maud::{Markup, Render, html};

use crate::gh::{Answer, Checks, Observed, PrLifecycle, Readiness, Reading, ReviewDecision};
use crate::model::Seen;
use crate::types::{
    Activity, ActivityKind, Age, AskUserQuestion, Instant, LocalFile, MultiSelect, PendingCard,
    SafeUrl, TurnTrigger, WaitingFor, secs_since,
};
use crate::util::redact::Redacted;
use crate::web::route::{Page, file_href};

use super::Ctx;

impl Render for Redacted<String> {
    fn render_to(&self, buffer: &mut String) {
        self.as_str().render_to(buffer);
    }
}

/// Text the model holds as a plain string (a failure reason, a path, an unrecognised trigger),
/// with known secret shapes masked. The view's one call of [`Redacted::of`].
pub(super) fn redact(text: &str) -> Redacted<String> {
    Redacted::of(text)
}

/// A link to a page of this site.
pub(super) fn local_link(page: &Page, label: &Redacted<String>) -> Markup {
    html! { a href=(page.to_string()) { (label) } }
}

/// A link to a local file the site shows: the page's script opens it in a pop-up, and a
/// modified click or a script-less page opens the file's own page.
pub(super) fn file_link(file: &LocalFile, label: &Markup) -> Markup {
    html! { a.file-link href=(file_href(file)) title=(redact(&file.to_string())) { (label) } }
}

/// A link out, opened in a new tab without handing over the opener.
pub(super) fn external_link(url: &SafeUrl, label: &Markup) -> Markup {
    html! { a href=(url.as_str()) target="_blank" rel="noopener noreferrer" { (label) } }
}

// --- time ---

/// `4 s ago`, `7 min ago`: seconds under a minute, then the single coarser unit of [`Age`].
pub(super) fn ago(then: Instant, now: Instant) -> String {
    let secs = secs_since(then, now);
    if secs < 60 {
        format!("{secs} s ago")
    } else {
        format!("{} ago", Age::from_secs(secs))
    }
}

/// `in 7 d`, `in 3 h`: how far off a time to come is, in the single coarser unit of [`Age`].
pub(super) fn ahead(then: Instant, now: Instant) -> String {
    format!("in {}", Age::between(now, then))
}

/// A moment to come or gone, as a person would name it in the viewer's zone: `Fri Nov 27
/// 05:00`, with the year when it is not the current one.
pub(super) fn moment(zone: &TimeZone, at: Instant, now: Instant) -> String {
    let at = at.to_zoned(zone.clone());
    let format = if at.year() == now.to_zoned(zone.clone()).year() {
        "%a %b %-d %H:%M"
    } else {
        "%a %b %-d %Y %H:%M"
    };
    at.strftime(format).to_string()
}

/// The calendar day in the viewer's zone.
pub(super) fn date(zone: &TimeZone, at: Instant) -> String {
    at.to_zoned(zone.clone()).strftime("%Y-%m-%d").to_string()
}

/// The wall-clock time in the viewer's zone, to the second.
pub(super) fn clock(zone: &TimeZone, at: Instant) -> String {
    at.to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string()
}

// --- activity ---

/// What a session is doing, as the page groups and colours it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Blocked,
    Watching,
    Idle,
    Running,
    Unknown,
    Dormant,
    Gone,
}

impl Kind {
    /// The groups of the sessions page, most urgent first.
    pub(super) const LIVE: [Self; 5] = [
        Self::Blocked,
        Self::Watching,
        Self::Idle,
        Self::Running,
        Self::Unknown,
    ];

    pub(super) const fn of(activity: &Activity) -> Self {
        match activity.kind() {
            ActivityKind::Blocked => Self::Blocked,
            ActivityKind::Watching => Self::Watching,
            ActivityKind::Idle => Self::Idle,
            ActivityKind::Running => Self::Running,
            ActivityKind::Unknown => Self::Unknown,
            ActivityKind::Dormant => Self::Dormant,
            ActivityKind::Gone => Self::Gone,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::Watching => "Watching",
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Unknown => "Unknown",
            Self::Dormant => "Dormant",
            Self::Gone => "Gone",
        }
    }

    const fn class(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Watching => "watching",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Dormant => "dormant",
            Self::Gone => "gone",
        }
    }
}

/// The moment the card or session has been in its state since, when the activity names one.
pub(super) fn since_of(activity: &Activity, card: Option<&PendingCard>) -> Option<Instant> {
    match activity {
        Activity::Blocked { .. } => card.map(PendingCard::since),
        Activity::Watching { ended_at, .. } | Activity::Idle { ended_at, .. } => Some(*ended_at),
        Activity::Running { since } => Some(*since),
        Activity::Dormant { last_seen, .. } => Some(*last_seen),
        Activity::Unknown { .. } | Activity::Gone { .. } => None,
    }
}

/// `Blocked · 7 min`: the activity's word, derived from the activity, and how long.
pub(super) fn badge(activity: &Activity, since: Option<Instant>, now: Instant) -> Markup {
    let kind = Kind::of(activity);
    html! {
        span class={ "badge " (kind.class()) } {
            (kind.label())
            @if let Some(since) = since {
                " · " (Age::between(since, now))
            }
        }
    }
}

/// What more there is to say about an activity: why it is unknown, what it waits for, how a
/// dormant background session ended, why a gone one cannot be resumed.
pub(super) fn activity_detail(activity: &Activity) -> Option<Markup> {
    match activity {
        Activity::Blocked { on } => Some(html! { "waiting for " (waiting_for(on)) }),
        Activity::Unknown { reason } => Some(html! { (reason) }),
        Activity::Dormant {
            background: Some(end),
            ..
        } => Some(html! { "background session " (end.words()) }),
        Activity::Gone { background } => Some(html! {
            "its transcript is gone, so there is nothing to resume"
            @if let Some(state) = background {
                "; the background session " (state.words()) " and its record stays until "
                code { "claude rm" } " removes it"
            }
        }),
        Activity::Running { .. }
        | Activity::Watching { .. }
        | Activity::Idle { .. }
        | Activity::Dormant {
            background: None, ..
        } => None,
    }
}

/// What a blocked session is stopped on, as a noun phrase.
pub(super) fn waiting_for(on: &WaitingFor) -> Markup {
    match on {
        WaitingFor::PermissionPrompt => html! { "a permission prompt" },
        WaitingFor::InputNeeded => html! { "an answer to a question" },
        WaitingFor::SandboxRequest => html! { "a sandbox request" },
        WaitingFor::WorkerRequest => html! { "a worker request" },
        WaitingFor::DialogOpen => html! { "an open dialog" },
        WaitingFor::QuotaHold => html! { "the usage quota to reset" },
        WaitingFor::Other(reason) => html! { (redact(reason)) },
    }
}

/// What started the turn that ended, as it reads after "ended".
pub(super) fn trigger_phrase(trigger: &TurnTrigger) -> Markup {
    match trigger {
        TurnTrigger::Human => html! { "after your prompt" },
        TurnTrigger::TaskNotification => html! { "after a task notification" },
        TurnTrigger::Peer => html! { "after a message from another session" },
        TurnTrigger::Scheduled => html! { "after a scheduled wake-up" },
        TurnTrigger::Coordinator => html! { "after a coordinator's dispatch" },
        TurnTrigger::AutoContinuation => html! { "after the agent continued itself" },
        TurnTrigger::LocalCommand => html! { "after a local command" },
        TurnTrigger::CompactionSummary => html! { "after a compaction" },
        TurnTrigger::Unknown(other) => html! {
            "after a trigger this tool does not know (" (redact(other)) ")"
        },
    }
}

// --- text ---

/// How many characters a body shows before it collapses behind "more".
pub(super) const BODY_CHARS_MAX: usize = 1200;

/// The first non-blank line of the text, cut to `max` characters. A line of redacted text is
/// redacted; the result is passed through [`redact`] again because the cut yields a new string.
pub(super) fn first_line(text: &Redacted<String>, max: usize) -> Redacted<String> {
    let line = text
        .as_str()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() > max {
        let cut: String = line.chars().take(max).collect();
        redact(&format!("{cut}…"))
    } else {
        redact(line)
    }
}

/// `body` behind a "more" toggle when `text` is longer than [`BODY_CHARS_MAX`]; the whole body
/// is in the page either way. `id` must be unique on the page.
pub(super) fn collapsible(id: &str, text: &Redacted<String>, body: Markup) -> Markup {
    if text.as_str().chars().count() <= BODY_CHARS_MAX {
        return body;
    }
    html! {
        input.more-toggle type="checkbox" id=(id);
        div.clip { (body) }
        label.more for=(id) {
            span.more-open { "more" }
            span.more-close { "less" }
        }
    }
}

/// The questions a turn ended on, each with its header and numbered options. The question and
/// the option descriptions are markdown.
pub(super) fn questions(ctx: &Ctx<'_>, asked: &AskUserQuestion) -> Markup {
    html! {
        @for question in asked.questions() {
            div.question {
                @if let Some(header) = &question.header {
                    strong { (header) }
                    " "
                }
                div.lead { (ctx.markdown(&question.question)) }
                @if question.multi_select == MultiSelect::Many {
                    " " span.tag { "multi-select" }
                }
                ol {
                    @for option in &question.options {
                        li {
                            strong { (option.label) }
                            @if let Some(description) = option.description.as_ref().filter(|d| !d.as_str().trim().is_empty()) {
                                " " div.lead.muted { (ctx.markdown(description)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// --- pull requests ---

/// The state of a pull request in one word, for its chip: what `gh` last said, or that it has
/// not said anything usable.
pub(super) fn pr_badge(reading: &Reading) -> Markup {
    match reading {
        Reading::Unread => html! { span.badge.pr-unread { "not read" } },
        Reading::Value { observed, .. } => {
            let (class, word) = match &observed.answer {
                Answer::Found(state) => match (&state.state, state.readiness) {
                    (PrLifecycle::Open, Readiness::Draft) => ("pr-draft", "draft"),
                    (PrLifecycle::Open, Readiness::Ready) => ("pr-open", "open"),
                    (PrLifecycle::Merged { .. }, _) => ("pr-merged", "merged"),
                    (PrLifecycle::Closed, _) => ("pr-closed", "closed"),
                },
                Answer::NotFound => ("pr-not-found", "not found"),
            };
            html! { span class={ "badge " (class) } { (word) } }
        }
        Reading::Failure(_) => html! { span.badge.pr-failed { "unavailable" } },
    }
}

/// The checks and the review of a pull request as two glyphs, for its chip; each carries its
/// words as an accessible name. Nothing for a reading that holds no state, and no review glyph
/// where the review says nothing worth a mark.
pub(super) fn pr_glyphs(reading: &Reading) -> Markup {
    let Reading::Value {
        observed: Observed {
            answer: Answer::Found(state),
            ..
        },
        ..
    } = reading
    else {
        return html! {};
    };
    html! {
        " " span.glyph.checks aria-label=(checks_text(state.checks)) { (checks_glyph(state.checks)) }
        @if let Some(glyph) = review_glyph(&state.review) {
            " " span.glyph.review aria-label=(review_text(&state.review)) { (glyph) }
        }
    }
}

fn checks_glyph(checks: Checks) -> String {
    match checks {
        Checks::Passing => "✓".to_owned(),
        Checks::Failing { count } => format!("✗{count}"),
        Checks::Pending { .. } => "…".to_owned(),
        Checks::NoChecks => "–".to_owned(),
        Checks::Unknown => "?".to_owned(),
    }
}

const fn review_glyph(review: &ReviewDecision) -> Option<&'static str> {
    match review {
        ReviewDecision::Approved => Some("✓"),
        ReviewDecision::ChangesRequested => Some("✎"),
        ReviewDecision::ReviewRequired => Some("◌"),
        ReviewDecision::NoneRequired | ReviewDecision::Other(_) => None,
    }
}

/// How long ago a pull request was read, when that is worth a chip's room: a reading that is
/// stale (its refresh failed) or failed. A fresh reading is no age; so is one never made.
pub(super) fn pr_age(reading: &Reading, now: Instant) -> Option<String> {
    match reading {
        Reading::Unread | Reading::Value { failure: None, .. } => None,
        Reading::Value {
            age,
            failure: Some(_),
            ..
        } => Some(format!("{age} old")),
        Reading::Failure(failed) => Some(ago(failed.at, now)),
    }
}

/// What `gh` last said about a pull request, in words and never blank: a stale or failed
/// reading says so. The chip's `title`.
pub(super) fn pr_reading_text(reading: &Reading, now: Instant) -> String {
    match reading {
        Reading::Unread => "not read yet".to_owned(),
        Reading::Value {
            observed,
            age,
            failure,
        } => {
            let answer = match &observed.answer {
                Answer::Found(state) => {
                    let lifecycle = match state.state {
                        PrLifecycle::Open => "open",
                        PrLifecycle::Merged { .. } => "merged",
                        PrLifecycle::Closed => "closed",
                    };
                    let draft = if state.readiness == Readiness::Draft {
                        " · draft"
                    } else {
                        ""
                    };
                    format!(
                        "{lifecycle}{draft} · {} · {}",
                        review_text(&state.review),
                        checks_text(state.checks),
                    )
                }
                Answer::NotFound => {
                    "GitHub has no such pull request, or this account cannot see it".to_owned()
                }
            };
            let refresh_failure = failure.as_ref().map_or_else(String::new, |failed| {
                format!(
                    " · stale: refresh failed {}: {}",
                    ago(failed.at, now),
                    redact(&failed.error.to_string()).as_str()
                )
            });
            format!("{answer} · read {age} ago{refresh_failure}")
        }
        Reading::Failure(failed) => format!(
            "unavailable: {} ({})",
            redact(&failed.error.to_string()).as_str(),
            ago(failed.at, now)
        ),
    }
}

fn review_text(review: &ReviewDecision) -> String {
    match review {
        ReviewDecision::Approved => "approved".to_owned(),
        ReviewDecision::ChangesRequested => "changes requested".to_owned(),
        ReviewDecision::ReviewRequired => "review required".to_owned(),
        ReviewDecision::NoneRequired => "no review required".to_owned(),
        ReviewDecision::Other(other) => format!("review {}", redact(other).as_str()),
    }
}

fn checks_text(checks: Checks) -> String {
    match checks {
        Checks::Passing => "checks passing".to_owned(),
        Checks::Failing { count } => format!("{count} checks failing"),
        Checks::Pending { count } => format!("{count} checks pending"),
        Checks::NoChecks => "no checks".to_owned(),
        Checks::Unknown => "checks unknown".to_owned(),
    }
}

/// Whether the card is dimmed.
pub(super) const fn dimmed(seen: Seen) -> bool {
    match seen {
        Seen::Seen => true,
        Seen::Unseen => false,
    }
}

#[cfg(test)]
mod tests {
    //! Goal: a chip's checks and review each read as one glyph. Method: a hand-written table
    //! from the spec, one row per state; no expected value is built by the code.

    use super::*;

    /// Mutation, each ran red on its row: swap the pending and unknown glyphs; drop the
    /// failing count; give `NoneRequired` a glyph.
    #[test]
    fn checks_and_review_have_one_glyph_each() {
        for (checks, glyph) in [
            (Checks::Passing, "✓"),
            (Checks::Failing { count: 3 }, "✗3"),
            (Checks::Pending { count: 2 }, "…"),
            (Checks::NoChecks, "–"),
            (Checks::Unknown, "?"),
        ] {
            assert_eq!(checks_glyph(checks), glyph, "{checks:?}");
        }
        for (review, glyph) in [
            (ReviewDecision::Approved, Some("✓")),
            (ReviewDecision::ChangesRequested, Some("✎")),
            (ReviewDecision::ReviewRequired, Some("◌")),
            (ReviewDecision::NoneRequired, None),
            (ReviewDecision::Other("X".to_owned()), None),
        ] {
            assert_eq!(review_glyph(&review), glyph, "{review:?}");
        }
    }

    /// A moment in the current year leaves the year out; one in another year names it; how
    /// far off it is reads in one unit.
    /// Mutation, each ran red: always leave the year out; compare the years in UTC instead
    /// of the viewer's zone (the New Year's Eve row); measure `ahead` from the wrong end.
    #[test]
    fn a_moment_names_its_year_only_when_it_is_not_this_one() {
        let at = |text: &str| text.parse::<Instant>().unwrap();
        let utc = TimeZone::UTC;
        let now = at("2026-11-19T12:00:00Z");
        assert_eq!(
            moment(&utc, at("2026-11-27T05:00:00Z"), now),
            "Fri Nov 27 05:00"
        );
        assert_eq!(
            moment(&utc, at("2027-01-13T22:00:00Z"), now),
            "Wed Jan 13 2027 22:00"
        );
        let new_york = TimeZone::get("America/New_York").unwrap();
        assert_eq!(
            moment(
                &new_york,
                at("2026-12-31T23:00:00Z"),
                at("2027-01-01T03:00:00Z")
            ),
            "Thu Dec 31 18:00"
        );
        assert_eq!(ahead(at("2026-11-27T05:00:00Z"), now), "in 7 d");
    }
}
