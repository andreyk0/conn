//! The PAM page, laid out for a glance: the grants held or waiting for approval first, with
//! their time left; then every other entitlement the user may request, grouped under where it
//! sits and filterable, each with a request form; break-glass entitlements apart at the
//! bottom.
//!
//! Never blank: an inventory never read says it is being read, a failed read says why (a
//! lapsed gcloud sign-in names the command that renews it), and a list with a later failure
//! is shown beside the failure with its age.

use maud::{Markup, html};

use super::Ctx;
use super::card::post_form;
use super::fmt::{ago, clock, external_link, redact};
use super::pages::filter_box;
use crate::pam::entitlement::{Approval, Entitlement, Justification};
use crate::pam::gcloud::{JUSTIFIED_CHARS_MAX, PamError};
use crate::pam::grant::{Grant, GrantState};
use crate::pam::ids::Parent;
use crate::pam::{Group, Observed, Places, Read, Reading, Row, Status};
use crate::types::{Instant, SafeUrl, secs_since};
use crate::web::route::PostRoute;

pub(super) fn page(ctx: &Ctx<'_>) -> Markup {
    let Some(reading) = &ctx.model.pam else {
        return html! {
            p.empty { "PAM is not configured: add a [pam] section with organizations and quota_project to the config to list your entitlements here." }
        };
    };
    let rows = reading.rows(ctx.now);
    html! {
        (header(ctx, reading))
        @if let Some(failure) = &reading.inventory.failure {
            (failure_line(ctx, "Reading the entitlements", failure))
        }
        @match &reading.inventory.good {
            None => {
                @if reading.inventory.failure.is_none() {
                    p.empty { "reading… (the first read asks every folder and project, and takes a few seconds)" }
                }
            }
            Some(inventory) => {
                @if !inventory.value.unread.is_empty() {
                    p.problem {
                        "Not every parent could be read, so entitlements under these may be missing: "
                        @for (at, unread) in inventory.value.unread.iter().enumerate() {
                            @if at > 0 { "; " }
                            (redact(&unread.parent)) " (" (redact(&unread.reason)) ")"
                        }
                    }
                }
                @let places = &inventory.value.places;
                @if let Places::Failed(reason) = places {
                    p.muted { "Folder and project names could not be read, so parents show by id: " (redact(reason)) }
                }
                section.pam-held {
                    h2 { "Held now" @if !rows.held.is_empty() { " (" (rows.held.len()) ")" } }
                    @if rows.held.is_empty() {
                        p.muted { "No grant is active or waiting for approval." }
                    } @else {
                        table.items.pam {
                            (Table::Held.heads())
                            @for row in &rows.held {
                                (entitlement_row(ctx, row, Table::Held, places.of(&row.entitlement.name.parent)))
                            }
                        }
                    }
                }
                @if rows.regular.is_empty() && rows.break_glass.is_empty() {
                    @if rows.held.is_empty() {
                        p.empty { "There is no entitlement you may request." }
                    }
                } @else {
                    h2 { "Request" }
                    (filter_box("filter by name, folder, project, role or approver"))
                }
                @if !rows.regular.is_empty() {
                    (grouped(ctx, &rows.regular))
                }
                @if !rows.break_glass.is_empty() {
                    section.break-glass data-filter-scope {
                        h2 { "Break glass" }
                        (grouped(ctx, &rows.break_glass))
                    }
                }
            }
        }
    }
}

/// How old the inventory and the grants are, and the button that reads the inventory again.
fn header(ctx: &Ctx<'_>, reading: &Reading) -> Markup {
    let grants_at = reading
        .grants
        .values()
        .filter_map(|observed| observed.good.as_ref().map(|good| good.at))
        .max();
    html! {
        div.actions {
            @if let Some(good) = &reading.inventory.good {
                span.muted { "Entitlements read " (ago(good.at, ctx.now)) " (" (clock(ctx.zone, good.at)) ")" }
            } @else {
                span.muted { "Entitlements not read yet" }
            }
            @if let Some(at) = grants_at {
                span.muted { "· grants read " (ago(at, ctx.now)) }
            }
            (post_form(PostRoute::RefreshPam, &[], &html! { button type="submit" { "Read entitlements again" } }))
        }
    }
}

fn failure_line(ctx: &Ctx<'_>, what: &str, failure: &Read<PamError>) -> Markup {
    html! {
        p.problem {
            @match &failure.value {
                PamError::NotSignedIn => {
                    "gcloud needs a fresh sign-in: run " code { "gcloud auth login" } " in a terminal."
                }
                error @ (PamError::Tool(_) | PamError::Malformed(_)) => {
                    (what) " failed " (ago(failure.at, ctx.now))
                    " (" (clock(ctx.zone, failure.at)) "): " (redact(&error.to_string()))
                }
            }
        }
    }
}

/// Which table a row is drawn in, which decides its columns and whether it names its place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// The grants held: no length or request to offer, and no group header, so each row names
    /// where it sits.
    Held,
    /// Under a group's header, which names the place, with the length and the request form.
    Grouped,
}

impl Table {
    fn heads(self) -> Markup {
        html! {
            tr {
                th { "Entitlement" } th { "Approval" }
                @if self == Self::Grouped { th { "Max" } }
                th { "Status" }
                @if self == Self::Grouped { th { "Request" } }
            }
        }
    }
}

/// The columns of a [`Table::Grouped`] row, which a group header spans.
const GROUPED_COLUMNS: u8 = 5;

/// One table of `groups`: a header row per parent, naming where it sits and linking its PAM
/// page, its entitlements under it. The table filters as one: a header row carries its rows'
/// filter text, so it shows while any of them matches, and the table hides when none does.
fn grouped(ctx: &Ctx<'_>, groups: &[Group<'_>]) -> Markup {
    html! {
        table.items.pam data-filter-scope {
            (Table::Grouped.heads())
            @for group in groups {
                @let texts: Vec<String> = group.rows.iter().map(|row| filter_text(row.entitlement, group.place)).collect();
                tr.pam-group data-filter=(redact(&texts.join(" "))) {
                    td colspan=(GROUPED_COLUMNS) { (place_label(group.parent, group.place)) }
                }
                @for row in &group.rows {
                    (entitlement_row(ctx, row, Table::Grouped, group.place))
                }
            }
        }
    }
}

/// Where a parent sits, linked to its PAM page: its path then its id, or its id alone.
fn place_label(parent: &Parent, place: Option<&str>) -> Markup {
    let id = redact(&parent.to_string());
    let label = html! {
        @match place {
            Some(place) => { span.pam-place { (redact(place)) } " " span.muted { (id) } },
            None => span.pam-place { (id) },
        }
    };
    match console_url(parent) {
        Some(url) => external_link(&url, &label),
        None => label,
    }
}

fn entitlement_row(ctx: &Ctx<'_>, row: &Row<'_>, table: Table, place: Option<&str>) -> Markup {
    let entitlement = row.entitlement;
    let name = entitlement.name.to_string();
    let held = match &row.status {
        Status::Live(grant) if grant.state == GrantState::Active => Some("held active"),
        Status::Live(_) => Some("held pending"),
        Status::Unclear(_) | Status::Over(_) | Status::Free | Status::NotRead => None,
    };
    html! {
        tr class=[held] data-filter=(redact(&filter_text(entitlement, place))) {
            td.entitlement title=(redact(&name)) {
                strong { (redact(entitlement.name.id.as_str())) }
                @if table == Table::Held {
                    span.pam-line { (place_label(&entitlement.name.parent, place)) }
                }
                @if !entitlement.roles.is_empty() {
                    @let roles = redact(&entitlement.roles.join(", "));
                    span.pam-line.muted title=(roles) { (roles) }
                }
            }
            td.approval { (approval(&entitlement.approval)) }
            @if table == Table::Grouped {
                td.max { (span_text(entitlement.max.as_secs())) }
            }
            td.grant { (status(ctx, entitlement, place, &row.status, row.grants)) }
            @if table == Table::Grouped {
                td.request {
                    @if row.status.requestable() {
                        (request_form(entitlement))
                    }
                }
            }
        }
    }
}

/// What the filter box matches a row against, in lower case: the full name (id and parent),
/// where the parent sits, the roles and the approvers.
fn filter_text(entitlement: &Entitlement, place: Option<&str>) -> String {
    let approvers = match &entitlement.approval {
        Approval::Auto => "automatic".to_owned(),
        Approval::Manual { approvers } => approvers.join(" "),
    };
    format!(
        "{} {}{} {approvers}",
        entitlement.name,
        place.map_or_else(String::new, |place| format!("{place} ")),
        entitlement.roles.join(" ")
    )
    .to_lowercase()
}

fn approval(approval: &Approval) -> Markup {
    match approval {
        Approval::Auto => html! { span.muted { "automatic" } },
        Approval::Manual { approvers } => html! {
            @for (at, principal) in approvers.iter().enumerate() {
                @if at > 0 { br; }
                span title=(redact(principal)) { (redact(&short_principal(principal))) }
            }
        },
    }
}

/// `group:team-a@example.com` reads as `team-a`; the whole principal is the title.
fn short_principal(principal: &str) -> String {
    let bare = principal
        .split_once(':')
        .map_or(principal, |(_kind, rest)| rest);
    bare.split_once('@')
        .map_or(bare, |(local, _domain)| local)
        .to_owned()
}

fn status(
    ctx: &Ctx<'_>,
    entitlement: &Entitlement,
    place: Option<&str>,
    status: &Status<'_>,
    grants: Option<&Observed<Vec<Grant>>>,
) -> Markup {
    let failure = grants.and_then(|observed| observed.failure.as_ref());
    html! {
        @match status {
            Status::Live(grant) => (live(ctx, entitlement, place, grant)),
            Status::Unclear(grant) => {
                span.badge { (grant.state.to_string()) }
                span.muted { " requested " (ago(grant.created, ctx.now)) }
            }
            Status::Over(grant) => {
                span.muted { (grant.state.to_string()) br; "requested " (ago(grant.created, ctx.now)) }
            }
            // Nothing held is the usual row; left blank, the rows that hold something stand out.
            Status::Free => {},
            Status::NotRead => span.muted { "not read yet" },
        }
        @if let Some(failure) = failure {
            br;
            span.muted.stale title=(redact(&failure.value.to_string())) {
                "the latest read failed " (ago(failure.at, ctx.now))
            }
        }
    }
}

fn live(ctx: &Ctx<'_>, entitlement: &Entitlement, place: Option<&str>, grant: &Grant) -> Markup {
    let ends = grant.ends_at();
    html! {
        strong.pam-state { (grant.state.to_string()) }
        @match (&grant.state, ends) {
            (GrantState::Active, Some(ends)) => {
                br;
                span.pam-left { (left(ctx.now, ends)) " left" } " "
                span.muted { "(ends " (clock(ctx.zone, ends)) ")" }
            }
            (GrantState::ApprovalAwaited, _) => {
                span.muted { " since " (ago(grant.created, ctx.now)) }
                @if let Some(url) = approvals_url(&entitlement.name.parent) {
                    br;
                    (external_link(&url, &html! { "approvals page" }))
                }
                @if let Some(ask) = slack_ask(entitlement, place, grant) {
                    br;
                    button.copy-text type="button" data-copy=(redact(&ask))
                        data-copied="Slack message copied: paste it where your approvers will see it."
                        title=(redact(&ask)) { "Copy Slack ask" }
                }
            }
            _ => {
                span.muted { " requested " (ago(grant.created, ctx.now)) }
            }
        }
    }
}

/// A one-line Slack message asking for `grant`'s approval, linking the page an approver acts
/// on (the scope's PAM page where that is not known): what, where and for how long. Where is
/// the project's id, or a folder's or an organization's path, since their numbers mean
/// nothing to a reader. `None` when there is no link to give.
fn slack_ask(entitlement: &Entitlement, place: Option<&str>, grant: &Grant) -> Option<String> {
    let parent = &entitlement.name.parent;
    let url = approvals_url(parent).or_else(|| console_url(parent))?;
    let target = match (parent, place) {
        (Parent::Project(project), _) => project.as_str().to_owned(),
        (Parent::Folder(_) | Parent::Organization(_), Some(place)) => place.to_owned(),
        (Parent::Folder(_) | Parent::Organization(_), None) => parent.to_string(),
    };
    Some(format!(
        "🔐 PAM approval, please: *{}* on `{target}` for {} 👉 [approve here]({}) 🙏",
        entitlement.name.id.as_str(),
        span_text(grant.requested.as_secs()),
        url.as_str()
    ))
}

/// The console's PAM page scoped to `parent`, through the console's resource parameter for
/// its kind (`project`, `folder`, `organizationId`).
fn console_url(parent: &Parent) -> Option<SafeUrl> {
    let scope = match parent {
        Parent::Project(project) => format!("project={}", project.as_str()),
        Parent::Folder(folder) => format!("folder={}", folder.as_str()),
        Parent::Organization(org) => format!("organizationId={}", org.as_str()),
    };
    SafeUrl::parse(&format!(
        "https://console.cloud.google.com/iam-admin/pam?{scope}"
    ))
    .ok()
}

/// The console page an approver acts on. Only the project form is known to work; a folder or
/// an organization gets none rather than a guessed link.
fn approvals_url(parent: &Parent) -> Option<SafeUrl> {
    match parent {
        Parent::Project(project) => SafeUrl::parse(&format!(
            "https://console.cloud.google.com/iam-admin/pam/grants/approvals?project={}&supportedpurview=project",
            project.as_str()
        ))
        .ok(),
        Parent::Folder(_) | Parent::Organization(_) => None,
    }
}

/// Time left as hours and minutes, rounded down: `3 h 12 min`, `45 min`, `<1 min`.
fn left(now: Instant, ends: Instant) -> String {
    let secs = secs_since(now, ends);
    let (hours, mins) = (secs.div_euclid(3600), secs.rem_euclid(3600).div_euclid(60));
    match (hours, mins) {
        (0, 0) => "<1 min".to_owned(),
        (0, mins) => format!("{mins} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, mins) => format!("{hours} h {mins} min"),
    }
}

/// A grant length: `4 h`, `90 min`.
fn span_text(secs: u64) -> String {
    if secs.rem_euclid(3600) == 0 {
        format!("{} h", secs.div_euclid(3600))
    } else {
        format!("{} min", secs.div_euclid(60))
    }
}

/// The length offered, the reason (marked required where PAM requires one) and the button.
fn request_form(entitlement: &Entitlement) -> Markup {
    let name = entitlement.name.to_string();
    let default = entitlement.default_length();
    let required = matches!(entitlement.justification, Justification::Required);
    let controls = html! {
        select name="secs" aria-label="Length" {
            @for length in entitlement.lengths() {
                option value=(length.secs()) selected[Some(length) == default] {
                    (span_text(length.secs()))
                }
            }
        }
        input type="text" name="justification" maxlength=(JUSTIFIED_CHARS_MAX)
            required[required]
            placeholder=(if required { "why (required)" } else { "why (optional)" })
            aria-label="Justification";
        button type="submit" { "Request" }
    };
    post_form(
        PostRoute::RequestGrant,
        &[("entitlement", &name)],
        &controls,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutation: round the minutes up, or show `0 h`.
    #[test]
    fn time_left_is_hours_and_minutes_rounded_down() {
        let now: Instant = "2026-11-19T10:00:00Z".parse().unwrap();
        let at = |text: &str| -> Instant { text.parse().unwrap() };
        assert_eq!(left(now, at("2026-11-19T13:12:59Z")), "3 h 12 min");
        assert_eq!(left(now, at("2026-11-19T14:00:00Z")), "4 h");
        assert_eq!(left(now, at("2026-11-19T10:45:30Z")), "45 min");
        assert_eq!(left(now, at("2026-11-19T10:00:30Z")), "<1 min");
        assert_eq!(left(now, at("2026-11-19T09:00:00Z")), "<1 min");
    }

    #[test]
    fn principals_read_as_their_local_part() {
        assert_eq!(short_principal("group:team-a@example.com"), "team-a");
        assert_eq!(short_principal("user:dev@example.com"), "dev");
        assert_eq!(short_principal("domain:example.com"), "example.com");
    }
}
