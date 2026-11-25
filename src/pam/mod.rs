//! GCP Privileged Access Manager: the entitlements the user may request, the grants they hold
//! and how long each has left, and a request for a new one.
//!
//! Optional: without a `[pam]` section there is no [`Pam`], so no gcloud process ever starts.
//! Reads happen only while the PAM page was viewed in the last [`VIEWED_WINDOW`]: the
//! inventory of requestable entitlements once, saved, and again only when the user asks (it
//! changes rarely, and costs one gcloud run per parent); the grants of each every
//! [`GRANTS_EVERY`]. Both keep their last good answer beside the latest failure, so a lapsed
//! sign-in shows the older state with its age.
//!
//! The inventory is read in two steps, because PAM answers "may I request this?" only per
//! parent: an asset search of each configured organization finds every parent that holds an
//! entitlement, then `pam entitlements search --caller-access-type=grant-requester` asks each.

pub mod entitlement;
pub mod gcloud;
pub mod grant;
pub mod hierarchy;
pub mod ids;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

use crate::types::Instant;
use crate::util::fs::write_private;
use crate::util::runner::CommandRunner;
use entitlement::{Entitlement, GrantLength, Justification, RawEntitlement};
use gcloud::{Justified, PamError};
use grant::{Grant, Phase};
use ids::{EntitlementName, NumericId, Parent, ProjectId};

/// The organizations whose entitlements are searched: one at least.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organizations(BTreeSet<NumericId>);

impl Organizations {
    /// `ids`, when there is one at least.
    pub fn of(ids: BTreeSet<NumericId>) -> Option<Self> {
        (!ids.is_empty()).then_some(Self(ids))
    }

    pub fn iter(&self) -> impl Iterator<Item = &NumericId> {
        self.0.iter()
    }

    /// Where every place is named: by its organization too when there are several.
    fn naming(&self) -> hierarchy::Naming {
        if self.0.len() > 1 {
            hierarchy::Naming::FromOrganization
        } else {
            hierarchy::Naming::UnderOrganization
        }
    }
}

/// The `[pam]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PamConfig {
    pub organizations: Organizations,
    /// The project that pays for the asset search.
    pub quota_project: ProjectId,
    /// Entitlements kept for emergencies: listed apart, at the bottom.
    pub break_glass: BTreeSet<EntitlementName>,
}

/// The file in the state directory that holds the last good inventory.
pub const FILE_NAME: &str = "pam-entitlements.json";

/// How long after a failed first inventory read the next is tried: long enough not to spend a
/// gcloud run per parent on every page view while the sign-in has lapsed.
const INVENTORY_RETRY: SignedDuration = SignedDuration::from_mins(10);

/// How often the grants are read while the page is watched. A grant's time left is computed
/// from its activation, so this only paces noticing what changed elsewhere: an approval, a
/// revocation, a request made outside conn.
pub const GRANTS_EVERY: SignedDuration = SignedDuration::from_mins(1);

/// How long a view of the PAM page keeps the reads going. The page reloads itself every few
/// seconds while open, so this is how long reads continue after it is closed.
pub const VIEWED_WINDOW: SignedDuration = SignedDuration::from_mins(5);

/// How long a grant that is over is still shown on its row: long enough to see a denial or an
/// expiry the next morning.
const RECENT_WINDOW: SignedDuration = SignedDuration::from_hours(24);

/// A value read at `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read<T> {
    pub at: Instant,
    pub value: T,
}

/// The last good answer and the latest failure, when it came after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed<T> {
    pub good: Option<Read<T>>,
    pub failure: Option<Read<PamError>>,
}

impl<T> Default for Observed<T> {
    fn default() -> Self {
        Self {
            good: None,
            failure: None,
        }
    }
}

impl<T> Observed<T> {
    fn record(&mut self, outcome: Result<T, PamError>, at: Instant) {
        match outcome {
            Ok(value) => {
                self.good = Some(Read { at, value });
                self.failure = None;
            }
            Err(error) => self.failure = Some(Read { at, value: error }),
        }
    }
}

/// A parent whose entitlements could not be read: the inventory is missing whatever it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unread {
    pub parent: String,
    pub reason: String,
}

/// Where each parent sits in the organization, as the names a person knows it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Places {
    /// The path of each parent that has one; see [`hierarchy::paths`].
    Read(BTreeMap<Parent, String>),
    /// The hierarchy could not be read: the page shows ids alone, and says why.
    Failed(String),
}

impl Places {
    /// The path of `parent`, when it has one.
    pub fn of(&self, parent: &Parent) -> Option<&str> {
        match self {
            Self::Read(paths) => paths.get(parent).map(String::as_str),
            Self::Failed(_) => None,
        }
    }
}

/// The entitlements the user may request.
#[derive(Debug, Clone)]
pub struct Inventory {
    pub entitlements: Vec<Entitlement>,
    pub unread: Vec<Unread>,
    pub places: Places,
    /// As gcloud printed them, which is what is saved.
    raw: Vec<RawEntitlement>,
}

impl Inventory {
    fn of(raw: Vec<RawEntitlement>, unread: Vec<Unread>, places: Places) -> Self {
        let entitlements = raw
            .iter()
            .filter_map(|r| {
                r.parse()
                    .inspect_err(|error| {
                        tracing::warn!(%error, "a requestable entitlement is passed over");
                    })
                    .ok()
            })
            .collect();
        Self {
            entitlements,
            unread,
            places,
            raw,
        }
    }

    pub fn get(&self, name: &EntitlementName) -> Option<&Entitlement> {
        self.entitlements.iter().find(|e| &e.name == name)
    }
}

/// The saved form of a good inventory.
#[derive(Serialize, Deserialize)]
struct Stored {
    read_at: Instant,
    entitlements: Vec<RawEntitlement>,
    unread: Vec<Unread>,
    /// Absent from a file written before the hierarchy was read with the inventory; such a
    /// file is used, and read again at the next view to get the names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    places: Option<StoredPlaces>,
    /// The organizations searched. Absent from a file written before several could be; a
    /// file whose organizations are not the configured ones is used, and read again at the
    /// next view.
    #[serde(default)]
    organizations: Vec<NumericId>,
}

/// [`Places`] as saved: each parent's path keyed by its text (`folders/123`).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPlaces {
    Read(BTreeMap<String, String>),
    Failed(String),
}

impl StoredPlaces {
    fn of(places: &Places) -> Self {
        match places {
            Places::Read(paths) => Self::Read(
                paths
                    .iter()
                    .map(|(parent, path)| (parent.to_string(), path.clone()))
                    .collect(),
            ),
            Places::Failed(reason) => Self::Failed(reason.clone()),
        }
    }

    fn places(self) -> Places {
        match self {
            Self::Read(paths) => Places::Read(
                paths
                    .into_iter()
                    .filter_map(|(parent, path)| Some((Parent::parse(&parent).ok()?, path)))
                    .collect(),
            ),
            Self::Failed(reason) => Places::Failed(reason),
        }
    }
}

/// What the PAM page shows, and when each read is due.
#[derive(Debug, Clone)]
pub struct Reading {
    pub config: PamConfig,
    pub inventory: Observed<Inventory>,
    pub grants: BTreeMap<EntitlementName, Observed<Vec<Grant>>>,
    inventory_attempted: Option<Instant>,
    inventory_requested: bool,
    grants_attempted: Option<Instant>,
    viewed: Option<Instant>,
}

impl Reading {
    fn new(config: PamConfig) -> Self {
        Self {
            config,
            inventory: Observed::default(),
            grants: BTreeMap::new(),
            inventory_attempted: None,
            inventory_requested: false,
            grants_attempted: None,
            viewed: None,
        }
    }

    fn watched(&self, now: Instant) -> bool {
        self.viewed
            .is_some_and(|at| now.duration_since(at) < VIEWED_WINDOW)
    }

    /// Due while watched, when asked for, or when there is no list at all and no try within
    /// [`INVENTORY_RETRY`]. A list, saved or read, is kept until the user asks again.
    fn inventory_due(&self, now: Instant) -> bool {
        if !self.watched(now) {
            return false;
        }
        if self.inventory_requested {
            return true;
        }
        self.inventory.good.is_none()
            && self
                .inventory_attempted
                .is_none_or(|tried| now.duration_since(tried) >= INVENTORY_RETRY)
    }

    /// Due while watched, once there is an inventory, every [`GRANTS_EVERY`].
    fn grants_due(&self, now: Instant) -> bool {
        self.watched(now)
            && self.inventory.good.is_some()
            && self
                .grants_attempted
                .is_none_or(|at| now.duration_since(at) >= GRANTS_EVERY)
    }

    /// The rows of the page as of `now`: those not kept for emergencies, then those that are.
    pub fn rows(&self, now: Instant) -> Rows<'_> {
        let Some(inventory) = &self.inventory.good else {
            return Rows::default();
        };
        let places = &inventory.value.places;
        let (mut held, rest): (Vec<Row<'_>>, Vec<Row<'_>>) = inventory
            .value
            .entitlements
            .iter()
            .map(|entitlement| Row::of(entitlement, self.grants.get(&entitlement.name), now))
            .partition(|row| matches!(row.status, Status::Live(_)));
        held.sort_by(Row::by_end);
        let (regular, break_glass): (Vec<Row<'_>>, Vec<Row<'_>>) = rest
            .into_iter()
            .partition(|row| !self.config.break_glass.contains(&row.entitlement.name));
        Rows {
            held,
            regular: groups(regular, places),
            break_glass: groups(break_glass, places),
        }
    }
}

/// The page's rows, in the order shown: every live grant first, kept for emergencies or not;
/// then the rest by where they sit.
#[derive(Debug, Default)]
pub struct Rows<'a> {
    pub held: Vec<Row<'a>>,
    pub regular: Vec<Group<'a>>,
    pub break_glass: Vec<Group<'a>>,
}

/// The entitlements of one parent, by id.
#[derive(Debug)]
pub struct Group<'a> {
    pub parent: &'a Parent,
    /// Where the parent sits, when its path is known.
    pub place: Option<&'a str>,
    pub rows: Vec<Row<'a>>,
}

/// `rows` by parent, the parents in the order of their paths (a parent with none by its id),
/// each parent's rows by id.
fn groups<'a>(rows: Vec<Row<'a>>, places: &'a Places) -> Vec<Group<'a>> {
    let mut by_parent: BTreeMap<&'a Parent, Vec<Row<'a>>> = BTreeMap::new();
    for row in rows {
        by_parent
            .entry(&row.entitlement.name.parent)
            .or_default()
            .push(row);
    }
    let mut groups: Vec<Group<'a>> = by_parent
        .into_iter()
        .map(|(parent, mut rows)| {
            rows.sort_by(|a, b| a.entitlement.name.id.cmp(&b.entitlement.name.id));
            Group {
                parent,
                place: places.of(parent),
                rows,
            }
        })
        .collect();
    groups.sort_by_cached_key(|group| {
        (
            group
                .place
                .map_or_else(|| group.parent.to_string(), str::to_lowercase),
            group.parent.clone(),
        )
    });
    groups
}

/// One entitlement, with what its grants say.
#[derive(Debug)]
pub struct Row<'a> {
    pub entitlement: &'a Entitlement,
    pub status: Status<'a>,
    /// Its grants' read: `None` before the first.
    pub grants: Option<&'a Observed<Vec<Grant>>>,
}

/// What a row's grants come to.
#[derive(Debug, PartialEq, Eq)]
pub enum Status<'a> {
    /// Pending or in effect: no new request.
    Live(&'a Grant),
    /// In a state not settled here: shown, and a request left to PAM.
    Unclear(&'a Grant),
    /// The latest grant ended within [`RECENT_WINDOW`].
    Over(&'a Grant),
    /// None held, none recent.
    Free,
    /// Its grants have not been read.
    NotRead,
}

impl Status<'_> {
    /// Whether the row offers a request.
    pub const fn requestable(&self) -> bool {
        match self {
            Self::Unclear(_) | Self::Over(_) | Self::Free => true,
            Self::Live(_) | Self::NotRead => false,
        }
    }
}

impl<'a> Row<'a> {
    fn of(
        entitlement: &'a Entitlement,
        grants: Option<&'a Observed<Vec<Grant>>>,
        now: Instant,
    ) -> Self {
        let status = match grants.and_then(|g| g.good.as_ref()) {
            None => Status::NotRead,
            Some(read) => status(&read.value, now),
        };
        Self {
            entitlement,
            status,
            grants,
        }
    }

    /// Live grants first, the soonest to end first and pending ones after them; then the rest
    /// by name.
    /// Held grants: the soonest to end first, then the pending ones, then by name.
    fn by_end(a: &Self, b: &Self) -> std::cmp::Ordering {
        let end = |row: &Self| match &row.status {
            Status::Live(grant) => grant.ends_at(),
            Status::Unclear(_) | Status::Over(_) | Status::Free | Status::NotRead => None,
        };
        match (end(a), end(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.entitlement.name.cmp(&b.entitlement.name))
    }
}

/// The grant that decides a row: the newest live one, else the newest unclear one, else the
/// newest that ended within [`RECENT_WINDOW`].
fn status(grants: &[Grant], now: Instant) -> Status<'_> {
    let newest = |phase: Phase| {
        grants
            .iter()
            .filter(|grant| grant.state.phase() == phase)
            .max_by_key(|grant| grant.created)
    };
    if let Some(grant) = newest(Phase::Live) {
        return Status::Live(grant);
    }
    if let Some(grant) = newest(Phase::Unclear) {
        return Status::Unclear(grant);
    }
    match newest(Phase::Over) {
        Some(grant) if now.duration_since(grant.created) < RECENT_WINDOW => Status::Over(grant),
        Some(_) | None => Status::Free,
    }
}

/// Why a request was not made. Each but [`RequestError::Create`] is refused before PAM is
/// asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("no [pam] section is configured")]
    NotConfigured,
    #[error("{0} is not among the entitlements you may request")]
    NotRequestable(EntitlementName),
    #[error("{name} offers no grant of {secs} s")]
    Length { name: EntitlementName, secs: u64 },
    #[error("{0} needs a justification")]
    JustificationRequired(EntitlementName),
    #[error("{name} already has a grant: {state}")]
    AlreadyLive {
        name: EntitlementName,
        state: String,
    },
    /// The fresh read of its grants, made just before asking, failed.
    #[error("could not check {name}'s grants before asking: {error}")]
    Check {
        name: EntitlementName,
        error: PamError,
    },
    #[error("PAM did not grant {name}: {error}")]
    Create {
        name: EntitlementName,
        error: PamError,
    },
}

/// The PAM state of a running tool.
#[derive(Debug)]
pub struct Pam {
    reading: Mutex<Reading>,
    path: PathBuf,
    /// Held across a request's check and create, so two clicks cannot make two grants.
    requesting: tokio::sync::Mutex<()>,
}

impl Pam {
    /// The state, with the inventory saved under `state_dir` when there is one.
    pub fn load(config: PamConfig, state_dir: &Path) -> Self {
        let path = state_dir.join(FILE_NAME);
        let mut reading = Reading::new(config);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(stored) => {
                    reading.inventory_requested = stored.places.is_none()
                        || !reading
                            .config
                            .organizations
                            .iter()
                            .eq(&stored.organizations);
                    let places = stored.places.map_or_else(
                        || Places::Failed("saved before names were read".to_owned()),
                        StoredPlaces::places,
                    );
                    reading.inventory.good = Some(Read {
                        at: stored.read_at,
                        value: Inventory::of(stored.entitlements, stored.unread, places),
                    });
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "the saved PAM inventory is unreadable; it is read again");
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the saved PAM inventory cannot be read; it is read again");
            }
        }
        Self {
            reading: Mutex::new(reading),
            path,
            requesting: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Reading> {
        self.reading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> Reading {
        self.lock().clone()
    }

    /// The PAM page was viewed: the reads continue for [`VIEWED_WINDOW`].
    pub fn viewed(&self, now: Instant) {
        self.lock().viewed = Some(now);
    }

    /// Read the inventory again at the next pass, whatever its age.
    pub fn request_inventory(&self) {
        self.lock().inventory_requested = true;
    }

    /// One pass of the reader: the inventory when due, then the grants when due. Returns
    /// whether anything was read.
    pub async fn read_due(&self, runner: &Arc<dyn CommandRunner>, now: Instant) -> bool {
        let inventory = self.read_inventory_if_due(runner, now).await;
        let grants = self.read_grants_if_due(runner, now).await;
        inventory || grants
    }

    async fn read_inventory_if_due(&self, runner: &Arc<dyn CommandRunner>, now: Instant) -> bool {
        let config = {
            let mut reading = self.lock();
            if !reading.inventory_due(now) {
                return false;
            }
            reading.inventory_attempted = Some(now);
            reading.inventory_requested = false;
            reading.config.clone()
        };
        let outcome = read_inventory(runner, &config).await;
        let at = Instant::now();
        match &outcome {
            Ok(inventory) => {
                self.save(inventory, &config.organizations, at);
                for unread in &inventory.unread {
                    tracing::warn!(parent = %unread.parent, reason = %unread.reason, "a parent's entitlements could not be read");
                }
                tracing::info!(
                    entitlements = inventory.entitlements.len(),
                    unread = inventory.unread.len(),
                    "the PAM inventory was read"
                );
            }
            Err(error) => tracing::warn!(%error, "the PAM inventory could not be read"),
        }
        let mut reading = self.lock();
        reading.inventory.record(outcome, at);
        // A grant read is due at once against the new list.
        reading.grants_attempted = None;
        true
    }

    async fn read_grants_if_due(&self, runner: &Arc<dyn CommandRunner>, now: Instant) -> bool {
        let names: Vec<EntitlementName> = {
            let mut reading = self.lock();
            if !reading.grants_due(now) {
                return false;
            }
            reading.grants_attempted = Some(now);
            reading
                .inventory
                .good
                .iter()
                .flat_map(|good| &good.value.entitlements)
                .map(|e| e.name.clone())
                .collect()
        };
        let answers = gcloud::each(runner, names.clone(), |runner, name| async move {
            let outcome = gcloud::grants(&*runner, &name).await;
            (name, outcome)
        })
        .await;
        let at = Instant::now();
        let mut reading = self.lock();
        reading.grants.retain(|name, _| names.contains(name));
        for (name, outcome) in answers {
            if let Err(error) = &outcome {
                tracing::warn!(%name, %error, "a PAM entitlement's grants could not be read");
            }
            reading.grants.entry(name).or_default().record(outcome, at);
        }
        true
    }

    fn save(&self, inventory: &Inventory, organizations: &Organizations, at: Instant) {
        let stored = Stored {
            read_at: at,
            entitlements: inventory.raw.clone(),
            unread: inventory.unread.clone(),
            places: Some(StoredPlaces::of(&inventory.places)),
            organizations: organizations.iter().cloned().collect(),
        };
        let written = serde_json::to_vec_pretty(&stored)
            .map_err(|e| e.to_string())
            .and_then(|bytes| write_private(&self.path, &bytes).map_err(|e| e.to_string()));
        if let Err(error) = written {
            tracing::warn!(path = %self.path.display(), %error, "the PAM inventory was read and could not be saved");
        }
    }

    /// Asks PAM for a grant on `name` for `secs`, after checking, against the inventory and a
    /// fresh read of its grants, that PAM would not refuse it and that it would not be a
    /// second one.
    pub async fn request(
        &self,
        runner: &dyn CommandRunner,
        name: &EntitlementName,
        secs: u64,
        justification: Option<Justified>,
    ) -> Result<Grant, RequestError> {
        let _one_at_a_time = self.requesting.lock().await;
        let length = self.admissible(name, secs, justification.as_ref())?;
        let check = gcloud::grants(runner, name).await;
        let at = Instant::now();
        let held = match &check {
            Ok(grants) => match status(grants, at) {
                Status::Live(grant) => Some(grant.state.to_string()),
                Status::Unclear(_) | Status::Over(_) | Status::Free | Status::NotRead => None,
            },
            Err(_) => None,
        };
        self.record_grants(name, check.clone(), at);
        if let Err(error) = check {
            return Err(RequestError::Check {
                name: name.clone(),
                error,
            });
        }
        if let Some(state) = held {
            return Err(RequestError::AlreadyLive {
                name: name.clone(),
                state,
            });
        }
        let created = gcloud::create(runner, name, length, justification.as_ref())
            .await
            .map_err(|error| RequestError::Create {
                name: name.clone(),
                error,
            })?;
        tracing::info!(%name, secs, state = %created.state, "a PAM grant was requested");
        let mut reading = self.lock();
        let observed = reading.grants.entry(name.clone()).or_default();
        let mut grants = observed
            .good
            .as_ref()
            .map_or_else(Vec::new, |good| good.value.clone());
        grants.push(created.clone());
        observed.record(Ok(grants), Instant::now());
        Ok(created)
    }

    /// The length a request for `secs` on `name` asks for, when the inventory offers it and
    /// the reason PAM needs is there.
    fn admissible(
        &self,
        name: &EntitlementName,
        secs: u64,
        justification: Option<&Justified>,
    ) -> Result<GrantLength, RequestError> {
        let reading = self.lock();
        let entitlement = reading
            .inventory
            .good
            .as_ref()
            .and_then(|good| good.value.get(name))
            .ok_or_else(|| RequestError::NotRequestable(name.clone()))?;
        let length = entitlement
            .length(secs)
            .ok_or_else(|| RequestError::Length {
                name: name.clone(),
                secs,
            })?;
        match (&entitlement.justification, justification) {
            (Justification::Required, None) => {
                Err(RequestError::JustificationRequired(name.clone()))
            }
            (Justification::Required | Justification::Optional | Justification::Other(_), _) => {
                Ok(length)
            }
        }
    }

    fn record_grants(
        &self,
        name: &EntitlementName,
        outcome: Result<Vec<Grant>, PamError>,
        at: Instant,
    ) {
        self.lock()
            .grants
            .entry(name.clone())
            .or_default()
            .record(outcome, at);
    }
}

/// Each organization's parents, then each parent's requestable entitlements, and beside them
/// where each parent sits. An organization or a parent that cannot be read is named in
/// [`Inventory::unread`], and a hierarchy that cannot be read leaves ids alone; only when no
/// organization's parents can be searched does the whole read fail.
async fn read_inventory(
    runner: &Arc<dyn CommandRunner>,
    config: &PamConfig,
) -> Result<Inventory, PamError> {
    let organizations: Vec<NumericId> = config.organizations.iter().cloned().collect();
    let quota = &config.quota_project;
    let searched = gcloud::each(runner, organizations.clone(), |runner, organization| {
        let quota = quota.clone();
        async move {
            let outcome = gcloud::parents(&*runner, &organization, &quota).await;
            (organization, outcome)
        }
    })
    .await;
    let mut parents = BTreeSet::new();
    let mut unread = Vec::new();
    let mut failure = None;
    let mut answered = false;
    for (organization, outcome) in searched {
        match outcome {
            Ok(found) => {
                answered = true;
                parents.extend(found);
            }
            Err(error) => {
                unread.push(Unread {
                    parent: Parent::Organization(organization).to_string(),
                    reason: error.to_string(),
                });
                failure.get_or_insert(error);
            }
        }
    }
    if !answered {
        return Err(failure.unwrap_or_else(|| {
            PamError::Malformed("no organization's search answered".to_owned())
        }));
    }
    let hierarchies = gcloud::each(runner, organizations, |runner, organization| {
        let quota = quota.clone();
        async move {
            let outcome = gcloud::hierarchy(&*runner, &organization, &quota).await;
            (organization, outcome)
        }
    });
    let entitlements = gcloud::each(
        runner,
        parents.iter().cloned().collect::<Vec<Parent>>(),
        |runner, parent| async move {
            let outcome = gcloud::requestable(&*runner, &parent).await;
            (parent, outcome)
        },
    );
    let (hierarchies, answers) = tokio::join!(hierarchies, entitlements);
    let mut nodes = Vec::new();
    let mut unnamed = Vec::new();
    for (organization, outcome) in hierarchies {
        match outcome {
            Ok(found) => nodes.extend(found),
            Err(error) => {
                let organization = Parent::Organization(organization);
                tracing::warn!(%organization, %error, "an organization's folder names could not be read");
                unnamed.push(format!("{organization}: {error}"));
            }
        }
    }
    let places = if unnamed.is_empty() {
        Places::Read(hierarchy::paths(
            &nodes,
            &parents,
            config.organizations.naming(),
        ))
    } else {
        Places::Failed(unnamed.join("; "))
    };
    let mut raw = Vec::new();
    for (parent, outcome) in answers {
        match outcome {
            Ok(found) => raw.extend(found),
            Err(error) => unread.push(Unread {
                parent: parent.to_string(),
                reason: error.to_string(),
            }),
        }
    }
    unread.sort_by(|a, b| a.parent.cmp(&b.parent));
    Ok(Inventory::of(raw, unread, places))
}

#[cfg(test)]
impl Reading {
    /// A reading of gcloud answers, for the page's tests: `entitlements` (the JSON
    /// `entitlements search` prints) read at `at`, each `(name, JSON)` of `grants` read then
    /// too, and `failure` as the latest inventory read's.
    pub(crate) fn of_answers(
        config: PamConfig,
        entitlements: &str,
        at: Instant,
        grants: &[(&str, &str)],
        failure: Option<PamError>,
        places: Places,
    ) -> Self {
        let mut reading = Self::new(config);
        let raw: Vec<RawEntitlement> = serde_json::from_str(entitlements).unwrap();
        reading.inventory.good = Some(Read {
            at,
            value: Inventory::of(raw, Vec::new(), places),
        });
        reading.inventory.failure = failure.map(|value| Read { at, value });
        for (name, answer) in grants {
            let grants = grant::parse_grants(answer.as_bytes()).unwrap();
            reading
                .grants
                .entry(EntitlementName::parse(name).unwrap())
                .or_default()
                .record(Ok(grants), at);
        }
        reading
    }
}

#[cfg(test)]
mod tests;
