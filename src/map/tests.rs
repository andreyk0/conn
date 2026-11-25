use std::num::NonZeroU32;
use std::path::PathBuf;

use super::*;
use crate::jira::WorkItemKey;
use crate::model::{LinkView, RowKind, StatusSection, WorkItemRow};
use crate::testutil::{ago, secs, session_id, session_view};
use crate::types::{BgState, JiraKey, JiraPrefix, ListingStatus};

fn key(n: u32) -> JiraKey {
    JiraKey::new(
        JiraPrefix::parse("ABC").unwrap(),
        NonZeroU32::new(n).unwrap(),
    )
}

/// The user's work item `ABC-<n>`.
fn mine(n: u32) -> WorkItemRow {
    WorkItemRow {
        key: WorkItemKey::Configured(key(n)),
        url: None,
        summary: Redacted::of("Mine"),
        creator: None,
        description: None,
        sessions: Vec::new(),
        comments: None,
        unanswered: None,
        kind: RowKind::Item,
        waits_on: Vec::new(),
        epic: None,
    }
}

fn jira(n: u32) -> LinkView {
    LinkView {
        link: Link::Jira { key: key(n) },
        url: SafeUrl::parse(&format!("https://example.atlassian.net/browse/ABC-{n}")).unwrap(),
        pr: None,
    }
}

fn live(n: u8) -> SessionView {
    session_view(
        n,
        Some("live"),
        Activity::Idle {
            ended_at: ago(30),
            listing: ListingStatus::Seen,
        },
    )
}

/// A session with no process, last active `days` days before `secs(0)`.
fn dormant(n: u8, days: i64) -> SessionView {
    let last = ago(days * 24 * 3600);
    let mut view = session_view(
        n,
        Some("dormant"),
        Activity::Dormant {
            last_seen: last,
            background: None,
        },
    );
    view.last_activity = Some(last);
    view
}

fn with(mut view: SessionView, links: &[LinkView], cwd: &str) -> SessionView {
    view.links = links.to_vec();
    view.session.cwd = Some(PathBuf::from(cwd));
    view
}

fn build_of(
    sessions: &[SessionView],
    dormant: &[SessionView],
    work_items: &WorkItemsView,
) -> WorkMap {
    build(&Sources {
        now: secs(0),
        sessions,
        dormant,
        inbox: &[],
        work_items,
        home: Some(Path::new("/home/dev")),
    })
}

fn ids(map: &WorkMap) -> Vec<SessionId> {
    map.sessions.iter().map(|node| node.id.clone()).collect()
}

/// Live sessions are active and come first; a dormant one is on the map, faded, for seven
/// days, the most recent first.
/// Mutation: compare `<` with `WINDOW`, drop the sort, or count a dormant one as active.
#[test]
fn the_map_holds_the_live_sessions_and_a_week_of_the_rest() {
    let map = build_of(
        &[live(1)],
        &[dormant(2, 6), dormant(3, 8), dormant(4, 1)],
        &WorkItemsView::NoJira,
    );
    assert_eq!(ids(&map), vec![session_id(1), session_id(4), session_id(2)]);
    let motion: Vec<Motion> = map.sessions.iter().map(|node| node.motion).collect();
    assert_eq!(motion, vec![Motion::Idle, Motion::Dormant, Motion::Dormant]);
}

/// A pinned dormant session stays on the map however long ago it was active, still dormant.
/// Mutation: drop the pin from `on_map`.
#[test]
fn a_pinned_dormant_session_stays_on_the_map_whatever_its_age() {
    let mut pinned = dormant(3, 30);
    pinned.pin = crate::model::Pin::Pinned(crate::model::PinRank::at(0));
    let map = build_of(
        &[live(1)],
        &[pinned, dormant(4, 30)],
        &WorkItemsView::NoJira,
    );
    assert_eq!(ids(&map), vec![session_id(1), session_id(3)]);
    let motion: Vec<Motion> = map.sessions.iter().map(|node| node.motion).collect();
    assert_eq!(motion, vec![Motion::Idle, Motion::Dormant]);
}

/// A card keeps a session on the map however long ago it last did anything, and so does
/// background work, which has no live process of its own.
/// Mutation: make `on_map` check the window for every motion.
#[test]
fn a_session_in_play_is_on_the_map_whatever_its_age() {
    let old = dormant(1, 30);
    for motion in [Motion::Blocked, Motion::Moving, Motion::Idle] {
        assert!(on_map(&old, motion, secs(0)), "{motion:?}");
    }
    assert!(!on_map(&old, Motion::Dormant, secs(0)));
}

/// What a session is doing: a wait on the user first, then what its card asks, then its
/// process. Written out from the rules on [`Motion`].
/// Mutation: let the process outrank the card (row 3), read a card with no ask yet as idle
/// (row 4), or leave an FYI card's dormant session dormant (row 6).
#[test]
fn motion_is_the_wait_on_the_user_then_the_card_then_the_process() {
    let idle = Activity::Idle {
        ended_at: ago(30),
        listing: ListingStatus::Seen,
    };
    let running = Activity::Running { since: ago(5) };
    let gone = Activity::Dormant {
        last_seen: ago(60),
        background: None,
    };
    let background = |state| Activity::Dormant {
        last_seen: ago(60),
        background: Some(state),
    };
    let card = |ask| Card::Pending { ask };
    let rows = [
        (background(BgState::Failed), Card::None, Motion::Dormant),
        (background(BgState::Working), Card::None, Motion::Dormant),
        (running.clone(), card(Some(Ask::Decide)), Motion::Blocked),
        (idle.clone(), card(None), Motion::Blocked),
        (
            idle.clone(),
            card(Some(Ask::Others)),
            Motion::WaitingOnOthers,
        ),
        (gone.clone(), card(Some(Ask::Nothing)), Motion::Idle),
        (idle.clone(), Card::ForRelease, Motion::AwaitingRelease),
        (running, card(Some(Ask::Nothing)), Motion::Moving),
        (idle, Card::None, Motion::Idle),
        (gone, Card::None, Motion::Dormant),
        (
            Activity::Unknown {
                reason: Redacted::of("no listing"),
            },
            Card::None,
            Motion::Unknown,
        ),
    ];
    for (row, (activity, card, expected)) in rows.into_iter().enumerate() {
        assert_eq!(motion_of(&activity, card), expected, "row {}", row + 1);
    }
}

/// The wait a card's session declared outranks the gist's reading of the card: a release
/// nobody runs is the user's release, one under way or any other wait is someone else's; with
/// no wait the gist's ask stands.
/// Mutation: let the gist's ask win over the wait, or read a running release as the user's.
#[test]
fn a_cards_wait_outranks_its_gist() {
    use crate::ledger::Slug;
    use crate::model::{ReleaseRun, Standing, Waiting};
    let release = |run| {
        Standing::Waiting(Waiting::Release {
            rollout: Slug::parse("checkout").unwrap(),
            run,
        })
    };
    let rows = [
        (release(ReleaseRun::NotStarted), Card::ForRelease),
        (
            release(ReleaseRun::Running),
            Card::Pending {
                ask: Some(Ask::Others),
            },
        ),
        (
            Standing::Waiting(Waiting::Other {
                on: "a review".to_owned(),
            }),
            Card::Pending {
                ask: Some(Ask::Others),
            },
        ),
        (
            Standing::Normal,
            Card::Pending {
                ask: Some(Ask::Act),
            },
        ),
    ];
    for (row, (standing, expected)) in rows.into_iter().enumerate() {
        let mut entry = crate::testutil::inbox_entry(1, crate::testutil::turn_card(1, secs(0)));
        entry.ask = Some(Ask::Act);
        entry.standing = standing;
        assert_eq!(Card::of(&entry), expected, "row {}", row + 1);
    }
}

fn pr_link(n: u32, reading: Option<Reading>) -> LinkView {
    LinkView {
        link: Link::PullRequest {
            owner: "example-org".to_owned(),
            repo: "example-repo".to_owned(),
            number: n,
        },
        url: SafeUrl::parse(&format!(
            "https://github.com/example-org/example-repo/pull/{n}"
        ))
        .unwrap(),
        pr: reading,
    }
}

fn open_pr() -> PrState {
    let pr = crate::gh::PullRequestRef::new("example-org", "example-repo", 42).unwrap();
    crate::gh::parse_pr_state(
        include_bytes!("../../tests/fixtures/gh/open_passing.json"),
        &pr,
    )
    .unwrap()
}

/// The open-passing fixture is ready and review-required; each row changes one field.
/// Mutation: let a pending check outrank a failing one (row 2), or call a draft waiting (row 5).
#[test]
fn a_pull_request_is_blocking_waiting_moving_or_done() {
    let with = |change: &dyn Fn(&mut PrState)| {
        let mut state = open_pr();
        change(&mut state);
        found_pr_progress(&state)
    };
    let rows = [
        with(&|_| {}),
        with(&|s| s.checks = Checks::Failing { count: 1 }),
        with(&|s| s.review = ReviewDecision::ChangesRequested),
        with(&|s| {
            s.review = ReviewDecision::Approved;
            s.checks = Checks::Pending { count: 2 };
        }),
        with(&|s| s.readiness = Readiness::Draft),
        with(&|s| s.review = ReviewDecision::Approved),
        with(&|s| s.state = PrLifecycle::Closed),
    ];
    assert_eq!(
        rows,
        [
            Progress::Waiting,
            Progress::Blocking,
            Progress::Blocking,
            Progress::Waiting,
            Progress::Moving,
            Progress::Moving,
            Progress::Done,
        ]
    );
}

/// Status names from several workflows, folded and read for the shared words.
/// Mutation: test "todo" before "block" ("Blocked - to do" would queue), or default to `Queued`.
#[test]
fn a_work_item_status_reads_as_progress() {
    for (status, expected) in [
        ("Blocked - to do", Progress::Blocking),
        ("On Hold", Progress::Blocking),
        ("In Progress", Progress::Moving),
        ("To Do", Progress::Queued),
        ("Backlog", Progress::Queued),
        ("Done", Progress::Done),
        ("In Review", Progress::Waiting),
        ("Dev", Progress::Waiting),
    ] {
        assert_eq!(status_progress(status), expected, "{status}");
    }
}

/// A shared pull request's progress is its reading's; an unread one is unknown; a work item's
/// is its status section's.
/// Mutation: ignore the reading (pull request rows unknown), or the section's status.
#[test]
fn an_anchor_carries_where_its_ticket_or_pull_request_stands() {
    let failing = {
        let mut state = open_pr();
        state.checks = Checks::Failing { count: 1 };
        state
    };
    let reading = Reading::Value {
        observed: crate::gh::Observed {
            answer: Answer::Found(failing),
            read_at: secs(0),
        },
        age: crate::types::Age::from_secs(0),
        failure: None,
    };
    let work_items = WorkItemsView::Listed {
        read_at: secs(0),
        sections: vec![StatusSection::new(
            Redacted::of("In Progress"),
            vec![mine(1)],
        )],
        failure: None,
    };
    let map = build_of(
        &[
            with(
                live(1),
                &[pr_link(7, Some(reading)), pr_link(8, None)],
                "/a",
            ),
            with(
                live(2),
                &[pr_link(7, None), pr_link(8, None), jira(1)],
                "/b",
            ),
        ],
        &[],
        &work_items,
    );
    let progress: Vec<Progress> = map.anchors.iter().map(|node| node.progress).collect();
    assert_eq!(
        progress,
        vec![Progress::Moving, Progress::Blocking, Progress::Unknown]
    );
}

/// An anchor joins two or more sessions, or is one of the user's work items; a session that
/// links a key twice counts once; the home directory is never an anchor.
/// Mutation: drop `SHARED_BY` (every link is drawn), drop the per-session dedup (ABC-1 joins
/// session 1 twice), or drop the home check.
#[test]
fn anchors_are_what_sessions_share_and_the_users_own_items() {
    let work_items = WorkItemsView::Listed {
        read_at: secs(0),
        sections: vec![StatusSection::new(
            Redacted::of("In Progress"),
            vec![mine(3)],
        )],
        failure: None,
    };
    let map = build_of(
        &[
            with(live(1), &[jira(1), jira(1), jira(2)], "/home/dev/repo"),
            with(live(2), &[jira(1)], "/home/dev/repo"),
            with(live(3), &[jira(3)], "/home/dev"),
            with(live(4), &[], "/home/dev"),
        ],
        &[],
        &work_items,
    );
    let anchors: Vec<(&Anchor, Vec<SessionId>)> = map
        .anchors
        .iter()
        .map(|node| (&node.anchor, node.sessions.clone()))
        .collect();
    assert_eq!(
        anchors,
        vec![
            (
                &Anchor::Link {
                    identifier: Identifier::of(&Link::Jira { key: key(3) }).unwrap(),
                    url: jira(3).url,
                    summary: Some(Redacted::of("Mine")),
                },
                vec![session_id(3)],
            ),
            (
                &Anchor::Link {
                    identifier: Identifier::of(&Link::Jira { key: key(1) }).unwrap(),
                    url: jira(1).url,
                    summary: None,
                },
                vec![session_id(1), session_id(2)],
            ),
            (
                &Anchor::Directory(PathBuf::from("/home/dev/repo")),
                vec![session_id(1), session_id(2)],
            ),
        ]
    );
}

/// An anchor is active when any of its sessions is.
/// Mutation: take the last session's recency instead of the most active.
#[test]
fn an_anchor_is_as_active_as_its_most_active_session() {
    let map = build_of(
        &[with(live(1), &[jira(1)], "/a")],
        &[with(dormant(2, 2), &[jira(1)], "/b")],
        &WorkItemsView::NoJira,
    );
    let recency: Vec<Recency> = map.anchors.iter().map(|node| node.recency).collect();
    assert_eq!(recency, vec![Recency::Active]);
    assert_eq!(
        map.anchors.first().map(|node| node.progress),
        Some(Progress::Unknown)
    );
}
