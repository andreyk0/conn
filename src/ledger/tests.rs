use jiff::tz::TimeZone;
use serde_json::json;

use super::command::{Context, IfOwned, Life, Refusal, Request, Sessions, decide, run};
use super::guard::{self, Verdict};
use super::store::{self, UpdateError};
use super::*;
use crate::testutil::Scratch;
use crate::types::{Instant, SessionId};

fn sid(n: u8) -> SessionId {
    SessionId::parse(&format!("{n:08x}-0000-0000-0000-000000000000")).unwrap()
}

fn at(minute: i64) -> Instant {
    Instant::from_second(1_793_900_000 + minute * 60).unwrap()
}

fn subject(text: &str) -> Subject {
    Subject::parse(text).unwrap()
}

fn slug(text: &str) -> Slug {
    Slug::parse(text).unwrap()
}

fn note(text: &str) -> Note {
    Note::parse(text).unwrap()
}

/// Sessions 1 and 2 run, 3 has ended, 4 could not be checked.
fn sessions() -> Sessions {
    let mut sessions = Sessions::default();
    sessions.add(sid(1), Life::Running, Some("one".to_owned()));
    sessions.add(sid(2), Life::Running, Some("two".to_owned()));
    sessions.add(sid(3), Life::Ended, None);
    sessions.add(sid(4), Life::Unknown("no domain".to_owned()), None);
    sessions
}

struct World {
    ledger: Ledger,
    sessions: Sessions,
    zone: TimeZone,
}

impl World {
    fn new() -> Self {
        Self {
            ledger: Ledger::default(),
            sessions: sessions(),
            zone: TimeZone::UTC,
        }
    }

    /// Decides the request for `who` at `minute`; applies what it appends.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the tests build each request in the call"
    )]
    fn act(&mut self, who: u8, minute: i64, request: Request) -> Result<String, Refusal> {
        let context = Context {
            sessions: &self.sessions,
            me: Some(sid(who)),
            now: at(minute),
            zone: &self.zone,
        };
        let (entries, answer) = decide(&request, &self.ledger, &sid(who), &context)?;
        for entry in entries {
            self.ledger.apply(entry);
        }
        Ok(answer)
    }
}

fn claim(text: &str, if_owned: IfOwned) -> Request {
    Request::Claim {
        subjects: vec![subject(text)],
        if_owned,
    }
}

/// Mutation: accept a lowercase or zero key, accept an uppercase or empty name, or drop a
/// prefix from the written form (the round trip breaks).
#[test]
fn a_subject_is_a_ticket_a_rollout_or_a_topic_and_reads_back_as_written() {
    for text in [
        "ABC-123",
        "AB_2-7",
        "rollout:checkout",
        "topic:vendor-follow-up.2",
    ] {
        assert_eq!(subject(text).to_string(), text);
        let encoded = serde_json::to_string(&subject(text)).unwrap();
        assert_eq!(
            serde_json::from_str::<Subject>(&encoded).unwrap(),
            subject(text)
        );
    }
    for text in [
        "abc-1",
        "ABC-0",
        "ABC-",
        "ABC-1x",
        "ABC",
        "rollout:",
        "rollout:CHECKOUT",
        "topic:two words",
        "-checkout",
        "ticket:ABC-1",
    ] {
        assert!(Subject::parse(text).is_err(), "{text} parsed");
    }
    assert!(Note::parse("   ").is_err());
    assert!(Note::parse(&"x".repeat(Note::CHARS_MAX + 1)).is_err());
    assert_eq!(note("  trimmed  ").as_str(), "trimmed");
}

/// A free subject is claimed; one a running session owns is refused with its owner named, and
/// nothing is written; an ended owner's, or a `--take`, passes it over and records from whom.
/// Mutation: let a claim through over a running owner, treat Unknown as Ended, write the
/// claims before an earlier subject's refusal, or drop `took_from`.
#[test]
fn a_running_owner_keeps_its_subject_unless_taken() {
    let mut world = World::new();
    assert_eq!(
        world.act(1, 0, claim("ABC-1", IfOwned::Refuse)),
        Ok("ABC-1 is yours.".to_owned())
    );
    assert_eq!(
        world.act(1, 1, claim("ABC-1", IfOwned::Refuse)),
        Ok("ABC-1 is already yours.".to_owned())
    );
    let refused = world
        .act(2, 2, claim("ABC-1", IfOwned::Refuse))
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("ABC-1 is owned by session 00000001 \"one\", running since"),
        "{refused}"
    );
    assert_eq!(
        world.ledger.owner(&subject("ABC-1")).unwrap().session,
        sid(1)
    );

    // Two subjects in one claim: the second is refused, so the first is not written either.
    let both = Request::Claim {
        subjects: vec![subject("ABC-2"), subject("ABC-1")],
        if_owned: IfOwned::Refuse,
    };
    assert!(world.act(2, 3, both).is_err());
    assert!(world.ledger.owner(&subject("ABC-2")).is_none());

    world.act(4, 4, claim("ABC-4", IfOwned::Refuse)).unwrap();
    assert!(world.act(2, 5, claim("ABC-4", IfOwned::Refuse)).is_err());

    world.act(3, 6, claim("ABC-3", IfOwned::Refuse)).unwrap();
    world.act(2, 7, claim("ABC-3", IfOwned::Refuse)).unwrap();
    world.act(2, 8, claim("ABC-1", IfOwned::Take)).unwrap();
    assert_eq!(
        world.ledger.claims_of(&sid(2)),
        [&subject("ABC-1"), &subject("ABC-3")]
    );
    assert!(world.ledger.entries().iter().any(|entry| matches!(
        &entry.act,
        Act::Claim { subject: s, took_from: Some(from) } if *s == subject("ABC-1") && *from == sid(1)
    )));

    let release = |text: &str| Request::Release {
        subjects: vec![subject(text)],
    };
    assert!(matches!(
        world.act(1, 9, release("ABC-1")),
        Err(Refusal::NotYours { .. })
    ));
    world.act(2, 10, release("ABC-1")).unwrap();
    assert!(world.ledger.owner(&subject("ABC-1")).is_none());
}

/// A wait on a rollout is over at the first finish after it began, not at an earlier one; a
/// wait for a time is over when the time comes; a wait in words never ends by itself.
/// Mutation: compare `>=` with the wait's start, or end a `For` wait at any finish.
#[test]
fn a_wait_ends_with_the_next_finish_of_its_rollout_or_at_its_time() {
    let mut world = World::new();
    let wait_for_checkout = Request::Wait {
        on: WaitOn::Rollout(slug("checkout")),
        then: note("re-run E2E"),
        ticket: None,
    };
    let start = Request::RolloutStart {
        rollout: slug("checkout"),
        ships: vec![subject("ABC-1")],
    };
    let finish = Request::RolloutFinish {
        rollout: slug("checkout"),
        outcome: Outcome::Shipped,
        note: None,
    };
    world.act(2, 0, start.clone()).unwrap();
    world.act(2, 1, finish.clone()).unwrap();
    // In the same minute, after the finish: that finish does not end the wait.
    world.act(1, 1, wait_for_checkout).unwrap();
    let state = world.ledger.wait_of(&sid(1), at(3)).unwrap();
    assert!(matches!(state, WaitState::Waiting { running: None, .. }));

    world.act(2, 4, start).unwrap();
    let state = world.ledger.wait_of(&sid(1), at(5)).unwrap();
    assert!(
        matches!(state, WaitState::Waiting { running: Some(run), .. } if run.session == sid(2))
    );

    let answer = world.act(2, 6, finish).unwrap();
    assert!(
        answer.contains("00000001 \"one\", running: then re-run E2E"),
        "{answer}"
    );
    let state = world.ledger.wait_of(&sid(1), at(7)).unwrap();
    assert!(
        matches!(state, WaitState::RolloutFinished { finish, .. } if finish.ships == [subject("ABC-1")])
    );

    world
        .act(
            3,
            0,
            Request::Wait {
                on: WaitOn::Until(at(60)),
                then: note("read the results"),
                ticket: None,
            },
        )
        .unwrap();
    assert!(!world.ledger.wait_of(&sid(3), at(59)).unwrap().is_over());
    assert!(world.ledger.wait_of(&sid(3), at(60)).unwrap().is_over());

    world
        .act(
            4,
            0,
            Request::Wait {
                on: WaitOn::For(note("the vendor to pick a time")),
                then: note("run the change"),
                ticket: None,
            },
        )
        .unwrap();
    assert!(!world.ledger.wait_of(&sid(4), at(10_000)).unwrap().is_over());
    world.act(4, 1, Request::Unwait).unwrap();
    assert!(world.ledger.wait_of(&sid(4), at(2)).is_none());
}

/// A free-text wait that names a known rollout as a word is kept, and the answer says to wait
/// on the rollout instead, with the same `then`; text naming none, or naming one only inside
/// another word, gets no such line.
/// Mutation, each ran red: match inside a word ("checkoutx"), match case-sensitively ("CHECKOUT" is
/// missed), keep a sentence's final dot ("checkout." is missed), or split on spaces only ("(checkout/"
/// is missed).
#[test]
fn a_free_text_wait_naming_a_rollout_is_pointed_at_it() {
    let mut world = World::new();
    world
        .act(
            2,
            0,
            Request::RolloutStart {
                rollout: slug("checkout"),
                ships: Vec::new(),
            },
        )
        .unwrap();
    world
        .act(
            2,
            1,
            Request::RolloutFinish {
                rollout: slug("checkout"),
                outcome: Outcome::Shipped,
                note: None,
            },
        )
        .unwrap();
    let wait_for = |text: &str| Request::Wait {
        on: WaitOn::For(note(text)),
        then: note("close ABC-1"),
        ticket: None,
    };
    for named in [
        "the user to ship CHECKOUT 0a1b2c3 to Prod",
        "a release of checkout.",
        "the user's release (checkout/0a1b2c3)",
    ] {
        let answer = world.act(1, 2, wait_for(named)).unwrap();
        assert!(
            answer.contains(
                "This names rollout:checkout. If you are waiting for that release, wait on the \
                 rollout instead, so the user's pages show a release waiting on them: conn wait \
                 rollout:checkout --then 'close ABC-1'"
            ),
            "{named}: {answer}"
        );
        assert!(
            matches!(world.ledger.wait_of(&sid(1), at(3)), Some(WaitState::Waiting { wait, .. }) if wait.on == WaitOn::For(note(named))),
            "the wait is kept as written"
        );
    }
    for unnamed in ["a checkoutx build", "the vendor to pick a time"] {
        let answer = world.act(1, 2, wait_for(unnamed)).unwrap();
        assert!(!answer.contains("This names"), "{unnamed}: {answer}");
    }
}

/// A release is pending while a session waits on its next finish and nobody runs it, whether
/// or not it ever ran; starting it makes it running with the same waiters, and its finish ends
/// the waits and leaves it quiet.
/// Mutation: count waits that are over as waiting (red, the release stays pending after its
/// finish); leave a rollout only waited on out of `releases` (red, checkout is missing).
#[test]
fn a_release_is_pending_while_sessions_wait_on_it_and_nobody_runs_it() {
    let mut world = World::new();
    let wait = |name: &str| Request::Wait {
        on: WaitOn::Rollout(slug(name)),
        then: note("carry on"),
        ticket: None,
    };
    world.act(1, 0, wait("checkout")).unwrap();
    world.act(3, 1, wait("checkout")).unwrap();
    world.act(2, 1, wait("portal")).unwrap();
    world.act(2, 2, Request::Unwait).unwrap();

    let releases = world.ledger.releases(at(3));
    let names: Vec<&str> = releases.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["checkout"],
        "only a rollout someone waits on or ran is listed"
    );
    assert!(matches!(
        &releases[0].1,
        Release::Pending { first, others, last: None } if **first == sid(1) && *others == [&sid(3)]
    ));

    world
        .act(
            2,
            4,
            Request::RolloutStart {
                rollout: slug("checkout"),
                ships: Vec::new(),
            },
        )
        .unwrap();
    let running = world.ledger.release(&slug("checkout"), at(5));
    assert!(matches!(&running, Release::Running { run, .. } if run.session == sid(2)));
    assert_eq!(running.waiting(), [&sid(1), &sid(3)]);

    world
        .act(
            2,
            6,
            Request::RolloutFinish {
                rollout: slug("checkout"),
                outcome: Outcome::Shipped,
                note: None,
            },
        )
        .unwrap();
    assert!(matches!(
        world.ledger.release(&slug("checkout"), at(7)),
        Release::Quiet { last: Some(_) }
    ));
}

/// A wait on a ticket lasts until it is released after the wait began, by whoever releases
/// it; an earlier release, or a release of another subject, does not end it.
/// Mutation: end it at any release of any subject, or at a release before the wait.
#[test]
fn a_wait_on_a_ticket_ends_when_its_owner_releases_it() {
    let mut world = World::new();
    let release = |text: &str| Request::Release {
        subjects: vec![subject(text)],
    };
    world.act(2, 0, claim("ABC-1", IfOwned::Refuse)).unwrap();
    world.act(2, 1, release("ABC-1")).unwrap();
    world.act(2, 2, claim("ABC-1", IfOwned::Refuse)).unwrap();
    world.act(2, 2, claim("ABC-2", IfOwned::Refuse)).unwrap();
    let answer = world
        .act(
            1,
            3,
            Request::Wait {
                on: WaitOn::Subject(subject("ABC-1")),
                then: note("deploy"),
                ticket: None,
            },
        )
        .unwrap();
    assert!(
        answer.contains("ABC-1 is owned by session 00000002 \"two\", running; the wait is over when it releases it."),
        "{answer}"
    );
    world.act(2, 4, release("ABC-2")).unwrap();
    assert!(!world.ledger.wait_of(&sid(1), at(5)).unwrap().is_over());
    world.act(2, 6, release("ABC-1")).unwrap();
    let state = world.ledger.wait_of(&sid(1), at(7)).unwrap();
    assert!(matches!(state, WaitState::Released { by, .. } if *by == sid(2)));
}

/// A second rollout of the same thing is refused while the first runs in a session that may
/// still be running; only its runner finishes it; a rollout that ships a ticket another
/// session owns says to report to that owner.
/// Mutation: drop the running check, let anyone finish, or skip `owners_to_tell`.
#[test]
fn one_rollout_of_a_thing_runs_at_a_time_and_reports_to_ticket_owners() {
    let mut world = World::new();
    world.act(1, 0, claim("ABC-9", IfOwned::Refuse)).unwrap();
    let start = |ships: Vec<Subject>| Request::RolloutStart {
        rollout: slug("portal"),
        ships,
    };
    let answer = world.act(2, 1, start(vec![subject("ABC-9")])).unwrap();
    assert!(
        answer.contains("ABC-9 is owned by session 00000001 \"one\", running: report this rollout"),
        "{answer}"
    );
    assert!(matches!(
        world.act(1, 2, start(Vec::new())),
        Err(Refusal::RolloutRunning { .. })
    ));
    assert!(matches!(
        world.act(2, 2, start(Vec::new())),
        Err(Refusal::RolloutRunning { .. })
    ));
    let finish = Request::RolloutFinish {
        rollout: slug("portal"),
        outcome: Outcome::RolledBack,
        note: Some(note("errors on the first export")),
    };
    assert!(matches!(
        world.act(1, 3, finish.clone()),
        Err(Refusal::NotYourRollout { .. })
    ));
    world.act(2, 4, finish).unwrap();
    let rollout = world.ledger.rollout(&slug("portal")).unwrap();
    assert!(rollout.running.is_none());
    assert_eq!(
        rollout.last_finish.as_ref().unwrap().outcome,
        Outcome::RolledBack
    );

    // A run whose session has ended is replaced, and the answer says so.
    world.act(3, 5, start(Vec::new())).unwrap();
    let answer = world.act(1, 6, start(Vec::new())).unwrap();
    assert!(
        answer.starts_with("The run started by session 00000003"),
        "{answer}"
    );
}

/// Through the file: a claim is appended and read back; a refused one appends nothing; a bad
/// or torn line is reported in its place and the good lines still count.
/// Mutation: append on refusal, or stop the parse at the first bad line.
#[test]
fn the_file_keeps_what_was_decided_and_names_every_bad_line() {
    let dir = Scratch::new("ledger");
    let path = dir.join(LEDGER_FILE);
    let sessions = sessions();
    let zone = TimeZone::UTC;
    let context = |me: u8, minute: i64| Context {
        sessions: &sessions,
        me: Some(sid(me)),
        now: at(minute),
        zone: &zone,
    };
    run(&claim("ABC-1", IfOwned::Refuse), &path, &context(1, 0)).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(run(&claim("ABC-1", IfOwned::Refuse), &path, &context(2, 1)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let mut bytes = before;
    bytes.extend_from_slice(b"{\"not\": \"an entry\"}\n\n");
    let torn = serde_json::to_vec(&Entry {
        at: at(2),
        session: sid(2),
        act: Act::Claim {
            subject: subject("ABC-2"),
            took_from: None,
        },
    })
    .unwrap();
    bytes.extend_from_slice(&torn);
    let loaded = store::parse(&bytes);
    assert_eq!(
        loaded.ledger.owner(&subject("ABC-1")).unwrap().session,
        sid(1)
    );
    assert!(loaded.ledger.owner(&subject("ABC-2")).is_none());
    let lines: Vec<usize> = loaded.bad_lines.iter().map(|bad| bad.line).collect();
    assert_eq!(lines, [2, 4]);

    let refused: Result<(), UpdateError<&str>> = store::update(&path, |_| Err("no"));
    assert!(matches!(refused, Err(UpdateError::Refused("no"))));

    let who = run(&Request::Who { subject: None }, &path, &context(2, 3)).unwrap();
    assert!(
        who.starts_with("Owned:\n- ABC-1: 00000001 \"one\", running since"),
        "{who}"
    );
}

fn defer(ticket: &str, minute: i64, check: &str) -> Request {
    Request::Defer {
        ticket: JiraKey::parse(ticket).unwrap(),
        until: at(minute),
        check: note(check),
    }
}

const DAY: i64 = 24 * 60;

/// A deferral holds a ticket's close check off until its time, which counts as come at that
/// instant; a later deferral replaces the earlier and says so; one for a time already past is
/// refused, since the check is due. `who` names it while it holds.
/// Mutation: compare with `<` in place of `<=` (red, at the instant); keep the first deferral
/// (red); accept a past time (red); leave it out of `who` (red).
#[test]
fn a_deferral_holds_a_close_check_off_until_its_time() {
    let mut world = World::new();
    let key = JiraKey::parse("ABC-1").unwrap();
    assert_eq!(world.ledger.close_check(&key, at(0)), CloseCheck::Open);

    let answer = world
        .act(1, 0, defer("ABC-1", DAY, "a day of clean error rates"))
        .unwrap();
    assert_eq!(
        answer,
        "ABC-1's close check is deferred until Nov 6 17:33; then check: a day of clean error rates"
    );
    assert!(matches!(
        world.ledger.close_check(&key, at(DAY - 1)),
        CloseCheck::Deferred(d) if d.until == at(DAY)
    ));
    assert!(matches!(
        world.ledger.close_check(&key, at(DAY)),
        CloseCheck::Reached(d) if d.check.as_str() == "a day of clean error rates"
    ));

    let answer = world
        .act(2, 10, defer("ABC-1", 2 * DAY, "two days"))
        .unwrap();
    assert!(
        answer.starts_with(
            "This replaces the earlier deferral to Nov 6 17:33: a day of clean error rates."
        ),
        "{answer}"
    );
    assert!(matches!(
        world.ledger.close_check(&key, at(DAY)),
        CloseCheck::Deferred(d) if d.until == at(2 * DAY) && d.by == sid(2)
    ));

    assert_eq!(
        world.act(1, 20, defer("ABC-1", 20, "now")),
        Err(Refusal::DeferPast {
            ticket: key.clone(),
            until: "Nov 5 17:53".to_owned(),
        })
    );

    let dir = Scratch::new("ledger-defer");
    let path = dir.join(LEDGER_FILE);
    let zone = TimeZone::UTC;
    let context = |minute: i64| Context {
        sessions: &world.sessions,
        me: Some(sid(1)),
        now: at(minute),
        zone: &zone,
    };
    run(
        &defer("ABC-1", DAY, "the invoices match"),
        &path,
        &context(0),
    )
    .unwrap();
    let ticket = Request::Who {
        subject: Some(subject("ABC-1")),
    };
    assert_eq!(
        run(&ticket, &path, &context(0)).unwrap(),
        "ABC-1: no owner.\nABC-1's close check is deferred until Nov 6 17:33 by session 00000001 \"one\", running: the invoices match"
    );
    assert_eq!(
        run(&Request::Who { subject: None }, &path, &context(0)).unwrap(),
        "Close checks deferred:\n- ABC-1's close check is deferred until Nov 6 17:33 by session 00000001 \"one\", running: the invoices match"
    );
    assert_eq!(
        run(&Request::Who { subject: None }, &path, &context(DAY)).unwrap(),
        "The ledger is empty: nothing is claimed, waited on, rolling out or deferred."
    );
}

/// A deferral is written as the ticket's key, and a line with something else there is a bad
/// line, not a deferral.
/// Mutation: serialize the key as its parts (red, the line differs).
#[test]
fn a_deferral_is_one_line_naming_its_ticket() {
    let entry = Entry {
        at: at(0),
        session: sid(1),
        act: Act::Defer {
            ticket: JiraKey::parse("ABC-1").unwrap(),
            until: at(DAY),
            check: note("the invoices match"),
        },
    };
    let line = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        line["act"],
        json!({"defer": {"ticket": "ABC-1", "until": "2026-11-06T17:33:20Z", "check": "the invoices match"}})
    );
    let mut bad = line;
    bad["act"]["defer"]["ticket"] = json!("abc-1");
    let loaded = store::parse(format!("{bad}\n").as_bytes());
    assert_eq!(loaded.bad_lines.len(), 1);
}

/// A start teaches the ledger and lists what the session owns; a prompt says nothing unless the
/// session waits, says it is waiting while it does, and says the wait is over once its rollout
/// finished.
/// Mutation: drop the guidance at a start, speak at a prompt with no wait, or word an ended
/// wait as still waiting.
#[test]
fn the_hook_tells_a_session_what_it_owns_and_when_its_wait_is_over() {
    use super::remind::{Moment, text};
    let mut world = World::new();
    world.act(1, 0, claim("ABC-1", IfOwned::Refuse)).unwrap();
    let say = |world: &World, moment: Moment, minute: i64| {
        let context = Context {
            sessions: &world.sessions,
            me: Some(sid(1)),
            now: at(minute),
            zone: &world.zone,
        };
        text(moment, &world.ledger, &sid(1), &context)
    };
    let start = say(&world, Moment::Start, 1).unwrap();
    assert!(
        start.starts_with("conn: sessions on this machine coordinate"),
        "{start}"
    );
    assert!(start.ends_with("This session owns: ABC-1."), "{start}");
    assert_eq!(say(&world, Moment::Prompt, 1), None);

    world
        .act(
            1,
            2,
            Request::Wait {
                on: WaitOn::Rollout(slug("checkout")),
                then: note("move ABC-1"),
                ticket: None,
            },
        )
        .unwrap();
    let waiting = say(&world, Moment::Prompt, 3).unwrap();
    assert!(
        waiting.starts_with("conn: this session is waiting on rollout:checkout since"),
        "{waiting}"
    );
    assert!(
        waiting.contains("(not started); then: move ABC-1."),
        "{waiting}"
    );

    world
        .act(
            2,
            4,
            Request::RolloutFinish {
                rollout: slug("checkout"),
                outcome: Outcome::Shipped,
                note: None,
            },
        )
        .unwrap();
    let over = say(&world, Moment::Prompt, 5).unwrap();
    assert!(
        over.starts_with("conn: the wait on rollout:checkout is over: shipped"),
        "{over}"
    );
    assert!(
        over.ends_with("next: move ABC-1. Run `conn wait --clear` once you have picked it up."),
        "{over}"
    );
}

/// A ticket two sessions wrote to is shared; so is one a session wrote while another owned it,
/// even alone; a ticket only its owner or one unclaimed session wrote is not; a thread two
/// sessions replied in is shared, a channel post outside any thread never is; writes before the
/// window do not count; ownership is judged at the time of the write.
/// Mutation: judge ownership now rather than then, count channel posts, keep writes before the
/// window, or list a single unclaimed writer.
#[test]
fn shared_targets_are_the_ones_several_sessions_or_a_non_owner_wrote() {
    use super::shared::{OwnerThen, find};
    use crate::types::JiraKey;
    use crate::types::outward::{Outward, Target, WriteKind, Written};
    let ticket = |key: &str| Target::Ticket(JiraKey::parse(key).unwrap());
    let written = |minute: i64, target: Target| Written {
        at: at(minute),
        write: Outward {
            target,
            kind: WriteKind::Comment,
            excerpt: None,
        },
    };
    let mut world = World::new();
    // Session 1 claims ABC-2 at minute 10.
    world.act(1, 10, claim("ABC-2", IfOwned::Refuse)).unwrap();
    let thread = || Target::Slack {
        channel: "C1".to_owned(),
        thread: Some("1.1".to_owned()),
    };
    let channel = || Target::Slack {
        channel: "C1".to_owned(),
        thread: None,
    };
    let writes = [
        (sid(1), written(1, ticket("ABC-1"))),
        (sid(2), written(2, ticket("ABC-1"))),
        // Before the claim: unowned, and session 2 alone, so not shared by itself...
        (sid(2), written(5, ticket("ABC-3"))),
        // ...but after the claim, session 2 writes ABC-2 while session 1 owns it.
        (sid(2), written(11, ticket("ABC-2"))),
        (sid(1), written(12, ticket("ABC-2"))),
        (sid(1), written(3, thread())),
        (sid(2), written(4, thread())),
        (sid(1), written(3, channel())),
        (sid(2), written(4, channel())),
        // Outside the window.
        (sid(1), written(-100, ticket("ABC-9"))),
        (sid(2), written(-100, ticket("ABC-9"))),
    ];
    let shared = find(
        writes.iter().map(|(session, written)| (session, written)),
        &world.ledger,
        at(0),
    );
    let targets: Vec<String> = shared.iter().map(|s| s.target.to_string()).collect();
    // The most recently written first.
    assert_eq!(targets, ["ABC-2", "slack:C1/1.1", "ABC-1"]);
    let abc2 = shared.first().unwrap();
    assert_eq!(
        abc2.writes
            .iter()
            .map(|w| w.owner_then.clone())
            .collect::<Vec<_>>(),
        [OwnerThen::Other(sid(1)), OwnerThen::Writer]
    );

    // A non-owner's write alone is enough.
    let alone = [(sid(2), written(11, ticket("ABC-2")))];
    let shared = find(
        alone.iter().map(|(session, written)| (session, written)),
        &world.ledger,
        at(0),
    );
    assert_eq!(shared.len(), 1);
    assert_eq!(shared.first().unwrap().writers(), [&sid(2)]);

    // Once session 1 releases, a later write by session 2 is no one's business.
    world
        .act(
            1,
            20,
            Request::Release {
                subjects: vec![subject("ABC-2")],
            },
        )
        .unwrap();
    let later = [(sid(2), written(21, ticket("ABC-2")))];
    assert_eq!(
        find(
            later.iter().map(|(session, written)| (session, written)),
            &world.ledger,
            at(0)
        ),
        []
    );
}

/// Mutation: keep a title that repeats the id, or drop the directory fallback.
#[test]
fn a_session_is_named_by_its_title_unless_the_title_is_its_id() {
    use super::command::session_name;
    use std::path::Path;
    let id = sid(1);
    let cwd = Path::new("/work/public-api");
    assert_eq!(
        session_name(Some("API release"), &id, cwd).as_deref(),
        Some("API release")
    );
    let id_title = format!("Claude: {id}");
    assert_eq!(
        session_name(Some(&id_title), &id, cwd).as_deref(),
        Some("public-api")
    );
    assert_eq!(session_name(None, &id, cwd).as_deref(), Some("public-api"));
    assert_eq!(session_name(None, &id, Path::new("/")), None);
}

/// The guard names the ticket of a Jira write and of nothing else, refuses a write to a
/// ticket another session that may be running owns, and lets the owner, an ended owner's
/// ticket and an unowned one through.
/// Mutation: match any tool name, read the key from another field, deny the owner, or deny
/// over an ended owner.
#[test]
fn the_guard_stops_a_write_to_a_ticket_another_running_session_owns() {
    let payload = |tool: &str, key: &str| json!({ "session_id": sid(2).as_str(), "tool_name": tool, "tool_input": { "issueIdOrKey": key } });
    let comment = "mcp__claude_ai_Atlassian__addCommentToJiraIssue";
    assert_eq!(
        guard::written_ticket(&payload(comment, "ABC-1")),
        Some(subject("ABC-1"))
    );
    assert_eq!(
        guard::written_ticket(&payload("mcp__x__transitionJiraIssue", " ABC-1 ")),
        Some(subject("ABC-1"))
    );
    assert_eq!(
        guard::written_ticket(&payload("mcp__x__getJiraIssue", "ABC-1")),
        None
    );
    assert_eq!(guard::written_ticket(&payload(comment, "10042")), None);
    assert_eq!(
        guard::written_ticket(&json!({ "tool_name": comment })),
        None
    );

    let mut world = World::new();
    world.act(1, 0, claim("ABC-1", IfOwned::Refuse)).unwrap();
    world.act(3, 0, claim("ABC-3", IfOwned::Refuse)).unwrap();
    let verdict = |ticket: &str, me: u8| {
        guard::decide(
            &world.ledger,
            &subject(ticket),
            Some(&sid(me)),
            &world.sessions,
            &world.zone,
        )
    };
    let Verdict::Deny(reason) = verdict("ABC-1", 2) else {
        panic!("not denied");
    };
    assert!(
        reason.contains("Send what you have to that session"),
        "{reason}"
    );
    assert_eq!(verdict("ABC-1", 1), Verdict::Allow);
    assert_eq!(verdict("ABC-3", 2), Verdict::Allow);
    assert_eq!(verdict("ABC-7", 2), Verdict::Allow);
}

fn closed(keys: &[&str]) -> crate::jira::resolution::ClosedTickets {
    use crate::jira::resolution::{Closed, ClosedTickets, Resolution};
    let mut tickets = ClosedTickets::default();
    for key in keys {
        tickets.record(
            JiraKey::parse(key).unwrap(),
            Resolution::Closed(Closed {
                resolution: "Done".to_owned(),
                status: "Done".to_owned(),
                at: Some(at(30)),
            }),
        );
    }
    tickets
}

/// A wait on a ticket is over once Jira has the ticket closed; a release stays the reason a
/// wait is over when there is one; only standing waits name tickets to watch, and none once
/// their tickets are known closed.
/// Mutation: let a closed ticket override a release (session 2 reads closed), or list over
/// waits as watched (ABC-3 is listed).
#[test]
fn a_closed_ticket_ends_the_waits_on_it_and_about_it() {
    let mut world = World::new();
    world
        .act(
            1,
            0,
            Request::Wait {
                on: WaitOn::Subject(subject("ABC-1")),
                then: note("deploy"),
                ticket: None,
            },
        )
        .unwrap();
    world
        .act(
            2,
            0,
            Request::Wait {
                on: WaitOn::Until(at(600)),
                then: note("re-check the reaper; close ABC-2"),
                ticket: Some(JiraKey::parse("ABC-2").unwrap()),
            },
        )
        .unwrap();
    world.act(3, 0, claim("ABC-3", IfOwned::Refuse)).unwrap();
    world
        .act(
            2,
            1,
            Request::Wait {
                on: WaitOn::Subject(subject("ABC-3")),
                then: note("rebase"),
                ticket: None,
            },
        )
        .unwrap();
    world
        .act(
            3,
            5,
            Request::Release {
                subjects: vec![subject("ABC-3")],
            },
        )
        .unwrap();
    let mut state = World::new();
    std::mem::swap(&mut state.ledger, &mut world.ledger);

    let now = at(60);
    let watched: Vec<String> = state
        .ledger
        .watched_tickets(now)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        watched,
        ["ABC-1"],
        "session 2's wait on ABC-3 is over by its release"
    );

    let ledger = state
        .ledger
        .knowing_closed(closed(&["ABC-1", "ABC-2", "ABC-3"]));
    assert!(matches!(
        ledger.wait_of(&sid(1), now),
        Some(WaitState::TicketClosed { ticket, .. }) if ticket.to_string() == "ABC-1"
    ));
    assert!(matches!(
        ledger.wait_of(&sid(2), now),
        Some(WaitState::Released { .. })
    ));
    assert_eq!(ledger.watched_tickets(now), Vec::<JiraKey>::new());
}

/// A time wait about a ticket closes with it before its time, and its words say so; a wait
/// line written before `ticket` existed still reads.
/// Mutation: drop `ticket` from `Wait::ticket` for time waits (red), or make the field
/// required (the old line fails).
#[test]
fn a_time_wait_about_a_ticket_ends_when_it_closes_and_old_lines_still_read() {
    let mut world = World::new();
    world
        .act(
            2,
            0,
            Request::Wait {
                on: WaitOn::Until(at(600)),
                then: note("close ABC-2"),
                ticket: Some(JiraKey::parse("ABC-2").unwrap()),
            },
        )
        .unwrap();
    let ledger = std::mem::take(&mut world.ledger);
    let now = at(60);
    assert!(matches!(
        ledger.wait_of(&sid(2), now),
        Some(WaitState::Waiting { .. })
    ));
    let ledger = ledger.knowing_closed(closed(&["ABC-2"]));
    let state = ledger.wait_of(&sid(2), now).unwrap();
    assert!(matches!(state, WaitState::TicketClosed { .. }));
    let context = Context {
        sessions: &world.sessions,
        me: Some(sid(2)),
        now,
        zone: &world.zone,
    };
    let words = super::command::wait_state_words(state, &context);
    assert!(words.contains("is over: ABC-2 closed (Done"), "{words}");

    let old = json!({"at": "2026-11-10T20:16:00Z", "session": sid(2).to_string(),
        "act": {"wait": {"on": {"until": "2026-11-16T20:16:00Z"}, "then": "re-check"}}});
    let loaded = store::parse(format!("{old}\n").as_bytes());
    assert_eq!(loaded.bad_lines, []);
}
