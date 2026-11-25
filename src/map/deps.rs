//! The dependency graph the map draws: the active sessions, and the rollouts, tickets, topics
//! and Slack threads that tie them together.
//!
//! Every edge is something a session declared in the coordination ledger, or a collision its
//! own writes made: what it owns, the rollout it runs or waits on, the tickets a rollout
//! ships, and the ticket or thread it wrote to where another session wrote too. A ticket two
//! sessions merely mention is not an edge: that is what made the old map interesting rather
//! than useful.

use std::collections::{BTreeSet, HashMap, HashSet};

use jiff::SignedDuration;

use crate::jira::links::Dependency;
use crate::ledger::shared::Shared;
use crate::ledger::{Ledger, Slug, Subject, WaitOn};
use crate::types::outward::Target;
use crate::types::{Instant, JiraKey, SessionId};

/// How recent a collision has to be to draw an edge; the Owners page keeps the week.
pub const COLLISION_EDGE_FOR: SignedDuration = SignedDuration::from_hours(24);

/// Something a session is tied to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Thing {
    Rollout(Slug),
    /// A ticket or a topic.
    Subject(String),
    /// A Slack thread: the channel and the thread's timestamp.
    Thread(String, String),
}

impl Thing {
    fn of_subject(subject: &Subject) -> Self {
        match subject {
            Subject::Rollout(name) => Self::Rollout(name.clone()),
            Subject::Ticket(_) | Subject::Topic(_) => Self::Subject(subject.to_string()),
        }
    }

    /// The subject this thing is, for looking up its owner; a thread has none.
    pub fn subject(&self) -> Option<Subject> {
        match self {
            Self::Rollout(name) => Some(Subject::Rollout(name.clone())),
            Self::Subject(text) => Subject::parse(text).ok(),
            Self::Thread(..) => None,
        }
    }

    /// The map's column: rollouts between the sessions and what they ship.
    pub const fn is_rollout(&self) -> bool {
        matches!(self, Self::Rollout(_))
    }
}

/// How a session or a rollout is tied to a thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relation {
    Owns,
    Runs,
    /// `over` once what the session waited on has come.
    WaitsOn {
        over: bool,
    },
    /// From a rollout to a ticket it ships.
    Ships,
    /// The session wrote to it while another session wrote to it too, or owned it.
    Collided,
    /// From a ticket to one its Jira links say it blocks, while neither is done.
    Blocks,
}

/// One edge. `from` is a session for every relation but [`Relation::Ships`], whose `from` is
/// the rollout, and [`Relation::Blocks`], whose `from` is the blocking ticket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum From {
    Session(SessionId),
    Rollout(Slug),
    /// A ticket, as written.
    Ticket(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: From,
    pub to: Thing,
    pub relation: Relation,
}

/// The graph, in drawing order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deps {
    /// Active sessions with at least one edge, ordered so that sessions tied to the same things
    /// sit together.
    pub sessions: Vec<SessionId>,
    /// Active sessions with no edge: listed beside the graph, not drawn in it.
    pub alone: Vec<SessionId>,
    /// The things, each column in the order that keeps edges short.
    pub rollouts: Vec<Thing>,
    pub targets: Vec<Thing>,
    pub edges: Vec<Edge>,
    /// Owned things whose owner is no longer active: nobody is on them.
    pub orphaned: BTreeSet<Thing>,
}

/// The tickets the map draws on its own account: claimed, shipped by a running rollout, or in a
/// recent collision. Their Jira links are what [`Relation::Blocks`] edges are read from.
pub fn tickets(ledger: &Ledger, shared: &[Shared], now: Instant) -> Vec<JiraKey> {
    let claimed = ledger
        .claims()
        .into_iter()
        .filter_map(|(subject, _)| subject.ticket().cloned());
    let shipped = ledger
        .rollouts()
        .into_iter()
        .filter_map(|(_, rollout)| rollout.running.as_ref())
        .flat_map(|run| run.ships.iter().filter_map(|s| s.ticket().cloned()));
    let collided = shared
        .iter()
        .filter(|item| {
            item.latest()
                .is_some_and(|at| at >= now - COLLISION_EDGE_FOR)
        })
        .filter_map(|item| match &item.target {
            Target::Ticket(key) => Some(key.clone()),
            Target::Slack { .. } => None,
        });
    let mut seen = std::collections::HashSet::new();
    claimed
        .chain(shipped)
        .chain(collided)
        .filter(|key| seen.insert(key.to_string()))
        .collect()
}

/// The graph of `active` sessions (in the order the model ranks them) from the ledger, the
/// recent collisions, and the open dependencies between the tickets.
pub fn build(
    active: &[SessionId],
    ledger: &Ledger,
    shared: &[Shared],
    dependencies: &[Dependency],
    now: Instant,
) -> Deps {
    let is_active = |session: &SessionId| active.contains(session);
    let mut edges = BTreeSet::new();
    let mut orphaned = BTreeSet::new();
    for (subject, held) in ledger.claims() {
        let thing = Thing::of_subject(subject);
        if is_active(&held.session) {
            edges.insert(Edge {
                from: From::Session(held.session.clone()),
                to: thing,
                relation: Relation::Owns,
            });
        } else {
            orphaned.insert(thing);
        }
    }
    for (name, rollout) in ledger.rollouts() {
        let Some(run) = &rollout.running else {
            continue;
        };
        if is_active(&run.session) {
            edges.insert(Edge {
                from: From::Session(run.session.clone()),
                to: Thing::Rollout(name.clone()),
                relation: Relation::Runs,
            });
        }
        for shipped in &run.ships {
            edges.insert(Edge {
                from: From::Rollout(name.clone()),
                to: Thing::of_subject(shipped),
                relation: Relation::Ships,
            });
        }
    }
    for (session, state) in ledger.waits(now) {
        let to = match &state.wait().on {
            WaitOn::Rollout(name) => Thing::Rollout(name.clone()),
            WaitOn::Subject(subject) => Thing::of_subject(subject),
            WaitOn::Until(_) | WaitOn::For(_) => continue,
        };
        if is_active(session) {
            edges.insert(Edge {
                from: From::Session(session.clone()),
                to,
                relation: Relation::WaitsOn {
                    over: state.is_over(),
                },
            });
        }
    }
    for item in shared.iter().filter(|item| {
        item.latest()
            .is_some_and(|at| at >= now - COLLISION_EDGE_FOR)
    }) {
        let thing = match &item.target {
            Target::Ticket(key) => Thing::Subject(key.to_string()),
            Target::Slack {
                channel,
                thread: Some(ts),
            } => Thing::Thread(channel.clone(), ts.clone()),
            Target::Slack { thread: None, .. } => continue,
        };
        for writer in item.writers().into_iter().filter(|w| is_active(w)) {
            edges.insert(Edge {
                from: From::Session(writer.clone()),
                to: thing.clone(),
                relation: Relation::Collided,
            });
        }
    }
    // The links read for the work items reach tickets no session touches; the map draws a
    // dependency only where one of its ends is already on it.
    let on_map: HashSet<JiraKey> = tickets(ledger, shared, now).into_iter().collect();
    for dependency in dependencies
        .iter()
        .filter(|d| on_map.contains(&d.blocker) || on_map.contains(&d.blocked))
    {
        edges.insert(Edge {
            from: From::Ticket(dependency.blocker.to_string()),
            to: Thing::Subject(dependency.blocked.to_string()),
            relation: Relation::Blocks,
        });
    }
    order(active, edges.into_iter().collect(), orphaned)
}

/// Lays the columns out: sessions keep the model's order, grouped so that sessions sharing a
/// thing sit next to each other; each thing then sits at the mean position of what points at
/// it, so most edges run straight across.
fn order(active: &[SessionId], edges: Vec<Edge>, orphaned: BTreeSet<Thing>) -> Deps {
    let (sessions, alone) = group_sessions(active, &edges);
    let (rollouts, targets) = place_things(&sessions, &edges);
    Deps {
        sessions,
        alone,
        rollouts,
        targets,
        edges,
        orphaned,
    }
}

/// The sessions tied to something, in `active`'s order with each joining the group of the last
/// earlier session it shares a thing with; and the sessions tied to nothing.
fn group_sessions(active: &[SessionId], edges: &[Edge]) -> (Vec<SessionId>, Vec<SessionId>) {
    let tied: BTreeSet<&SessionId> = edges
        .iter()
        .filter_map(|edge| match &edge.from {
            From::Session(session) => Some(session),
            From::Rollout(_) | From::Ticket(_) => None,
        })
        .collect();
    // Group: a session joins the group of the first earlier session it shares a thing with.
    let things_of = |session: &SessionId| -> BTreeSet<&Thing> {
        edges
            .iter()
            .filter(|edge| edge.from == From::Session(session.clone()))
            .map(|edge| &edge.to)
            .collect()
    };
    let mut sessions: Vec<SessionId> = Vec::new();
    for session in active.iter().filter(|session| tied.contains(session)) {
        let mine = things_of(session);
        let after = sessions
            .iter()
            .rposition(|placed| !things_of(placed).is_disjoint(&mine));
        match after {
            Some(at) => sessions.insert(at.saturating_add(1), session.clone()),
            None => sessions.push(session.clone()),
        }
    }
    let alone = active
        .iter()
        .filter(|session| !tied.contains(session))
        .cloned()
        .collect();
    (sessions, alone)
}

/// The rollouts and the targets, each column ordered by the mean row of what points at each.
fn place_things(sessions: &[SessionId], edges: &[Edge]) -> (Vec<Thing>, Vec<Thing>) {
    let row: HashMap<&SessionId, usize> = sessions.iter().zip(0_usize..).collect();
    // A thing's place: the mean row of the sessions (or, for a shipped ticket, the rollouts'
    // sessions) tied to it.
    let mut places: HashMap<Thing, (usize, usize)> = HashMap::new();
    let note = |places: &mut HashMap<Thing, (usize, usize)>, thing: &Thing, at: usize| {
        let entry = places.entry(thing.clone()).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(at);
        entry.1 = entry.1.saturating_add(1);
    };
    for edge in edges {
        if let From::Session(session) = &edge.from
            && let Some(at) = row.get(session)
        {
            note(&mut places, &edge.to, *at);
        }
    }
    let rollout_place = |name: &Slug, places: &HashMap<Thing, (usize, usize)>| {
        places
            .get(&Thing::Rollout(name.clone()))
            .map(|(sum, count)| sum.checked_div(*count).unwrap_or_default())
    };
    let shipped: Vec<(Thing, usize)> = edges
        .iter()
        .filter_map(|edge| match &edge.from {
            From::Rollout(name) => rollout_place(name, &places).map(|at| (edge.to.clone(), at)),
            From::Session(_) | From::Ticket(_) => None,
        })
        .collect();
    for (thing, at) in &shipped {
        note(&mut places, thing, *at);
    }
    // A blocking ticket sits by the ticket it blocks, and the other way round, wherever one of
    // the two has a place already.
    let mean = |places: &HashMap<Thing, (usize, usize)>, thing: &Thing| {
        places
            .get(thing)
            .map(|(sum, count)| sum.checked_div(*count).unwrap_or_default())
    };
    let blocking: Vec<(Thing, usize)> = edges
        .iter()
        .filter_map(|edge| match &edge.from {
            From::Ticket(blocker) => {
                let blocker = Thing::Subject(blocker.clone());
                match (mean(&places, &blocker), mean(&places, &edge.to)) {
                    (None, Some(at)) => Some((blocker, at)),
                    (Some(at), None) => Some((edge.to.clone(), at)),
                    (Some(_), Some(_)) | (None, None) => None,
                }
            }
            From::Session(_) | From::Rollout(_) => None,
        })
        .collect();
    for (thing, at) in &blocking {
        note(&mut places, thing, *at);
    }
    // A rollout no active session runs or waits on, and what only it ships or blocks, go last.
    let last = sessions.len();
    let mut things: Vec<(Thing, usize)> = edges
        .iter()
        .flat_map(|edge| {
            let from = match &edge.from {
                From::Rollout(name) => Some(Thing::Rollout(name.clone())),
                From::Ticket(key) => Some(Thing::Subject(key.clone())),
                From::Session(_) => None,
            };
            from.into_iter().chain(std::iter::once(edge.to.clone()))
        })
        .collect::<BTreeSet<Thing>>()
        .into_iter()
        .map(|thing| {
            let at = places
                .get(&thing)
                .map_or(last, |(sum, count)| sum.checked_div(*count).unwrap_or(last));
            (thing, at)
        })
        .collect();
    things.sort_by(|(a, at_a), (b, at_b)| at_a.cmp(at_b).then_with(|| a.cmp(b)));
    things
        .into_iter()
        .map(|(thing, _)| thing)
        .partition(Thing::is_rollout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::shared::{OwnerThen, SharedWrite};
    use crate::ledger::{Act, Entry, Note, Subject};
    use crate::testutil::session_id as sid;
    use crate::types::JiraKey;
    use crate::types::outward::WriteKind;

    fn at(minute: i64) -> Instant {
        Instant::from_second(1_793_900_000 + minute * 60).unwrap()
    }

    fn subject(text: &str) -> Subject {
        Subject::parse(text).unwrap()
    }

    fn edge(from: From, to: Thing, relation: Relation) -> Edge {
        Edge { from, to, relation }
    }

    fn ticket(key: &str) -> Thing {
        Thing::Subject(key.to_owned())
    }

    /// Session 2 owns ABC-1, session 1 runs the checkout rollout that ships it, session 3 waits on
    /// checkout, session 6 waits on ABC-1 itself, Jira says ABC-7 blocks ABC-1, an ended session 5
    /// owns ABC-5, sessions 1 and 2 both wrote ABC-1 just now and both wrote ABC-9 two days
    /// ago; session 4 has nothing. ABC-7 sits beside the ticket it blocks.
    /// Mutation: draw edges for an ended session, keep a collision older than a day, drop
    /// the shipped edge, drop a wait on a ticket, drop the blocking edge, or leave a session with
    /// no edge in the graph.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one scenario, its ledger and its expected edges, read top to bottom"
    )]
    fn the_graph_holds_what_sessions_declared_and_recent_collisions_only() {
        let checkout = Slug::parse("checkout").unwrap();
        let entry = |session: u8, minute: i64, act| Entry {
            at: at(minute),
            session: sid(session),
            act,
        };
        let ledger = Ledger::fold([
            entry(
                2,
                0,
                Act::Claim {
                    subject: subject("ABC-1"),
                    took_from: None,
                },
            ),
            entry(
                5,
                0,
                Act::Claim {
                    subject: subject("ABC-5"),
                    took_from: None,
                },
            ),
            entry(
                1,
                1,
                Act::RolloutStart {
                    rollout: checkout.clone(),
                    ships: vec![subject("ABC-1")],
                },
            ),
            entry(
                3,
                2,
                Act::Wait {
                    on: WaitOn::Rollout(checkout.clone()),
                    then: Note::parse("go on").unwrap(),
                    ticket: None,
                },
            ),
            entry(
                6,
                3,
                Act::Wait {
                    on: WaitOn::Subject(subject("ABC-1")),
                    then: Note::parse("rebase").unwrap(),
                    ticket: None,
                },
            ),
        ]);
        let write = |session: u8, minute: i64| SharedWrite {
            at: at(minute),
            session: sid(session),
            kind: WriteKind::Comment,
            excerpt: None,
            owner_then: OwnerThen::Unowned,
        };
        let shared = [
            Shared {
                target: Target::Ticket(JiraKey::parse("ABC-1").unwrap()),
                writes: vec![write(1, 5), write(2, 6)],
            },
            Shared {
                target: Target::Ticket(JiraKey::parse("ABC-9").unwrap()),
                writes: vec![write(1, -2 * 24 * 60), write(2, -2 * 24 * 60)],
            },
        ];
        let active = [sid(1), sid(2), sid(3), sid(4), sid(6)];
        // ABC-50 and ABC-51 are on no session's map: their dependency is read for the work
        // items page and left off the map.
        let dependencies = [
            Dependency {
                blocker: JiraKey::parse("ABC-7").unwrap(),
                blocked: JiraKey::parse("ABC-1").unwrap(),
            },
            Dependency {
                blocker: JiraKey::parse("ABC-50").unwrap(),
                blocked: JiraKey::parse("ABC-51").unwrap(),
            },
        ];
        let deps = build(&active, &ledger, &shared, &dependencies, at(10));
        let edges: BTreeSet<Edge> = deps.edges.iter().cloned().collect();
        let expected: BTreeSet<Edge> = [
            edge(From::Session(sid(2)), ticket("ABC-1"), Relation::Owns),
            edge(
                From::Session(sid(1)),
                Thing::Rollout(checkout.clone()),
                Relation::Runs,
            ),
            edge(
                From::Rollout(checkout.clone()),
                ticket("ABC-1"),
                Relation::Ships,
            ),
            edge(
                From::Session(sid(3)),
                Thing::Rollout(checkout.clone()),
                Relation::WaitsOn { over: false },
            ),
            edge(From::Session(sid(1)), ticket("ABC-1"), Relation::Collided),
            edge(From::Session(sid(2)), ticket("ABC-1"), Relation::Collided),
            edge(
                From::Session(sid(6)),
                ticket("ABC-1"),
                Relation::WaitsOn { over: false },
            ),
            edge(
                From::Ticket("ABC-7".to_owned()),
                ticket("ABC-1"),
                Relation::Blocks,
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(edges, expected);
        assert_eq!(deps.alone, [sid(4)]);
        assert_eq!(deps.sessions.len(), 4);
        assert_eq!(deps.rollouts, [Thing::Rollout(checkout)]);
        assert_eq!(deps.targets, [ticket("ABC-1"), ticket("ABC-7")]);
        assert_eq!(deps.orphaned, [ticket("ABC-5")].into_iter().collect());
    }
}
