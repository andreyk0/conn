//! The poller: reads every source on its own cadence and keeps the [`Model`] current.
//!
//! - every 2 s: the event log from the kept offset, every followed transcript from its kept
//!   offset, and a rebuild of the model;
//! - every 10 s: the supervisor's listing, the registry with `/proc`, `seen.json` and the
//!   coordination ledger;
//! - every 30 s: the projects directory, for transcripts that appeared; each new file is
//!   scanned once on a blocking thread, one scan job at a time.
//!
//! Two further tasks refresh what is slow. One reads `gh` states (two at a time, only the ones
//! that are due); it reads the model to learn what to ask and wakes the poller when it has an
//! answer. The other asks `acli` for the user's open work items when the cache says they are
//! due: the cache's age, or the human's request. A third reads the plan's rate-limit windows
//! once a minute while a page was viewed in the last hour, more slowly while the endpoint
//! refuses reads as too frequent.
//!
//! A source that fails is recorded with its time in the model's [`Health`](crate::model::Health)
//! and logged once at `warn`; nothing is only logged.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::SignedDuration;
use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant as Tick, MissedTickBehavior};

use super::builder::{Inputs, build, live_pull_requests};
use super::links::{DerivedMemo, LinkEnv};
use super::notify::Notifier;
use super::seen::{SEEN_FILE, SeenError, SeenStore, TicketMark};
use super::sources::{
    EventLogState, GoodListing, ListingForModel, RegisteredEntry, RegistrySnapshot, Source,
    TailFault, Tracked, TranscriptState, Transcripts,
};
use crate::config::Config;
use crate::events::parse::parse;
use crate::events::{EventLog, HookEvent, LOG_FILE, LogLineError, ReadBatch, fold_into};
use crate::gh::{Cache, PullRequestRef, pr_state};
use crate::gist::{self, Input as GistInput};
use crate::jira::resolution::ClosedTickets;
use crate::jira::{self, Jql};
use crate::links::{Extractor, LinksError};
use crate::model::{Model, ScanProgress, SourceState};
use crate::pam::gcloud::Justified;
use crate::pam::ids::EntitlementName;
use crate::pam::{Pam, RequestError};
use crate::reviews::classify::Verdict;
use crate::reviews::github::CommitSha;
use crate::reviews::journal::{Entry as JournalEntry, Journal, JournalError};
use crate::reviews::slack::{self, ChannelId};
use crate::reviews::watch::Watch;
use crate::reviews::{self, ApproveError, ReviewsConfig, classify, read_queue};
use crate::saves::system::SystemState;
use crate::saves::{self, SaveId, SavesError};
use crate::sessions::launch::LaunchArgs;
use crate::sessions::listing::{ListedSession, listing};
use crate::sessions::proc::SystemProc;
use crate::sessions::registry::{Liveness, RegistryError, registry};
use crate::sessions::transcripts::{SkipReason, TranscriptScan, transcripts};
use crate::transcript::{Summary, Tailer, TranscriptError, fold_event, scan};
use crate::types::{Instant, JiraKey, JiraPrefix, LinkTargets, SessionId};
use crate::usage;
use crate::util::runner::{CommandRunner, TokioRunner};

/// How often the event log and the followed transcripts are read and the model rebuilt.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How often the listing, the registry and the seen store are read.
const LISTING_EVERY: Duration = Duration::from_secs(10);
/// How often the projects directory is walked for new transcripts.
const DISCOVERY_EVERY: Duration = Duration::from_secs(30);
/// How often the `gh` refresher looks for pull requests that are due.
const GH_EVERY: Duration = Duration::from_secs(5);
/// How often the work items cache is asked whether a read is due; the cache decides, by its own
/// age and the human's request, so this bounds how late a request is served when nothing wakes
/// the task.
const JIRA_EVERY: Duration = Duration::from_secs(5);
/// How many `gh` calls run at once: `gh` is a network call and GitHub rate-limits a token, so
/// a handful of cards must not become a burst.
const GH_CONCURRENCY: usize = 2;
/// A transcript untouched for this long is no longer followed; it is read again if it grows.
const FOLLOW_WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Links(#[from] LinksError),
}

/// The work items were asked for where the config names no Jira.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no Jira is configured, so there are no work items to read")]
pub struct NoJira;

/// What the tasks share.
struct Shared {
    config: Config,
    extractor: Extractor,
    targets: LinkTargets,
    memo: DerivedMemo,
    seen: Mutex<Source<SeenStore>>,
    gh: Mutex<Cache>,
    /// `None` where there is no Jira.
    jira: Option<WorkItems>,
    /// The human asked for the work items to be read again now.
    jira_wake: Notify,
    /// `None` where the config listens to no Slack channel for review requests.
    reviews: Option<ReviewQueue>,
    /// A page view made a review read due.
    reviews_wake: Notify,
    gists: Mutex<gist::Store>,
    usage: Mutex<usage::Reading>,
    /// The coordination ledger, read with the seen store.
    ledger: Mutex<Source<crate::ledger::store::Loaded>>,
    /// The Jira links of the map's tickets and the blocked work items, and the children of the
    /// epics; read only where Jira is configured.
    jira_relations: Mutex<jira::Relations>,
    /// The saved sets of open sessions, and the tracker behind the automatic one.
    saves: Mutex<saves::Saves>,
    /// Pull requests the human asked to read again before their entry is due.
    refresh_requested: Mutex<HashSet<PullRequestRef>>,
    /// The tickets read as closed, which end the waits that name them; saved for the hook.
    closed_tickets: Mutex<ClosedTickets>,
    /// When each ticket a wait names was last read for its resolution.
    resolution_reads: Mutex<jira::reads::PerTicket<()>>,
    /// `None` where no `[pam]` is configured.
    pam: Option<Pam>,
    /// The PAM page was viewed, or its inventory asked for again.
    pam_wake: Notify,
    /// Something changed that the next rebuild should show.
    wake: Notify,
}

/// The user's open work items, with the last good list and the last failure, and the
/// prefixes whose keys they are read under.
struct WorkItems {
    cache: Mutex<jira::Cache>,
    prefixes: Vec<JiraPrefix>,
    /// The comments on the listed items.
    comments: Mutex<jira::comments::Reads>,
}

impl WorkItems {
    fn lock(&self) -> std::sync::MutexGuard<'_, jira::Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn comments(&self) -> std::sync::MutexGuard<'_, jira::comments::Reads> {
        self.comments.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The keys of the items in the last good list, as Jira wrote them.
    fn open_keys(&self) -> Vec<String> {
        match self.lock().reading(Instant::now()) {
            jira::Reading::Value { observed, .. } => observed
                .items
                .iter()
                .map(|item| item.key.to_string())
                .collect(),
            jira::Reading::Unread | jira::Reading::Failure(_) => Vec::new(),
        }
    }
}

/// The review requests heard in the configured channels, when to read them again, and the
/// journal of what was approved.
struct ReviewQueue {
    config: ReviewsConfig,
    watch: Mutex<Watch>,
    journal: Mutex<Journal>,
}

impl ReviewQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Watch> {
        self.watch.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why an approval from a card did not happen, or did and was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum ReviewActionError {
    #[error("no [reviews] section is configured")]
    NotConfigured,
    #[error(transparent)]
    Approve(#[from] ApproveError),
    /// GitHub has the approval; the journal does not.
    #[error("approved on GitHub, but not recorded: {0}")]
    Unrecorded(#[from] JournalError),
    #[error("no approval of {pr} at {head} is in the journal")]
    NotApproved { pr: String, head: String },
}

/// Why "Save now" saved nothing.
#[derive(Debug, thiserror::Error)]
pub enum SaveNowError {
    /// The sessions are not all read yet, so a save could leave running ones out.
    #[error("conn is still reading the sessions; save again in a moment")]
    StillReading,
    #[error(transparent)]
    Saves(#[from] SavesError),
}

/// The emoji a review approval leaves on the message that asked.
const APPROVED_EMOJI: &str = "thumbsup";

impl Shared {
    fn new(config: Config) -> Result<Self, StartError> {
        let jira = config.jira.as_ref().map(|jira| WorkItems {
            cache: Mutex::new(jira::Cache::load(
                &config.paths.state_dir,
                &jira.prefixes,
                Instant::now(),
            )),
            prefixes: jira.prefixes.clone(),
            comments: Mutex::new(jira::comments::Reads::load(&config.paths.state_dir)),
        });
        let reviews = config.reviews.clone().map(|reviews| {
            let (journal, problem) = Journal::load(&config.paths.state_dir);
            if let Some(problem) = problem {
                tracing::warn!(%problem, "the approvals journal starts empty");
            }
            ReviewQueue {
                config: reviews,
                watch: Mutex::new(Watch::default()),
                journal: Mutex::new(journal),
            }
        });
        let gists = Mutex::new(gist::Store::load(&config.paths.state_dir, Instant::now()));
        let boot = saves::BootId::read(std::path::Path::new(saves::system::BOOT_ID_PATH))
            .inspect_err(|error| {
                tracing::warn!(%error, "the boot id cannot be read; the automatic save is kept as if after a reboot");
            })
            .ok();
        let saves = Mutex::new(saves::Saves::load(&config.paths.state_dir, boot));
        let pam = config
            .pam
            .clone()
            .map(|pam| Pam::load(pam, &config.paths.state_dir));
        let closed_tickets = Mutex::new(ClosedTickets::load(&config.paths.state_dir));
        Ok(Self {
            extractor: Extractor::from_config(&config)?,
            targets: config.link_targets(),
            config,
            memo: DerivedMemo::default(),
            seen: Mutex::new(Source::NeverRead),
            gh: Mutex::new(Cache::default()),
            jira,
            jira_wake: Notify::new(),
            reviews,
            reviews_wake: Notify::new(),
            gists,
            usage: Mutex::new(usage::Reading::default()),
            ledger: Mutex::new(Source::NeverRead),
            jira_relations: Mutex::new(jira::Relations::default()),
            saves,
            refresh_requested: Mutex::new(HashSet::new()),
            closed_tickets,
            resolution_reads: Mutex::new(jira::reads::PerTicket::new(RESOLUTION_READ_EVERY)),
            pam,
            pam_wake: Notify::new(),
            wake: Notify::new(),
        })
    }

    fn gists(&self) -> std::sync::MutexGuard<'_, gist::Store> {
        self.gists.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn usage(&self) -> std::sync::MutexGuard<'_, usage::Reading> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn saves(&self) -> std::sync::MutexGuard<'_, saves::Saves> {
        self.saves.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn seen_path(&self) -> PathBuf {
        self.config.paths.state_dir.join(SEEN_FILE)
    }
}

/// The running tool's state, for the pages: the model, and the only ways to change what the
/// user owns.
pub struct Handle {
    model: Arc<RwLock<Model>>,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Handle {
    pub fn model(&self) -> &Arc<RwLock<Model>> {
        &self.model
    }

    /// Asks for the pull request to be read again at the next refresh pass, whatever its
    /// entry's age. A pull request that is on no card by then is forgotten.
    pub fn request_refresh(&self, pr: PullRequestRef) {
        self.shared
            .refresh_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pr);
    }

    /// Records that a page was viewed: review requests are read and gists written only while
    /// someone looks.
    pub fn page_viewed(&self) {
        self.shared.gists().viewed(Instant::now());
        let due = self
            .shared
            .reviews
            .as_ref()
            .is_some_and(|reviews| reviews.lock().viewed(Instant::now()));
        if due {
            self.shared.reviews_wake.notify_one();
        }
    }

    /// Approves `pr` at `head` as the user, records it, and leaves the 👍 on the message that
    /// asked in the background, recording that too. The card leaves the queue at once.
    pub async fn approve_review(
        &self,
        runner: Arc<dyn CommandRunner>,
        pr: &PullRequestRef,
        head: &CommitSha,
    ) -> Result<(), ReviewActionError> {
        let reviews = self
            .shared
            .reviews
            .as_ref()
            .ok_or(ReviewActionError::NotConfigured)?;
        let (request, me, verdict) = {
            let watch = reviews.lock();
            let (request, me) = watch
                .request(pr)
                .ok_or_else(|| ApproveError::NotInQueue(pr.clone()))?;
            let verdict = watch.verdict(pr, head).cloned();
            (request, me, verdict)
        };
        let outcome = reviews::approve(&*runner, &request, head, &me).await;
        if matches!(outcome, Ok(()) | Err(ApproveError::NoLongerWaits(_))) {
            reviews.lock().resolved(pr);
            self.shared.wake.notify_one();
        }
        outcome?;
        let (verdict, flags) = verdict_words(verdict.as_ref());
        let recorded = reviews.journal().append(JournalEntry::Approved {
            at: Instant::now(),
            pr: pr.to_string(),
            head: head.as_str().to_owned(),
            title: request.facts.title.clone(),
            author: request.facts.author.clone(),
            requester: request.message.author.clone(),
            channel: request.message.channel.as_str().to_owned(),
            ts: request.message.ts.clone(),
            verdict,
            flags,
        });
        // GitHub has the approval either way, so the 👍 follows it either way.
        self.react(
            runner,
            pr.to_string(),
            head.as_str().to_owned(),
            request.message.channel,
            request.message.ts,
        );
        Ok(recorded?)
    }

    /// Tries the 👍 of a recorded approval again.
    pub fn retry_reaction(
        &self,
        runner: Arc<dyn CommandRunner>,
        pr: &str,
        head: &str,
    ) -> Result<(), ReviewActionError> {
        let reviews = self
            .shared
            .reviews
            .as_ref()
            .ok_or(ReviewActionError::NotConfigured)?;
        let message = reviews
            .journal()
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                JournalEntry::Approved {
                    pr: approved,
                    head: at,
                    channel,
                    ts,
                    ..
                } if approved == pr && at == head => Some((channel.clone(), ts.clone())),
                JournalEntry::Approved { .. }
                | JournalEntry::Reacted { .. }
                | JournalEntry::ReactionFailed { .. } => None,
            });
        let not_approved = || ReviewActionError::NotApproved {
            pr: pr.to_owned(),
            head: head.to_owned(),
        };
        let (channel, ts) = message.ok_or_else(not_approved)?;
        let channel = ChannelId::parse(&channel).map_err(|_| not_approved())?;
        self.react(runner, pr.to_owned(), head.to_owned(), channel, ts);
        Ok(())
    }

    /// Leaves the 👍 in the background and records how it went.
    fn react(
        &self,
        runner: Arc<dyn CommandRunner>,
        pr: String,
        head: String,
        channel: ChannelId,
        ts: String,
    ) {
        let shared = Arc::clone(&self.shared);
        drop(tokio::spawn(async move {
            let Some(reviews) = shared.reviews.as_ref() else {
                return;
            };
            let outcome = slack::react(&*runner, &channel, &ts, APPROVED_EMOJI).await;
            let at = Instant::now();
            let entry = match outcome {
                Ok(()) => JournalEntry::Reacted { at, pr, head },
                Err(error) => {
                    tracing::warn!(%error, "the approval's reaction could not be added");
                    JournalEntry::ReactionFailed {
                        at,
                        pr,
                        head,
                        reason: error.to_string(),
                    }
                }
            };
            if let Err(error) = reviews.journal().append(entry) {
                tracing::warn!(%error, "the approval's reaction was not recorded");
            }
            shared.wake.notify_one();
        }));
    }

    /// Records that the PAM page was viewed: its reads run only while someone looks.
    pub fn pam_viewed(&self) {
        if let Some(pam) = &self.shared.pam {
            pam.viewed(Instant::now());
            self.shared.pam_wake.notify_one();
        }
    }

    /// Asks for the PAM inventory to be read again now, whatever its age.
    pub fn request_pam_inventory(&self) -> Result<(), RequestError> {
        self.shared
            .pam
            .as_ref()
            .ok_or(RequestError::NotConfigured)?
            .request_inventory();
        self.shared.pam_wake.notify_one();
        Ok(())
    }

    /// Asks PAM for a grant on `name` for `secs`; see [`Pam::request`].
    pub async fn request_grant(
        &self,
        runner: &dyn CommandRunner,
        name: &EntitlementName,
        secs: u64,
        justification: Option<Justified>,
    ) -> Result<(), RequestError> {
        let pam = self
            .shared
            .pam
            .as_ref()
            .ok_or(RequestError::NotConfigured)?;
        let outcome = pam.request(runner, name, secs, justification).await;
        // A refusal after the fresh check changed what the page shows too.
        self.shared.wake.notify_one();
        outcome.map(|_grant| ())
    }

    /// Asks for the work items to be read again now, whatever the age of the last read. The
    /// read happens within moments; the page shows its outcome when it lands.
    pub fn request_work_items_refresh(&self) -> Result<(), NoJira> {
        self.shared
            .jira
            .as_ref()
            .ok_or(NoJira)?
            .lock()
            .request_refresh();
        self.shared.jira_wake.notify_one();
        Ok(())
    }

    /// Marks the card of the session whose `since` is `card_since` seen, and saves.
    pub fn mark_seen(
        &self,
        session: SessionId,
        card_since: Instant,
        tickets: Vec<TicketMark>,
    ) -> Result<(), SeenError> {
        self.edit_seen(|seen| {
            seen.mark_seen(session, card_since);
            for mark in tickets {
                seen.mark_ticket_seen(mark.ticket, mark.through);
            }
        })
    }

    /// Keeps the sessions `model` shows open as a manual save.
    pub fn save_sessions(&self, model: &Model) -> Result<SaveId, SaveNowError> {
        let seen = saves::observation(model).ok_or(SaveNowError::StillReading)?;
        let sessions = saves::most_recent_first(seen.live);
        let id = self.shared.saves().save_now(sessions, Instant::now())?;
        self.shared.wake.notify_one();
        Ok(id)
    }

    /// Deletes the kept save `id`.
    pub fn delete_save(&self, id: SaveId) -> Result<(), SavesError> {
        self.shared.saves().delete(id)?;
        self.shared.wake.notify_one();
        Ok(())
    }

    pub fn pin(&self, session: SessionId) -> Result<(), SeenError> {
        self.edit_seen(|seen| seen.pin(session))
    }

    pub fn unpin(&self, session: &SessionId) -> Result<(), SeenError> {
        self.edit_seen(|seen| seen.unpin(session))
    }

    fn edit_seen(&self, change: impl FnOnce(&mut SeenStore)) -> Result<(), SeenError> {
        edit_store(
            &self.shared.seen,
            &self.shared.wake,
            SeenError::NotRead,
            |store| {
                change(store);
                Ok(())
            },
            |store| store.save(&self.shared.seen_path()),
        )
    }
}

/// One edit of a store the human owns: change a copy, save it, and only then put it in the
/// slot, all under the slot's lock so no reload sees the file older than the slot. Only a
/// store that was read can be saved over its file: an unreadable one is not replaced, and one
/// not read yet may hold marks this edit would erase.
fn edit_store<T: Clone, E>(
    slot: &Mutex<Source<T>>,
    wake: &Notify,
    not_read: impl FnOnce(String) -> E,
    change: impl FnOnce(&mut T) -> Result<(), E>,
    save: impl FnOnce(&T) -> Result<(), E>,
) -> Result<(), E> {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    let mut store = match &*slot {
        Source::Read { value, .. } => value.clone(),
        Source::Failed { reason, .. } => return Err(not_read(reason.clone())),
        Source::NeverRead => return Err(not_read("not read yet".to_owned())),
    };
    change(&mut store)?;
    save(&store)?;
    *slot = Source::Read {
        at: Instant::now(),
        value: store,
    };
    drop(slot);
    wake.notify_one();
    Ok(())
}

/// Starts the poller and its two helpers. The model is `unread` until the first poll lands.
pub fn start(config: Config) -> Result<Handle, StartError> {
    start_with(config, Arc::new(TokioRunner))
}

/// [`start`], with the runner of external programs supplied.
pub fn start_with(config: Config, runner: Arc<dyn CommandRunner>) -> Result<Handle, StartError> {
    let shared = Arc::new(Shared::new(config)?);
    let model = Arc::new(RwLock::new(Model::unread(Instant::now())));
    let tasks = vec![
        tokio::spawn(run(
            Poller::new(Arc::clone(&shared), Arc::clone(&runner)),
            Arc::clone(&model),
        )),
        tokio::spawn(refresh_pull_requests(
            Arc::clone(&shared),
            Arc::clone(&model),
            Arc::clone(&runner),
        )),
        tokio::spawn(refresh_work_items(Arc::clone(&shared), Arc::clone(&runner))),
        tokio::spawn(write_gists(
            Arc::clone(&shared),
            Arc::clone(&model),
            Arc::clone(&runner),
        )),
        tokio::spawn(read_usage(Arc::clone(&shared), Arc::clone(&runner))),
        tokio::spawn(read_jira_relations(
            Arc::clone(&shared),
            Arc::clone(&model),
            Arc::clone(&runner),
        )),
        tokio::spawn(read_jira_comments(Arc::clone(&shared), Arc::clone(&runner))),
        tokio::spawn(read_pam(Arc::clone(&shared), Arc::clone(&runner))),
        tokio::spawn(read_ticket_resolutions(
            Arc::clone(&shared),
            Arc::clone(&runner),
        )),
        tokio::spawn(read_reviews(Arc::clone(&shared), runner)),
    ];
    Ok(Handle {
        model,
        shared,
        tasks,
    })
}

/// How long [`read_once`] waits for the first scan of every transcript.
const READ_ONCE_PATIENCE: Duration = Duration::from_secs(300);

/// One read of every source, for a command that shows the model once and exits: the model
/// after the first scan of every transcript has finished, or after [`READ_ONCE_PATIENCE`]
/// with the scan still marked in progress.
pub async fn read_once(
    config: Config,
    runner: Arc<dyn CommandRunner>,
) -> Result<Model, StartError> {
    let mut poller = Poller::new(Arc::new(Shared::new(config)?), runner);
    let mut model = poller.poll(Due::EVERYTHING, Instant::now()).await;
    let settle = async {
        while matches!(
            model.health.transcripts.progress,
            ScanProgress::InProgress { .. }
        ) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            model = poller.poll(Due::ONLY_FAST, Instant::now()).await;
        }
        model
    };
    // On timeout the model so far is not returned: the future owning it is dropped.
    Ok(
        match tokio::time::timeout(READ_ONCE_PATIENCE, settle).await {
            Ok(settled) => settled,
            Err(_elapsed) => poller.rebuild(Instant::now()),
        },
    )
}

/// Whether a source is read on this poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    Now,
    Later,
}

/// Which of the slower sources this poll reads; the event log and the transcripts are read
/// on every poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub listing: Refresh,
    pub discovery: Refresh,
}

impl Due {
    pub const EVERYTHING: Self = Self {
        listing: Refresh::Now,
        discovery: Refresh::Now,
    };
    pub const ONLY_FAST: Self = Self {
        listing: Refresh::Later,
        discovery: Refresh::Later,
    };
}

/// When a slower source is next read.
struct Cadence {
    every: Duration,
    next: Tick,
}

impl Cadence {
    fn starting_now(every: Duration) -> Self {
        Self {
            every,
            next: Tick::now(),
        }
    }

    fn due(&mut self) -> Refresh {
        let now = Tick::now();
        if now >= self.next {
            self.next = now + self.every;
            Refresh::Now
        } else {
            Refresh::Later
        }
    }
}

async fn run(mut poller: Poller, model: Arc<RwLock<Model>>) {
    let mut ticker = tokio::time::interval(POLL_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut listing = Cadence::starting_now(LISTING_EVERY);
    let mut discovery = Cadence::starting_now(DISCOVERY_EVERY);
    // The poller never ends: it is a supervisor of the model, stopped by dropping the Handle.
    loop {
        let woken = tokio::select! {
            _ = ticker.tick() => false,
            () = poller.shared.wake.notified() => true,
        };
        let now = Instant::now();
        let mut next = if woken {
            poller.rebuild(now)
        } else {
            let due = Due {
                listing: listing.due(),
                discovery: discovery.due(),
            };
            poller.poll(due, now).await
        };
        poller.notifier.observe(&next, &*poller.runner, now).await;
        next.health.notifications = poller.notifier.state().clone();
        autosave(&poller.shared, &*poller.runner, &next, now).await;
        next.saves = poller.shared.saves().view().clone();
        *model.write().await = next;
    }
}

/// Brings the automatic save up to date with the sessions `model` shows open: a session that
/// opened joins at once, one stopped for a minute leaves only while systemd says the machine
/// is up. A model not built from a complete read changes nothing.
async fn autosave(shared: &Shared, runner: &dyn CommandRunner, model: &Model, now: Instant) {
    let Some(seen) = saves::observation(model) else {
        return;
    };
    let (changed, leaving_due) = {
        let mut saves = shared.saves();
        let changed = saves.tracker().observe(seen, now);
        (changed, saves.tracker().leaving_due(now))
    };
    let left = if leaving_due {
        match saves::system::state(runner).await {
            SystemState::Up => shared.saves().tracker().leave(now),
            state @ (SystemState::Other(_) | SystemState::Unreadable { .. }) => {
                tracing::info!(
                    ?state,
                    "sessions stopped while the machine is not up; the automatic save keeps them"
                );
                false
            }
        }
    } else {
        false
    };
    let mut saves = shared.saves();
    if (changed || left || saves.auto_behind())
        && let Err(error) = saves.write_auto(now)
    {
        tracing::warn!(%error, "the automatic save could not be written");
    }
}

/// A transcript's first scan, finished.
struct ScanDone {
    id: SessionId,
    result: Result<Summary, TranscriptError>,
}

/// Owns the sources that are read incrementally, and reads them.
pub struct Poller {
    shared: Arc<Shared>,
    runner: Arc<dyn CommandRunner>,
    log: EventLog,
    log_offset: u64,
    events: EventLogState,
    /// The latest attempt, whatever it was.
    listing: Source<Vec<ListedSession>>,
    /// The latest attempt that succeeded, which covers a failed one for a while.
    listing_last_good: Option<GoodListing>,
    registry: Source<RegistrySnapshot>,
    transcripts: Transcripts,
    scan_results: mpsc::UnboundedReceiver<ScanDone>,
    scan_sender: mpsc::UnboundedSender<ScanDone>,
    /// Files whose first scan has been handed to a blocking thread and not come back.
    scans_outstanding: usize,
    /// Files discovered while a scan job was running; started when it ends.
    scan_backlog: Vec<(SessionId, PathBuf)>,
    notifier: Notifier,
}

impl Poller {
    fn new(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) -> Self {
        let (scan_sender, scan_results) = mpsc::unbounded_channel();
        Self {
            log: EventLog::open(shared.config.paths.state_dir.join(LOG_FILE)),
            shared,
            runner,
            log_offset: 0,
            events: EventLogState::unread(),
            listing: Source::NeverRead,
            listing_last_good: None,
            registry: Source::NeverRead,
            transcripts: Transcripts::default(),
            scan_results,
            scan_sender,
            scans_outstanding: 0,
            scan_backlog: Vec::new(),
            notifier: Notifier::default(),
        }
    }

    /// One poll: reads what is `due` and what is always read, and returns the model they make.
    pub async fn poll(&mut self, due: Due, now: Instant) -> Model {
        self.integrate_scans();
        self.read_event_log(now).await;
        if due.listing == Refresh::Now {
            self.read_listing(now).await;
            self.read_registry(now).await;
            self.reload_seen(now);
            self.reload_ledger(now);
        }
        if due.discovery == Refresh::Now {
            self.discover(now).await;
        }
        self.follow_transcripts(now).await;
        self.rebuild(now)
    }

    /// The model from what has been read so far, without reading anything.
    pub fn rebuild(&self, now: Instant) -> Model {
        let shared = &self.shared;
        let gh = shared.gh.lock().unwrap_or_else(PoisonError::into_inner);
        let work_items = shared.jira.as_ref().map(|jira| jira.lock().reading(now));
        let reviews = shared
            .reviews
            .as_ref()
            .map(|reviews| reviews.lock().clone());
        let approvals = shared
            .reviews
            .as_ref()
            .map(|reviews| reviews.journal().entries().to_vec());
        let seen = shared.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let gists = shared.gists();
        let usage = shared.usage();
        let ledger = shared.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        let jira_relations = shared
            .jira_relations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let jira_comments = shared.jira.as_ref().map(WorkItems::comments);
        let listing = ListingForModel::at(&self.listing, self.listing_last_good.as_ref(), now);
        let saves = shared.saves();
        let pam = shared.pam.as_ref().map(Pam::snapshot);
        build(&Inputs {
            now,
            listing: &listing.used,
            listing_health: listing.health.clone(),
            registry: &self.registry,
            event_log: &self.events,
            transcripts: &self.transcripts,
            gh: &gh,
            work_items: work_items.as_ref(),
            reviews: reviews.as_ref(),
            approvals: approvals.as_deref(),
            gists: &gists,
            usage: &usage,
            ledger: &ledger,
            jira_relations: shared.jira.as_ref().map(|_| &*jira_relations),
            jira_comments: jira_comments.as_deref(),
            home: Some(&shared.config.paths.home),
            followups: shared.config.followups.as_ref(),
            saves: saves.view(),
            pam: pam.as_ref(),
            seen: &seen,
            links: LinkEnv {
                extractor: &shared.extractor,
                targets: &shared.targets,
                memo: &shared.memo,
            },
        })
    }

    // --- the event log ---

    async fn read_event_log(&mut self, now: Instant) {
        let log = self.log.clone();
        let offset = self.log_offset;
        let read = tokio::task::spawn_blocking(move || log.read_from(offset)).await;
        let before = self.events.health.clone();
        match read {
            Ok(batch) => self.integrate_batch(batch, now),
            Err(error) => {
                self.events.health = SourceState::Failed {
                    at: now,
                    reason: format!("the log reader stopped: {error}"),
                };
            }
        }
        note_transition("event log", &before, &self.events.health);
    }

    fn integrate_batch(&mut self, batch: ReadBatch, now: Instant) {
        let mut hook_events: Vec<HookEvent> = Vec::new();
        let mut failure: Option<LogFault> = None;
        let mut shrunk = false;
        for item in batch.events {
            match item {
                Ok(raw) => match parse(&raw) {
                    Ok(event) => hook_events.push(event),
                    Err(error) => {
                        tracing::warn!(%error, "an event log entry is not a hook payload");
                        self.events.line_errors += 1;
                    }
                },
                Err(LogLineError::NotRegular { path, kind }) => {
                    failure = Some(LogFault::Failed(format!(
                        "{} is not a regular file: it is {kind}",
                        path.display()
                    )));
                }
                Err(LogLineError::Unreadable { path, source }) => {
                    failure = Some(if source.kind() == std::io::ErrorKind::NotFound {
                        LogFault::Absent(path)
                    } else {
                        LogFault::Failed(format!("cannot read {}: {source}", path.display()))
                    });
                }
                Err(LogLineError::Shrunk { len, offset, .. }) => {
                    shrunk = true;
                    failure = Some(LogFault::Failed(format!(
                        "the event log is {len} bytes, shorter than the {offset} already read; \
                         reading it again from the start"
                    )));
                }
                Err(
                    error @ (LogLineError::Malformed { .. }
                    | LogLineError::TooLong { .. }
                    | LogLineError::Truncated { .. }
                    | LogLineError::Unparsed { .. }
                    | LogLineError::Oversized { .. }),
                ) => {
                    tracing::warn!(%error, "an event log line carries no event");
                    self.events.line_errors += 1;
                }
            }
        }
        if shrunk {
            self.log_offset = 0;
            self.events.sessions.clear();
            self.events.line_errors = 0;
        } else if failure.is_none() {
            self.log_offset = batch.next_offset;
        }
        fold_into(&mut self.events.sessions, hook_events.into_iter());
        self.events.health = match failure {
            Some(LogFault::Absent(path)) => SourceState::NeverWritten { at: now, path },
            Some(LogFault::Failed(reason)) => SourceState::Failed { at: now, reason },
            None => SourceState::Ok { at: now },
        };
    }

    // --- the listing, the registry and the stores ---

    async fn read_listing(&mut self, now: Instant) {
        let before = self.listing.state();
        self.listing = match listing(&*self.runner).await {
            Ok(value) => {
                self.listing_last_good = Some(GoodListing {
                    at: now,
                    sessions: value.clone(),
                });
                Source::Read { at: now, value }
            }
            Err(error) => Source::Failed {
                at: now,
                reason: error.to_string(),
            },
        };
        note_transition("listing", &before, &self.listing.state());
    }

    async fn read_registry(&mut self, now: Instant) {
        let claude_dir = self.shared.config.paths.claude_dir.clone();
        let read = tokio::task::spawn_blocking(move || read_registry(&claude_dir)).await;
        let before = self.registry.state();
        self.registry = match read {
            Ok(Ok(value)) => Source::Read { at: now, value },
            Ok(Err(reason)) => Source::Failed { at: now, reason },
            Err(error) => Source::Failed {
                at: now,
                reason: format!("the registry reader stopped: {error}"),
            },
        };
        note_transition(
            "registry",
            &before,
            &super::builder::registry_state(&self.registry),
        );
    }

    /// Reads `seen.json` again, so a hand edit shows up. Edits made through the [`Handle`]
    /// save before they update the slot, and both hold its lock, so the file read here is
    /// never older than the slot it replaces.
    fn reload_seen(&self, now: Instant) {
        let shared = &self.shared;
        let mut seen = shared.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let before = seen.state();
        *seen = match SeenStore::load(&shared.seen_path()) {
            Ok(value) => Source::Read { at: now, value },
            Err(error) => Source::Failed {
                at: now,
                reason: error.to_string(),
            },
        };
        note_transition("seen.json", &before, &seen.state());
    }

    /// Reads the coordination ledger again; the sessions append to it themselves. A failed
    /// read keeps nothing: what the pages show of the ledger is what was just read, or its
    /// failure.
    fn reload_ledger(&self, now: Instant) {
        let path = self
            .shared
            .config
            .paths
            .state_dir
            .join(crate::ledger::LEDGER_FILE);
        let mut ledger = self
            .shared
            .ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let before = ledger.state();
        *ledger = match crate::ledger::store::read(&path) {
            Ok(value) => Source::Read {
                at: now,
                value: crate::ledger::store::Loaded {
                    ledger: value.ledger.knowing_closed(
                        self.shared
                            .closed_tickets
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone(),
                    ),
                    ..value
                },
            },
            Err(error) => Source::Failed {
                at: now,
                reason: error.to_string(),
            },
        };
        note_transition("ledger.jsonl", &before, &ledger.state());
    }

    // --- transcripts ---

    async fn discover(&mut self, now: Instant) {
        let claude_dir = self.shared.config.paths.claude_dir.clone();
        let found = tokio::task::spawn_blocking(move || transcripts(&claude_dir)).await;
        let before = self
            .transcripts
            .discovery
            .clone()
            .unwrap_or(SourceState::NeverRead);
        let state = match found {
            Ok(found) => self.integrate_discovery(found, now),
            Err(error) => SourceState::Failed {
                at: now,
                reason: format!("the discovery walk stopped: {error}"),
            },
        };
        note_transition("transcript discovery", &before, &state);
        self.transcripts.discovery = Some(state);
    }

    fn integrate_discovery(&mut self, found: TranscriptScan, now: Instant) -> SourceState {
        self.transcripts.skipped = found.skipped.len();
        let unreadable: Vec<String> = found
            .skipped
            .iter()
            .filter_map(|(path, reason)| match reason {
                SkipReason::Unreadable(why) => Some(format!("{}: {why}", path.display())),
                SkipReason::NotSessionIdName
                | SkipReason::NotRegularFile
                | SkipReason::NotProjectDir
                | SkipReason::NonUtf8Name => None,
            })
            .collect();
        let present: HashSet<SessionId> =
            found.files.iter().map(|f| f.session_id.clone()).collect();
        let mut to_scan: Vec<(SessionId, PathBuf)> = Vec::new();
        let mut walked: HashSet<SessionId> = HashSet::new();
        // Newest first, so the sessions most likely to be live are read first, and so the
        // newest of two files holding one session is the one followed.
        let mut files = found.files;
        files.sort_by_key(|f| std::cmp::Reverse(f.modified));
        for file in files {
            let id = file.session_id.clone();
            if !walked.insert(id.clone()) {
                tracing::warn!(path = %file.path.display(), session = %id, "a second transcript of one session is not followed; the newest is");
                continue;
            }
            match self.transcripts.tracked.get_mut(&id) {
                None => {
                    to_scan.push((id.clone(), file.path.clone()));
                    self.transcripts.tracked.insert(
                        id,
                        Tracked {
                            file,
                            state: TranscriptState::Queued,
                            tailer: None,
                            tail_fault: None,
                        },
                    );
                }
                Some(tracked) => {
                    // Claude Code moves a session's transcript into the worktree's project
                    // directory when the session enters a worktree; what was read came from
                    // the old file, so the new one is read from the start.
                    let moved = tracked.file.path != file.path;
                    let retry = moved || matches!(tracked.state, TranscriptState::Unreadable(_));
                    tracked.file = file;
                    if retry {
                        tracked.state = TranscriptState::Queued;
                        tracked.tailer = None;
                        tracked.tail_fault = None;
                        to_scan.push((id, tracked.file.path.clone()));
                    } else {
                        follow_if_recent(tracked, now);
                    }
                }
            }
        }
        // A directory that could not be walked proves nothing about what is gone from it.
        if unreadable.is_empty() {
            self.transcripts
                .tracked
                .retain(|id, _| present.contains(id));
        }
        self.queue_scans(to_scan);
        match unreadable.first() {
            None => SourceState::Ok { at: now },
            Some(first) => SourceState::Failed {
                at: now,
                reason: format!("{} entries unreadable, first: {first}", unreadable.len()),
            },
        }
    }

    /// Hands files to one blocking thread at a time; the rest wait in the backlog.
    fn queue_scans(&mut self, files: Vec<(SessionId, PathBuf)>) {
        self.scan_backlog.extend(files);
        if self.scans_outstanding > 0 || self.scan_backlog.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.scan_backlog);
        self.scans_outstanding = batch.len();
        let sender = self.scan_sender.clone();
        // The job's result is the messages it sends; a panic in it would leave files queued,
        // which the model shows as a scan that does not finish.
        drop(tokio::task::spawn_blocking(move || {
            for (id, path) in batch {
                let result = scan(&path);
                if sender.send(ScanDone { id, result }).is_err() {
                    return;
                }
            }
        }));
    }

    fn integrate_scans(&mut self) {
        while let Ok(done) = self.scan_results.try_recv() {
            self.scans_outstanding = self.scans_outstanding.saturating_sub(1);
            let Some(tracked) = self.transcripts.tracked.get_mut(&done.id) else {
                continue;
            };
            match done.result {
                Ok(summary) => {
                    tracked.state = TranscriptState::Scanned(Box::new(summary));
                    tracked.tail_fault = None;
                    follow_if_recent(tracked, jiff::Timestamp::now());
                }
                Err(TranscriptError::Unreadable { path, source }) => {
                    tracing::warn!(path = %path.display(), %source, "a transcript cannot be read");
                    tracked.state = TranscriptState::Unreadable(source.to_string());
                }
            }
        }
        if self.scans_outstanding == 0 && !self.scan_backlog.is_empty() {
            self.queue_scans(Vec::new());
        }
    }

    /// Reads what was appended to every followed transcript, on a blocking thread.
    async fn follow_transcripts(&mut self, now: Instant) {
        let jobs: Vec<TailJob> = self
            .transcripts
            .tracked
            .iter_mut()
            .filter_map(|(id, tracked)| {
                let tailer = tracked.tailer.take()?;
                match std::mem::replace(&mut tracked.state, TranscriptState::Queued) {
                    TranscriptState::Scanned(summary) => Some(TailJob {
                        id: id.clone(),
                        tailer,
                        summary,
                    }),
                    unscanned @ (TranscriptState::Queued | TranscriptState::Unreadable(_)) => {
                        tracked.state = unscanned;
                        tracked.tailer = Some(tailer);
                        None
                    }
                }
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let ids: Vec<SessionId> = jobs.iter().map(|j| j.id.clone()).collect();
        match tokio::task::spawn_blocking(move || {
            jobs.into_iter().map(|job| job.run(now)).collect::<Vec<_>>()
        })
        .await
        {
            Ok(done) => {
                for finished in done {
                    if let Some(tracked) = self.transcripts.tracked.get_mut(&finished.id) {
                        tracked.state = TranscriptState::Scanned(finished.summary);
                        tracked.tailer = Some(finished.tailer);
                        tracked.tail_fault = finished.fault;
                    }
                }
            }
            Err(error) => {
                // The summaries went with the thread: read them again.
                tracing::warn!(%error, "the transcript follower stopped; its transcripts are scanned again");
                let files: Vec<(SessionId, PathBuf)> = ids
                    .into_iter()
                    .filter_map(|id| {
                        let tracked = self.transcripts.tracked.get_mut(&id)?;
                        tracked.state = TranscriptState::Queued;
                        Some((id, tracked.file.path.clone()))
                    })
                    .collect();
                self.queue_scans(files);
            }
        }
    }
}

struct TailJob {
    id: SessionId,
    tailer: Tailer,
    summary: Box<Summary>,
}

struct TailDone {
    id: SessionId,
    tailer: Tailer,
    summary: Box<Summary>,
    /// The poll could not read the file; the summary may lack what was appended.
    fault: Option<TailFault>,
}

impl TailJob {
    fn run(mut self, now: Instant) -> TailDone {
        let fault = match self.tailer.poll() {
            Ok(events) => {
                for event in &events {
                    fold_event(&mut self.summary, event);
                }
                self.summary
                    .set_extent(self.tailer.consumed(), self.tailer.unterminated());
                None
            }
            Err(error) => {
                tracing::warn!(%error, "a followed transcript cannot be read");
                Some(TailFault {
                    at: now,
                    reason: error.to_string(),
                })
            }
        };
        TailDone {
            id: self.id,
            tailer: self.tailer,
            summary: self.summary,
            fault,
        }
    }
}

/// Follows the file from the end of what was scanned when it was touched within
/// [`FOLLOW_WINDOW`], and stops following one that went quiet.
fn follow_if_recent(tracked: &mut Tracked, now: Instant) {
    let recent = now.duration_since(tracked.file.modified) <= FOLLOW_WINDOW;
    match (&tracked.state, recent, tracked.tailer.is_some()) {
        (TranscriptState::Scanned(summary), true, false) => {
            tracked.tailer = Some(Tailer::starting_at(
                tracked.file.path.clone(),
                summary.bytes_read,
            ));
        }
        (_, false, true) => tracked.tailer = None,
        _ => {}
    }
}

/// The registry's entries with what `/proc` says about each; the directory unreadable is the
/// error, one bad file is a line in `errors`.
fn read_registry(claude_dir: &std::path::Path) -> Result<RegistrySnapshot, String> {
    let sessions_dir = claude_dir.join("sessions");
    let procs = SystemProc;
    let mut snapshot = RegistrySnapshot::default();
    for item in registry(claude_dir) {
        match item {
            Ok(entry) => {
                let liveness = Liveness::check(&entry, &procs);
                let launch = LaunchArgs::of_entry(&entry, &procs);
                snapshot.entries.push(RegisteredEntry {
                    entry,
                    liveness,
                    launch,
                });
            }
            Err(RegistryError::Unreadable { path, source }) if path == sessions_dir => {
                return Err(format!("cannot read {}: {source}", path.display()));
            }
            Err(error) => snapshot.errors.push(error.to_string()),
        }
    }
    Ok(snapshot)
}

/// Why the latest read of the event log was not clean.
enum LogFault {
    /// There is no file at the path: no hook has written it.
    Absent(PathBuf),
    /// The file is there and could not be read as a log.
    Failed(String),
}

/// Logs a source going from fine to failed, a changed failure, and a recovery: one line per
/// change, not one per poll.
fn note_transition(source: &str, before: &SourceState, after: &SourceState) {
    match (before, after) {
        (_, SourceState::Failed { reason, .. }) => {
            let same = matches!(before, SourceState::Failed { reason: r, .. } if r == reason);
            if !same {
                tracing::warn!(source, %reason, "source failed");
            }
        }
        (SourceState::Failed { .. } | SourceState::NeverWritten { .. }, SourceState::Ok { .. }) => {
            tracing::info!(source, "source recovered");
        }
        (_, SourceState::Ok { .. } | SourceState::NeverRead | SourceState::NeverWritten { .. }) => {
        }
    }
}

// --- the two helpers ---

/// Keeps the `gh` state of the pull requests on screen: the ones that are due, two at a time.
async fn refresh_pull_requests(
    shared: Arc<Shared>,
    model: Arc<RwLock<Model>>,
    runner: Arc<dyn CommandRunner>,
) {
    let mut ticker = tokio::time::interval(GH_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let permits = Arc::new(Semaphore::new(GH_CONCURRENCY));
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let wanted: Vec<PullRequestRef> = {
            let model = model.read().await;
            live_pull_requests(model.sessions.iter().flat_map(|v| &v.links), &model.inbox)
        };
        let now = Instant::now();
        let requested: HashSet<PullRequestRef> = {
            let mut requested = shared
                .refresh_requested
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            requested.retain(|pr| wanted.contains(pr));
            requested.clone()
        };
        let due: Vec<PullRequestRef> = {
            let cache = shared.gh.lock().unwrap_or_else(PoisonError::into_inner);
            due_now(wanted, &requested, &cache, now)
        };
        shared
            .refresh_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|pr| !due.contains(pr));
        if due.is_empty() {
            continue;
        }
        let mut jobs = JoinSet::new();
        for pr in due {
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                return;
            };
            let (shared, runner) = (Arc::clone(&shared), Arc::clone(&runner));
            jobs.spawn(async move {
                let outcome = pr_state(&*runner, &pr).await;
                shared
                    .gh
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record(&pr, outcome, Instant::now());
                drop(permit);
            });
        }
        while let Some(joined) = jobs.join_next().await {
            if let Err(error) = joined {
                tracing::warn!(%error, "a gh refresh stopped before recording its answer");
            }
        }
        shared.wake.notify_one();
    }
}

/// What a card's verdict said, as the journal records it: a word and the flags' reasons.
fn verdict_words(verdict: Option<&Verdict>) -> (String, Vec<String>) {
    match verdict {
        None => ("pending".to_owned(), Vec::new()),
        Some(Verdict::Routine) => ("routine".to_owned(), Vec::new()),
        Some(Verdict::Look(flags)) => (
            "look".to_owned(),
            flags
                .iter()
                .map(|flag| match &flag.file {
                    Some(file) => format!("{file}: {}", flag.why),
                    None => flag.why.clone(),
                })
                .collect(),
        ),
        Some(Verdict::Unclassified { reason }) => ("unclassified".to_owned(), vec![reason.clone()]),
    }
}

/// How often the review reader checks whether a read is due; a page view wakes it sooner.
const REVIEWS_CHECK_EVERY: Duration = Duration::from_mins(1);

/// Reads the review queue whenever its watch says one is due, and wakes the poller with what
/// came back. Does nothing where no channel is configured.
async fn read_reviews(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    let Some(reviews) = shared.reviews.as_ref() else {
        return;
    };
    let mut ticker = tokio::time::interval(REVIEWS_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // A supervisor loop: it ends when the Handle drops and aborts it. One read at a time.
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = shared.reviews_wake.notified() => {}
        }
        let now = Instant::now();
        {
            let mut watch = reviews.lock();
            if !watch.is_due(now) {
                continue;
            }
            watch.started(now);
        }
        let outcome = read_queue(&*runner, &reviews.config, &shared.extractor, now).await;
        match &outcome {
            Ok(queue) => {
                for problem in &queue.problems {
                    tracing::warn!(%problem, "part of the review queue could not be read");
                }
            }
            Err(error) => tracing::warn!(%error, "the review queue could not be read"),
        }
        reviews.lock().record(outcome, Instant::now());
        shared.wake.notify_one();
        // The cards show at once; each verdict joins its card as it lands.
        let unclassified = reviews.lock().unclassified();
        for request in unclassified {
            let verdict = classify::verdict(&*runner, &request.pr, &request.facts).await;
            reviews.lock().classified(&request, verdict);
            shared.wake.notify_one();
        }
    }
}

/// How often a ticket a wait names is read for its resolution. Waits last hours to days, and
/// each read is an `acli` call of several seconds.
const RESOLUTION_READ_EVERY: SignedDuration = SignedDuration::from_mins(5);

/// How often the resolution reader looks for a ticket whose read is due.
const RESOLUTION_CHECK_EVERY: Duration = Duration::from_secs(60);

/// Reads, one at a time, the resolution of each ticket a standing wait names, and keeps the
/// closed ones, saved for the hook; the next ledger reload ends their waits. Does nothing where
/// no Jira is configured. A failed read changes nothing: the wait stands.
async fn read_ticket_resolutions(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    if shared.jira.is_none() {
        return;
    }
    let mut ticker = tokio::time::interval(RESOLUTION_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let watched = match &*shared.ledger.lock().unwrap_or_else(PoisonError::into_inner) {
            Source::Read { value, .. } => value.ledger.watched_tickets(now),
            Source::Failed { .. } | Source::NeverRead => continue,
        };
        let due: Vec<JiraKey> = {
            let mut reads = shared
                .resolution_reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            reads.retain(&watched.iter().collect());
            watched
                .into_iter()
                .filter(|key| reads.is_due(key, now))
                .collect()
        };
        for key in due {
            let outcome = jira::resolution::fetch(&*runner, &key).await;
            let at = Instant::now();
            let changed = match &outcome {
                Ok(resolution) => shared
                    .closed_tickets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record(key.clone(), resolution.clone()),
                Err(error) => {
                    tracing::warn!(%key, %error, "a waited ticket's resolution could not be read");
                    false
                }
            };
            shared
                .resolution_reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(key.clone(), outcome.map(|_| ()), at);
            if changed {
                tracing::info!(%key, "a waited ticket's resolution changed");
                let saved = shared
                    .closed_tickets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .save(&shared.config.paths.state_dir);
                if let Err(error) = saved {
                    tracing::warn!(%error, "the closed tickets could not be saved");
                }
                shared.wake.notify_one();
            }
        }
    }
}

/// How often the PAM reader checks whether a read is due; a view of the page wakes it sooner.
const PAM_CHECK_EVERY: Duration = Duration::from_secs(15);

/// Reads the PAM inventory and grants whenever they are due, and wakes the poller with what
/// came back. Does nothing where no `[pam]` is configured. One pass at a time.
async fn read_pam(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    let Some(pam) = shared.pam.as_ref() else {
        return;
    };
    let mut ticker = tokio::time::interval(PAM_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = shared.pam_wake.notified() => {}
        }
        if pam.read_due(&runner, Instant::now()).await {
            shared.wake.notify_one();
        }
    }
}

/// How often the gist writer looks for cards that are due.
const GISTS_EVERY: Duration = Duration::from_secs(5);

/// How many gist runs go at once: each is a model call of a few seconds, and a page opened
/// after an idle hour may find a dozen cards due.
const GIST_CONCURRENCY: usize = 2;

/// Writes the gist of each card that is due while someone looks, and wakes the poller as each
/// lands.
async fn write_gists(
    shared: Arc<Shared>,
    model: Arc<RwLock<Model>>,
    runner: Arc<dyn CommandRunner>,
) {
    let mut ticker = tokio::time::interval(GISTS_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let permits = Arc::new(Semaphore::new(GIST_CONCURRENCY));
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let due: Vec<GistInput> = {
            let model = model.read().await;
            let store = shared.gists();
            if !store.is_watched(now) {
                continue;
            }
            gist::due(&model, &store, now)
        };
        let mut jobs = JoinSet::new();
        for input in due {
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                return;
            };
            let (shared, runner) = (Arc::clone(&shared), Arc::clone(&runner));
            jobs.spawn(async move {
                let outcome = gist::write(&*runner, &input).await;
                shared.gists().record(&input, outcome, Instant::now());
                shared.wake.notify_one();
                drop(permit);
            });
        }
        while let Some(joined) = jobs.join_next().await {
            if let Err(error) = joined {
                tracing::warn!(%error, "a gist run stopped before recording its answer");
            }
        }
    }
}

/// How often the usage reader checks whether a read is due; [`usage::READ_EVERY`] sets the
/// pace, this how late a read is after a page view ends an idle spell.
const USAGE_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Reads the plan's rate-limit windows whenever [`usage::Reading::is_due`] says so (once a
/// minute, slower after a refusal) while a page was viewed in the last hour, and wakes the
/// poller with what came back. One read at a time.
async fn read_usage(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    let mut ticker = tokio::time::interval(USAGE_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let credentials = shared.config.paths.claude_dir.join(usage::CREDENTIALS_FILE);
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let now = Instant::now();
        if !shared.gists().is_watched(now) {
            continue;
        }
        {
            let mut reading = shared.usage();
            if !reading.is_due(now) {
                continue;
            }
            reading.started(now);
        }
        let outcome = usage::fetch(&*runner, &credentials, now).await;
        if let Err(error) = &outcome {
            tracing::warn!(%error, "the plan usage could not be read");
        }
        shared.usage().record(outcome, Instant::now());
        shared.wake.notify_one();
    }
}

/// Reads the work items whenever the cache says one is due, and wakes the poller with what
/// came back. Does nothing where no Jira is configured.
async fn refresh_work_items(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    let Some(jira) = shared.jira.as_ref() else {
        return;
    };
    let mut ticker = tokio::time::interval(JIRA_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let query = Jql::default();
    // A supervisor loop: it ends when the Handle drops and aborts it. One read at a time:
    // the next is not asked before this one is filed, so a slow `acli` is never asked twice.
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = shared.jira_wake.notified() => {}
        }
        let due = jira.lock().is_due(Instant::now());
        if !due {
            continue;
        }
        let outcome = jira::fetch(&*runner, &query, &jira.prefixes).await;
        if let Err(error) = &outcome {
            tracing::warn!(%error, "the Jira work items could not be read");
        }
        let saved = jira.lock().record(outcome, Instant::now());
        if let Err(error) = saved {
            tracing::warn!(%error, "the Jira work items were read and could not be saved");
        }
        shared.wake.notify_one();
    }
}

/// How often the relations reader looks for tickets whose reads are due;
/// [`jira::links::READ_EVERY`] and [`jira::children::READ_EVERY`] set how often each is read.
const JIRA_RELATIONS_CHECK_EVERY: Duration = Duration::from_secs(60);

/// The most tickets of each kind read in one pass: the map, the linked work items and the
/// epics hold a handful each, and each read is one `acli` call, so a pass stays around a
/// minute even when every read falls due at once.
const JIRA_RELATIONS_PER_PASS: usize = 20;

/// The tickets one pass reads: links, then epics' children.
struct RelationsDue {
    links: Vec<JiraKey>,
    epics: Vec<JiraKey>,
}

/// Reads, while a page was viewed in the last hour and one ticket at a time: which of the
/// user's work items wait on another ticket, the links of those and of the map's tickets, and
/// the children of the open epics among the work items.
async fn read_jira_relations(
    shared: Arc<Shared>,
    model: Arc<RwLock<Model>>,
    runner: Arc<dyn CommandRunner>,
) {
    if shared.jira.is_none() {
        return;
    }
    let mut ticker = tokio::time::interval(JIRA_RELATIONS_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let relations = || {
        shared
            .jira_relations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    };
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let now = Instant::now();
        if !shared.gists().is_watched(now) {
            continue;
        }
        let linked_due = relations().linked.is_due(now);
        if linked_due {
            let outcome = jira::links::fetch_linked(&*runner).await;
            if let Err(error) = &outcome {
                tracing::warn!(%error, "the work items waiting on other tickets could not be read");
            }
            relations().linked.record(outcome, Instant::now());
        }
        let epics = open_epics(&shared, now);
        let due = {
            let model = model.read().await;
            let coordination = &model.coordination;
            let map = crate::map::deps::tickets(&coordination.ledger, &coordination.shared, now);
            relations_due(&mut relations(), map, &epics, now)
        };
        for key in due.links {
            let outcome = jira::links::fetch(&*runner, &key).await;
            if let Err(error) = &outcome {
                tracing::warn!(%error, %key, "a ticket's Jira links could not be read");
            }
            relations().links.record(key, outcome, Instant::now());
        }
        for epic in due.epics {
            let outcome = jira::children::fetch(&*runner, &epic).await;
            if let Err(error) = &outcome {
                tracing::warn!(%error, %epic, "an epic's children could not be read");
            }
            relations().children.record(epic, outcome, Instant::now());
        }
        shared.wake.notify_one();
    }
}

/// The epics among the work items as last read; none before a good read.
fn open_epics(shared: &Shared, now: Instant) -> Vec<JiraKey> {
    let Some(jira) = &shared.jira else {
        return Vec::new();
    };
    match jira.lock().reading(now) {
        jira::Reading::Value { observed, .. } => observed
            .items
            .iter()
            .filter(|item| item.level == jira::Level::Epic)
            .filter_map(|item| JiraKey::parse(&item.key.to_string()))
            .collect(),
        jira::Reading::Unread | jira::Reading::Failure(_) => Vec::new(),
    }
}

/// What is due of the links of the map's tickets and the linked work items, and of the
/// epics' children; a ticket in neither set is forgotten first, so what it last said stops
/// counting.
fn relations_due(
    relations: &mut jira::Relations,
    map: Vec<JiraKey>,
    epics: &[JiraKey],
    now: Instant,
) -> RelationsDue {
    let mut seen = HashSet::new();
    let with_links: Vec<JiraKey> = map
        .into_iter()
        .chain(relations.linked.keys().iter().cloned())
        .filter(|key| seen.insert(key.clone()))
        .collect();
    relations.links.retain(&with_links.iter().collect());
    relations.children.retain(&epics.iter().collect());
    RelationsDue {
        links: with_links
            .into_iter()
            .filter(|key| relations.links.is_due(key, now))
            .take(JIRA_RELATIONS_PER_PASS)
            .collect(),
        epics: epics
            .iter()
            .filter(|key| relations.children.is_due(key, now))
            .take(JIRA_RELATIONS_PER_PASS)
            .cloned()
            .collect(),
    }
}

/// How often the comment reader runs a pass; [`jira::comments::CHANGES_EVERY`] sets how often
/// a pass asks which items changed.
const JIRA_COMMENTS_CHECK_EVERY: Duration = Duration::from_secs(60);

/// The most tickets whose comments one pass reads. Each is one `acli` call of a second or two,
/// so a backfill of a full list (100 items) spreads over five passes instead of one burst.
const JIRA_COMMENTS_PER_PASS: usize = 20;

/// Reads the comments of the listed work items while a page was viewed in the last hour: a
/// pass asks which items changed since the last ask, then reads those and any never read.
async fn read_jira_comments(shared: Arc<Shared>, runner: Arc<dyn CommandRunner>) {
    let Some(jira) = shared.jira.as_ref() else {
        return;
    };
    let mut ticker = tokio::time::interval(JIRA_COMMENTS_CHECK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // A supervisor loop: it ends when the Handle drops and aborts it.
    loop {
        ticker.tick().await;
        let started = Instant::now();
        if !shared.gists().is_watched(started) {
            continue;
        }
        comments_pass(jira, &*runner, started).await;
        shared.wake.notify_one();
    }
}

/// One pass of the comment reader, started at `started`: ask which listed items changed when
/// an ask is due, read up to [`JIRA_COMMENTS_PER_PASS`] of the due ones, and save.
async fn comments_pass(jira: &WorkItems, runner: &dyn CommandRunner, started: Instant) {
    use jira::comments::{self, Ask};
    let open = jira.open_keys();
    if open.is_empty() {
        return;
    }
    let window = match jira.comments().ask(started) {
        Ask::Not => None,
        Ask::Within(window) => Some(Some(window)),
        Ask::Everything => Some(None),
    };
    if let Some(window) = window {
        match comments::changed(runner, window).await {
            Ok(changed) => jira.comments().asked(started, changed, &open),
            Err(error) => {
                tracing::warn!(%error, "the changed Jira items could not be asked for");
                jira.comments().failed(Instant::now(), &error);
            }
        }
    }
    let (due, me) = {
        let reads = jira.comments();
        (
            reads.to_read(&open, JIRA_COMMENTS_PER_PASS),
            reads.me().cloned(),
        )
    };
    if let Some(me) = me {
        for key in due {
            match comments::fetch(runner, &key, &me).await {
                Ok(viewed) => jira.comments().record(key, viewed, Instant::now()),
                Err(error) => {
                    tracing::warn!(%error, %key, "a ticket's comments could not be read");
                    jira.comments().failed(Instant::now(), &error);
                }
            }
        }
    } else if !due.is_empty() {
        tracing::warn!(
            due = due.len(),
            "no ask has named the user's account yet; comments wait for one"
        );
    }
    if let Err(error) = jira.comments().keep_and_save(&open) {
        tracing::warn!(%error, "the Jira comment reads could not be saved");
    }
}

/// The pull requests to ask `gh` about now: those the human asked to read again, and those
/// whose entry is due.
fn due_now(
    wanted: Vec<PullRequestRef>,
    requested: &HashSet<PullRequestRef>,
    cache: &Cache,
    now: Instant,
) -> Vec<PullRequestRef> {
    wanted
        .into_iter()
        .filter(|pr| requested.contains(pr) || cache.is_due(pr, now))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The poller driven by hand against a scratch Claude directory, a scratch state
    //! directory and a scripted `claude agents`: the model is read after each poll, so what
    //! shows between the first poll and the end of the scan is what a page would show.

    use std::io::Write;
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::model::ScanProgress;
    use crate::testutil::at;
    use crate::types::{Activity, ListingStatus, PendingCard};
    use crate::util::runner::{Exit, FakeRunner, Output};

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    /// Links are due for the map's tickets and the linked work items, each once; children for
    /// the epics; a read of a ticket in neither set is forgotten, so a link removed in Jira (the
    /// item leaves the linked list) stops marking it as waiting; a fresh read is not due.
    /// Mutation: skip either `retain`, read only the map's tickets, or drop the dedupe.
    #[test]
    fn relations_due_reads_the_map_and_linked_items_and_forgets_the_rest() {
        let key = |text: &str| JiraKey::parse(text).unwrap();
        let now = at("2026-11-17T12:00:00Z");
        let mut relations = jira::Relations::default();
        relations
            .linked
            .record(Ok(vec![key("ABC-2"), key("ABC-1")]), now);
        let gone = jira::links::Dependency {
            blocker: key("ABC-9"),
            blocked: key("ABC-8"),
        };
        relations.links.record(key("ABC-8"), Ok(vec![gone]), now);
        relations.links.record(key("ABC-2"), Ok(vec![]), now);
        let progress = jira::children::Progress::count(std::iter::empty());
        relations.children.record(key("ABC-70"), Ok(progress), now);
        relations.children.record(key("ABC-71"), Ok(progress), now);

        let due = relations_due(
            &mut relations,
            vec![key("ABC-1"), key("ABC-3")],
            &[key("ABC-70"), key("ABC-72")],
            now,
        );
        let texts = |keys: &[JiraKey]| keys.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(texts(&due.links), ["ABC-1", "ABC-3"]);
        assert_eq!(texts(&due.epics), ["ABC-72"]);
        assert_eq!(relations.links.dependencies(), []);
        assert!(relations.links.get(&key("ABC-2")).is_some());
        assert!(relations.children.get(&key("ABC-71")).is_none());
        assert!(relations.children.get(&key("ABC-70")).is_some());
    }

    /// A scratch directory laid out as a Claude Code home and a state directory.
    struct Scratch(crate::testutil::Scratch);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = crate::testutil::Scratch::new(&format!("poller-{label}"));
            std::fs::create_dir_all(dir.join("claude/projects/-home-dev-proj")).unwrap();
            std::fs::create_dir_all(dir.join("state")).unwrap();
            Self(dir)
        }

        fn transcript(&self) -> PathBuf {
            self.0
                .join(format!("claude/projects/-home-dev-proj/{ID}.jsonl"))
        }

        fn events(&self) -> PathBuf {
            self.0.join("state").join(LOG_FILE)
        }

        fn config(&self) -> Config {
            let text = format!(
                "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n[slack]\nhost = \"example.slack.com\"\n[paths]\nclaude_dir = \"{0}/claude\"\nstate_dir = \"{0}/state\"\n",
                self.0.display()
            );
            Config::parse(&text, Path::new("/cfg.toml"), Path::new("/home/dev")).unwrap()
        }
    }

    fn line(kind: &str, time: &str, extra: &serde_json::Value) -> String {
        let mut record = json!({"type": kind, "timestamp": time, "sessionId": ID,
            "cwd": "/home/dev/proj", "isSidechain": false});
        if let (Some(base), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        format!("{record}\n")
    }

    fn prompt(time: &str, text: &str) -> String {
        line(
            "user",
            time,
            &json!({"origin": {"kind": "human"}, "message": {"role": "user", "content": text}}),
        )
    }

    fn ended(time: &str) -> String {
        line(
            "system",
            time,
            &json!({"subtype": "turn_duration", "durationMs": 1}),
        )
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn listing_reply() -> Output {
        let listing = json!([{"pid": 4242, "cwd": "/home/dev/proj", "kind": "interactive",
            "startedAt": 1_761_645_600_000_u64, "sessionId": ID, "status": "idle"}]);
        Output {
            exit: Exit::Success,
            stdout: listing.to_string().into_bytes(),
            stderr: String::new(),
        }
    }

    async fn settle(poller: &mut Poller, now: Instant) -> Model {
        for _ in 0..200 {
            let model = poller.poll(Due::ONLY_FAST, now).await;
            if model.health.transcripts.progress == ScanProgress::Complete {
                return model;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        poller.rebuild(now)
    }

    fn new_poller(scratch: &Scratch, runner: FakeRunner) -> Poller {
        Poller::new(
            Arc::new(Shared::new(scratch.config()).unwrap()),
            Arc::new(runner),
        )
    }

    /// A log that is absent is `NeverWritten`; one that is there and cannot be read as a log
    /// (a directory in its place, a file without read permission) is `Failed`.
    /// Mutation: classify by the error's text rather than its kind (the permission case goes
    /// `NeverWritten` or the missing case `Failed`), or map every `Unreadable` to `NeverWritten`.
    #[tokio::test]
    async fn an_absent_event_log_is_never_written_and_an_unreadable_one_failed() {
        use std::os::unix::fs::PermissionsExt;

        let now = at("2026-10-28T12:00:00Z");
        let health_with = async |label: &str, prepare: &dyn Fn(&Path)| {
            let scratch = Scratch::new(label);
            prepare(&scratch.events());
            let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
            poller.poll(Due::EVERYTHING, now).await.health.event_log
        };

        assert!(matches!(
            health_with("log-absent", &|_| {}).await,
            SourceState::NeverWritten { .. }
        ));
        assert!(matches!(
            health_with("log-dir", &|path| std::fs::create_dir(path).unwrap()).await,
            SourceState::Failed { .. }
        ));
        let locked = health_with("log-locked", &|path| {
            std::fs::write(path, "").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        })
        .await;
        assert!(
            matches!(locked, SourceState::Failed { ref reason, .. } if reason.contains("cannot read")),
            "{locked:?}"
        );
    }

    /// A followed transcript that cannot be read makes its session `Unknown`, and the model
    /// says so twice more: a fault in health with its reason and time, and the unknown count.
    /// Mutation: drop `tail_fault` on a failed poll (the session stays `Idle`), leave
    /// `follow_faults` at its empty value, or count only `Unknown` for unreadable first scans.
    #[tokio::test]
    async fn a_followed_transcript_that_cannot_be_read_is_unknown_and_counted() {
        let scratch = Scratch::new("tailfault");
        std::fs::write(
            scratch.transcript(),
            format!(
                "{}{}",
                prompt("2026-10-28T10:00:00Z", "go"),
                ended("2026-10-28T10:01:00Z")
            ),
        )
        .unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
        let id = SessionId::parse(ID).unwrap();
        let now = at("2026-10-28T12:00:00Z");
        poller.poll(Due::EVERYTHING, now).await;
        let model = settle(&mut poller, now).await;
        assert!(matches!(
            model.session(&id).unwrap().session.activity,
            Activity::Idle { .. }
        ));
        assert_eq!(model.health.follow_faults.transcripts, 0);
        assert_eq!(model.unknown_sessions, 0);

        std::fs::remove_file(scratch.transcript()).unwrap();
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert!(matches!(
            &model.session(&id).unwrap().session.activity,
            Activity::Unknown { reason } if reason.as_str().contains("could not be followed")
        ));
        assert_eq!(model.health.follow_faults.transcripts, 1);
        let fault = model.health.follow_faults.latest.as_ref().unwrap();
        assert_eq!(fault.at, now);
        assert_ne!(fault.reason, "");
        assert_eq!(model.unknown_sessions, 1);
    }

    /// The path from nothing read to a card, to a typed prompt, to a hook event, to a failed
    /// listing. Mutation: do not re-read the listing on `Due::Now`, keep the event-log offset
    /// at 0 (the second Stop is folded twice and `line_errors` is untouched, but the first
    /// poll would re-read the file and the offset assertion fails), or skip `integrate_scans`.
    #[tokio::test]
    async fn the_model_follows_the_sources_and_shows_each_failure() {
        let scratch = Scratch::new("follow");
        std::fs::write(
            scratch.transcript(),
            format!(
                "{}{}",
                prompt("2026-10-28T10:00:00Z", "go, see ABC-5"),
                ended("2026-10-28T10:01:00Z")
            ),
        )
        .unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
        let id = SessionId::parse(ID).unwrap();
        let now = at("2026-10-28T12:00:00Z");

        // First poll: everything is read except the transcript, whose scan is still running.
        let model = poller.poll(Due::EVERYTHING, now).await;
        assert!(
            matches!(
                model.health.transcripts.progress,
                ScanProgress::InProgress { .. }
            ),
            "{:?}",
            model.health.transcripts
        );
        assert_eq!(
            model.inbox.len(),
            0,
            "an empty inbox while scanning is not calm"
        );
        assert!(matches!(
            model.session(&id).map(|v| &v.session.activity),
            Some(Activity::Unknown { reason }) if reason.as_str().contains("not been scanned")
        ));
        assert_eq!(
            model.health.event_log,
            SourceState::NeverWritten {
                at: now,
                path: scratch.events(),
            }
        );
        assert!(
            matches!(model.health.registry, SourceState::Failed { .. }),
            "no sessions directory"
        );
        assert!(matches!(model.health.listing, SourceState::Ok { .. }));

        // The scan lands: the finished turn is a card, with its link.
        let model = settle(&mut poller, now).await;
        let view = model.session(&id).unwrap();
        assert_eq!(
            view.session.activity,
            Activity::Idle {
                ended_at: at("2026-10-28T10:01:00Z"),
                listing: ListingStatus::Seen
            }
        );
        assert_eq!(model.inbox.len(), 1);
        assert_eq!(
            view.links.first().map(|l| l.link.to_string()),
            Some("ABC-5".to_owned())
        );

        // The human types: the transcript tail moves the session to Running.
        append(
            &scratch.transcript(),
            &prompt("2026-10-28T11:00:00Z", "next"),
        );
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert_eq!(
            model.session(&id).unwrap().session.activity,
            Activity::Running {
                since: at("2026-10-28T11:00:00Z")
            }
        );
        assert_eq!(model.inbox.len(), 0);

        // A Stop hook arrives before the transcript has the turn end: the card is back, from
        // the hook's own text, and the log is read from its offset afterwards, not again.
        let stop = |time: &str, text: &str| {
            let payload = json!({"session_id": ID, "hook_event_name": "Stop",
                "background_tasks": [], "last_assistant_message": text});
            format!("{}\n", json!({"received_at": time, "payload": payload}))
        };
        append(
            &scratch.events(),
            &stop("2026-10-28T11:05:00Z", "from the hook"),
        );
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert!(matches!(model.health.event_log, SourceState::Ok { .. }));
        assert!(matches!(
            model.inbox.first().map(|e| &e.card),
            Some(PendingCard::Turn { turn, .. }) if turn.text.as_str() == "from the hook"
        ));
        let size = std::fs::metadata(scratch.events()).unwrap().len();
        assert_eq!(poller.log_offset, size);
        append(&scratch.events(), "not json\n");
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert_eq!(model.health.event_log_line_errors, 1);
        assert_eq!(poller.log_offset, size + 9);

        // The listing is read again, and the script has no answer left: it fails, visibly,
        // and, long after the last good read, the session is unknown rather than quietly
        // something else.
        let later = at("2026-10-28T12:05:00Z");
        let model = poller.poll(Due::EVERYTHING, later).await;
        assert!(matches!(model.health.listing, SourceState::Failed { .. }));
        assert!(matches!(
            model.session(&id).unwrap().session.activity,
            Activity::Unknown { ref reason } if reason.as_str().contains("listing")
        ));
    }

    /// A listing that fails within 60 s of the last good one keeps the sessions classified and
    /// the card in the inbox, with the failure and the age of the listing in the health; past
    /// 60 s the sessions are `Unknown` and the health still shows the failure.
    /// Mutation: drop `listing_last_good` (the covered poll goes `Unknown`), widen the window
    /// to `<= 600` (the late poll keeps `Idle`), or report `health.listing` from `used`
    /// (the covered poll says `Ok`).
    #[tokio::test]
    async fn a_failed_listing_covers_for_sixty_seconds_and_then_does_not() {
        let scratch = Scratch::new("cover");
        std::fs::write(
            scratch.transcript(),
            format!(
                "{}{}",
                prompt("2026-10-28T10:00:00Z", "go"),
                ended("2026-10-28T10:01:00Z")
            ),
        )
        .unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
        let id = SessionId::parse(ID).unwrap();
        let read = at("2026-10-28T12:00:00Z");
        poller.poll(Due::EVERYTHING, read).await;
        let model = settle(&mut poller, read).await;
        assert_eq!(model.inbox.len(), 1);

        // 59 s later the script is empty, so the listing fails.
        let covered = at("2026-10-28T12:00:59Z");
        let model = poller.poll(Due::EVERYTHING, covered).await;
        assert!(
            matches!(&model.health.listing, SourceState::Failed { at, reason }
                if *at == covered && reason.as_str().contains("59 s ago")),
            "{:?}",
            model.health.listing
        );
        assert_eq!(
            model.session(&id).unwrap().session.activity,
            Activity::Idle {
                ended_at: at("2026-10-28T10:01:00Z"),
                listing: ListingStatus::Seen
            }
        );
        assert_eq!(model.inbox.len(), 1);

        let past = at("2026-10-28T12:01:01Z");
        let model = poller.poll(Due::EVERYTHING, past).await;
        assert!(matches!(&model.health.listing, SourceState::Failed { at, .. } if *at == past));
        assert!(matches!(
            model.session(&id).unwrap().session.activity,
            Activity::Unknown { .. }
        ));
        assert_eq!(model.inbox.len(), 0);
    }

    /// A pull request whose entry is fresh is left alone unless the human asked for it again;
    /// one never asked is always due, and a request is kept until a pass takes it.
    /// Mutation: drop `requested.contains(pr)` from `due_now` (the forced row is missing), or
    /// make `request_refresh` a no-op (the handle row is empty).
    #[tokio::test]
    async fn a_requested_pull_request_is_due_before_its_entry_is() {
        let pr = |n| PullRequestRef::new("example-org", "example-repo", n).unwrap();
        let state = crate::gh::parse_pr_state(
            include_bytes!("../../tests/fixtures/gh/open_passing.json"),
            &pr(42),
        )
        .unwrap();
        let read_at = at("2026-10-28T12:00:00Z");
        let mut cache = Cache::default();
        cache.record(&pr(42), Ok(crate::gh::Answer::Found(state)), read_at);
        let soon = at("2026-10-28T12:00:10Z");
        let wanted = || vec![pr(42), pr(43)];

        let none = HashSet::new();
        assert_eq!(due_now(wanted(), &none, &cache, soon), [pr(43)]);
        let forced: HashSet<_> = [pr(42)].into();
        assert_eq!(due_now(wanted(), &forced, &cache, soon), [pr(42), pr(43)]);

        let scratch = Scratch::new("refresh-request");
        let handle = start_with(scratch.config(), Arc::new(FakeRunner::default())).unwrap();
        handle.request_refresh(pr(42));
        let kept = handle.shared.refresh_requested.lock().unwrap().clone();
        assert_eq!(kept, forced);
    }

    /// A shrunk log is read again from the start, and is reported while it is.
    /// Mutation: keep the old offset after `Shrunk`.
    #[tokio::test]
    async fn a_log_that_shrank_is_read_again_from_the_start() {
        let scratch = Scratch::new("shrunk");
        std::fs::write(scratch.transcript(), prompt("2026-10-28T10:00:00Z", "x")).unwrap();
        let payload = |time: &str| {
            format!(
                "{}\n",
                json!({"received_at": time, "payload": {"session_id": ID, "hook_event_name": "Stop",
                    "background_tasks": [], "last_assistant_message": "m"}})
            )
        };
        append(
            &scratch.events(),
            &format!(
                "{}{}",
                payload("2026-10-28T10:00:00Z"),
                payload("2026-10-28T10:01:00Z")
            ),
        );
        let mut poller = new_poller(&scratch, FakeRunner::new([]));
        let now = at("2026-10-28T12:00:00Z");
        poller.poll(Due::ONLY_FAST, now).await;
        assert!(poller.log_offset > 0);
        std::fs::write(scratch.events(), payload("2026-10-28T10:02:00Z")).unwrap();
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert!(matches!(
            model.health.event_log,
            SourceState::Failed { ref reason, .. } if reason.as_str().contains("shorter")
        ));
        let model = poller.poll(Due::ONLY_FAST, now).await;
        assert!(matches!(model.health.event_log, SourceState::Ok { .. }));
        let id = SessionId::parse(ID).unwrap();
        assert_eq!(
            poller
                .events
                .sessions
                .get(&id)
                .and_then(|e| e.last_stop.as_ref())
                .map(|s| s.at),
            Some(at("2026-10-28T10:02:00Z"))
        );
        drop(model);
    }

    /// A transcript deleted from disk leaves the model at the next discovery; one in a
    /// directory that cannot be listed is kept, and discovery is a failure.
    /// Mutation: always forget what was not found.
    #[tokio::test]
    async fn discovery_forgets_deleted_files_but_not_what_it_could_not_list() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("discovery");
        std::fs::write(scratch.transcript(), prompt("2026-10-28T10:00:00Z", "x")).unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([]));
        let now = at("2026-10-28T12:00:00Z");
        poller.poll(Due::EVERYTHING, now).await;
        let model = settle(&mut poller, now).await;
        assert_eq!(model.health.transcripts.files, 1);

        let dir = scratch.transcript().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listable = std::fs::read_dir(&dir).is_ok();
        let model = poller
            .poll(
                Due {
                    listing: Refresh::Later,
                    discovery: Refresh::Now,
                },
                now,
            )
            .await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !listable {
            assert_eq!(model.health.transcripts.files, 1, "kept, not forgotten");
            assert!(matches!(
                model.health.transcripts.discovery,
                SourceState::Failed { .. }
            ));
        }

        std::fs::remove_file(scratch.transcript()).unwrap();
        let model = poller
            .poll(
                Due {
                    listing: Refresh::Later,
                    discovery: Refresh::Now,
                },
                now,
            )
            .await;
        assert_eq!(model.health.transcripts.files, 0);
        assert!(matches!(
            model.health.transcripts.discovery,
            SourceState::Ok { .. }
        ));
    }

    /// A transcript moved to another project directory (a session entering a worktree) is
    /// followed at its new path from the next discovery on, and what it gains there is read.
    /// Mutation: keep the tailer when the path changes (the follow fails on the old path).
    #[tokio::test]
    async fn a_transcript_moved_to_another_project_is_followed_there() {
        let scratch = Scratch::new("moved");
        std::fs::write(
            scratch.transcript(),
            format!(
                "{}{}",
                prompt("2026-10-28T10:00:00Z", "go"),
                ended("2026-10-28T10:01:00Z")
            ),
        )
        .unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
        let id = SessionId::parse(ID).unwrap();
        let now = at("2026-10-28T12:00:00Z");
        poller.poll(Due::EVERYTHING, now).await;
        settle(&mut poller, now).await;

        let worktree = scratch
            .0
            .join("claude/projects/-home-dev-proj--claude-worktrees-w");
        std::fs::create_dir_all(&worktree).unwrap();
        let moved = worktree.join(format!("{ID}.jsonl"));
        std::fs::rename(scratch.transcript(), &moved).unwrap();
        let discover = Due {
            listing: Refresh::Later,
            discovery: Refresh::Now,
        };
        poller.poll(discover, now).await;
        settle(&mut poller, now).await;
        append(&moved, &prompt("2026-10-28T10:02:00Z", "more"));
        let model = poller.poll(Due::ONLY_FAST, now).await;

        assert_eq!(model.health.follow_faults.transcripts, 0);
        assert!(
            matches!(
                model.session(&id).unwrap().session.activity,
                Activity::Running { .. }
            ),
            "{:?}",
            model.session(&id).unwrap().session.activity
        );
    }

    /// Of two files holding one session, the newest is the one read.
    /// Mutation: drop the `walked` check (the older file, met last, replaces the newer).
    #[tokio::test]
    async fn of_two_transcripts_of_one_session_the_newest_is_read() {
        let scratch = Scratch::new("twice");
        let older = format!(
            "{}{}",
            prompt("2026-10-28T10:00:00Z", "go"),
            ended("2026-10-28T10:01:00Z")
        );
        std::fs::write(scratch.transcript(), &older).unwrap();
        std::fs::File::options()
            .write(true)
            .open(scratch.transcript())
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let worktree = scratch
            .0
            .join("claude/projects/-home-dev-proj--claude-worktrees-w");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(format!("{ID}.jsonl")),
            format!("{older}{}", prompt("2026-10-28T10:02:00Z", "more")),
        )
        .unwrap();
        let mut poller = new_poller(&scratch, FakeRunner::new([Ok(listing_reply())]));
        let id = SessionId::parse(ID).unwrap();
        let now = at("2026-10-28T12:00:00Z");
        poller.poll(Due::EVERYTHING, now).await;
        let model = settle(&mut poller, now).await;

        assert!(
            matches!(
                model.session(&id).unwrap().session.activity,
                Activity::Running { .. }
            ),
            "{:?}",
            model.session(&id).unwrap().session.activity
        );
    }

    /// Marks are saved on top of what was read, and refused while the file is unread or corrupt.
    /// Mutation: start from `SeenStore::default()` when the slot is not `Read`.
    #[tokio::test]
    async fn the_handle_saves_seen_marks_and_pins_and_refuses_over_a_corrupt_file() {
        let scratch = Scratch::new("seen");
        let shared = Arc::new(Shared::new(scratch.config()).unwrap());
        let poller = Poller::new(Arc::clone(&shared), Arc::new(FakeRunner::new([])));
        let now = at("2026-10-28T12:00:00Z");
        let handle = Handle {
            model: Arc::new(RwLock::new(Model::unread(now))),
            shared,
            tasks: Vec::new(),
        };
        let session = SessionId::parse(ID).unwrap();
        let path = scratch.0.join("state/seen.json");
        std::fs::write(&path, format!("{{\"pinned\": [\"{ID}\"]}}")).unwrap();
        assert!(matches!(
            handle.mark_seen(session.clone(), now, Vec::new()),
            Err(SeenError::NotRead(_))
        ));
        poller.reload_seen(now);
        handle.mark_seen(session.clone(), now, Vec::new()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("\"pinned\": [\n    \"0a1b2c3d"),
            "kept: {written}"
        );
        assert!(written.contains("2026-10-28T12:00:00Z"), "{written}");
        handle.unpin(&session).unwrap();
        assert!(
            SeenStore::load(&path)
                .unwrap()
                .seen_through(&session)
                .is_some()
        );
        assert_eq!(SeenStore::load(&path).unwrap().pin_rank(&session), None);
        std::fs::write(&path, "[]").unwrap();
        poller.reload_seen(now);
        assert!(matches!(handle.pin(session), Err(SeenError::NotRead(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }

    /// The automatic save through `autosave`, against a scratch state directory and a scripted
    /// `systemctl`: a session that opens is saved at once; one that stops stays for a minute,
    /// stays while systemd says `stopping`, and leaves once it says `running`; a model not built
    /// from a complete read changes nothing, though it shows no session at all.
    /// Mutation: skip the systemd check, leave without waiting, or observe a partial model.
    #[tokio::test]
    async fn the_automatic_save_keeps_sessions_a_shutdown_stops() {
        use crate::testutil::session_view;
        let scratch = Scratch::new("autosave");
        let shared = Shared::new(scratch.config()).unwrap();
        let start = at("2026-11-17T12:00:00Z");
        let secs = |n: i64| start + SignedDuration::from_secs(n);
        let running = |since| Activity::Running { since };
        let model = |ids: &[u8], complete: bool| {
            let mut model = Model::unread(start);
            if complete {
                model.health.listing = SourceState::Ok { at: start };
                model.health.registry = SourceState::Ok { at: start };
                model.health.transcripts.progress = ScanProgress::Complete;
            }
            model.sessions = ids
                .iter()
                .map(|n| session_view(*n, Some("work"), running(start)))
                .collect();
            model
        };
        let systemctl = |state: &str| Output {
            exit: Exit::Code(1),
            stdout: format!("{state}\n").into_bytes(),
            stderr: String::new(),
        };
        let runner = FakeRunner::new([Ok(systemctl("stopping")), Ok(systemctl("running"))]);
        let saved = |shared: &Shared| {
            shared
                .saves()
                .view()
                .auto
                .as_ref()
                .map_or(0, |save| save.sessions.len())
        };
        let on_disk = || {
            let text = std::fs::read_to_string(scratch.0.join("state/saves/auto.json")).unwrap();
            serde_json::from_str::<saves::Save>(&text)
                .unwrap()
                .sessions
                .len()
        };

        autosave(&shared, &runner, &model(&[1, 2], false), secs(0)).await;
        assert_eq!(saved(&shared), 0);
        autosave(&shared, &runner, &model(&[1, 2], true), secs(0)).await;
        assert_eq!((saved(&shared), on_disk()), (2, 2));
        autosave(&shared, &runner, &model(&[], false), secs(5)).await;
        autosave(&shared, &runner, &model(&[1], true), secs(10)).await;
        assert_eq!(saved(&shared), 2);
        assert!(
            runner.calls().is_empty(),
            "systemd is asked only once a session is due to leave"
        );
        autosave(&shared, &runner, &model(&[1], true), secs(70)).await;
        assert_eq!(saved(&shared), 2);
        autosave(&shared, &runner, &model(&[1], true), secs(75)).await;
        assert_eq!((saved(&shared), on_disk()), (1, 1));
        assert_eq!(runner.calls().len(), 2);
    }

    /// The comment reader's passes against a scripted `acli`: the first asks for every open
    /// item and reads each; one within two minutes asks nothing and reads nothing; a later one
    /// asks for what changed since, two minutes back further, and reads only that. The user's
    /// id comes from the assignee, so the fixture's comment by `…02` is the user's. The search fixture
    /// lists EX-1, EX-2 and EX-3.
    /// Mutation: ask for every open item on each pass, skip the ask on the first pass, or read
    /// every open item again whether or not it changed.
    #[tokio::test]
    async fn the_comment_reader_backfills_once_then_reads_what_changed() {
        use crate::jira::comments::{Author, Thread};
        const SEARCH: &[u8] = include_bytes!("../../tests/fixtures/jira/search_result.json");
        const VIEW: &[u8] = include_bytes!("../../tests/fixtures/jira/view_comments.json");
        let ok = |stdout: &[u8]| {
            Ok(Output {
                exit: Exit::Success,
                stdout: stdout.to_vec(),
                stderr: String::new(),
            })
        };
        let scratch = crate::testutil::Scratch::new("poller-comments");
        let prefixes = vec![JiraPrefix::parse("EX").unwrap()];
        let start = at("2026-11-06T17:00:00Z");
        let mut cache = jira::Cache::load(&scratch, &prefixes, start);
        cache
            .record(
                Ok(jira::parse_work_items(SEARCH, &prefixes).unwrap()),
                start,
            )
            .unwrap();
        let jira = WorkItems {
            cache: Mutex::new(cache),
            prefixes,
            comments: Mutex::new(jira::comments::Reads::load(&scratch)),
        };
        let mine = json!([
            {"key": "EX-1", "fields": {"assignee": {"accountId": "000000000000000000000002"}}},
            {"key": "EX-2", "fields": {"assignee": {"accountId": "000000000000000000000002"}}},
            {"key": "EX-3", "fields": {"assignee": null}},
        ])
        .to_string();
        let changed = json!([{"key": "EX-2", "fields": {"assignee": null}}]).to_string();
        let runner = FakeRunner::new([
            ok(mine.as_bytes()),
            ok(VIEW),
            ok(VIEW),
            ok(VIEW),
            ok(changed.as_bytes()),
            ok(VIEW),
        ]);
        let minutes = |n: i64| start + SignedDuration::from_mins(n);
        comments_pass(&jira, &runner, start).await;
        comments_pass(&jira, &runner, minutes(1)).await;
        comments_pass(&jira, &runner, minutes(3)).await;

        let asked: Vec<String> = runner
            .calls()
            .into_iter()
            .map(|call| call.args.get(2..5).unwrap_or_default().join(" "))
            .collect();
        assert_eq!(
            asked,
            [
                "search --jql assignee = currentUser() AND resolution = Unresolved",
                "view EX-1 --json",
                "view EX-2 --json",
                "view EX-3 --json",
                "search --jql assignee = currentUser() AND resolution = Unresolved AND updated >= -5m",
                "view EX-2 --json",
            ]
        );
        let reads = jira.comments();
        let Some(Thread::Whole(comments)) = reads.thread("EX-1").map(|read| &read.thread) else {
            panic!("EX-1 was read whole");
        };
        let authors: Vec<Author> = comments.iter().map(|c| c.author).collect();
        assert_eq!(authors, [Author::Other, Author::Me]);
        drop(reads);
        assert!(scratch.join(jira::comments::FILE_NAME).exists());
    }

    /// Reads this machine's real sources; run with `CONN_CONFIG=<file> cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the claude CLI, a running supervisor and a config file"]
    #[allow(
        clippy::print_stdout,
        reason = "an ignored smoke test reports counts to its runner"
    )]
    async fn the_real_machine_builds_a_model() {
        let model = read_once(Config::load().unwrap(), Arc::new(TokioRunner))
            .await
            .unwrap();
        let mut counts: Vec<_> = model.activity_counts().into_iter().collect();
        counts.sort_unstable();
        println!("sessions by activity: {counts:?}");
        println!("inbox: {}", model.inbox.len());
        println!("health: {:?}", model.health);
        match saves::observation(&model) {
            Some(seen) => println!(
                "an automatic save would hold {} live sessions ({} unknown)",
                seen.live.len(),
                seen.unknown.len()
            ),
            None => println!("an automatic save would wait: the read was not complete"),
        }
    }
}
