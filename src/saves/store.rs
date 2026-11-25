//! The saves on disk: `<state_dir>/saves/auto.json` and one `<millis>.json` per kept save.
//!
//! A file that cannot be read is listed as a problem and left in place, never deleted or
//! written over. On a new boot the automatic save of the old one is kept as a
//! [`SaveKind::BeforeReboot`] save before the automatic save starts again from what is open.

use std::io;
use std::path::{Path, PathBuf};

use super::auto::Tracker;
use super::{BootId, KEPT_MAX, Save, SaveId, SaveKind, SavedSession};
use crate::types::Instant;
use crate::util::fs::write_private;

/// The directory under the state directory that holds the saves.
pub const DIR: &str = "saves";
const AUTO_FILE: &str = "auto.json";
const SUFFIX: &str = ".json";

#[derive(Debug, thiserror::Error)]
pub enum SavesError {
    #[error("cannot write {}: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("cannot delete {}: {source}", path.display())]
    Delete { path: PathBuf, source: io::Error },
    #[error("there is no save {0}")]
    NotFound(SaveId),
    #[error("the automatic save follows the open sessions and is not deleted")]
    AutoIsNotDeleted,
    #[error("no session is running, so there is nothing to save")]
    NothingRunning,
}

/// What the pages show of the saves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavesView {
    pub auto: Option<Save>,
    /// Newest first.
    pub kept: Vec<(SaveId, Save)>,
    /// Files that could not be read, and kept saves that could not be written or pushed out,
    /// as said when they happened.
    pub problems: Vec<String>,
    /// The latest write of the automatic save, when it failed; it is tried again on the next
    /// pass until one succeeds.
    pub auto_failure: Option<String>,
}

impl SavesView {
    pub fn get(&self, id: SaveId) -> Option<&Save> {
        match id {
            SaveId::Auto => self.auto.as_ref(),
            SaveId::Kept(_) => self
                .kept
                .iter()
                .find(|(kept, _)| *kept == id)
                .map(|(_, save)| save),
        }
    }
}

/// The saves, and the tracker that decides the automatic one.
#[derive(Debug)]
pub struct Saves {
    dir: PathBuf,
    boot: Option<BootId>,
    view: SavesView,
    tracker: Tracker,
}

impl Saves {
    /// The saves under `state_dir`, in the boot `boot` (`None` when it could not be read,
    /// which counts as a new boot: a kept copy too many costs less than a lost one).
    pub fn load(state_dir: &Path, boot: Option<BootId>) -> Self {
        let dir = state_dir.join(DIR);
        let mut view = SavesView::default();
        read_dir(&dir, &mut view);
        let mut saves = Self {
            dir,
            boot,
            view,
            tracker: Tracker::default(),
        };
        saves.start_boot();
        saves
    }

    /// Keeps the automatic save of another boot, then starts the automatic save over; an
    /// automatic save of this boot is taken up where it was.
    fn start_boot(&mut self) {
        let Some(auto) = self.view.auto.clone() else {
            return;
        };
        let same_boot = self.boot.is_some() && auto.boot == self.boot;
        if same_boot {
            self.tracker = Tracker::holding(auto.sessions);
            return;
        }
        if !auto.sessions.is_empty() {
            let kept = Save {
                kind: SaveKind::BeforeReboot,
                ..auto
            };
            if let Err(error) = self.keep(kept) {
                // The old automatic save stays as it is, to be kept on the next start.
                tracing::warn!(%error, "the automatic save of the last boot could not be kept");
                self.view.problems.push(error.to_string());
                return;
            }
        }
        let path = self.dir.join(AUTO_FILE);
        if let Err(source) = std::fs::remove_file(&path) {
            let error = SavesError::Delete { path, source };
            tracing::warn!(%error, "the automatic save of the last boot could not be removed");
            self.view.problems.push(error.to_string());
        }
        self.view.auto = None;
    }

    pub fn view(&self) -> &SavesView {
        &self.view
    }

    pub fn tracker(&mut self) -> &mut Tracker {
        &mut self.tracker
    }

    /// Whether the automatic save on disk is behind the tracker: its last write failed.
    pub const fn auto_behind(&self) -> bool {
        self.view.auto_failure.is_some()
    }

    /// Writes the automatic save from the tracker, as of `at`. A failure is kept in the view
    /// until a later write succeeds.
    pub fn write_auto(&mut self, at: Instant) -> Result<(), SavesError> {
        let save = Save {
            kind: SaveKind::Auto,
            at,
            boot: self.boot.clone(),
            sessions: self.tracker.sessions(),
        };
        match write(&self.dir.join(AUTO_FILE), &save) {
            Ok(()) => {
                self.view.auto = Some(save);
                self.view.auto_failure = None;
                Ok(())
            }
            Err(error) => {
                self.view.auto_failure = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Keeps `sessions` as a manual save made at `at`.
    pub fn save_now(
        &mut self,
        sessions: Vec<SavedSession>,
        at: Instant,
    ) -> Result<SaveId, SavesError> {
        if sessions.is_empty() {
            return Err(SavesError::NothingRunning);
        }
        self.keep(Save {
            kind: SaveKind::Manual,
            at,
            boot: self.boot.clone(),
            sessions,
        })
    }

    /// Deletes the kept save `id`.
    pub fn delete(&mut self, id: SaveId) -> Result<(), SavesError> {
        if id == SaveId::Auto {
            return Err(SavesError::AutoIsNotDeleted);
        }
        if self.view.get(id).is_none() {
            return Err(SavesError::NotFound(id));
        }
        let path = self.path(id);
        std::fs::remove_file(&path).map_err(|source| SavesError::Delete { path, source })?;
        self.view.kept.retain(|(kept, _)| *kept != id);
        Ok(())
    }

    /// Writes `save` as a kept save, named by the millisecond it was made in (the next free
    /// one when two share it), and pushes out the oldest beyond [`KEPT_MAX`].
    fn keep(&mut self, save: Save) -> Result<SaveId, SavesError> {
        let mut millis = save.at.as_millisecond();
        while self.view.get(SaveId::Kept(millis)).is_some() {
            millis += 1;
        }
        let id = SaveId::Kept(millis);
        write(&self.path(id), &save)?;
        self.view.kept.push((id, save));
        self.view.kept.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
        while self.view.kept.len() > KEPT_MAX {
            let Some((oldest, _)) = self.view.kept.last().cloned() else {
                break;
            };
            if let Err(error) = self.delete(oldest) {
                tracing::warn!(%error, "an old save could not be pushed out");
                self.view.problems.push(error.to_string());
                break;
            }
        }
        Ok(id)
    }

    fn path(&self, id: SaveId) -> PathBuf {
        match id {
            SaveId::Auto => self.dir.join(AUTO_FILE),
            SaveId::Kept(_) => self.dir.join(format!("{id}{SUFFIX}")),
        }
    }
}

fn write(path: &Path, save: &Save) -> Result<(), SavesError> {
    let bytes = serde_json::to_vec_pretty(save).map_err(|e| SavesError::Write {
        path: path.to_path_buf(),
        source: io::Error::other(e),
    })?;
    write_private(path, &bytes).map_err(|source| SavesError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads every save file in `dir` into `view`; a missing directory holds none.
fn read_dir(dir: &Path, view: &mut SavesView) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            view.problems
                .push(format!("cannot list {}: {error}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                view.problems
                    .push(format!("cannot list {}: {error}", dir.display()));
                continue;
            }
        };
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(SUFFIX))
            .and_then(|stem| SaveId::parse(stem).ok())
        else {
            // A write's temporary sibling, or a file this tool did not write.
            tracing::debug!(path = %path.display(), "not a save file; left alone");
            continue;
        };
        match read_save(&path) {
            Ok(save) if id == SaveId::Auto => view.auto = Some(save),
            Ok(save) => view.kept.push((id, save)),
            Err(problem) => {
                tracing::warn!(%problem, "a save could not be read");
                view.problems.push(problem);
            }
        }
    }
    view.kept.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
}

fn read_save(path: &Path) -> Result<Save, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} is not a save: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    //! The store against a scratch state directory: boots, kept saves and deletes, each
    //! checked by loading again from disk.

    use super::*;
    use crate::saves::Resume;
    use crate::saves::auto::Observation;
    use crate::testutil::Scratch;
    use crate::types::SessionId;

    fn at(secs: i64) -> Instant {
        Instant::from_second(1_700_000_000 + secs).unwrap()
    }

    fn session(n: u8) -> SavedSession {
        SavedSession {
            id: SessionId::parse(&format!("00000000-0000-4000-8000-{n:012}")).unwrap(),
            title: Some(format!("session {n}")),
            gist: None,
            cwd: Some(PathBuf::from("/home/dev/proj")),
            last_activity: Some(at(i64::from(n))),
            resume: Resume::Command {
                text: format!("cd /home/dev/proj && claude --resume {n}"),
                warnings: vec![],
            },
        }
    }

    fn boot(text: &str) -> BootId {
        BootId::of(text)
    }

    fn auto_holding(dir: &Path, boot_id: Option<BootId>, sessions: &[SavedSession]) {
        let mut saves = Saves::load(dir, boot_id);
        saves.tracker().observe(
            Observation {
                live: sessions.to_vec(),
                unknown: std::collections::HashSet::new(),
            },
            at(100),
        );
        saves.write_auto(at(100)).unwrap();
    }

    /// The same boot takes the automatic save up where it was; a new boot keeps it as a
    /// "before reboot" save and starts over, once; an unreadable boot counts as a new one.
    /// Mutation: compare boots with `!=` (the same boot is kept away), skip removing
    /// `auto.json` (the next start keeps it twice), or count an unknown boot as the same.
    #[test]
    fn a_new_boot_keeps_the_old_automatic_save_once() {
        let dir = Scratch::new("saves-boot");
        auto_holding(&dir, Some(boot("one")), &[session(1), session(2)]);

        let same = Saves::load(&dir, Some(boot("one")));
        assert_eq!(same.view().auto.as_ref().unwrap().sessions.len(), 2);
        assert_eq!(same.view().kept, []);

        let rebooted = Saves::load(&dir, Some(boot("two")));
        assert!(rebooted.view().auto.is_none());
        let [(id, kept)] = rebooted.view().kept.as_slice() else {
            panic!("one kept save: {:?}", rebooted.view().kept);
        };
        assert_eq!(*id, SaveId::Kept(at(100).as_millisecond()));
        assert_eq!(kept.kind, SaveKind::BeforeReboot);
        assert_eq!(kept.sessions, [session(2), session(1)]);

        let again = Saves::load(&dir, Some(boot("two")));
        assert_eq!(again.view().kept.len(), 1);
        assert!(again.view().auto.is_none());

        auto_holding(&dir, Some(boot("two")), &[session(3)]);
        let unknown = Saves::load(&dir, None);
        assert_eq!(unknown.view().kept.len(), 2);
        // Unreadable both times: still not known to be the same boot.
        auto_holding(&dir, None, &[session(4)]);
        let unknown_again = Saves::load(&dir, None);
        assert_eq!(unknown_again.view().kept.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An eleventh kept save pushes the oldest out, on disk too; a delete removes one; the
    /// automatic save cannot be deleted; nothing running is not saved.
    /// Mutation: keep 11, push out the newest, or let `delete` leave the file.
    #[test]
    fn kept_saves_are_ten_at_most_and_deleted_on_request() {
        let dir = Scratch::new("saves-kept");
        let mut saves = Saves::load(&dir, Some(boot("one")));
        for n in 0..11 {
            saves.save_now(vec![session(1)], at(n)).unwrap();
        }
        let ids = |saves: &Saves| {
            saves
                .view()
                .kept
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        let expected: Vec<SaveId> = (1..11)
            .rev()
            .map(|n| SaveId::Kept(at(n).as_millisecond()))
            .collect();
        assert_eq!(ids(&saves), expected);
        assert_eq!(ids(&Saves::load(&dir, Some(boot("one")))), expected);

        let newest = SaveId::Kept(at(10).as_millisecond());
        saves.delete(newest).unwrap();
        assert_eq!(
            ids(&Saves::load(&dir, Some(boot("one")))).first(),
            expected.get(1)
        );
        assert!(matches!(saves.delete(newest), Err(SavesError::NotFound(_))));
        assert!(matches!(
            saves.delete(SaveId::Auto),
            Err(SavesError::AutoIsNotDeleted)
        ));
        assert!(matches!(
            saves.save_now(vec![], at(20)),
            Err(SavesError::NothingRunning)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two saves in one millisecond get two names; a file that is not a save is a problem and
    /// stays where it is.
    /// Mutation: write the second over the first, or delete the unreadable file.
    #[test]
    fn same_millisecond_saves_both_stay_and_a_bad_file_is_a_problem() {
        let dir = Scratch::new("saves-odd");
        let mut saves = Saves::load(&dir, Some(boot("one")));
        let first = saves.save_now(vec![session(1)], at(5)).unwrap();
        let second = saves.save_now(vec![session(2)], at(5)).unwrap();
        assert_ne!(first, second);
        let bad = dir.join(DIR).join("123.json");
        std::fs::write(&bad, b"not json").unwrap();
        let loaded = Saves::load(&dir, Some(boot("one")));
        assert_eq!(loaded.view().kept.len(), 2);
        assert_eq!(loaded.view().problems.len(), 1);
        assert!(bad.is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
