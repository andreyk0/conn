//! How a page takes the human to a session, decided in one place for every page: while a live
//! process runs it in a terminal, a button raises that terminal (resuming it elsewhere would
//! run it twice); otherwise its resume command is shown, to copy.

use maud::{Markup, html};

use super::card::post_button;
use crate::model::SessionView;
use crate::types::{Activity, ResumeCommand, ResumeUnavailable, SessionId};
use crate::web::route::PostRoute;

/// The way to a session.
pub(super) enum Reach<'v> {
    /// Raise its terminal. With no view the session is not in the model, so there is no resume
    /// command to fall back on; the focus handler says what it found.
    Terminal {
        id: &'v SessionId,
        view: Option<&'v SessionView>,
    },
    Resume(&'v SessionView),
    /// No way: its transcript is gone, which the activity's detail says.
    Gone,
}

impl<'v> Reach<'v> {
    pub(super) const fn of(view: &'v SessionView) -> Self {
        if matches!(view.session.activity, Activity::Gone { .. }) {
            Self::Gone
        } else if view.session.runs_in_terminal() {
            Self::Terminal {
                id: &view.session.id,
                view: Some(view),
            }
        } else {
            Self::Resume(view)
        }
    }

    /// For a card whose session may be missing from the model: the terminal is tried.
    pub(super) fn of_card(id: &'v SessionId, view: Option<&'v SessionView>) -> Self {
        view.map_or(Self::Terminal { id, view: None }, Self::of)
    }

    /// The primary button, for a row of actions; nothing when the way is the resume command.
    pub(super) fn button(&self) -> Markup {
        match self {
            Self::Terminal { id, .. } => html! {
                (post_button(PostRoute::Focus, &[("session", id.as_str())], "Open terminal", "primary"))
            },
            Self::Resume(_) | Self::Gone => html! {},
        }
    }

    /// What goes below the actions: the hidden box the focus outcome fills in, which keeps the
    /// resume command for a terminal that cannot be raised; or the resume command itself.
    pub(super) fn below(&self) -> Markup {
        match self {
            Self::Terminal { view: None, .. } | Self::Gone => html! {},
            Self::Terminal {
                view: Some(view), ..
            } => html! {
                div.focus-result hidden {
                    p.msg {}
                    div.resume { (resume_block(&view.resume)) }
                }
            },
            Self::Resume(view) => resume_block(&view.resume),
        }
    }
}

/// The way to `view`'s session, for a place with no other actions.
pub(super) fn reach(view: &SessionView) -> Markup {
    let reach = Reach::of(view);
    html! {
        @if let Reach::Terminal { .. } = reach { div.actions { (reach.button()) } }
        (reach.below())
    }
}

/// A resume command with its copy button, and the reasons it may fall short.
fn resume_block(resume: &Result<ResumeCommand, ResumeUnavailable>) -> Markup {
    match resume {
        Ok(command) => html! {
            div.cmd {
                pre { (command.to_string()) }
                button.copy type="button" { "Copy" }
            }
            @if !command.warnings().is_empty() {
                ul.muted {
                    @for warning in command.warnings() { li { (warning.to_string()) } }
                }
            }
        },
        Err(why) => html! { p.muted { "No resume command: " (why.to_string()) } },
    }
}
