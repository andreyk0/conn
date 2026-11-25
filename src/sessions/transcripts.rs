//! Finding transcripts on disk: `<claude_dir>/projects/<dir>/<session id>.jsonl`.
//!
//! Only the top level of each project directory is read. Beside each transcript Claude Code
//! keeps a directory of the same id holding `subagents/`, `tool-results/` and workflow files,
//! which are other things.
//!
//! The project directory's name is a lossy encoding of the working directory, so it is kept
//! as an opaque name and never decoded: the transcript's own `cwd` is authoritative.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::types::{Instant, ProjectDirName, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    pub project_dir: ProjectDirName,
    pub session_id: SessionId,
    pub path: PathBuf,
    pub modified: Instant,
    pub size: u64,
}

/// Why an entry that could have been a transcript or a project was not taken as one.
///
/// Entries that are plainly something else (a session's own directory, a file without the
/// `.jsonl` extension such as Claude Code's `.session-aliases`) are not candidates and have
/// no reason: listing them would bury the ones that need a look.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkipReason {
    #[error("a .jsonl file whose name is not a session id")]
    NotSessionIdName,
    #[error("a .jsonl entry that is a symbolic link or special file, not a regular file")]
    NotRegularFile,
    #[error("an entry in projects/ that is not a directory")]
    NotProjectDir,
    #[error("a project directory name that is not valid UTF-8")]
    NonUtf8Name,
    #[error("cannot read it: {0}")]
    Unreadable(String),
}

/// What a scan of the projects directory found, and what it passed over and why.
///
/// `skipped` is part of the answer: an unreadable `projects` directory shows up there, so
/// an empty `files` is never the only sign of one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptScan {
    /// Sorted by project directory, then session id.
    pub files: Vec<TranscriptFile>,
    pub skipped: Vec<(PathBuf, SkipReason)>,
}

pub fn transcripts(claude_dir: &Path) -> TranscriptScan {
    let projects = claude_dir.join("projects");
    let mut scan = TranscriptScan::default();
    let dirs = match read_dir_sorted(&projects) {
        Ok(dirs) => dirs,
        Err(e) => {
            scan.skipped
                .push((projects, SkipReason::Unreadable(e.to_string())));
            return scan;
        }
    };
    for dir in dirs {
        scan_project(&dir, &mut scan);
    }
    scan.files.sort_by(|a, b| {
        (a.project_dir.as_str(), &a.session_id).cmp(&(b.project_dir.as_str(), &b.session_id))
    });
    scan
}

fn read_dir_sorted(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

fn scan_project(dir: &Path, scan: &mut TranscriptScan) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        scan.skipped.push((
            dir.to_owned(),
            SkipReason::Unreadable("cannot stat".to_owned()),
        ));
        return;
    };
    if !meta.is_dir() {
        scan.skipped
            .push((dir.to_owned(), SkipReason::NotProjectDir));
        return;
    }
    let Some(project_dir) = dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| ProjectDirName::new(n).ok())
    else {
        scan.skipped.push((dir.to_owned(), SkipReason::NonUtf8Name));
        return;
    };
    let files = match read_dir_sorted(dir) {
        Ok(files) => files,
        Err(e) => {
            scan.skipped
                .push((dir.to_owned(), SkipReason::Unreadable(e.to_string())));
            return;
        }
    };
    for path in files {
        match take_transcript(&project_dir, &path) {
            Taken::File(file) => scan.files.push(file),
            Taken::Skip(reason) => scan.skipped.push((path, reason)),
            Taken::NotCandidate => {}
        }
    }
}

enum Taken {
    File(TranscriptFile),
    Skip(SkipReason),
    NotCandidate,
}

fn take_transcript(project_dir: &ProjectDirName, path: &Path) -> Taken {
    if path.extension().is_none_or(|e| e != "jsonl") {
        return Taken::NotCandidate;
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => return Taken::Skip(SkipReason::Unreadable(e.to_string())),
    };
    if !meta.is_file() {
        return Taken::Skip(SkipReason::NotRegularFile);
    }
    let Some(session_id) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| SessionId::parse(s).ok())
    else {
        return Taken::Skip(SkipReason::NotSessionIdName);
    };
    let modified = meta
        .modified()
        .map_err(|e| e.to_string())
        .and_then(|t| Instant::try_from(t).map_err(|e| e.to_string()));
    match modified {
        Ok(modified) => Taken::File(TranscriptFile {
            project_dir: project_dir.clone(),
            session_id,
            path: path.to_owned(),
            modified,
            size: meta.len(),
        }),
        Err(reason) => Taken::Skip(SkipReason::Unreadable(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sessions/claude")
    }

    fn summary(scan: &TranscriptScan) -> Vec<(String, String, u64)> {
        scan.files
            .iter()
            .map(|f| {
                (
                    f.project_dir.as_str().to_owned(),
                    f.session_id.to_string(),
                    f.size,
                )
            })
            .collect()
    }

    /// Mutation: descend into subdirectories (walk recursively) -> the `subagents` file with a
    /// session-id name appears -> red.
    #[test]
    fn takes_top_level_transcripts_only_and_never_a_session_directorys_contents() {
        let scan = transcripts(&claude_dir());
        assert_eq!(
            summary(&scan),
            [
                (
                    "-home-dev-alpha".to_owned(),
                    "00000000-0000-4000-8000-000000000011".to_owned(),
                    91
                ),
                (
                    // Sorted by id, not by file name: the upper-case `AB` file name sorts
                    // before `a0` as text but its id, lower-cased, sorts after.
                    "-home-dev-beta".to_owned(),
                    "00000000-0000-4000-8000-0000000000a0".to_owned(),
                    3
                ),
                (
                    "-home-dev-beta".to_owned(),
                    "00000000-0000-4000-8000-0000000000ab".to_owned(),
                    60
                ),
            ]
        );
    }

    /// `notes.txt` in a project directory is not a candidate and is not listed.
    /// Mutation: filter a bad name without pushing to `skipped` -> red; list every non-`.jsonl`
    /// file as skipped -> `notes.txt` appears -> red.
    #[test]
    fn what_it_passes_over_is_counted_with_a_reason() {
        let scan = transcripts(&claude_dir());
        let skipped: Vec<(String, SkipReason)> = scan
            .skipped
            .iter()
            .map(|(p, r)| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    r.clone(),
                )
            })
            .collect();
        assert_eq!(
            skipped,
            [
                ("not-a-uuid.jsonl".to_owned(), SkipReason::NotSessionIdName),
                ("stray.txt".to_owned(), SkipReason::NotProjectDir),
            ]
        );
    }

    /// The modified time is the file's, as UTC.
    /// Mutation: set `modified` to `Instant::UNIX_EPOCH` -> red.
    #[test]
    fn modified_is_the_file_mtime() {
        let scan = transcripts(&claude_dir());
        let file = scan.files.first().unwrap();
        let on_disk: Instant = fs::metadata(&file.path)
            .unwrap()
            .modified()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(file.modified, on_disk);
        assert!(file.modified > Instant::UNIX_EPOCH);
    }

    /// Mutation: return an empty scan when `projects` is unreadable -> red.
    #[test]
    fn an_unreadable_projects_directory_is_reported_not_empty() {
        let scan = transcripts(&claude_dir().join("no-such-dir"));
        assert_eq!(scan.files, []);
        assert!(matches!(
            scan.skipped.as_slice(),
            [(_, SkipReason::Unreadable(_))]
        ));
    }

    /// Reads this machine's transcripts and prints counts; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "reads ~/.claude/projects"]
    #[allow(
        clippy::print_stdout,
        reason = "an ignored smoke test reports counts to its runner"
    )]
    fn real_transcripts_are_found() {
        let claude_dir = Path::new(&std::env::var_os("HOME").unwrap()).join(".claude");
        let scan = transcripts(&claude_dir);
        println!(
            "transcripts: {} files, {} skipped",
            scan.files.len(),
            scan.skipped.len()
        );
    }
}
