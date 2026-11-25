//! A [`RawEvent`] into a [`HookEvent`].
//!
//! Strict about shape, open about values: a documented field with the wrong type is a
//! [`ParseError`] (the caller logs it and keeps the raw line), while an event name, start
//! source, task status or task type this code has not seen becomes an `Unrecognised`/`Other`
//! value.
//!
//! `transcript_path` and `cwd` are part of every payload but are not carried by the typed
//! events; sessions are located from the transcript directory, not from the hook. The prompt
//! text of `UserPromptSubmit` is reduced to its [`TurnTrigger`] and dropped.

use serde_json::Value;

use super::{HookEvent, HookEventName, RawEvent, StartSource};
use crate::types::{
    AssistantText, BackgroundTask, BackgroundTasks, PromptFacts, SessionId, SessionIdError,
    TaskKind, TaskStatus, TurnTrigger,
};
use crate::util::json::{FieldError, Fields};

/// The payload was read and does not have the documented shape.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("payload is not a JSON object")]
    NotAnObject,
    #[error("field {0}")]
    Field(#[from] FieldError),
    #[error("field `session_id`: {0}")]
    SessionId(#[from] SessionIdError),
}

pub fn parse(raw: &RawEvent) -> Result<HookEvent, ParseError> {
    let payload = Fields::of(&raw.payload).map_err(|_| ParseError::NotAnObject)?;
    let name = payload.required_str("hook_event_name")?;
    let Some(event) = HookEventName::ALL
        .into_iter()
        .find(|known| known.as_str() == name)
    else {
        return Ok(HookEvent::Unrecognised {
            session: session_id(&payload).ok(),
            event_name: name.to_owned(),
        });
    };
    let session = session_id(&payload)?;
    Ok(match event {
        HookEventName::SessionStart => HookEvent::SessionStart {
            session,
            at: raw.received_at,
            source: start_source(payload.required_str("source")?),
        },
        HookEventName::UserPromptSubmit => HookEvent::UserPromptSubmit {
            session,
            at: raw.received_at,
            trigger: TurnTrigger::of_prompt(
                payload.required_str("prompt")?,
                PromptFacts::default(),
            ),
        },
        HookEventName::Stop => HookEvent::Stop {
            session,
            at: raw.received_at,
            last_assistant_message: payload
                .optional_str("last_assistant_message")?
                .map(AssistantText::of),
            background_tasks: background_tasks(&payload)?,
        },
    })
}

fn session_id(payload: &Fields<'_>) -> Result<SessionId, ParseError> {
    Ok(SessionId::parse(payload.required_str("session_id")?)?)
}

fn start_source(text: &str) -> StartSource {
    match text {
        "startup" => StartSource::Startup,
        "resume" => StartSource::Resume,
        "clear" => StartSource::Clear,
        "compact" => StartSource::Compact,
        other => StartSource::Other(other.to_owned()),
    }
}

/// An absent or null list is "the payload does not say"; a present list, even empty, is a
/// statement.
fn background_tasks(payload: &Fields<'_>) -> Result<BackgroundTasks, ParseError> {
    match payload.optional_array("background_tasks")? {
        None => Ok(BackgroundTasks::Unknown),
        Some(items) => items
            .iter()
            .map(background_task)
            .collect::<Result<Vec<_>, _>>()
            .map(BackgroundTasks::Known),
    }
}

fn background_task(item: &Value) -> Result<BackgroundTask, ParseError> {
    let task = Fields::scoped(item, "background_tasks[]")?;
    let kind = match task.required_str("type")? {
        "shell" => TaskKind::Shell {
            command: task.required_str("command")?.to_owned(),
        },
        "subagent" => TaskKind::Subagent {
            agent_type: task.required_str("agent_type")?.to_owned(),
        },
        other => TaskKind::Other(other.to_owned()),
    };
    Ok(BackgroundTask {
        id: task.required_str("id")?.to_owned(),
        status: task_status(task.required_str("status")?),
        description: task.required_str("description")?.to_owned(),
        kind,
    })
}

fn task_status(text: &str) -> TaskStatus {
    match text {
        "running" => TaskStatus::Running,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        other => TaskStatus::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testutil::{at, fixture, id};
    use crate::types::TurnTrigger;
    use serde_json::Map;
    use serde_json::json;

    const ARRIVED: &str = "2026-10-28T12:00:00Z";

    fn parse_fixture(name: &str) -> HookEvent {
        parse(&fixture(name, ARRIVED)).unwrap()
    }

    /// A fixture with `edit` applied to its payload.
    fn edited(name: &str, edit: impl FnOnce(&mut Map<String, Value>)) -> RawEvent {
        let mut raw = fixture(name, ARRIVED);
        edit(raw.payload.as_object_mut().unwrap());
        raw
    }

    fn known_tasks(event: HookEvent) -> Option<Vec<BackgroundTask>> {
        if let HookEvent::Stop {
            background_tasks: BackgroundTasks::Known(tasks),
            ..
        } = event
        {
            Some(tasks)
        } else {
            None
        }
    }

    /// The fixtures hold exactly the key sets observed in Claude Code 2.1.312, so the parser
    /// is exercised on the payloads it will receive and not on tidier ones.
    #[test]
    fn fixtures_carry_exactly_the_observed_keys() {
        let common = ["cwd", "hook_event_name", "session_id", "transcript_path"];
        let stop = [
            "background_tasks",
            "effort",
            "last_assistant_message",
            "permission_mode",
            "prompt_id",
            "session_crons",
            "stop_hook_active",
        ];
        let table: [(&str, Vec<&str>); 8] = [
            ("session_start_startup.json", vec!["source"]),
            (
                "session_start_resume.json",
                vec![
                    "source",
                    "context_tokens",
                    "estimated_cache_write_usd",
                    "prompt_cache_likely_expired",
                    "seconds_since_last_response",
                ],
            ),
            (
                "user_prompt_submit_human.json",
                vec!["permission_mode", "prompt", "prompt_id"],
            ),
            (
                "user_prompt_submit_task_notification.json",
                vec!["permission_mode", "prompt", "prompt_id"],
            ),
            ("stop_pending.json", stop.to_vec()),
            ("stop_idle.json", stop.to_vec()),
            (
                "subagent_stop.json",
                [
                    &stop[..],
                    &["agent_id", "agent_type", "agent_transcript_path"],
                ]
                .concat(),
            ),
            ("session_end.json", vec!["reason"]),
        ];
        for (name, extra) in table {
            let mut expected: Vec<&str> = common.iter().copied().chain(extra).collect();
            expected.sort_unstable();
            let raw = fixture(name, ARRIVED);
            let mut actual: Vec<&str> = raw
                .payload
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            actual.sort_unstable();
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn session_start_startup() {
        assert_eq!(
            parse_fixture("session_start_startup.json"),
            HookEvent::SessionStart {
                session: id(),
                at: at(ARRIVED),
                source: StartSource::Startup
            }
        );
    }

    #[test]
    fn session_start_resume_ignores_the_resume_only_keys() {
        assert_eq!(
            parse_fixture("session_start_resume.json"),
            HookEvent::SessionStart {
                session: id(),
                at: at(ARRIVED),
                source: StartSource::Resume
            }
        );
    }

    #[test]
    fn session_start_sources() {
        for (text, expected) in [
            ("startup", StartSource::Startup),
            ("resume", StartSource::Resume),
            ("clear", StartSource::Clear),
            ("compact", StartSource::Compact),
            ("teleport", StartSource::Other("teleport".into())),
        ] {
            let raw = edited("session_start_startup.json", |p| {
                p.insert("source".into(), json!(text));
            });
            assert_eq!(
                parse(&raw).unwrap(),
                HookEvent::SessionStart {
                    session: id(),
                    at: at(ARRIVED),
                    source: expected
                }
            );
        }
    }

    #[test]
    fn user_prompt_submit_keeps_the_trigger_and_drops_the_prompt() {
        let event = parse_fixture("user_prompt_submit_human.json");
        assert_eq!(
            event,
            HookEvent::UserPromptSubmit {
                session: id(),
                at: at(ARRIVED),
                trigger: TurnTrigger::Human
            }
        );
        assert!(!format!("{event:?}").contains("Placeholder human prompt"));
    }

    #[test]
    fn a_machine_started_prompt_gets_its_trigger_at_parse() {
        assert_eq!(
            parse_fixture("user_prompt_submit_task_notification.json"),
            HookEvent::UserPromptSubmit {
                session: id(),
                at: at(ARRIVED),
                trigger: TurnTrigger::TaskNotification
            }
        );
    }

    #[test]
    fn stop_with_pending_tasks_of_both_kinds() {
        assert_eq!(
            parse_fixture("stop_pending.json"),
            HookEvent::Stop {
                session: id(),
                at: at(ARRIVED),
                last_assistant_message: Some(AssistantText::of("Placeholder assistant message.")),
                background_tasks: BackgroundTasks::Known(vec![
                    BackgroundTask {
                        id: "task-001".into(),
                        status: TaskStatus::Running,
                        description: "Placeholder shell task".into(),
                        kind: TaskKind::Shell {
                            command: "sleep 600".into()
                        },
                    },
                    BackgroundTask {
                        id: "task-002".into(),
                        status: TaskStatus::Running,
                        description: "Placeholder subagent task".into(),
                        kind: TaskKind::Subagent {
                            agent_type: "general-purpose".into()
                        },
                    },
                ]),
            }
        );
    }

    #[test]
    fn stop_with_an_empty_task_list_knows_nothing_is_running() {
        assert_eq!(
            parse_fixture("stop_idle.json"),
            HookEvent::Stop {
                session: id(),
                at: at(ARRIVED),
                last_assistant_message: Some(AssistantText::of("Placeholder assistant message.")),
                background_tasks: BackgroundTasks::Known(vec![]),
            }
        );
    }

    #[test]
    fn stop_without_the_optional_fields_does_not_claim_to_know() {
        let raw = edited("stop_idle.json", |p| {
            p.remove("background_tasks");
            p.remove("last_assistant_message");
        });
        assert_eq!(
            parse(&raw).unwrap(),
            HookEvent::Stop {
                session: id(),
                at: at(ARRIVED),
                last_assistant_message: None,
                background_tasks: BackgroundTasks::Unknown,
            }
        );
    }

    #[test]
    fn unseen_task_statuses_and_types_are_values_not_errors() {
        let raw = edited("stop_idle.json", |p| {
            p.insert(
                "background_tasks".into(),
                json!([
                    { "id": "t1", "type": "monitor", "status": "paused",
                      "description": "Placeholder monitor", "server": "example" },
                    { "id": "t2", "type": "shell", "status": "completed",
                      "description": "Placeholder", "command": "true" },
                    { "id": "t3", "type": "shell", "status": "failed",
                      "description": "Placeholder", "command": "false" },
                ]),
            );
        });
        let tasks = known_tasks(parse(&raw).unwrap()).unwrap();
        let shapes: Vec<_> = tasks.into_iter().map(|t| (t.status, t.kind)).collect();
        assert_eq!(
            shapes,
            vec![
                (
                    TaskStatus::Other("paused".into()),
                    TaskKind::Other("monitor".into())
                ),
                (
                    TaskStatus::Completed,
                    TaskKind::Shell {
                        command: "true".into()
                    }
                ),
                (
                    TaskStatus::Failed,
                    TaskKind::Shell {
                        command: "false".into()
                    }
                ),
            ]
        );
    }

    #[test]
    fn events_the_tool_does_not_register_are_unrecognised_with_their_name() {
        for (name, event_name) in [
            ("subagent_stop.json", "SubagentStop"),
            ("session_end.json", "SessionEnd"),
        ] {
            assert_eq!(
                parse_fixture(name),
                HookEvent::Unrecognised {
                    session: Some(id()),
                    event_name: event_name.into()
                }
            );
        }
    }

    #[test]
    fn unknown_event_without_a_usable_session_still_parses() {
        let raw = edited("session_end.json", |p| {
            p.insert("session_id".into(), json!("garbage"));
        });
        assert_eq!(
            parse(&raw).unwrap(),
            HookEvent::Unrecognised {
                session: None,
                event_name: "SessionEnd".into()
            }
        );
    }

    #[test]
    fn malformed_payloads_are_errors_naming_the_problem() {
        let not_object = RawEvent {
            received_at: at(ARRIVED),
            payload: json!([1, 2]),
        };
        assert_eq!(parse(&not_object), Err(ParseError::NotAnObject));

        let no_name = edited("stop_idle.json", |p| {
            p.remove("hook_event_name");
        });
        assert_eq!(
            parse(&no_name),
            Err(ParseError::Field(FieldError::missing("hook_event_name")))
        );

        let no_session = edited("stop_idle.json", |p| {
            p.remove("session_id");
        });
        assert_eq!(
            parse(&no_session),
            Err(ParseError::Field(FieldError::missing("session_id")))
        );

        let no_source = edited("session_start_startup.json", |p| {
            p.remove("source");
        });
        assert_eq!(
            parse(&no_source),
            Err(ParseError::Field(FieldError::missing("source")))
        );

        // A prompt event without its text must not be read as a human prompt.
        let no_prompt = edited("user_prompt_submit_human.json", |p| {
            p.remove("prompt");
        });
        assert_eq!(
            parse(&no_prompt),
            Err(ParseError::Field(FieldError::missing("prompt")))
        );

        let bad_id = edited("stop_idle.json", |p| {
            p.insert("session_id".into(), json!("nope"));
        });
        assert!(matches!(parse(&bad_id), Err(ParseError::SessionId(_))));

        let wrong_message = edited("stop_idle.json", |p| {
            p.insert("last_assistant_message".into(), json!(7));
        });
        assert_eq!(
            parse(&wrong_message),
            Err(ParseError::Field(FieldError::WrongType {
                key: "last_assistant_message".to_owned(),
                expected: "a string"
            }))
        );
    }

    #[test]
    fn malformed_task_lists_are_errors_naming_the_field() {
        let cases = [
            (
                json!("none"),
                ParseError::Field(FieldError::WrongType {
                    key: "background_tasks".to_owned(),
                    expected: "an array",
                }),
            ),
            (
                json!(["tests"]),
                ParseError::Field(FieldError::WrongType {
                    key: "background_tasks[]".to_owned(),
                    expected: "a JSON object",
                }),
            ),
            (
                json!([{ "type": "shell", "status": "running", "description": "d", "command": "c" }]),
                ParseError::Field(FieldError::missing("background_tasks[].id")),
            ),
            (
                json!([{ "id": "t", "type": "shell", "status": "running", "description": "d" }]),
                ParseError::Field(FieldError::missing("background_tasks[].command")),
            ),
            (
                json!([{ "id": "t", "type": "subagent", "status": "running", "description": "d" }]),
                ParseError::Field(FieldError::missing("background_tasks[].agent_type")),
            ),
            (
                json!([{ "id": "t", "type": "shell", "status": 3, "description": "d", "command": "c" }]),
                ParseError::Field(FieldError::WrongType {
                    key: "background_tasks[].status".to_owned(),
                    expected: "a string",
                }),
            ),
        ];
        for (tasks, expected) in cases {
            let raw = edited("stop_idle.json", |p| {
                p.insert("background_tasks".into(), tasks.clone());
            });
            assert_eq!(parse(&raw), Err(expected), "tasks {tasks}");
        }
    }
}
