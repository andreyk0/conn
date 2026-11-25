//! A grant as `gcloud pam grants search` and `grants create` print it: its state, how long it
//! was asked for, and when it was activated, which is when its window starts.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use super::entitlement::seconds;
use crate::types::Instant;

/// A grant's state, as PAM names it. A name this tool does not know is kept, never refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantState {
    ApprovalAwaited,
    Scheduled,
    Activating,
    Active,
    /// On its way to one of the states that end a grant.
    Ending,
    Revoking,
    Withdrawing,
    Ended,
    Denied,
    Expired,
    ActivationFailed,
    Revoked,
    Withdrawn,
    /// Its bindings were changed outside PAM.
    ExternallyModified,
    Unknown(String),
}

/// What a state means for a new request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Pending or in effect: a second request would duplicate it.
    Live,
    /// Over: a new request is the way back in.
    Over,
    /// A state whose meaning is not settled here; it is shown as PAM names it, and a request
    /// is left to PAM to accept or refuse.
    Unclear,
}

impl GrantState {
    fn parse(text: &str) -> Self {
        match text {
            "APPROVAL_AWAITED" => Self::ApprovalAwaited,
            "SCHEDULED" => Self::Scheduled,
            "ACTIVATING" => Self::Activating,
            "ACTIVE" => Self::Active,
            "ENDING" => Self::Ending,
            "REVOKING" => Self::Revoking,
            "WITHDRAWING" => Self::Withdrawing,
            "ENDED" => Self::Ended,
            "DENIED" => Self::Denied,
            "EXPIRED" => Self::Expired,
            "ACTIVATION_FAILED" => Self::ActivationFailed,
            "REVOKED" => Self::Revoked,
            "WITHDRAWN" => Self::Withdrawn,
            "EXTERNALLY_MODIFIED" => Self::ExternallyModified,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub const fn phase(&self) -> Phase {
        match self {
            Self::ApprovalAwaited
            | Self::Scheduled
            | Self::Activating
            | Self::Active
            | Self::Ending
            | Self::Revoking
            | Self::Withdrawing => Phase::Live,
            Self::Ended
            | Self::Denied
            | Self::Expired
            | Self::ActivationFailed
            | Self::Revoked
            | Self::Withdrawn => Phase::Over,
            Self::ExternallyModified | Self::Unknown(_) => Phase::Unclear,
        }
    }
}

impl fmt::Display for GrantState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApprovalAwaited => "Awaiting approval",
            Self::Scheduled => "Scheduled",
            Self::Activating => "Activating",
            Self::Active => "Active",
            Self::Ending => "Ending",
            Self::Revoking => "Being revoked",
            Self::Withdrawing => "Being withdrawn",
            Self::Ended => "Ended",
            Self::Denied => "Denied",
            Self::Expired => "Expired unapproved",
            Self::ActivationFailed => "Activation failed",
            Self::Revoked => "Revoked",
            Self::Withdrawn => "Withdrawn",
            Self::ExternallyModified => "Externally modified",
            Self::Unknown(name) => name,
        })
    }
}

/// One grant the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub state: GrantState,
    pub created: Instant,
    pub requested: Duration,
    /// When it was activated, the start of its window; `None` until it is.
    pub activated: Option<Instant>,
}

impl Grant {
    /// When its window closes: activation plus the length asked for. `None` before activation,
    /// since the window runs from activation, not from the request.
    pub fn ends_at(&self) -> Option<Instant> {
        let requested = jiff::SignedDuration::try_from(self.requested).ok()?;
        self.activated?.checked_add(requested).ok()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGrant {
    state: String,
    create_time: Instant,
    requested_duration: String,
    #[serde(default)]
    timeline: Option<RawTimeline>,
}

#[derive(Debug, Deserialize)]
struct RawTimeline {
    #[serde(default)]
    events: Vec<RawEvent>,
}

/// An event names its kind by which key it carries beside `eventTime`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvent {
    event_time: Instant,
    #[serde(default)]
    activated: Option<serde::de::IgnoredAny>,
}

/// The answer was not the grants gcloud prints.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GrantParseError(String);

impl RawGrant {
    fn parse(self) -> Result<Grant, GrantParseError> {
        let requested = seconds(&self.requested_duration).ok_or_else(|| {
            GrantParseError(format!(
                "requestedDuration `{}` is not whole seconds",
                self.requested_duration
            ))
        })?;
        let activated = self
            .timeline
            .iter()
            .flat_map(|timeline| &timeline.events)
            .filter(|event| event.activated.is_some())
            .map(|event| event.event_time)
            .min();
        Ok(Grant {
            state: GrantState::parse(&self.state),
            created: self.create_time,
            requested,
            activated,
        })
    }
}

/// The grants `gcloud pam grants search --format=json` printed: a JSON array, empty when there
/// are none.
pub fn parse_grants(stdout: &[u8]) -> Result<Vec<Grant>, GrantParseError> {
    let raw: Vec<RawGrant> =
        serde_json::from_slice(stdout).map_err(|error| GrantParseError(error.to_string()))?;
    raw.into_iter().map(RawGrant::parse).collect()
}

/// The grant `gcloud pam grants create --format=json` printed: one JSON object.
pub fn parse_created(stdout: &[u8]) -> Result<Grant, GrantParseError> {
    let raw: RawGrant =
        serde_json::from_slice(stdout).map_err(|error| GrantParseError(error.to_string()))?;
    raw.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Instant {
        text.parse().unwrap()
    }

    /// The window runs from activation: an active grant created at 09:00, activated at 09:05,
    /// asked for 4 h, ends at 13:05. A pending one has no end; an unknown state is kept. The
    /// fixture is written from the PAM API reference, not from a live answer.
    /// Mutation: take `create_time` as the start (13:00), or drop unknown states as errors.
    #[test]
    fn grants_parse_and_end_four_hours_after_activation() {
        let grants = parse_grants(include_bytes!("../../tests/fixtures/pam/grants.json")).unwrap();
        let [active, pending, odd] = grants.as_slice() else {
            panic!("three grants")
        };
        assert_eq!(active.state, GrantState::Active);
        assert_eq!(active.activated, Some(at("2026-11-19T09:05:00Z")));
        assert_eq!(active.ends_at(), Some(at("2026-11-19T13:05:00Z")));
        assert_eq!(pending.state, GrantState::ApprovalAwaited);
        assert_eq!(pending.ends_at(), None);
        assert_eq!(pending.state.phase(), Phase::Live);
        assert_eq!(odd.state, GrantState::Unknown("SOMETHING_NEW".to_owned()));
        assert_eq!(odd.state.phase(), Phase::Unclear);
        assert_eq!(parse_grants(b"[]"), Ok(Vec::new()));
        assert!(parse_grants(b"{}").is_err());
    }

    /// Mutation: put `EXPIRED` or `DENIED` among the live states.
    #[test]
    fn only_pending_and_effective_states_are_live() {
        for state in [
            "APPROVAL_AWAITED",
            "SCHEDULED",
            "ACTIVATING",
            "ACTIVE",
            "ENDING",
        ] {
            assert_eq!(GrantState::parse(state).phase(), Phase::Live, "{state}");
        }
        for state in [
            "ENDED",
            "DENIED",
            "EXPIRED",
            "ACTIVATION_FAILED",
            "REVOKED",
            "WITHDRAWN",
        ] {
            assert_eq!(GrantState::parse(state).phase(), Phase::Over, "{state}");
        }
    }
}
