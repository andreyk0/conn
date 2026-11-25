//! The server-rendered site on 127.0.0.1.
//!
//! **The site reads and shows. Nothing here writes to a session.** The only changes it makes
//! are the ones [`action`] lists: the seen and pinned marks, a request to read a pull request
//! or the Jira work items again, raising a terminal window the compositor already shows, and
//! the few that reach out as the user, each a click on its own button (approving a review,
//! asking PAM for a grant). The only file a request can name is a local `.md` file under the
//! configured roots, read by [`file`].
//!
//! All HTML is built by `maud`, which escapes every interpolation. `maud::PreEscaped` is
//! banned by `clippy.toml`; an external `href` is a [`crate::types::SafeUrl`], a local one a
//! [`route::Page`]. Third-party text reaches a template only as
//! [`crate::util::redact::Redacted`].
//!
//! Access is loopback only, with two tokens generated at launch, one to read and one to write:
//! see [`auth`]. The one line the process prints is the sign-in URL, which carries the read
//! token.
//!
//! **The URL scheme, the cookie, the `data-write-token` attribute and the `X-Conn-Token` header
//! are a contract with the browser.** Pages: `/`, `/sessions`, `/sessions/<id>`, `/dormant`,
//! `/map`, `/owners`, `/work-items`, `/followups`, `/pam`, `/search?q=<text>` (a GET, so a search can be bookmarked;
//! `q` is at most 200 characters), `/health`, `/file?path=<absolute .md path>`; files:
//! `/static/style.css`, `/static/app.js`;
//! changes, all POST with a form body: the paths of [`route::PostRoute`], each carrying the
//! write token read from the page's `data-write-token` in the `X-Conn-Token` header. The
//! sign-in is `GET /?t=<read token>`, answered with the cookie
//! `conn=<read token>; HttpOnly; SameSite=Strict; Path=/` and a redirect to `/`.

pub mod action;
pub mod auth;
pub mod file;
pub mod route;
mod view;

#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, SET_COOKIE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use jiff::tz::TimeZone;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use self::action::{Action, ActionError, Effects, Fields, perform};
use self::auth::{Credential, Intent, Names, Read, Token, Tokens, Verdict, admit, session_cookie};
use self::route::{Asset, PostRoute, Route, RouteError};
use crate::config::Config;
use crate::links::{Extractor, LinksError};
use crate::model::Model;
use crate::saves::SavesError;
use crate::state::{Handle, SaveNowError};
use crate::types::{Instant, LinkTargets, LocalFile};
use crate::util::runner::CommandRunner;
use view::{Ctx, NotFound};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("cannot listen on 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error("the server stopped: {0}")]
    Serve(std::io::Error),
    #[error(transparent)]
    Token(#[from] auth::TokenError),
    #[error(transparent)]
    Links(#[from] LinksError),
}

/// A request body is a form of a few fields; 64 KiB is orders of magnitude more, and bounds
/// what one request can make the server hold.
const BODY_BYTES_MAX: usize = 64 * 1024;

/// What the site needs, supplied by whoever starts it.
pub struct Parts {
    /// The only way to change what the human owns.
    pub handle: Handle,
    /// The model the pages read; the handle's own in a running tool.
    pub model: Arc<RwLock<Model>>,
    /// How the compositor is asked.
    pub runner: Arc<dyn CommandRunner>,
    pub tokens: Tokens,
    pub port: u16,
    pub extractor: Extractor,
    pub targets: LinkTargets,
    /// The GitHub owner a pull request chip leaves out of its name.
    pub default_owner: Option<String>,
    /// The viewer's time zone, for wall-clock times.
    pub zone: TimeZone,
    pub clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

/// The shared state of the site.
#[derive(Clone)]
pub struct App {
    inner: Arc<Parts>,
    names: Arc<Names>,
}

impl App {
    pub fn new(parts: Parts) -> Self {
        Self {
            names: Arc::new(Names::on_port(parts.port)),
            inner: Arc::new(parts),
        }
    }
}

/// The routes: one entry point decides admission first, then what the request is for, so no
/// path is reachable without passing both.
pub fn router(app: App) -> Router {
    Router::new()
        .fallback(entry)
        .layer(DefaultBodyLimit::max(BODY_BYTES_MAX))
        .with_state(app)
}

/// What happens to the sign-in URL once the server listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    Print,
    /// Printed, and handed to the desktop's default browser.
    PrintAndOpen,
}

/// The desktop's opener: it hands an `http` URL to the default browser.
const OPENER: &str = "xdg-open";

/// Starts the poller's handle serving on `127.0.0.1:port` until interrupted.
///
/// Binds first, then prints the one line of output: the sign-in URL; then opens it, if asked.
pub async fn serve(
    port: std::num::NonZeroU16,
    sign_in: SignIn,
    handle: Handle,
    config: &Config,
    runner: Arc<dyn CommandRunner>,
) -> Result<(), WebError> {
    let tokens = Tokens::generate()?;
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port.get())))
        .await
        .map_err(|source| WebError::Bind {
            port: port.get(),
            source,
        })?;
    let url = format!("http://127.0.0.1:{port}/?t={}", tokens.read.as_str());
    let model = Arc::clone(handle.model());
    let opener = Arc::clone(&runner);
    let app = App::new(Parts {
        handle,
        model,
        runner,
        tokens,
        port: port.get(),
        extractor: Extractor::from_config(config)?,
        targets: config.link_targets(),
        default_owner: config.github.default_owner.clone(),
        zone: TimeZone::system(),
        clock: Arc::new(Instant::now),
    });
    announce(&url);
    match sign_in {
        SignIn::Print => {}
        SignIn::PrintAndOpen => open(opener.as_ref(), &url),
    }
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::warn!(%error, "cannot wait for an interrupt; serving until killed");
                std::future::pending::<()>().await;
            }
        })
        .await
        .map_err(WebError::Serve)
}

/// The sign-in URL, on stdout, which carries nothing else: the person opens it, and the
/// process says nothing more there.
#[allow(
    clippy::print_stdout,
    reason = "the sign-in URL is the command's one line of output"
)]
fn announce(url: &str) {
    println!("{url}");
}

/// Hands the sign-in URL to the default browser. The server serves either way and the URL is
/// already printed, so a failure to start the opener is a warning.
fn open(runner: &dyn CommandRunner, url: &str) {
    if let Err(error) = runner.launch(OPENER, &[url]) {
        tracing::warn!(%error, "cannot run {OPENER}; open the printed URL by hand");
    }
}

async fn entry(
    State(app): State<App>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    harden(respond(&app, &method, &uri, &headers, &body).await)
}

async fn respond(
    app: &App,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let url_token = (uri.path() == "/")
        .then(|| query_value(uri.query(), "t"))
        .flatten();
    let verdict = admit(
        &app.names,
        &app.inner.tokens,
        Intent::of(method),
        url_token.as_deref(),
        headers,
    );
    match verdict {
        Verdict::Refused(refusal) => plain(refusal.status(), refusal.message()),
        Verdict::Admitted(Credential::UrlToken) => sign_in(&app.inner.tokens.read),
        Verdict::Admitted(Credential::Cookie) => {
            match Route::resolve(method, uri.path(), uri.query()) {
                Ok(Route::Page(page)) => render_page(app, &page).await,
                Ok(Route::Asset(asset)) => serve_asset(asset),
                Ok(Route::Post(route)) => change(app, route, headers, body).await,
                Ok(Route::File(file)) => show_file(app, file).await,
                Err(error) => route_error(&error),
            }
        }
    }
}

fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    form_urlencoded_pairs(query?).find_map(|(key, value)| (key == name).then_some(value))
}

fn form_urlencoded_pairs(query: &str) -> impl Iterator<Item = (String, String)> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(query)
        .unwrap_or_default()
        .into_iter()
}

fn sign_in(token: &Token<Read>) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static("/"));
    if let Ok(cookie) = HeaderValue::from_str(&session_cookie(token)) {
        response.headers_mut().insert(SET_COOKIE, cookie);
    }
    response
}

fn ctx<'a>(app: &'a App, model: &'a Model) -> Ctx<'a> {
    Ctx::new(
        model,
        (app.inner.clock)(),
        &app.inner.zone,
        &app.inner.extractor,
        &app.inner.targets,
        app.inner.default_owner.as_deref(),
        &app.inner.tokens.write,
    )
}

async fn render_page(app: &App, page: &route::Page) -> Response {
    app.inner.handle.page_viewed();
    if *page == route::Page::Pam {
        app.inner.handle.pam_viewed();
    }
    let model = app.inner.model.read().await;
    let ctx = ctx(app, &model);
    match view::render(&ctx, page) {
        Ok(markup) => markup.into_response(),
        Err(error @ (NotFound::Session(_) | NotFound::Save(_))) => {
            plain(StatusCode::NOT_FOUND, &error.to_string())
        }
    }
}

/// A local markdown file's page, or a plain answer saying why it is not shown. The roots are
/// the extractor's, so what a page links and what the server shows are one set.
async fn show_file(app: &App, file: LocalFile) -> Response {
    let roots = app.inner.extractor.roots().clone();
    let wanted = file.clone();
    let read = tokio::task::spawn_blocking(move || file::read(&roots, &wanted)).await;
    let text = match read {
        Ok(Ok(text)) => text,
        Ok(Err(error)) => {
            tracing::info!(%error, "a local file was not shown");
            return plain(file_status(&error), &error.to_string());
        }
        Err(error) => {
            tracing::warn!(%error, "the file read stopped");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "the file read stopped");
        }
    };
    let model = app.inner.model.read().await;
    view::file(&ctx(app, &model), &file, &text).into_response()
}

const fn file_status(error: &file::FileError) -> StatusCode {
    use file::FileError;
    match error {
        FileError::OutsideRoots(_) | FileError::NotMarkdown { .. } => StatusCode::FORBIDDEN,
        FileError::Missing(_) => StatusCode::NOT_FOUND,
        FileError::NotAFile(_) | FileError::TooLarge { .. } | FileError::NotText(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        FileError::Unreadable { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn serve_asset(asset: Asset) -> Response {
    let (kind, text) = match asset {
        Asset::Style => ("text/css; charset=utf-8", include_str!("style.css")),
        Asset::Script => ("text/javascript; charset=utf-8", include_str!("app.js")),
    };
    ([(CONTENT_TYPE, kind)], text).into_response()
}

/// Parses the body into an action and performs it; a body that is not one is a 400 that names
/// the field.
async fn change(app: &App, route: PostRoute, headers: &HeaderMap, body: &Bytes) -> Response {
    let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let fields = match Fields::parse(content_type, body) {
        Ok(fields) => fields,
        Err(error @ action::BodyError::MediaType) => {
            return plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, &error.to_string());
        }
        Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let action = match Action::parse(route, &fields, &app.inner.extractor) {
        Ok(action) => action,
        Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let effects = Effects {
        handle: &app.inner.handle,
        model: &app.inner.model,
        runner: &app.inner.runner,
    };
    match perform(&effects, action).await {
        Ok(done) => axum::Json(done.to_json()).into_response(),
        Err(error) => {
            tracing::warn!(%error, ?route, "a change was refused");
            plain(action_status(&error), &error.to_string())
        }
    }
}

fn action_status(error: &ActionError) -> StatusCode {
    use crate::reviews::ApproveError;
    use crate::state::ReviewActionError;
    use crate::state::seen::SeenError;
    match error {
        ActionError::NoJira(_)
        | ActionError::Review(
            ReviewActionError::NotConfigured
            | ReviewActionError::Approve(
                ApproveError::NotInQueue(_)
                | ApproveError::Moved { .. }
                | ApproveError::NoLongerWaits(_),
            ),
        ) => StatusCode::CONFLICT,
        ActionError::UnknownSession(_)
        | ActionError::Review(ReviewActionError::NotApproved { .. }) => StatusCode::NOT_FOUND,
        ActionError::Review(ReviewActionError::Approve(ApproveError::GitHub { .. })) => {
            StatusCode::BAD_GATEWAY
        }
        ActionError::Seen(SeenError::NotRead(_))
        | ActionError::SaveNow(SaveNowError::StillReading) => StatusCode::SERVICE_UNAVAILABLE,
        ActionError::Seen(
            SeenError::Unreadable { .. }
            | SeenError::Unwritable { .. }
            | SeenError::Malformed { .. },
        )
        | ActionError::Review(ReviewActionError::Unrecorded(_)) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        ActionError::SaveNow(SaveNowError::Saves(error)) | ActionError::Saves(error) => {
            saves_status(error)
        }
        ActionError::Pam(error) => pam_status(error),
    }
}

const fn pam_status(error: &crate::pam::RequestError) -> StatusCode {
    use crate::pam::RequestError;
    match error {
        RequestError::NotConfigured
        | RequestError::NotRequestable(_)
        | RequestError::AlreadyLive { .. } => StatusCode::CONFLICT,
        RequestError::Length { .. } | RequestError::JustificationRequired(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        RequestError::Check { .. } | RequestError::Create { .. } => StatusCode::BAD_GATEWAY,
    }
}

const fn saves_status(error: &SavesError) -> StatusCode {
    match error {
        SavesError::NotFound(_) => StatusCode::NOT_FOUND,
        SavesError::AutoIsNotDeleted | SavesError::NothingRunning => StatusCode::CONFLICT,
        SavesError::Write { .. } | SavesError::Delete { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn route_error(error: &RouteError) -> Response {
    let status = match error {
        RouteError::NotFound => StatusCode::NOT_FOUND,
        RouteError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        RouteError::BadParameter { .. } => StatusCode::BAD_REQUEST,
    };
    plain(status, &error.to_string())
}

/// A one-line plain-text answer.
fn plain(status: StatusCode, line: &str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{line}\n"),
    )
        .into_response()
}

/// The headers every response carries: nothing is cached, nothing is framed or sniffed, and
/// the page may load only its own two files and talk only to its own origin.
fn harden(mut response: Response) -> Response {
    const CSP: &str = "default-src 'none'; style-src 'self'; script-src 'self'; \
        connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}
