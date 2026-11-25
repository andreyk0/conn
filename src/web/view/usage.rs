//! The plan's rate-limit gauges in the header: one meter per window, with when it resets.

use jiff::tz::TimeZone;
use maud::{Markup, html};

use super::Ctx;
use super::fmt::{ago, redact};
use crate::model::SourceState;
use crate::types::{Age, Instant};
use crate::usage::Window;

/// Where a meter turns amber and then red, in percent used.
const AMBER_FROM: u8 = 75;
const RED_FROM: u8 = 90;

/// The gauges, marked stale with the reason when the latest read failed after them; a mark
/// alone when no read has succeeded and one failed; nothing before the first read lands.
pub(super) fn gauges(ctx: &Ctx<'_>) -> Markup {
    let reading = &ctx.model.usage;
    let failure = match reading.state() {
        SourceState::Failed { reason, .. } => Some(match reading.backed_off_until(ctx.now) {
            None => reason.clone(),
            Some(until) => format!(
                "{reason}; the next read is in {}",
                Age::between(ctx.now, until)
            ),
        }),
        SourceState::Ok { .. } | SourceState::NeverRead | SourceState::NeverWritten { .. } => None,
    };
    match (reading.last_good(), failure.as_deref()) {
        (None, None) => html! {},
        (None, Some(reason)) => html! {
            span.usage.stale title=(redact(&format!("plan usage not read: {reason}"))) { "usage ?" }
        },
        (Some(read), failure) => {
            let title = match failure {
                None => format!("plan usage as of {}", ago(read.at, ctx.now)),
                Some(reason) => format!(
                    "plan usage as of {}; the latest read failed: {reason}",
                    ago(read.at, ctx.now)
                ),
            };
            html! {
                span.usage.stale[failure.is_some()] title=(redact(&title)) {
                    @for window in &read.windows { (gauge(ctx, window)) }
                }
            }
        }
    }
}

fn gauge(ctx: &Ctx<'_>, window: &Window) -> Markup {
    let percent = format!("{:.0}", window.used_percent.floor());
    let reset = window
        .resets_at
        .map(|at| Reset::of(window, ctx.zone, at, ctx.now));
    let title = match &reset {
        Some(reset) => format!(
            "{}: {percent}% used, resets {}",
            window.span,
            reset.phrase()
        ),
        None => format!("{}: {percent}% used", window.span),
    };
    html! {
        span.gauge title=(title) {
            (window.span.to_string()) " "
            meter min="0" max="100" low=(AMBER_FROM) high=(RED_FROM) optimum="0"
                value=(window.used_percent) {}
            " " (percent) "%"
            @if let Some(reset) = &reset { " " span.reset { "↻" (reset.short()) } }
        }
    }
}

/// When a window resets: a countdown for one that resets within the day, a weekday and time
/// for a week.
enum Reset {
    Now,
    /// `3h 5m`, `40m`.
    After(String),
    /// `Fri 15:00`, in the viewer's zone.
    On(String),
}

impl Reset {
    fn of(window: &Window, zone: &TimeZone, at: Instant, now: Instant) -> Self {
        if !window.span.resets_within_the_day() {
            return Self::On(at.to_zoned(zone.clone()).strftime("%a %H:%M").to_string());
        }
        let left = at.duration_since(now).as_secs();
        if left <= 0 {
            return Self::Now;
        }
        let (hours, minutes) = (left / 3_600, left % 3_600 / 60);
        Self::After(if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m")
        })
    }

    /// What the gauge shows beside the percentage.
    fn short(&self) -> &str {
        match self {
            Self::Now => "now",
            Self::After(left) => left,
            Self::On(when) => when,
        }
    }

    /// What follows "resets" in the gauge's title.
    fn phrase(&self) -> String {
        match self {
            Self::Now => "now".to_owned(),
            Self::After(left) => format!("in {left}"),
            Self::On(when) => when.clone(),
        }
    }
}
