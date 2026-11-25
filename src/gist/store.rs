//! The gists written so far, by session, and when a card's run is due.
//!
//! Runs cost money, so they happen only while someone looks: while a page was viewed in the
//! last [`VIEWED_WITHIN`]. A card is run once; a failed run is tried again after
//! [`RETRY_AFTER`], and a new card of the session is run at once.
//!
//! The written gists are kept in `gists.json` in the state directory, so a restart neither
//! pays for them again nor loses the previous gist the next run keeps to. The file is derived:
//! one that cannot be read is replaced, since running again rebuilds it.

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

use super::{Ask, Gist, GistError, Input};
use crate::model::SourceState;
use crate::types::{Instant, SessionId};
use crate::util::fs::write_private;
use crate::util::redact::Redacted;

pub const FILE: &str = "gists.json";

/// How long after the last page view runs go on.
pub const VIEWED_WITHIN: SignedDuration = SignedDuration::from_hours(1);

/// How long a failed card waits before it is run again: a connector or quota blip is over by
/// then, and a run that fails every time costs one call per card per ten minutes.
const RETRY_AFTER: SignedDuration = SignedDuration::from_mins(10);

/// The most sessions kept, the most recently written first: far more than are ever on cards.
const SESSIONS_KEPT: usize = 500;

/// A gist and the card it was written for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The `since` of the card it was written for.
    pub covers: Instant,
    pub at: Instant,
    pub gist: Gist,
}

/// A run that wrote nothing, and the card it was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub covers: Instant,
    pub at: Instant,
    pub error: GistError,
}

/// What the latest run for a session gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Written(Written),
    /// The latest run failed; `earlier` is the last gist written before it.
    Failed {
        failure: Failure,
        earlier: Option<Written>,
    },
}

impl Entry {
    /// The latest gist written for the session, whichever card it was for.
    pub const fn written(&self) -> Option<&Written> {
        match self {
            Self::Written(written) => Some(written),
            Self::Failed { earlier, .. } => earlier.as_ref(),
        }
    }

    const fn at(&self) -> Instant {
        match self {
            Self::Written(written) => written.at,
            Self::Failed { failure, .. } => failure.at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    entries: HashMap<SessionId, Entry>,
    viewed: Option<Instant>,
    health: SourceState,
}

/// The file as written: ordered, so a rewrite of unchanged state is the same bytes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    sessions: BTreeMap<SessionId, WireGist>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGist {
    covers: Instant,
    at: Instant,
    gist: String,
    ask: Ask,
}

impl Store {
    /// The gists in `state_dir`. A missing file is an empty store; one that cannot be read is
    /// an empty store whose health says why.
    pub fn load(state_dir: &Path, now: Instant) -> Self {
        let path = state_dir.join(FILE);
        let (entries, health) = match read(&path) {
            Ok(entries) => (entries, SourceState::NeverRead),
            Err(reason) => {
                tracing::warn!(%reason, "the gists start empty");
                (HashMap::new(), SourceState::Failed { at: now, reason })
            }
        };
        Self {
            path,
            entries,
            viewed: None,
            health,
        }
    }

    pub fn get(&self, session: &SessionId) -> Option<&Entry> {
        self.entries.get(session)
    }

    /// The latest run's outcome, or why the file could not be read or written.
    pub const fn health(&self) -> &SourceState {
        &self.health
    }

    pub const fn viewed(&mut self, now: Instant) {
        self.viewed = Some(now);
    }

    /// A page was viewed within [`VIEWED_WITHIN`].
    pub fn is_watched(&self, now: Instant) -> bool {
        self.viewed
            .is_some_and(|at| now.duration_since(at) <= VIEWED_WITHIN)
    }

    /// Whether the card of `session` that has waited since `since` is to be run at `now`.
    pub fn is_due(&self, session: &SessionId, since: Instant, now: Instant) -> bool {
        match self.entries.get(session) {
            None => true,
            Some(Entry::Written(written)) => written.covers < since,
            Some(Entry::Failed { failure, .. }) => {
                failure.covers < since || now.duration_since(failure.at) >= RETRY_AFTER
            }
        }
    }

    /// Records what the run for `input` gave at `now`, and writes the file.
    pub fn record(&mut self, input: &Input, outcome: Result<Gist, GistError>, now: Instant) {
        let entry = match outcome {
            Ok(gist) => {
                self.health = SourceState::Ok { at: now };
                Entry::Written(Written {
                    covers: input.covers,
                    at: now,
                    gist,
                })
            }
            Err(error) => {
                tracing::warn!(session = %input.session, %error, "no gist was written");
                self.health = SourceState::Failed {
                    at: now,
                    reason: error.to_string(),
                };
                Entry::Failed {
                    earlier: self
                        .entries
                        .get(&input.session)
                        .and_then(Entry::written)
                        .cloned(),
                    failure: Failure {
                        covers: input.covers,
                        at: now,
                        error,
                    },
                }
            }
        };
        self.entries.insert(input.session.clone(), entry);
        self.forget_oldest();
        if let Err(error) = self.save() {
            tracing::warn!(%error, "the gists could not be saved");
            self.health = SourceState::Failed {
                at: now,
                reason: format!("cannot write {}: {error}", self.path.display()),
            };
        }
    }

    fn forget_oldest(&mut self) {
        let excess = self.entries.len().saturating_sub(SESSIONS_KEPT);
        if excess == 0 {
            return;
        }
        let mut by_age: Vec<(Instant, SessionId)> = self
            .entries
            .iter()
            .map(|(id, entry)| (entry.at(), id.clone()))
            .collect();
        by_age.sort();
        for (_, id) in by_age.into_iter().take(excess) {
            self.entries.remove(&id);
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let wire = Wire {
            sessions: self
                .entries
                .iter()
                .filter_map(|(id, entry)| {
                    entry.written().map(|written| {
                        (
                            id.clone(),
                            WireGist {
                                covers: written.covers,
                                at: written.at,
                                gist: written.gist.text.as_str().to_owned(),
                                ask: written.gist.ask,
                            },
                        )
                    })
                })
                .collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&wire).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        write_private(&self.path, &bytes)
    }
}

/// The written gists in the file at `path`; a missing file holds none.
fn read(path: &Path) -> Result<HashMap<SessionId, Entry>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let wire: Wire = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not the gists file: {e}", path.display()))?;
    Ok(wire
        .sessions
        .into_iter()
        .map(|(id, wire)| {
            let written = Written {
                covers: wire.covers,
                at: wire.at,
                gist: Gist {
                    text: Redacted::of(&wire.gist),
                    ask: wire.ask,
                },
            };
            (id, Entry::Written(written))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    //! When a card is due, what a failure keeps, and the file's round trip, with hand-picked
    //! instants.

    use super::*;
    use crate::testutil::{Scratch, secs as at, session_id as id};
    use crate::util::oneshot::OneshotError;

    fn input(n: u8, covers: i64) -> Input {
        Input {
            session: id(n),
            covers: at(covers),
            prompts: Vec::new(),
            latest: Redacted::of(""),
            previous: None,
        }
    }

    fn gist(text: &str) -> Gist {
        Gist {
            text: Redacted::of(text),
            ask: Ask::Decide,
        }
    }

    fn failed() -> GistError {
        GistError::Run(OneshotError::RunFailed {
            detail: "overloaded".to_owned(),
        })
    }

    /// Mutation: run a written card again (`<=`), retry a failure at once or never, or keep
    /// running a failed card after a later one appears only after the retry wait.
    #[test]
    fn a_card_is_run_once_and_a_failure_again_after_a_wait() {
        let dir = Scratch::new("due");
        let mut store = Store::load(&dir, at(0));
        assert!(store.is_due(&id(1), at(10), at(10)));
        store.record(&input(1, 10), Ok(gist("g")), at(11));
        assert!(!store.is_due(&id(1), at(10), at(5_000)));
        assert!(store.is_due(&id(1), at(20), at(21)));

        store.record(&input(1, 20), Err(failed()), at(21));
        assert!(!store.is_due(&id(1), at(20), at(22)));
        assert!(store.is_due(&id(1), at(20), at(21 + 600)));
        assert!(store.is_due(&id(1), at(30), at(31)));
        assert!(matches!(store.health(), SourceState::Failed { .. }));
    }

    /// A failure keeps the gist written before it. Mutation: drop `earlier`.
    #[test]
    fn a_failure_keeps_the_earlier_gist() {
        let dir = Scratch::new("earlier");
        let mut store = Store::load(&dir, at(0));
        store.record(&input(1, 10), Ok(gist("before")), at(11));
        store.record(&input(1, 20), Err(failed()), at(21));
        assert_eq!(
            store
                .get(&id(1))
                .and_then(Entry::written)
                .map(|w| w.gist.text.as_str()),
            Some("before")
        );
    }

    /// What was written survives a restart, a failure's earlier gist included; a file that is
    /// not the gists file is an empty store that says so. Mutation: save only `Written`
    /// entries (session 2's gist is lost), or treat a malformed file as missing.
    #[test]
    fn the_written_gists_survive_a_restart() {
        let dir = Scratch::new("restart");
        let mut store = Store::load(&dir, at(0));
        store.record(&input(1, 10), Ok(gist("one")), at(11));
        store.record(&input(2, 10), Ok(gist("two")), at(12));
        store.record(&input(2, 20), Err(failed()), at(21));

        let again = Store::load(&dir, at(30));
        assert_eq!(again.health(), &SourceState::NeverRead);
        let text = |n| {
            again
                .get(&id(n))
                .and_then(Entry::written)
                .map(|w| (w.covers, w.gist.text.as_str().to_owned(), w.gist.ask))
        };
        assert_eq!(text(1), Some((at(10), "one".to_owned(), Ask::Decide)));
        assert_eq!(text(2), Some((at(10), "two".to_owned(), Ask::Decide)));

        std::fs::write(dir.join(FILE), b"[]").unwrap();
        let broken = Store::load(&dir, at(40));
        assert!(broken.get(&id(1)).is_none());
        assert!(matches!(broken.health(), SourceState::Failed { .. }));
    }

    /// Mutation: never forget, or forget the newest.
    #[test]
    fn the_oldest_sessions_are_forgotten_past_the_cap() {
        let dir = Scratch::new("cap");
        let mut store = Store::load(&dir, at(0));
        for n in 0..=SESSIONS_KEPT {
            let session = SessionId::parse(&format!("{n:08}-0000-4000-8000-000000000000")).unwrap();
            let covers = i64::try_from(n).unwrap();
            let input = Input {
                session,
                ..input(0, covers)
            };
            store.record(&input, Ok(gist("g")), at(covers));
        }
        assert_eq!(store.entries.len(), SESSIONS_KEPT);
        let oldest = SessionId::parse("00000000-0000-4000-8000-000000000000").unwrap();
        assert!(store.get(&oldest).is_none());
        let newest =
            SessionId::parse(&format!("{SESSIONS_KEPT:08}-0000-4000-8000-000000000000")).unwrap();
        assert!(store.get(&newest).is_some());
    }

    /// Mutation: drop the view window, or compare it the wrong way round.
    #[test]
    fn runs_happen_only_while_someone_looks() {
        let dir = Scratch::new("viewed");
        let mut store = Store::load(&dir, at(0));
        assert!(!store.is_watched(at(0)));
        store.viewed(at(100));
        assert!(store.is_watched(at(100 + 3_600)));
        assert!(!store.is_watched(at(100 + 3_601)));
    }
}
