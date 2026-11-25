//! The site driven through its router with `tower::ServiceExt::oneshot`: the admission matrix,
//! one rendered page per route from a fixture model built in code, and the changes against a
//! real store in a scratch directory.
//!
//! The fixture is placeholder data throughout (`example.atlassian.net`, generated ids). Every
//! expected string is written out by hand from what the page should say; where one is a
//! fragment of generated HTML it is the smallest fragment that pins the behaviour.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use jiff::tz::TimeZone;
use tower::ServiceExt;

use super::auth::Tokens;
use super::{App, Parts, router};
use crate::config::Config;
use crate::gh::{
    Answer, Checks, Failed, GhError, Observed, PrLifecycle, PrState, Readiness, Reading,
    ReviewDecision,
};
use crate::gist::{Ask, Gist, GistError, GistView};
use crate::jira::WorkItemKey;
use crate::links::Extractor;
use crate::model::{
    ApprovalRow, CardFlag, CardVerdict, FollowFault, FollowFaults, HeldPrompt, InboxEntry,
    LinkView, Model, Pin, Rank, Reaction, ReviewCard, ReviewsView, ScanProgress, Seen, SessionView,
    SourceState, StatusSection, Teaser, WorkItemRow, WorkItemsFailure, WorkItemsFault,
    WorkItemsView,
};
use crate::state::{self, Handle};
use crate::testutil::{Scratch, ago as at, session_id as sid};
use crate::types::{
    Activity, Age, AskUserQuestion, AssistantText, BackgroundTasks, Instant, JiraKey, JiraPrefix,
    Link, ListingStatus, MultiSelect, PendingCard, Question, QuestionOption, ResumeCommand,
    SafeUrl, Session, SessionKind, TurnEnd, TurnTrigger, WaitingFor,
};
use crate::util::redact::Redacted;
use crate::util::runner::{Exit, FakeRunner, Output, ToolError};

const READ_TOKEN: &str = "tok-0123456789abcdef";
const WRITE_TOKEN: &str = "wtk-fedcba9876543210";
const PORT: u16 = 7357;
const NIRI_WINDOWS: &str = include_str!("../../tests/fixtures/sessions/niri_windows.json");
/// A made-up credential in the shape the redaction patterns know.
const SECRET: &str = "ghp_aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";

fn targets() -> crate::types::LinkTargets {
    config("/nonexistent").link_targets()
}

const JIRA_SECTION: &str = "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n";

fn config(dir: &str) -> Config {
    config_with(dir, JIRA_SECTION)
}

/// The test config with `jira` as its Jira section, which may be empty.
fn config_with(dir: &str, jira: &str) -> Config {
    let text = format!(
        "{jira}[slack]\nhost = \"example.slack.com\"\n[paths]\nclaude_dir = \"{dir}/claude\"\nstate_dir = \"{dir}/state\"\n[files]\nroots = [\"{dir}/files\"]\n"
    );
    Config::parse(&text, Path::new("/cfg.toml"), Path::new("/home/dev")).unwrap()
}

fn jira(n: u32) -> Link {
    Link::Jira {
        key: JiraKey::new(
            JiraPrefix::parse("ABC").unwrap(),
            NonZeroU32::new(n).unwrap(),
        ),
    }
}

fn pr(number: u32) -> Link {
    Link::PullRequest {
        owner: "example-org".to_owned(),
        repo: "example-repo".to_owned(),
        number,
    }
}

fn link_view(link: Link, reading: Option<Reading>) -> LinkView {
    LinkView {
        url: link.url(&targets()).unwrap(),
        link,
        pr: reading,
    }
}

fn pr_state() -> PrState {
    PrState {
        title: "placeholder".to_owned(),
        state: PrLifecycle::Open,
        readiness: Readiness::Ready,
        review: ReviewDecision::Approved,
        checks: Checks::Passing,
        updated_at: at(600),
        url: SafeUrl::parse("https://github.com/example-org/example-repo/pull/42").unwrap(),
    }
}

fn good_reading(failure: Option<Failed>) -> Reading {
    Reading::Value {
        observed: Observed {
            answer: Answer::Found(pr_state()),
            read_at: at(180),
        },
        age: Age::from_secs(180),
        failure,
    }
}

fn turn_end(secs_before_now: i64, trigger: TurnTrigger, text: &str) -> TurnEnd {
    TurnEnd {
        at: at(secs_before_now),
        trigger,
        text: AssistantText::of(text),
        background_tasks: BackgroundTasks::Unknown,
        asked: None,
    }
}

fn session(n: u8, title: &str, activity: Activity) -> Session {
    Session {
        id: sid(n),
        cwd: Some(PathBuf::from(format!("/home/dev/proj-{n}"))),
        ai_title: Some(title.to_owned()),
        window_title: Some(title.to_owned()),
        name: None,
        kind: SessionKind::Interactive,
        activity,
    }
}

fn view(session: Session, pending: Option<PendingCard>) -> SessionView {
    SessionView {
        title: session.ai_title.as_deref().map(Redacted::of),
        resume: Ok(
            ResumeCommand::new(session.cwd.clone().unwrap(), vec![], session.id.clone()).unwrap(),
        ),
        pending,
        links: Vec::new(),
        last_turn_ends: Vec::new(),
        last_activity: Some(at(30)),
        pin: Pin::Unpinned,
        gist: None,
        session,
    }
}

fn entry(view: &SessionView, seen: Seen, now_line: Option<&str>) -> InboxEntry {
    let card = view.pending.clone().unwrap();
    InboxEntry {
        session: view.session.id.clone(),
        title: view.title.clone(),
        rank: Rank::of(&card),
        standing: crate::model::Standing::Normal,
        seen,
        now_line: now_line.map(Redacted::of),
        links: view.links.clone(),
        gist: GistView::Pending,
        ask: None,
        card,
    }
}

fn question_card(n: u8, secs: i64) -> PendingCard {
    PendingCard::Asked {
        session: sid(n),
        asked: AskUserQuestion::new(vec![Question {
            header: Some(Redacted::of("Scope")),
            question: Redacted::of("Which parts should change?"),
            multi_select: MultiSelect::Many,
            options: vec![
                QuestionOption {
                    label: Redacted::of("The parser"),
                    description: Some(Redacted::of(&format!("only the parser {SECRET}"))),
                },
                QuestionOption {
                    label: Redacted::of("Everything"),
                    description: None,
                },
            ],
        }])
        .unwrap(),
        since: at(secs),
    }
}

fn all_read(model: &mut Model) {
    let ok = SourceState::Ok { at: at(5) };
    let health = &mut model.health;
    health.listing = ok.clone();
    health.registry = ok.clone();
    health.event_log = ok.clone();
    health.seen = ok.clone();
    model.coordination.state = ok.clone();
    health.transcripts.discovery = ok;
    health.transcripts.progress = ScanProgress::Complete;
}

/// The model every page test renders: a blocked session (permission prompt), a question, a
/// finished turn with links and a pull request, a watching one, a running one, an unknown
/// one, a seen card, two dormant sessions.
#[allow(
    clippy::too_many_lines,
    reason = "the fixture is one model, read top to bottom"
)]
fn fixture() -> Model {
    let mut model = Model::unread(at(4));
    all_read(&mut model);

    // 1: blocked on a permission prompt; its title is the one niri knows.
    let wait = PendingCard::Blocked {
        session: sid(1),
        on: WaitingFor::PermissionPrompt,
        since: at(420),
    };
    let s1 = view(
        session(
            1,
            "Plan the migration",
            Activity::Blocked {
                on: WaitingFor::PermissionPrompt,
            },
        ),
        Some(wait),
    );

    // 2: a finished turn that carries a script tag, a Jira key, a document, a secret and a PR.
    let text = format!(
        "Done with the parser. <script>alert(1)</script>\n\nSee ABC-123 and https://example.com/design for the plan. Token {SECRET} leaked."
    );
    let end = turn_end(300, TurnTrigger::TaskNotification, &text);
    let mut s2 = view(
        session(
            2,
            "Parser rework",
            Activity::Idle {
                ended_at: at(300),
                listing: ListingStatus::Seen,
            },
        ),
        Some(PendingCard::Turn {
            session: sid(2),
            turn: end.clone(),
            since: at(300),
        }),
    );
    s2.links = vec![
        link_view(jira(123), None),
        link_view(pr(42), Some(good_reading(None))),
        link_view(
            pr(43),
            Some(good_reading(Some(Failed {
                error: GhError::Tool(ToolError::TimedOut {
                    program: "gh".to_owned(),
                    after: Duration::from_secs(10),
                }),
                at: at(20),
            }))),
        ),
        link_view(
            pr(44),
            Some(Reading::Failure(Failed {
                error: GhError::AuthRequired,
                at: at(20),
            })),
        ),
        link_view(pr(45), Some(Reading::Unread)),
        link_view(
            pr(46),
            Some(Reading::Value {
                observed: Observed {
                    answer: Answer::NotFound,
                    read_at: at(180),
                },
                age: Age::from_secs(180),
                failure: None,
            }),
        ),
    ];
    s2.last_turn_ends = vec![end, turn_end(900, TurnTrigger::Human, "Earlier turn text.")];

    // 3: a question.
    let s3 = view(
        session(
            3,
            "Schema question",
            Activity::Blocked {
                on: WaitingFor::InputNeeded,
            },
        ),
        Some(question_card(3, 120)),
    );

    // 4: watching, a seen card.
    let end4 = turn_end(60, TurnTrigger::Human, "Waiting on the build.");
    let s4 = view(
        session(
            4,
            "Build watcher",
            Activity::Watching {
                ended_at: at(60),
                listing: ListingStatus::Seen,
            },
        ),
        Some(PendingCard::Turn {
            session: sid(4),
            turn: end4,
            since: at(60),
        }),
    );

    // 5: running; 6: unknown.
    let s5 = view(
        session(5, "Runner", Activity::Running { since: at(10) }),
        None,
    );
    let s6 = view(
        session(
            6,
            "Mystery",
            Activity::Unknown {
                reason: Redacted::of("the listing is unavailable: timed out"),
            },
        ),
        None,
    );
    // 7: a second terminal with the same title as another window.
    let s7 = view(
        session(7, "Shared title", Activity::Running { since: at(5) }),
        None,
    );

    model.inbox = vec![
        entry(
            &s1,
            Seen::Unseen,
            Some(&format!("Earlier line with {SECRET} inside")),
        ),
        entry(&s3, Seen::Unseen, None),
        entry(&s2, Seen::Unseen, None),
        entry(&s4, Seen::Seen, None),
    ];
    model.sessions = vec![s1, s3, s2, s4, s5, s6, s7];

    let mut d1 = view(
        session(
            8,
            "Old <b>work</b>",
            Activity::Dormant {
                last_seen: at(86_400 * 2),
                background: None,
            },
        ),
        None,
    );
    d1.last_turn_ends = vec![turn_end(
        86_400 * 2,
        TurnTrigger::Human,
        "First line of the old turn.\nSecond line.",
    )];
    let d2 = view(
        session(
            9,
            "Another one",
            Activity::Dormant {
                last_seen: at(86_400 * 5),
                background: Some(crate::types::BgState::Done),
            },
        ),
        None,
    );
    model.dormant = vec![d1, d2];

    model
}

struct Site {
    router: Router,
    runner: Arc<FakeRunner>,
    /// What the poller's tasks ran: the listing, and the `acli` reads of the work items.
    poller_runner: Arc<FakeRunner>,
    scratch: Scratch,
}

/// The site over `model`, with a real poller and stores in a scratch directory, and `script`
/// as the answers the compositor gives.
async fn site(
    label: &str,
    model: Model,
    script: Vec<Result<Output, crate::util::runner::RunError>>,
) -> Site {
    site_with(label, model, script, JIRA_SECTION).await
}

/// [`site`], with `jira` as the config's Jira section.
async fn site_with(
    label: &str,
    model: Model,
    script: Vec<Result<Output, crate::util::runner::RunError>>,
    jira: &str,
) -> Site {
    let scratch = Scratch::new(&format!("web-{label}"));
    std::fs::create_dir_all(scratch.join("claude/projects")).unwrap();
    std::fs::create_dir_all(scratch.join("state")).unwrap();
    std::fs::create_dir_all(scratch.join("files")).unwrap();
    let config = config_with(&scratch.to_string_lossy(), jira);
    let poller_runner = Arc::new(FakeRunner::default());
    let handle: Handle = state::start_with(config.clone(), poller_runner.clone()).unwrap();
    assert!(
        stores_read(handle.model()).await,
        "the poller did not read the stores in ten seconds"
    );
    let runner = Arc::new(FakeRunner::new(script));
    let dyn_runner: Arc<dyn crate::util::runner::CommandRunner> = runner.clone();
    let now = at(0);
    let app = App::new(Parts {
        handle,
        model: Arc::new(tokio::sync::RwLock::new(model)),
        runner: dyn_runner,
        tokens: Tokens::from_texts(READ_TOKEN, WRITE_TOKEN),
        port: PORT,
        extractor: Extractor::from_config(&config).unwrap(),
        targets: config.link_targets(),
        default_owner: config.github.default_owner.clone(),
        zone: TimeZone::UTC,
        clock: Arc::new(move || now),
    });
    Site {
        router: router(app),
        runner,
        poller_runner,
        scratch,
    }
}

/// Waits until the poller has read the seen store, which an edit needs.
async fn stores_read(model: &Arc<tokio::sync::RwLock<Model>>) -> bool {
    for _ in 0..400 {
        {
            let model = model.read().await;
            let health = &model.health;
            if matches!(health.seen, SourceState::Ok { .. }) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

const HOST: (&str, &str) = ("host", "127.0.0.1:7357");
const COOKIE: (&str, &str) = ("cookie", "conn=tok-0123456789abcdef");
const ORIGIN: (&str, &str) = ("origin", "http://127.0.0.1:7357");
const HEADER: (&str, &str) = ("x-conn-token", WRITE_TOKEN);
const READ_TOKEN_HEADER: (&str, &str) = ("x-conn-token", READ_TOKEN);
const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

async fn send(
    site: &Site,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = site
        .router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn page(site: &Site, path: &str) -> Reply {
    send(site, Method::GET, path, &[HOST, COOKIE], "").await
}

async fn post(site: &Site, path: &str, body: &str) -> Reply {
    send(
        site,
        Method::POST,
        path,
        &[HOST, COOKIE, ORIGIN, HEADER, FORM],
        body,
    )
    .await
}

async fn fixture_site(label: &str) -> Site {
    site(label, fixture(), Vec::new()).await
}

#[track_caller]
fn assert_has(reply: &Reply, fragment: &str) {
    assert!(
        reply.body.contains(fragment),
        "missing {fragment:?} in:\n{}",
        reply.body
    );
}

#[track_caller]
fn assert_lacks(reply: &Reply, fragment: &str) {
    assert!(
        !reply.body.contains(fragment),
        "unexpected {fragment:?} in:\n{}",
        reply.body
    );
}

// --- admission ---

/// The matrix, through the real router. Mutation: skip the host check (the wrong-host row
/// answers 200), skip the cookie check (the no-cookie row answers 200), skip the origin
/// check or the header check on POST (their rows answer 200), compare the header with the
/// cookie's token instead of the write token (the read-token row answers 200), or move the
/// checks after routing (the unknown-path row without a cookie answers 404).
#[tokio::test]
async fn the_admission_matrix() {
    let site = fixture_site("matrix").await;
    let get = |headers: &'static [(&'static str, &'static str)]| {
        let site = &site;
        async move { send(site, Method::GET, "/", headers, "").await }
    };

    let no_cookie = get(&[HOST]).await;
    assert_eq!(no_cookie.status, StatusCode::UNAUTHORIZED);
    assert!(
        no_cookie
            .body
            .contains("open the URL that `conn serve` printed")
    );
    assert_eq!(
        send(&site, Method::GET, "/nope", &[HOST], "").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&[("host", "evil.example:7357"), COOKIE]).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&[("host", "127.0.0.1:9999"), COOKIE]).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&[("cookie", "conn=wrong"), HOST]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(get(&[HOST, COOKIE]).await.status, StatusCode::OK);
    assert_eq!(
        get(&[("host", "localhost:7357"), COOKIE]).await.status,
        StatusCode::OK
    );

    // A POST: the order of refusals is host, cookie, origin, header.
    let focus = "/focus";
    let body = "session=00000000-0000-4000-8000-000000000008";
    let posted = |headers: &'static [(&'static str, &'static str)]| {
        let site = &site;
        async move { send(site, Method::POST, focus, headers, body).await.status }
    };
    assert_eq!(
        posted(&[("host", "evil.example"), COOKIE, ORIGIN, HEADER, FORM]).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        posted(&[HOST, ORIGIN, HEADER, FORM]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        posted(&[HOST, COOKIE, HEADER, FORM]).await,
        StatusCode::FORBIDDEN,
        "no Origin"
    );
    assert_eq!(
        posted(&[
            HOST,
            COOKIE,
            ("origin", "http://evil.example"),
            HEADER,
            FORM
        ])
        .await,
        StatusCode::FORBIDDEN,
        "foreign Origin"
    );
    assert_eq!(
        posted(&[HOST, COOKIE, ORIGIN, FORM]).await,
        StatusCode::FORBIDDEN,
        "no header token"
    );
    assert_eq!(
        posted(&[HOST, COOKIE, ORIGIN, ("x-conn-token", "wrong"), FORM]).await,
        StatusCode::FORBIDDEN,
        "wrong header token"
    );
    assert_eq!(
        posted(&[HOST, COOKIE, ORIGIN, READ_TOKEN_HEADER, FORM]).await,
        StatusCode::FORBIDDEN,
        "the cookie's token in the header is not the write token"
    );
    assert_eq!(
        posted(&[HOST, ORIGIN, HEADER, FORM]).await,
        StatusCode::UNAUTHORIZED,
        "the write token alone, without the cookie"
    );
    assert_eq!(
        posted(&[HOST, COOKIE, ORIGIN, HEADER, FORM]).await,
        StatusCode::OK,
        "a dormant session has no terminal, and the answer says so"
    );
}

/// Mutation: drop one cookie attribute, answer 200 instead of redirecting, accept a wrong
/// token, or put the write token in the cookie.
#[tokio::test]
async fn the_url_token_sets_the_cookie_and_redirects() {
    let site = fixture_site("signin").await;
    let reply = send(
        &site,
        Method::GET,
        &format!("/?t={READ_TOKEN}"),
        &[HOST],
        "",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.headers.get("set-cookie").unwrap(),
        "conn=tok-0123456789abcdef; HttpOnly; SameSite=Strict; Path=/"
    );
    assert_eq!(reply.headers.get("location").unwrap(), "/");
    assert_eq!(
        reply.headers.get_all("set-cookie").iter().count(),
        1,
        "one cookie, and it is not the write token"
    );

    let wrong = send(&site, Method::GET, "/?t=nope", &[HOST], "").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.headers.get("set-cookie").is_none());
    // The token in a URL signs in only at the root.
    let elsewhere = send(
        &site,
        Method::GET,
        &format!("/health?t={READ_TOKEN}"),
        &[HOST],
        "",
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::UNAUTHORIZED);
}

/// Mutation: drop one hardening header, or serve an asset without the cookie.
#[tokio::test]
async fn responses_are_not_cached_and_assets_need_the_cookie() {
    let site = fixture_site("headers").await;
    let reply = page(&site, "/").await;
    assert_eq!(reply.headers.get("cache-control").unwrap(), "no-store");
    assert!(
        reply
            .headers
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("default-src 'none'")
    );
    assert_eq!(
        reply.headers.get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        send(&site, Method::GET, "/static/app.js", &[HOST], "")
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let script = page(&site, "/static/app.js").await;
    assert_eq!(script.status, StatusCode::OK);
    assert_has(&script, "document.body.dataset.writeToken");
    assert_has(&script, "'X-Conn-Token': writeToken");
    assert_eq!(
        script.headers.get("content-type").unwrap(),
        "text/javascript; charset=utf-8"
    );
    let style = page(&site, "/static/style.css").await;
    assert_has(&style, "prefers-color-scheme: dark");
}

#[tokio::test]
async fn unknown_paths_and_methods_and_parameters_are_refused_after_admission() {
    let site = fixture_site("routes").await;
    assert_eq!(page(&site, "/nope").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        page(&site, "/seen").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        page(&site, "/sessions/zzz").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        page(&site, "/sessions/00000000-0000-4000-8000-0000000000ff")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&site, Method::PUT, "/", &[HOST, COOKIE, ORIGIN, HEADER], "")
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

/// A local file under the configured root is its own page: rendered, escaped, redacted, with
/// the pop-up but without the reload the other pages carry. One outside the root, absent, or
/// named by a path that is not a plain `.md` path is refused with its own status, and none is
/// served without the cookie.
/// Mutation: answer `Route::File` with the view before `file::read` (the outside file is
/// served), map `OutsideRoots` to 404, or put `data-reload` on the file page.
#[tokio::test]
async fn a_local_file_is_a_page_of_its_own_and_only_under_a_root() {
    let site = fixture_site("file").await;
    let root = site.scratch.join("files");
    let brief = root.join("brief.md");
    std::fs::write(
        &brief,
        "# Brief\n\n<script>x()</script>\n\nsee ABC-7 and token ghp_0123456789abcdefghijABCDEFGHIJ012345\n",
    )
    .unwrap();
    std::fs::write(site.scratch.join("outside.md"), "nope").unwrap();
    let href = |path: &Path| {
        format!(
            "/file?path={}",
            percent_encoding::utf8_percent_encode(
                &path.to_string_lossy(),
                percent_encoding::NON_ALPHANUMERIC
            )
        )
    };

    let shown = page(&site, &href(&brief)).await;
    assert_eq!(shown.status, StatusCode::OK);
    assert_has(&shown, "<h3>Brief</h3>");
    assert_has(&shown, "&lt;script&gt;x()&lt;/script&gt;");
    assert_has(
        &shown,
        r#"href="https://example.atlassian.net/browse/ABC-7""#,
    );
    assert_lacks(&shown, "ghp_0123456789");
    assert_has(&shown, r#"id="file-dialog""#);
    assert_lacks(&shown, "data-reload");

    let outside = page(&site, &href(&site.scratch.join("outside.md"))).await;
    assert_eq!(outside.status, StatusCode::FORBIDDEN);
    assert_lacks(&outside, "nope");
    assert_eq!(
        page(&site, &href(&root.join("none.md"))).await.status,
        StatusCode::NOT_FOUND
    );
    for bad in [
        "/file",
        "/file?path=relative.md",
        "/file?path=%2Fa%2F..%2Fb.md",
        "/file?path=%2Fa.txt",
    ] {
        assert_eq!(
            page(&site, bad).await.status,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    let unsigned = send(&site, Method::GET, &href(&brief), &[HOST], "").await;
    assert_ne!(unsigned.status, StatusCode::OK);
    assert_lacks(&unsigned, "Brief");
}

// --- pages ---

/// The inbox: cards in the model's order, the blocked wait, the question with its numbered
/// options, the turn with its links, escaped markup, redaction, PR readings and the header.
/// Mutation: skip `redact` on any text (the secret row goes red), build text with a raw string
/// (the `<script>` row goes red), drop the `Blocked` arm of `card_body`, or drop a `Reading`
/// arm.
#[tokio::test]
async fn the_inbox_renders_every_kind_of_card_and_escapes_and_redacts() {
    let site = fixture_site("inbox").await;
    let reply = page(&site, "/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers.get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    // The header: age, nav with the unseen count, no stale badge, no scanning badge.
    // A green dot: 4 s is within the 15 s threshold. It carries the model's own time, so the
    // script can count the age on while the page is not reloading: 1 800 000 000 s, less the
    // fixture's 4 s, in milliseconds.
    assert_has(
        &reply,
        r#"<span id="freshness" class="dot fresh" data-generated-at="1799999996000" data-fresh-secs="15" role="img" title="as of 4 s ago" aria-label="as of 4 s ago"></span>"#,
    );
    assert_has(&reply, r#"<a href="/" class="active">Inbox (3)</a>"#);
    assert_lacks(&reply, "dot stale");
    assert_lacks(&reply, "still scanning");
    assert_lacks(&reply, "Not everything was read");
    assert_has(
        &reply,
        r#"<body data-write-token="wtk-fedcba9876543210" data-reload>"#,
    );
    assert_lacks(&reply, READ_TOKEN);
    // Cards in order: blocked, question, finished turn, then the seen one.
    let order = |needle: &str| reply.body.find(needle).unwrap_or(usize::MAX);
    assert!(order("Plan the migration") < order("Schema question"));
    assert!(order("Schema question") < order("Parser rework"));
    assert!(order("Parser rework") < order("Build watcher"));
    assert_has(&reply, r#"class="card seen""#);
    // The blocked card.
    assert_has(
        &reply,
        r#"<span class="badge blocked">Blocked · 7 min</span>"#,
    );
    assert_has(
        &reply,
        "Waiting for a permission prompt: open the terminal.",
    );
    // The question: header, numbered options, multi-select.
    assert_has(
        &reply,
        r#"<strong>Scope</strong> <div class="lead"><p>Which parts should change?</p></div>"#,
    );
    assert_has(&reply, r#"<span class="tag">multi-select</span>"#);
    assert_has(
        &reply,
        "<ol><li><strong>The parser</strong> <div class=\"lead muted\"><p>only the parser [redacted]</p></div></li><li><strong>Everything</strong></li></ol>",
    );
    // The turn: escaped script, links made clickable, trigger phrase.
    assert_has(
        &reply,
        "Done with the parser. &lt;script&gt;alert(1)&lt;/script&gt;",
    );
    assert_lacks(&reply, "<script>alert");
    assert_has(
        &reply,
        r#"<a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a>"#,
    );
    assert_has(
        &reply,
        r#"<a href="https://example.com/design" target="_blank" rel="noopener noreferrer">https://example.com/design</a>"#,
    );
    assert_has(&reply, "Turn ended after a task notification.");
    // Redaction everywhere text from outside appears.
    assert_lacks(&reply, SECRET);
    assert_has(&reply, "Token [redacted] leaked.");
    assert_has(&reply, "Earlier line with [redacted] inside");
    // The earlier line is collapsed context.
    assert_has(
        &reply,
        "<details class=\"earlier\"><summary>earlier</summary>",
    );
    // Link chips and PR readings: fresh, stale, failed and unread are all marked, in the
    // chip's `title`; `a_pull_request_chip_is_compact_and_keeps_the_words_in_its_title` pins
    // the chip's own markup.
    assert_has(
        &reply,
        r#"title="open · approved · checks passing · read 3 min ago""#,
    );
    assert_has(
        &reply,
        "stale: refresh failed 20 s ago: gh did not answer within 10s",
    );
    assert_has(
        &reply,
        r#"title="unavailable: gh has no usable credentials; run `gh auth login` (20 s ago)""#,
    );
    assert_has(&reply, r#"title="not read yet""#);
    // Actions.
    assert_has(
        &reply,
        r#"<form class="act" method="post" action="/focus" data-kind="focus">"#,
    );
    assert_has(&reply, r#"action="/seen""#);
    assert_has(
        &reply,
        r#"<input type="hidden" name="since" value="2027-01-15T07:55:00Z">"#,
    );
    assert_has(&reply, r#"action="/pin""#);
    assert_has(&reply, r#"<div class="focus-result" hidden>"#);
    // The resume command sits in a hidden block next to the focus outcome.
    assert_has(
        &reply,
        "cd /home/dev/proj-1 &amp;&amp; claude --resume 00000000-0000-4000-8000-000000000001",
    );
    assert_has(&reply, "Redaction is best effort");
}

/// A long body collapses behind "more" and the whole text stays in the page.
/// Mutation: change the threshold comparison to `>=` 1201 (the 1200 row collapses), or cut the
/// text instead of hiding it.
#[tokio::test]
async fn a_body_over_twelve_hundred_characters_collapses_and_keeps_its_text() {
    let long = "x".repeat(1201);
    let exact = "y".repeat(1200);
    let mut model = fixture();
    for (id, text) in [(sid(2), &long), (sid(4), &exact)] {
        let card = |card: &mut PendingCard| {
            if let PendingCard::Turn { turn, .. } = card {
                turn.text = AssistantText::of(text);
            }
        };
        if let Some(view) = model.sessions.iter_mut().find(|v| v.session.id == id) {
            view.pending.iter_mut().for_each(card);
        }
        if let Some(entry) = model.inbox.iter_mut().find(|e| e.session == id) {
            card(&mut entry.card);
        }
    }
    let site = site("collapse", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(&reply, &format!("<div class=\"clip\"><p>{long}</p></div>"));
    assert_has(&reply, "<span class=\"more-open\">more</span>");
    assert_has(&reply, &format!("<p>{exact}</p>"));
    assert_lacks(&reply, &format!("<div class=\"clip\"><p>{exact}</p>"));
}

/// A card opens with what its session is about and whom it waits on: the gist escaped, an
/// earlier gist kept beside the failure that left it, a failure alone said, and the wait
/// shown before any gist is written.
/// Mutation: drop the `ask` badge, show nothing for a failure, drop the earlier gist when its
/// card's run failed, or write the gist without escaping it (the `<b>` row goes red).
#[tokio::test]
async fn a_card_shows_its_gist_and_whom_it_waits_on() {
    let mut model = fixture();
    let gist = |text: &str| Gist {
        text: Redacted::of(text),
        ask: Ask::Others,
    };
    let failure = || GistError::Empty;
    for entry in &mut model.inbox {
        let (view, ask) = match entry.session.as_str().chars().last() {
            Some('1') => (
                GistView::Current(gist("Rework the <b>parser</b>")),
                Some(Ask::Others),
            ),
            Some('2') => (
                GistView::Earlier {
                    gist: gist("Ship the importer"),
                    failure: Some(failure()),
                },
                None,
            ),
            Some('3') => (GistView::Failed(failure()), Some(Ask::Decide)),
            _ => (GistView::Pending, None),
        };
        entry.gist = view;
        entry.ask = ask;
    }
    let site = site("gist", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(
        &reply,
        r#"<p class="gist"><span class="badge ask-others">Waiting on others</span><span class="text">Rework the &lt;b&gt;parser&lt;/b&gt;</span></p>"#,
    );
    assert_has(
        &reply,
        r#"<p class="gist"><span class="text">Ship the importer</span><span class="note">not updated: the model wrote an empty gist</span></p>"#,
    );
    assert_has(
        &reply,
        r#"<p class="gist"><span class="badge ask-decide">Your decision</span><span class="note">no gist: the model wrote an empty gist</span></p>"#,
    );
    assert_eq!(reply.body.matches(r#"<p class="gist">"#).count(), 3);
}

/// A card placed by its session's ledger wait says that wait, whatever the gist read: a
/// release nobody runs as the user's release, one under way and any other wait as someone
/// else's; a card with no wait keeps the gist's ask.
/// Mutation: let the gist's "Your move" win over a wait, show a running release as the user's,
/// or leave the wait's words unredacted.
#[tokio::test]
async fn a_card_says_the_wait_its_session_declared() {
    use crate::ledger::Slug;
    use crate::model::{ReleaseRun, Standing, Waiting};
    let mut model = fixture();
    let release = |run| {
        Standing::Waiting(Waiting::Release {
            rollout: Slug::parse("checkout").unwrap(),
            run,
        })
    };
    for entry in &mut model.inbox {
        entry.ask = Some(Ask::Act);
        entry.standing = match entry.session.as_str().chars().last() {
            Some('1') => release(ReleaseRun::NotStarted),
            Some('2') => release(ReleaseRun::Running),
            Some('3') => Standing::Waiting(Waiting::Other {
                on: "token ghp_0123456789abcdefghijklmnopqrstuvwxyz to land".to_owned(),
            }),
            _ => Standing::Normal,
        };
    }
    let site = site("card-wait", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(
        &reply,
        r#"<span class="badge ask-release" title="the session waits for you to run this release">Waits on release: checkout</span>"#,
    );
    assert_has(
        &reply,
        r#"<span class="badge ask-others">Release running: checkout</span>"#,
    );
    assert_has(
        &reply,
        r#"<span class="badge ask-others">Waiting on: token [redacted] to land</span>"#,
    );
    assert_lacks(&reply, "ghp_0123456789");
    assert_lacks(&reply, "waiting on its own");
    let moves = reply.body.matches("Your move").count();
    assert_eq!(
        moves,
        model_cards(&reply) - 3,
        "only the cards without a wait"
    );
}

/// How many cards a page shows.
fn model_cards(reply: &Reply) -> usize {
    reply.body.matches(r#"<article class="card"#).count()
}

/// A session row shows the session's latest gist, escaped, and the filter finds the row by it.
/// Mutation: drop the row's gist, or leave it out of `data-filter`.
#[tokio::test]
async fn a_session_row_shows_its_gist_and_is_found_by_it() {
    let mut model = fixture();
    model.sessions[0].gist = Some(Redacted::of("Retry the <i>Slack</i> reader"));
    let site = site("row-gist", model, Vec::new()).await;
    let reply = page(&site, "/sessions").await;
    assert_has(
        &reply,
        r#"<p class="gist"><span class="text">Retry the &lt;i&gt;Slack&lt;/i&gt; reader</span></p>"#,
    );
    assert_has(&reply, "retry the &lt;i&gt;slack&lt;/i&gt; reader\">");
    assert_eq!(reply.body.matches(r#"<p class="gist">"#).count(), 1);
}

/// The map draws what the ledger ties together, with no script: session 2 owns ABC-123 and runs
/// the checkout rollout that ships it, session 1 waits on checkout. Each node links to its detail card,
/// which carries the owner, the rollout and the session's line; the active sessions tied to
/// nothing are listed apart.
/// Mutation: drop an edge kind from the drawing, link a node to the wrong card, or leave the
/// sessions on their own out.
#[tokio::test]
async fn the_map_draws_the_dependencies_and_a_card_for_each_node() {
    let mut model = fixture();
    model.coordination = coordination(false);
    // Jira says ABC-7 blocks ABC-123.
    model.coordination.dependencies = vec![crate::jira::links::Dependency {
        blocker: crate::types::JiraKey::parse("ABC-7").unwrap(),
        blocked: crate::types::JiraKey::parse("ABC-123").unwrap(),
    }];
    let site = site("map", model, Vec::new()).await;
    let reply = page(&site, "/map").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_lacks(&reply, "<script src=\"/static/map.js\"");
    assert_has(&reply, ">blocked by ABC-7</text>");
    assert_has(
        &reply,
        r##"Blocked by <a href="#n-t-ABC-7">ABC-7</a> (Jira)"##,
    );
    assert_has(
        &reply,
        r##"Blocks <a href="#n-t-ABC-123">ABC-123</a> (Jira)"##,
    );
    for class in ["e-owns", "e-runs", "e-waits", "e-ships", "e-blocks"] {
        assert_has(&reply, &format!(r#"<path class="{class}""#));
    }
    let two = "00000000-0000-4000-8000-000000000002";
    assert_has(&reply, &format!(r##"<a href="#n-s-{two}">"##));
    assert_has(&reply, r##"<a href="#n-t-ABC-123">"##);
    assert_has(&reply, r##"<a href="#n-r-checkout">"##);
    assert_has(
        &reply,
        &format!(r#"<section class="detail" id="n-s-{two}">"#),
    );
    assert_has(&reply, r#"<section class="detail" id="n-t-ABC-123">"#);
    assert_has(
        &reply,
        r#"<span class="owned-by">owned by <span class="who running">"#,
    );
    assert_has(&reply, r#"<section class="detail" id="n-r-checkout">"#);
    assert_has(
        &reply,
        r#"<span class="outcome running">running</span> in "#,
    );
    assert_has(
        &reply,
        r#"<p class="coord">waiting on <code>rollout:checkout</code>"#,
    );
    assert_has(&reply, "<h2>On their own</h2>");
    let script = page(&site, "/static/map.js").await;
    assert_eq!(script.status, StatusCode::NOT_FOUND);
}

/// Calm is shown only when every source was read, and the badges say so when not.
/// Mutation: show "Nothing is waiting on you." whatever the health, drop the stale badge, or
/// drop the scanning badge.
#[tokio::test]
async fn an_empty_inbox_is_calm_only_if_everything_was_read() {
    let mut model = fixture();
    model.inbox.clear();
    let calm = site("calm", model.clone(), Vec::new()).await;
    let reply = page(&calm, "/").await;
    assert_has(&reply, "Nothing is waiting on you.");

    model.health.listing = SourceState::Failed {
        at: at(8),
        reason: "timed out".to_owned(),
    };
    model.health.transcripts.progress = ScanProgress::InProgress {
        scanned: 3,
        total: 12,
    };
    model.generated_at = at(16);
    let unsure = site("unsure", model, Vec::new()).await;
    let reply = page(&unsure, "/").await;
    assert_lacks(&reply, "Nothing is waiting on you.");
    assert_has(
        &reply,
        "Nothing is listed, but that is not the same as nothing waiting: listing (failed), transcript scan (3 of 12 read). See the health page.",
    );
    assert_has(&reply, r#"<span id="freshness" class="dot stale""#);
    assert_has(
        &reply,
        r#"<span class="badge scan">still scanning 3 of 12</span>"#,
    );
    assert_has(
        &reply,
        r#"<div class="healthline">Not everything was read: listing (failed), transcript scan (3 of 12 read). <a href="/health">Details on the health page.</a></div>"#,
    );
    assert_has(&reply, r#"title="as of 16 s ago""#);
}

/// Mutation: group by anything but the activity, show a dormant session here, or drop the
/// unknown reason.
#[tokio::test]
async fn the_sessions_page_groups_by_activity_in_urgency_order() {
    let site = fixture_site("sessions").await;
    let reply = page(&site, "/sessions").await;
    assert_eq!(reply.status, StatusCode::OK);
    let order = |needle: &str| reply.body.find(needle).unwrap_or(usize::MAX);
    assert!(order("<h2>Blocked (2)</h2>") < order("<h2>Watching (1)</h2>"));
    assert!(order("<h2>Watching (1)</h2>") < order("<h2>Idle (1)</h2>"));
    assert!(order("<h2>Idle (1)</h2>") < order("<h2>Running (2)</h2>"));
    assert!(order("<h2>Running (2)</h2>") < order("<h2>Unknown (1)</h2>"));
    assert_has(&reply, "the listing is unavailable: timed out");
    assert_has(&reply, "waiting for a permission prompt");
    assert_has(&reply, r#"<div class="cwd">/home/dev/proj-2</div>"#);
    assert_has(
        &reply,
        r#"<div class="first">Done with the parser. &lt;script&gt;alert(1)&lt;/script&gt;</div>"#,
    );
    assert_lacks(&reply, "Old &lt;b&gt;work");
}

/// The row of session `n` on a list page: from its link to the start of the next row.
fn row_of(body: &str, n: u8) -> &str {
    let link = format!(r#"<a href="/sessions/00000000-0000-4000-8000-0000000000{n:02}">"#);
    let start = body.find(&link).unwrap();
    let rest = body.get(start..).unwrap();
    rest.get(..rest.find(r#"<div class="row""#).unwrap_or(rest.len()))
        .unwrap()
}

/// A session running in a terminal gets "Open terminal", with its resume command kept for a
/// failed focus but hidden; any other session shows the resume command itself.
/// Mutation: show the resume command on every row (session 2's command is visible), or the
/// button on every row (session 6 gets one).
#[tokio::test]
async fn a_running_session_jumps_to_its_terminal_and_only_the_rest_resume() {
    let site = fixture_site("session-rows").await;
    let reply = page(&site, "/sessions").await;
    let idle = row_of(&reply.body, 2);
    assert!(
        idle.contains(r#"<form class="act" method="post" action="/focus" data-kind="focus"><input type="hidden" name="session" value="00000000-0000-4000-8000-000000000002"><button type="submit" class="primary">Open terminal</button></form>"#),
        "{idle}"
    );
    assert!(
        idle.contains(r#"<div class="focus-result" hidden><p class="msg"></p><div class="resume"><div class="cmd">"#),
        "{idle}"
    );
    let unknown = row_of(&reply.body, 6);
    assert!(!unknown.contains(r#"action="/focus""#), "{unknown}");
    assert!(unknown.contains(r#"<div class="cmd"><pre>"#), "{unknown}");
    assert!(!unknown.contains("focus-result"), "{unknown}");
}

/// The box the page script filters session rows by.
const FILTER_BOX: &str =
    r#"<input id="filter" type="search" placeholder="filter by title, directory or gist""#;

/// The live sessions page has the filter box over its rows, and a URL that carries the filter
/// (the script keeps it there across reloads) serves the same page; with no session there is
/// nothing to filter.
/// Mutation: leave the box off the live page (red); show it with no session (red).
#[tokio::test]
async fn the_sessions_page_has_the_filter_and_keeps_it_in_the_url() {
    let live = fixture_site("sessions-filter").await;
    let reply = page(&live, "/sessions").await;
    assert_has(&reply, FILTER_BOX);
    assert_has(&reply, r#"data-filter=""#);
    let filtered = page(&live, "/sessions?filter=parser").await;
    assert_eq!(filtered.status, StatusCode::OK);
    assert_has(&filtered, FILTER_BOX);

    let mut model = fixture();
    model.sessions.clear();
    let empty = site("sessions-none", model, Vec::new()).await;
    assert_lacks(&page(&empty, "/sessions").await, r#"id="filter""#);
}

/// Mutation: list oldest first, drop the filter attribute, or render a title unescaped.
#[tokio::test]
async fn the_dormant_page_lists_newest_first_with_a_filter() {
    let site = fixture_site("dormant").await;
    let reply = page(&site, "/dormant").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.find("Old &lt;b&gt;work&lt;/b&gt;") < reply.body.find("Another one"));
    assert_has(&reply, FILTER_BOX);
    assert_has(
        &reply,
        r#"data-filter="old &lt;b&gt;work&lt;/b&gt; /home/dev/proj-8""#,
    );
    assert_has(&reply, "First line of the old turn.");
    assert_lacks(&reply, "Second line.");
    assert_has(
        &reply,
        "cd /home/dev/proj-8 &amp;&amp; claude --resume 00000000-0000-4000-8000-000000000008",
    );
    assert_has(
        &reply,
        r#"<button class="copy" type="button">Copy</button>"#,
    );
    assert_has(
        &reply,
        r#"<span class="badge dormant">Dormant · 2 d</span>"#,
    );
    // Session 9 ran in the background: it is dormant like the rest, and says how it ended.
    assert_has(
        &reply,
        r#"<span class="badge dormant">Dormant · 5 d</span> <span class="muted">background session is done</span>"#,
    );
    assert_eq!(reply.body.matches("background session").count(), 1);
}

/// The session page, an inbox card and a search hit decide alike: "Open terminal" with the
/// resume command hidden for a session running in a terminal, the resume command alone for any
/// other.
/// Mutation: keep the session page on `resume_block` alone (session 2 has no button), or give
/// the inbox card the button whatever the session (session 6's card gets one).
#[tokio::test]
async fn every_page_takes_a_running_session_to_its_terminal_and_the_rest_to_resume() {
    const BUTTON: &str = r#"<button type="submit" class="primary">Open terminal</button>"#;
    const HIDDEN_RESUME: &str = r#"<div class="focus-result" hidden><p class="msg"></p><div class="resume"><div class="cmd">"#;
    const SHOWN_RESUME: &str = r#"<div class="cmd"><pre>cd /home/dev/proj-6"#;
    let mut model = fixture();
    // Session 6's process cannot be told: its card offers the resume command, not the button.
    let mut card6 = model
        .inbox
        .iter()
        .find(|e| e.session == sid(2))
        .unwrap()
        .clone();
    card6.session = sid(6);
    model.inbox.push(card6);
    let site = site("reach", model, Vec::new()).await;

    let running = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_has(&running, BUTTON);
    assert_has(&running, HIDDEN_RESUME);
    let unknown = page(&site, "/sessions/00000000-0000-4000-8000-000000000006").await;
    assert_lacks(&unknown, BUTTON);
    assert_has(&unknown, SHOWN_RESUME);

    let inbox = page(&site, "/").await;
    let card6 = inbox
        .body
        .get(
            inbox
                .body
                .find(r#"id="00000000-0000-4000-8000-000000000006""#)
                .unwrap()..,
        )
        .unwrap();
    assert!(!card6.contains(BUTTON), "{card6}");
    assert!(card6.contains(SHOWN_RESUME), "{card6}");

    let search = page(&site, "/search?q=Mystery").await;
    assert_lacks(&search, BUTTON);
    assert_has(&search, SHOWN_RESUME);
}

/// Mutation: render the turn ends oldest first, drop the cap, or drop a link chip.
#[tokio::test]
async fn a_session_page_shows_its_turn_ends_newest_first_and_its_resume_command() {
    let site = fixture_site("session").await;
    let reply = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.find("Done with the parser.") < reply.body.find("Earlier turn text."));
    assert_has(&reply, "Ended 5 min ago after a task notification (");
    assert_has(&reply, "Ended 15 min ago after your prompt (");
    assert_has(&reply, r#"<span class="badge idle">Idle · 5 min</span>"#);
    assert_has(
        &reply,
        "cd /home/dev/proj-2 &amp;&amp; claude --resume 00000000-0000-4000-8000-000000000002",
    );
    assert_has(&reply, "browse/ABC-123");
    assert_lacks(&reply, SECRET);
}

/// The inbox card of session `n` in a page's `body`.
fn card_of(body: &str, n: u8) -> &str {
    let (_, card) = body.split_once(&format!(r#"id="{}""#, sid(n))).unwrap();
    card.split_once("</article>").unwrap().0
}

/// The instant the turn session `n` waits on ended, as `model` holds it.
fn waiting_turn_at(model: &Model, n: u8) -> Instant {
    match model
        .session(&sid(n))
        .and_then(|view| view.pending.as_ref())
    {
        Some(PendingCard::Turn { turn, .. }) => turn.at,
        other => panic!("session {n} waits on no turn: {other:?}"),
    }
}

/// The reply composer is offered on the text of a finished turn the session waits on and on
/// an open question's card, each keyed by the session and the instant it began, and on nothing
/// else: not on a harness prompt, not on a turn already answered.
/// Mutation: answer every turn end on the session page instead of the waiting one (red, two
/// composers); key the draft by the session alone (red, the key is missing); leave the question
/// card's body plain (red, no question composer).
#[tokio::test]
async fn a_waiting_turn_or_question_and_only_it_takes_a_reply() {
    let model = fixture();
    let key = format!("{}@{}", sid(2).as_str(), waiting_turn_at(&model, 2));
    let asked_at = model
        .session(&sid(3))
        .and_then(|v| v.pending.as_ref())
        .map(PendingCard::since)
        .unwrap();
    let site = site("reply", model, Vec::new()).await;

    let inbox = page(&site, "/").await;
    let card = |n: u8| card_of(&inbox.body, n);
    let turn = card(2);
    assert!(
        turn.contains(&format!(
            r#"<div class="replyable" data-reply-key="{key}" data-answers="turn">"#
        )),
        "{turn}"
    );
    assert!(
        turn.contains(r#"data-delivery="terminal">Copy &amp; open terminal</button>"#),
        "{turn}"
    );
    let question = card(3);
    assert!(
        question.contains(&format!(
            r#"<div class="replyable" data-reply-key="{}@{asked_at}" data-answers="question"><div class="question">"#,
            sid(3).as_str()
        )),
        "{question}"
    );
    assert!(
        !card(1).contains("replyable"),
        "a permission prompt is answered in its terminal"
    );

    let session = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_eq!(session.body.matches(r#"class="replyable""#).count(), 1);
    let composer = session.body.find(&key).unwrap();
    assert!(composer < session.body.find("Done with the parser.").unwrap());
    assert!(
        session.body.find("Earlier turn text.").unwrap()
            > session.body.find("Copy &amp; open terminal").unwrap()
    );
}

/// A session with no terminal to raise gets a reply to copy and paste after resuming.
/// Mutation: map every reach to the terminal delivery (red, the button raises a terminal).
#[tokio::test]
async fn a_reply_to_a_session_without_a_terminal_is_copied_only() {
    let mut model = fixture();
    if let Some(view) = model.sessions.iter_mut().find(|v| v.session.id == sid(2)) {
        view.session.kind = SessionKind::Headless;
    }
    let site = site("reply-copy", model, Vec::new()).await;
    let inbox = page(&site, "/").await;
    let card = card_of(&inbox.body, 2);
    assert!(
        card.contains(r#"data-delivery="copy">Copy reply</button>"#),
        "{card}"
    );
    assert!(!card.contains("Copy &amp; open terminal"), "{card}");
}

/// Mutation: drop a Health field's row, or show a failure as ok.
#[tokio::test]
async fn the_health_page_has_a_row_for_every_field_with_its_time() {
    let mut model = fixture();
    model.health.registry = SourceState::Failed {
        at: at(9),
        reason: "cannot read the sessions directory".to_owned(),
    };
    model.health.transcripts.unreadable = 2;
    model.health.notifications = crate::model::Notifications::Missing { at: at(3) };
    let site = site("health", model, Vec::new()).await;
    let reply = page(&site, "/health").await;
    assert_eq!(reply.status, StatusCode::OK);
    for label in [
        "listing (claude agents)",
        "registry and process table",
        "event log",
        "transcript discovery",
        "transcripts found",
        "transcript entries passed over",
        "transcript lines without a record",
        "transcripts unreadable",
        "transcript scan",
        "pull requests read",
        "pull requests not found",
        "pull requests stale",
        "pull requests failed",
        "pull requests not asked yet",
        "seen.json",
        "Jira work items (acli)",
        "plan usage (curl)",
        "coordination ledger",
        "coordination ledger lines that could not be read",
        "links that cannot be rendered",
        "listed sessions without an id yet",
        "desktop notification",
    ] {
        assert_has(&reply, &format!("<td>{label}</td>"));
    }
    assert_has(
        &reply,
        r#"<td class="bad">failed</td><td>2027-01-15 07:59:51 (9 s ago)</td><td>cannot read the sessions directory</td>"#,
    );
    assert_has(&reply, r#"<td class="bad">2</td>"#);
    assert_has(&reply, "notify-send is not installed");
    assert_has(&reply, "Model built 2027-01-15 07:59:56 (4 s ago)");
}

/// A ledger where session 2 owns ABC-123 and session 1 waits on the checkout rollout, which session
/// 2 runs; `finished` adds its finish.
fn coordination(finished: bool) -> crate::model::Coordination {
    use crate::ledger::{Act, Entry, Ledger, Note, Outcome, Slug, Subject, WaitOn};
    let entry = |session: u8, secs_ago: i64, act| Entry {
        at: at(secs_ago),
        session: sid(session),
        act,
    };
    let checkout = Slug::parse("checkout").unwrap();
    let mut entries = vec![
        entry(
            2,
            60,
            Act::Claim {
                subject: Subject::parse("ABC-123").unwrap(),
                took_from: None,
            },
        ),
        entry(
            1,
            50,
            Act::Wait {
                on: WaitOn::Rollout(checkout.clone()),
                then: Note::parse("move <ABC-123>").unwrap(),
                ticket: None,
            },
        ),
        entry(
            2,
            40,
            Act::RolloutStart {
                rollout: checkout.clone(),
                ships: vec![Subject::parse("ABC-123").unwrap()],
            },
        ),
    ];
    if finished {
        entries.push(entry(
            2,
            30,
            Act::RolloutFinish {
                rollout: checkout,
                outcome: Outcome::Shipped,
                note: None,
            },
        ));
    }
    crate::model::Coordination {
        state: SourceState::Ok { at: at(5) },
        ledger: Ledger::fold(entries),
        bad_lines: Vec::new(),
        shared: Vec::new(),
        dependencies: Vec::new(),
        links: None,
    }
}

/// A card says what its session owns and waits on; once the rollout it waited on finishes, the
/// card says the wait is over and what comes next; the owners page lists the wait, then the
/// rollout with the ticket's owner it reports to, then each owner with what it owns (its wait
/// marked), then the log. The session's own words are escaped.
/// Mutation: drop `session_line` from the card, word a finished wait as still waiting, drop
/// the shipped ticket's owner from the rollout, or list Owned before Waiting.
#[tokio::test]
async fn cards_and_the_owners_page_show_who_owns_what_and_whose_wait_is_over() {
    let mut model = fixture();
    model.coordination = coordination(false);
    let waiting = site("owners", model, Vec::new()).await;
    let inbox = page(&waiting, "/").await;
    assert_has(
        &inbox,
        r#"<p class="coord">owns <a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a></p>"#,
    );
    assert_has(
        &inbox,
        r#"<p class="coord">waiting on <code>rollout:checkout</code> (running in <span class="who running">"#,
    );
    assert_has(&inbox, ") → then: move &lt;ABC-123&gt;</p>");

    let owners = page(&waiting, "/owners").await;
    assert_eq!(owners.status, StatusCode::OK);
    assert_has(&owners, "<h1>Who owns what</h1>");
    assert_has(
        &owners,
        r#"<div class="owner-groups"><div class="owner-group running"><div class="owner-head"><span class="who running">"#,
    );
    assert_has(
        &owners,
        r#">Parser rework</a></span></div><ul class="owned-chips"><li title="claimed Jan 15 07:59">"#,
    );
    assert_has(
        &owners,
        r#"<a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a>"#,
    );
    assert_has(
        &owners,
        r#"<span class="outcome running">running</span> in "#,
    );
    assert_has(
        &owners,
        r#">ABC-123</a> owned by <span class="who running">"#,
    );
    assert_has(&owners, "started <code>rollout:checkout</code> shipping ");
    let heading = |text: &str| owners.body.find(&format!("<h2>{text}</h2>")).unwrap();
    assert!(heading("Waiting") < heading("Rollouts"));
    assert!(heading("Rollouts") < heading("Owned"));
    assert!(heading("Owned") < heading("Recent"));

    let detail = page(&waiting, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_has(&detail, r#"<h2>Coordination</h2><p class="coord">owns "#);
    assert_has(
        &detail,
        r#"</span> claimed <a href="https://example.atlassian.net/browse/ABC-123""#,
    );
    assert_has(&detail, r#"<a href="/owners">Who owns what</a>"#);
    let search = page(&waiting, "/search?q=parser").await;
    assert_has(&search, r#"<p class="coord">owns "#);

    let mut model = fixture();
    model.coordination = coordination(true);
    let over = site("owners-over", model, Vec::new()).await;
    let inbox = page(&over, "/").await;
    assert_has(
        &inbox,
        r#"<p class="coord"><strong class="over">wait over</strong>: <code>rollout:checkout</code> <span class="outcome shipped">shipped</span>"#,
    );
    assert_has(&inbox, "→ next: move &lt;ABC-123&gt;</p>");
    assert_lacks(&inbox, "(running in");
}

/// A ticket whose close check is put off says so, with when and what to check, wherever the
/// ticket is shown: its work items row, its map card, the Owners page's list of deferrals and
/// its log. The check's words are the session's, so they are escaped.
/// Mutation: drop the deferral from the work items row (red); from the map card (red); drop
/// the Owners page section (red); word the log line as anything else (red).
#[tokio::test]
async fn a_deferred_close_check_shows_on_every_page_with_the_ticket() {
    use crate::ledger::{Act, Entry, Note};
    let mut model = fixture();
    let mut coordination = coordination(false);
    coordination.ledger.apply(Entry {
        at: at(20),
        session: sid(2),
        act: Act::Defer {
            ticket: crate::types::JiraKey::parse("ABC-123").unwrap(),
            until: at(-86_400),
            check: Note::parse("a day of <clean> error rates").unwrap(),
        },
    });
    model.coordination = coordination;
    model.work_items = WorkItemsView::Listed {
        read_at: at(240),
        sections: vec![section(
            "Prod",
            vec![work_row("ABC-123", "Fix", vec![sid(2)])],
        )],
        failure: None,
    };
    let site = site("deferred", model, Vec::new()).await;
    let line = r#"<span class="deferred">close check deferred until Jan 16 08:00: a day of &lt;clean&gt; error rates</span>"#;
    assert_has(&page(&site, "/work-items").await, line);
    assert_has(&page(&site, "/map").await, line);
    let owners = page(&site, "/owners").await;
    assert_has(&owners, "<h2>Close checks deferred</h2>");
    assert_has(
        &owners,
        r#">ABC-123</a> until Jan 16 08:00: a day of &lt;clean&gt; error rates<span class="muted"> ("#,
    );
    assert_has(
        &owners,
        r#"</span> deferred the close check of <a href="https://example.atlassian.net/browse/ABC-123""#,
    );
}

/// A rollout a session waits on that nobody has started is a pending release: the map draws it
/// in the blocked colour with how many wait, and its card and the Owners page say so; a running
/// one is neither.
/// Mutation: draw a pending release as "not started" (red, no pending class); list only rollouts
/// that ran on the Owners page (red, portal missing there).
#[tokio::test]
async fn a_release_sessions_wait_on_and_nobody_runs_is_pending_on_the_map_and_owners_page() {
    use crate::ledger::{Act, Entry, Note, Slug, WaitOn};
    let mut model = fixture();
    model.coordination = coordination(false);
    model.coordination.ledger.apply(Entry {
        at: at(20),
        session: sid(4),
        act: Act::Wait {
            on: WaitOn::Rollout(Slug::parse("portal").unwrap()),
            then: Note::parse("re-run the pilot").unwrap(),
            ticket: None,
        },
    });
    let site = site("release-pending", model, Vec::new()).await;

    let map = page(&site, "/map").await;
    assert_eq!(
        map.body.matches(r#"class="n-rollout pending""#).count(),
        2,
        "the legend's sample and portal's node"
    );
    assert_has(&map, "release pending · 1 waiting");
    assert_has(&map, "running · 1 waiting");
    let pending = "Release pending: 1 session waits on its next finish, and nobody is running it.";
    assert_eq!(
        map.body.matches(pending).count(),
        1,
        "only portal's card says it"
    );

    let owners = page(&site, "/owners").await;
    assert_has(&owners, "<h3><code>rollout:portal</code></h3>");
    assert_has(&owners, pending);
}

/// A ticket another session wrote while session 2 owned it: the Owners page lists it with each
/// write after the rollouts and before the log, the non-owner's marked and its text escaped, and
/// that session's card warns.
/// Mutation: drop the collisions section, list it first on the page, drop the card's warning, or
/// name the writer instead of the owner in "while … owned it".
#[tokio::test]
async fn a_collision_is_listed_with_its_writes_and_warns_on_the_card() {
    use crate::ledger::shared::{OwnerThen, Shared, SharedWrite};
    use crate::types::JiraKey;
    use crate::types::outward::{Target, WriteKind};
    let mut model = fixture();
    model.coordination = coordination(false);
    let write = |session: u8, secs_ago: i64, owner_then: OwnerThen| SharedWrite {
        at: at(secs_ago),
        session: sid(session),
        kind: WriteKind::Comment,
        excerpt: Some("Moving <to> Prod".to_owned()),
        owner_then,
    };
    model.coordination.shared = vec![Shared {
        target: Target::Ticket(JiraKey::parse("ABC-123").unwrap()),
        writes: vec![
            write(2, 30, OwnerThen::Writer),
            write(1, 20, OwnerThen::Other(sid(2))),
        ],
    }];
    let site = site("collisions", model, Vec::new()).await;
    let owners = page(&site, "/owners").await;
    assert_has(&owners, "<h2>Where the work collided</h2>");
    let heading = |text: &str| owners.body.find(&format!("<h2>{text}</h2>")).unwrap();
    assert!(heading("Rollouts") < heading("Where the work collided"));
    assert!(heading("Where the work collided") < heading("Recent"));
    assert_has(&owners, "2 sessions, 1 write(s) by a non-owner");
    assert_has(&owners, r#"<tr class="foreign">"#);
    assert_has(
        &owners,
        r#"<span class="excerpt">Moving &lt;to&gt; Prod</span>"#,
    );
    let inbox = page(&site, "/").await;
    assert_has(
        &inbox,
        r#"<span class="clash">⚠ wrote <a href="https://example.atlassian.net/browse/ABC-123""#,
    );
    assert_has(
        &inbox,
        r#"</a> while <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000002">"#,
    );
}

fn usage_window(
    span: crate::usage::Span,
    percent: f64,
    resets_at: crate::types::Instant,
) -> crate::usage::Window {
    crate::usage::Window {
        span,
        used_percent: percent,
        resets_at: Some(resets_at),
    }
}

/// The header carries one meter per window, each followed by its reset: the five-hour one as a
/// countdown and the week's as a day; a failed read after it keeps the numbers, dims them and
/// says why, and the banner about unread sources stays out of it.
/// Mutation: show the week's reset as a countdown, show the title's "in 3h 5m" beside the
/// meter, drop the `stale` class on a failure, or round 7.6 up -> red.
#[tokio::test]
async fn the_header_shows_the_plan_usage_and_a_failed_read_dims_it() {
    use crate::usage::{Reading, Span, UsageError};
    let mut model = fixture();
    let mut reading = Reading::default();
    reading.record(
        Ok(vec![
            usage_window(Span::FiveHours, 7.6, at(-(3 * 3_600 + 5 * 60))),
            usage_window(Span::Week, 92.0, "2027-01-15T15:00:00Z".parse().unwrap()),
        ]),
        at(30),
    );
    model.usage = reading.clone();
    let fresh = site("usage", model, Vec::new()).await;
    let reply = page(&fresh, "/").await;
    assert_has(
        &reply,
        r#"<span class="usage" title="plan usage as of 30 s ago"><span class="gauge" title="5h: 7% used, resets in 3h 5m">5h <meter min="0" max="100" low="75" high="90" optimum="0" value="7.6"></meter> 7% <span class="reset">↻3h 5m</span></span><span class="gauge" title="7d: 92% used, resets Fri 15:00">7d <meter min="0" max="100" low="75" high="90" optimum="0" value="92"></meter> 92% <span class="reset">↻Fri 15:00</span></span>"#,
    );

    let mut model = fixture();
    reading.record(Err(UsageError::Expired), at(10));
    model.usage = reading;
    let failed = site("usage-failed", model, Vec::new()).await;
    let reply = page(&failed, "/").await;
    assert_has(
        &reply,
        r#"<span class="usage stale" title="plan usage as of 30 s ago; the latest read failed: Claude Code's sign-in has expired"#,
    );
    assert_lacks(&reply, "Not everything was read");
}

/// A read refused as too frequent says when the next one goes out: the refusal 30 s ago
/// doubled the minute to two, so it is 90 s away.
/// Mutation: leave out the next read, or count it from the last success -> red.
#[tokio::test]
async fn a_refused_usage_read_says_when_the_next_one_is() {
    use crate::usage::{Reading, RetryAfter, Span, UsageError};
    let mut reading = Reading::default();
    reading.record(
        Ok(vec![usage_window(Span::FiveHours, 7.6, at(-3_600))]),
        at(600),
    );
    reading.started(at(31));
    reading.record(
        Err(UsageError::Refused {
            retry_after: RetryAfter::NotSaid,
        }),
        at(30),
    );
    let mut model = fixture();
    model.usage = reading;
    let refused = site("usage-refused", model, Vec::new()).await;
    let reply = page(&refused, "/").await;
    assert_has(
        &reply,
        r#"<span class="usage stale" title="plan usage as of 10 min ago; the latest read failed: the usage endpoint turned the read away as too frequent (HTTP 429); it named no wait; the next read is in 1 min""#,
    );
}

/// A session this code could not classify is not calm: the header and the empty inbox count
/// them and link to the sessions page, and the health page has a row.
/// Mutation: drop `unknown_sessions > 0` from `problems` (the calm text returns), drop the
/// plural arm (the one-session row reads "1 sessions"), or link somewhere but `/sessions`.
#[tokio::test]
async fn unclassified_sessions_are_never_calm() {
    let mut model = fixture();
    model.inbox.clear();
    model.unknown_sessions = 2;
    let many = site("unknown-many", model.clone(), Vec::new()).await;
    let reply = page(&many, "/").await;
    assert_lacks(&reply, "Nothing is waiting on you.");
    assert_has(
        &reply,
        r#"Nothing is listed, but that is not the same as nothing waiting: <a href="/sessions">2 sessions could not be classified</a>. See the health page."#,
    );
    assert_has(
        &reply,
        r#"<div class="healthline">Not everything was read: <a href="/sessions">2 sessions could not be classified</a>. "#,
    );
    let health = page(&many, "/health").await;
    assert_has(
        &health,
        r#"<td>sessions that could not be classified</td><td class="bad">2</td><td></td><td><a href="/sessions">see each one's reason</a></td>"#,
    );

    model.unknown_sessions = 1;
    let one = site("unknown-one", model, Vec::new()).await;
    assert_has(
        &page(&one, "/").await,
        r#"<a href="/sessions">1 session could not be classified</a>"#,
    );
}

/// A followed transcript that could not be read is named in the header and on the health page
/// with its latest reason.
/// Mutation: drop the `follow_faults` arm of `problems`, or the health row.
#[tokio::test]
async fn transcript_tail_faults_show_in_the_header_and_on_the_health_page() {
    let mut model = fixture();
    model.health.follow_faults = FollowFaults {
        transcripts: 2,
        latest: Some(FollowFault {
            at: at(9),
            reason: format!("cannot read the file: permission denied {SECRET}"),
        }),
    };
    let site = site("tails", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(
        &reply,
        "Not everything was read: transcript tails (2 could not be read). ",
    );
    let health = page(&site, "/health").await;
    assert_has(
        &health,
        r#"<td>transcript tails not followed</td><td class="bad">2</td><td>2027-01-15 07:59:51 (9 s ago)</td><td>cannot read the file: permission denied [redacted]</td>"#,
    );
    assert_lacks(&health, SECRET);
}

fn slack(ts: &str) -> Link {
    Link::SlackThread {
        channel: "C0123ABCD".to_owned(),
        ts: ts.to_owned(),
    }
}

fn doc(url: &str) -> Link {
    Link::Doc {
        url: SafeUrl::parse(url).unwrap(),
    }
}

fn other(url: &str) -> Link {
    Link::Other {
        url: SafeUrl::parse(url).unwrap(),
    }
}

/// Chips read as what they are, and sort pull requests, Jira, Slack, documents, other. The
/// expected labels are written out: the Slack date is 2023-11-14 for `1700000000`, and the
/// long path is cut after its fortieth character.
/// Mutation: sort by anything else (the order row goes red), label a Slack chip with its
/// timestamp, cut the path at 41 characters, or show the Other chip's path.
#[tokio::test]
async fn link_chips_say_what_they_are_and_sort_by_kind() {
    let long_doc =
        "https://docs.example.com/wiki/spaces/ENG/pages/123456789/A-very-long-page-title?v=2";
    let mut model = fixture();
    let links = vec![
        link_view(other("https://other.example.org:8443/x/y?z=1"), None),
        link_view(doc(long_doc), None),
        link_view(doc("https://docs.example.com/"), None),
        link_view(slack("1700000000.000100"), None),
        link_view(jira(7), None),
        link_view(pr(42), Some(good_reading(None))),
    ];
    for view in &mut model.sessions {
        if view.session.id == sid(2) {
            view.links.clone_from(&links);
        }
    }
    let site = site("chips", model, Vec::new()).await;
    let reply = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    let chip = |class: &str, href: &str, label: &str| {
        // A pull request's chip carries its words in a `title` before the link.
        let title = if class == "pr" {
            r#" title="open · approved · checks passing · read 3 min ago""#
        } else {
            ""
        };
        format!(
            r#"<span class="chip {class}"{title}><a href="{href}" target="_blank" rel="noopener noreferrer">{label}</a>"#
        )
    };
    let expected = [
        chip(
            "pr",
            "https://github.com/example-org/example-repo/pull/42",
            "example-org/example-repo#42",
        ),
        chip(
            "jira",
            "https://example.atlassian.net/browse/ABC-7",
            "ABC-7",
        ),
        chip(
            "slack",
            "https://example.slack.com/archives/C0123ABCD/p1700000000000100",
            "Slack thread C0123ABCD · 2023-11-14",
        ),
        chip(
            "doc",
            long_doc,
            "docs.example.com/wiki/spaces/ENG/pages/123456789/A-very-…",
        ),
        chip("doc", "https://docs.example.com/", "docs.example.com"),
        chip(
            "link",
            "https://other.example.org:8443/x/y?z=1",
            "other.example.org:8443 · /x/y",
        ),
    ];
    let mut last = 0;
    for fragment in &expected {
        let at = reply.body.find(fragment.as_str());
        assert!(
            at.is_some_and(|at| at >= last),
            "{fragment} after {last} in:\n{}",
            reply.body
        );
        last = at.unwrap_or(last);
    }
}

/// A Jira chip says whose its ticket is, seen from the session it is shown for: session 2
/// claimed ABC-123, so its own page marks the chip as its own, session 1's page dims it and
/// points to session 2, and an unclaimed ticket's chip stays plain on both. A pull request
/// whose title names ABC-123 after a key nobody claimed (`UTF-8`) is marked the same way;
/// one whose title names no ticket stays plain.
/// Mutation: compare the owner with anything but the viewer (both pages go red), drop the
/// `→ owner` link, mark unclaimed tickets, or take only the title's first key.
#[tokio::test]
async fn a_chip_says_which_session_owns_its_ticket() {
    let mut model = fixture();
    model.coordination = coordination(false);
    let titled = Reading::Value {
        observed: Observed {
            answer: Answer::Found(PrState {
                title: "UTF-8 fix (ABC-123): change-log".to_owned(),
                ..pr_state()
            }),
            read_at: at(180),
        },
        age: Age::from_secs(180),
        failure: None,
    };
    let links = vec![
        link_view(jira(123), None),
        link_view(jira(7), None),
        link_view(pr(42), Some(titled)),
        link_view(pr(43), Some(good_reading(None))),
    ];
    for view in &mut model.sessions {
        if view.session.id == sid(1) || view.session.id == sid(2) {
            view.links.clone_from(&links);
        }
    }
    let site = site("chip-owners", model, Vec::new()).await;
    let ticket = |n: u32| {
        format!(
            r#"<a href="https://example.atlassian.net/browse/ABC-{n}" target="_blank" rel="noopener noreferrer">ABC-{n}</a>"#
        )
    };
    let plain = format!(r#"<span class="chip jira">{}"#, ticket(7));

    let owner = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_has(
        &owner,
        &format!(
            r#"<span class="chip jira mine"><span class="own-mark" title="This session owns it">●</span> {}"#,
            ticket(123)
        ),
    );
    assert_lacks(&owner, r#"<span class="owner""#);
    assert_has(&owner, &plain);
    assert_has(&owner, r#"<span class="chip pr mine" title="#);
    assert_has(
        &owner,
        r#"●</span> <a href="https://github.com/example-org/example-repo/pull/42""#,
    );
    assert_has(&owner, r#"<span class="chip pr" title="#);
    assert_eq!(owner.body.matches("own-mark").count(), 2, "{}", owner.body);

    let other = page(&site, "/sessions/00000000-0000-4000-8000-000000000001").await;
    assert_has(
        &other,
        &format!(
            r#"<span class="chip jira theirs">{} <span class="owner" title="Owned by another session">→ <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000002">"#,
            ticket(123)
        ),
    );
    assert_lacks(&other, "own-mark");
    assert_has(&other, &plain);
    assert_has(&other, r#"<span class="chip pr theirs" title="#);
    assert_has(&other, r#"<span class="chip pr" title="#);
    assert_eq!(
        other.body.matches(r#"<span class="owner""#).count(),
        2,
        "{}",
        other.body
    );
}

/// Assistant text is markdown, drawn as HTML by the renderer: headings, a table, a list and
/// links; raw HTML stays text; a link that is refused is its text and is counted in the
/// card's footnote.
/// Mutation: render nothing of the markdown (the `<h3>` row goes red), pass a `javascript:`
/// destination as an `href`, or drop the withheld count from the footnote.
#[tokio::test]
async fn card_text_is_markdown_and_refused_links_are_counted() {
    let text = "# Plan\n\nSee [the doc](https://example.com/doc), [bad](javascript:alert(1)) and http://plain.example/x.\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n- one\n- **two**\n\n<img src=x onerror=alert(1)>";
    let mut model = fixture();
    let card = |card: &mut PendingCard| {
        if let PendingCard::Turn { turn, .. } = card {
            turn.text = AssistantText::of(text);
        }
    };
    if let Some(entry) = model.inbox.iter_mut().find(|e| e.session == sid(2)) {
        card(&mut entry.card);
    }
    let site = site("markdown", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(&reply, "<h3>Plan</h3>");
    assert_has(
        &reply,
        r#"<a href="https://example.com/doc" target="_blank" rel="noopener noreferrer">the doc</a>"#,
    );
    assert_has(&reply, "<p>See <a ");
    assert_has(&reply, ", bad and http://plain.example/x.</p>");
    assert_has(
        &reply,
        r#"<table><thead><tr><th>a</th><th class="right">b</th></tr></thead><tr><td>1</td><td class="right">2</td></tr></table>"#,
    );
    assert_has(&reply, "<ul><li>one</li><li><strong>two</strong></li></ul>");
    assert_has(&reply, "&lt;img src=x onerror=alert(1)&gt;");
    assert_lacks(&reply, "<img");
    assert_lacks(&reply, "javascript:");
    assert_has(&reply, "2 links not clickable: not https or local http");
}

/// The card whose body text has `text`, in a model that was read completely, and its page.
async fn page_with_card_text(label: &str, text: &str) -> Reply {
    let mut model = fixture();
    if let Some(entry) = model.inbox.iter_mut().find(|e| e.session == sid(2))
        && let PendingCard::Turn { turn, .. } = &mut entry.card
    {
        turn.text = AssistantText::of(text);
    }
    let site = site(label, model, Vec::new()).await;
    page(&site, "/").await
}

/// A relative markdown link and a bare `http://` one are both left without an anchor; the
/// card says how many at its bottom, and the header, which is about what was not read, says
/// nothing.
/// Mutation: leave the note out of the inbox card (red, `link_note` call removed).
#[tokio::test]
async fn a_card_with_links_left_out_says_so_and_the_header_stays_calm() {
    let reply = page_with_card_text(
        "linknote",
        "Read [the guide](docs/guide.md) and http://plain.example/x.",
    )
    .await;
    assert_has(
        &reply,
        "<p class=\"muted\">2 links not clickable: not https or local http</p>",
    );
    assert_has(&reply, "Read the guide and http://plain.example/x.");
    assert_lacks(&reply, "Not everything was read");
    assert_lacks(&reply, "not shown");
}

/// One link left out reads "1 link", and text whose links all are https has no note.
/// Mutation: always say "links", or emit the note when the count is 0.
#[tokio::test]
async fn the_link_note_is_singular_for_one_and_absent_for_none() {
    let one = page_with_card_text("linknote1", "See [the guide](docs/guide.md).").await;
    assert_has(&one, "1 link not clickable: not https or local http");
    assert_lacks(&one, "1 links");

    let none = page_with_card_text("linknote0", "See [the doc](https://example.com/doc).").await;
    assert_has(&none, ">the doc</a>");
    assert_lacks(&none, "not clickable");
}

/// A turn end on the session page carries the same note, under its own text only.
/// Mutation: drop the note from the session page's turn end, or count the whole page's
/// links in each turn end.
#[tokio::test]
async fn a_turn_end_on_the_session_page_has_its_own_link_note() {
    let mut model = fixture();
    let view = model
        .sessions
        .iter_mut()
        .find(|view| view.session.id == sid(2))
        .unwrap();
    view.last_turn_ends[0].text = AssistantText::of("See [the guide](docs/guide.md).");
    view.last_turn_ends[1].text = AssistantText::of("Nothing to link.");
    let site = site("linknote-session", model, Vec::new()).await;
    let reply = page(&site, "/sessions/00000000-0000-4000-8000-000000000002").await;
    assert_eq!(
        reply.body.matches("not clickable").count(),
        1,
        "{}",
        reply.body
    );
    assert_has(&reply, "1 link not clickable: not https or local http");
}

/// A log that does not exist is "no hook has run yet", still not calm, in the header and
/// on the health page with its path.
/// Mutation: report `NeverWritten` as ok (the header row goes red), as `Failed` (the phrase
/// goes red), or leave the path out of the health row.
#[tokio::test]
async fn a_missing_event_log_says_no_hook_has_run_yet() {
    let mut model = fixture();
    model.health.event_log = SourceState::NeverWritten {
        at: at(7),
        path: PathBuf::from("/state/events.jsonl"),
    };
    let site = site("neverwritten", model, Vec::new()).await;
    let inbox = page(&site, "/").await;
    assert_has(
        &inbox,
        "Not everything was read: event log: no hook has run yet (register it with conn hook --print-settings). ",
    );
    assert_lacks(&inbox, "event log (failed)");
    let health = page(&site, "/health").await;
    assert_has(&health, "<td class=\"bad\">never written</td>");
    assert_has(
        &health,
        "no file at /state/events.jsonl: no hook has run yet (register it with conn hook --print-settings)",
    );
}

// --- changes ---

/// Mutation: save the wrong mark (the card's `since` replaced by the clock), skip `known`,
/// answer 200 for a session the model lacks, or let a missing `since` default to now (the
/// no-card row answers 200).
#[tokio::test]
async fn seen_pin_and_unpin_write_the_marks_and_refuse_unknown_sessions() {
    let site = fixture_site("marks").await;
    let id = "00000000-0000-4000-8000-000000000002";
    let seen = post(
        &site,
        "/seen",
        &format!("session={id}&since=2027-01-15T07%3A55%3A00Z"),
    )
    .await;
    assert_eq!(seen.status, StatusCode::OK);
    assert_eq!(seen.body.trim(), r#"{"outcome":"changed"}"#);
    assert_eq!(
        post(&site, "/pin", &format!("session={id}")).await.status,
        StatusCode::OK
    );
    let file = std::fs::read_to_string(site.scratch.join("state/seen.json")).unwrap();
    assert!(
        file.contains(&format!("\"{id}\": \"2027-01-15T07:55:00Z\"")),
        "{file}"
    );
    assert!(
        file.contains(&format!("\"pinned\": [\n    \"{id}\"\n  ]")),
        "{file}"
    );
    assert_eq!(
        post(&site, "/unpin", &format!("session={id}")).await.status,
        StatusCode::OK
    );
    let file = std::fs::read_to_string(site.scratch.join("state/seen.json")).unwrap();
    assert!(file.contains("\"pinned\": []"), "{file}");

    let unknown = post(
        &site,
        "/seen",
        "session=00000000-0000-4000-8000-0000000000ee&since=2027-01-15T07%3A55%3A00Z",
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let bad = post(
        &site,
        "/seen",
        "session=nope&since=2027-01-15T07%3A55%3A00Z",
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.body.starts_with("field `session`:"), "{}", bad.body);
    let no_card = post(&site, "/seen", &format!("session={id}")).await;
    assert_eq!(no_card.status, StatusCode::BAD_REQUEST);
    assert!(
        no_card.body.starts_with("field `since`: missing or blank"),
        "{}",
        no_card.body
    );
}

/// Mutation: read a JSON body again (the JSON row answers 200), or name the wrong field.
#[tokio::test]
async fn bad_bodies_are_refused_and_a_bad_field_is_named() {
    let site = fixture_site("bodies").await;
    let json = send(
        &site,
        Method::POST,
        "/pin",
        &[
            HOST,
            COOKIE,
            ORIGIN,
            HEADER,
            ("content-type", "application/json"),
        ],
        r#"{"session": "00000000-0000-4000-8000-000000000002"}"#,
    )
    .await;
    assert_eq!(json.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let missing = post(&site, "/pin", "other=1").await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert!(
        missing
            .body
            .starts_with("field `session`: missing or blank"),
        "{}",
        missing.body
    );
    let bad_link = post(&site, "/refresh-link", "link=ftp%3A%2F%2Fx").await;
    assert_eq!(bad_link.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_link.body.starts_with("field `link`:"),
        "{}",
        bad_link.body
    );
    let plain = send(
        &site,
        Method::POST,
        "/seen",
        &[HOST, COOKIE, ORIGIN, HEADER, ("content-type", "text/plain")],
        "x",
    )
    .await;
    assert_eq!(plain.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let big = "a".repeat(70 * 1024);
    let oversized = post(&site, "/seen", &format!("session={big}")).await;
    assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A scripted command that exits 0 printing `stdout`.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a script is a list of call results, some of them errors"
)]
fn ok(stdout: &str) -> Result<Output, crate::util::runner::RunError> {
    Ok(Output {
        exit: Exit::Success,
        stdout: stdout.as_bytes().to_vec(),
        stderr: String::new(),
    })
}

/// The focus outcomes against the scripted compositor, from the real niri fixture: one
/// window with the title is focused with exactly the two calls; two windows are ambiguous and
/// nothing is focused; no terminal for a dormant session. Mutation: focus on `Ambiguous`, or
/// ask for the window of a session that is not live.
#[tokio::test]
async fn focus_raises_a_unique_window_and_reports_every_other_outcome() {
    let site = site(
        "focus",
        fixture(),
        vec![ok(NIRI_WINDOWS), ok(""), ok(NIRI_WINDOWS), ok(NIRI_WINDOWS)],
    )
    .await;
    let unique = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000001",
    )
    .await;
    assert_eq!(unique.status, StatusCode::OK);
    assert_eq!(
        unique.body.trim(),
        r#"{"detail":"The terminal is focused.","outcome":"focused"}"#
    );
    assert_eq!(
        site.runner.lines(),
        [
            "niri msg --json windows",
            "niri msg action focus-window --id 11"
        ]
    );
    let ambiguous = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000007",
    )
    .await;
    assert_eq!(
        ambiguous.body.trim(),
        r#"{"detail":"2 terminals show this session's title, so none was picked; switch to it by hand.","outcome":"ambiguous"}"#
    );
    assert_eq!(
        site.runner.lines().len(),
        3,
        "no focus call after an ambiguous match"
    );
    let not_found = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000003",
    )
    .await;
    assert_has(&not_found, r#""outcome":"not_found""#);
    let dormant = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000008",
    )
    .await;
    assert_has(&dormant, r#""outcome":"no_terminal""#);
    assert_eq!(
        site.runner.lines().len(),
        4,
        "a dormant session asks the compositor nothing"
    );
}

/// A live terminal session with no title of its own and no window pinned to its id is
/// `untitled`, not `no_terminal`, and offers no resume command.
/// Mutation: answer `NoTerminal` for a missing title (the outcome word differs).
#[tokio::test]
async fn a_live_session_without_a_title_is_untitled() {
    let mut model = fixture();
    let untitled = sid(3);
    for view in &mut model.sessions {
        if view.session.id == untitled {
            view.session.window_title = None;
        }
    }
    let site = site("untitled", model, vec![ok(NIRI_WINDOWS)]).await;
    let reply = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000003",
    )
    .await;
    assert_has(&reply, r#""outcome":"untitled""#);
    assert_lacks(&reply, "resume");
}

/// A window titled `Claude: <session id>` by a pinning hook is the session's, whatever title
/// the transcript says Claude Code would show; it is tried first.
/// Mutation: try only the transcript's title (no window has it: `not_found`), or try the
/// pinned title second (the stale-titled session 3 has none, so this still passes; the
/// listing order is pinned in `windows.rs`).
#[tokio::test]
async fn a_window_pinned_to_the_session_id_is_focused() {
    let windows = r#"[
        {"id": 41, "app_id": "kitty", "title": "◑ Claude: 00000000-0000-4000-8000-000000000003"},
        {"id": 42, "app_id": "kitty", "title": "✳ Claude Code"}
    ]"#;
    let site = site("pinned", fixture(), vec![ok(windows), ok("")]).await;
    let reply = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000003",
    )
    .await;
    assert_eq!(
        reply.body.trim(),
        r#"{"detail":"The terminal is focused.","outcome":"focused"}"#
    );
    assert_eq!(
        site.runner.lines(),
        [
            "niri msg --json windows",
            "niri msg action focus-window --id 41"
        ]
    );
}

/// Mutation: answer `focused` when the compositor was unavailable.
#[tokio::test]
async fn an_unreachable_compositor_is_reported_as_unavailable() {
    let site = site(
        "niri-down",
        fixture(),
        vec![Err(crate::util::runner::RunError::NotFound)],
    )
    .await;
    let reply = post(
        &site,
        "/focus",
        "session=00000000-0000-4000-8000-000000000001",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_has(&reply, r#""outcome":"unavailable""#);
    assert_has(&reply, "niri is not installed or not on PATH");
}

/// Mutation: mark nothing, or mark a pull request that is on no card.
#[tokio::test]
async fn refresh_link_accepts_a_pull_request_and_refuses_anything_else() {
    let site = fixture_site("refresh").await;
    let ok = post(
        &site,
        "/refresh-link",
        "link=https%3A%2F%2Fgithub.com%2Fexample-org%2Fexample-repo%2Fpull%2F42",
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    let jira = post(
        &site,
        "/refresh-link",
        "link=https%3A%2F%2Fexample.atlassian.net%2Fbrowse%2FABC-123",
    )
    .await;
    assert_eq!(jira.status, StatusCode::BAD_REQUEST);
    assert!(jira.body.contains("`link`"));
}

// --- PAM ---

const PAM_AUTO: &str = "folders/111/locations/global/entitlements/admin";
const PAM_MANUAL: &str =
    "projects/example-project/locations/global/entitlements/become-network-admin";

fn pam_config(break_glass: &[&str]) -> crate::pam::PamConfig {
    crate::pam::PamConfig {
        organizations: crate::pam::Organizations::of(
            [crate::pam::ids::NumericId::parse("999").unwrap()].into(),
        )
        .unwrap(),
        quota_project: crate::pam::ids::ProjectId::parse("quota-project").unwrap(),
        break_glass: break_glass
            .iter()
            .map(|name| crate::pam::ids::EntitlementName::parse(name).unwrap())
            .collect(),
    }
}

/// The page over the fixture entitlements: the automatic one holds a grant activated at
/// 06:48 for 4 h, so 2 h 48 min are left at the site's 08:00; the manual one is kept for
/// emergencies, and its grant was denied an hour ago.
async fn pam_page(label: &str, failure: Option<crate::pam::gcloud::PamError>) -> Reply {
    let denied = r#"[{"state": "DENIED", "createTime": "2027-01-15T07:00:00Z",
        "requestedDuration": "3600s"}]"#;
    pam_page_with(label, failure, denied).await
}

/// [`pam_page`] with `manual` as the manual entitlement's grants.
async fn pam_page_with(
    label: &str,
    failure: Option<crate::pam::gcloud::PamError>,
    manual: &str,
) -> Reply {
    let active = r#"[{"state": "ACTIVE", "createTime": "2027-01-15T06:45:00Z",
        "requestedDuration": "14400s",
        "timeline": {"events": [{"eventTime": "2027-01-15T06:48:00Z", "activated": {}}]}}]"#;
    let mut model = fixture();
    model.pam = Some(crate::pam::Reading::of_answers(
        pam_config(&[PAM_MANUAL]),
        include_str!("../../tests/fixtures/pam/entitlements.json"),
        at(600),
        &[(PAM_AUTO, active), (PAM_MANUAL, manual)],
        failure,
        crate::pam::Places::Read(
            [(
                crate::pam::ids::Parent::parse("folders/111").unwrap(),
                "platform / orchard".to_owned(),
            )]
            .into(),
        ),
    ));
    let site = site(label, model, Vec::new()).await;
    let reply = page(&site, "/pam").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply
}

/// Off without `[pam]`: no nav entry, and the page says how to turn it on.
/// Mutation: list PAM in the nav unconditionally (red).
#[tokio::test]
async fn the_pam_page_is_off_without_its_section() {
    let site = fixture_site("pam-off").await;
    let reply = page(&site, "/pam").await;
    assert_has(&reply, "PAM is not configured");
    assert_lacks(&page(&site, "/health").await, r#"href="/pam""#);
}

/// The held grant leads the page, tinted as active, naming where it sits (linked to that
/// folder's PAM page) and its time left, with no request; the rest sit under a header row per
/// parent, linked the same way, and a free entitlement offers only the lengths PAM accepts,
/// four hours (or the maximum) first, with the reason marked required where PAM requires it;
/// break-glass entitlements sit in their own box below.
/// Mutation: offer a request beside a live grant (red, two forms), drop the held section
/// (red), drop the break-glass section (red), leave `required` off a mandatory reason (red),
/// or give the group header no filter text (red).
#[tokio::test]
async fn the_pam_page_shows_time_left_and_requests_only_what_pam_accepts() {
    let reply = pam_page("pam-on", None).await;
    assert_has(&reply, r#"<a href="/pam" class="active">PAM</a>"#);
    assert_has(&reply, r#"<input id="filter" type="search""#);
    // `.status` is the header bar's flex box; a cell carrying it leaves the table layout.
    assert_lacks(&reply, r#"<td class="status">"#);
    let (held, rest) = reply
        .body
        .split_once("<h2>Request</h2>")
        .expect("a held section, then the rest");
    assert!(held.contains("<h2>Held now (1)</h2>"), "{held}");
    assert!(
        held.contains("<tr><th>Entitlement</th><th>Approval</th><th>Status</th></tr>"),
        "a held grant has no length or request to offer: {held}"
    );
    assert!(
        rest.contains(r#"<table class="items pam" data-filter-scope>"#),
        "the request table hides when the filter leaves none of its rows: {rest}"
    );
    assert!(
        held.contains(concat!(
            r#"<tr class="held active" data-filter="folders/111/locations/global/entitlements/admin platform / orchard roles/admin automatic">"#,
            r#"<td class="entitlement" title="folders/111/locations/global/entitlements/admin"><strong>admin</strong>"#,
            r#"<span class="pam-line"><a href="https://console.cloud.google.com/iam-admin/pam?folder=111" target="_blank" rel="noopener noreferrer">"#,
            r#"<span class="pam-place">platform / orchard</span> <span class="muted">folders/111</span></a></span>"#,
        )),
        "{held}"
    );
    assert!(held.contains("2 h 48 min left"), "{held}");
    assert!(held.contains("(ends 2027-01-15 10:48:00)"), "{held}");
    assert!(
        !held.contains(r#"action="/pam/request""#),
        "a live grant offers no request"
    );
    assert!(
        rest.contains(concat!(
            r#"<tr class="pam-group" data-filter="projects/example-project/locations/global/entitlements/become-network-admin"#,
            r#" roles/compute.networkadmin roles/compute.securityadmin group:team-a@example.com">"#,
            r#"<td colspan="5"><a href="https://console.cloud.google.com/iam-admin/pam?project=example-project" target="_blank" rel="noopener noreferrer">"#,
            r#"<span class="pam-place">projects/example-project</span></a></td></tr>"#,
        )),
        "{rest}"
    );
    let glass = rest
        .split_once(r#"<section class="break-glass" data-filter-scope>"#)
        .map(|(_, glass)| glass)
        .expect("a break-glass section");
    assert!(glass.contains("become-network-admin"), "{glass}");
    assert!(glass.contains("Denied<br>requested 1 h ago"), "{glass}");
    assert!(glass.contains(r#"action="/pam/request""#), "{glass}");
    assert!(
        glass.contains(&format!(
            r#"<input type="hidden" name="entitlement" value="{PAM_MANUAL}">"#
        )),
        "{glass}"
    );
    assert!(
        glass.contains(r#"<option value="14400" selected>4 h</option>"#),
        "{glass}"
    );
    assert!(
        glass.contains(r#"<option value="28800">8 h</option>"#),
        "{glass}"
    );
    assert!(
        !glass.contains(r#"value="32400""#),
        "nothing past the maximum"
    );
    assert!(
        glass.contains(r#"required placeholder="why (required)""#),
        "{glass}"
    );
}

/// A grant awaiting approval offers a one-line Slack ask: what, where and for how long, linked
/// to the approvals page.
/// Mutation: drop the button from the pending arm (red), or give the length asked of the
/// entitlement's maximum instead of the grant's (red: 8 h).
#[tokio::test]
async fn a_grant_awaiting_approval_offers_a_slack_ask() {
    let pending = r#"[{"state": "APPROVAL_AWAITED", "createTime": "2027-01-15T07:30:00Z",
        "requestedDuration": "21600s"}]"#;
    let reply = pam_page_with("pam-ask", None, pending).await;
    assert_has(&reply, "approvals page");
    assert_has(
        &reply,
        concat!(
            r#"<button class="copy-text" type="button" data-copy="🔐 PAM approval, please: *become-network-admin* on `example-project` for 6 h 👉 "#,
            r#"[approve here](https://console.cloud.google.com/iam-admin/pam/grants/approvals?project=example-project&amp;supportedpurview=project) 🙏""#,
        ),
    );
}

/// A lapsed gcloud sign-in names the command that renews it, and the last list stays.
/// Mutation: show the error text instead of the hint (red).
#[tokio::test]
async fn a_lapsed_gcloud_sign_in_says_how_to_renew_it() {
    let reply = pam_page(
        "pam-signin",
        Some(crate::pam::gcloud::PamError::NotSignedIn),
    )
    .await;
    assert_has(
        &reply,
        "gcloud needs a fresh sign-in: run <code>gcloud auth login</code>",
    );
    assert_has(&reply, "2 h 48 min left");
}

/// A request is a change like any other: it needs the write token, names a bad field, and is
/// refused with a conflict where PAM is not configured, before gcloud is asked.
/// Mutation: map `NotConfigured` to 200 (red), or skip the entitlement parse (red, 409).
#[tokio::test]
async fn a_grant_request_is_parsed_and_refused_without_pam() {
    let site = fixture_site("pam-request").await;
    let bad = post(&site, "/pam/request", "entitlement=admin&secs=3600").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.body.contains("`entitlement`"), "{}", bad.body);
    let body = format!(
        "entitlement={}&secs=3600&justification=deploy",
        PAM_AUTO.replace('/', "%2F")
    );
    let off = post(&site, "/pam/request", &body).await;
    assert_eq!(off.status, StatusCode::CONFLICT);
    assert!(off.body.contains("no [pam] section"), "{}", off.body);
    let unsigned = send(
        &site,
        Method::POST,
        "/pam/request",
        &[HOST, COOKIE, ORIGIN, FORM],
        &body,
    )
    .await;
    assert_eq!(unsigned.status, StatusCode::FORBIDDEN);
    assert_eq!(site.runner.calls(), []);
}

// --- navigation, search and work items ---

/// The header's entries, in the order the page promises.
/// Mutation: swap `Work items` and `Search` in `Nav::ALL` (ran red).
#[tokio::test]
async fn the_nav_lists_every_page_in_order() {
    let site = fixture_site("nav").await;
    let reply = page(&site, "/health").await;
    assert_has(
        &reply,
        concat!(
            r#"<a href="/">Inbox (3)</a><a href="/followups">Follow-ups</a>"#,
            r#"<a href="/owners">Owners</a><a href="/sessions">Sessions</a>"#,
            r#"<a href="/dormant">Dormant</a><a href="/map">Map</a>"#,
            r#"<a href="/work-items">Work items</a><a href="/approvals">Approvals</a>"#,
            r#"<a href="/search">Search</a><a href="/health" class="active">Health</a>"#,
        ),
    );
}

/// The Follow-ups page says how to turn it on while it is off; on, each row names who acts and
/// hands them the prompt the one way that reaches them: copied with the terminal raised for a
/// session in a terminal, its resume command with the prompt for a dormant one, a new
/// session's command in the directory found for it, and nothing to paste for a release. The
/// close checks are one sweep that lists each item, with what to check for one whose deferral
/// is over; one not ready is listed under "Not yet" with why (rolling out, with its owner,
/// deferred), outside the count, and any deferral says until when, how far off, which session
/// put it off (linked) and what to check. The nav counts the rows, and a prompt is
/// shell-quoted.
/// Mutation: offer the terminal to a dormant session (red, no resume command); leave the prompt
/// unquoted in a command (red, the quoted form is missing); drop the nav count (red); drop the
/// reached deferral's line from the sweep prompt (red); drop the "Not yet" list (red); drop the
/// owner from a with-owner line (red); drop the deferral from a with-owner line (red); drop
/// the deferring session's link (red).
#[tokio::test]
async fn the_followups_page_hands_each_prompt_to_whoever_acts() {
    let off = fixture_site("followups-off").await;
    assert_has(&page(&off, "/followups").await, "Follow-ups are off");

    let mut model = fixture();
    model.followups = crate::model::followups::FollowUpsView::On {
        due: followup_rows(),
        not_yet: not_yet_rows(),
    };
    let site = site("followups", model, Vec::new()).await;
    let reply = page(&site, "/followups").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_has(
        &reply,
        r#"<a href="/followups" class="active">Follow-ups (5)</a>"#,
    );
    assert_has(
        &reply,
        r#"<div class="cmd prompt"><pre>2 comment(s) on ABC-7 came after my latest one. Read them and draft a reply for me to approve before you post it.</pre><button class="copy-open primary" type="button">Copy &amp; open terminal</button></div>"#,
    );
    assert_has(
        &reply,
        r"<pre>cd /home/dev/proj-8 &amp;&amp; claude --resume 00000000-0000-4000-8000-000000000008 'Your wait on rollout:checkout is over. You said you would then: re-run the team'\''s dry run. Go ahead.'</pre>",
    );
    assert_has(
        &reply,
        "Release pending: 1 session waits on its next finish",
    );
    assert_has(&reply, "<strong>You</strong>: start the release");
    assert_has(
        &reply,
        r"<pre>cd '/home/dev/proj 2' &amp;&amp; claude 'Take over ABC-9: run `conn claim ABC-9` first,",
    );
    assert_has(
        &reply,
        r"(run it in the right repository: no session that named it worked in one)",
    );
    assert_has(
        &reply,
        "<pre>claude 'Close-check sweep. These work items are shipped and waiting for their close check:\n- ABC-10 (Prod)\n- ABC-11 (Prod): deferred until 2027-01-15T07:59:20Z, to check then: error rates under 0.1% for a week\n\nFor each one, in turn: run `conn claim &lt;key&gt;` first;",
    );
    assert_has(&reply, "<h3>Close checks: 2 work items</h3>");
    assert_has(
        &reply,
        r#"Deferred until Fri Jan 15 07:59 by <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000003">Schema question</a></span>, which has come: error rates under 0.1% for a week"#,
    );
    assert_has(&reply, "<h2>Not yet</h2>");
    assert_has(
        &reply,
        r#"<tr><td class="when"><span class="muted">—</span></td><td class="when muted"></td><td><code>ABC-13</code> <span class="muted">a shipped ticket</span></td><td>rolling out in <code>rollout:nightly</code>, run by <span class="who running">"#,
    );
    assert_has(
        &reply,
        r#"<tr><td class="when"><strong>Fri Jan 22 08:00</strong></td><td class="when muted">in 7 d</td><td><code>ABC-14</code> <span class="muted">a shipped ticket</span></td><td>its owner <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000002">Parser rework</a></span>, which is at work<br>deferred by <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000003">Schema question</a></span></td><td class="muted">the export ran twice</td></tr>"#,
    );
    assert_has(
        &reply,
        r#"<tr><td class="when"><strong>Sat Jan 16 08:00</strong></td><td class="when muted">in 1 d</td><td><code>ABC-12</code> <span class="muted">a shipped ticket</span></td><td>deferred by <span class="who running"><a href="/sessions/00000000-0000-4000-8000-000000000003">Schema question</a></span></td><td class="muted">the November invoices match</td></tr>"#,
    );
}

/// One follow-up per way of handing a prompt over: an idle session in a terminal, a dormant
/// one, the user, a new session with a directory and one without.
fn followup_rows() -> Vec<crate::model::followups::FollowUp> {
    use crate::ledger::{Note, Slug, WaitOn};
    use crate::model::followups::{
        CloseCheckDue, Due, FollowUp, Handler, Over, Presence, TicketReason,
    };
    use std::num::NonZeroUsize;

    let key = |n| WorkItemKey::classify(&format!("ABC-{n}"), &[JiraPrefix::parse("ABC").unwrap()]);
    let ticket = |n, reasons| Due::Ticket {
        key: key(n),
        url: None,
        summary: Redacted::of("a ticket"),
        reasons,
        mentioned_in: Vec::new(),
    };
    vec![
        FollowUp {
            due: ticket(
                7,
                vec![TicketReason::Asked {
                    count: NonZeroUsize::new(2).unwrap(),
                    newest: at(60),
                }],
            ),
            handler: Handler::Session {
                id: sid(2),
                presence: Presence::Live,
            },
            since: Some(at(60)),
        },
        FollowUp {
            due: Due::Wait {
                session: sid(8),
                on: WaitOn::Rollout(Slug::parse("checkout").unwrap()),
                then: Note::parse("re-run the team's dry run").unwrap(),
                over: Over::Time { at: at(30) },
            },
            handler: Handler::Session {
                id: sid(8),
                presence: Presence::Dormant,
            },
            since: Some(at(30)),
        },
        FollowUp {
            due: Due::Release {
                rollout: Slug::parse("portal").unwrap(),
                waiting: vec![sid(2)],
            },
            handler: Handler::You,
            since: None,
        },
        FollowUp {
            due: ticket(9, vec![TicketReason::OwnerGone { owner: sid(9) }]),
            handler: Handler::NewSession {
                dir: Some(PathBuf::from("/home/dev/proj 2")),
            },
            since: None,
        },
        FollowUp {
            due: Due::CloseChecks {
                items: vec![
                    CloseCheckDue {
                        item: closing_item(10),
                        reached: None,
                    },
                    CloseCheckDue {
                        item: closing_item(11),
                        reached: Some(deferral(40, "error rates under 0.1% for a week")),
                    },
                ],
            },
            handler: Handler::NewSession { dir: None },
            since: Some(at(40)),
        },
    ]
}

/// One item per reason it is not ready for its close check.
fn not_yet_rows() -> Vec<crate::model::followups::NotYet> {
    use crate::model::followups::{Hold, NotYet, Presence};
    vec![
        NotYet {
            item: closing_item(13),
            hold: Hold::RollingOut {
                rollout: crate::ledger::Slug::parse("nightly").unwrap(),
                run: crate::ledger::Run {
                    session: sid(2),
                    since: at(3600),
                    ships: Vec::new(),
                },
                deferred: None,
            },
        },
        NotYet {
            item: closing_item(14),
            hold: Hold::WithOwner {
                owner: sid(2),
                presence: Presence::Working,
                deferred: Some(deferral(-7 * 86_400, "the export ran twice")),
            },
        },
        NotYet {
            item: closing_item(12),
            hold: Hold::Deferred(deferral(-86_400, "the November invoices match")),
        },
    ]
}

fn closing_item(n: u32) -> crate::model::followups::ClosingItem {
    crate::model::followups::ClosingItem {
        key: WorkItemKey::classify(&format!("ABC-{n}"), &[JiraPrefix::parse("ABC").unwrap()]),
        url: None,
        summary: Redacted::of("a shipped ticket"),
        status: Redacted::of("Prod"),
    }
}

fn deferral(until: i64, check: &str) -> crate::ledger::Deferral {
    crate::ledger::Deferral {
        until: at(until),
        check: crate::ledger::Note::parse(check).unwrap(),
        by: sid(3),
        since: at(0),
    }
}

/// A Jira key on any page is a plain link to `/browse/KEY` on the configured host, opening in
/// a new tab.
/// Mutation: build the href with `/issue/` in place of `/browse/` (ran red).
#[tokio::test]
async fn a_jira_key_on_the_session_page_is_a_plain_browse_link() {
    let site = fixture_site("jira-chip").await;
    let reply = page(&site, &format!("/sessions/{}", sid(2))).await;
    assert_has(
        &reply,
        r#"<a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a>"#,
    );
}

fn search_model() -> Model {
    let mut model = fixture();
    model.prompts.insert(
        sid(5),
        Arc::from(vec![HeldPrompt {
            at: at(900),
            text: Redacted::of(&format!("please check ABC-123 with {SECRET} set")),
        }]),
    );
    model
}

/// An empty query says what is searched and shows no count; a query shows the count, each
/// session with the matched text emphasised, and the session's actions.
/// Mutation, each ran red: drop the Open terminal button, or search for a fixed query even when none was
/// typed (the empty page shows a count).
#[tokio::test]
async fn the_search_page_explains_itself_and_shows_matches_with_actions() {
    let site = site("search", search_model(), Vec::new()).await;

    let empty = page(&site, "/search").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_has(
        &empty,
        "Searches the titles, working directories and links of every session, and its last 50 prompts and last 5 replies; not whole transcripts.",
    );
    assert_lacks(&empty, "match");
    assert_eq!(page(&site, "/search?q=%20").await.body, empty.body);

    let found = page(&site, "/search?q=abc-123").await;
    assert_eq!(found.status, StatusCode::OK);
    assert_has(&found, r#"<input type="search" name="q" value="abc-123""#);
    assert_has(&found, "<strong>2 sessions match</strong>");
    // Session 2 mentions the key in a link (its chip), session 5 in a prompt.
    assert_has(&found, "<mark>ABC-123</mark>");
    assert_has(&found, r#"<span class="tag">link</span>"#);
    assert_has(&found, r#"<span class="tag">prompt</span>"#);
    assert_has(&found, "please check ");
    assert_has(&found, "Open terminal");
    assert_has(
        &found,
        r#"<a href="/sessions/00000000-0000-4000-8000-000000000002">Parser rework</a>"#,
    );
}

/// Snippets are redacted and escaped, and so is the echoed query.
/// Mutation, ran red: echo the query without `redact` (the token row shows the token). Matching
/// unredacted text does not compile: the search reads `Redacted` only.
#[tokio::test]
async fn search_snippets_and_the_echoed_query_are_redacted_and_escaped() {
    let site = site("search-escape", search_model(), Vec::new()).await;
    let token = page(&site, &format!("/search?q={SECRET}")).await;
    assert_has(&token, "<strong>0 sessions match</strong>");
    assert_has(&token, r#"value="[redacted]""#);
    assert_lacks(&token, SECRET);

    let script = page(&site, "/search?q=%3Cscript%3Ealert").await;
    assert_has(&script, "<strong>1 session matches</strong>");
    assert_has(&script, "<mark>&lt;script&gt;alert</mark>");
    assert_has(&script, r#"value="&lt;script&gt;alert""#);
    assert_lacks(&script, "<script>alert");

    let leaked = page(&site, "/search?q=leaked").await;
    assert_has(&leaked, "Token [redacted] <mark>leaked</mark>.");
    assert_lacks(&leaked, SECRET);
}

/// A query over 200 characters is a 400 that names `q`.
/// Mutation: drop the bound in `Query::parse` (ran red on the route test too).
#[tokio::test]
async fn a_query_over_two_hundred_characters_is_a_bad_request_naming_q() {
    let site = fixture_site("search-long").await;
    let ok = page(&site, &format!("/search?q={}", "x".repeat(200))).await;
    assert_eq!(ok.status, StatusCode::OK);
    let long = page(&site, &format!("/search?q={}", "x".repeat(201))).await;
    assert_eq!(long.status, StatusCode::BAD_REQUEST);
    assert_has(
        &long,
        "q in the path: is 201 characters; at most 200 are allowed",
    );
}

/// More than 200 matches show 200 and say so.
/// Mutation: change the truncation sentence (ran red).
#[tokio::test]
async fn a_truncated_search_says_how_many_matched() {
    let mut model = Model::unread(at(4));
    all_read(&mut model);
    model.sessions = (1..=201)
        .map(|n| {
            view(
                session(n, "same title", Activity::Running { since: at(10) }),
                None,
            )
        })
        .collect();
    let site = site("search-bound", model, Vec::new()).await;
    let reply = page(&site, "/search?q=same").await;
    assert_has(&reply, "<strong>201 sessions match</strong>");
    assert_has(
        &reply,
        "Showing the best 200 of them; narrow the query to see the rest.",
    );
    assert_eq!(reply.body.matches(r#"<article class="card""#).count(), 200);
}

fn work_row(key: &str, summary: &str, sessions: Vec<crate::types::SessionId>) -> WorkItemRow {
    let key = WorkItemKey::classify(key, &[JiraPrefix::parse("ABC").unwrap()]);
    let link = key.to_string().parse::<Link>().unwrap();
    WorkItemRow {
        url: Some(link.url(&targets()).unwrap()),
        key,
        summary: Redacted::of(summary),
        creator: Some(Redacted::of("Ada Example")),
        description: Some(Teaser {
            text: Redacted::of("Tokens: [redacted] and more"),
            cut: true,
        }),
        sessions,
        comments: None,
        unanswered: None,
        kind: crate::model::RowKind::Item,
        waits_on: Vec::new(),
        epic: None,
    }
}

/// A listed ticket with unseen comments wears its count on the work items table and the
/// owners page; the card of the session that owns it carries the newest comment's time in its
/// "seen", and posting that "seen" saves the ticket's mark.
/// Mutation: drop the badge from the table cell or from `owners::subject`, leave the tickets
/// out of the card's form, or drop the ticket marks in `Handle::mark_seen`.
#[tokio::test]
async fn unseen_comments_show_on_the_ticket_and_the_owners_seen_marks_them() {
    use crate::jira::comments::Unseen;
    let newest = at(100);
    let mut row = work_row("ABC-123", "Fix", vec![sid(2)]);
    row.comments = Some(Unseen::New {
        count: std::num::NonZeroUsize::new(2).unwrap(),
        newest,
    });
    let mut model = fixture();
    model.coordination = coordination(false);
    model.work_items = WorkItemsView::Listed {
        read_at: at(240),
        sections: vec![section("In Progress", vec![row])],
        failure: None,
    };
    let site = site("comments", model, Vec::new()).await;
    let badge = r#"<span class="tag comments" title="2 comments you have not seen">💬 2</span>"#;
    assert_has(&page(&site, "/work-items").await, badge);
    assert_has(&page(&site, "/owners").await, badge);
    let marks = format!(r#"<input type="hidden" name="tickets" value="ABC-123={newest}">"#);
    assert_has(&page(&site, "/").await, &marks);

    let id = "00000000-0000-4000-8000-000000000002";
    let body = format!(
        "session={id}&since=2027-01-15T07%3A55%3A00Z&tickets=ABC-123%3D{}",
        newest.to_string().replace(':', "%3A")
    );
    assert_eq!(post(&site, "/seen", &body).await.status, StatusCode::OK);
    let file = std::fs::read_to_string(site.scratch.join("state/seen.json")).unwrap();
    assert!(
        file.contains(&format!("\"tickets\": {{\n    \"ABC-123\": \"{newest}\"")),
        "{file}"
    );
}

/// Every card opens with the pin that toggles it: a pinned card's unpins it and reads as
/// pressed, and the card carries the `pinned` class that colours it; an unpinned card's pins
/// it. The cards have no other Pin or Unpin button.
/// Mutation: post a pinned card's pin to `/pin`, drop the `pinned` class, or keep the old
/// buttons in the card's actions.
#[tokio::test]
async fn the_card_pin_pins_and_unpins_and_shows_which() {
    let mut model = fixture();
    let pinned = sid(2);
    for view in &mut model.sessions {
        if view.session.id == pinned {
            view.pin = Pin::Pinned(crate::model::PinRank::at(0));
        }
    }
    let site = site("pin-toggle", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    let toggle = |route: &str, id: &str, label: &str, pressed: &str| {
        format!(
            r#"<div class="pin-toggle"><form class="act" method="post" action="/{route}" data-kind="change"><input type="hidden" name="session" value="{id}"><button type="submit" title="{label}" aria-label="{label}" aria-pressed="{pressed}">"#
        )
    };
    let two = pinned.as_str();
    assert_has(
        &reply,
        &format!(
            r#"<article class="card pinned" id="{two}">{}"#,
            toggle("unpin", two, "Unpin", "true")
        ),
    );
    let one = sid(1);
    assert_has(&reply, &toggle("pin", one.as_str(), "Pin", "false"));
    assert_eq!(
        reply.body.matches(r#"<div class="pin-toggle">"#).count(),
        reply.body.matches("<article class=\"card").count(),
        "one pin per card"
    );
    assert_lacks(&reply, r#"<button type="submit" class="">Pin</button>"#);
    assert_lacks(&reply, r#"<button type="submit" class="">Unpin</button>"#);
}

/// A pinned dormant session heads the Dormant page under "Pinned", ahead of the newer
/// unpinned one under "Others"; every session row, dormant or live, carries the corner pin,
/// the pinned one's unpinning it.
/// Mutation: drop the "Pinned" section (session 9 falls back below session 8), keep the old
/// "pinned" tag in place of the row's pin, or leave the pin off the live rows.
#[tokio::test]
async fn pinned_dormant_sessions_head_the_dormant_page_and_every_row_has_a_pin() {
    let mut model = fixture();
    for view in &mut model.dormant {
        if view.session.id == sid(9) {
            view.pin = Pin::Pinned(crate::model::PinRank::at(0));
        }
    }
    let site = site("dormant-pins", model, Vec::new()).await;
    let reply = page(&site, "/dormant").await;
    let pinned_heading = reply.body.find("<h2>Pinned (1)</h2>").unwrap();
    let nine = reply.body.find(sid(9).as_str()).unwrap();
    let others_heading = reply.body.find("<h2>Others (1)</h2>").unwrap();
    let eight = reply.body.find(sid(8).as_str()).unwrap();
    assert!(pinned_heading < nine && nine < others_heading && others_heading < eight);
    assert_has(
        &reply,
        &format!(
            r#"<div class="row pinned" data-filter="another one /home/dev/proj-9"><div class="pin-toggle"><form class="act" method="post" action="/unpin" data-kind="change"><input type="hidden" name="session" value="{}">"#,
            sid(9).as_str()
        ),
    );
    assert_lacks(&reply, r#"<span class="tag">pinned</span>"#);

    let sessions = page(&site, "/sessions").await;
    assert_eq!(
        sessions.body.matches(r#"<div class="pin-toggle">"#).count(),
        sessions.body.matches(r#"<div class="row"#).count(),
        "one pin per live row"
    );
    assert_has(&sessions, r#"action="/pin""#);
}

/// A pinned dormant session is drawn on the map with what it owns, its node saying it is
/// pinned; a dormant session that is not pinned is not.
/// Mutation: leave the pinned dormant sessions out of the page's sessions, or drop the
/// node's "pinned".
#[tokio::test]
async fn the_map_draws_a_pinned_dormant_session() {
    use crate::ledger::{Act, Entry, Subject};
    let mut model = fixture();
    model.coordination = coordination(false);
    model.coordination.ledger.apply(Entry {
        at: at(30),
        session: sid(9),
        act: Act::Claim {
            subject: Subject::parse("ABC-9").unwrap(),
            took_from: None,
        },
    });
    model.coordination.ledger.apply(Entry {
        at: at(30),
        session: sid(8),
        act: Act::Claim {
            subject: Subject::parse("ABC-8").unwrap(),
            took_from: None,
        },
    });
    for view in &mut model.dormant {
        if view.session.id == sid(9) {
            view.pin = Pin::Pinned(crate::model::PinRank::at(0));
        }
    }
    let site = site("map-pinned", model, Vec::new()).await;
    let reply = page(&site, "/map").await;
    let nine = sid(9);
    assert_has(&reply, &format!(r##"<a href="#n-s-{}">"##, nine.as_str()));
    // The fixture's map model is empty, so every node's motion reads "state unknown" here; the
    // motion a pinned dormant session gets is pinned in `map::tests`.
    assert_has(
        &reply,
        "Another one</text><text class=\"sub\" x=\"10\" y=\"141\">state unknown · pinned</text>",
    );
    assert_has(&reply, r##"<a href="#n-t-ABC-9">"##);
    assert_lacks(&reply, sid(8).as_str());
    assert_lacks(&reply, "ABC-8");
}

/// A gone session is listed on the Dormant page saying why there is nothing to resume, with
/// no resume command and no terminal button.
/// Mutation: give `Gone` the resume block in `Reach`, or drop its detail.
#[tokio::test]
async fn a_gone_session_says_why_and_offers_no_resume() {
    let mut model = fixture();
    model.dormant.push(view(
        session(
            10,
            "Lost one",
            Activity::Gone {
                background: Some(crate::types::BgState::Done),
            },
        ),
        None,
    ));
    let site = site("gone", model, Vec::new()).await;
    let reply = page(&site, "/dormant").await;
    assert_has(
        &reply,
        r#"<span class="badge gone">Gone</span> <span class="muted">its transcript is gone, so there is nothing to resume; the background session is done and its record stays until <code>claude rm</code> removes it</span>"#,
    );
    let ten = sid(10);
    assert_has(&reply, ten.as_str());
    assert_lacks(&reply, &format!("claude --resume {}", ten.as_str()));
    assert_lacks(
        &reply,
        &format!(
            r#"value="{}"><button type="submit" class="primary">"#,
            ten.as_str()
        ),
    );
}

/// One section of `rows` under `status`.
fn section(status: &str, rows: Vec<WorkItemRow>) -> StatusSection {
    StatusSection::new(Redacted::of(status), rows)
}

fn failure(secs: i64, fault: WorkItemsFault) -> WorkItemsFailure {
    WorkItemsFailure {
        at: at(secs),
        fault,
    }
}

fn not_authenticated() -> WorkItemsFault {
    WorkItemsFault::NotAuthenticated {
        hint: "run `acli jira auth login`",
    }
}

async fn work_items_page(label: &str, view: WorkItemsView) -> Reply {
    let mut model = fixture();
    model.work_items = view;
    let site = site(label, model, Vec::new()).await;
    let reply = page(&site, "/work-items").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply
}

/// A list: a heading with its count per status section, in the model's order, columns, the
/// key as a plain link to the configured host, the description cut with an ellipsis, an item no
/// session owns marked so with the sessions that mention it folded away (badges and a pending
/// marker), and the read's age with a Refresh button.
/// Mutation: rename the Owner column, show the mentions open, or drop the section heading.
#[tokio::test]
async fn the_work_items_table_shows_rows_their_owner_and_the_age_of_the_read() {
    let parser = work_row("ABC-123", "Fix <b>parser</b>", vec![sid(2), sid(8)]);
    let sections = vec![
        section("In Progress", vec![parser]),
        section("To Do", vec![work_row("XYZ-9", "Other project", vec![])]),
    ];
    let reply = work_items_page(
        "wi-list",
        WorkItemsView::Listed {
            read_at: at(240),
            sections,
            failure: None,
        },
    )
    .await;
    let in_progress = reply
        .body
        .find(r#"<h2 class="items-section">In Progress (1)</h2>"#)
        .unwrap();
    let to_do = reply
        .body
        .find(r#"<h2 class="items-section">To Do (1)</h2>"#)
        .unwrap();
    assert!(in_progress < to_do);
    assert!(reply.body.find("ABC-123").unwrap() < to_do);
    assert!(to_do < reply.body.find("XYZ-9").unwrap());
    assert_has(&reply, "Read 4 min ago (2027-01-15 07:56:00)");
    assert_has(
        &reply,
        r#"<form class="act" method="post" action="/work-items/refresh" data-kind="change"><button type="submit" class="">Refresh</button></form>"#,
    );
    assert_has(
        &reply,
        "<th>Key</th><th>Summary</th><th>Creator</th><th>Description</th><th>Owner</th>",
    );
    assert_has(
        &reply,
        r#"<td class="key"><a href="https://example.atlassian.net/browse/ABC-123" target="_blank" rel="noopener noreferrer">ABC-123</a></td><td>Fix &lt;b&gt;parser&lt;/b&gt;</td><td class="creator">Ada Example</td><td class="desc"><div class="clamp" title="Tokens: [redacted] and more…">Tokens: [redacted] and more…</div></td><td class="sessions"><span class="muted">no owner</span><details class="mentions"><summary>mentioned in 2 sessions</summary><div class="chips">"#,
    );
    assert_has(
        &reply,
        r#"<td class="sessions"><span class="muted">no owner</span></td>"#,
    );
    assert_has(
        &reply,
        r#"<a href="https://example.atlassian.net/browse/XYZ-9" target="_blank" rel="noopener noreferrer">XYZ-9</a>"#,
    );
    // Session 2 is idle with a card; session 8 is dormant without one.
    assert_has(
        &reply,
        r#"<a href="/sessions/00000000-0000-4000-8000-000000000002">Parser rework</a> <span class="badge idle">Idle"#,
    );
    assert_has(&reply, r#"<span class="tag">pending</span>"#);
    assert_has(
        &reply,
        r#"<a href="/sessions/00000000-0000-4000-8000-000000000008">Old &lt;b&gt;work&lt;/b&gt;</a> <span class="badge dormant">Dormant"#,
    );
    assert_eq!(
        reply
            .body
            .matches(r#"<span class="tag">pending</span>"#)
            .count(),
        1
    );
}

/// An epic row is tinted and tagged, with its done children out of all as a bar and as text,
/// or what is known instead of a count; an item waiting on open tickets recedes and names each,
/// linked to Jira.
/// Mutation: drop the row classes, show the count as done/remaining, or leave the blockers'
/// links out.
#[tokio::test]
async fn epics_show_their_progress_and_waiting_items_name_their_blockers() {
    use crate::jira::StatusCategory;
    use crate::jira::children::Progress;
    use crate::model::{Blocker, Children, RowKind};
    let categories = [
        StatusCategory::Done,
        StatusCategory::Done,
        StatusCategory::ToDo,
        StatusCategory::InProgress,
        StatusCategory::ToDo,
    ];
    let mut epic = work_row("ABC-10", "An epic", vec![]);
    epic.kind = RowKind::Epic(Children::Counted(Progress::count(categories.iter())));
    let mut empty = work_row("ABC-11", "Empty epic", vec![]);
    empty.kind = RowKind::Epic(Children::Counted(Progress::count(std::iter::empty())));
    let mut unread = work_row("ABC-12", "Unread epic", vec![]);
    unread.kind = RowKind::Epic(Children::Unreadable);
    let mut waiting = work_row("ABC-13", "Later", vec![]);
    waiting.waits_on = ["ABC-14", "ABC-15"]
        .map(|text| {
            let key = crate::types::JiraKey::parse(text).unwrap();
            Blocker {
                url: Some(Link::Jira { key: key.clone() }.url(&targets()).unwrap()),
                key,
            }
        })
        .to_vec();
    let reply = work_items_page(
        "wi-epics",
        WorkItemsView::Listed {
            read_at: at(240),
            sections: vec![section("To Do", vec![epic, empty, unread, waiting])],
            failure: None,
        },
    )
    .await;
    assert_has(
        &reply,
        r#"<tr class="epic"><td class="key"><a href="https://example.atlassian.net/browse/ABC-10" target="_blank" rel="noopener noreferrer">ABC-10</a> <span class="tag epic">epic</span> <span class="epic-progress" title="2 of 5 children done"><meter value="2" max="5"></meter>2/5</span></td>"#,
    );
    assert_has(&reply, r#"<span class="muted">no children</span>"#);
    assert_has(
        &reply,
        r#"<span class="muted" title="see the health page">children could not be read</span>"#,
    );
    assert_has(
        &reply,
        r#"<tr class="waiting"><td class="key"><a href="https://example.atlassian.net/browse/ABC-13" target="_blank" rel="noopener noreferrer">ABC-13</a></td><td>Later<div class="waits-on">waits on <a href="https://example.atlassian.net/browse/ABC-14" target="_blank" rel="noopener noreferrer">ABC-14</a>, <a href="https://example.atlassian.net/browse/ABC-15" target="_blank" rel="noopener noreferrer">ABC-15</a></div></td>"#,
    );
    assert_eq!(reply.body.matches(r#"<tr class="epic">"#).count(), 3);
}

/// An epic's items in the same status sit under its row, `├` before each but the last and `└`
/// before the last; items of an epic not in the status sit under a head naming and linking
/// it; the section counts items, not heads.
/// Mutation: give the last child `├`, drop the `child` class, or count the head as an item.
#[tokio::test]
async fn an_epics_items_are_nested_under_it() {
    use crate::model::{Children, EpicRef, RowKind};
    let epic_ref = |text: &str, summary: &str| {
        let key = crate::types::JiraKey::parse(text).unwrap();
        Some(EpicRef {
            url: Some(Link::Jira { key: key.clone() }.url(&targets()).unwrap()),
            key,
            summary: Redacted::of(summary),
        })
    };
    let mut epic = work_row("ABC-10", "An epic", vec![]);
    epic.kind = RowKind::Epic(Children::NotRead);
    let mut first = work_row("ABC-11", "First part", vec![]);
    first.epic = epic_ref("ABC-10", "An epic");
    let mut second = work_row("ABC-12", "Second part", vec![]);
    second.epic = epic_ref("ABC-10", "An epic");
    let mut elsewhere = work_row("ABC-13", "Part of another", vec![]);
    elsewhere.epic = epic_ref("ABC-20", "Someone's epic");
    let reply = work_items_page(
        "wi-tree",
        WorkItemsView::Listed {
            read_at: at(240),
            sections: vec![section("In Progress", vec![first, epic, elsewhere, second])],
            failure: None,
        },
    )
    .await;
    assert_has(&reply, "In Progress (4)");
    assert_has(
        &reply,
        r#"<tr class="child"><td class="key"><span class="branch" aria-hidden="true">├ </span><a href="https://example.atlassian.net/browse/ABC-11""#,
    );
    assert_has(
        &reply,
        r#"<tr class="child"><td class="key"><span class="branch" aria-hidden="true">└ </span><a href="https://example.atlassian.net/browse/ABC-12""#,
    );
    assert_has(
        &reply,
        r#"<tr class="epic epic-head"><td class="key"><a href="https://example.atlassian.net/browse/ABC-20" target="_blank" rel="noopener noreferrer">ABC-20</a> <span class="tag epic">epic</span></td><td colspan="4">Someone's epic <span class="muted">(not in this status)</span></td></tr><tr class="child"><td class="key"><span class="branch" aria-hidden="true">└ </span><a href="https://example.atlassian.net/browse/ABC-13""#,
    );
    let order: Vec<usize> = ["ABC-10<", "ABC-11<", "ABC-12<", "ABC-20<", "ABC-13<"]
        .iter()
        .map(|needle| reply.body.find(needle).unwrap())
        .collect();
    assert!(
        order.is_sorted(),
        "epic, its items, then the head and its item: {order:?}"
    );
}

/// An item a session owns shows that session alone, not the others that mention it.
/// Mutation: list the mentioning sessions beside the owner, or look the owner up by another key.
#[tokio::test]
async fn an_owned_work_item_shows_only_its_owner() {
    let mut model = fixture();
    model.coordination = coordination(false);
    model.work_items = WorkItemsView::Listed {
        read_at: at(240),
        sections: vec![section(
            "In Progress",
            vec![work_row("ABC-123", "Fix parser", vec![sid(2), sid(8)])],
        )],
        failure: None,
    };
    let site = site("wi-owned", model, Vec::new()).await;
    let reply = page(&site, "/work-items").await;
    assert_has(
        &reply,
        r#"<td class="sessions"><div class="chips"><span class="chip"><a href="/sessions/00000000-0000-4000-8000-000000000002">Parser rework</a>"#,
    );
    assert_lacks(&reply, "00000000-0000-4000-8000-000000000008");
    assert_lacks(&reply, "no owner");
}

/// Each state says something: never read, no list and not logged in, no list and another
/// failure, a stale list beside a failure, and a list that is empty.
/// Mutation, each ran red: drop `once` from the hint line, blank the `reading…` text, drop the
/// stale-list sentence.
#[tokio::test]
async fn every_state_of_the_work_items_says_what_it_is() {
    let never = work_items_page("wi-never", WorkItemsView::NeverRead).await;
    assert_has(&never, "reading…");
    assert_has(&never, "Not read yet");
    assert_lacks(&never, "<table");

    let logged_out = work_items_page(
        "wi-auth",
        WorkItemsView::Failed(failure(30, not_authenticated())),
    )
    .await;
    assert_has(
        &logged_out,
        "acli is not logged in: run `acli jira auth login` once",
    );
    assert_lacks(&logged_out, "<table");

    let failed = work_items_page(
        "wi-failed",
        WorkItemsView::Failed(failure(
            30,
            WorkItemsFault::Failed {
                reason: Redacted::of("acli did not answer within 20 s"),
            },
        )),
    )
    .await;
    assert_has(
        &failed,
        "Reading the work items failed 30 s ago (2027-01-15 07:59:30): acli did not answer within 20 s",
    );

    let stale = work_items_page(
        "wi-stale",
        WorkItemsView::Listed {
            read_at: at(7200),
            sections: vec![section(
                "In Progress",
                vec![work_row("ABC-123", "Still listed", vec![])],
            )],
            failure: Some(failure(30, not_authenticated())),
        },
    )
    .await;
    assert_has(
        &stale,
        "acli is not logged in: run `acli jira auth login` once",
    );
    assert_has(
        &stale,
        "The list below is from the last read that worked, 2 h ago.",
    );
    assert_has(&stale, "Read 2 h ago (2027-01-15 06:00:00)");
    assert_has(&stale, "Still listed");

    let empty = work_items_page(
        "wi-empty",
        WorkItemsView::Listed {
            read_at: at(5),
            sections: vec![],
            failure: None,
        },
    )
    .await;
    assert_has(&empty, "No open work items are assigned to you.");
    assert_lacks(&empty, "<table");
}

/// Refresh asks the poller to read the work items again, and needs the write token.
/// Mutation: make `perform` ignore `RefreshWorkItems` (no second `acli` call, ran red).
#[tokio::test]
async fn refresh_work_items_needs_the_write_token_and_asks_for_another_read() {
    let site = fixture_site("wi-refresh").await;
    assert!(
        acli_calls_reach(&site, 1).await,
        "the first read happens at startup"
    );
    let refused = send(
        &site,
        Method::POST,
        "/work-items/refresh",
        &[HOST, COOKIE, ORIGIN, READ_TOKEN_HEADER, FORM],
        "",
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let accepted = post(&site, "/work-items/refresh", "").await;
    assert_eq!(accepted.status, StatusCode::OK);
    assert_eq!(accepted.body.trim(), r#"{"outcome":"changed"}"#);
    assert!(
        acli_calls_reach(&site, 2).await,
        "a refresh is read at once, not at the end of the ten minutes"
    );
}

/// Without a `[jira]` section the work items page and the health row say Jira is not
/// configured, a refresh is refused as a conflict, and `acli` is never run.
/// Mutation: start the work items task with prefixes from an empty config (an `acli` call
/// appears), or answer the refresh with `Changed` (the status is 200).
#[tokio::test]
async fn without_jira_nothing_is_read_and_the_pages_say_so() {
    let mut model = fixture();
    model.work_items = WorkItemsView::NoJira;
    model.health.work_items = None;
    let site = site_with("no-jira", model, Vec::new(), "").await;
    let items = page(&site, "/work-items").await;
    assert_has(&items, "No Jira is configured");
    assert_lacks(&items, "/work-items/refresh");
    let health = page(&site, "/health").await;
    assert_has(
        &health,
        "<td>Jira work items (acli)</td><td>not configured</td>",
    );
    let refused = post(&site, "/work-items/refresh", "").await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(
        !acli_calls_reach(&site, 1).await,
        "acli was run without a Jira config"
    );
}

fn review_card(pr: &str, title: &str) -> ReviewCard {
    let (repo, number) = pr.split_once('#').unwrap();
    let pr = crate::gh::PullRequestRef::new("example-org", repo, number.parse().unwrap()).unwrap();
    ReviewCard {
        pr_url: Some(
            SafeUrl::parse(&format!(
                "https://github.com/example-org/{repo}/pull/{number}"
            ))
            .unwrap(),
        ),
        pr,
        title: Redacted::of(title),
        author: Redacted::of("bo"),
        head: crate::reviews::github::CommitSha::parse("0123456789aaaaaaaa").unwrap(),
        additions: 9,
        deletions: 0,
        files: 1,
        requester: Redacted::of("Ada Example"),
        request: Redacted::of("Placeholder ask: approve this <b>PR</b>?"),
        requested_at: Some(at(7200)),
        message_url: Some(
            SafeUrl::parse("https://example.slack.com/archives/C0123ABCD/p1700000000000100")
                .unwrap(),
        ),
        verdict: CardVerdict::Look(vec![
            CardFlag {
                by: crate::reviews::classify::FlaggedBy::Rule,
                file: Some(Redacted::of("infra/iam/<roles>.tf")),
                why: Redacted::of("touches access control"),
            },
            CardFlag {
                by: crate::reviews::classify::FlaggedBy::Model,
                file: None,
                why: Redacted::of("reads another stack's state"),
            },
        ]),
    }
}

/// A review card sits after the unseen blocked sessions and before every other card, is
/// counted in the inbox number, links the pull request and the message, and escapes the
/// request's text; a failed read says so where the cards would be.
/// Mutation: render the review section before the blocked cards or after the rest, or leave
/// the cards out of the count (the number stays 3).
#[tokio::test]
async fn review_requests_are_inbox_cards_after_the_blocked_sessions() {
    let mut model = fixture();
    model.reviews = ReviewsView::Listed {
        read_at: at(60),
        cards: vec![review_card("infra#4242", "Grant read on the logs bucket")],
        problems: vec![Redacted::of("Slack channel D0X: the run did not call it")],
        failure: None,
    };
    let site = site("reviews", model, Vec::new()).await;
    let reply = page(&site, "/").await;
    assert_has(&reply, r#"<a href="/" class="active">Inbox (4)</a>"#);
    assert_has(
        &reply,
        r#"<h2><a href="https://github.com/example-org/infra/pull/4242" target="_blank" rel="noopener noreferrer">example-org/infra#4242</a></h2>"#,
    );
    assert_has(&reply, "+9 −0 · 1 file · 0123456");
    assert_has(
        &reply,
        r#"<div class="verdict look"><strong>Look here</strong> · <a href="https://github.com/example-org/infra/pull/4242/files" target="_blank" rel="noopener noreferrer">files changed</a><ul><li><span class="flag-by">rule</span> <code>infra/iam/&lt;roles&gt;.tf</code>: touches access control</li><li><span class="flag-by">model</span> reads another stack's state</li></ul></div>"#,
    );
    assert_has(
        &reply,
        "Placeholder ask: approve this &lt;b&gt;PR&lt;/b&gt;?",
    );
    assert_has(
        &reply,
        r#"Ada Example asked 2 h ago in <a href="https://example.slack.com/archives/C0123ABCD/p1700000000000100""#,
    );
    assert_has(
        &reply,
        "Review requests: Slack channel D0X: the run did not call it",
    );
    assert_has(
        &reply,
        r#"<form class="act" method="post" action="/reviews/approve" data-kind="change"><input type="hidden" name="pr" value="example-org/infra#4242"><input type="hidden" name="head" value="0123456789aaaaaaaa"><button type="submit" class="primary">Approve</button></form>"#,
    );
    let position = |needle: &str| reply.body.find(needle).unwrap();
    let blocked = position(r#"id="00000000-0000-4000-8000-000000000001""#);
    let review = position(r#"<article class="card review">"#);
    let turn = position(r#"id="00000000-0000-4000-8000-000000000002""#);
    assert!(
        blocked < review && review < turn,
        "{blocked} {review} {turn}"
    );

    let mut failed = fixture();
    failed.reviews = ReviewsView::Failed {
        at: at(30),
        reason: Redacted::of("cannot tell who is signed in to gh"),
    };
    let reply = page(
        &site_with("reviews-failed", failed, Vec::new(), JIRA_SECTION).await,
        "/",
    )
    .await;
    assert_has(&reply, "Review requests could not be read 30 s ago");
    assert_lacks(&reply, "card review");
}

/// The approvals page: newest first, the pull request and the message linked, the verdict and
/// its flags, and a Retry for a 👍 that failed; without a `[reviews]` section it says so, and
/// approving refuses as a conflict. A field that is not a pull request or a commit is a 400.
/// Mutation: drop the Retry button, or answer an approval without reviews with 200.
#[tokio::test]
async fn the_approvals_page_shows_each_approval_and_its_reaction() {
    let mut model = fixture();
    model.approvals = Some(vec![ApprovalRow {
        at: at(120),
        pr: "example-org/infra#4242".to_owned(),
        pr_url: Some(SafeUrl::parse("https://github.com/example-org/infra/pull/4242").unwrap()),
        head: "0123456789".to_owned(),
        title: Redacted::of("Grant <b>x</b>"),
        author: Redacted::of("bo"),
        requester: Redacted::of("Ada Example"),
        message_url: None,
        verdict: Redacted::of("look"),
        flags: vec![Redacted::of("infra/iam.tf: touches access control")],
        reaction: Reaction::Failed {
            at: at(100),
            reason: Redacted::of("the run did not call slack_add_reaction"),
        },
    }]);
    let site = site("approvals", model, Vec::new()).await;
    let reply = page(&site, "/approvals").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_has(
        &reply,
        r#"<a href="/approvals" class="active">Approvals</a>"#,
    );
    assert_has(&reply, "Grant &lt;b&gt;x&lt;/b&gt; · by bo");
    assert_has(&reply, "infra/iam.tf: touches access control");
    assert_has(&reply, "failed: the run did not call slack_add_reaction");
    assert_has(
        &reply,
        r#"<form class="act" method="post" action="/reviews/react" data-kind="change"><input type="hidden" name="pr" value="example-org/infra#4242"><input type="hidden" name="head" value="0123456789"><button type="submit" class="">Retry</button></form>"#,
    );

    let off = page(&fixture_site("approvals-off").await, "/approvals").await;
    assert_has(&off, "No [reviews] section is configured");
    let refused = post(
        &site,
        "/reviews/approve",
        "pr=example-org%2Finfra%234242&head=0123456789",
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_has(&refused, "no [reviews] section is configured");
    for bad in [
        "pr=nope&head=0123456789",
        "pr=example-org%2Finfra%234242&head=xyz",
    ] {
        assert_eq!(
            post(&site, "/reviews/approve", bad).await.status,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
}

/// Waits until the poller has asked `acli` `n` times, up to ten seconds.
async fn acli_calls_reach(site: &Site, n: usize) -> bool {
    for _ in 0..400 {
        let acli = site
            .poller_runner
            .calls()
            .iter()
            .filter(|call| call.program == "acli")
            .count();
        if acli >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

// --- compact pull request chips, session names, the work items table ---

/// A pull request's chip is one inline unit: name, state badge, checks and review glyphs, the
/// reading's age only when it is stale or failed (and then the chip is `stale`), and a `↻`
/// button inside it. The words, which used to fill a line, are its `title`. Expected chips are
/// written out from the spec: `good_reading` is open, approved, checks passing, read 3 min
/// ago; its stale twin failed to refresh 20 s ago.
/// Mutation, each ran red on its row: show the age of a fresh reading (row 1), drop the
/// `stale` class (row 2), drop the failed reading's age (row 3), put a glyph on the unread
/// chip (row 4), badge a not-found answer `unavailable` (row 5).
#[tokio::test]
async fn a_pull_request_chip_is_compact_and_keeps_the_words_in_its_title() {
    let site = fixture_site("pr-chip").await;
    let reply = page(&site, &format!("/sessions/{}", sid(2))).await;
    let link = |n: u32| format!("https://github.com/example-org/example-repo/pull/{n}");
    let chip = |n: u32, class: &str, title: &str, inside: &str| {
        format!(
            concat!(
                r#"<span class="{class}" title="{title}"><a href="{url}" target="_blank" rel="noopener noreferrer">example-org/example-repo#{n}</a>{inside} "#,
                r#"<form class="act" method="post" action="/refresh-link" data-kind="change">"#,
                r#"<input type="hidden" name="link" value="{url}">"#,
                r#"<button class="icon" type="submit" title="Read again" aria-label="Refresh">↻</button></form></span>"#,
            ),
            class = class,
            title = title,
            url = link(n),
            n = n,
            inside = inside,
        )
    };
    let checks = r#" <span class="glyph checks" aria-label="checks passing">✓</span>"#;
    let review = r#" <span class="glyph review" aria-label="approved">✓</span>"#;
    for (row, expected) in [
        chip(
            42,
            "chip pr",
            "open · approved · checks passing · read 3 min ago",
            &format!(r#" <span class="badge pr-open">open</span>{checks}{review}"#),
        ),
        chip(
            43,
            "chip pr stale",
            "open · approved · checks passing · read 3 min ago · stale: refresh failed 20 s ago: gh did not answer within 10s",
            &format!(
                r#" <span class="badge pr-open">open</span>{checks}{review} <span class="age">3 min old</span>"#
            ),
        ),
        chip(
            44,
            "chip pr stale",
            "unavailable: gh has no usable credentials; run `gh auth login` (20 s ago)",
            r#" <span class="badge pr-failed">unavailable</span> <span class="age">20 s ago</span>"#,
        ),
        chip(
            45,
            "chip pr",
            "not read yet",
            r#" <span class="badge pr-unread">not read</span>"#,
        ),
        chip(
            46,
            "chip pr",
            "GitHub has no such pull request, or this account cannot see it · read 3 min ago",
            r#" <span class="badge pr-not-found">not found</span>"#,
        ),
    ]
    .iter()
    .enumerate()
    {
        assert!(
            reply.body.contains(expected.as_str()),
            "row {}: missing {expected}",
            row + 1
        );
    }
    assert_lacks(&reply, "pr-state");
}

/// A session with no title is named by the start of its first prompt (60 characters, redacted)
/// with its short id muted after it; with no prompt either, by the id alone; a title wins over
/// a prompt.
/// Mutation, each ran red: cut at 61 characters; take the latest prompt rather than the first;
/// drop the muted id from the link, and from the plain name on the session's page; prefer the
/// prompt over a title. The secret row holds already because the held prompt is `Redacted`.
#[tokio::test]
async fn an_untitled_session_is_named_by_its_first_prompt_then_its_id() {
    let mut model = fixture();
    let untitled = |n: u8| {
        let mut untitled = view(session(n, "", Activity::Running { since: at(5) }), None);
        untitled.title = None;
        untitled
    };
    model.sessions.extend([untitled(10), untitled(11)]);
    let held = |text: &str| HeldPrompt {
        at: at(100),
        text: Redacted::of(text),
    };
    model.prompts.insert(
        sid(10),
        Arc::from(vec![
            held(&format!(
                "Please refactor the tokenizer so that it handles unicode escapes correctly {SECRET}"
            )),
            held("a later prompt"),
        ]),
    );
    // Session 2 has a title and a prompt: the title is its name.
    model
        .prompts
        .insert(sid(2), Arc::from(vec![held("never shown as a name")]));
    let site = site("untitled", model, Vec::new()).await;
    let reply = page(&site, "/sessions").await;
    assert_has(
        &reply,
        r#"<a href="/sessions/00000000-0000-4000-8000-00000000000a">Please refactor the tokenizer so that it handles unicode esc…</a> <span class="muted">00000000</span>"#,
    );
    assert_has(
        &reply,
        r#"<a href="/sessions/00000000-0000-4000-8000-00000000000b">00000000</a></strong> <span class="badge"#,
    );
    assert_has(&reply, r#">Parser rework</a></strong> <span class="badge"#);
    // The session's own page names it the same way, without a link.
    let own = page(&site, &format!("/sessions/{}", sid(10))).await;
    assert_has(
        &own,
        r#"<h2>Please refactor the tokenizer so that it handles unicode esc… <span class="muted">00000000</span></h2>"#,
    );
    assert_lacks(&reply, "never shown as a name</a>");
    assert_lacks(&reply, "a later prompt</a>");
    assert_lacks(&reply, SECRET);
}

/// The work items table is page-level: it is in no `.body` or `.panel`, whose markdown-table
/// rules turn a table into a scrolling block, and the stylesheet applies `display: block` to
/// no table outside those scopes (and a map detail card's, which stacks its writes). Its
/// columns follow the spec's rules.
/// Mutation, each ran red: drop the `.body ` prefix from the markdown table rule; change the
/// line clamp; drop `nowrap` from the key and creator cells. Wrapping the table in
/// `div.body` was not run.
#[tokio::test]
async fn the_work_items_table_is_not_in_a_body_scope_and_its_columns_hold() {
    let reply = work_items_page(
        "wi-css",
        WorkItemsView::Listed {
            read_at: at(240),
            sections: vec![section(
                "In Progress",
                vec![work_row("ABC-123", "Fix", vec![sid(2)])],
            )],
            failure: None,
        },
    )
    .await;
    assert_has(&reply, r#"<table class="items">"#);
    assert_lacks(&reply, r#"class="body""#);
    assert_lacks(&reply, r#"class="panel""#);

    let site = fixture_site("wi-css-sheet").await;
    let sheet = page(&site, "/static/style.css").await;
    for rule in sheet.body.split('}') {
        let Some((selectors, declarations)) = rule.split_once('{') else {
            continue;
        };
        if declarations.contains("display: block") {
            for selector in selectors.split(',').map(str::trim) {
                assert!(
                    !selector.contains("table")
                        || selector.starts_with(".body ")
                        || selector.starts_with(".panel ")
                        || selector.starts_with(".detail "),
                    "a table outside .body, .panel and .detail becomes a block: {selector}"
                );
            }
        }
    }
    for needle in [
        "table.items { table-layout: auto; width: 100%; }",
        "table.items td.key, table.items td.creator { white-space: nowrap; }",
        "table.items td.desc { min-width: 28ch; max-width: 60ch;",
        "-webkit-line-clamp: 3;",
        "table.items td.sessions { max-width: 45%; }",
        "table.items .chips { display: inline-flex; flex-wrap: wrap; gap: 4px; }",
    ] {
        assert!(sheet.body.contains(needle), "missing {needle}");
    }
}

/// `--open` launches `xdg-open` with the URL as its one argument, and a missing opener does
/// not stop the server (`open` returns; the warning is the only trace).
/// Mutation: pass the URL as a `--` prefixed or second argument, or name another program.
#[test]
fn open_hands_the_url_to_xdg_open_and_survives_its_absence() {
    let url = "http://127.0.0.1:7357/?t=abc";
    let runner = FakeRunner::new([Ok(Output {
        exit: Exit::Success,
        stdout: Vec::new(),
        stderr: String::new(),
    })]);
    super::open(&runner, url);
    assert_eq!(runner.lines(), ["xdg-open http://127.0.0.1:7357/?t=abc"]);

    let missing = FakeRunner::new([Err(crate::util::runner::RunError::NotFound)]);
    super::open(&missing, url);
    assert_eq!(missing.lines(), ["xdg-open http://127.0.0.1:7357/?t=abc"]);
}

fn saved(n: u8, title: &str) -> crate::saves::SavedSession {
    crate::saves::SavedSession {
        id: sid(n),
        title: Some(title.to_owned()),
        gist: Some("Was <b>migrating</b>".to_owned()),
        cwd: Some(PathBuf::from(format!("/home/dev/proj-{n}"))),
        last_activity: Some(at(600)),
        resume: crate::saves::Resume::Command {
            text: format!("cd /home/dev/proj-{n} && claude --resume {}", sid(n)),
            warnings: vec!["a flag was dropped".to_owned()],
        },
    }
}

/// The Sessions page lists the automatic save first, then the kept ones, each linking to its
/// page, with "Save now" and a delete on each kept save but not the automatic one. A save's
/// page marks a session running again and gives the others their saved command to copy.
/// Mutation: offer a delete on the automatic save, show the command of a running session, or
/// escape nothing in the gist.
#[tokio::test]
async fn saves_are_listed_and_a_save_resumes_its_sessions() {
    use crate::saves::{Save, SaveId, SaveKind, SavesView};
    let save = |kind, sessions| Save {
        kind,
        at: at(900),
        boot: None,
        sessions,
    };
    let mut model = fixture();
    model.saves = SavesView {
        auto: Some(save(
            SaveKind::Auto,
            vec![saved(1, "Plan"), saved(77, "Gone one")],
        )),
        kept: vec![
            (
                SaveId::Kept(2000),
                save(SaveKind::Manual, vec![saved(1, "Plan")]),
            ),
            (
                SaveId::Kept(1000),
                save(SaveKind::BeforeReboot, vec![saved(2, "Parser")]),
            ),
        ],
        problems: vec![],
        auto_failure: None,
    };
    model.coordination = coordination(false);
    let site = site("saves", model, Vec::new()).await;
    let sessions = page(&site, "/sessions").await;
    assert_has(
        &sessions,
        r#"<form class="act" method="post" action="/save-sessions" data-kind="change"><button type="submit" class="">Save now</button></form>"#,
    );
    let auto = sessions.body.find(r#"<a href="/saves/auto">"#).unwrap();
    let manual = sessions.body.find(r#"<a href="/saves/2000">"#).unwrap();
    let reboot = sessions.body.find(r#"<a href="/saves/1000">"#).unwrap();
    assert!(auto < manual && manual < reboot);
    assert_has(
        &sessions,
        r#"<span class="tag before-reboot">before reboot</span>"#,
    );
    assert_has(
        &sessions,
        r#"<input type="hidden" name="save" value="2000">"#,
    );
    assert_has(
        &sessions,
        r#"<input type="hidden" name="save" value="1000">"#,
    );
    assert_lacks(&sessions, r#"name="save" value="auto""#);

    let restore = page(&site, "/saves/auto").await;
    assert_eq!(restore.status, StatusCode::OK);
    assert_has(&restore, "2 sessions, 1 running again.");
    assert_has(&restore, r#"<span class="tag back">running again</span>"#);
    assert_has(&restore, "Was &lt;b&gt;migrating&lt;/b&gt;");
    let gone = format!(
        "<pre>cd /home/dev/proj-77 &amp;&amp; claude --resume {}</pre><button class=\"copy\" type=\"button\">Copy</button>",
        sid(77)
    );
    assert_has(&restore, &gone);
    assert_has(&restore, "<li>a flag was dropped</li>");
    assert_lacks(&restore, "cd /home/dev/proj-1 &amp;&amp;");

    // What a saved session owns is the ledger's now, as its card says it.
    let kept = page(&site, "/saves/1000").await;
    assert_has(&kept, r#"<p class="coord">owns "#);
    assert_has(&kept, "ABC-123");
    assert_lacks(&restore, r#"<p class="coord">owns "#);

    assert_eq!(
        page(&site, "/saves/3000").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        page(&site, "/saves/x").await.status,
        StatusCode::BAD_REQUEST
    );
}

/// "Save now" writes a kept save of the running sessions and a delete removes its file; the
/// automatic save is refused, as is a save before the sessions are all read.
/// Mutation: save the dormant sessions too, or let a delete of `auto` through.
#[tokio::test]
async fn save_now_keeps_the_running_sessions_and_a_delete_removes_them() {
    let live_site = fixture_site("save-now").await;
    let reply = post(&live_site, "/save-sessions", "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let dir = live_site.scratch.join("state/saves");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().is_some_and(|name| name != "auto.json"))
        .collect();
    let [file] = files.as_slice() else {
        panic!("one kept save: {files:?}");
    };
    let save: crate::saves::Save =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let live = fixture()
        .sessions
        .iter()
        .filter(|view| view.session.activity.kind().is_live())
        .count();
    assert_eq!(save.sessions.len(), live);
    assert!(live > 0);

    let id = file.file_stem().unwrap().to_str().unwrap().to_owned();
    let reply = post(&live_site, "/delete-save", &format!("save={id}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(!file.exists());
    assert_eq!(
        post(&live_site, "/delete-save", "save=auto").await.status,
        StatusCode::CONFLICT
    );

    let mut reading = fixture();
    reading.health.transcripts.progress = ScanProgress::InProgress {
        scanned: 1,
        total: 2,
    };
    let early = site("save-early", reading, Vec::new()).await;
    assert_eq!(
        post(&early, "/save-sessions", "").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
