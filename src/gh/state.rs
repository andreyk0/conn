//! `gh pr view --json …` output, parsed into [`PrState`].
//!
//! Parsing is strict about what the card cannot do without (title, lifecycle, times, URL)
//! and lenient about what GitHub is free to extend (review decisions, check shapes): an
//! unrecognised review decision is kept as text, an unrecognised check makes the summary
//! `Unknown`, never `Passing`.

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use super::{GhError, PullRequestRef};
use crate::types::{Instant, SafeUrl};

/// The fields `gh pr view` is asked for; the one list both the command line and the parse use.
pub(super) const JSON_FIELDS: &str =
    "number,title,state,isDraft,mergedAt,reviewDecision,statusCheckRollup,updatedAt,url";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrState {
    pub title: String,
    pub state: PrLifecycle,
    pub readiness: Readiness,
    pub review: ReviewDecision,
    pub checks: Checks,
    pub updated_at: Instant,
    pub url: SafeUrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    Open,
    Merged { at: Instant },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Draft,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    /// The repository requires no review, so GitHub states no decision.
    NoneRequired,
    /// A decision this tool does not know, as GitHub wrote it.
    Other(String),
}

/// The checks on the latest commit, summarised. When several kinds are present the
/// summary names the worst: failing, then pending, then unknown, then passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    Passing,
    Failing {
        count: u32,
    },
    Pending {
        count: u32,
    },
    NoChecks,
    /// The rollup was absent, not a list, or held a check of a shape not understood.
    Unknown,
}

/// The wire shape, kept private: `isDraft: bool` and `state: "MERGED"` become domain types here.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    number: u32,
    title: String,
    state: String,
    is_draft: bool,
    merged_at: Option<Timestamp>,
    review_decision: Option<String>,
    status_check_rollup: Option<Value>,
    updated_at: Timestamp,
    url: String,
}

/// Parses the stdout of the `gh pr view` call made for `pr`.
pub fn parse_pr_state(stdout: &[u8], pr: &PullRequestRef) -> Result<PrState, GhError> {
    let unparseable = |detail: String| GhError::Unparseable { detail };
    let wire: Wire = serde_json::from_slice(stdout).map_err(|e| unparseable(e.to_string()))?;
    if wire.number != pr.number().get() {
        return Err(unparseable(format!(
            "asked for pull request {}, got {}",
            pr.number(),
            wire.number
        )));
    }
    let state = match (wire.state.as_str(), wire.merged_at) {
        ("OPEN", _) => PrLifecycle::Open,
        ("CLOSED", _) => PrLifecycle::Closed,
        ("MERGED", Some(at)) => PrLifecycle::Merged { at },
        ("MERGED", None) => return Err(unparseable("state MERGED without mergedAt".to_owned())),
        (other, _) => return Err(unparseable(format!("unknown state {other:?}"))),
    };
    let url = SafeUrl::parse(&wire.url).map_err(|e| unparseable(e.to_string()))?;
    Ok(PrState {
        title: wire.title,
        state,
        readiness: if wire.is_draft {
            Readiness::Draft
        } else {
            Readiness::Ready
        },
        review: review_decision(wire.review_decision.as_deref()),
        checks: wire
            .status_check_rollup
            .as_ref()
            .map_or(Checks::Unknown, summarise_checks),
        updated_at: wire.updated_at,
        url,
    })
}

fn review_decision(wire: Option<&str>) -> ReviewDecision {
    // GitHub states "no decision" as an empty string or null.
    match wire {
        None | Some("") => ReviewDecision::NoneRequired,
        Some("APPROVED") => ReviewDecision::Approved,
        Some("CHANGES_REQUESTED") => ReviewDecision::ChangesRequested,
        Some("REVIEW_REQUIRED") => ReviewDecision::ReviewRequired,
        Some(other) => ReviewDecision::Other(other.to_owned()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed,
    Pending,
    Unrecognised,
}

fn summarise_checks(rollup: &Value) -> Checks {
    let Some(checks) = rollup.as_array() else {
        return Checks::Unknown;
    };
    let count = |wanted: Verdict| {
        let n = checks.iter().filter(|c| verdict(c) == wanted).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    };
    match (
        count(Verdict::Failed),
        count(Verdict::Pending),
        count(Verdict::Unrecognised),
    ) {
        (0, 0, 0) if checks.is_empty() => Checks::NoChecks,
        (0, 0, 0) => Checks::Passing,
        (count, _, _) if count > 0 => Checks::Failing { count },
        (_, count, _) if count > 0 => Checks::Pending { count },
        _ => Checks::Unknown,
    }
}

/// A check run reports `status` and, once completed, `conclusion`; a commit status
/// reports `state`.
fn verdict(check: &Value) -> Verdict {
    let field = |name: &str| check.get(name).and_then(Value::as_str);
    match (field("status"), field("conclusion"), field("state")) {
        (Some("COMPLETED"), Some(conclusion), _) => match conclusion {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => Verdict::Passed,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" => Verdict::Failed,
            _ => Verdict::Unrecognised,
        },
        (Some("QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED"), _, _) => {
            Verdict::Pending
        }
        (None, _, Some(state)) => match state {
            "SUCCESS" => Verdict::Passed,
            "PENDING" | "EXPECTED" => Verdict::Pending,
            "FAILURE" | "ERROR" => Verdict::Failed,
            _ => Verdict::Unrecognised,
        },
        _ => Verdict::Unrecognised,
    }
}

#[cfg(test)]
mod tests {
    //! Each fixture under `tests/fixtures/gh` is a masked `gh pr view --json` answer; the
    //! expected values are written out by hand from the fixture's contents. Variations on
    //! a fixture edit one field of its JSON, so a test names exactly what differs.

    use super::*;
    use crate::testutil::at;

    const OPEN_PASSING: &[u8] = include_bytes!("../../tests/fixtures/gh/open_passing.json");
    const DRAFT: &[u8] = include_bytes!("../../tests/fixtures/gh/draft.json");
    const MERGED: &[u8] = include_bytes!("../../tests/fixtures/gh/merged.json");
    const CHANGES_REQUESTED: &[u8] =
        include_bytes!("../../tests/fixtures/gh/changes_requested.json");
    const FAILING: &[u8] = include_bytes!("../../tests/fixtures/gh/failing.json");
    const PENDING: &[u8] = include_bytes!("../../tests/fixtures/gh/pending.json");
    const UNKNOWN_ROLLUP: &[u8] = include_bytes!("../../tests/fixtures/gh/unknown_rollup.json");
    const NO_CHECKS: &[u8] = include_bytes!("../../tests/fixtures/gh/no_checks.json");

    fn pr() -> PullRequestRef {
        PullRequestRef::new("example-org", "example-repo", 42).unwrap()
    }

    fn parse(json: &[u8]) -> PrState {
        parse_pr_state(json, &pr()).unwrap()
    }

    /// The open-with-passing-checks fixture with one top-level field replaced.
    fn open_with(field: &str, value: &Value) -> Vec<u8> {
        let mut doc: Value = serde_json::from_slice(OPEN_PASSING).unwrap();
        doc[field] = value.clone();
        serde_json::to_vec(&doc).unwrap()
    }

    fn detail(result: Result<PrState, GhError>) -> String {
        match result {
            Err(GhError::Unparseable { detail }) => detail,
            // The text then fails the caller's `contains`, and the failure message shows it.
            other => format!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn open_with_passing_checks() {
        assert_eq!(
            parse(OPEN_PASSING),
            PrState {
                title: "Placeholder pull request title".to_owned(),
                state: PrLifecycle::Open,
                readiness: Readiness::Ready,
                review: ReviewDecision::ReviewRequired,
                checks: Checks::Passing,
                updated_at: at("2026-10-20T09:10:00Z"),
                url: SafeUrl::parse("https://github.com/example-org/example-repo/pull/42").unwrap(),
            }
        );
    }

    #[test]
    fn a_draft_is_not_ready_and_an_empty_review_decision_is_none_required() {
        let state = parse(DRAFT);
        assert_eq!(state.readiness, Readiness::Draft);
        assert_eq!(state.review, ReviewDecision::NoneRequired);
    }

    #[test]
    fn a_merged_pull_request_carries_when() {
        let state = parse(MERGED);
        assert_eq!(
            state.state,
            PrLifecycle::Merged {
                at: at("2026-10-20T09:12:00Z")
            }
        );
        assert_eq!(state.review, ReviewDecision::Approved);
    }

    #[test]
    fn changes_requested() {
        assert_eq!(
            parse(CHANGES_REQUESTED).review,
            ReviewDecision::ChangesRequested
        );
    }

    #[test]
    fn failing_counts_failures_of_every_check_type_and_outranks_pending() {
        // FAILURE and TIMED_OUT check runs and an ERROR commit status; one run is still going.
        assert_eq!(parse(FAILING).checks, Checks::Failing { count: 3 });
    }

    #[test]
    fn pending_counts_queued_and_running_checks() {
        assert_eq!(parse(PENDING).checks, Checks::Pending { count: 2 });
    }

    #[test]
    fn a_check_of_an_unknown_shape_is_unknown_never_passing() {
        assert_eq!(parse(UNKNOWN_ROLLUP).checks, Checks::Unknown);
    }

    #[test]
    fn an_empty_rollup_is_no_checks_and_a_missing_or_malformed_one_is_unknown() {
        assert_eq!(parse(NO_CHECKS).checks, Checks::NoChecks);
        for rollup in [Value::Null, Value::from("oops"), serde_json::json!({})] {
            let json = open_with("statusCheckRollup", &rollup);
            assert_eq!(parse(&json).checks, Checks::Unknown, "{rollup}");
        }
    }

    #[test]
    fn a_known_failure_is_reported_beside_an_unknown_check() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"__typename": "Hologram"},
        ]);
        let json = open_with("statusCheckRollup", &rollup);
        assert_eq!(parse(&json).checks, Checks::Failing { count: 1 });
    }

    #[test]
    fn a_completed_run_with_an_unlisted_conclusion_is_unknown() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "VAPORISED"},
        ]);
        let json = open_with("statusCheckRollup", &rollup);
        assert_eq!(parse(&json).checks, Checks::Unknown);
    }

    #[test]
    fn an_unlisted_review_decision_is_kept_as_written() {
        let json = open_with("reviewDecision", &Value::from("SOMETHING_NEW"));
        assert_eq!(
            parse(&json).review,
            ReviewDecision::Other("SOMETHING_NEW".to_owned())
        );
    }

    #[test]
    fn a_null_review_decision_is_none_required() {
        let json = open_with("reviewDecision", &Value::Null);
        assert_eq!(parse(&json).review, ReviewDecision::NoneRequired);
    }

    #[test]
    fn a_closed_pull_request_is_closed() {
        let json = open_with("state", &Value::from("CLOSED"));
        assert_eq!(parse(&json).state, PrLifecycle::Closed);
    }

    #[test]
    fn answers_that_cannot_be_trusted_are_unparseable_not_absent() {
        let merged_without_time = {
            let mut doc: Value = serde_json::from_slice(MERGED).unwrap();
            doc["mergedAt"] = Value::Null;
            serde_json::to_vec(&doc).unwrap()
        };
        let cases: [(&str, Vec<u8>, &str); 6] = [
            ("empty output", Vec::new(), "EOF"),
            ("not json", b"gh: boom".to_vec(), "expected"),
            (
                "other pull request",
                open_with("number", &Value::from(43)),
                "got 43",
            ),
            (
                "unknown state",
                open_with("state", &Value::from("LIMBO")),
                "LIMBO",
            ),
            ("merged without time", merged_without_time, "mergedAt"),
            (
                "unsafe url",
                open_with("url", &Value::from("javascript:alert(1)")),
                "not a safe https URL",
            ),
        ];
        for (name, json, expected) in cases {
            let shown = detail(parse_pr_state(&json, &pr()));
            assert!(shown.contains(expected), "{name}: {shown}");
        }
    }
}
