//! Helpers shared by the tests of this module.

use std::path::Path;

use super::RawEvent;
use crate::types::SessionId;

pub(super) const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

pub(super) fn id() -> SessionId {
    SessionId::parse(ID).unwrap()
}

pub(super) use crate::testutil::at;

/// The text of a masked fixture under `tests/fixtures/events/`.
pub(super) fn fixture_text(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/events")
        .join(name);
    std::fs::read_to_string(&path).unwrap()
}

/// A fixture as the log would hold it, arrived at `received_at`.
pub(super) fn fixture(name: &str, received_at: &str) -> RawEvent {
    RawEvent {
        received_at: at(received_at),
        payload: serde_json::from_str(&fixture_text(name)).unwrap(),
    }
}
