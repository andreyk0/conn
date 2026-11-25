//! Builders the tests of several modules share.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::gist::GistView;
use crate::model::{InboxEntry, Pin, Rank, Seen, SessionView, Standing};
use crate::types::{
    Activity, AskUserQuestion, AssistantText, BackgroundTasks, Host, Instant, LinkTargets,
    MultiSelect, PendingCard, Question, ResumeCommand, Session, SessionId, SessionKind, TurnEnd,
    TurnTrigger,
};
use crate::util::redact::Redacted;

/// A fresh, empty directory of one test's own under the system temp directory (which a run may
/// point elsewhere with `TMPDIR`), removed on drop. Its name carries the label, the process and
/// a counter, so no two tests share one, whether they run at once or reuse a label.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "conn-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a leftover scratch directory is not a test failure.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The instant a test names as RFC 3339 text.
pub(crate) fn at(text: &str) -> Instant {
    text.parse().unwrap()
}

/// `secs` seconds after the Unix epoch.
pub(crate) fn epoch(secs: i64) -> Instant {
    Instant::from_second(secs).unwrap()
}

/// `offset` seconds after a fixed base instant (2027-01-15T08:00:00Z), for tests that think in
/// offsets.
pub(crate) fn secs(offset: i64) -> Instant {
    Instant::from_second(1_800_000_000 + offset).unwrap()
}

/// `secs_before` seconds before the base instant of [`secs`], for tests whose "now" is that
/// instant and which think in ages.
pub(crate) fn ago(secs_before: i64) -> Instant {
    secs(-secs_before)
}

/// The `n`th test session: `00000000-0000-4000-8000-0000000000nn`.
pub(crate) fn session_id(n: u8) -> SessionId {
    SessionId::parse(&format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
}

/// A question with one single-select question and no options.
pub(crate) fn asked() -> AskUserQuestion {
    AskUserQuestion::new(vec![Question {
        header: None,
        question: Redacted::of("which one?"),
        multi_select: MultiSelect::One,
        options: vec![],
    }])
    .unwrap()
}

/// The hosts links are built against in tests: `example.atlassian.net` and `example.slack.com`.
pub(crate) fn link_targets() -> LinkTargets {
    LinkTargets {
        jira: Some(Host::parse("example.atlassian.net").unwrap()),
        slack: Some(Host::parse("example.slack.com").unwrap()),
    }
}

/// The inbox entry of `card` for the `n`th test session, as the builder makes it before the
/// ledger places it: no title, links or gist, unseen, [`Standing::Normal`].
pub(crate) fn inbox_entry(n: u8, card: PendingCard) -> InboxEntry {
    InboxEntry {
        session: session_id(n),
        title: None,
        rank: Rank::of(&card),
        standing: Standing::Normal,
        seen: Seen::Unseen,
        now_line: None,
        links: Vec::new(),
        gist: GistView::Pending,
        ask: None,
        card,
    }
}

/// A finished turn of the `n`th test session, by the human, pending since `since`.
pub(crate) fn turn_card(n: u8, since: Instant) -> PendingCard {
    PendingCard::Turn {
        session: session_id(n),
        turn: TurnEnd {
            at: since,
            trigger: TurnTrigger::Human,
            text: AssistantText::of(""),
            background_tasks: BackgroundTasks::Unknown,
            asked: None,
        },
        since,
    }
}

/// The `n`th test session as a view: working in `/home/dev/proj-<n>`, titled `title`, with no
/// links, turn ends or card, last active 30 s before [`secs`]`(0)`.
pub(crate) fn session_view(n: u8, title: Option<&str>, activity: Activity) -> SessionView {
    let cwd = std::path::PathBuf::from(format!("/home/dev/proj-{n}"));
    SessionView {
        title: title.map(Redacted::of),
        resume: Ok(ResumeCommand::new(cwd.clone(), vec![], session_id(n)).unwrap()),
        pending: None,
        links: Vec::new(),
        last_turn_ends: Vec::new(),
        last_activity: Some(ago(30)),
        pin: Pin::Unpinned,
        gist: None,
        session: Session {
            id: session_id(n),
            cwd: Some(cwd),
            ai_title: title.map(str::to_owned),
            window_title: title.map(str::to_owned),
            name: None,
            kind: SessionKind::Interactive,
            activity,
        },
    }
}
