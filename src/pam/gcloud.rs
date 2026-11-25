//! The gcloud calls: every PAM read and the one write, each a subprocess whose JSON is parsed
//! at this boundary.
//!
//! Every call carries `--quiet`, so gcloud never waits on a prompt (it would offer to enable an
//! API, a write, and take the default "no"), and `--format=json`. "Could not tell" stays apart
//! from "told": an empty list is a value, and every [`PamError`] is a failure to find out.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::entitlement::{GrantLength, RawEntitlement};
use super::grant::{Grant, parse_created, parse_grants};
use super::hierarchy::{ASSET_TYPES, RawNode};
use super::ids::{EntitlementName, LOCATION, NumericId, Parent, ProjectId};
use crate::util::runner::{CommandRunner, ToolError, run_tool};

const GCLOUD: &str = "gcloud";

/// One read's deadline. A PAM search answers in about a second, and the organization-wide
/// asset search in a few; a read is never what a page waits on.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A request's deadline. Creating a grant is one call that answers in seconds; the page waits
/// on it, so it is bounded closer.
const CREATE_TIMEOUT: Duration = Duration::from_secs(45);

/// How many gcloud processes run at once. Each is a Python start of about half a CPU second;
/// eight keeps a 50-parent inventory to seconds without taking every core.
pub const CONCURRENCY: usize = 8;

/// What gcloud prints on stderr when its sign-in cannot be used without a person, matched in
/// lower case. Observed: "There was a problem refreshing your current auth tokens:
/// Reauthentication failed. cannot prompt during non-interactive execution."
const SIGN_IN_MARKERS: [&str; 4] = [
    "reauthentication failed",
    "problem refreshing your current auth tokens",
    "do not currently have an active account",
    "gcloud auth login",
];

pub const SIGN_IN_HINT: &str = "gcloud auth login";

/// Why gcloud gave no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PamError {
    /// gcloud's sign-in needs a person: run [`SIGN_IN_HINT`].
    #[error("gcloud needs a fresh sign-in: run `{SIGN_IN_HINT}`")]
    NotSignedIn,
    #[error(transparent)]
    Tool(ToolError),
    #[error("gcloud printed something unexpected: {0}")]
    Malformed(String),
}

impl From<ToolError> for PamError {
    fn from(error: ToolError) -> Self {
        match &error {
            ToolError::NonZeroExit { stderr, .. } => {
                let stderr = stderr.to_lowercase();
                if SIGN_IN_MARKERS.iter().any(|marker| stderr.contains(marker)) {
                    Self::NotSignedIn
                } else {
                    Self::Tool(error)
                }
            }
            ToolError::Missing { .. }
            | ToolError::Spawn { .. }
            | ToolError::TimedOut { .. }
            | ToolError::OutputTooLarge { .. }
            | ToolError::Unparseable { .. } => Self::Tool(error),
        }
    }
}

async fn gcloud(
    runner: &dyn CommandRunner,
    mut args: Vec<String>,
    timeout: Duration,
) -> Result<Vec<u8>, PamError> {
    args.extend(["--quiet".to_owned(), "--format=json".to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(run_tool(runner, GCLOUD, &args, timeout).await?.stdout)
}

/// One asset of the organization-wide search: its full resource name.
#[derive(Deserialize)]
struct Asset {
    name: String,
}

/// The prefix the asset inventory puts before a PAM resource's own name.
const ASSET_PREFIX: &str = "//privilegedaccessmanager.googleapis.com/";

/// The parents that hold at least one entitlement anywhere under the organization. The quota
/// project pays for the asset search, which the user's default project may not allow.
pub async fn parents(
    runner: &dyn CommandRunner,
    organization: &NumericId,
    quota_project: &ProjectId,
) -> Result<BTreeSet<Parent>, PamError> {
    let stdout = gcloud(
        runner,
        vec![
            "asset".to_owned(),
            "search-all-resources".to_owned(),
            format!("--scope=organizations/{}", organization.as_str()),
            "--asset-types=privilegedaccessmanager.googleapis.com/Entitlement".to_owned(),
            format!("--billing-project={}", quota_project.as_str()),
            "--read-mask=name".to_owned(),
        ],
        READ_TIMEOUT,
    )
    .await?;
    let assets: Vec<Asset> =
        serde_json::from_slice(&stdout).map_err(|e| PamError::Malformed(e.to_string()))?;
    Ok(assets
        .iter()
        .filter_map(|asset| {
            let parsed = asset
                .name
                .strip_prefix(ASSET_PREFIX)
                .ok_or_else(|| "not a PAM resource".to_owned())
                .and_then(|name| EntitlementName::parse(name).map_err(|e| e.to_string()));
            parsed
                .inspect_err(|reason| {
                    tracing::warn!(asset = %asset.name, %reason, "an entitlement in the inventory is passed over");
                })
                .ok()
                .map(|name| name.parent)
        })
        .collect())
}

/// The organization's folders, projects and itself, each with its name and parent: one asset
/// search of a few seconds, read with the inventory.
pub async fn hierarchy(
    runner: &dyn CommandRunner,
    organization: &NumericId,
    quota_project: &ProjectId,
) -> Result<Vec<RawNode>, PamError> {
    let stdout = gcloud(
        runner,
        vec![
            "asset".to_owned(),
            "search-all-resources".to_owned(),
            format!("--scope=organizations/{}", organization.as_str()),
            format!("--asset-types={ASSET_TYPES}"),
            format!("--billing-project={}", quota_project.as_str()),
            "--read-mask=name,displayName,parentFullResourceName".to_owned(),
        ],
        READ_TIMEOUT,
    )
    .await?;
    serde_json::from_slice(&stdout).map_err(|e| PamError::Malformed(e.to_string()))
}

/// The entitlements under `parent` the user may request; PAM resolves group membership.
pub async fn requestable(
    runner: &dyn CommandRunner,
    parent: &Parent,
) -> Result<Vec<RawEntitlement>, PamError> {
    let stdout = gcloud(
        runner,
        vec![
            "pam".to_owned(),
            "entitlements".to_owned(),
            "search".to_owned(),
            parent.flag(),
            format!("--location={LOCATION}"),
            "--caller-access-type=grant-requester".to_owned(),
        ],
        READ_TIMEOUT,
    )
    .await?;
    serde_json::from_slice(&stdout).map_err(|e| PamError::Malformed(e.to_string()))
}

/// The grants the user created on `name`, every state.
pub async fn grants(
    runner: &dyn CommandRunner,
    name: &EntitlementName,
) -> Result<Vec<Grant>, PamError> {
    let stdout = gcloud(
        runner,
        vec![
            "pam".to_owned(),
            "grants".to_owned(),
            "search".to_owned(),
            format!("--entitlement={}", name.id.as_str()),
            name.parent.flag(),
            format!("--location={LOCATION}"),
            "--caller-relationship=had-created".to_owned(),
        ],
        READ_TIMEOUT,
    )
    .await?;
    parse_grants(&stdout).map_err(|e| PamError::Malformed(e.to_string()))
}

/// Asks PAM for a grant on `name` for `length`, with `justification` when there is one.
pub async fn create(
    runner: &dyn CommandRunner,
    name: &EntitlementName,
    length: GrantLength,
    justification: Option<&Justified>,
) -> Result<Grant, PamError> {
    let mut args = vec![
        "pam".to_owned(),
        "grants".to_owned(),
        "create".to_owned(),
        format!("--entitlement={}", name.id.as_str()),
        name.parent.flag(),
        format!("--location={LOCATION}"),
        format!("--requested-duration={}s", length.secs()),
    ];
    if let Some(text) = justification {
        args.push(format!("--justification={}", text.as_str()));
    }
    let stdout = gcloud(runner, args, CREATE_TIMEOUT).await?;
    parse_created(&stdout).map_err(|e| PamError::Malformed(e.to_string()))
}

/// A request's reason: trimmed, not empty, one line, at most [`JUSTIFIED_CHARS_MAX`]
/// characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Justified(String);

/// Long enough for a sentence and a link; a reason is read by an approver at a glance.
pub const JUSTIFIED_CHARS_MAX: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JustifiedError {
    #[error("is {0} characters; at most {JUSTIFIED_CHARS_MAX} are allowed")]
    TooLong(usize),
    #[error("must be one line")]
    Multiline,
}

impl Justified {
    /// `None` for blank text.
    pub fn parse(text: &str) -> Result<Option<Self>, JustifiedError> {
        let text = text.trim();
        let chars = text.chars().count();
        if text.is_empty() {
            Ok(None)
        } else if chars > JUSTIFIED_CHARS_MAX {
            Err(JustifiedError::TooLong(chars))
        } else if text.contains(['\n', '\r']) {
            Err(JustifiedError::Multiline)
        } else {
            Ok(Some(Self(text.to_owned())))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `read` over every item, at most [`CONCURRENCY`] at once, each answer beside its item.
pub async fn each<I, T, F, Fut>(
    runner: &Arc<dyn CommandRunner>,
    items: Vec<I>,
    read: F,
) -> Vec<(I, Result<T, PamError>)>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(Arc<dyn CommandRunner>, I) -> Fut,
    Fut: Future<Output = (I, Result<T, PamError>)> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let mut jobs = JoinSet::new();
    for item in items {
        let permits = Arc::clone(&permits);
        let job = read(Arc::clone(runner), item);
        jobs.spawn(async move {
            // The semaphore is never closed, so a permit always comes.
            let _permit = permits.acquire_owned().await.ok();
            job.await
        });
    }
    let mut answers = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok(answer) => answers.push(answer),
            Err(error) => tracing::warn!(%error, "a gcloud read stopped before answering"),
        }
    }
    answers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::runner::{Exit, FakeRunner, Output};

    fn failing(stderr: &str) -> FakeRunner {
        FakeRunner::new([Ok(Output {
            exit: Exit::Code(1),
            stdout: Vec::new(),
            stderr: stderr.to_owned(),
        })])
    }

    fn name() -> EntitlementName {
        EntitlementName::parse("folders/111/locations/global/entitlements/admin").unwrap()
    }

    /// The reauthentication failure seen live is a sign-in to renew, not a failed read; any
    /// other exit is the tool's failure.
    /// Mutation: drop the markers (the first row turns into `Tool`).
    #[tokio::test]
    async fn a_sign_in_that_needs_a_person_is_told_apart() {
        let runner = failing(
            "ERROR: (gcloud.pam.grants.search) There was a problem refreshing your current auth \
             tokens: Reauthentication failed. cannot prompt during non-interactive execution.",
        );
        assert_eq!(grants(&runner, &name()).await, Err(PamError::NotSignedIn));
        let runner = failing("ERROR: PERMISSION_DENIED");
        assert!(matches!(
            grants(&runner, &name()).await,
            Err(PamError::Tool(ToolError::NonZeroExit { .. }))
        ));
    }

    /// The command line is the one the research proved: entitlement id, parent flag, global,
    /// the user's own grants, and never a prompt.
    /// Mutation: drop `--quiet`, or pass the full name as `--entitlement`.
    #[tokio::test]
    async fn the_grants_search_names_the_entitlement_by_id_under_its_parent() {
        let runner = FakeRunner::printing("[]");
        assert_eq!(grants(&runner, &name()).await, Ok(Vec::new()));
        assert_eq!(
            runner.lines(),
            [
                "gcloud pam grants search --entitlement=admin --folder=111 --location=global \
              --caller-relationship=had-created --quiet --format=json"
            ]
        );
    }

    /// Mutation: leave out the duration, or send a blank justification.
    #[tokio::test]
    async fn a_request_sends_the_length_and_the_reason() {
        let runner = FakeRunner::printing(
            r#"{"state": "ACTIVATING", "createTime": "2026-11-19T10:00:00Z", "requestedDuration": "3600s"}"#,
        );
        let entitlement = crate::pam::entitlement::Entitlement {
            name: name(),
            max: Duration::from_secs(3600),
            approval: crate::pam::entitlement::Approval::Auto,
            justification: crate::pam::entitlement::Justification::Optional,
            roles: Vec::new(),
        };
        let length = entitlement.length(3600).unwrap();
        let reason = Justified::parse("  deploy the fix ").unwrap();
        let grant = create(&runner, &name(), length, reason.as_ref())
            .await
            .unwrap();
        assert_eq!(grant.state, crate::pam::grant::GrantState::Activating);
        assert_eq!(
            runner.lines(),
            [
                "gcloud pam grants create --entitlement=admin --folder=111 --location=global \
              --requested-duration=3600s --justification=deploy the fix --quiet --format=json"
            ]
        );
    }

    /// Parents come from the asset names, once each; a regional entitlement is passed over.
    /// Mutation: keep duplicates (three parents), or fail on the regional name.
    #[tokio::test]
    async fn the_inventory_yields_each_parent_once() {
        let runner = FakeRunner::printing(
            r#"[
              {"name": "//privilegedaccessmanager.googleapis.com/folders/111/locations/global/entitlements/admin"},
              {"name": "//privilegedaccessmanager.googleapis.com/folders/111/locations/global/entitlements/other"},
              {"name": "//privilegedaccessmanager.googleapis.com/projects/example-project/locations/global/entitlements/admin"},
              {"name": "//privilegedaccessmanager.googleapis.com/projects/example-project/locations/us-east1/entitlements/admin"}
            ]"#,
        );
        let found = parents(
            &runner,
            &NumericId::parse("999").unwrap(),
            &ProjectId::parse("quota-project").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            found.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["projects/example-project", "folders/111"]
        );
        assert_eq!(
            runner.lines(),
            [
                "gcloud asset search-all-resources --scope=organizations/999 \
              --asset-types=privilegedaccessmanager.googleapis.com/Entitlement \
              --billing-project=quota-project --read-mask=name --quiet --format=json"
            ]
        );
    }

    /// Mutation: accept a newline, or count bytes instead of characters.
    #[test]
    fn a_reason_is_one_bounded_line_and_blank_is_none() {
        assert_eq!(Justified::parse("  "), Ok(None));
        assert_eq!(Justified::parse("a\nb"), Err(JustifiedError::Multiline));
        assert!(
            Justified::parse(&"é".repeat(JUSTIFIED_CHARS_MAX))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            Justified::parse(&"x".repeat(JUSTIFIED_CHARS_MAX + 1)),
            Err(JustifiedError::TooLong(JUSTIFIED_CHARS_MAX + 1))
        );
    }
}
