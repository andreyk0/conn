//! `seen.json`: which cards the human has looked at, and which sessions they pinned.
//!
//! The only state that the user's actions write. Marking a card seen records that card's
//! `since` for its session; a card whose `since` is at or before the recorded one sorts last in
//! the inbox and is not removed, and a later card (a later turn end) is unseen again. Pins are
//! kept in the order they were made, the earliest first, which is the order pinned sessions
//! head every list in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::PinRank;
use crate::types::{Instant, JiraKey, SessionId};
use crate::util::fs::write_private;

pub const SEEN_FILE: &str = "seen.json";

#[derive(Debug, thiserror::Error)]
pub enum SeenError {
    #[error("cannot read {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write {}: {source}", path.display())]
    Unwritable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was not read, so saving would overwrite what is in it.
    #[error("the seen-state file was not read ({0}); it is left as it is")]
    NotRead(String),
    #[error("{} is not the seen-state file: {source}", path.display())]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// The comments on `ticket` up to the one created at `through`, which a card showed as unseen
/// and the human marked seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketMark {
    pub ticket: JiraKey,
    pub through: Instant,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeenStore {
    /// For each session, the `since` of the latest card marked seen.
    seen: HashMap<SessionId, Instant>,
    /// The pinned sessions in the order they were pinned, each once.
    pinned: Vec<SessionId>,
    /// For each ticket, the `created` of the latest comment marked seen: that comment and every
    /// earlier one are seen.
    tickets: HashMap<JiraKey, Instant>,
}

/// The file as written: ordered, so a rewrite of unchanged state is the same bytes. `pinned`
/// is in pin order; a file written before pins kept their order holds them sorted by id, which
/// is read as that order. `tickets` is left out while empty, so a file with no ticket marks is
/// the shape it was before they existed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    #[serde(default)]
    seen: BTreeMap<SessionId, Instant>,
    #[serde(default)]
    pinned: Vec<SessionId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tickets: BTreeMap<String, Instant>,
}

impl SeenStore {
    /// A missing file is an empty store: nothing has been marked yet. Any other failure is an
    /// error and the file is left as it is.
    pub fn load(path: &Path) -> Result<Self, SeenError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::parse(&bytes, path),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SeenError::Unreadable {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// `path` only labels errors.
    pub fn parse(bytes: &[u8], path: &Path) -> Result<Self, SeenError> {
        let malformed = |source| SeenError::Malformed {
            path: path.to_path_buf(),
            source,
        };
        // A derived `Deserialize` also accepts a JSON array for a struct, and `[]` would then
        // read as an empty store and be saved over whatever was there.
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(malformed)?;
        if !value.is_object() {
            return Err(malformed(<serde_json::Error as serde::de::Error>::custom(
                "expected a JSON object",
            )));
        }
        let wire: Wire = serde_json::from_value(value).map_err(malformed)?;
        let tickets = wire
            .tickets
            .into_iter()
            .map(|(text, at)| {
                JiraKey::parse(&text).map(|key| (key, at)).ok_or_else(|| {
                    malformed(<serde_json::Error as serde::de::Error>::custom(format!(
                        "`{text}` is not a ticket key"
                    )))
                })
            })
            .collect::<Result<HashMap<JiraKey, Instant>, SeenError>>()?;
        // A session listed twice (the file is the human's to edit) keeps its first place.
        let mut listed = HashSet::new();
        Ok(Self {
            seen: wire.seen.into_iter().collect(),
            pinned: wire
                .pinned
                .into_iter()
                .filter(|id| listed.insert(id.clone()))
                .collect(),
            tickets,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), SeenError> {
        let wire = Wire {
            seen: self.seen.iter().map(|(id, at)| (id.clone(), *at)).collect(),
            pinned: self.pinned.clone(),
            tickets: self
                .tickets
                .iter()
                .map(|(key, at)| (key.to_string(), *at))
                .collect(),
        };
        let unwritable = |source| SeenError::Unwritable {
            path: path.to_path_buf(),
            source,
        };
        let mut bytes =
            serde_json::to_vec_pretty(&wire).map_err(|e| unwritable(std::io::Error::other(e)))?;
        bytes.push(b'\n');
        write_private(path, &bytes).map_err(unwritable)
    }

    /// The `since` of the latest card of the session marked seen: that card and every earlier
    /// one are seen.
    pub fn seen_through(&self, session: &SessionId) -> Option<Instant> {
        self.seen.get(session).copied()
    }

    /// Where the session stands among the pins, the earliest pinned first; `None` when it is
    /// not pinned.
    pub fn pin_rank(&self, session: &SessionId) -> Option<PinRank> {
        self.pinned
            .iter()
            .position(|pinned| pinned == session)
            .map(PinRank::at)
    }

    /// Marks the session's card with this `since` seen; a mark of an earlier card never moves a
    /// later one back.
    pub fn mark_seen(&mut self, session: SessionId, card_since: Instant) {
        let latest = self.seen.entry(session).or_insert(card_since);
        *latest = (*latest).max(card_since);
    }

    /// The `created` of the latest comment on the ticket marked seen.
    pub fn ticket_seen_through(&self, ticket: &JiraKey) -> Option<Instant> {
        self.tickets.get(ticket).copied()
    }

    /// Marks the ticket's comments up to `through` seen; an earlier mark never moves a later
    /// one back.
    pub fn mark_ticket_seen(&mut self, ticket: JiraKey, through: Instant) {
        let latest = self.tickets.entry(ticket).or_insert(through);
        *latest = (*latest).max(through);
    }

    /// Pins the session after every earlier pin; pinning a pinned session keeps its place.
    pub fn pin(&mut self, session: SessionId) {
        if !self.pinned.contains(&session) {
            self.pinned.push(session);
        }
    }

    pub fn unpin(&mut self, session: &SessionId) {
        self.pinned.retain(|pinned| pinned != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Scratch, at};

    const ID_A: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    const ID_B: &str = "11111111-2222-4333-8444-555555555555";

    fn id(text: &str) -> SessionId {
        SessionId::parse(text).unwrap()
    }

    /// The expected file is written out by hand.
    /// Mutation: drop `pinned` from `Wire` in `save`, or write `seen` unsorted (`HashMap` order).
    #[test]
    fn save_writes_the_documented_shape_and_load_reads_it_back() {
        let dir = Scratch::new("roundtrip");
        let path = dir.join(SEEN_FILE);
        let mut store = SeenStore::default();
        store.mark_seen(id(ID_B), at("2026-10-28T10:00:00Z"));
        store.mark_seen(id(ID_A), at("2026-10-28T09:00:00Z"));
        store.pin(id(ID_A));
        store.save(&path).unwrap();
        let expected = format!(
            "{{\n  \"seen\": {{\n    \"{ID_A}\": \"2026-10-28T09:00:00Z\",\n    \"{ID_B}\": \"2026-10-28T10:00:00Z\"\n  }},\n  \"pinned\": [\n    \"{ID_A}\"\n  ]\n}}\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(SeenStore::load(&path).unwrap(), store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A state directory that is not there yet is created private by the save.
    /// Mutation: `save` calls `write_atomic` (the directory is missing, so it fails).
    #[test]
    fn save_creates_a_missing_state_directory_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("mkdir");
        let path = dir.join("state").join(SEEN_FILE);
        SeenStore::default().save(&path).unwrap();
        let mode = std::fs::metadata(dir.join("state"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: `save` writes in place (`std::fs::write`) -> same inode -> red.
    #[test]
    fn save_replaces_the_file_atomically() {
        use std::os::unix::fs::MetadataExt;
        let dir = Scratch::new("atomic");
        let path = dir.join(SEEN_FILE);
        let mut store = SeenStore::default();
        store.save(&path).unwrap();
        let first = std::fs::metadata(&path).unwrap().ino();
        store.pin(id(ID_A));
        store.save(&path).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), first);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: replace `max` with the new value.
    #[test]
    fn an_earlier_mark_never_moves_a_later_one_back() {
        let mut store = SeenStore::default();
        store.mark_seen(id(ID_A), at("2026-10-28T10:00:00Z"));
        store.mark_seen(id(ID_A), at("2026-10-28T09:00:00Z"));
        assert_eq!(
            store.seen_through(&id(ID_A)),
            Some(at("2026-10-28T10:00:00Z"))
        );
        assert_eq!(store.seen_through(&id(ID_B)), None);
    }

    /// Pins rank in the order they were made; pinning again keeps a pin's place, and unpinning
    /// closes the gap.
    /// Mutation: push a repeated pin again (B would rank 2 after the repeat), or insert new pins
    /// at the front.
    #[test]
    fn pins_rank_in_the_order_they_were_made() {
        const ID_C: &str = "22222222-3333-4444-8555-666666666666";
        let mut store = SeenStore::default();
        store.pin(id(ID_B));
        store.pin(id(ID_A));
        store.pin(id(ID_C));
        store.pin(id(ID_B));
        let ranks = |store: &SeenStore| [ID_A, ID_B, ID_C].map(|text| store.pin_rank(&id(text)));
        assert_eq!(
            ranks(&store),
            [
                Some(PinRank::at(1)),
                Some(PinRank::at(0)),
                Some(PinRank::at(2))
            ]
        );
        store.unpin(&id(ID_A));
        assert_eq!(
            ranks(&store),
            [None, Some(PinRank::at(0)), Some(PinRank::at(1))]
        );
    }

    /// A ticket mark never moves back, is saved under `tickets` by key, and is read back; a
    /// key that is not a ticket key makes the file malformed. The expected file is written out
    /// by hand.
    /// Mutation: replace `max` with the new mark, drop `tickets` from `save`, or skip the key's
    /// parse on load.
    #[test]
    fn ticket_marks_keep_the_latest_and_round_trip() {
        let dir = Scratch::new("tickets");
        let path = dir.join(SEEN_FILE);
        let key = JiraKey::parse("ABC-123").unwrap();
        let mut store = SeenStore::default();
        store.mark_ticket_seen(key.clone(), at("2026-11-06T10:00:00Z"));
        store.mark_ticket_seen(key.clone(), at("2026-11-06T09:00:00Z"));
        assert_eq!(
            store.ticket_seen_through(&key),
            Some(at("2026-11-06T10:00:00Z"))
        );
        store.save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"seen\": {},\n  \"pinned\": [],\n  \"tickets\": {\n    \"ABC-123\": \"2026-11-06T10:00:00Z\"\n  }\n}\n"
        );
        assert_eq!(SeenStore::load(&path).unwrap(), store);
        std::fs::write(&path, r#"{"tickets": {"nope": "2026-11-06T10:00:00Z"}}"#).unwrap();
        assert!(matches!(
            SeenStore::load(&path),
            Err(SeenError::Malformed { .. })
        ));
    }

    /// The file keeps pin order, not id order, and a session listed twice keeps its first
    /// place. The expected file is written out by hand.
    /// Mutation: sort `pinned` on save, or keep the last of two listings on load.
    #[test]
    fn the_file_keeps_pin_order_and_its_first_listing() {
        let dir = Scratch::new("pin-order");
        let path = dir.join(SEEN_FILE);
        let mut store = SeenStore::default();
        store.pin(id(ID_B));
        store.pin(id(ID_A));
        store.save(&path).unwrap();
        let expected = format!(
            "{{\n  \"seen\": {{}},\n  \"pinned\": [\n    \"{ID_B}\",\n    \"{ID_A}\"\n  ]\n}}\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(SeenStore::load(&path).unwrap(), store);

        std::fs::write(
            &path,
            format!("{{\"pinned\": [\"{ID_A}\", \"{ID_B}\", \"{ID_A}\"]}}"),
        )
        .unwrap();
        let loaded = SeenStore::load(&path).unwrap();
        assert_eq!(loaded.pin_rank(&id(ID_A)), Some(PinRank::at(0)));
        assert_eq!(loaded.pin_rank(&id(ID_B)), Some(PinRank::at(1)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A corrupt file is an error, and `load` never rewrites it.
    /// Mutation: turn `Malformed` into `Ok(default)`.
    #[test]
    fn a_corrupt_file_is_an_error_and_stays_as_it_was() {
        let dir = Scratch::new("corrupt");
        let path = dir.join(SEEN_FILE);
        for text in [
            "{",
            "[]",
            "{\"seen\": {\"nope\": \"2026-10-28T10:00:00Z\"}}",
            "{\"extra\": 1}",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(
                matches!(SeenStore::load(&path), Err(SeenError::Malformed { .. })),
                "{text}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        assert_eq!(
            SeenStore::load(&dir.join("absent.json")).unwrap(),
            SeenStore::default()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
