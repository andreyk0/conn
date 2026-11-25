//! The URL scheme: every path the server answers, as a type.
//!
//! A path is parsed once into a [`Route`] (parameters included, through their constructors),
//! and the same type renders the `href`s the pages link with, so the scheme is written down in
//! one place and a page cannot link to a path nothing serves.

use std::fmt;

use axum::http::Method;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::model::{Query, QueryError};
use crate::saves::SaveId;
use crate::types::{LocalFile, SessionId};

/// A page the server renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Inbox,
    /// What is due and who should act on it.
    FollowUps,
    Sessions,
    Dormant,
    /// The active sessions and what ties them, as a dependency graph.
    Map,
    /// Who owns which ticket, rollout and topic, the rollouts, and what sessions wait on.
    Owners,
    Session(SessionId),
    /// One saved set of sessions, with what resumes each.
    Save(SaveId),
    WorkItems,
    /// The journal of approvals made from review cards.
    Approvals,
    /// The search page; without a query it only says what is searched.
    Search(Option<Query>),
    /// The PAM entitlements the user may request, and their grants.
    Pam,
    Health,
}

/// A file the pages load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    Style,
    Script,
}

/// A change the human asks for; each is one [`PostRoute`] and one `Action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostRoute {
    Seen,
    Pin,
    Unpin,
    Focus,
    RefreshLink,
    RefreshWorkItems,
    ApproveReview,
    RetryReaction,
    SaveSessions,
    DeleteSave,
    RequestGrant,
    RefreshPam,
}

impl PostRoute {
    pub const ALL: [Self; 12] = [
        Self::Seen,
        Self::Pin,
        Self::Unpin,
        Self::Focus,
        Self::RefreshLink,
        Self::RefreshWorkItems,
        Self::ApproveReview,
        Self::RetryReaction,
        Self::SaveSessions,
        Self::DeleteSave,
        Self::RequestGrant,
        Self::RefreshPam,
    ];

    pub const fn path(self) -> &'static str {
        match self {
            Self::Seen => "/seen",
            Self::Pin => "/pin",
            Self::Unpin => "/unpin",
            Self::Focus => "/focus",
            Self::RefreshLink => "/refresh-link",
            Self::RefreshWorkItems => "/work-items/refresh",
            Self::ApproveReview => "/reviews/approve",
            Self::RetryReaction => "/reviews/react",
            Self::SaveSessions => "/save-sessions",
            Self::DeleteSave => "/delete-save",
            Self::RequestGrant => "/pam/request",
            Self::RefreshPam => "/pam/refresh",
        }
    }
}

/// What a request is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Page(Page),
    Asset(Asset),
    Post(PostRoute),
    /// A local markdown file, rendered; whether it may be shown is decided when it is read.
    File(LocalFile),
}

/// Why a path and method are not a [`Route`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("no such page")]
    NotFound,
    #[error("that path does not answer this method")]
    MethodNotAllowed,
    #[error("{field} in the path: {problem}")]
    BadParameter {
        field: &'static str,
        problem: String,
    },
}

const STYLE_PATH: &str = "/static/style.css";
const SCRIPT_PATH: &str = "/static/app.js";
const FILE_PATH: &str = "/file";

/// The `href` of a local file's page: its path, percent-encoded, in `path`.
pub fn file_href(file: &LocalFile) -> String {
    format!(
        "{FILE_PATH}?path={}",
        utf8_percent_encode(&file.to_string(), NON_ALPHANUMERIC)
    )
}

impl Route {
    /// The route for a request's method, path and query string (without the `?`).
    pub fn resolve(method: &Method, path: &str, query: Option<&str>) -> Result<Self, RouteError> {
        if *method == Method::POST {
            return PostRoute::ALL
                .into_iter()
                .find(|route| route.path() == path)
                .map(Self::Post)
                .ok_or_else(|| Self::missing(path));
        }
        let found = Self::resolve_get(path, query)?;
        if *method == Method::GET {
            Ok(found)
        } else {
            Err(RouteError::MethodNotAllowed)
        }
    }

    /// A path no method of ours serves is `NotFound`; one that a POST serves is
    /// `MethodNotAllowed` for anything else.
    fn missing(path: &str) -> RouteError {
        if Self::resolve_get(path, None).is_ok() {
            RouteError::MethodNotAllowed
        } else {
            RouteError::NotFound
        }
    }

    fn resolve_get(path: &str, query: Option<&str>) -> Result<Self, RouteError> {
        let page = |page| Ok(Self::Page(page));
        match path {
            "/" => page(Page::Inbox),
            "/followups" => page(Page::FollowUps),
            "/sessions" => page(Page::Sessions),
            "/dormant" => page(Page::Dormant),
            "/map" => page(Page::Map),
            "/owners" => page(Page::Owners),
            "/health" => page(Page::Health),
            "/work-items" => page(Page::WorkItems),
            "/approvals" => page(Page::Approvals),
            "/pam" => page(Page::Pam),
            "/search" => page(Page::Search(search_query(query)?)),
            STYLE_PATH => Ok(Self::Asset(Asset::Style)),
            SCRIPT_PATH => Ok(Self::Asset(Asset::Script)),
            FILE_PATH => Ok(Self::File(file_param(query)?)),
            _ => {
                if let Some(raw) = path.strip_prefix("/sessions/") {
                    let id = SessionId::parse(raw).map_err(|e| RouteError::BadParameter {
                        field: "session",
                        problem: e.to_string(),
                    })?;
                    page(Page::Session(id))
                } else if let Some(raw) = path.strip_prefix("/saves/") {
                    let id = SaveId::parse(raw).map_err(|e| RouteError::BadParameter {
                        field: "save",
                        problem: e.to_string(),
                    })?;
                    page(Page::Save(id))
                } else if PostRoute::ALL.iter().any(|route| route.path() == path) {
                    Err(RouteError::MethodNotAllowed)
                } else {
                    Err(RouteError::NotFound)
                }
            }
        }
    }
}

/// The `q` of a search request: absent or blank is no query, one over the bound is refused
/// naming the field. The first `q` wins.
fn search_query(query: Option<&str>) -> Result<Option<Query>, RouteError> {
    let bad = |problem: String| RouteError::BadParameter {
        field: "q",
        problem,
    };
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(query.unwrap_or_default()).map_err(|e| bad(e.to_string()))?;
    let Some((_, text)) = pairs.into_iter().find(|(name, _)| name == "q") else {
        return Ok(None);
    };
    match Query::parse(&text) {
        Ok(query) => Ok(Some(query)),
        Err(QueryError::Blank) => Ok(None),
        Err(error @ QueryError::TooLong { .. }) => Err(bad(error.to_string())),
    }
}

/// The `path` of a file request; the first `path` wins.
fn file_param(query: Option<&str>) -> Result<LocalFile, RouteError> {
    let bad = |problem: String| RouteError::BadParameter {
        field: "path",
        problem,
    };
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(query.unwrap_or_default()).map_err(|e| bad(e.to_string()))?;
    let (_, text) = pairs
        .into_iter()
        .find(|(name, _)| name == "path")
        .ok_or_else(|| bad("missing".to_owned()))?;
    LocalFile::parse(&text).map_err(|e| bad(e.to_string()))
}

impl fmt::Display for Page {
    /// The path, percent-encoded where a parameter needs it: the only `href` a page makes to
    /// itself.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inbox => f.write_str("/"),
            Self::FollowUps => f.write_str("/followups"),
            Self::Sessions => f.write_str("/sessions"),
            Self::Dormant => f.write_str("/dormant"),
            Self::Map => f.write_str("/map"),
            Self::Owners => f.write_str("/owners"),
            Self::Health => f.write_str("/health"),
            Self::WorkItems => f.write_str("/work-items"),
            Self::Approvals => f.write_str("/approvals"),
            Self::Pam => f.write_str("/pam"),
            Self::Search(None) => f.write_str("/search"),
            Self::Search(Some(query)) => write!(
                f,
                "/search?q={}",
                utf8_percent_encode(query.as_str(), NON_ALPHANUMERIC)
            ),
            Self::Session(id) => write!(f, "/sessions/{id}"),
            Self::Save(id) => write!(f, "/saves/{id}"),
        }
    }
}

impl Asset {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Style => STYLE_PATH,
            Self::Script => SCRIPT_PATH,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The scheme written out by hand: each path, each method, and the parameters that must
    //! be refused before a handler sees them.

    use super::*;

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    /// A GET of a request target: the path, then the query string after a `?`.
    fn get(target: &str) -> Result<Route, RouteError> {
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (target, None),
        };
        Route::resolve(&Method::GET, path, query)
    }

    fn query(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    /// Mutation: drop one arm of `resolve_get`, or one entry of `PostRoute::ALL`.
    #[test]
    fn every_path_resolves_to_its_route() {
        assert_eq!(get("/"), Ok(Route::Page(Page::Inbox)));
        assert_eq!(get("/sessions"), Ok(Route::Page(Page::Sessions)));
        assert_eq!(get("/dormant"), Ok(Route::Page(Page::Dormant)));
        assert_eq!(get("/map"), Ok(Route::Page(Page::Map)));
        assert_eq!(get("/health"), Ok(Route::Page(Page::Health)));
        assert_eq!(get("/work-items"), Ok(Route::Page(Page::WorkItems)));
        assert_eq!(get("/pam"), Ok(Route::Page(Page::Pam)));
        assert_eq!(get("/search"), Ok(Route::Page(Page::Search(None))));
        assert_eq!(
            get(&format!("/sessions/{ID}")),
            Ok(Route::Page(Page::Session(SessionId::parse(ID).unwrap())))
        );
        assert_eq!(get("/static/app.js"), Ok(Route::Asset(Asset::Script)));
        assert_eq!(get("/static/style.css"), Ok(Route::Asset(Asset::Style)));
        let posts = [
            ("/seen", PostRoute::Seen),
            ("/pin", PostRoute::Pin),
            ("/unpin", PostRoute::Unpin),
            ("/focus", PostRoute::Focus),
            ("/refresh-link", PostRoute::RefreshLink),
            ("/work-items/refresh", PostRoute::RefreshWorkItems),
            ("/pam/request", PostRoute::RequestGrant),
            ("/pam/refresh", PostRoute::RefreshPam),
        ];
        for (path, expected) in posts {
            assert_eq!(
                Route::resolve(&Method::POST, path, None),
                Ok(Route::Post(expected)),
                "{path}"
            );
        }
    }

    /// Mutation: let a GET reach a post route, or a POST a page.
    #[test]
    fn a_path_answers_only_its_own_method() {
        assert_eq!(get("/seen"), Err(RouteError::MethodNotAllowed));
        assert_eq!(
            Route::resolve(&Method::POST, "/health", None),
            Err(RouteError::MethodNotAllowed)
        );
        assert_eq!(
            Route::resolve(&Method::PUT, "/", None),
            Err(RouteError::MethodNotAllowed)
        );
        assert_eq!(get("/nope"), Err(RouteError::NotFound));
        assert_eq!(
            Route::resolve(&Method::POST, "/nope", None),
            Err(RouteError::NotFound)
        );
    }

    /// Mutation: skip `SessionId::parse`.
    #[test]
    fn path_parameters_go_through_their_constructors() {
        assert!(matches!(
            get("/sessions/not-an-id"),
            Err(RouteError::BadParameter {
                field: "session",
                ..
            })
        ));
    }

    /// The `q` parameter: trimmed, blank is no query, 200 characters pass and 201 are a 400
    /// that names the field, and the first `q` wins.
    /// Mutation, each ran red: treat blank as an error, or ignore the query string.
    #[test]
    fn the_search_query_is_trimmed_bounded_and_named_when_refused() {
        assert_eq!(
            get("/search?q=++ABC-12+"),
            Ok(Route::Page(Page::Search(Some(query("ABC-12")))))
        );
        assert_eq!(
            get("/search?q=a%26b%20%23c"),
            Ok(Route::Page(Page::Search(Some(query("a&b #c")))))
        );
        assert_eq!(get("/search?q="), Ok(Route::Page(Page::Search(None))));
        assert_eq!(get("/search?q=%20%20"), Ok(Route::Page(Page::Search(None))));
        assert_eq!(get("/search?other=1"), Ok(Route::Page(Page::Search(None))));
        assert_eq!(
            get("/search?q=one&q=two"),
            Ok(Route::Page(Page::Search(Some(query("one")))))
        );
        assert!(get(&format!("/search?q={}", "x".repeat(200))).is_ok());
        let Err(RouteError::BadParameter { field, problem }) =
            get(&format!("/search?q={}", "x".repeat(201)))
        else {
            panic!("201 characters are refused");
        };
        assert_eq!(field, "q");
        assert_eq!(problem, "is 201 characters; at most 200 are allowed");
    }

    /// The `href` a page writes resolves back to the page it names.
    /// Mutation: write the search query without percent-encoding.
    #[test]
    fn a_page_renders_the_path_that_resolves_to_it() {
        for page in [
            Page::Inbox,
            Page::FollowUps,
            Page::Sessions,
            Page::Dormant,
            Page::Map,
            Page::Owners,
            Page::Health,
            Page::WorkItems,
            Page::Pam,
            Page::Search(None),
            Page::Search(Some(query("a&b #c é"))),
            Page::Session(SessionId::parse(ID).unwrap()),
            Page::Save(SaveId::Auto),
            Page::Save(SaveId::Kept(1_700_000_000_123)),
        ] {
            assert_eq!(
                get(&page.to_string()),
                Ok(Route::Page(page.clone())),
                "{page}"
            );
        }
        assert_eq!(
            Page::Search(Some(query("ABC-12 & #3"))).to_string(),
            "/search?q=ABC%2D12%20%26%20%233"
        );
    }
}
