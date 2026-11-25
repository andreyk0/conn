//! Finding and focusing the terminal window of a session under the niri compositor.
//!
//! kitty shows a session as `<status glyph> <title>` (a spinner or star, one non-ASCII
//! character and a space, then the session's custom title, else its AI title: see
//! `Summary::window_title`), so a window is matched on its title with that prefix removed.
//! Titles repeat across windows, which is why a match can be ambiguous.

use std::time::Duration;

use serde_json::Value;

use crate::util::json::{FieldError, Fields};
use crate::util::runner::{CommandRunner, ToolError, run_tool};

pub type WindowError = ToolError;

const PROGRAM: &str = "niri";
const TERMINAL_APP_ID: &str = "kitty";
/// niri answers from memory; five seconds means the compositor is wedged.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A niri window id. Only [`find_window`] makes one, so holding it means a window with
/// this id was listed by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowId(u64);

impl WindowId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowMatch {
    Unique(WindowId),
    /// Several terminals carry the same title; focusing any would be a guess.
    Ambiguous {
        count: usize,
    },
    NotFound,
    /// The compositor could not be asked; this says nothing about whether a window exists.
    Unavailable {
        reason: ToolError,
    },
}

/// What sets a session's title apart from every other window's: ` · ` and the first block of
/// its id. `conn hook` ends every title it sets with it, and [`TitleMatch::Pinned`] looks for
/// it, so the one function is both sides of the agreement.
pub fn pinned_suffix(session: &crate::types::SessionId) -> String {
    let short = session.as_str().split('-').next().unwrap_or_default();
    format!(" · {short}")
}

/// The longest headline a title carries before the suffix; a terminal tab shows little more.
const TITLE_HEADLINE_CHARS: usize = 48;

/// The title `conn hook` gives a session: what it is about, then its [`pinned_suffix`]. What it
/// is about is its gist's headline, else the last part of its working directory, else
/// `Claude`. A long headline is cut at a word.
pub fn session_title(
    headline: Option<&str>,
    cwd: Option<&std::path::Path>,
    session: &crate::types::SessionId,
) -> String {
    let directory = cwd
        .and_then(std::path::Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());
    let about = headline
        .map(str::to_owned)
        .or(directory)
        .unwrap_or_else(|| "Claude".to_owned());
    let about = match about.char_indices().nth(TITLE_HEADLINE_CHARS) {
        None => about,
        Some((cut, _)) => {
            let head = about.get(..cut).unwrap_or_default();
            let at_word = head.rfind(' ').filter(|at| *at > cut / 2).unwrap_or(cut);
            format!("{}…", head.get(..at_word).unwrap_or(head).trim_end())
        }
    };
    format!("{about}{}", pinned_suffix(session))
}

/// How a window's title (its status glyph removed) is matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleMatch<'t> {
    Exact(&'t str),
    /// Ends with the session's [`pinned_suffix`]: whatever the headline said when it was set.
    Pinned(String),
}

impl TitleMatch<'_> {
    fn matches(&self, title: &str) -> bool {
        match self {
            Self::Exact(expected) => title == *expected,
            Self::Pinned(suffix) => title.ends_with(suffix.as_str()),
        }
    }
}

/// `title` without the status glyph kitty puts in front of it.
///
/// The glyph is the first character when it is not ASCII, and only counts as a glyph when a
/// space follows it; any other title is returned unchanged.
pub fn strip_status_glyph(title: &str) -> &str {
    let mut chars = title.chars();
    match chars.next() {
        Some(first) if !first.is_ascii() => chars.as_str().strip_prefix(' ').unwrap_or(title),
        Some(_) | None => title,
    }
}

/// The terminal window titled by the first of `titles` any window carries: a title some
/// window has decides, unique or not, and only a title no window has passes to the next.
pub async fn find_window(runner: &dyn CommandRunner, titles: &[TitleMatch<'_>]) -> WindowMatch {
    let out = match run_tool(runner, PROGRAM, &["msg", "--json", "windows"], TIMEOUT).await {
        Ok(out) => out,
        Err(reason) => return WindowMatch::Unavailable { reason },
    };
    match parse_windows(&out.stdout) {
        Ok(windows) => titles
            .iter()
            .map(|title| matching(&windows, title))
            .find(|found| *found != WindowMatch::NotFound)
            .unwrap_or(WindowMatch::NotFound),
        Err(reason) => WindowMatch::Unavailable {
            reason: ToolError::Unparseable {
                program: PROGRAM.to_owned(),
                reason,
            },
        },
    }
}

/// Focuses a window found by [`find_window`].
pub async fn focus_window(runner: &dyn CommandRunner, id: WindowId) -> Result<(), WindowError> {
    let id = id.get().to_string();
    run_tool(
        runner,
        PROGRAM,
        &["msg", "action", "focus-window", "--id", &id],
        TIMEOUT,
    )
    .await
    .map(drop)
}

#[derive(Debug, PartialEq, Eq)]
struct Window {
    id: WindowId,
    app_id: Option<String>,
    title: Option<String>,
}

fn parse_windows(stdout: &[u8]) -> Result<Vec<Window>, String> {
    let json: Value = serde_json::from_slice(stdout).map_err(|e| e.to_string())?;
    json.as_array()
        .ok_or_else(|| "the top level is not an array".to_owned())?
        .iter()
        .enumerate()
        .map(|(at, w)| parse_window(w).map_err(|e| format!("window {at}: {e}")))
        .collect()
}

fn parse_window(json: &Value) -> Result<Window, FieldError> {
    let fields = Fields::of(json)?;
    let id = json
        .get("id")
        .and_then(Value::as_u64)
        .ok_or(FieldError::missing("id"))?;
    Ok(Window {
        id: WindowId(id),
        app_id: fields.optional_str("app_id")?.map(str::to_owned),
        title: fields.optional_str("title")?.map(str::to_owned),
    })
}

fn matching(windows: &[Window], title: &TitleMatch<'_>) -> WindowMatch {
    let hits: Vec<WindowId> = windows
        .iter()
        .filter(|w| w.app_id.as_deref() == Some(TERMINAL_APP_ID))
        .filter(|w| {
            w.title
                .as_deref()
                .map(strip_status_glyph)
                .is_some_and(|shown| title.matches(shown))
        })
        .map(|w| w.id)
        .collect();
    match hits.as_slice() {
        [] => WindowMatch::NotFound,
        [only] => WindowMatch::Unique(*only),
        several => WindowMatch::Ambiguous {
            count: several.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::runner::{Exit, FakeRunner, Output, RunError};

    const WINDOWS: &str = include_str!("../../tests/fixtures/sessions/niri_windows.json");
    const GLYPH_TABLE: &str = include_str!("../../tests/fixtures/sessions/glyph_titles.json");

    /// Hand-written rows in the fixture. Mutation: strip any first character (ASCII too), strip
    /// without requiring the space, or strip two characters -> red on named rows.
    #[test]
    fn glyphs_are_stripped_only_when_they_are_a_status_prefix() {
        let rows: Vec<(String, String)> = serde_json::from_str(GLYPH_TABLE).unwrap();
        assert!(rows.len() >= 7);
        for (raw, expected) in rows {
            assert_eq!(strip_status_glyph(&raw), expected, "{raw:?}");
        }
    }

    fn windows_runner() -> FakeRunner {
        FakeRunner::printing(WINDOWS)
    }

    fn exact<'t>(titles: &[&'t str]) -> Vec<TitleMatch<'t>> {
        titles
            .iter()
            .map(|title| TitleMatch::Exact(title))
            .collect()
    }

    /// The title `conn hook` sets ends with the suffix the match looks for, whatever the
    /// headline; a long headline is cut at a word; without one, the directory names it.
    /// Mutation: match the suffix anywhere in the title, cut mid-word, or drop the directory
    /// fallback.
    #[tokio::test]
    async fn a_pinned_title_is_found_by_its_session_whatever_its_headline() {
        let session =
            crate::types::SessionId::parse("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d").unwrap();
        assert_eq!(
            session_title(Some("Map labels"), None, &session),
            "Map labels · 0a1b2c3d"
        );
        assert_eq!(
            session_title(None, Some(std::path::Path::new("/work/conn")), &session),
            "conn · 0a1b2c3d"
        );
        assert_eq!(session_title(None, None, &session), "Claude · 0a1b2c3d");
        assert_eq!(
            session_title(
                Some("Renaming histogram buckets between graphs through a reviewed change"),
                None,
                &session
            ),
            "Renaming histogram buckets between graphs… · 0a1b2c3d"
        );
        let runner = FakeRunner::printing(
            r#"[{"id": 5, "app_id": "kitty", "title": "✳ An older headline · 0a1b2c3d"},
                {"id": 6, "app_id": "kitty", "title": "✳ 0a1b2c3d · not at the end"}]"#,
        );
        assert_eq!(
            find_window(&runner, &[TitleMatch::Pinned(pinned_suffix(&session))]).await,
            WindowMatch::Unique(WindowId(5))
        );
    }

    /// Mutation: drop the `app_id` filter -> the `Placeholder Docs` window in another app
    /// with the same title makes `Plan the migration` ambiguous -> red.
    #[tokio::test]
    async fn one_matching_terminal_is_unique() {
        let runner = windows_runner();
        assert_eq!(
            find_window(&runner, &exact(&["Plan the migration"])).await,
            WindowMatch::Unique(WindowId(11))
        );
        assert_eq!(runner.lines(), ["niri msg --json windows"]);
    }

    /// Mutation: return the first hit -> red.
    #[tokio::test]
    async fn repeated_titles_are_ambiguous_with_their_count() {
        assert_eq!(
            find_window(&windows_runner(), &exact(&["Shared title"])).await,
            WindowMatch::Ambiguous { count: 2 }
        );
    }

    /// The first title some window carries decides, unique or not; a title no window has
    /// passes to the next, and the windows are listed once.
    /// Mutation: try only the first title (the absent one makes it `NotFound`), or skip an
    /// ambiguous title for the next one (`Plan the migration` would be focused).
    #[tokio::test]
    async fn titles_are_tried_in_order_from_one_listing() {
        let runner = windows_runner();
        assert_eq!(
            find_window(&runner, &exact(&["Absent title", "Plan the migration"])).await,
            WindowMatch::Unique(WindowId(11))
        );
        assert_eq!(runner.lines(), ["niri msg --json windows"]);
        assert_eq!(
            find_window(
                &windows_runner(),
                &exact(&["Shared title", "Plan the migration"])
            )
            .await,
            WindowMatch::Ambiguous { count: 2 }
        );
        assert_eq!(
            find_window(&windows_runner(), &[]).await,
            WindowMatch::NotFound
        );
    }

    /// Mutation: compare the raw title instead of the stripped one -> red; match as a prefix
    /// or substring -> `Plan` would match -> red.
    #[tokio::test]
    async fn no_match_is_not_found_and_partial_titles_do_not_match() {
        for title in [
            "Absent title",
            "Plan",
            "plan the migration",
            "✳ Plan the migration",
        ] {
            assert_eq!(
                find_window(&windows_runner(), &exact(&[title])).await,
                WindowMatch::NotFound,
                "{title}"
            );
        }
    }

    /// Mutation: turn a runner failure into `NotFound` -> red.
    #[tokio::test]
    async fn a_compositor_that_cannot_answer_is_unavailable_not_not_found() {
        let missing = FakeRunner::new([Err(RunError::NotFound)]);
        assert_eq!(
            find_window(&missing, &exact(&["x"])).await,
            WindowMatch::Unavailable {
                reason: ToolError::Missing {
                    program: "niri".into()
                }
            }
        );
        let garbage = FakeRunner::printing("not json");
        assert!(matches!(
            find_window(&garbage, &exact(&["x"])).await,
            WindowMatch::Unavailable {
                reason: ToolError::Unparseable { .. }
            }
        ));
        let not_array = FakeRunner::printing(r#"{"windows": []}"#);
        assert!(matches!(
            find_window(&not_array, &exact(&["x"])).await,
            WindowMatch::Unavailable {
                reason: ToolError::Unparseable { .. }
            }
        ));
    }

    /// Mutation: send `--id` without the number, or the wrong action name -> red.
    #[tokio::test]
    async fn focus_sends_the_window_id() {
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Success,
            stdout: Vec::new(),
            stderr: String::new(),
        })]);
        focus_window(&runner, WindowId(11)).await.unwrap();
        assert_eq!(runner.lines(), ["niri msg action focus-window --id 11"]);
    }

    /// Mutation: ignore the exit code in `run_tool` -> red.
    #[tokio::test]
    async fn focus_reports_a_refusal() {
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Code(1),
            stdout: Vec::new(),
            stderr: "no such window".to_owned(),
        })]);
        assert_eq!(
            focus_window(&runner, WindowId(404)).await,
            Err(ToolError::NonZeroExit {
                program: "niri".into(),
                code: Some(1),
                stderr: "no such window".into()
            })
        );
    }
}
