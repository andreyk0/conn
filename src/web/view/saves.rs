//! The saved sets of open sessions: the panel on the Sessions page that makes and lists them,
//! and the page of one save, which resumes its sessions one at a time.

use maud::{Markup, html};

use super::Ctx;
use super::NotFound;
use super::card::post_button;
use super::fmt::{ago, clock, local_link, redact};
use crate::saves::{Resume, Save, SaveId, SaveKind, SavedSession};
use crate::web::route::{Page, PostRoute};

/// "Save now", the automatic save, then the kept saves, newest first, each with its delete.
pub(super) fn panel(ctx: &Ctx<'_>) -> Markup {
    let saves = &ctx.model.saves;
    html! {
        section.saves {
            div.saves-head {
                h2 { "Saved sessions" }
                (post_button(PostRoute::SaveSessions, &[], "Save now", ""))
            }
            ul.saves-list {
                li {
                    @match &saves.auto {
                        Some(save) => (save_link(ctx, SaveId::Auto, save)),
                        None => span.muted { "No automatic save yet: it is written once the sessions are read." },
                    }
                }
                @for (id, save) in &saves.kept {
                    li {
                        (save_link(ctx, *id, save))
                        (post_button(PostRoute::DeleteSave, &[("save", &id.to_string())], "🗑", "trash"))
                    }
                }
            }
            @if let Some(failure) = &saves.auto_failure {
                p.problem { "The automatic save could not be written: " (redact(failure)) }
            }
            @for problem in &saves.problems {
                p.problem { (redact(problem)) }
            }
        }
    }
}

/// A save's name, when it was made, linking to its page, with what made it and how many
/// sessions it holds.
fn save_link(ctx: &Ctx<'_>, id: SaveId, save: &Save) -> Markup {
    html! {
        a href=(Page::Save(id).to_string()) { (clock(ctx.zone, save.at)) }
        " " (kind_tag(save.kind))
        " " span.muted { (count_of(save.sessions.len())) ", " (ago(save.at, ctx.now)) }
    }
}

/// "1 session", "2 sessions".
fn count_of(count: usize) -> Markup {
    html! { (count) @if count == 1 { " session" } @else { " sessions" } }
}

fn kind_tag(kind: SaveKind) -> Markup {
    match kind {
        SaveKind::Auto => html! { span.tag.auto-save { "auto" } },
        SaveKind::Manual => html! {},
        SaveKind::BeforeReboot => html! { span.tag.before-reboot { "before reboot" } },
    }
}

/// One save: each session with where it stands now and the command that resumes it.
pub(super) fn page(ctx: &Ctx<'_>, id: SaveId) -> Result<Markup, NotFound> {
    let save = ctx.model.saves.get(id).ok_or(NotFound::Save(id))?;
    let back = save
        .sessions
        .iter()
        .filter(|session| is_running(ctx, session))
        .count();
    Ok(html! {
        p {
            "Saved " (clock(ctx.zone, save.at)) " " (kind_tag(save.kind)) ": "
            (count_of(save.sessions.len())) ", " (back) " running again. "
            a href=(Page::Sessions.to_string()) { "All saves" }
        }
        p.muted {
            "A resume restores the conversation, not a command that was running. Each command "
            "was made while the session ran, with the flags it was launched with."
        }
        @for session in &save.sessions { (saved_session(ctx, session)) }
    })
}

/// Whether the session runs now.
fn is_running(ctx: &Ctx<'_>, session: &SavedSession) -> bool {
    ctx.model
        .sessions
        .iter()
        .any(|view| view.session.id == session.id && view.session.activity.kind().is_live())
}

fn saved_session(ctx: &Ctx<'_>, session: &SavedSession) -> Markup {
    let title = redact(session.title.as_deref().unwrap_or("(no title)"));
    let known = ctx.model.session(&session.id).is_some();
    let running = is_running(ctx, session);
    html! {
        article.saved-session.running[running] {
            h3 {
                @if known { (local_link(&Page::Session(session.id.clone()), &title)) } @else { (title) }
                " "
                @if running { span.tag.back { "running again" } } @else { span.tag { "not running" } }
            }
            @if let Some(gist) = &session.gist { p.gist { (redact(gist)) } }
            p.muted {
                @if let Some(cwd) = &session.cwd { code { (redact(&cwd.display().to_string())) } " · " }
                @if let Some(at) = session.last_activity { "last active " (clock(ctx.zone, at)) " · " }
                "session " code { (session.id.as_str()) }
            }
            (super::owners::session_line(ctx, &session.id))
            @if !running { (resume(&session.resume)) }
        }
    }
}

/// The saved resume command with its copy button, and what it may get wrong. The command is
/// conn's own, built from the session's paths and flags, and shown whole as the session page
/// shows it, so a paste runs exactly what was saved.
fn resume(resume: &Resume) -> Markup {
    match resume {
        Resume::Command { text, warnings } => html! {
            div.cmd {
                pre { (text) }
                button.copy type="button" { "Copy" }
            }
            @if !warnings.is_empty() {
                ul.muted { @for warning in warnings { li { (redact(warning)) } } }
            }
        },
        Resume::Unavailable { why } => html! { p.muted { "No resume command: " (redact(why)) } },
    }
}
