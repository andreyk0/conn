//! What the human can change through the site, and how a request becomes one change.
//!
//! **Nothing here writes to a session or resumes one; what reaches out as the user (approving
//! a review, asking PAM for a grant) is a click on its own button, never a side effect.** The
//! changes
//! are exactly: the seen and pinned marks (`seen.json`), a request to read a pull request's
//! state or the Jira work items again, raising a terminal window the compositor already
//! shows, approving a review request, making or deleting a saved set of sessions, asking PAM
//! for a grant, and asking for the PAM inventory to be read again. Every other module of the
//! site only reads the model.
//!
//! A request body is parsed once, at the boundary, into an [`Action`]; [`perform`] is the one
//! function that acts on one, through [`crate::state::Handle`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::route::PostRoute;
use crate::gh::PullRequestRef;
use crate::links::Extractor;
use crate::model::Model;
use crate::pam::RequestError;
use crate::pam::gcloud::Justified;
use crate::pam::ids::EntitlementName;
use crate::reviews::github::CommitSha;
use crate::saves::{SaveId, SavesError};
use crate::sessions::windows::{TitleMatch, WindowMatch, find_window, focus_window, pinned_suffix};
use crate::state::seen::{SeenError, TicketMark};
use crate::state::{Handle, NoJira, ReviewActionError, SaveNowError};
use crate::types::{Instant, JiraKey, Link, SessionId};
use crate::util::runner::CommandRunner;

/// The form fields of a request, every value text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fields(BTreeMap<String, String>);

/// The body could not be read as fields.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    #[error("send application/x-www-form-urlencoded")]
    MediaType,
    #[error("the body is not a form: {0}")]
    Form(String),
}

impl Fields {
    /// The fields of a form body; any other declared type is refused.
    pub fn parse(content_type: Option<&str>, body: &[u8]) -> Result<Self, BodyError> {
        let media_type = content_type
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase());
        match media_type.as_deref() {
            Some("application/x-www-form-urlencoded") => {
                let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
                    .map_err(|e| BodyError::Form(e.to_string()))?;
                Ok(Self(pairs.into_iter().collect()))
            }
            Some(_) | None => Err(BodyError::MediaType),
        }
    }

    /// The field's text; absent or only whitespace is [`Problem::Missing`].
    fn required(&self, field: &'static str) -> Result<&str, FieldError> {
        self.0
            .get(field)
            .map(String::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or(FieldError {
                field,
                problem: Problem::Missing,
            })
    }
}

/// A field that could not become what the action needs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("field `{field}`: {problem}")]
pub struct FieldError {
    pub field: &'static str,
    pub problem: Problem,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("missing or blank")]
    Missing,
    #[error("{0}")]
    Invalid(String),
}

/// One change the human asked for, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The card of `session` that appeared at `since`, as the page that showed it sent it, and
    /// the unseen comments the card showed on the tickets its session owns.
    Seen {
        session: SessionId,
        since: Instant,
        tickets: Vec<TicketMark>,
    },
    Pin {
        session: SessionId,
    },
    Unpin {
        session: SessionId,
    },
    Focus {
        session: SessionId,
    },
    RefreshLink {
        pr: PullRequestRef,
    },
    /// Read the user's open Jira work items again now, whatever the age of the last read.
    RefreshWorkItems,
    /// Approve a review request at the commit its card showed, and 👍 the message that asked.
    ApproveReview {
        pr: PullRequestRef,
        head: CommitSha,
    },
    /// Try the 👍 of a recorded approval again.
    RetryReaction {
        pr: PullRequestRef,
        head: CommitSha,
    },
    /// Keep the open sessions as a manual save.
    SaveSessions,
    /// Delete a kept save.
    DeleteSave {
        save: SaveId,
    },
    /// Ask PAM for a grant on an entitlement, for `secs`, with the reason given, if any.
    RequestGrant {
        entitlement: EntitlementName,
        secs: u64,
        justification: Option<Justified>,
    },
    /// Read the PAM inventory again now.
    RefreshPam,
}

impl Action {
    /// The action a POST to `route` with these fields asks for.
    pub fn parse(
        route: PostRoute,
        fields: &Fields,
        extractor: &Extractor,
    ) -> Result<Self, FieldError> {
        let session = || session_field(fields);
        let link = || link_field(fields, extractor);
        Ok(match route {
            PostRoute::Seen => Self::Seen {
                session: session()?,
                since: since_field(fields)?,
                tickets: tickets_field(fields)?,
            },
            PostRoute::Pin => Self::Pin {
                session: session()?,
            },
            PostRoute::Unpin => Self::Unpin {
                session: session()?,
            },
            PostRoute::Focus => Self::Focus {
                session: session()?,
            },
            PostRoute::RefreshWorkItems => Self::RefreshWorkItems,
            PostRoute::ApproveReview => Self::ApproveReview {
                pr: pr_field(fields)?,
                head: head_field(fields)?,
            },
            PostRoute::RetryReaction => Self::RetryReaction {
                pr: pr_field(fields)?,
                head: head_field(fields)?,
            },
            PostRoute::RefreshLink => {
                let link = link()?;
                Self::RefreshLink {
                    pr: PullRequestRef::try_from(&link).map_err(|e| FieldError {
                        field: "link",
                        problem: Problem::Invalid(e.to_string()),
                    })?,
                }
            }
            PostRoute::SaveSessions => Self::SaveSessions,
            PostRoute::DeleteSave => Self::DeleteSave {
                save: save_field(fields)?,
            },
            PostRoute::RequestGrant => Self::RequestGrant {
                entitlement: entitlement_field(fields)?,
                secs: secs_field(fields)?,
                justification: justification_field(fields)?,
            },
            PostRoute::RefreshPam => Self::RefreshPam,
        })
    }
}

fn entitlement_field(fields: &Fields) -> Result<EntitlementName, FieldError> {
    EntitlementName::parse(fields.required("entitlement")?.trim()).map_err(|e| FieldError {
        field: "entitlement",
        problem: Problem::Invalid(e.to_string()),
    })
}

fn secs_field(fields: &Fields) -> Result<u64, FieldError> {
    fields
        .required("secs")?
        .trim()
        .parse()
        .map_err(|_error| FieldError {
            field: "secs",
            problem: Problem::Invalid("not a number of seconds".to_owned()),
        })
}

/// Absent or blank is no reason; whether one is required is the entitlement's to say.
fn justification_field(fields: &Fields) -> Result<Option<Justified>, FieldError> {
    Justified::parse(fields.0.get("justification").map_or("", String::as_str)).map_err(|e| {
        FieldError {
            field: "justification",
            problem: Problem::Invalid(e.to_string()),
        }
    })
}

fn save_field(fields: &Fields) -> Result<SaveId, FieldError> {
    SaveId::parse(fields.required("save")?.trim()).map_err(|e| FieldError {
        field: "save",
        problem: Problem::Invalid(e.to_string()),
    })
}

/// `owner/repo#n`, as a review card writes it.
fn pr_field(fields: &Fields) -> Result<PullRequestRef, FieldError> {
    let invalid = |problem: String| FieldError {
        field: "pr",
        problem: Problem::Invalid(problem),
    };
    let text = fields.required("pr")?.trim();
    let (repository, number) = text
        .split_once('#')
        .ok_or_else(|| invalid("not owner/repo#number".to_owned()))?;
    let (owner, repo) = repository
        .split_once('/')
        .ok_or_else(|| invalid("not owner/repo#number".to_owned()))?;
    let number = number
        .parse()
        .map_err(|_| invalid("not owner/repo#number".to_owned()))?;
    PullRequestRef::new(owner, repo, number).map_err(|e| invalid(e.to_string()))
}

fn head_field(fields: &Fields) -> Result<CommitSha, FieldError> {
    CommitSha::parse(fields.required("head")?.trim()).ok_or_else(|| FieldError {
        field: "head",
        problem: Problem::Invalid("not a commit".to_owned()),
    })
}

fn session_field(fields: &Fields) -> Result<SessionId, FieldError> {
    SessionId::parse(fields.required("session")?.trim()).map_err(|e| FieldError {
        field: "session",
        problem: Problem::Invalid(e.to_string()),
    })
}

fn since_field(fields: &Fields) -> Result<Instant, FieldError> {
    fields
        .required("since")?
        .trim()
        .parse::<Instant>()
        .map_err(|e| FieldError {
            field: "since",
            problem: Problem::Invalid(e.to_string()),
        })
}

/// The tickets a card's "seen" marks with it, as the card wrote them: `KEY=instant` pairs
/// separated by spaces, each the ticket and its newest unseen comment's `created`. Absent or
/// empty is none.
fn tickets_field(fields: &Fields) -> Result<Vec<TicketMark>, FieldError> {
    let invalid = |problem: String| FieldError {
        field: "tickets",
        problem: Problem::Invalid(problem),
    };
    fields
        .0
        .get("tickets")
        .map_or("", String::as_str)
        .split_whitespace()
        .map(|pair| {
            let (key, through) = pair
                .split_once('=')
                .ok_or_else(|| invalid(format!("`{pair}` is not KEY=instant")))?;
            Ok(TicketMark {
                ticket: JiraKey::parse(key)
                    .ok_or_else(|| invalid(format!("`{key}` is not a ticket key")))?,
                through: through
                    .parse::<Instant>()
                    .map_err(|e| invalid(format!("`{through}`: {e}")))?,
            })
        })
        .collect()
}

fn link_field(fields: &Fields, extractor: &Extractor) -> Result<Link, FieldError> {
    let text = fields.required("link")?.trim();
    extractor.parse_exact(text).ok_or_else(|| FieldError {
        field: "link",
        problem: Problem::Invalid("not a link this tool recognises".to_owned()),
    })
}

/// What happened when the compositor was asked to raise a session's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusOutcome {
    Focused,
    /// Several terminals carry the title.
    Ambiguous {
        count: usize,
    },
    NotFound,
    /// The compositor could not be asked, or refused the focus request.
    Unavailable {
        reason: String,
    },
    /// The session is not running in a terminal.
    NoTerminal,
    /// The session runs in a terminal, but Claude Code has not titled it, so its window
    /// carries only the generic title every untitled session shares.
    Untitled,
}

impl FocusOutcome {
    /// The reply the page's script reads: a short `outcome` word and a `detail` line.
    pub fn to_json(&self) -> Value {
        let (outcome, detail) = match self {
            Self::Focused => ("focused", "The terminal is focused.".to_owned()),
            // The session is running in each case below but `NoTerminal`, so none of them
            // offers the resume command: it would run the session a second time.
            Self::Ambiguous { count } => (
                "ambiguous",
                format!(
                    "{count} terminals show this session's title, so none was picked; switch to \
                     it by hand."
                ),
            ),
            Self::NotFound => (
                "not_found",
                "This session is running, but no terminal shows its title (it may run outside \
                 kitty); switch to it by hand."
                    .to_owned(),
            ),
            Self::Unavailable { reason } => ("unavailable", reason.clone()),
            Self::NoTerminal => (
                "no_terminal",
                "This session has no live terminal to focus; run the resume command instead."
                    .to_owned(),
            ),
            Self::Untitled => (
                "untitled",
                "This session is running, but its terminal shows no title of its own, so it \
                 cannot be told apart from other untitled ones; switch to it by hand (look for \
                 \u{201c}Claude Code\u{201d})."
                    .to_owned(),
            ),
        };
        json!({"outcome": outcome, "detail": detail})
    }
}

/// What a performed action answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Changed,
    Focus(FocusOutcome),
}

impl Done {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Changed => json!({"outcome": "changed"}),
            Self::Focus(outcome) => outcome.to_json(),
        }
    }
}

/// An action that could not be carried out. Each variant is a refusal with a reason, never a
/// silent no-op.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("no session {0} is known")]
    UnknownSession(SessionId),
    #[error(transparent)]
    Seen(#[from] SeenError),
    #[error(transparent)]
    NoJira(#[from] NoJira),
    #[error(transparent)]
    Review(#[from] ReviewActionError),
    #[error(transparent)]
    SaveNow(#[from] SaveNowError),
    #[error(transparent)]
    Saves(#[from] SavesError),
    #[error(transparent)]
    Pam(#[from] RequestError),
}

/// What the actions touch: the state's handle for what the human owns, the model to check
/// sessions against, and the runner the compositor is asked through.
pub struct Effects<'a> {
    pub handle: &'a Handle,
    pub model: &'a tokio::sync::RwLock<Model>,
    pub runner: &'a Arc<dyn CommandRunner>,
}

/// Carries out one action.
pub async fn perform(effects: &Effects<'_>, action: Action) -> Result<Done, ActionError> {
    let handle = effects.handle;
    match action {
        Action::Seen {
            session,
            since,
            tickets,
        } => {
            known(effects, &session).await?;
            handle.mark_seen(session, since, tickets)?;
        }
        Action::Pin { session } => {
            known(effects, &session).await?;
            handle.pin(session)?;
        }
        Action::Unpin { session } => {
            known(effects, &session).await?;
            handle.unpin(&session)?;
        }
        Action::Focus { session } => return focus(effects, &session).await.map(Done::Focus),
        Action::RefreshLink { pr } => handle.request_refresh(pr),
        Action::RefreshWorkItems => handle.request_work_items_refresh()?,
        Action::ApproveReview { pr, head } => {
            handle
                .approve_review(Arc::clone(effects.runner), &pr, &head)
                .await?;
        }
        Action::RetryReaction { pr, head } => {
            handle.retry_reaction(Arc::clone(effects.runner), &pr.to_string(), head.as_str())?;
        }
        Action::SaveSessions => {
            let model = effects.model.read().await;
            handle.save_sessions(&model)?;
        }
        Action::DeleteSave { save } => handle.delete_save(save)?,
        Action::RequestGrant {
            entitlement,
            secs,
            justification,
        } => {
            handle
                .request_grant(&**effects.runner, &entitlement, secs, justification)
                .await?;
        }
        Action::RefreshPam => handle.request_pam_inventory()?,
    }
    Ok(Done::Changed)
}

async fn known(effects: &Effects<'_>, session: &SessionId) -> Result<(), ActionError> {
    if effects.model.read().await.session(session).is_some() {
        Ok(())
    } else {
        Err(ActionError::UnknownSession(session.clone()))
    }
}

/// Raises the window whose title is the session's, when exactly one has it.
async fn focus(effects: &Effects<'_>, session: &SessionId) -> Result<FocusOutcome, ActionError> {
    let title = {
        let model = effects.model.read().await;
        let view = model
            .session(session)
            .ok_or_else(|| ActionError::UnknownSession(session.clone()))?;
        if !view.session.runs_in_terminal() {
            return Ok(FocusOutcome::NoTerminal);
        }
        view.session.window_title.clone()
    };
    // A title pinned to the session's id names exactly one window, so those are tried first:
    // the one `conn hook` sets (ending in the id's first block), then the older whole-id form;
    // then the title Claude Code itself would show.
    let legacy = format!("{LEGACY_PINNED_TITLE_PREFIX}{session}");
    let titles: Vec<TitleMatch<'_>> = [
        TitleMatch::Pinned(pinned_suffix(session)),
        TitleMatch::Exact(&legacy),
    ]
    .into_iter()
    .chain(title.as_deref().map(TitleMatch::Exact))
    .collect();
    let runner: &dyn CommandRunner = &**effects.runner;
    Ok(match find_window(runner, &titles).await {
        WindowMatch::Unique(window) => match focus_window(runner, window).await {
            Ok(()) => FocusOutcome::Focused,
            Err(error) => FocusOutcome::Unavailable {
                reason: error.to_string(),
            },
        },
        WindowMatch::Ambiguous { count } => FocusOutcome::Ambiguous { count },
        WindowMatch::NotFound if title.is_none() => FocusOutcome::Untitled,
        WindowMatch::NotFound => FocusOutcome::NotFound,
        WindowMatch::Unavailable { reason } => FocusOutcome::Unavailable {
            reason: reason.to_string(),
        },
    })
}

/// What a `sessionTitle` hook used before `conn hook` set titles put before the whole id:
/// `Claude: <session id>`. A window still carrying one keeps it until its session's next
/// prompt.
const LEGACY_PINNED_TITLE_PREFIX: &str = "Claude: ";

#[cfg(test)]
mod tests {
    //! The parse of each action from hand-written bodies, and the focus outcomes against a
    //! scripted `niri`. The state-changing arms are exercised through the router tests, which
    //! run them against a real state in a scratch directory.

    use super::*;
    use crate::config::Config;

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    const JIRA_URL: &str = "https://example.atlassian.net/browse/ABC-123";
    const SINCE: &str = "2026-10-28T10:02:01Z";
    const PR_URL: &str = "https://github.com/example-org/example-repo/pull/42";

    fn extractor() -> Extractor {
        let config = Config::parse(
            "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n[slack]\nhost = \"example.slack.com\"\n",
            std::path::Path::new("/cfg.toml"),
            std::path::Path::new("/home/dev"),
        )
        .unwrap();
        Extractor::from_config(&config).unwrap()
    }

    fn form(pairs: &[(&str, &str)]) -> Fields {
        let body = serde_urlencoded::to_string(pairs).unwrap();
        Fields::parse(Some("application/x-www-form-urlencoded"), body.as_bytes()).unwrap()
    }

    fn parse(route: PostRoute, pairs: &[(&str, &str)]) -> Result<Action, FieldError> {
        Action::parse(route, &form(pairs), &extractor())
    }

    fn session() -> SessionId {
        SessionId::parse(ID).unwrap()
    }

    /// Mutation: swap two arms of `Action::parse`, or map `RefreshWorkItems` to another action (ran red as `Pin`).
    #[test]
    fn each_route_parses_into_its_action() {
        assert_eq!(
            parse(
                PostRoute::Seen,
                &[("session", ID), ("since", "2026-10-28T10:02:01Z")]
            ),
            Ok(Action::Seen {
                session: session(),
                since: "2026-10-28T10:02:01Z".parse().unwrap(),
                tickets: Vec::new(),
            })
        );
        assert_eq!(
            parse(
                PostRoute::Seen,
                &[
                    ("session", ID),
                    ("since", "2026-10-28T10:02:01Z"),
                    (
                        "tickets",
                        "ABC-1=2026-10-28T09:00:00Z ABC-22=2026-10-28T09:30:00Z"
                    ),
                ]
            ),
            Ok(Action::Seen {
                session: session(),
                since: "2026-10-28T10:02:01Z".parse().unwrap(),
                tickets: vec![
                    TicketMark {
                        ticket: JiraKey::parse("ABC-1").unwrap(),
                        through: "2026-10-28T09:00:00Z".parse().unwrap(),
                    },
                    TicketMark {
                        ticket: JiraKey::parse("ABC-22").unwrap(),
                        through: "2026-10-28T09:30:00Z".parse().unwrap(),
                    },
                ],
            })
        );
        assert_eq!(
            parse(PostRoute::Pin, &[("session", ID)]),
            Ok(Action::Pin { session: session() })
        );
        assert_eq!(
            parse(PostRoute::Unpin, &[("session", ID)]),
            Ok(Action::Unpin { session: session() })
        );
        assert_eq!(
            parse(PostRoute::Focus, &[("session", ID)]),
            Ok(Action::Focus { session: session() })
        );
        assert_eq!(
            parse(PostRoute::RefreshLink, &[("link", PR_URL)]),
            Ok(Action::RefreshLink {
                pr: PullRequestRef::new("example-org", "example-repo", 42).unwrap()
            })
        );
        assert_eq!(
            parse(PostRoute::RefreshWorkItems, &[]),
            Ok(Action::RefreshWorkItems)
        );
    }

    /// Mutation: skip a constructor (the matching row accepts the bad value), or name the
    /// wrong field in the error.
    #[test]
    fn a_bad_field_is_refused_and_named() {
        let field = |route, pairs: &[(&str, &str)]| parse(route, pairs).unwrap_err().field;
        assert_eq!(field(PostRoute::Seen, &[]), "session");
        assert_eq!(
            field(PostRoute::Seen, &[("session", "nope"), ("since", SINCE)]),
            "session"
        );
        assert_eq!(
            field(PostRoute::Seen, &[("session", ID)]),
            "since",
            "a seen mark must name its card"
        );
        assert_eq!(
            field(PostRoute::Seen, &[("session", ID), ("since", "yesterday")]),
            "since"
        );
        for tickets in ["ABC-1", "nope=2026-10-28T09:00:00Z", "ABC-1=yesterday"] {
            assert_eq!(
                field(
                    PostRoute::Seen,
                    &[("session", ID), ("since", SINCE), ("tickets", tickets)]
                ),
                "tickets",
                "{tickets}"
            );
        }
        assert_eq!(
            field(PostRoute::RefreshLink, &[("link", "not a link")]),
            "link"
        );
        assert_eq!(
            field(PostRoute::RefreshLink, &[("link", JIRA_URL)]),
            "link",
            "a Jira link is not a pull request"
        );
    }

    /// Mutation: accept any media type (the `text/plain` and JSON rows go red), or read a JSON
    /// body as a form.
    #[test]
    fn only_a_form_body_is_read() {
        let fields = Fields::parse(
            Some("Application/X-WWW-Form-Urlencoded; charset=utf-8"),
            b"session=x&other=p",
        )
        .unwrap();
        assert_eq!(fields.required("other"), Ok("p"));
        for (content_type, body) in [
            (Some("application/json"), &br#"{"session": "x"}"#[..]),
            (Some("text/plain"), b"session=x"),
            (None, b""),
        ] {
            assert_eq!(
                Fields::parse(content_type, body),
                Err(BodyError::MediaType),
                "{content_type:?}"
            );
        }
    }
}
