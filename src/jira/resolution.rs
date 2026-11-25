//! Whether a ticket is done with, as Jira says it: a ticket is closed once it has a resolution
//! (Done, Won't Do, Duplicate, …), whatever its workflow calls the status it sits in. A status
//! name or category would not do: a "Prod" status is in the In Progress category, and names
//! differ per project.
//!
//! Read one ticket at a time with `acli jira workitem view`, for the tickets the sessions'
//! waits name, and kept in [`FILE_NAME`] under the state directory, so the hook, which must not
//! wait on Jira, can tell a waiting session that its ticket closed.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ACLI, ACLI_TIMEOUT, JiraError, classify_failure, jira_time};
use crate::types::{Instant, JiraKey};
use crate::util::fs::write_private;
use crate::util::runner::{CommandRunner, run_tool};

/// The file in the state directory that holds the tickets read as closed.
pub const FILE_NAME: &str = "ticket-resolutions.json";

/// A closed ticket: how it was resolved, the status it was left in, and when it was resolved
/// when Jira says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub resolution: String,
    pub status: String,
    pub at: Option<Instant>,
}

/// What one read of a ticket said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// No resolution: the ticket is still open, whatever its status.
    Open,
    Closed(Closed),
}

/// Reads `key`'s resolution now.
pub async fn fetch(runner: &dyn CommandRunner, key: &JiraKey) -> Result<Resolution, JiraError> {
    let key_text = key.to_string();
    let args = [
        "jira",
        "workitem",
        "view",
        key_text.as_str(),
        "--json",
        "--fields",
        "status,resolution,resolutiondate",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse(&output.stdout)
}

/// A ticket's resolution in its `view --json` output.
pub fn parse(stdout: &[u8]) -> Result<Resolution, JiraError> {
    let unparseable = |detail: &str| JiraError::Unparseable {
        detail: detail.to_owned(),
    };
    let json: Value =
        serde_json::from_slice(stdout).map_err(|_| unparseable("the output is not JSON"))?;
    let fields = json.get("fields").ok_or_else(|| unparseable("no fields"))?;
    let name_of = |field: &str| {
        fields
            .get(field)
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
    };
    match fields.get("resolution") {
        None => Err(unparseable("no resolution field")),
        Some(Value::Null) => Ok(Resolution::Open),
        Some(_) => Ok(Resolution::Closed(Closed {
            resolution: name_of("resolution")
                .ok_or_else(|| unparseable("a resolution with no name"))?
                .to_owned(),
            status: name_of("status")
                .ok_or_else(|| unparseable("no status name"))?
                .to_owned(),
            at: fields
                .get("resolutiondate")
                .and_then(Value::as_str)
                .and_then(jira_time),
        })),
    }
}

/// The tickets read as closed, by key. A ticket read as open again leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosedTickets(HashMap<JiraKey, Closed>);

/// The saved form: keys as text, parsed again on load.
#[derive(Serialize, Deserialize)]
struct Stored(BTreeMap<String, Closed>);

impl ClosedTickets {
    /// The set saved under `state_dir`; empty when none was saved or it cannot be read, which
    /// only means no wait is known to be over by its ticket.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Stored>(&bytes)
                .inspect_err(|error| {
                    tracing::warn!(path = %path.display(), %error, "the closed tickets are unreadable");
                })
                .ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the closed tickets cannot be read");
                None
            }
        };
        Self(stored.map_or_else(HashMap::new, |Stored(by_key)| {
            by_key
                .into_iter()
                .filter_map(|(key, closed)| Some((JiraKey::parse(&key)?, closed)))
                .collect()
        }))
    }

    pub fn get(&self, key: &JiraKey) -> Option<&Closed> {
        self.0.get(key)
    }

    /// Takes in one read; whether the set changed.
    pub fn record(&mut self, key: JiraKey, resolution: Resolution) -> bool {
        match resolution {
            Resolution::Closed(closed) => {
                self.0.insert(key, closed.clone()).as_ref() != Some(&closed)
            }
            Resolution::Open => self.0.remove(&key).is_some(),
        }
    }

    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        let stored = Stored(
            self.0
                .iter()
                .map(|(key, closed)| (key.to_string(), closed.clone()))
                .collect(),
        );
        let bytes = serde_json::to_vec_pretty(&stored).map_err(std::io::Error::other)?;
        write_private(&state_dir.join(FILE_NAME), &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    fn key(text: &str) -> JiraKey {
        JiraKey::parse(text).unwrap()
    }

    /// A resolved ticket is closed with its resolution, status and time (Jira's `-0500` read
    /// by hand as 20:43:29 UTC); a null resolution is open, even in a done-looking status; an
    /// answer without the field is not read as either.
    /// Mutation: read a missing resolution as open, or the status category as the test.
    #[test]
    fn a_ticket_is_closed_exactly_when_it_has_a_resolution() {
        let closed = parse(include_bytes!(
            "../../tests/fixtures/jira/view-resolved.json"
        ))
        .unwrap();
        assert_eq!(
            closed,
            Resolution::Closed(Closed {
                resolution: "Done".to_owned(),
                status: "Done".to_owned(),
                at: Some("2026-11-16T12:00:00.250Z".parse().unwrap()),
            })
        );
        let open = br#"{"key": "ABC-1", "fields": {"resolution": null,
            "status": {"name": "Prod", "statusCategory": {"key": "indeterminate"}}}}"#;
        assert_eq!(parse(open).unwrap(), Resolution::Open);
        assert!(parse(br#"{"key": "ABC-1", "fields": {"status": {"name": "Done"}}}"#).is_err());
    }

    /// The set survives a save and a load, and a ticket read open again leaves it.
    /// Mutation: ignore an `Open` read (the reopened ticket stays closed).
    #[test]
    fn closed_tickets_are_saved_and_a_reopened_one_leaves() {
        let scratch = Scratch::new("closed-tickets");
        let closed = Closed {
            resolution: "Won't Do".to_owned(),
            status: "Closed".to_owned(),
            at: None,
        };
        let mut tickets = ClosedTickets::load(&scratch);
        assert!(tickets.record(key("ABC-1"), Resolution::Closed(closed.clone())));
        assert!(!tickets.record(key("ABC-1"), Resolution::Closed(closed.clone())));
        tickets.save(&scratch).unwrap();
        let loaded = ClosedTickets::load(&scratch);
        assert_eq!(loaded.get(&key("ABC-1")), Some(&closed));
        let mut loaded = loaded;
        assert!(loaded.record(key("ABC-1"), Resolution::Open));
        assert_eq!(loaded.get(&key("ABC-1")), None);
    }
}
