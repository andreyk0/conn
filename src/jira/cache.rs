//! The last answer `acli` gave, with the age that lets a page show a stale list as stale.
//!
//! A failed read never erases the last good list and never passes for one: the cache keeps
//! both, and [`Reading`] hands both to the renderer. The last good list is saved under the state
//! directory, so a restart shows it with its age. Time is passed in, so the cache makes no clock
//! reads of its own.

use std::io;
use std::path::{Path, PathBuf};

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

use super::JiraError;
use super::item::{Level, Person, Status, WorkItem, WorkItemKey};
use crate::types::{Age, Instant, JiraPrefix};
use crate::util::fs::write_private;

/// The file in the state directory that holds the last good list.
pub const FILE_NAME: &str = "jira-work-items.json";

/// The name a corrupt file is moved to, so it can be looked at and is not overwritten.
const CORRUPT_FILE_NAME: &str = "jira-work-items.json.corrupt";

/// How long an answer, good or not, is served before it is asked for again. A person's
/// assignments change on the scale of hours, and every read is an authenticated REST call.
const DEFAULT_TTL: SignedDuration = SignedDuration::from_mins(10);

/// Why the saved copy of the list could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheFileError {
    /// The file exists and could not be read; it is left where it is.
    #[error("cannot read {}: {detail}", path.display())]
    Unreadable { path: PathBuf, detail: String },
    /// The file was read and is not what this tool wrote. `moved_to` is where it was set aside
    /// untouched; `None` when it could not be moved and stays in place.
    #[error("{} is corrupt: {detail}", path.display())]
    Corrupt {
        path: PathBuf,
        detail: String,
        moved_to: Option<PathBuf>,
    },
    #[error("cannot save {}: {detail}", path.display())]
    Write { path: PathBuf, detail: String },
}

/// A good list and when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub items: Vec<WorkItem>,
    pub read_at: Instant,
}

/// A failed read and when it happened.
#[derive(Debug, Clone)]
pub struct Failed {
    pub error: JiraError,
    pub at: Instant,
}

/// What the cache knows.
#[derive(Debug, Clone)]
pub enum Reading {
    /// Never asked, and nothing saved.
    Unread,
    /// A good list; if a later attempt failed, that failure is `failure` and is shown beside
    /// the list, whose age is how stale it is.
    Value {
        observed: Observed,
        age: Age,
        failure: Option<Failed>,
    },
    /// Asked, and no attempt has succeeded.
    Failure(Failed),
}

#[derive(Debug)]
pub struct Cache {
    ttl: SignedDuration,
    path: PathBuf,
    good: Option<Observed>,
    /// Set by a failed attempt; cleared by the next success.
    failure: Option<Failed>,
    /// Set by [`Cache::request_refresh`] and by a load that failed; cleared by the next attempt.
    refresh_requested: bool,
}

/// The saved form of the good list. Keys are text and classified again on load, so the saved
/// file does not depend on which prefixes were configured when it was written.
#[derive(Serialize, Deserialize)]
struct Stored {
    read_at: Instant,
    items: Vec<StoredItem>,
}

#[derive(Serialize, Deserialize)]
struct StoredItem {
    key: String,
    summary: String,
    status: Status,
    issue_type: String,
    /// Absent from a file written before the level was read; such an item is an
    /// [`Level::Item`] until the next read, a few minutes later, says otherwise.
    #[serde(default)]
    level: Level,
    priority: Option<String>,
    creator: Option<Person>,
    assignee: Option<Person>,
    updated: Option<Instant>,
    created: Option<Instant>,
    description: Option<String>,
}

impl StoredItem {
    fn from_item(item: &WorkItem) -> Self {
        Self {
            key: item.key.to_string(),
            summary: item.summary.clone(),
            status: item.status.clone(),
            issue_type: item.issue_type.clone(),
            level: item.level,
            priority: item.priority.clone(),
            creator: item.creator.clone(),
            assignee: item.assignee.clone(),
            updated: item.updated,
            created: item.created,
            description: item.description.clone(),
        }
    }

    fn into_item(self, prefixes: &[JiraPrefix]) -> WorkItem {
        WorkItem {
            key: WorkItemKey::classify(&self.key, prefixes),
            summary: self.summary,
            status: self.status,
            issue_type: self.issue_type,
            level: self.level,
            priority: self.priority,
            creator: self.creator,
            assignee: self.assignee,
            updated: self.updated,
            created: self.created,
            description: self.description,
        }
    }
}

impl Cache {
    /// A cache backed by `<state_dir>/jira-work-items.json`, holding what that file holds.
    ///
    /// A missing file is an empty cache. A file that cannot be read, or is not what this tool
    /// wrote, is a visible failure ([`JiraError::CacheFile`], at `now`) and the cache is due at
    /// once; a corrupt file is moved to `jira-work-items.json.corrupt` and never overwritten
    /// by the next save.
    pub fn load(state_dir: &Path, prefixes: &[JiraPrefix], now: Instant) -> Self {
        Self::load_with(state_dir, prefixes, now, DEFAULT_TTL)
    }

    fn load_with(
        state_dir: &Path,
        prefixes: &[JiraPrefix],
        now: Instant,
        ttl: SignedDuration,
    ) -> Self {
        let path = state_dir.join(FILE_NAME);
        let mut cache = Self {
            ttl,
            path,
            good: None,
            failure: None,
            refresh_requested: false,
        };
        match read_stored(&cache.path, prefixes) {
            Ok(good) => cache.good = good,
            Err(fault) => {
                let fault = quarantine(fault, state_dir);
                tracing::error!(error = %fault, "the saved Jira work items cannot be used");
                cache.failure = Some(Failed {
                    error: JiraError::CacheFile(fault),
                    at: now,
                });
                cache.refresh_requested = true;
            }
        }
        cache
    }

    /// Makes the next [`Cache::is_due`] true, whatever the TTL.
    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    /// Whether the next read should ask `acli` again: a refresh was requested, nothing was
    /// ever asked, or the latest attempt is at least the TTL old. A clock that ran backwards
    /// makes the cache due, since its age cannot be trusted.
    pub fn is_due(&self, now: Instant) -> bool {
        if self.refresh_requested {
            return true;
        }
        let Some(attempted) = self.attempted_at() else {
            return true;
        };
        let elapsed = now.duration_since(attempted);
        elapsed.is_negative() || elapsed >= self.ttl
    }

    /// When the latest attempt, successful or not, was made.
    fn attempted_at(&self) -> Option<Instant> {
        self.failure
            .as_ref()
            .map(|f| f.at)
            .or_else(|| self.good.as_ref().map(|g| g.read_at))
    }

    /// Files the outcome of one `acli` call made at `now`, and saves a good list. The cache
    /// holds the outcome even when the save fails; the failure to save is returned.
    pub fn record(
        &mut self,
        outcome: Result<Vec<WorkItem>, JiraError>,
        now: Instant,
    ) -> Result<(), CacheFileError> {
        self.refresh_requested = false;
        match outcome {
            Ok(items) => {
                let observed = Observed {
                    items,
                    read_at: now,
                };
                let saved = save(&self.path, &observed);
                self.good = Some(observed);
                self.failure = None;
                saved
            }
            Err(error) => {
                self.failure = Some(Failed { error, at: now });
                Ok(())
            }
        }
    }

    pub fn reading(&self, now: Instant) -> Reading {
        match (&self.good, &self.failure) {
            (None, None) => Reading::Unread,
            (Some(observed), failure) => Reading::Value {
                age: Age::between(observed.read_at, now),
                observed: observed.clone(),
                failure: failure.clone(),
            },
            (None, Some(failed)) => Reading::Failure(failed.clone()),
        }
    }
}

/// The saved list, `None` when there is no file.
fn read_stored(path: &Path, prefixes: &[JiraPrefix]) -> Result<Option<Observed>, CacheFileError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CacheFileError::Unreadable {
                path: path.to_owned(),
                detail: e.to_string(),
            });
        }
    };
    let stored: Stored = serde_json::from_slice(&bytes).map_err(|e| CacheFileError::Corrupt {
        path: path.to_owned(),
        detail: e.to_string(),
        moved_to: None,
    })?;
    Ok(Some(Observed {
        items: stored
            .items
            .into_iter()
            .map(|item| item.into_item(prefixes))
            .collect(),
        read_at: stored.read_at,
    }))
}

/// Sets a corrupt file aside, so the next save does not overwrite the evidence.
fn quarantine(fault: CacheFileError, state_dir: &Path) -> CacheFileError {
    let CacheFileError::Corrupt {
        path,
        detail,
        moved_to: None,
    } = fault
    else {
        return fault;
    };
    let target = state_dir.join(CORRUPT_FILE_NAME);
    let moved_to = std::fs::rename(&path, &target).ok().map(|()| target);
    CacheFileError::Corrupt {
        path,
        detail,
        moved_to,
    }
}

fn save(path: &Path, observed: &Observed) -> Result<(), CacheFileError> {
    let stored = Stored {
        read_at: observed.read_at,
        items: observed.items.iter().map(StoredItem::from_item).collect(),
    };
    let write = |detail: String| CacheFileError::Write {
        path: path.to_owned(),
        detail,
    };
    let bytes = serde_json::to_vec(&stored).map_err(|e| write(e.to_string()))?;
    write_private(path, &bytes).map_err(|e| write(e.to_string()))
}

#[cfg(test)]
mod tests {
    //! The cache is driven with explicit instants, so every age and every due-time is a
    //! hand-written number of seconds after `T0`; the file is a real file in a temporary
    //! directory.

    use super::*;
    use crate::jira::parse_work_items;
    use crate::testutil::{Scratch, secs};

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/jira/search_result.json");

    fn prefixes() -> Vec<JiraPrefix> {
        vec![JiraPrefix::parse("EX").unwrap()]
    }

    fn items() -> Vec<WorkItem> {
        parse_work_items(FIXTURE, &prefixes()).unwrap()
    }

    fn timeout() -> JiraError {
        JiraError::Timeout
    }

    fn cache_with_good_at_t0(dir: &Path) -> Cache {
        let mut cache = Cache::load(dir, &prefixes(), secs(0));
        cache.record(Ok(items()), secs(0)).unwrap();
        cache
    }

    #[test]
    fn nothing_recorded_is_unread_and_due() {
        let dir = Scratch::new("unread");
        let cache = Cache::load(&dir, &prefixes(), secs(0));
        assert!(matches!(cache.reading(secs(0)), Reading::Unread));
        assert!(cache.is_due(secs(0)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: a TTL of 5 minutes, or `>` for `>=`.
    #[test]
    fn a_good_list_is_served_with_its_age_until_ten_minutes_pass() {
        let dir = Scratch::new("ttl");
        let cache = cache_with_good_at_t0(&dir);
        assert!(!cache.is_due(secs(599)));
        assert!(cache.is_due(secs(600)));
        let Reading::Value {
            observed,
            age,
            failure,
        } = cache.reading(secs(900))
        else {
            panic!("a recorded good list reads as a value");
        };
        assert_eq!(observed.items, items());
        assert_eq!(age, Age::Minutes(15));
        assert!(failure.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: have `record` replace the good list with nothing on `Err`.
    #[test]
    fn a_failure_after_a_good_list_is_shown_beside_the_stale_list() {
        let dir = Scratch::new("stale");
        let mut cache = cache_with_good_at_t0(&dir);
        cache.record(Err(timeout()), secs(610)).unwrap();
        let Reading::Value {
            observed,
            age,
            failure: Some(failed),
        } = cache.reading(secs(620))
        else {
            panic!("the good list survives a failed refresh, with the failure");
        };
        assert_eq!(observed.read_at, secs(0));
        assert_eq!(observed.items.len(), 3);
        assert_eq!(age, Age::Minutes(10));
        assert_eq!(failed.error, JiraError::Timeout);
        assert_eq!(failed.at, secs(610));
        // The failed attempt starts a new TTL window.
        assert!(!cache.is_due(secs(1209)));
        assert!(cache.is_due(secs(1210)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_later_success_clears_the_failure() {
        let dir = Scratch::new("recover");
        let mut cache = cache_with_good_at_t0(&dir);
        cache
            .record(Err(JiraError::NotInstalled), secs(610))
            .unwrap();
        cache.record(Ok(vec![]), secs(1300)).unwrap();
        let Reading::Value {
            observed, failure, ..
        } = cache.reading(secs(1301))
        else {
            panic!("a recorded good list reads as a value");
        };
        assert_eq!(observed.read_at, secs(1300));
        assert_eq!(observed.items.len(), 0);
        assert!(failure.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failure_with_no_good_list_is_a_failure_not_a_blank() {
        let dir = Scratch::new("failonly");
        let mut cache = Cache::load(&dir, &prefixes(), secs(0));
        cache
            .record(
                Err(JiraError::NotAuthenticated {
                    hint: "run `acli jira auth login`",
                }),
                secs(0),
            )
            .unwrap();
        let Reading::Failure(failed) = cache.reading(secs(5)) else {
            panic!("only a failure was recorded");
        };
        assert!(matches!(failed.error, JiraError::NotAuthenticated { .. }));
        assert!(!dir.join(FILE_NAME).exists(), "a failure is not saved");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: drop the `refresh_requested` clause of `is_due`, or leave the flag set after
    /// `record`.
    #[test]
    fn a_requested_refresh_is_due_at_once_until_it_is_attempted() {
        let dir = Scratch::new("request");
        let mut cache = cache_with_good_at_t0(&dir);
        assert!(!cache.is_due(secs(1)));
        cache.request_refresh();
        assert!(cache.is_due(secs(1)));
        cache.record(Err(timeout()), secs(2)).unwrap();
        assert!(!cache.is_due(secs(3)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_clock_that_ran_backwards_makes_the_cache_due() {
        let dir = Scratch::new("clock");
        let cache = cache_with_good_at_t0(&dir);
        assert!(cache.is_due(secs(-60)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: skip the `save` in `record`, or save before the items are classified (the
    /// keys change form), or drop `read_at` from the file.
    #[test]
    fn a_restart_shows_the_last_read_with_its_age() {
        let dir = Scratch::new("roundtrip");
        drop(cache_with_good_at_t0(&dir));
        assert!(dir.join(FILE_NAME).is_file());
        let restarted = Cache::load(&dir, &prefixes(), secs(700));
        let Reading::Value {
            observed,
            age,
            failure,
        } = restarted.reading(secs(700))
        else {
            panic!("the saved list is the restarted cache's value");
        };
        assert_eq!(observed.items, items());
        assert_eq!(observed.read_at, secs(0));
        assert_eq!(age, Age::Minutes(11));
        assert!(failure.is_none());
        // The file's age counts: the list read at 0 is past its TTL at 700.
        assert!(restarted.is_due(secs(700)));
        assert!(!restarted.is_due(secs(599)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file saved before the level was read still loads, its items as plain items.
    /// Mutation: drop `#[serde(default)]` from `level` (the file is then corrupt).
    #[test]
    fn a_file_without_levels_loads_its_items_as_plain_items() {
        let dir = Scratch::new("nolevel");
        std::fs::create_dir_all(&dir).unwrap();
        let saved = r#"{"read_at":"2026-01-01T00:00:00Z","items":[{"key":"EX-1","summary":"s","status":{"name":"To Do","category":"ToDo"},"issue_type":"Epic","priority":null,"creator":null,"assignee":null,"updated":null,"created":null,"description":null}]}"#;
        std::fs::write(dir.join(FILE_NAME), saved).unwrap();
        let loaded = Cache::load(&dir, &prefixes(), secs(0));
        let Reading::Value { observed, .. } = loaded.reading(secs(0)) else {
            panic!("an older file is still the saved list");
        };
        assert_eq!(observed.items.first().unwrap().level, Level::Item);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Keys are classified on load, so a prefix configured after the save takes effect.
    /// Mutation: classify against a fixed `EX` prefix instead of the prefixes passed to `load`.
    #[test]
    fn keys_are_classified_against_the_prefixes_at_load() {
        let dir = Scratch::new("reclassify");
        drop(cache_with_good_at_t0(&dir));
        let restarted = Cache::load(&dir, &[], secs(1));
        let Reading::Value { observed, .. } = restarted.reading(secs(1)) else {
            panic!("saved list");
        };
        assert_eq!(
            observed.items.first().unwrap().key,
            WorkItemKey::Other("EX-1".to_owned())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: treat a parse failure as "no file" (`Ok(None)`), or skip the quarantine so the
    /// next save overwrites the evidence.
    #[test]
    fn a_corrupt_file_is_a_visible_failure_and_is_set_aside_not_overwritten() {
        let dir = Scratch::new("corrupt");
        std::fs::write(dir.join(FILE_NAME), b"{ not what was written").unwrap();
        let mut cache = Cache::load(&dir, &prefixes(), secs(5));
        let Reading::Failure(failed) = cache.reading(secs(6)) else {
            panic!("a corrupt file is a failure, not an empty cache");
        };
        let JiraError::CacheFile(CacheFileError::Corrupt { moved_to, .. }) = &failed.error else {
            panic!("{:?}", failed.error);
        };
        assert_eq!(
            moved_to.as_deref(),
            Some(dir.join(CORRUPT_FILE_NAME).as_path())
        );
        assert_eq!(failed.at, secs(5));
        assert!(
            cache.is_due(secs(5)),
            "a failed load does not delay the first read"
        );
        assert_eq!(
            std::fs::read(dir.join(CORRUPT_FILE_NAME)).unwrap(),
            b"{ not what was written"
        );
        assert!(!dir.join(FILE_NAME).exists());

        cache.record(Ok(items()), secs(10)).unwrap();
        assert_eq!(
            std::fs::read(dir.join(CORRUPT_FILE_NAME)).unwrap(),
            b"{ not what was written",
            "a save leaves the set-aside file alone"
        );
        assert!(matches!(
            cache.reading(secs(11)),
            Reading::Value { failure: None, .. }
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file that parses as JSON of the wrong shape is as corrupt as one that does not parse.
    #[test]
    fn a_file_of_the_wrong_shape_is_corrupt() {
        let dir = Scratch::new("shape");
        std::fs::write(dir.join(FILE_NAME), b"[]").unwrap();
        let cache = Cache::load(&dir, &prefixes(), secs(0));
        let Reading::Failure(failed) = cache.reading(secs(0)) else {
            panic!("wrong shape is a failure");
        };
        assert!(matches!(
            failed.error,
            JiraError::CacheFile(CacheFileError::Corrupt { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Mutation: swallow the save error (`let _ = save(..)`).
    #[test]
    fn a_save_that_fails_is_reported_and_the_list_is_still_held() {
        let dir = Scratch::new("savefail");
        let mut cache = Cache::load(&dir, &prefixes(), secs(0));
        // A directory where the file belongs makes the rename fail.
        std::fs::create_dir(dir.join(FILE_NAME)).unwrap();
        std::fs::write(dir.join(FILE_NAME).join("keep"), b"x").unwrap();
        let saved = cache.record(Ok(items()), secs(1));
        assert!(
            matches!(saved, Err(CacheFileError::Write { .. })),
            "{saved:?}"
        );
        assert!(matches!(cache.reading(secs(2)), Reading::Value { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
