//! The read model, built from what the sources currently say. A pure function of its inputs:
//! the poller reads the world, this decides what it means.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use jiff::SignedDuration;

use super::links::{Derived, Fingerprint, LinkEnv};
use super::seen::SeenStore;
use super::sources::{
    EventLogState, RegisteredEntry, RegistrySnapshot, Source, TailFault, Tracked, TranscriptState,
    Transcripts,
};
use crate::events::{SessionEvents, StopEvent};
use crate::gh::{Answer, Cache, PullRequestRef, Reading};
use crate::gist::{self, GistView};
use crate::ledger::{WaitOn, WaitState};
use crate::links::{Extracted, extract};
use crate::map;
use crate::model::followups::{self, FollowUpsView};
use crate::model::{
    FollowFault, FollowFaults, GhHealth, Health, HeldPrompt, InboxEntry, LAST_TURN_ENDS, LinkView,
    Model, NOW_LINE_CHARS, Notifications, Pin, Rank, ReleaseRun, ReviewsView, ScanProgress,
    Scanned, Seen, SessionView, SourceState, Standing, WaitStanding, Waiting, WorkItemsView,
    approvals, reviews_state, work_items_state,
};
use crate::pending::{
    Alive, Classified, Evidence, Listed, Listing, ListingOpinion, ProcessFacts, RegistryOpinion,
    TranscriptOnDisk, classify, join_alive,
};
use crate::reviews::watch::Watch;
use crate::sessions::launch::{LaunchArgs, LaunchUnknown};
use crate::sessions::listing::{ListedSession, LiveProcess};
use crate::sessions::registry::{Liveness, RegistryEntry, RegistryKind};
use crate::sessions::resume::{ResumeFacts, resume_command};
use crate::transcript::Summary;
use crate::types::{
    Activity, ActivityKind, AssistantText, Instant, Link, PendingCard, ResumeCommand,
    ResumeUnavailable, Session, SessionId, SessionKind, TurnEnd,
};
use crate::util::redact::Redacted;

/// How far apart a Stop hook and the transcript's record of the same turn end may be.
///
/// The hook fires as the turn ends and the transcript's `turn_duration` follows after the
/// other hooks have run, so the two differ by seconds. Another turn that ends inside the
/// window without its own Stop is the only way two turns are taken for one.
const SAME_TURN_SKEW: SignedDuration = SignedDuration::from_secs(60);

/// Everything the builder reads.
pub struct Inputs<'a> {
    pub now: Instant,
    /// What the sessions are classified from; see [`super::sources::ListingForModel`].
    pub listing: &'a Source<Vec<ListedSession>>,
    /// What [`Health::listing`] reports, which can be a failure while `listing` is a covered one.
    pub listing_health: SourceState,
    pub registry: &'a Source<RegistrySnapshot>,
    pub event_log: &'a EventLogState,
    pub transcripts: &'a Transcripts,
    pub gh: &'a Cache,
    /// What the Jira cache holds, read at `now`; `None` where there is no Jira.
    pub work_items: Option<&'a crate::jira::Reading>,
    /// What the review reads and the classifier gave; `None` where no channel is configured.
    pub reviews: Option<&'a crate::reviews::watch::Watch>,
    /// The approvals journal, oldest first; `None` where no channel is configured.
    pub approvals: Option<&'a [crate::reviews::journal::Entry]>,
    /// The gists written so far.
    pub gists: &'a gist::Store,
    /// The plan's rate-limit windows as last read.
    pub usage: &'a crate::usage::Reading,
    /// The coordination ledger as last read.
    pub ledger: &'a Source<crate::ledger::store::Loaded>,
    /// The tickets' links and the epics' children as last read; `None` where no Jira is
    /// configured.
    pub jira_relations: Option<&'a crate::jira::Relations>,
    /// The work items' comments as last read; `None` where no Jira is configured.
    pub jira_comments: Option<&'a crate::jira::comments::Reads>,
    /// Never a directory the map joins sessions by.
    pub home: Option<&'a Path>,
    /// The Follow-ups page's settings; `None` turns it off.
    pub followups: Option<&'a crate::config::FollowUpsConfig>,
    /// The saved sets of open sessions.
    pub saves: &'a crate::saves::SavesView,
    /// The PAM entitlements and grants as last read; `None` where no `[pam]` is configured.
    pub pam: Option<&'a crate::pam::Reading>,
    pub seen: &'a Source<SeenStore>,
    pub links: LinkEnv<'a>,
}

/// A session built, with what the model needs beyond its view.
struct Built {
    view: SessionView,
    prompts: Arc<[HeldPrompt]>,
    entry: Option<InboxEntry>,
}

pub fn build(inputs: &Inputs<'_>) -> Model {
    let ctx = Context::new(inputs);
    let mut unrenderable = 0_u32;
    let mut built: Vec<Built> = ctx
        .universe()
        .iter()
        .filter_map(|id| build_session(inputs, &ctx, id, &mut unrenderable))
        .collect();
    inputs
        .links
        .memo
        .retain(&built.iter().map(|b| &b.view.session.id).collect());

    let mut inbox: Vec<InboxEntry> = built.iter_mut().filter_map(|b| b.entry.take()).collect();
    if let Some(loaded) = inputs.ledger.value() {
        for entry in &mut inbox {
            let wait = wait_standing(loaded.ledger.wait_of(&entry.session, inputs.now));
            entry.standing = Standing::of(entry.rank, wait);
        }
    }
    // By urgency alone: a pin orders the session lists, not the inbox. A stable sort: entries
    // that tie keep the order of their session ids.
    inbox.sort_by_key(|entry| (entry.seen, entry.standing.band(), entry.rank));

    let prompts: HashMap<SessionId, Arc<[HeldPrompt]>> = built
        .iter()
        .filter(|b| !b.prompts.is_empty())
        .map(|b| (b.view.session.id.clone(), Arc::clone(&b.prompts)))
        .collect();
    let views: Vec<SessionView> = built.into_iter().map(|b| b.view).collect();
    let (mut dormant, mut sessions): (Vec<_>, Vec<_>) = views.into_iter().partition(|view| {
        matches!(
            view.session.activity.kind(),
            ActivityKind::Dormant | ActivityKind::Gone
        )
    });
    sessions.sort_by_key(|view| {
        (
            view.pin,
            view.session.activity.kind(),
            std::cmp::Reverse(view.last_activity),
        )
    });
    dormant.sort_by_key(|view| (view.pin, std::cmp::Reverse(view.last_activity)));

    let unknown_sessions = u32::try_from(
        sessions
            .iter()
            .filter(|view| view.session.activity.kind() == ActivityKind::Unknown)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let prs = live_pull_requests(sessions.iter().flat_map(|v| &v.links), &inbox);
    let gh = gh_health(&prs, inputs.gh, inputs.now);
    let coordination = coordination(inputs);
    let work_items = work_items(inputs, &sessions, &dormant, &coordination);
    let map = map::build(&map::Sources {
        now: inputs.now,
        sessions: &sessions,
        dormant: &dormant,
        inbox: &inbox,
        work_items: &work_items,
        home: inputs.home,
    });
    Model {
        generated_at: inputs.now,
        health: health(inputs, gh, unrenderable),
        inbox,
        unknown_sessions,
        prompts,
        reviews: ReviewsView::of(inputs.reviews, inputs.links.targets),
        approvals: inputs
            .approvals
            .map(|entries| approvals(entries, inputs.links.targets)),
        map,
        usage: inputs.usage.clone(),
        followups: followups_of(inputs, &work_items, &coordination, &sessions, &dormant),
        saves: inputs.saves.clone(),
        pam: inputs.pam.cloned(),
        sessions,
        dormant,
        work_items,
        coordination,
    }
}

/// What is due, where the config turns the Follow-ups page on.
fn followups_of(
    inputs: &Inputs<'_>,
    work_items: &WorkItemsView,
    coordination: &crate::model::Coordination,
    sessions: &[SessionView],
    dormant: &[SessionView],
) -> FollowUpsView {
    inputs.followups.map_or(FollowUpsView::Off, |config| {
        followups::build(
            config,
            work_items,
            &coordination.ledger,
            sessions,
            dormant,
            inputs.now,
        )
    })
}

/// The ledger as the pages see it: the last read's state, its contents when it was read, and
/// the transcripts' writes compared against it.
/// The work items joined to the sessions that mention them, their comments, and what the
/// relation reads say of them.
fn work_items(
    inputs: &Inputs<'_>,
    sessions: &[SessionView],
    dormant: &[SessionView],
    coordination: &crate::model::Coordination,
) -> WorkItemsView {
    let seen_through = |key: &crate::types::JiraKey| {
        inputs
            .seen
            .value()
            .and_then(|seen| seen.ticket_seen_through(key))
    };
    let comment_marks = inputs
        .jira_comments
        .map(|reads| crate::model::CommentMarks {
            reads,
            seen_through: &seen_through,
        });
    let relations = inputs
        .jira_relations
        .map(|relations| crate::model::ItemRelations {
            dependencies: &coordination.dependencies,
            children: &relations.children,
        });
    WorkItemsView::of(
        inputs.work_items,
        inputs.links.targets,
        sessions,
        dormant,
        comment_marks.as_ref(),
        relations.as_ref(),
    )
}

fn coordination(inputs: &Inputs<'_>) -> crate::model::Coordination {
    let mut coordination = match inputs.ledger.value() {
        Some(loaded) => crate::model::Coordination {
            state: inputs.ledger.state(),
            ledger: loaded.ledger.clone(),
            bad_lines: loaded.bad_lines.clone(),
            shared: Vec::new(),
            dependencies: Vec::new(),
            links: None,
        },
        None => crate::model::Coordination {
            state: inputs.ledger.state(),
            ..crate::model::Coordination::unread()
        },
    };
    let writes = inputs
        .transcripts
        .tracked
        .iter()
        .filter_map(|(id, tracked)| tracked.state.summary().map(|summary| (id, summary)))
        .flat_map(|(id, summary)| summary.writes.iter().map(move |written| (id, written)));
    coordination.shared = crate::ledger::shared::find(
        writes,
        &coordination.ledger,
        inputs.now - crate::model::SHARED_WINDOW,
    );
    if let Some(relations) = inputs.jira_relations {
        coordination.dependencies = relations.links.dependencies();
        coordination.links = Some(relations.links.state());
    }
    coordination
}

fn health(inputs: &Inputs<'_>, gh: GhHealth, links_unrenderable: u32) -> Health {
    let tracked = &inputs.transcripts.tracked;
    let scanned = tracked
        .values()
        .filter(|t| !matches!(t.state, TranscriptState::Queued))
        .count();
    let progress = if scanned == tracked.len() && inputs.transcripts.discovery.is_some() {
        ScanProgress::Complete
    } else {
        ScanProgress::InProgress {
            scanned,
            total: tracked.len(),
        }
    };
    Health {
        listing: inputs.listing_health.clone(),
        registry: registry_state(inputs.registry),
        event_log: inputs.event_log.health.clone(),
        event_log_line_errors: inputs.event_log.line_errors,
        transcripts: Scanned {
            files: tracked.len(),
            skipped: inputs.transcripts.skipped,
            line_errors: tracked
                .values()
                .filter_map(|t| t.state.summary())
                .map(|s| u64::from(s.line_errors))
                .sum(),
            unreadable: tracked
                .values()
                .filter(|t| matches!(t.state, TranscriptState::Unreadable(_)))
                .count(),
            discovery: inputs
                .transcripts
                .discovery
                .clone()
                .unwrap_or(SourceState::NeverRead),
            progress,
        },
        follow_faults: follow_faults(tracked),
        gh,
        seen: inputs.seen.state(),
        links_unrenderable,
        unidentified_listed: unidentified(inputs.listing),
        // Sending is the poller's act, after the model is built.
        notifications: Notifications::Untried,
        work_items: work_items_state(inputs.work_items),
        work_item_comments: inputs
            .jira_comments
            .map(crate::jira::comments::Reads::state),
        work_items_linked: inputs
            .jira_relations
            .map(|relations| relations.linked.state()),
        epic_children: inputs
            .jira_relations
            .map(|relations| relations.children.state()),
        reviews: reviews_state(inputs.reviews.map(Watch::reading)),
        gists: inputs.gists.health().clone(),
    }
}

/// The transcripts whose latest follow failed, and the most recent failure among them.
fn follow_faults(tracked: &HashMap<SessionId, Tracked>) -> FollowFaults {
    let faults: Vec<&TailFault> = tracked
        .values()
        .filter_map(|t| t.tail_fault.as_ref())
        .collect();
    FollowFaults {
        transcripts: faults.len(),
        latest: faults
            .into_iter()
            .max_by_key(|fault| fault.at)
            .map(|fault| FollowFault {
                at: fault.at,
                reason: fault.reason.clone(),
            }),
    }
}

/// The registry is read when its directory is; it is partly failed while any entry is not.
pub(super) fn registry_state(registry: &Source<RegistrySnapshot>) -> SourceState {
    match registry {
        Source::Read { at, value } if !value.errors.is_empty() => SourceState::Failed {
            at: *at,
            reason: format!(
                "{} entries unreadable, first: {}",
                value.errors.len(),
                value.errors.first().map_or("", String::as_str)
            ),
        },
        Source::Read { .. } | Source::Failed { .. } | Source::NeverRead => registry.state(),
    }
}

fn unidentified(listing: &Source<Vec<ListedSession>>) -> u32 {
    let count = listing.value().map_or(0, |listed| {
        listed
            .iter()
            .filter(|entry| entry.common().session_id.is_none())
            .count()
    });
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The pull requests the live sessions and the inbox mention, distinct: the ones whose state
/// is kept fresh.
pub fn live_pull_requests<'l>(
    sessions: impl Iterator<Item = &'l LinkView>,
    inbox: &'l [InboxEntry],
) -> Vec<PullRequestRef> {
    let mut seen = HashSet::new();
    sessions
        .chain(inbox.iter().flat_map(|entry| &entry.links))
        .filter_map(|view| pull_request_of(&view.link))
        .filter(|pr| seen.insert(pr.clone()))
        .collect()
}

/// How current the states of those pull requests are.
fn gh_health(prs: &[PullRequestRef], cache: &Cache, now: Instant) -> GhHealth {
    let mut health = GhHealth::default();
    for pr in prs {
        match cache.reading(pr, now) {
            Reading::Unread => health.unread += 1,
            Reading::Value {
                observed,
                failure: None,
                ..
            } => match observed.answer {
                Answer::Found(_) => health.fresh += 1,
                Answer::NotFound => health.not_found += 1,
            },
            Reading::Value {
                failure: Some(_), ..
            } => health.stale += 1,
            Reading::Failure(_) => health.failed += 1,
        }
    }
    health
}

fn pull_request_of(link: &Link) -> Option<PullRequestRef> {
    PullRequestRef::try_from(link).ok()
}

/// What the builder looks up by session id.
struct Context<'a> {
    inputs: &'a Inputs<'a>,
    listing: HashMap<&'a SessionId, &'a ListedSession>,
    registry: HashMap<&'a SessionId, &'a RegisteredEntry>,
}

impl<'a> Context<'a> {
    fn new(inputs: &'a Inputs<'a>) -> Self {
        let listing = inputs
            .listing
            .value()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.common().session_id.as_ref().map(|id| (id, entry)))
            .collect();
        let mut registry: HashMap<&SessionId, &RegisteredEntry> = HashMap::new();
        for registered in inputs.registry.value().into_iter().flat_map(|s| &s.entries) {
            let id = &registered.entry.session_id;
            let better = registry.get(id).is_none_or(|kept| {
                rank_liveness(&registered.liveness) < rank_liveness(&kept.liveness)
            });
            if better {
                registry.insert(id, registered);
            }
        }
        Self {
            inputs,
            listing,
            registry,
        }
    }

    /// Every session worth a line: a transcript on disk, or a process somebody reports.
    /// A registry entry whose process is gone and which has no transcript is a leftover.
    fn universe(&self) -> BTreeSet<SessionId> {
        let mut ids: BTreeSet<SessionId> =
            self.inputs.transcripts.tracked.keys().cloned().collect();
        ids.extend(self.listing.keys().map(|id| (*id).clone()));
        ids.extend(
            self.registry
                .iter()
                .filter(|(_, r)| !matches!(r.liveness, Liveness::Gone { .. }))
                .map(|(id, _)| (*id).clone()),
        );
        ids
    }
}

/// Prefers a live entry over one that cannot be told over a dead one.
const fn rank_liveness(liveness: &Liveness) -> u8 {
    match liveness {
        Liveness::Live => 0,
        Liveness::Unknown(_) => 1,
        Liveness::Gone { .. } => 2,
    }
}

/// How a session's transcript stands.
enum Transcript<'a> {
    Scanned {
        summary: &'a Summary,
        fault: Option<&'a str>,
    },
    Queued,
    Unreadable {
        reason: &'a str,
    },
    /// No file has been discovered for the session.
    Absent,
}

fn transcript_of<'a>(inputs: &'a Inputs<'a>, id: &SessionId) -> Transcript<'a> {
    let Some(tracked) = inputs.transcripts.tracked.get(id) else {
        return Transcript::Absent;
    };
    match &tracked.state {
        TranscriptState::Scanned(summary) => Transcript::Scanned {
            summary,
            fault: tracked
                .tail_fault
                .as_ref()
                .map(|fault| fault.reason.as_str()),
        },
        TranscriptState::Queued => Transcript::Queued,
        TranscriptState::Unreadable(reason) => Transcript::Unreadable { reason },
    }
}

/// The summary of a session no transcript has been read for.
static NO_SUMMARY: LazyLock<Summary> = LazyLock::new(Summary::default);

/// What one session's three sources and its transcript say, before they are classified.
struct Facts<'a> {
    listed: Option<&'a ListedSession>,
    registered: Option<&'a RegisteredEntry>,
    alive: Alive,
    /// Why the transcript cannot be relied on, which makes the session `Unknown`.
    blocker: Option<String>,
    summary: &'a Summary,
    events: Option<&'a SessionEvents>,
    stop: Option<&'a StopEvent>,
    /// Newest first.
    ends: Vec<TurnEnd>,
    last_human_prompt_at: Option<Instant>,
    process: ProcessInfo,
    cwd: Option<PathBuf>,
    resume: Result<ResumeCommand, ResumeUnavailable>,
    transcript: TranscriptOnDisk,
}

impl<'a> Facts<'a> {
    /// `None` for a session there is nothing to show yet: its transcript waits for its first
    /// scan and no process is known (the scan progress says so), or it has no transcript and
    /// nowhere to put one.
    fn gather(inputs: &'a Inputs<'a>, ctx: &Context<'a>, id: &SessionId) -> Option<Self> {
        let listed = ctx.listing.get(id).copied();
        let registered = ctx.registry.get(id).copied();
        let alive = join_alive(
            &registry_opinion(inputs, registered),
            &listing_opinion(inputs.listing, listed),
        );
        let transcript = transcript_of(inputs, id);
        let blocker = match (&transcript, &alive) {
            (Transcript::Queued, Alive::No) => return None,
            (Transcript::Queued, Alive::Yes | Alive::Unknown(_)) => {
                Some("its transcript has not been scanned yet".to_owned())
            }
            (Transcript::Unreadable { reason }, _) => {
                Some(format!("its transcript cannot be read: {reason}"))
            }
            (
                Transcript::Scanned {
                    fault: Some(fault), ..
                },
                _,
            ) => Some(format!("its transcript could not be followed: {fault}")),
            (Transcript::Scanned { fault: None, .. } | Transcript::Absent, _) => None,
        };
        let summary = match &transcript {
            Transcript::Scanned { summary, .. } => summary,
            Transcript::Queued | Transcript::Unreadable { .. } | Transcript::Absent => &*NO_SUMMARY,
        };
        let events = inputs.event_log.sessions.get(id);
        let stop = events.and_then(|e| e.last_stop.as_ref());
        let process = process_facts(listed, registered);
        let cwd = working_directory(&alive, &process, summary);
        if matches!(transcript, Transcript::Absent)
            && cwd.as_deref().is_none_or(|cwd| cwd.as_os_str().is_empty())
        {
            tracing::warn!(session = %id, "a running session has neither a transcript nor a working directory; it is not shown");
            return None;
        }
        // A file found but not read yet, or not readable, is still a file: those cases are
        // `Unknown` through `blocker` before the transcript's presence matters.
        let transcript = match (&transcript, &inputs.transcripts.discovery) {
            (Transcript::Absent, Some(SourceState::Ok { .. })) => TranscriptOnDisk::Missing,
            (
                Transcript::Absent,
                None
                | Some(
                    SourceState::Failed { .. }
                    | SourceState::NeverWritten { .. }
                    | SourceState::NeverRead,
                ),
            ) => TranscriptOnDisk::NotFoundYet,
            (
                Transcript::Scanned { .. } | Transcript::Queued | Transcript::Unreadable { .. },
                _,
            ) => TranscriptOnDisk::Found,
        };
        Some(Self {
            transcript,
            resume: resume_for(id, cwd.as_deref(), &alive, registered, summary),
            ends: turn_ends_newest_first(
                summary,
                stop,
                events.and_then(|e| e.last_human_prompt_at),
            ),
            last_human_prompt_at: summary
                .last_human_prompt_at
                .max(events.and_then(|e| e.last_human_prompt_at)),
            listed,
            registered,
            alive,
            blocker,
            summary,
            events,
            stop,
            process,
            cwd,
        })
    }

    fn kind(&self) -> SessionKind {
        kind_of(self.listed, self.registered, self.summary)
    }

    /// The activity and card, or `Unknown` with the reason the transcript cannot be relied on.
    fn classify(&self, inputs: &Inputs<'_>, id: &SessionId) -> Classified {
        if let Some(why) = &self.blocker {
            return Classified {
                activity: Activity::Unknown {
                    reason: Redacted::of(why),
                },
                card: None,
            };
        }
        classify(
            id,
            &Evidence {
                kind: self.kind(),
                alive: self.alive.clone(),
                listing: listing_evidence(inputs.listing, self.listed),
                started_at: self.process.started_at,
                pending_ask: self.summary.pending_ask.as_ref(),
                last_human_prompt_at: self.last_human_prompt_at,
                latest_turn_end: self.ends.first(),
                last_record_at: self.summary.last_record_at,
                transcript: self.transcript,
                resume: &self.resume,
            },
        )
    }
}

/// Where a session stands by its wait in the ledger, if it declared one.
fn wait_standing(state: Option<WaitState<'_>>) -> WaitStanding {
    match state {
        None => WaitStanding::None,
        Some(WaitState::Waiting { wait, running }) => WaitStanding::Waiting(match &wait.on {
            WaitOn::Rollout(rollout) => Waiting::Release {
                rollout: rollout.clone(),
                run: match running {
                    None => ReleaseRun::NotStarted,
                    Some(_) => ReleaseRun::Running,
                },
            },
            on @ (WaitOn::Subject(_) | WaitOn::Until(_) | WaitOn::For(_)) => Waiting::Other {
                on: followups::on_words(on),
            },
        }),
        Some(
            WaitState::RolloutFinished { .. }
            | WaitState::TimeReached { .. }
            | WaitState::Released { .. }
            | WaitState::TicketClosed { .. },
        ) => WaitStanding::Over,
    }
}

fn build_session(
    inputs: &Inputs<'_>,
    ctx: &Context<'_>,
    id: &SessionId,
    unrenderable: &mut u32,
) -> Option<Built> {
    let facts = Facts::gather(inputs, ctx, id)?;
    let classified = facts.classify(inputs, id);
    let fingerprint = Fingerprint {
        bytes_read: facts.summary.bytes_read,
        last_record_at: facts.summary.last_record_at,
        last_stop_at: facts.stop.map(|s| s.at),
    };
    let derived = inputs
        .links
        .memo
        .get_or_derive(id, fingerprint, || Derived {
            extracted: extract(inputs.links.extractor, facts.summary, &facts.ends),
            prompts: held_prompts(facts.summary),
        });
    let links = link_views(&derived.extracted, inputs, unrenderable);

    let session = Session {
        id: id.clone(),
        cwd: facts.cwd.clone(),
        ai_title: facts.summary.ai_title.clone(),
        window_title: facts.summary.window_title().map(str::to_owned),
        name: facts.process.name.clone(),
        kind: facts.kind(),
        activity: classified.activity,
    };
    let title = session
        .name
        .as_deref()
        .or(session.ai_title.as_deref())
        .map(Redacted::of);
    let entry = classified.card.as_ref().map(|card| {
        let gist = GistView::of(inputs.gists.get(id), card.since());
        InboxEntry {
            session: id.clone(),
            title: title.clone(),
            rank: Rank::of(card),
            standing: Standing::Normal,
            seen: seen_state(inputs.seen.value(), id, card),
            now_line: now_line(card, &facts.ends),
            links: links.clone(),
            ask: gist::ask(card, &gist),
            gist,
            card: card.clone(),
        }
    });
    Some(Built {
        prompts: Arc::clone(&derived.prompts),
        entry,
        view: SessionView {
            title,
            pending: classified.card,
            links,
            last_activity: last_activity(facts.summary, facts.events),
            pin: pin_state(inputs.seen.value(), id),
            gist: inputs
                .gists
                .get(id)
                .and_then(gist::Entry::written)
                .map(|written| written.gist.text.clone()),
            resume: facts.resume,
            last_turn_ends: facts.ends,
            session,
        },
    })
}

/// The human prompts the transcript holds, oldest first, redacted once here so that search
/// reads the same text the pages show.
fn held_prompts(summary: &Summary) -> Arc<[HeldPrompt]> {
    summary
        .human_prompts
        .iter()
        .map(|prompt| HeldPrompt {
            at: prompt.at,
            text: Redacted::of(&prompt.text),
        })
        .collect()
}

fn pin_state(seen: Option<&SeenStore>, id: &SessionId) -> Pin {
    seen.and_then(|s| s.pin_rank(id))
        .map_or(Pin::Unpinned, Pin::Pinned)
}

/// A card is seen when the human marked a card of its session seen that is the card itself or
/// an earlier one: the mark records the `since` of the card it was made on, so a later turn end
/// is a new card that no earlier mark covers.
fn seen_state(seen: Option<&SeenStore>, id: &SessionId, card: &PendingCard) -> Seen {
    match seen.and_then(|s| s.seen_through(id)) {
        Some(marked) if card.since() <= marked => Seen::Seen,
        Some(_) | None => Seen::Unseen,
    }
}

fn last_activity(summary: &Summary, events: Option<&SessionEvents>) -> Option<Instant> {
    [
        summary.last_record_at,
        events.and_then(|e| e.last_stop.as_ref().map(|s| s.at)),
        events.and_then(|e| e.last_prompt.as_ref().map(|p| p.at)),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// The first [`NOW_LINE_CHARS`] characters of the turn end before the card's: for a
/// finished turn the one before it, for a question or a wait the latest. The cut is made on
/// the redacted text, which [`TurnEnd::text`] already is.
fn now_line(card: &PendingCard, ends_newest_first: &[TurnEnd]) -> Option<Redacted<String>> {
    let context = match card {
        PendingCard::Asked { .. } | PendingCard::Blocked { .. } => ends_newest_first.first(),
        PendingCard::Turn { .. } => ends_newest_first.get(1),
    }?;
    let text = context.text.redacted();
    (!text.as_str().trim().is_empty()).then(|| text.prefix(NOW_LINE_CHARS))
}

fn link_views(extracted: &Extracted, inputs: &Inputs<'_>, unrenderable: &mut u32) -> Vec<LinkView> {
    extracted
        .links
        .iter()
        .filter_map(|link| match link.url(inputs.links.targets) {
            Ok(url) => Some(LinkView {
                pr: pull_request_of(link).map(|pr| inputs.gh.reading(&pr, inputs.now)),
                link: link.clone(),
                url,
            }),
            Err(error) => {
                *unrenderable = unrenderable.saturating_add(1);
                tracing::warn!(%link, %error, "a link cannot be rendered as a URL");
                None
            }
        })
        .collect()
}

/// The turn ends of a session, newest first, at most [`LAST_TURN_ENDS`]: the transcript's, with
/// the latest Stop hook folded in.
///
/// A Stop within [`SAME_TURN_SKEW`] of the transcript's latest turn end is the same turn, unless
/// a human prompt lies between that turn end and the Stop (a new turn then ended within the
/// window): the transcript's end, with the background tasks only the Stop lists, and the
/// Stop's text when the Stop is not older. A later Stop is a turn the transcript has not caught
/// up with; an earlier one is stale and ignored.
///
/// `logged_prompt` is the event log's latest human prompt, which the transcript may not
/// have recorded yet.
fn turn_ends_newest_first(
    summary: &Summary,
    stop: Option<&StopEvent>,
    logged_prompt: Option<Instant>,
) -> Vec<TurnEnd> {
    let limit = LAST_TURN_ENDS.get();
    let mut ends: Vec<TurnEnd> = summary
        .turn_ends
        .iter()
        .rev()
        .take(limit)
        .cloned()
        .collect();
    let Some(stop) = stop else {
        return ends;
    };
    match ends.first_mut() {
        Some(latest)
            if stop.at.duration_since(latest.at).abs() <= SAME_TURN_SKEW
                && !summary
                    .human_prompt_times
                    .iter()
                    .copied()
                    .chain(logged_prompt)
                    .any(|prompt| latest.at < prompt && prompt <= stop.at) =>
        {
            latest.background_tasks = stop.background_tasks.clone();
            if stop.at >= latest.at
                && let Some(text) = &stop.text
            {
                latest.text = text.clone();
            }
        }
        Some(latest) if stop.at < latest.at => {}
        Some(_) | None => {
            ends.insert(0, turn_end_of(stop));
            ends.truncate(limit);
        }
    }
    ends
}

fn turn_end_of(stop: &StopEvent) -> TurnEnd {
    TurnEnd {
        at: stop.at,
        trigger: stop.turn_trigger.clone(),
        text: stop.text.clone().unwrap_or_else(|| AssistantText::of("")),
        background_tasks: stop.background_tasks.clone(),
        asked: None,
    }
}

// --- the three per-session sources, each possibly absent ---

fn registry_opinion(inputs: &Inputs<'_>, registered: Option<&RegisteredEntry>) -> RegistryOpinion {
    match (inputs.registry, registered) {
        (Source::Read { .. }, Some(registered)) => match &registered.liveness {
            Liveness::Live => RegistryOpinion::Alive,
            Liveness::Gone { .. } => RegistryOpinion::Gone,
            Liveness::Unknown(reason) => RegistryOpinion::Cannot(reason.to_string()),
        },
        (Source::Read { value, .. }, None) if value.errors.is_empty() => RegistryOpinion::Absent,
        (Source::Read { value, .. }, None) => RegistryOpinion::Cannot(format!(
            "{} registry entries could not be read",
            value.errors.len()
        )),
        (Source::Failed { reason, .. }, _) => RegistryOpinion::Cannot(reason.clone()),
        (Source::NeverRead, _) => {
            RegistryOpinion::Cannot("the registry has not been read yet".to_owned())
        }
    }
}

fn listing_opinion(
    listing: &Source<Vec<ListedSession>>,
    listed: Option<&ListedSession>,
) -> ListingOpinion {
    match (listing, listed) {
        (Source::Read { .. }, Some(entry)) => {
            if live_process(entry).is_some() {
                ListingOpinion::Process
            } else {
                ListingOpinion::NoProcess
            }
        }
        (Source::Read { .. }, None) => ListingOpinion::NoProcess,
        (Source::Failed { reason, .. }, _) => ListingOpinion::Cannot(reason.clone()),
        (Source::NeverRead, _) => {
            ListingOpinion::Cannot("the listing has not been read yet".to_owned())
        }
    }
}

fn live_process(entry: &ListedSession) -> Option<&LiveProcess> {
    match entry {
        ListedSession::Interactive(s) => Some(&s.process),
        ListedSession::Background(s) => s.process.as_ref(),
        ListedSession::Other(s) => s.process.as_ref(),
    }
}

fn listing_evidence(
    listing: &Source<Vec<ListedSession>>,
    listed: Option<&ListedSession>,
) -> Listing {
    match (listing, listed) {
        (Source::Failed { reason, .. }, _) => Listing::Failed(reason.clone()),
        (Source::NeverRead, _) => Listing::Failed("the listing has not been read yet".to_owned()),
        (Source::Read { .. }, None) => Listing::Absent,
        (Source::Read { .. }, Some(entry)) => Listing::Listed(Listed {
            process: live_process(entry).map(|p| ProcessFacts {
                status: p.status.clone(),
                waiting_for: p.waiting_for.clone(),
            }),
            bg_state: match entry {
                ListedSession::Background(b) => Some(b.state.clone()),
                ListedSession::Interactive(_) | ListedSession::Other(_) => None,
            },
        }),
    }
}

/// What the listing and registry say about the process, joined for fields that have no
/// opinion of their own.
struct ProcessInfo {
    cwd: Option<PathBuf>,
    started_at: Option<Instant>,
    name: Option<String>,
}

fn process_facts(
    listed: Option<&ListedSession>,
    registered: Option<&RegisteredEntry>,
) -> ProcessInfo {
    let entry: Option<&RegistryEntry> = registered.map(|r| &r.entry);
    let common = listed.map(ListedSession::common);
    ProcessInfo {
        cwd: common
            .map(|c| c.cwd.clone())
            .or_else(|| entry.map(|e| e.cwd.clone())),
        started_at: common
            .map(|c| c.started_at)
            .or_else(|| entry.and_then(|e| e.started_at)),
        name: common
            .and_then(|c| c.name.clone())
            .or_else(|| entry.and_then(|e| e.name.clone())),
    }
}

/// Where the session works: a live process's directory, else the transcript's last, else
/// whatever a process once reported.
fn working_directory(alive: &Alive, process: &ProcessInfo, summary: &Summary) -> Option<PathBuf> {
    match alive {
        Alive::Yes | Alive::Unknown(_) => process.cwd.clone().or_else(|| summary.cwd.clone()),
        Alive::No => summary.cwd.clone().or_else(|| process.cwd.clone()),
    }
}

/// How the session was started: what the supervisor says, else the registry, else the
/// transcript's entrypoint, else a terminal session.
fn kind_of(
    listed: Option<&ListedSession>,
    registered: Option<&RegisteredEntry>,
    summary: &Summary,
) -> SessionKind {
    let from_listing = match listed {
        Some(ListedSession::Interactive(_)) => Some(SessionKind::Interactive),
        Some(ListedSession::Background(_)) => Some(SessionKind::Background),
        Some(ListedSession::Other(_)) | None => None,
    };
    let from_registry = registered.and_then(|r| match &r.entry.kind {
        Some(RegistryKind::Interactive) => Some(SessionKind::Interactive),
        Some(RegistryKind::Background) => Some(SessionKind::Background),
        Some(RegistryKind::Other(_)) | None => None,
    });
    let from_entrypoint = summary.entrypoint.as_deref().map(|entrypoint| {
        if entrypoint == "sdk-cli" {
            SessionKind::Headless
        } else {
            SessionKind::Interactive
        }
    });
    from_listing
        .or(from_registry)
        .or(from_entrypoint)
        .unwrap_or(SessionKind::Interactive)
}

fn resume_for(
    id: &SessionId,
    cwd: Option<&Path>,
    alive: &Alive,
    registered: Option<&RegisteredEntry>,
    summary: &Summary,
) -> Result<ResumeCommand, ResumeUnavailable> {
    let Some(cwd) = cwd else {
        return Err(ResumeUnavailable::NoWorkingDirectory);
    };
    let launch_args = match (alive, registered) {
        (Alive::Yes, Some(registered)) => registered.launch.clone(),
        (Alive::Yes, None) => LaunchArgs::Unknown(LaunchUnknown::LivenessUnknown(
            "the session is not in the registry".to_owned(),
        )),
        (Alive::No, _) => LaunchArgs::Unknown(LaunchUnknown::Dormant),
        (Alive::Unknown(why), _) => {
            LaunchArgs::Unknown(LaunchUnknown::LivenessUnknown(why.clone()))
        }
    };
    let facts = ResumeFacts {
        session_id: id.clone(),
        cwd: cwd.to_path_buf(),
        launch_args,
        last_permission_mode: summary.permission_mode.clone(),
        added_dirs: Vec::new(),
    };
    Ok(resume_command(&facts)?)
}

#[cfg(test)]
mod tests {
    //! The builder on hand-built sources: a world of five sessions whose activity, card, rank
    //! and links are written out from the rules, and the failures the model must show rather
    //! than hide. Transcripts are folded from synthetic records, the other sources are built
    //! as the poller would hold them.

    use serde_json::{Value, json};

    use super::*;
    use crate::config::Config;
    use crate::links::Extractor;
    use crate::model::Tier;
    use crate::sessions::facts::ProcessStatus;
    use crate::sessions::listing::{ListedCommon, ListedInteractive};
    use crate::sessions::registry::GoneReason;
    use crate::sessions::transcripts::TranscriptFile;
    use crate::state::links::DerivedMemo;
    use crate::state::sources::Tracked;
    use crate::testutil::{at, session_id as sid};
    use crate::transcript::{TailEvent, fold_event, parse_line};
    use crate::types::{
        BgState, LinkTargets, ListingStatus, Pid, ProjectDirName, TurnTrigger, WaitingFor,
    };

    const NOW: &str = "2026-10-28T12:00:00Z";

    /// GitHub saying a pull request does not exist is an answer: it is counted apart and never
    /// as a failure, so a placeholder link in a session's text does not keep the header
    /// saying "gh (failed)".
    /// Mutation: count `Answer::NotFound` as `failed` (or as `fresh`).
    #[test]
    fn a_pull_request_github_has_no_record_of_is_not_found_not_failed() {
        use crate::gh::{GhError, parse_pr_state};
        let pr = |n| PullRequestRef::new("example-org", "example-repo", n).unwrap();
        let state = parse_pr_state(
            include_bytes!("../../tests/fixtures/gh/open_passing.json"),
            &pr(42),
        )
        .unwrap();
        let now = at(NOW);
        let mut cache = Cache::default();
        cache.record(&pr(42), Ok(Answer::Found(state)), now);
        cache.record(&pr(43), Ok(Answer::NotFound), now);
        cache.record(&pr(44), Err(GhError::AuthRequired), now);
        assert_eq!(
            gh_health(&[pr(42), pr(43), pr(44), pr(45)], &cache, now),
            GhHealth {
                fresh: 1,
                not_found: 1,
                stale: 0,
                failed: 1,
                unread: 1,
            }
        );
    }

    // --- synthetic transcripts ---

    fn record(kind: &str, time: &str, extra: &Value) -> Value {
        let mut base = json!({"type": kind, "timestamp": time, "sessionId": sid(1).as_str(),
            "cwd": "/home/dev/proj", "isSidechain": false});
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        base
    }

    fn prompt(time: &str, origin: &str, text: &str) -> Value {
        record(
            "user",
            time,
            &json!({"origin": {"kind": origin}, "message": {"role": "user", "content": text}}),
        )
    }

    fn reply(time: &str, text: &str) -> Value {
        record(
            "assistant",
            time,
            &json!({"message": {"id": format!("m-{time}"), "role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": text}]}}),
        )
    }

    fn ended(time: &str) -> Value {
        record(
            "system",
            time,
            &json!({"subtype": "turn_duration", "durationMs": 1}),
        )
    }

    fn question(time: &str) -> Value {
        record(
            "assistant",
            time,
            &json!({"message": {"id": "m-ask", "role": "assistant", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": "toolu_q", "name": "AskUserQuestion",
                    "input": {"questions": [{"question": "Which one? See ABC-9", "header": "H",
                        "multiSelect": false, "options": [{"label": "A", "description": "d"}]}]}}]}}),
        )
    }

    fn summary_of(records: &[Value]) -> Summary {
        let mut summary = Summary::default();
        for record in records {
            fold_event(
                &mut summary,
                &TailEvent::from(parse_line(&record.to_string())),
            );
        }
        summary.set_extent(u64::try_from(records.len()).unwrap(), 0);
        summary
    }

    fn tracked(id: &SessionId, summary: Summary) -> Tracked {
        Tracked {
            file: TranscriptFile {
                project_dir: ProjectDirName::new("-home-dev-proj").unwrap(),
                session_id: id.clone(),
                path: format!("/claude/projects/-home-dev-proj/{id}.jsonl").into(),
                modified: at("2026-10-28T11:00:00Z"),
                size: 1,
            },
            state: TranscriptState::Scanned(Box::new(summary)),
            tailer: None,
            tail_fault: None,
        }
    }

    // --- synthetic sources ---

    fn listed(n: u8, status: Option<ProcessStatus>, waiting: Option<WaitingFor>) -> ListedSession {
        ListedSession::Interactive(ListedInteractive {
            common: ListedCommon {
                cwd: "/home/dev/proj".into(),
                started_at: at("2026-10-28T07:00:00Z"),
                session_id: Some(sid(n)),
                name: None,
            },
            process: LiveProcess {
                pid: Pid::new(1000 + u32::from(n)),
                status,
                waiting_for: waiting,
            },
        })
    }

    fn registered(n: u8, liveness: Liveness) -> RegisteredEntry {
        RegisteredEntry {
            entry: RegistryEntry {
                pid: Pid::new(1000 + u32::from(n)),
                session_id: sid(n),
                cwd: "/home/dev/proj".into(),
                started_at: None,
                proc_start: None,
                pid_domain: None,
                version: None,
                kind: Some(RegistryKind::Interactive),
                entrypoint: None,
                name: None,
                name_source: None,
                status: None,
                status_updated_at: None,
                job_id: None,
            },
            liveness,
            launch: LaunchArgs::Unknown(LaunchUnknown::Dormant),
        }
    }

    /// Everything the builder reads, owned.
    struct World {
        listing: Source<Vec<ListedSession>>,
        registry: Source<RegistrySnapshot>,
        events: EventLogState,
        transcripts: Transcripts,
        gh: Cache,
        work_items: crate::jira::Reading,
        seen: Source<SeenStore>,
        gists: gist::Store,
        usage: crate::usage::Reading,
        ledger: Source<crate::ledger::store::Loaded>,
        extractor: Extractor,
        targets: LinkTargets,
        memo: DerivedMemo,
    }

    impl World {
        fn new() -> Self {
            let config = Config::parse(
                "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n[slack]\nhost = \"example.slack.com\"\n",
                Path::new("/cfg.toml"),
                Path::new("/home/dev"),
            )
            .unwrap();
            let now = at(NOW);
            Self {
                listing: Source::Read {
                    at: now,
                    value: Vec::new(),
                },
                registry: Source::Read {
                    at: now,
                    value: RegistrySnapshot::default(),
                },
                events: EventLogState {
                    sessions: HashMap::new(),
                    health: SourceState::Ok { at: now },
                    line_errors: 0,
                },
                transcripts: Transcripts {
                    tracked: HashMap::new(),
                    skipped: 0,
                    discovery: Some(SourceState::Ok { at: now }),
                },
                gh: Cache::default(),
                work_items: crate::jira::Reading::Unread,
                seen: Source::Read {
                    at: now,
                    value: SeenStore::default(),
                },
                // No file is ever read there: the store is empty.
                gists: gist::Store::load(Path::new("/nonexistent/conn-builder-test"), now),
                usage: crate::usage::Reading::default(),
                ledger: Source::NeverRead,
                extractor: Extractor::from_config(&config).unwrap(),
                targets: config.link_targets(),
                memo: DerivedMemo::default(),
            }
        }

        fn with(mut self, n: u8, records: &[Value]) -> Self {
            self.transcripts
                .tracked
                .insert(sid(n), tracked(&sid(n), summary_of(records)));
            self
        }

        fn live(mut self, n: u8, status: Option<ProcessStatus>) -> Self {
            if let Source::Read { value, .. } = &mut self.listing {
                value.push(listed(n, status, None));
            }
            if let Source::Read { value, .. } = &mut self.registry {
                value.entries.push(registered(n, Liveness::Live));
            }
            self
        }

        fn model(&self) -> Model {
            build(&Inputs {
                now: at(NOW),
                listing: &self.listing,
                listing_health: self.listing.state(),
                registry: &self.registry,
                event_log: &self.events,
                transcripts: &self.transcripts,
                gh: &self.gh,
                work_items: Some(&self.work_items),
                reviews: None,
                approvals: None,
                gists: &self.gists,
                usage: &self.usage,
                ledger: &self.ledger,
                jira_relations: None,
                jira_comments: None,
                home: None,
                followups: None,
                saves: &crate::saves::SavesView::default(),
                pam: None,
                seen: &self.seen,
                links: LinkEnv {
                    extractor: &self.extractor,
                    targets: &self.targets,
                    memo: &self.memo,
                },
            })
        }
    }

    fn inbox_order(model: &Model) -> Vec<SessionId> {
        model.inbox.iter().map(|e| e.session.clone()).collect()
    }

    /// Session 1 asks a question; 2 finished a turn the human started; 3 finished one a task
    /// notification started; 4 is running; 5 is dormant. All but 5 are live.
    fn world() -> World {
        World::new()
            .with(
                1,
                &[
                    prompt("2026-10-28T10:00:00Z", "human", "go, see ABC-123"),
                    question("2026-10-28T10:01:00Z"),
                ],
            )
            .with(
                2,
                &[
                    prompt("2026-10-28T09:00:00Z", "human", "first"),
                    reply("2026-10-28T09:01:00Z", "first answer"),
                    ended("2026-10-28T09:01:01Z"),
                    prompt("2026-10-28T10:00:00Z", "human", "second"),
                    reply("2026-10-28T10:02:00Z", "second answer"),
                    ended("2026-10-28T10:02:01Z"),
                ],
            )
            .with(
                3,
                &[
                    prompt("2026-10-28T09:30:00Z", "human", "start"),
                    ended("2026-10-28T09:31:00Z"),
                    prompt(
                        "2026-10-28T10:10:00Z",
                        "task-notification",
                        "<task-notification/>",
                    ),
                    reply("2026-10-28T10:11:00Z", "notified"),
                    ended("2026-10-28T10:11:01Z"),
                ],
            )
            .with(4, &[prompt("2026-10-28T10:20:00Z", "human", "work")])
            .with(
                5,
                &[
                    prompt("2026-10-28T08:00:00Z", "human", "old"),
                    ended("2026-10-28T08:01:00Z"),
                ],
            )
            .live(1, Some(ProcessStatus::Waiting))
            .live(2, Some(ProcessStatus::Idle))
            .live(3, Some(ProcessStatus::Idle))
            .live(4, Some(ProcessStatus::Busy))
    }

    /// Mutation: sort the inbox by `since` alone, or by `rank` descending, or leave `Unseen`
    /// out of the key.
    #[test]
    fn the_inbox_is_ordered_blocked_then_trigger_then_oldest_and_seen_cards_sink() {
        let mut world = world();
        assert_eq!(
            inbox_order(&world.model()),
            [sid(1), sid(2), sid(3)],
            "blocked, then the human's turn, then the notification's"
        );
        let mut seen = SeenStore::default();
        // At or after session 2's card (10:02:01): seen. Before session 3's card (10:11:01): not.
        seen.mark_seen(sid(2), at("2026-10-28T10:02:01Z"));
        seen.mark_seen(sid(3), at("2026-10-28T10:11:00Z"));
        world.seen = Source::Read {
            at: at(NOW),
            value: seen,
        };
        let model = world.model();
        assert_eq!(inbox_order(&model), [sid(1), sid(3), sid(2)]);
        let seen_flags: Vec<Seen> = model.inbox.iter().map(|e| e.seen).collect();
        assert_eq!(seen_flags, [Seen::Unseen, Seen::Unseen, Seen::Seen]);
        let tiers: Vec<Tier> = model.inbox.iter().map(|e| e.rank.tier()).collect();
        assert_eq!(tiers, [Tier::Blocked, Tier::TaskNotification, Tier::Human]);
    }

    /// A pin leaves the inbox in urgency order (the blocked card first, the seen one last), and
    /// the sessions list leads with the pinned sessions in the order they were pinned.
    /// Mutation: put the pin in the inbox's sort key, or rank pins by session id.
    #[test]
    fn pins_order_the_sessions_list_and_leave_the_inbox_alone() {
        let mut world = world();
        let mut seen = SeenStore::default();
        seen.pin(sid(3));
        seen.pin(sid(2));
        seen.mark_seen(sid(2), at("2026-10-28T10:02:01Z"));
        world.seen = Source::Read {
            at: at(NOW),
            value: seen,
        };
        let model = world.model();
        assert_eq!(inbox_order(&model), [sid(1), sid(3), sid(2)]);
        let leading: Vec<&SessionId> = model
            .sessions
            .iter()
            .take(2)
            .map(|view| &view.session.id)
            .collect();
        assert_eq!(leading, [&sid(3), &sid(2)]);
    }

    /// A finished turn whose session still waits on something sinks below the other finished
    /// turns; one whose wait is over rises to just after the blocked cards; a blocked card
    /// stays first whatever its session waits on.
    /// Mutation: leave the band out of the sort key, park a blocked card, or treat an ended
    /// wait as still waiting.
    #[test]
    fn a_waiting_session_sinks_and_one_whose_wait_is_over_rises() {
        use crate::ledger::store::Loaded;
        use crate::ledger::{Act, Entry, Ledger, Note, Outcome, Slug, WaitOn};
        let mut world = world();
        let entry = |session: u8, at_text: &str, act| Entry {
            at: at(at_text),
            session: sid(session),
            act,
        };
        let wait = |rollout: &str| Act::Wait {
            on: WaitOn::Rollout(Slug::parse(rollout).unwrap()),
            then: Note::parse("carry on").unwrap(),
            ticket: None,
        };
        let ledger = Ledger::fold([
            // Session 2 (the human's turn) waits on checkout, which never finishes.
            entry(2, "2026-10-28T09:00:00Z", wait("checkout")),
            // Session 1 (blocked on a question) waits too: it stays first.
            entry(1, "2026-10-28T09:00:00Z", wait("checkout")),
            // Session 3 (the notification's turn) waited on portal, which has finished.
            entry(3, "2026-10-28T09:00:00Z", wait("portal")),
            entry(
                4,
                "2026-10-28T09:30:00Z",
                Act::RolloutFinish {
                    rollout: Slug::parse("portal").unwrap(),
                    outcome: Outcome::Shipped,
                    note: None,
                },
            ),
        ]);
        world.ledger = Source::Read {
            at: at(NOW),
            value: Loaded {
                ledger,
                bad_lines: Vec::new(),
            },
        };
        let model = world.model();
        assert_eq!(inbox_order(&model), [sid(1), sid(3), sid(2)]);
        let standings: Vec<&Standing> = model.inbox.iter().map(|e| &e.standing).collect();
        assert_eq!(
            standings,
            [
                &Standing::Now,
                &Standing::Now,
                &Standing::Waiting(Waiting::Release {
                    rollout: Slug::parse("checkout").unwrap(),
                    run: ReleaseRun::NotStarted,
                }),
            ]
        );
    }

    /// A wait on a rollout nobody runs is a release not started, one somebody runs is running;
    /// any other wait is said in the ledger's words; a wait whose time has come is over; no
    /// wait is none.
    /// Mutation: read every rollout wait as not started, or a `--for` wait as a release.
    #[test]
    fn a_ledger_wait_becomes_what_the_card_waits_on() {
        use crate::ledger::{Act, Entry, Ledger, Note, Slug};
        let entry = |session: u8, act| Entry {
            at: at("2026-10-28T09:00:00Z"),
            session: sid(session),
            act,
        };
        let then = || Note::parse("carry on").unwrap();
        let ledger = Ledger::fold([
            entry(
                1,
                Act::Wait {
                    on: WaitOn::Rollout(Slug::parse("checkout").unwrap()),
                    then: then(),
                    ticket: None,
                },
            ),
            entry(
                2,
                Act::Wait {
                    on: WaitOn::Rollout(Slug::parse("portal").unwrap()),
                    then: then(),
                    ticket: None,
                },
            ),
            entry(
                4,
                Act::RolloutStart {
                    rollout: Slug::parse("portal").unwrap(),
                    ships: Vec::new(),
                },
            ),
            entry(
                3,
                Act::Wait {
                    on: WaitOn::For(Note::parse("a teammate's review").unwrap()),
                    then: then(),
                    ticket: None,
                },
            ),
            entry(
                5,
                Act::Wait {
                    on: WaitOn::Until(at("2026-10-28T09:30:00Z")),
                    then: then(),
                    ticket: None,
                },
            ),
        ]);
        let now = at(NOW);
        let standing = |n| wait_standing(ledger.wait_of(&sid(n), now));
        assert_eq!(
            [1, 2, 3, 5, 6].map(standing),
            [
                WaitStanding::Waiting(Waiting::Release {
                    rollout: Slug::parse("checkout").unwrap(),
                    run: ReleaseRun::NotStarted,
                }),
                WaitStanding::Waiting(Waiting::Release {
                    rollout: Slug::parse("portal").unwrap(),
                    run: ReleaseRun::Running,
                }),
                WaitStanding::Waiting(Waiting::Other {
                    on: "a teammate's review".to_owned(),
                }),
                WaitStanding::Over,
                WaitStanding::None,
            ]
        );
    }

    /// A live session the listing says is waiting on a permission prompt is a `Blocked` card:
    /// ranked with the questions, oldest first, carrying its latest turn end as context.
    /// Mutation: drop `Blocked` from `Rank::of`'s blocked tier (it sorts last), or take the
    /// context from the turn end before the latest.
    #[test]
    fn a_permission_wait_is_a_blocked_card_with_the_latest_turn_end_as_context() {
        let mut world = world().with(
            6,
            &[
                prompt("2026-10-28T09:00:00Z", "human", "go"),
                reply("2026-10-28T09:01:00Z", "the context line"),
                ended("2026-10-28T09:01:01Z"),
            ],
        );
        if let Source::Read { value, .. } = &mut world.listing {
            value.push(listed(
                6,
                Some(ProcessStatus::Waiting),
                Some(WaitingFor::PermissionPrompt),
            ));
        }
        if let Source::Read { value, .. } = &mut world.registry {
            value.entries.push(registered(6, Liveness::Live));
        }
        let model = world.model();
        assert_eq!(inbox_order(&model), [sid(6), sid(1), sid(2), sid(3)]);
        let entry = model.inbox.first().unwrap();
        assert_eq!(
            entry.card,
            PendingCard::Blocked {
                session: sid(6),
                on: WaitingFor::PermissionPrompt,
                since: at("2026-10-28T09:01:01Z"),
            }
        );
        assert_eq!(entry.rank.tier(), Tier::Blocked);
        assert_eq!(
            entry.now_line.as_ref().map(Redacted::as_str),
            Some("the context line")
        );
    }

    /// Mutation: give `Running` a card, drop the dormant partition, or file a dormant
    /// session under `sessions`.
    #[test]
    fn each_session_gets_the_activity_the_rule_gives_it() {
        let model = world().model();
        let activity = |n: u8| model.session(&sid(n)).unwrap().session.activity.clone();
        assert_eq!(
            activity(1),
            Activity::Blocked {
                on: WaitingFor::InputNeeded
            }
        );
        assert_eq!(
            activity(2),
            Activity::Idle {
                ended_at: at("2026-10-28T10:02:01Z"),
                listing: ListingStatus::Seen
            }
        );
        assert_eq!(
            activity(3),
            Activity::Idle {
                ended_at: at("2026-10-28T10:11:01Z"),
                listing: ListingStatus::Seen
            }
        );
        assert_eq!(
            activity(4),
            Activity::Running {
                since: at("2026-10-28T10:20:00Z")
            }
        );
        assert!(
            matches!(activity(5), Activity::Dormant { last_seen, .. } if last_seen == at("2026-10-28T08:01:00Z"))
        );
        let live: Vec<_> = model
            .sessions
            .iter()
            .map(|v| v.session.id.clone())
            .collect();
        // Blocked first; of the two idle ones the more recent; then the running one.
        assert_eq!(live, [sid(1), sid(3), sid(2), sid(4)]);
        let dormant: Vec<_> = model.dormant.iter().map(|v| v.session.id.clone()).collect();
        assert_eq!(dormant, [sid(5)]);
        // Running and dormant sessions have no card.
        assert!(model.session(&sid(4)).unwrap().pending.is_none());
        assert!(model.session(&sid(5)).unwrap().pending.is_none());
        // Dormant still shows its last turn end as context, and a resume command.
        let dormant = model.session(&sid(5)).unwrap();
        assert_eq!(dormant.last_turn_ends.len(), 1);
        assert_eq!(
            dormant.resume.as_ref().unwrap().to_string(),
            format!("cd /home/dev/proj && claude --resume {}", sid(5))
        );
    }

    /// Mutation: take `now_line` from the latest turn end instead of the one before, or
    /// skip the 200-character cut.
    #[test]
    fn a_card_carries_the_turn_end_before_it_and_the_links_of_its_session() {
        let model = world().model();
        let entry = |n: u8| model.inbox.iter().find(|e| e.session == sid(n)).unwrap();
        // A finished turn: the one before. A question: the latest turn end (none yet for 1).
        assert_eq!(
            entry(2).now_line.as_ref().map(Redacted::as_str),
            Some("first answer")
        );
        assert_eq!(entry(1).now_line, None);
        assert_eq!(
            entry(3).now_line,
            None,
            "the earlier turn of session 3 said nothing"
        );
        // Links: the question's text and the first prompt; ABC-9 first (the card's text).
        let shown: Vec<String> = entry(1).links.iter().map(|l| l.link.to_string()).collect();
        assert_eq!(shown, ["ABC-9", "ABC-123"]);
        assert_eq!(
            entry(1).links.first().map(|l| l.url.as_str()),
            Some("https://example.atlassian.net/browse/ABC-9")
        );
    }

    /// A long earlier turn end is cut to 200 characters.
    #[test]
    fn the_now_line_is_cut_at_two_hundred_characters() {
        let long = "x".repeat(500);
        let world = World::new()
            .with(
                2,
                &[
                    prompt("2026-10-28T09:00:00Z", "human", "first"),
                    reply("2026-10-28T09:01:00Z", &long),
                    ended("2026-10-28T09:01:01Z"),
                    prompt("2026-10-28T10:00:00Z", "human", "second"),
                    ended("2026-10-28T10:02:01Z"),
                ],
            )
            .live(2, Some(ProcessStatus::Idle));
        let model = world.model();
        let line = model
            .inbox
            .first()
            .and_then(|e| e.now_line.as_ref())
            .unwrap();
        assert_eq!(line.as_str().chars().count(), 200);
    }

    /// With the listing down, a live session whose turn ended keeps its card (the turn-end rule
    /// needs no listing) and says the listing was unavailable; whatever else needs the listing
    /// is `Unknown`, and the failure is in health.
    /// Mutation: fold a failed listing into `Dormant`/`Running`, drop the listing health, or
    /// make the failed listing `Unknown` before the turn-end rule is read (the cards vanish).
    #[test]
    fn a_failed_listing_is_unknown_except_for_a_question_and_a_finished_turn_and_is_in_health() {
        let mut world = world();
        world.listing = Source::Failed {
            at: at(NOW),
            reason: "claude agents timed out".into(),
        };
        let model = world.model();
        let shown = |n: u8| model.session(&sid(n)).unwrap().session.activity.clone();
        assert_eq!(
            shown(1),
            Activity::Blocked {
                on: WaitingFor::InputNeeded
            }
        );
        for (n, ended_at) in [(2, "2026-10-28T10:02:01Z"), (3, "2026-10-28T10:11:01Z")] {
            assert_eq!(
                shown(n),
                Activity::Idle {
                    ended_at: at(ended_at),
                    listing: ListingStatus::Unavailable
                },
                "{n}"
            );
        }
        // Session 4 is running or blocked, which only the listing can tell.
        assert!(
            matches!(shown(4), Activity::Unknown { ref reason } if reason.as_str().contains("claude agents timed out")),
            "{:?}",
            shown(4)
        );
        // Session 5 has no registry entry and no listing: its process cannot be told.
        assert!(matches!(shown(5), Activity::Unknown { .. }));
        assert_eq!(model.unknown_sessions, 2);
        assert_eq!(
            model.health.listing,
            SourceState::Failed {
                at: at(NOW),
                reason: "claude agents timed out".into()
            }
        );
        assert_eq!(
            inbox_order(&model),
            [sid(1), sid(2), sid(3)],
            "the finished turns keep their cards; none is invented for the unknown ones"
        );
    }

    /// A session the code cannot classify is counted, not folded into calm: a background state
    /// this code does not know is `Unknown`, it is in `unknown_sessions`, and it has no card.
    /// Mutation: map an unknown background state to `Dormant`, or leave `unknown_sessions` at 0.
    #[test]
    fn an_unrecognised_background_state_is_unknown_and_counted() {
        let mut world = World::new().with(
            5,
            &[
                prompt("2026-10-28T08:00:00Z", "human", "old"),
                ended("2026-10-28T08:01:00Z"),
            ],
        );
        if let Source::Read { value, .. } = &mut world.registry {
            value.entries.push(registered(
                5,
                Liveness::Gone {
                    reason: GoneReason::NoProcess,
                },
            ));
        }
        if let Source::Read { value, .. } = &mut world.listing {
            value.push(ListedSession::Background(
                crate::sessions::listing::ListedBackground {
                    common: ListedCommon {
                        cwd: "/home/dev/proj".into(),
                        started_at: at("2026-10-28T07:00:00Z"),
                        session_id: Some(sid(5)),
                        name: None,
                    },
                    job_id: crate::sessions::facts::JobId::parse("job1").unwrap(),
                    state: crate::sessions::listing::BackgroundState::Other("paused".into()),
                    process: None,
                },
            ));
        }
        let model = world.model();
        assert!(matches!(
            model.session(&sid(5)).unwrap().session.activity,
            Activity::Unknown { .. }
        ));
        assert_eq!(model.unknown_sessions, 1);
        assert_eq!(model.health.follow_faults.transcripts, 0);
        assert!(model.inbox.is_empty());
    }

    /// A seen mark belongs to the card it was made on: a later turn end of the same session is
    /// a new card and is unseen again, while the card itself and any earlier one stay seen.
    /// Mutation: record the time of the click instead of the card's `since` (compare against
    /// `NOW`), or compare with `>=` the other way round.
    #[test]
    fn a_seen_mark_does_not_cover_a_later_turn_end() {
        let mut world = World::new()
            .with(
                2,
                &[
                    prompt("2026-10-28T10:00:00Z", "human", "first"),
                    ended("2026-10-28T10:02:01Z"),
                ],
            )
            .live(2, Some(ProcessStatus::Idle));
        let mut seen = SeenStore::default();
        // The human saw the card of the turn that ended at 10:02:01, some time before NOW.
        seen.mark_seen(sid(2), at("2026-10-28T10:02:01Z"));
        world.seen = Source::Read {
            at: at(NOW),
            value: seen,
        };
        assert_eq!(world.model().inbox[0].seen, Seen::Seen);

        // A second turn ends at 10:30:01, before NOW and after the mark.
        world = world.with(
            2,
            &[
                prompt("2026-10-28T10:00:00Z", "human", "first"),
                ended("2026-10-28T10:02:01Z"),
                prompt("2026-10-28T10:30:00Z", "human", "second"),
                ended("2026-10-28T10:30:01Z"),
            ],
        );
        let model = world.model();
        assert_eq!(model.inbox.len(), 1);
        assert_eq!(model.inbox[0].seen, Seen::Unseen);
    }

    /// Third-party text is redacted where the model is built: a title carries no token, and the
    /// 200-character cut of an earlier turn end is made after the redaction, so a token that
    /// straddles the cut is masked whole.
    /// Mutation: take the title or the context from the raw text, or cut before redacting.
    #[test]
    fn titles_and_context_lines_are_redacted_before_they_are_cut() {
        let token = format!("ghp_{}", "a1B2".repeat(9));
        // 174 characters, a space, then a 40-character token: the cut at 200 falls inside it.
        let context = format!("{} {token} tail", "x".repeat(174));
        let mut records = vec![
            prompt("2026-10-28T09:00:00Z", "human", "first"),
            reply("2026-10-28T09:01:00Z", &context),
            ended("2026-10-28T09:01:01Z"),
            prompt("2026-10-28T10:00:00Z", "human", "second"),
            ended("2026-10-28T10:02:01Z"),
        ];
        records.push(json!({"type": "ai-title", "aiTitle": format!("Fix {token}"), "sessionId": sid(2).as_str()}));
        let model = World::new()
            .with(2, &records)
            .live(2, Some(ProcessStatus::Idle))
            .model();
        let entry = &model.inbox[0];
        assert_eq!(
            entry.now_line.as_ref().unwrap().as_str(),
            format!("{} [redacted] tail", "x".repeat(174))
        );
        assert_eq!(entry.title.as_ref().unwrap().as_str(), "Fix [redacted]");
        let view = model.session(&sid(2)).unwrap();
        assert_eq!(view.title.as_ref().unwrap().as_str(), "Fix [redacted]");
    }

    /// Mutation: report `Complete` while a transcript is queued, or drop a queued but live
    /// session instead of showing it as unknown.
    #[test]
    fn an_unscanned_transcript_set_is_in_progress_and_a_live_session_waiting_for_it_is_unknown() {
        let mut world = world();
        if let Some(t) = world.transcripts.tracked.get_mut(&sid(2)) {
            t.state = TranscriptState::Queued;
        }
        if let Some(t) = world.transcripts.tracked.get_mut(&sid(5)) {
            t.state = TranscriptState::Queued;
        }
        let model = world.model();
        assert_eq!(
            model.health.transcripts.progress,
            ScanProgress::InProgress {
                scanned: 3,
                total: 5
            }
        );
        assert!(matches!(
            model.session(&sid(2)).unwrap().session.activity,
            Activity::Unknown { ref reason } if reason.as_str().contains("not been scanned")
        ));
        // Not live and not scanned: nothing to say yet, and the progress says so.
        assert!(model.session(&sid(5)).is_none());
        assert_eq!(model.health.transcripts.files, 5);
    }

    /// Mutation: call a transcript that failed to read `Dormant`, or drop it.
    #[test]
    fn an_unreadable_transcript_is_an_unknown_session_and_a_health_count() {
        let mut world = world();
        if let Some(t) = world.transcripts.tracked.get_mut(&sid(5)) {
            t.state = TranscriptState::Unreadable("permission denied".into());
        }
        let model = world.model();
        assert!(matches!(
            model.session(&sid(5)).unwrap().session.activity,
            Activity::Unknown { ref reason } if reason.as_str().contains("permission denied")
        ));
        assert_eq!(model.health.transcripts.unreadable, 1);
        assert_eq!(model.health.transcripts.progress, ScanProgress::Complete);
    }

    /// A Stop hook within the skew of the transcript's end lists the background tasks the
    /// transcript cannot, which makes the finished turn `Watching`; a later Stop is a turn
    /// the transcript has not reached and brings its own text.
    /// Mutation: ignore `background_tasks` of the Stop, or drop the same-turn skew.
    #[test]
    fn the_stop_hook_adds_what_the_transcript_cannot_say() {
        use crate::events::{SessionEvents, StopEvent};
        use crate::types::{BackgroundTask, BackgroundTasks, TaskKind, TaskStatus};
        let task = BackgroundTask {
            id: "t".into(),
            status: TaskStatus::Running,
            description: "watch".into(),
            kind: TaskKind::Shell {
                command: "tail -f".into(),
            },
        };
        let stop = |time: &str, text: &str, tasks: BackgroundTasks| SessionEvents {
            last_stop: Some(StopEvent {
                at: at(time),
                text: Some(AssistantText::of(text)),
                background_tasks: tasks,
                turn_trigger: TurnTrigger::Human,
            }),
            ..SessionEvents::default()
        };
        let mut world = world();
        // Two seconds before the transcript's turn_duration for session 2 (10:02:01).
        world.events.sessions.insert(
            sid(2),
            stop(
                "2026-10-28T10:01:59Z",
                "from the hook",
                BackgroundTasks::Known(vec![task.clone()]),
            ),
        );
        let model = world.model();
        let view = model.session(&sid(2)).unwrap();
        assert_eq!(
            view.session.activity,
            Activity::Watching {
                ended_at: at("2026-10-28T10:02:01Z"),
                listing: ListingStatus::Seen
            }
        );
        // The stop is older than the transcript's end, so the transcript's text stays.
        let Some(PendingCard::Turn { turn, .. }) = &view.pending else {
            panic!("a watching session has a turn card");
        };
        assert_eq!(turn.text.as_str(), "second answer");
        assert_eq!(view.last_turn_ends.len(), 2, "one turn, not two");

        // A Stop an hour after the transcript's last end is a new turn end with its own text.
        world.events.sessions.insert(
            sid(2),
            stop(
                "2026-10-28T11:30:00Z",
                "newer than the file",
                BackgroundTasks::Known(vec![]),
            ),
        );
        let model = world.model();
        let view = model.session(&sid(2)).unwrap();
        assert_eq!(view.last_turn_ends.len(), 3);
        let Some(PendingCard::Turn { turn, since, .. }) = &view.pending else {
            panic!("an idle session has a turn card");
        };
        assert_eq!(turn.text.as_str(), "newer than the file");
        assert_eq!(*since, at("2026-10-28T11:30:00Z"));
    }

    /// A Stop within the skew of the transcript's end is the same turn only when no human
    /// prompt lies between the two: turn end 10:02:01, prompt 10:02:20, Stop 10:02:50 is a
    /// second turn, whichever source recorded the prompt.
    /// Mutation: drop the prompt condition from the same-turn arm (all three red).
    #[test]
    fn a_prompt_between_the_transcript_end_and_the_stop_keeps_them_two_turns() {
        use crate::events::StopEvent;
        use crate::types::BackgroundTasks;
        let stop = StopEvent {
            at: at("2026-10-28T10:02:50Z"),
            text: Some(AssistantText::of("second turn")),
            background_tasks: BackgroundTasks::Known(vec![]),
            turn_trigger: TurnTrigger::Human,
        };
        let first_turn = [
            prompt("2026-10-28T10:00:00Z", "human", "go"),
            reply("2026-10-28T10:02:00Z", "first turn"),
            ended("2026-10-28T10:02:01Z"),
        ];
        let texts = |ends: &[TurnEnd]| -> Vec<String> {
            ends.iter().map(|e| e.text.as_str().to_owned()).collect()
        };

        // The prompt only in the event log.
        let summary = summary_of(&first_turn);
        let ends = turn_ends_newest_first(&summary, Some(&stop), Some(at("2026-10-28T10:02:20Z")));
        assert_eq!(texts(&ends), ["second turn", "first turn"]);

        // The prompt only in the transcript.
        let mut records = first_turn.to_vec();
        records.push(prompt("2026-10-28T10:02:20Z", "human", "and now"));
        let summary = summary_of(&records);
        let ends = turn_ends_newest_first(&summary, Some(&stop), None);
        assert_eq!(texts(&ends), ["second turn", "first turn"]);

        // No prompt between: one turn, the Stop's text newer than the transcript's.
        let summary = summary_of(&first_turn);
        let ends = turn_ends_newest_first(&summary, Some(&stop), Some(at("2026-10-28T10:00:00Z")));
        assert_eq!(texts(&ends), ["second turn"]);
    }

    /// Mutation: leave the dead process in `sessions`, or fold `Gone` with a listed entry
    /// into `Dormant`.
    #[test]
    fn a_dead_process_is_dormant_unless_the_listing_still_has_it() {
        let mut world = world();
        if let Source::Read { value, .. } = &mut world.registry {
            value.entries.push(registered(
                5,
                Liveness::Gone {
                    reason: GoneReason::NoProcess,
                },
            ));
        }
        assert!(matches!(
            world.model().session(&sid(5)).unwrap().session.activity,
            Activity::Dormant { .. }
        ));
        // The listing shows it as a finished background session: still dormant, now saying so.
        if let Source::Read { value, .. } = &mut world.listing {
            value.push(ListedSession::Background(
                crate::sessions::listing::ListedBackground {
                    common: ListedCommon {
                        cwd: "/home/dev/proj".into(),
                        started_at: at("2026-10-28T07:00:00Z"),
                        session_id: Some(sid(5)),
                        name: None,
                    },
                    job_id: crate::sessions::facts::JobId::parse("job1").unwrap(),
                    state: crate::sessions::listing::BackgroundState::Done,
                    process: None,
                },
            ));
        }
        assert!(matches!(
            world.model().session(&sid(5)).unwrap().session.activity,
            Activity::Dormant {
                background: Some(BgState::Done),
                ..
            }
        ));
    }

    /// A finished background session the listing still names, with no transcript on disk
    /// after a complete walk, is gone: on the dormant list, not counted as unknown. Before a
    /// complete walk it is not yet known.
    /// Mutation: call a missing transcript `Missing` whatever the walk said, or leave `Gone`
    /// out of the dormant partition.
    #[test]
    fn a_listed_session_with_no_transcript_is_gone_once_the_walk_is_complete() {
        let mut world = world();
        if let Source::Read { value, .. } = &mut world.listing {
            value.push(ListedSession::Background(
                crate::sessions::listing::ListedBackground {
                    common: ListedCommon {
                        cwd: "/home/dev/deleted".into(),
                        started_at: at("2026-08-14T15:25:41Z"),
                        session_id: Some(sid(10)),
                        name: None,
                    },
                    job_id: crate::sessions::facts::JobId::parse("job10").unwrap(),
                    state: crate::sessions::listing::BackgroundState::Done,
                    process: None,
                },
            ));
        }
        let model = world.model();
        assert_eq!(
            model.session(&sid(10)).unwrap().session.activity,
            Activity::Gone {
                background: Some(BgState::Done)
            }
        );
        assert!(model.dormant.iter().any(|view| view.session.id == sid(10)));
        assert!(!model.sessions.iter().any(|view| view.session.id == sid(10)));
        let before = model.unknown_sessions;

        world.transcripts.discovery = Some(SourceState::Failed {
            at: at(NOW),
            reason: "permission denied".into(),
        });
        let model = world.model();
        assert!(matches!(
            model.session(&sid(10)).unwrap().session.activity,
            Activity::Unknown { .. }
        ));
        assert_eq!(model.unknown_sessions, before + 1);
    }

    /// Mutation: skip the registry source's health, or treat a never-read registry as empty.
    #[test]
    fn every_source_failure_lands_in_health() {
        let mut world = world();
        world.registry = Source::Failed {
            at: at(NOW),
            reason: "cannot read sessions".into(),
        };
        world.seen = Source::NeverRead;
        world.events.health = SourceState::Failed {
            at: at(NOW),
            reason: "no event log".into(),
        };
        let health = world.model().health;
        assert_eq!(
            health.registry,
            SourceState::Failed {
                at: at(NOW),
                reason: "cannot read sessions".into()
            }
        );
        assert_eq!(health.seen, SourceState::NeverRead);
        assert_eq!(
            health.event_log,
            SourceState::Failed {
                at: at(NOW),
                reason: "no event log".into()
            }
        );
    }

    /// A partly readable registry is a failure with a count, not a clean read.
    /// Mutation: report `Ok` whenever the directory was read.
    #[test]
    fn a_registry_with_an_unreadable_entry_is_a_failed_read() {
        let mut world = world();
        if let Source::Read { value, .. } = &mut world.registry {
            value
                .errors
                .push("4303.json is not a session record".into());
        }
        assert!(matches!(
            world.model().health.registry,
            SourceState::Failed { ref reason, .. } if reason.starts_with("1 entries unreadable")
        ));
    }

    /// A refused link is counted, not dropped silently.
    #[test]
    fn an_unrenderable_link_is_counted_in_health() {
        // `Link::url` fails only for parts that do not form a URL; a Slack channel with a
        // space is one.
        let mut world = World::new();
        world.transcripts.tracked.insert(
            sid(7),
            tracked(
                &sid(7),
                summary_of(&[prompt("2026-10-28T10:00:00Z", "human", "x")]),
            ),
        );
        let extracted = Extracted {
            links: vec![Link::SlackThread {
                channel: "C 1".into(),
                ts: "1.2".into(),
            }],
            identifiers: vec![],
        };
        let mut count = 0;
        let views = link_views(
            &extracted,
            &Inputs {
                now: at(NOW),
                listing: &world.listing,
                listing_health: world.listing.state(),
                registry: &world.registry,
                event_log: &world.events,
                transcripts: &world.transcripts,
                gh: &world.gh,
                work_items: Some(&world.work_items),
                reviews: None,
                approvals: None,
                gists: &world.gists,
                usage: &world.usage,
                ledger: &world.ledger,
                jira_relations: None,
                jira_comments: None,
                home: None,
                followups: None,
                saves: &crate::saves::SavesView::default(),
                pam: None,
                seen: &world.seen,
                links: LinkEnv {
                    extractor: &world.extractor,
                    targets: &world.targets,
                    memo: &world.memo,
                },
            },
            &mut count,
        );
        assert_eq!(views.len(), 0);
        assert_eq!(count, 1);
    }
    /// The built model joins a work item to the session whose prompt names its key, holds that
    /// session's prompts redacted for search, and reports the source in health.
    /// Mutation, each ran red: pass no active sessions to the join (the sessions list is
    /// empty), or leave `health.work_items` at `NeverRead`. Holding a prompt unredacted does
    /// not compile: `HeldPrompt::text` is `Redacted`.
    #[test]
    fn the_model_holds_redacted_prompts_and_joins_work_items_to_sessions() {
        use crate::jira::{Observed, Person, Status, StatusCategory, WorkItem, WorkItemKey};
        let secret = "ghp_aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let mut world = World::new()
            .with(
                1,
                &[prompt(
                    "2026-10-28T10:00:00Z",
                    "human",
                    &format!("fix ABC-123 using {secret}"),
                )],
            )
            .live(1, Some(ProcessStatus::Busy));
        let item = WorkItem {
            key: WorkItemKey::classify(
                "ABC-123",
                &[crate::types::JiraPrefix::parse("ABC").unwrap()],
            ),
            summary: "Fix it".to_owned(),
            status: Status {
                name: "To Do".to_owned(),
                category: StatusCategory::ToDo,
            },
            issue_type: "Task".to_owned(),
            level: crate::jira::Level::Item,
            priority: None,
            creator: Some(Person {
                display_name: "Ada Example".to_owned(),
            }),
            assignee: None,
            updated: None,
            created: None,
            description: None,
        };
        world.work_items = crate::jira::Reading::Value {
            observed: Observed {
                items: vec![item],
                read_at: at(NOW),
            },
            age: crate::types::Age::from_secs(0),
            failure: None,
        };
        let model = world.model();
        let crate::model::WorkItemsView::Listed { sections, .. } = &model.work_items else {
            panic!("a read list is `Listed`");
        };
        assert_eq!(
            sections
                .first()
                .and_then(|section| section.rows().next())
                .map(|row| row.sessions.clone()),
            Some(vec![sid(1)])
        );
        assert_eq!(
            model.health.work_items,
            Some(SourceState::Ok { at: at(NOW) })
        );
        let held: Vec<&str> = model
            .prompts
            .get(&sid(1))
            .unwrap()
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect();
        assert_eq!(held, ["fix ABC-123 using [redacted]"]);
    }
}
