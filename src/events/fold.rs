//! The event stream reduced to what each session last did.
//!
//! The pending rule downstream reads this as one comparison: a session is pending when
//! `last_stop.at` is later than `last_human_prompt_at`, or when there is no human prompt at
//! all. Machine-started prompts are recorded (`last_prompt`, `turn_trigger`) but never move
//! `last_human_prompt_at`.

use std::collections::HashMap;
use std::hash::BuildHasher;

use super::{HookEvent, StartSource};
use crate::types::{AssistantText, BackgroundTasks, Instant, SessionId, TurnTrigger};

/// What a Stop is told about the turn it ends when no prompt was logged before it, as with a
/// session that started before the hook was registered.
const NO_PROMPT_SEEN: &str = "no prompt seen";

/// A `UserPromptSubmit`, without its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEvent {
    pub at: Instant,
    pub trigger: TurnTrigger,
}

/// A `Stop`: one turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopEvent {
    pub at: Instant,
    pub text: Option<AssistantText>,
    pub background_tasks: BackgroundTasks,
    /// The trigger of the latest prompt logged before this Stop in the same session.
    pub turn_trigger: TurnTrigger,
}

/// The latest of each kind of event for one session, in log order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEvents {
    pub last_prompt: Option<PromptEvent>,
    pub last_stop: Option<StopEvent>,
    /// The latest prompt whose trigger is `Human`.
    pub last_human_prompt_at: Option<Instant>,
    pub starts: Vec<(Instant, StartSource)>,
}

impl SessionEvents {
    fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Start { at, source } => self.starts.push((at, source)),
            SessionEvent::Prompt { at, trigger } => {
                if trigger == TurnTrigger::Human {
                    self.last_human_prompt_at = Some(at);
                }
                self.last_prompt = Some(PromptEvent { at, trigger });
            }
            SessionEvent::Stop {
                at,
                text,
                background_tasks,
            } => {
                let turn_trigger = self.last_prompt.as_ref().map_or_else(
                    || TurnTrigger::Unknown(NO_PROMPT_SEEN.to_owned()),
                    |prompt| prompt.trigger.clone(),
                );
                self.last_stop = Some(StopEvent {
                    at,
                    text,
                    background_tasks,
                    turn_trigger,
                });
            }
        }
    }
}

/// A hook event with its session split off, so [`fold_into`] can route it.
enum SessionEvent {
    Start {
        at: Instant,
        source: StartSource,
    },
    Prompt {
        at: Instant,
        trigger: TurnTrigger,
    },
    Stop {
        at: Instant,
        text: Option<AssistantText>,
        background_tasks: BackgroundTasks,
    },
}

/// The per-session state after `events`, which must be in log order.
pub fn fold(events: impl Iterator<Item = HookEvent>) -> HashMap<SessionId, SessionEvents> {
    let mut sessions = HashMap::new();
    fold_into(&mut sessions, events);
    sessions
}

/// Applies `events`, in log order, on top of `sessions`: the live poller feeds it each batch
/// from [`super::EventLog::read_from`] and never re-reads the log.
pub fn fold_into<S: BuildHasher>(
    sessions: &mut HashMap<SessionId, SessionEvents, S>,
    events: impl Iterator<Item = HookEvent>,
) {
    for event in events {
        let (session, event) = match event {
            HookEvent::SessionStart {
                session,
                at,
                source,
            } => (session, SessionEvent::Start { at, source }),
            HookEvent::UserPromptSubmit {
                session,
                at,
                trigger,
            } => (session, SessionEvent::Prompt { at, trigger }),
            HookEvent::Stop {
                session,
                at,
                last_assistant_message,
                background_tasks,
            } => (
                session,
                SessionEvent::Stop {
                    at,
                    text: last_assistant_message,
                    background_tasks,
                },
            ),
            // Not an event this tool registers for, or one with no usable session.
            HookEvent::Unrecognised { .. } => continue,
        };
        sessions.entry(session).or_default().apply(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testutil::{at, id};

    fn other_id() -> SessionId {
        SessionId::parse("11111111-2222-4333-8444-555555555555").unwrap()
    }

    fn prompt(session: SessionId, time: &str, trigger: TurnTrigger) -> HookEvent {
        HookEvent::UserPromptSubmit {
            session,
            at: at(time),
            trigger,
        }
    }

    fn stop(session: SessionId, time: &str, message: &str) -> HookEvent {
        HookEvent::Stop {
            session,
            at: at(time),
            last_assistant_message: Some(AssistantText::of(message)),
            background_tasks: BackgroundTasks::Known(vec![]),
        }
    }

    fn start(session: SessionId, time: &str, source: StartSource) -> HookEvent {
        HookEvent::SessionStart {
            session,
            at: at(time),
            source,
        }
    }

    #[test]
    fn interleaved_sessions_fold_independently() {
        let (a, b) = (id(), other_id());
        let events = vec![
            start(a.clone(), "2026-10-28T09:00:00Z", StartSource::Startup),
            start(b.clone(), "2026-10-28T09:00:01Z", StartSource::Resume),
            prompt(a.clone(), "2026-10-28T09:01:00Z", TurnTrigger::Human),
            prompt(b.clone(), "2026-10-28T09:01:30Z", TurnTrigger::Human),
            stop(a.clone(), "2026-10-28T09:02:00Z", "a first"),
            prompt(a.clone(), "2026-10-28T09:03:00Z", TurnTrigger::Human),
            stop(b.clone(), "2026-10-28T09:04:00Z", "b first"),
            stop(a.clone(), "2026-10-28T09:05:00Z", "a second"),
        ];
        let folded = fold(events.into_iter());
        assert_eq!(folded.len(), 2);

        let a = &folded[&a];
        assert_eq!(
            a.starts,
            vec![(at("2026-10-28T09:00:00Z"), StartSource::Startup)]
        );
        assert_eq!(a.last_human_prompt_at, Some(at("2026-10-28T09:03:00Z")));
        assert_eq!(
            a.last_prompt,
            Some(PromptEvent {
                at: at("2026-10-28T09:03:00Z"),
                trigger: TurnTrigger::Human
            })
        );
        assert_eq!(
            a.last_stop,
            Some(StopEvent {
                at: at("2026-10-28T09:05:00Z"),
                text: Some(AssistantText::of("a second")),
                background_tasks: BackgroundTasks::Known(vec![]),
                turn_trigger: TurnTrigger::Human,
            })
        );

        let b = &folded[&b];
        assert_eq!(
            b.starts,
            vec![(at("2026-10-28T09:00:01Z"), StartSource::Resume)]
        );
        assert_eq!(b.last_human_prompt_at, Some(at("2026-10-28T09:01:30Z")));
        assert_eq!(
            b.last_stop.as_ref().map(|s| s.text.clone()),
            Some(Some(AssistantText::of("b first")))
        );
    }

    #[test]
    fn a_stop_with_no_preceding_prompt_says_so_and_is_pending() {
        let folded = fold(std::iter::once(stop(
            id(),
            "2026-10-28T09:02:00Z",
            "orphan",
        )));
        let session = &folded[&id()];
        let last_stop = session.last_stop.as_ref().unwrap();
        assert_eq!(
            last_stop.turn_trigger,
            TurnTrigger::Unknown("no prompt seen".into())
        );
        assert_eq!(session.last_human_prompt_at, None);
        assert_eq!(session.last_prompt, None);
    }

    #[test]
    fn a_stop_is_attributed_to_the_prompt_of_its_own_session() {
        let (a, b) = (id(), other_id());
        let folded = fold(
            vec![
                prompt(
                    a.clone(),
                    "2026-10-28T09:01:00Z",
                    TurnTrigger::TaskNotification,
                ),
                prompt(b.clone(), "2026-10-28T09:01:30Z", TurnTrigger::Human),
                stop(a.clone(), "2026-10-28T09:02:00Z", "a"),
            ]
            .into_iter(),
        );
        assert_eq!(
            folded[&a].last_stop.as_ref().unwrap().turn_trigger,
            TurnTrigger::TaskNotification
        );
    }

    /// The pending rule is one comparison because a machine prompt does not move
    /// `last_human_prompt_at`.
    #[test]
    fn only_human_prompts_move_the_human_prompt_time() {
        let folded = fold(
            vec![
                prompt(id(), "2026-10-28T09:01:00Z", TurnTrigger::Human),
                stop(id(), "2026-10-28T09:02:00Z", "done"),
                prompt(id(), "2026-10-28T09:03:00Z", TurnTrigger::TaskNotification),
                prompt(id(), "2026-10-28T09:03:30Z", TurnTrigger::Peer),
                stop(id(), "2026-10-28T09:04:00Z", "handled notification"),
            ]
            .into_iter(),
        );
        let session = &folded[&id()];
        let human = session.last_human_prompt_at.unwrap();
        let stopped = session.last_stop.as_ref().unwrap().at;
        assert_eq!(human, at("2026-10-28T09:01:00Z"));
        assert!(stopped > human);
        assert_eq!(
            session.last_stop.as_ref().unwrap().turn_trigger,
            TurnTrigger::Peer
        );
    }

    #[test]
    fn unrecognised_events_change_nothing() {
        let folded = fold(
            vec![HookEvent::Unrecognised {
                session: Some(id()),
                event_name: "SessionEnd".into(),
            }]
            .into_iter(),
        );
        assert!(folded.is_empty());
    }

    #[test]
    fn fold_into_continues_from_earlier_state() {
        let mut sessions = fold(std::iter::once(prompt(
            id(),
            "2026-10-28T09:01:00Z",
            TurnTrigger::Human,
        )));
        fold_into(
            &mut sessions,
            std::iter::once(stop(id(), "2026-10-28T09:02:00Z", "later batch")),
        );
        let stop = sessions[&id()].last_stop.as_ref().unwrap();
        assert_eq!(stop.turn_trigger, TurnTrigger::Human);
    }
}
