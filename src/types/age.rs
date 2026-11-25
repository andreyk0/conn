//! Durations for people.

use std::fmt;

use super::Instant;

const SECS_PER_MIN: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MIN;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

/// Whole seconds from `then` to `now`; a clock that ran backwards is zero, because nothing
/// here has use for a negative span.
pub fn secs_since(then: Instant, now: Instant) -> u64 {
    u64::try_from(now.duration_since(then).as_secs()).unwrap_or(0)
}

/// How long ago something happened, at the one resolution a card needs.
///
/// Witnesses that the span was reduced to a single unit, rounded down: 3599 s is
/// "59 min", never "1 h".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Age {
    LessThanMinute,
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

impl Age {
    /// The age of a span of whole seconds.
    pub const fn from_secs(secs: u64) -> Self {
        // Integer division floors, which is the rounding the type promises.
        match secs {
            0..SECS_PER_MIN => Self::LessThanMinute,
            SECS_PER_MIN..SECS_PER_HOUR => Self::Minutes(secs / SECS_PER_MIN),
            SECS_PER_HOUR..SECS_PER_DAY => Self::Hours(secs / SECS_PER_HOUR),
            SECS_PER_DAY.. => Self::Days(secs / SECS_PER_DAY),
        }
    }

    /// The age of `earlier` as seen at `now`; see [`secs_since`] for a backwards clock.
    pub fn between(earlier: Instant, now: Instant) -> Self {
        Self::from_secs(secs_since(earlier, now))
    }
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LessThanMinute => f.write_str("<1 min"),
            Self::Minutes(n) => write!(f, "{n} min"),
            Self::Hours(n) => write!(f, "{n} h"),
            Self::Days(n) => write!(f, "{n} d"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(secs: u64) -> String {
        Age::from_secs(secs).to_string()
    }

    #[test]
    fn under_a_minute() {
        assert_eq!(shown(0), "<1 min");
        assert_eq!(shown(59), "<1 min");
    }

    #[test]
    fn minutes_round_down() {
        assert_eq!(shown(60), "1 min");
        assert_eq!(shown(7 * 60 + 59), "7 min");
        assert_eq!(shown(3599), "59 min");
    }

    #[test]
    fn hours_round_down() {
        assert_eq!(shown(3600), "1 h");
        assert_eq!(shown(3 * 3600 + 1799), "3 h");
        assert_eq!(shown(86_399), "23 h");
    }

    #[test]
    fn days_round_down() {
        assert_eq!(shown(86_400), "1 d");
        assert_eq!(shown(2 * 86_400 + 5), "2 d");
    }

    #[test]
    fn between_measures_from_earlier_to_now() {
        let earlier: Instant = "2026-10-28T10:00:00Z".parse().unwrap();
        let now: Instant = "2026-10-28T10:07:30Z".parse().unwrap();
        assert_eq!(Age::between(earlier, now).to_string(), "7 min");
    }

    /// Mutation: return the absolute difference instead of clamping.
    #[test]
    fn secs_since_clamps_a_backwards_clock() {
        let early: Instant = "2026-10-28T10:00:00Z".parse().unwrap();
        let late: Instant = "2026-10-28T10:00:07Z".parse().unwrap();
        assert_eq!(secs_since(early, late), 7);
        assert_eq!(secs_since(late, early), 0);
    }

    #[test]
    fn between_clamps_a_backwards_clock() {
        let earlier: Instant = "2026-10-28T10:07:30Z".parse().unwrap();
        let now: Instant = "2026-10-28T10:00:00Z".parse().unwrap();
        assert_eq!(Age::between(earlier, now).to_string(), "<1 min");
    }
}
