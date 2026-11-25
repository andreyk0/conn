//! Tests for the transcript reader.
//!
//! Method: `parse` is checked line by line against `tests/fixtures/transcript/records.jsonl`,
//! one record per kind, with the expectation written out by hand per line. `fold` is checked
//! on the two mini transcripts (`interactive.jsonl`, `headless.jsonl`) and on short sequences
//! built from JSON values, one behaviour per sequence. Reading is checked on files in a
//! scratch directory, and the one property that matters, that chunking never changes the
//! result, is a proptest over arbitrary chunk boundaries.

use crate::util::redact::Redacted;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use serde_json::{Value, json};

use crate::types::{
    AskUserQuestion, AssistantText, BackgroundTasks, Instant, MultiSelect, Question,
    QuestionOption, SafeUrl, TurnEnd, TurnTrigger,
};

use crate::testutil::Scratch;

use super::lines::{Feed, LINE_BYTES_MAX};
use super::tail;
use super::*;

const RECORDS: &str = include_str!("../../tests/fixtures/transcript/records.jsonl");
const INTERACTIVE: &str = include_str!("../../tests/fixtures/transcript/interactive.jsonl");
const HEADLESS: &str = include_str!("../../tests/fixtures/transcript/headless.jsonl");

fn t(text: &str) -> Instant {
    text.parse().unwrap()
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcript")
        .join(name)
}

// --- parse ---------------------------------------------------------------------------------

/// A compact, hand-readable rendering of what a record says, for the table below.
fn describe(record: &Record) -> String {
    match record {
        Record::User(user) => match &user.content {
            UserContent::Prompt(prompt) => format!("user:prompt:{:?}", prompt.trigger),
            UserContent::Injected => "user:injected".to_owned(),
            UserContent::ToolResults(results) => {
                let one = results.first().unwrap();
                let content = match one.content {
                    ToolResultContent::Inline(_) => "inline",
                    ToolResultContent::Persisted { .. } => "persisted",
                };
                format!(
                    "user:results:{}:{:?}:{content}",
                    one.tool_use_id, one.outcome
                )
            }
        },
        Record::Assistant(assistant) => {
            let stop = assistant
                .stop_reason
                .as_ref()
                .map_or("none".to_owned(), |reason| format!("{reason:?}"));
            let block = match assistant.blocks.first().unwrap() {
                Block::Text(_) => "text".to_owned(),
                Block::Thinking => "thinking".to_owned(),
                Block::ToolUse(ToolUse {
                    call: ToolCall::AskUserQuestion(_),
                    ..
                }) => "ask".to_owned(),
                Block::ToolUse(ToolUse {
                    call: ToolCall::Outward(write),
                    ..
                }) => format!("write({})", write.target),
                Block::ToolUse(ToolUse {
                    call: ToolCall::Other { name },
                    ..
                }) => format!("tool_use({name})"),
                Block::Other { kind } => format!("other({kind})"),
            };
            let chain = match assistant.env.chain {
                Chain::Main => "",
                Chain::Sidechain => ":sidechain",
            };
            format!(
                "assistant:{}:{stop}:{block}{chain}",
                assistant.message_id.as_deref().unwrap()
            )
        }
        Record::System(system) => match &system.event {
            SystemEvent::TurnDuration { .. } => "system:turn_duration".to_owned(),
            SystemEvent::Other { subtype } => format!("system:other({subtype})"),
        },
        Record::Attachment(attachment) => match &attachment.attachment {
            Attachment::QueuedCommand(prompt) => format!("attachment:queued:{:?}", prompt.trigger),
            Attachment::Other { kind } => format!("attachment:other({kind})"),
        },
        Record::Meta(Meta::AiTitle(_)) => "meta:ai-title".to_owned(),
        Record::Meta(Meta::CustomTitle(_)) => "meta:custom-title".to_owned(),
        Record::Meta(Meta::LastPrompt(prompt)) => format!(
            "meta:last-prompt({})",
            if prompt.is_some() { "Some" } else { "None" }
        ),
        Record::Meta(Meta::PrLink(_)) => "meta:pr-link".to_owned(),
        Record::Meta(Meta::PermissionMode(_)) => "meta:permission-mode".to_owned(),
        Record::Unknown { kind } => format!("unknown:{kind}"),
    }
}

fn record_at(line_number: usize) -> Record {
    parse_line(RECORDS.lines().nth(line_number - 1).unwrap()).unwrap()
}

fn user_at(line_number: usize) -> UserRecord {
    let Record::User(user) = record_at(line_number) else {
        panic!("line {line_number} is not a user record")
    };
    user
}

fn results_at(line_number: usize) -> Vec<ToolResult> {
    let UserContent::ToolResults(results) = user_at(line_number).content else {
        panic!("line {line_number} is not a carrier")
    };
    results
}

fn assistant_at(line_number: usize) -> AssistantRecord {
    let Record::Assistant(assistant) = record_at(line_number) else {
        panic!("line {line_number} is not an assistant record")
    };
    assistant
}

/// Mutation: map `peer` to `Human` in `TurnTrigger::of_origin_kind` (line 4 goes red); drop the
/// `is_meta` test in `prompt_trigger` (line 9 goes red); treat a missing `is_error` as an
/// error (line 15 goes red).
#[test]
fn every_record_kind_in_the_fixture_reads_as_what_it_is() {
    let expected = [
        "user:prompt:Human",
        "user:prompt:Human",
        "user:prompt:TaskNotification",
        "user:prompt:Peer",
        "user:prompt:Coordinator",
        "user:prompt:AutoContinuation",
        "user:prompt:CompactionSummary",
        "user:prompt:LocalCommand",
        "user:injected",
        "user:injected",
        "user:prompt:Human",
        "user:prompt:Unknown(\"sdk\")",
        "user:prompt:Scheduled",
        "user:prompt:Unknown(\"future-kind\")",
        "user:results:toolu_placeholder_ok:Ok:inline",
        "user:results:toolu_placeholder_err:Error:inline",
        "user:results:toolu_placeholder_big:Ok:persisted",
        "user:results:toolu_placeholder_ask:Ok:inline",
        "assistant:msg_placeholder_r1:ToolUse:thinking",
        "assistant:msg_placeholder_r2:EndTurn:text",
        "assistant:msg_placeholder_r3:ToolUse:tool_use(Bash)",
        "assistant:msg_placeholder_r4:ToolUse:ask",
        "assistant:msg_placeholder_r5:none:text",
        "assistant:msg_placeholder_r6:Other(\"max_tokens\"):other(future_block)",
        "system:turn_duration",
        "system:other(compact_boundary)",
        "attachment:queued:Human",
        "attachment:queued:TaskNotification",
        "attachment:other(total_tokens_reminder)",
        "meta:ai-title",
        "meta:last-prompt(Some)",
        "meta:last-prompt(None)",
        "meta:pr-link",
        "meta:permission-mode",
        "unknown:mode",
        "assistant:msg_placeholder_s1:EndTurn:text:sidechain",
        "unknown:queue-operation",
    ];
    let actual: Vec<String> = RECORDS
        .lines()
        .map(|line| describe(&parse_line(line).unwrap()))
        .collect();
    assert_eq!(actual, expected);
}

/// Mutation: join blocks of every type instead of `text` blocks only (the base64 payload
/// `AAAA` appears in the text).
#[test]
fn prompt_text_is_its_text_blocks_without_images() {
    let UserContent::Prompt(prompt) = user_at(2).content else {
        panic!("line 2 is a prompt")
    };
    assert_eq!(prompt.text, "Placeholder prompt with image.");
}

/// Mutation: return `Inline` whenever the stub text is present, ignoring `persistedOutputPath`.
#[test]
fn a_persisted_stub_names_its_file_and_size() {
    assert_eq!(
        results_at(17).first().unwrap().content,
        ToolResultContent::Persisted {
            path: PathBuf::from(
                "/home/dev/.claude/projects/-home-dev-proj/1d8f3c52-a07b-4e96-b2d1-8c4f6a09e357/tool-results/placeholder.txt"
            ),
            size: Some(34611),
        }
    );
}

/// Mutation: read `answers` from the wrong key, or drop the `answers` extraction.
#[test]
fn an_answered_question_carries_its_answers() {
    let answers = results_at(18).first().unwrap().answers.clone().unwrap();
    assert_eq!(
        answers.into_iter().collect::<Vec<_>>(),
        [("Placeholder question?".to_owned(), "Option A".to_owned())]
    );
}

/// Mutation: map `multiSelect: true` to `One`; drop the options' `description`.
#[test]
fn an_ask_user_question_call_is_parsed_in_full() {
    let assistant = assistant_at(22);
    let Some(Block::ToolUse(tool_use)) = assistant.blocks.first() else {
        panic!("line 22 holds a tool_use")
    };
    assert_eq!(tool_use.id, "toolu_placeholder_ask");
    assert_eq!(
        tool_use.call,
        ToolCall::AskUserQuestion(
            AskUserQuestion::new(vec![Question {
                header: Some(Redacted::of("Placeholder")),
                question: Redacted::of("Placeholder question?"),
                multi_select: MultiSelect::Many,
                options: vec![
                    QuestionOption {
                        label: Redacted::of("Option A"),
                        description: Some(Redacted::of("Placeholder description A.")),
                    },
                    QuestionOption {
                        label: Redacted::of("Option B"),
                        description: Some(Redacted::of("Placeholder description B.")),
                    },
                ],
            }])
            .unwrap()
        )
    );
}

/// Mutation: read the PR number from `prRepository`, or swap url and repository.
#[test]
fn a_pr_link_has_number_url_and_repo() {
    assert_eq!(
        record_at(33),
        Record::Meta(Meta::PrLink(PrLink {
            number: 42,
            url: SafeUrl::parse("https://github.com/example-org/example-repo/pull/42").unwrap(),
            repo: "example-org/example-repo".to_owned(),
        }))
    );
}

/// Mutation: ignore `isSidechain` in `envelope`.
#[test]
fn the_sidechain_flag_is_read() {
    assert_eq!(assistant_at(36).env.chain, Chain::Sidechain);
    assert_eq!(assistant_at(20).env.chain, Chain::Main);
}

/// Mutation: replace the `Field`/`Json` returns with `Ok(Record::Unknown { .. })`.
#[test]
fn lines_that_cannot_be_read_are_errors_not_records() {
    let session = "1d8f3c52-a07b-4e96-b2d1-8c4f6a09e357";
    for line in ["not json", "[1,2]"] {
        assert!(
            matches!(parse_line(line), Err(LineError::Json(_))),
            "{line}"
        );
    }
    let field = |kind, field, problem| LineError::Field {
        kind,
        field,
        problem,
    };
    let cases = [
        ("{}".to_owned(), field("record", "type", FieldProblem::Missing)),
        (
            json!({"type": "system", "subtype": "turn_duration"}).to_string(),
            field("system", "timestamp", FieldProblem::Missing),
        ),
        (
            json!({"type": "system", "subtype": "x", "timestamp": "yesterday"}).to_string(),
            field("system", "timestamp", FieldProblem::Malformed),
        ),
        (
            json!({"type": "system", "subtype": "x", "timestamp": "2026-10-28T12:00:00Z", "sessionId": "nope"})
                .to_string(),
            field("system", "sessionId", FieldProblem::Malformed),
        ),
        (
            json!({"type": "assistant", "timestamp": "2026-10-28T12:00:00Z", "sessionId": session,
                   "message": {"id": "m", "content": [{"type": "tool_use", "id": "t", "name": "AskUserQuestion", "input": {}}]}})
                .to_string(),
            field("assistant", "input.questions", FieldProblem::Malformed),
        ),
        (
            // A call with no question cannot be answered, so it is not a question.
            json!({"type": "assistant", "timestamp": "2026-10-28T12:00:00Z", "sessionId": session,
                   "message": {"id": "m", "content": [{"type": "tool_use", "id": "t", "name": "AskUserQuestion", "input": {"questions": []}}]}})
                .to_string(),
            field("assistant", "input.questions", FieldProblem::Malformed),
        ),
        (
            json!({"type": "pr-link", "prNumber": 4_294_967_296_u64, "prUrl": "u", "prRepository": "r"})
                .to_string(),
            field("pr-link", "prNumber", FieldProblem::Malformed),
        ),
    ];
    for (line, expected) in cases {
        assert_eq!(parse_line(&line), Err(expected), "{line}");
    }
}

/// Mutation: build the `PrLink` from `SafeUrl::parse(..).unwrap_or(fallback)`, or return
/// `Record::Unknown` for a refused URL -> the line is not an error any more -> red.
#[test]
fn a_pr_link_with_an_unsafe_url_is_a_counted_line_error() {
    for url in [
        "javascript:alert(1)",
        "http://github.com/o/r/pull/1",
        "https://a@evil.example/x",
    ] {
        let line = json!({"type": "pr-link", "prNumber": 1, "prUrl": url, "prRepository": "o/r"})
            .to_string();
        let expected = LineError::Field {
            kind: "pr-link",
            field: "prUrl",
            problem: FieldProblem::UnsafeUrl,
        };
        assert_eq!(parse_line(&line), Err(expected.clone()), "{url}");
        assert!(!expected.to_string().contains(url));
        let mut summary = Summary::default();
        fold_event(&mut summary, &TailEvent::from(parse_line(&line)));
        assert_eq!(summary.line_errors, 1);
        assert_eq!(summary.pr_links, []);
    }
}

/// Mutation: put the offending value into the message (`error.to_string()`).
#[test]
fn a_json_error_never_repeats_transcript_text() {
    // Serde's own message for this quotes the offending string.
    let error = parse_line(r#"{"type": "user", "isSidechain": "placeholder-secret"}"#).unwrap_err();
    assert!(matches!(error, LineError::Json(_)));
    assert!(!error.to_string().contains("placeholder-secret"));
}

// --- bytes to lines ------------------------------------------------------------------------

fn unknown_line_of(len: usize) -> Vec<u8> {
    let head = br#"{"type":"padding","pad":""#;
    let tail = br#""}"#;
    let fill = len - head.len() - tail.len();
    let mut line = head.to_vec();
    line.extend(std::iter::repeat_n(b'x', fill));
    line.extend_from_slice(tail);
    line
}

/// Mutation: change `>` to `>=` in `finish_line` or `extend_partial` (the exact-limit line
/// becomes an error); change it to `> LINE_BYTES_MAX + 1` (the limit-plus-one line parses).
#[test]
fn a_line_of_exactly_the_limit_parses_and_one_byte_more_is_counted() {
    for (len, expect_ok) in [(LINE_BYTES_MAX, true), (LINE_BYTES_MAX + 1, false)] {
        let mut stream = unknown_line_of(len);
        stream.push(b'\n');
        let mut feed = Feed::new();
        let mut lines = Vec::new();
        // A power of two, so a chunk ends exactly at the limit of the exact-limit line.
        for chunk in stream.chunks(64 * 1024) {
            lines.extend(feed.push(chunk));
        }
        assert_eq!(lines.len(), 1);
        assert_eq!(lines.first().unwrap().is_ok(), expect_ok, "length {len}");
        if !expect_ok {
            assert_eq!(lines.first().unwrap(), &Err(LineError::TooLong));
        }
        assert_eq!(feed.unterminated(), 0);
    }
}

/// Mutation: keep buffering after the overflow (the held bytes exceed the limit).
#[test]
fn an_overlong_line_is_never_buffered() {
    let mut feed = Feed::new();
    let chunk = vec![b'x'; 1_000_000];
    for _ in 0..10 {
        let _ = feed.push(&chunk);
    }
    assert_eq!(feed.unterminated(), 10_000_000);
    // The Debug text lists every byte a feed holds, so a short one means nothing is held.
    assert!(format!("{feed:?}").len() < 200);
    let after = feed.push(b"\n{\"type\":\"mode\"}\n");
    assert_eq!(
        after,
        [
            Err(LineError::TooLong),
            Ok(Record::Unknown {
                kind: "mode".to_owned()
            })
        ]
    );
}

/// Mutation: stop skipping empty lines (`decode` returns the JSON error for them).
#[test]
fn blank_lines_are_not_records_and_bad_utf8_is_counted() {
    let mut feed = Feed::new();
    let lines = feed.push(b"\n\n{\"type\":\"mode\"}\n\xff\xfe\n");
    assert_eq!(
        lines,
        [
            Ok(Record::Unknown {
                kind: "mode".to_owned()
            }),
            Err(LineError::NotUtf8)
        ]
    );
}

// --- fold: turns ---------------------------------------------------------------------------

const SESSION: &str = "7c1e5a90-3b2d-4f68-a1c4-52e9d0b7f813";

fn base(kind: &str, at: &str) -> Value {
    json!({"type": kind, "timestamp": at, "sessionId": SESSION, "cwd": "/home/dev/proj", "isSidechain": false})
}

fn with(mut value: Value, extra: &Value) -> Value {
    let Value::Object(fields) = extra else {
        panic!("extra must be an object")
    };
    for (key, field) in fields {
        value[key] = field.clone();
    }
    value
}

fn user_prompt(at: &str, origin: &str, text: &str) -> Value {
    with(
        base("user", at),
        &json!({"origin": {"kind": origin}, "message": {"role": "user", "content": text}}),
    )
}

fn assistant(at: &str, message: &str, stop: Option<&str>, block: &Value) -> Value {
    with(
        base("assistant", at),
        &json!({"message": {"id": message, "role": "assistant", "content": [block], "stop_reason": stop}}),
    )
}

fn text_end(at: &str, message: &str, text: &str) -> Value {
    assistant(
        at,
        message,
        Some("end_turn"),
        &json!({"type": "text", "text": text}),
    )
}

fn ask_call(at: &str, message: &str, id: &str, question: &str) -> Value {
    assistant(
        at,
        message,
        Some("tool_use"),
        &json!({"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": [
            {"question": question, "header": "H", "multiSelect": false, "options": [{"label": "Yes", "description": "d"}]}
        ]}}),
    )
}

fn carrier(at: &str, tool_use_id: &str) -> Value {
    with(
        base("user", at),
        &json!({"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": "out"}]}}),
    )
}

fn duration(at: &str) -> Value {
    with(
        base("system", at),
        &json!({"subtype": "turn_duration", "durationMs": 1}),
    )
}

fn fold_values(values: &[Value]) -> Summary {
    let mut summary = Summary::default();
    for value in values {
        let line = value.to_string();
        fold_event(&mut summary, &TailEvent::from(parse_line(&line)));
    }
    summary
}

fn turn_end(at: &str, trigger: TurnTrigger, text: &str) -> TurnEnd {
    TurnEnd {
        at: t(at),
        trigger,
        text: AssistantText::of(text),
        background_tasks: BackgroundTasks::Unknown,
        asked: None,
    }
}

/// Mutation: drop the `Duration` upgrade in `end_by_duration` (the end stays at 10:00:09.900),
/// or push a second turn end there (the list has two entries).
#[test]
fn turn_duration_ends_the_turn_the_text_block_provisionally_ended() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:09.900Z", "m1", "done"),
        duration("2026-10-28T10:00:10Z"),
    ]);
    assert_eq!(
        summary.turn_ends,
        [turn_end("2026-10-28T10:00:10Z", TurnTrigger::Human, "done")]
    );
}

/// Mutation: remove `retract_text_end`'s `pop`, or call it for `Settled` ends too.
#[test]
fn a_tool_result_right_after_an_end_turn_block_takes_the_end_back() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:05Z", "m1", "almost"),
        carrier("2026-10-28T10:00:06Z", "toolu_1"),
        text_end("2026-10-28T10:00:09Z", "m2", "done"),
    ]);
    assert_eq!(
        summary.turn_ends,
        [turn_end("2026-10-28T10:00:09Z", TurnTrigger::Human, "done")]
    );
}

/// Mutation: let `settle` run for no record (the carrier after the attachment retracts).
#[test]
fn a_record_between_the_block_and_a_carrier_confirms_the_end() {
    let attachment = with(
        base("attachment", "2026-10-28T10:00:06Z"),
        &json!({"attachment": {"type": "task_reminder"}}),
    );
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:05Z", "m1", "done"),
        attachment,
        carrier("2026-10-28T10:00:07Z", "toolu_1"),
    ]);
    assert_eq!(summary.turn_ends.len(), 1);
}

/// Mutation: drop the `TailOfEndedMessage` return (the second block of the message makes a
/// second turn end).
#[test]
fn two_blocks_of_the_message_that_ended_the_turn_are_one_end() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:05Z", "m1", "done"),
        assistant(
            "2026-10-28T10:00:05.100Z",
            "m1",
            Some("end_turn"),
            &json!({"type": "text", "text": "done again"}),
        ),
        duration("2026-10-28T10:00:06Z"),
    ]);
    assert_eq!(
        summary.turn_ends,
        [turn_end("2026-10-28T10:00:06Z", TurnTrigger::Human, "done")]
    );
}

/// Mutation: end turns only in the `end_turn` path (the interrupted turn has no end), or take
/// the stop reason of a `tool_use` message as an end.
#[test]
fn a_turn_without_an_end_turn_block_ends_at_its_turn_duration() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:01Z", "m0", "first part"),
        carrier("2026-10-28T10:00:02Z", "toolu_1"),
        assistant(
            "2026-10-28T10:00:03Z",
            "m1",
            Some("tool_use"),
            &json!({"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {}}),
        ),
        duration("2026-10-28T10:00:04Z"),
    ]);
    assert_eq!(
        summary.turn_ends,
        [turn_end(
            "2026-10-28T10:00:04Z",
            TurnTrigger::Human,
            "first part"
        )]
    );
}

/// Mutation: give the turn the trigger of the previous prompt, or `Human` by default.
#[test]
fn each_turn_keeps_the_trigger_that_started_it() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        text_end("2026-10-28T10:00:01Z", "m1", "one"),
        duration("2026-10-28T10:00:02Z"),
        user_prompt("2026-10-28T10:01:00Z", "task-notification", "<n/>"),
        text_end("2026-10-28T10:01:01Z", "m2", "two"),
        duration("2026-10-28T10:01:02Z"),
    ]);
    let triggers: Vec<_> = summary
        .turn_ends
        .iter()
        .map(|end| end.trigger.clone())
        .collect();
    assert_eq!(
        triggers,
        [TurnTrigger::Human, TurnTrigger::TaskNotification]
    );
}

/// Mutation: remove the `continues` rule in `start` (the end is attributed to the compaction).
#[test]
fn a_compaction_inside_a_running_turn_does_not_take_over_its_trigger() {
    let compaction = with(
        base("user", "2026-10-28T10:00:30Z"),
        &json!({"isCompactSummary": true, "message": {"role": "user", "content": "This session is being continued"}}),
    );
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        compaction.clone(),
        text_end("2026-10-28T10:01:00Z", "m1", "done"),
        duration("2026-10-28T10:01:01Z"),
        compaction,
        text_end("2026-10-28T10:02:00Z", "m2", "after"),
        duration("2026-10-28T10:02:01Z"),
    ]);
    let triggers: Vec<_> = summary
        .turn_ends
        .iter()
        .map(|end| end.trigger.clone())
        .collect();
    assert_eq!(
        triggers,
        [TurnTrigger::Human, TurnTrigger::CompactionSummary]
    );
}

/// Mutation: seed the initial phase with `Human`.
#[test]
fn a_file_that_starts_mid_turn_has_an_unattributed_first_turn() {
    let summary = fold_values(&[
        text_end("2026-10-28T10:00:01Z", "m1", "one"),
        duration("2026-10-28T10:00:02Z"),
    ]);
    assert_eq!(
        summary.turn_ends.first().unwrap().trigger,
        TurnTrigger::Unknown("unattributed".to_owned())
    );
}

/// Mutation: remove the `Chain::Sidechain` arm's early exit in `on_envelope`.
#[test]
fn a_subagent_record_changes_nothing() {
    let mut sidechain = text_end("2026-10-28T10:00:05Z", "m1", "subagent");
    sidechain["isSidechain"] = json!(true);
    sidechain["cwd"] = json!("/home/dev/elsewhere");
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        sidechain,
    ]);
    assert_eq!(summary.turn_ends, []);
    assert_eq!(summary.cwd, Some(PathBuf::from("/home/dev/proj")));
    assert_eq!(summary.last_record_at, Some(t("2026-10-28T10:00:00Z")));
}

// --- fold: questions -----------------------------------------------------------------------

fn the_ask(question: &str) -> AskUserQuestion {
    AskUserQuestion::new(vec![Question {
        header: Some(Redacted::of("H")),
        question: Redacted::of(question),
        multi_select: MultiSelect::One,
        options: vec![QuestionOption {
            label: Redacted::of("Yes"),
            description: Some(Redacted::of("d")),
        }],
    }])
    .unwrap()
}

/// Mutation: never remove the call on its tool result; or remove by the wrong id.
#[test]
fn a_question_is_pending_until_its_tool_result_arrives() {
    let asked = [
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        ask_call("2026-10-28T10:00:01Z", "m1", "toolu_a", "First?"),
    ];
    assert_eq!(fold_values(&asked).pending_ask, Some(the_ask("First?")));

    let mut answered = asked.to_vec();
    answered.push(carrier("2026-10-28T10:00:09Z", "toolu_a"));
    assert_eq!(fold_values(&answered).pending_ask, None);

    let mut other_answered = asked.to_vec();
    other_answered.push(carrier("2026-10-28T10:00:09Z", "toolu_other"));
    assert_eq!(
        fold_values(&other_answered).pending_ask,
        Some(the_ask("First?"))
    );
}

/// Mutation: return the first unanswered call instead of the last.
#[test]
fn the_pending_question_is_the_last_unanswered_one() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        ask_call("2026-10-28T10:00:01Z", "m1", "toolu_a", "First?"),
        ask_call("2026-10-28T10:00:02Z", "m2", "toolu_b", "Second?"),
    ]);
    assert_eq!(summary.pending_ask, Some(the_ask("Second?")));

    let answered_last = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        ask_call("2026-10-28T10:00:01Z", "m1", "toolu_a", "First?"),
        ask_call("2026-10-28T10:00:02Z", "m2", "toolu_b", "Second?"),
        carrier("2026-10-28T10:00:03Z", "toolu_b"),
    ]);
    assert_eq!(answered_last.pending_ask, Some(the_ask("First?")));
}

/// Mutation: leave `asked` empty in `end`; or skip clearing it in `answer`.
#[test]
fn a_turn_that_ends_with_a_question_open_says_so_until_it_is_answered() {
    let ended = [
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        ask_call("2026-10-28T10:00:01Z", "m1", "toolu_a", "First?"),
        duration("2026-10-28T10:00:02Z"),
    ];
    let open = fold_values(&ended);
    assert_eq!(
        open.turn_ends.first().unwrap().asked,
        Some(the_ask("First?"))
    );

    let mut answered = ended.to_vec();
    answered.push(carrier("2026-10-28T10:05:00Z", "toolu_a"));
    let closed = fold_values(&answered);
    assert_eq!(closed.turn_ends.first().unwrap().asked, None);
}

/// Mutation: attach any unanswered call to a turn end, not only the turn's own.
#[test]
fn a_question_of_an_earlier_turn_is_not_attributed_to_a_later_one() {
    let summary = fold_values(&[
        user_prompt("2026-10-28T10:00:00Z", "human", "go"),
        ask_call("2026-10-28T10:00:01Z", "m1", "toolu_a", "First?"),
        user_prompt("2026-10-28T10:01:00Z", "human", "never mind"),
        text_end("2026-10-28T10:01:01Z", "m2", "ok"),
        duration("2026-10-28T10:01:02Z"),
    ]);
    assert_eq!(summary.turn_ends.first().unwrap().asked, None);
    assert_eq!(summary.pending_ask, Some(the_ask("First?")));
}

// --- fold: facts ---------------------------------------------------------------------------

/// Mutation: count every queued command as human; or ignore `queued_command`.
#[test]
fn only_human_prompts_count_as_human_prompts() {
    let queued = |at: &str, mode: &str, origin: Option<&str>| {
        let mut attachment = json!({"type": "queued_command", "prompt": "p", "commandMode": mode});
        if let Some(kind) = origin {
            attachment["origin"] = json!({"kind": kind});
        }
        with(base("attachment", at), &json!({"attachment": attachment}))
    };
    let summary = fold_values(&[
        queued("2026-10-28T10:00:01Z", "prompt", Some("human")),
        queued("2026-10-28T10:00:02Z", "task-notification", None),
        queued(
            "2026-10-28T10:00:03Z",
            "task-notification",
            Some("task-notification"),
        ),
        user_prompt("2026-10-28T10:00:04Z", "peer", "hello"),
        user_prompt("2026-10-28T10:00:05Z", "human", "typed"),
    ]);
    let seen: Vec<_> = summary
        .human_prompts
        .iter()
        .map(|prompt| (prompt.at, prompt.text.as_str()))
        .collect();
    assert_eq!(
        seen,
        [
            (t("2026-10-28T10:00:01Z"), "p"),
            (t("2026-10-28T10:00:05Z"), "typed")
        ]
    );
    assert_eq!(
        summary.last_human_prompt_at,
        Some(t("2026-10-28T10:00:05Z"))
    );
}

/// Mutation: change the cap to 51, or drop the oldest instead of keeping the newest.
#[test]
fn the_last_fifty_human_prompts_are_kept() {
    let prompts: Vec<Value> = (0..60)
        .map(|n| {
            user_prompt(
                &format!("2026-10-28T10:{n:02}:00Z"),
                "human",
                &format!("p{n}"),
            )
        })
        .collect();
    let summary = fold_values(&prompts);
    assert_eq!(summary.human_prompts.len(), 50);
    assert_eq!(summary.human_prompts.first().unwrap().text, "p10");
    assert_eq!(summary.human_prompts.last().unwrap().text, "p59");
}

/// Mutation: slice at the byte limit without the boundary search (panics or splits `é`).
#[test]
fn a_kept_prompt_is_cut_on_a_character_boundary() {
    // Three bytes a character, so the 32 KiB limit falls inside one.
    let long = "€".repeat(20_000);
    let summary = fold_values(&[user_prompt("2026-10-28T10:00:00Z", "human", &long)]);
    let kept = &summary.human_prompts.first().unwrap().text;
    assert_eq!(kept.len(), 32_766);
    assert!(kept.chars().all(|c| c == '€'));
}

/// The window title is Claude Code's: the last custom title over any AI title, the AI title
/// only while there is no custom one, and none with neither.
/// Mutation: prefer the AI title in `window_title` (the hook-named row), or keep the first
/// custom title (the `Claude: b` row).
#[test]
fn the_window_title_is_the_custom_title_before_the_ai_title() {
    let ai = json!({"type": "ai-title", "aiTitle": "Fix the parser"});
    let custom = |name: &str| json!({"type": "custom-title", "customTitle": name});
    assert_eq!(fold_values(&[]).window_title(), None);
    assert_eq!(
        fold_values(std::slice::from_ref(&ai)).window_title(),
        Some("Fix the parser")
    );
    let named = fold_values(&[custom("Claude: a"), ai.clone(), custom("Claude: b")]);
    assert_eq!(named.window_title(), Some("Claude: b"));
    assert_eq!(named.ai_title.as_deref(), Some("Fix the parser"));
}

/// Mutation: keep the first value instead of the last for `cwd`/branch/title/mode.
#[test]
fn metadata_takes_the_last_value_and_entrypoint_the_first() {
    let with_entry = |at: &str, entry: &str, cwd: &str, branch: &str| {
        with(
            user_prompt(at, "human", "p"),
            &json!({"entrypoint": entry, "cwd": cwd, "gitBranch": branch}),
        )
    };
    let summary = fold_values(&[
        with_entry("2026-10-28T10:00:00Z", "cli", "/a", "one"),
        json!({"type": "ai-title", "aiTitle": "first"}),
        json!({"type": "permission-mode", "permissionMode": "acceptEdits"}),
        with_entry("2026-10-28T10:00:01Z", "sdk-cli", "/b", "two"),
        json!({"type": "ai-title", "aiTitle": "second"}),
        json!({"type": "permission-mode", "permissionMode": "plan"}),
        json!({"type": "last-prompt", "lastPrompt": "a"}),
        json!({"type": "last-prompt", "lastPrompt": "b"}),
    ]);
    assert_eq!(summary.entrypoint.as_deref(), Some("cli"));
    assert_eq!(summary.cwd, Some(PathBuf::from("/b")));
    assert_eq!(summary.git_branch.as_deref(), Some("two"));
    assert_eq!(summary.ai_title.as_deref(), Some("second"));
    assert_eq!(summary.permission_mode.as_deref(), Some("plan"));
    assert_eq!(summary.last_prompt.as_deref(), Some("b"));
    assert_eq!(summary.session_id.unwrap().as_str(), SESSION);
}

/// Mutation: take the last session id instead of the first (`is_none` guard to `true`); take
/// the last timestamp instead of the latest (`max` to the plain assignment).
#[test]
fn the_session_id_is_the_first_seen_and_the_clock_the_latest() {
    let other = "9f8e7d6c-5b4a-4321-8abc-0123456789ab";
    let mut later = user_prompt("2026-10-28T10:00:09Z", "human", "a");
    later["sessionId"] = json!(other);
    let earlier = user_prompt("2026-10-28T10:00:01Z", "human", "b");
    let summary = fold_values(&[later, earlier]);
    assert_eq!(summary.session_id.unwrap().as_str(), other);
    assert_eq!(summary.last_record_at, Some(t("2026-10-28T10:00:09Z")));
}

/// Mutation: drop the `any(url ==)` guard in `on_meta`.
#[test]
fn a_re_appended_pr_link_is_one_link() {
    let link = |number: u32| json!({"type": "pr-link", "prNumber": number, "prUrl": format!("https://github.com/example-org/r/pull/{number}"), "prRepository": "example-org/r"});
    let summary = fold_values(&[link(7), link(8), link(7), link(7)]);
    let numbers: Vec<_> = summary.pr_links.iter().map(|l| l.number).collect();
    assert_eq!(numbers, [7, 8]);
}

/// Mutation: push the time in `note_human_prompt` for every trigger, or count an ask in
/// `asks_answered` when the tool result names an id that was never asked.
#[test]
fn every_human_prompt_time_and_every_ask_is_counted() {
    let summary = scan(&fixture_path("interactive.jsonl")).unwrap();
    // Prompt one, the queued human prompt, and prompt three; the task notification between
    // them is not a human prompt.
    assert_eq!(
        summary.human_prompt_times,
        [
            t("2026-10-28T10:00:00Z"),
            t("2026-10-28T10:05:35Z"),
            t("2026-10-28T10:10:00Z")
        ]
    );
    // Two questions asked; the first was answered, the second is the open one.
    assert_eq!((summary.asks_made, summary.asks_answered), (2, 1));
    let stray = fold_values(&[carrier("2026-10-28T10:00:00Z", "toolu_never_asked")]);
    assert_eq!((stray.asks_made, stray.asks_answered), (0, 0));
}

/// Mutation: ignore `Malformed` events in `fold_event`; or keep the old state on `Truncated`.
#[test]
fn malformed_lines_are_counted_and_truncation_starts_over() {
    let mut summary = fold_values(&[user_prompt("2026-10-28T10:00:00Z", "human", "go")]);
    fold_event(&mut summary, &TailEvent::Malformed(LineError::TooLong));
    fold_event(&mut summary, &TailEvent::Malformed(LineError::NotUtf8));
    assert_eq!(summary.line_errors, 2);
    assert_eq!(summary.human_prompts.len(), 1);
    fold_event(&mut summary, &TailEvent::Truncated);
    assert_eq!(summary, Summary::default());
}

// --- scan: the fixtures --------------------------------------------------------------------

/// Mutation: any change to the fold of turn ends, prompts, links or the pending question
/// reddens this; the byte count is the fixture's length, so a splitter that loses a byte does.
#[test]
fn the_interactive_transcript_reads_as_two_turns_and_an_open_question() {
    let summary = scan(&fixture_path("interactive.jsonl")).unwrap();
    assert_eq!(summary.session_id.unwrap().as_str(), SESSION);
    assert_eq!(summary.cwd, Some(PathBuf::from("/home/dev/proj")));
    assert_eq!(summary.git_branch.as_deref(), Some("feature/example"));
    assert_eq!(summary.ai_title.as_deref(), Some("Placeholder title"));
    assert_eq!(
        summary.permission_mode.as_deref(),
        Some("bypassPermissions")
    );
    assert_eq!(summary.entrypoint.as_deref(), Some("cli"));
    assert_eq!(
        summary.last_prompt.as_deref(),
        Some("Placeholder prompt three.")
    );
    let links: Vec<_> = summary.pr_links.iter().map(|l| l.url.as_str()).collect();
    assert_eq!(
        links,
        [
            "https://github.com/example-org/example-repo/pull/7",
            "https://github.com/example-org/example-repo/pull/8"
        ]
    );
    let prompts: Vec<_> = summary
        .human_prompts
        .iter()
        .map(|p| (p.at, p.text.as_str()))
        .collect();
    assert_eq!(
        prompts,
        [
            (
                t("2026-10-28T10:00:00Z"),
                "Placeholder prompt one: see https://example.atlassian.net/browse/ABC-123"
            ),
            (t("2026-10-28T10:05:35Z"), "Placeholder queued prompt."),
            (t("2026-10-28T10:10:00Z"), "Placeholder prompt three."),
        ]
    );
    assert_eq!(
        summary.last_human_prompt_at,
        Some(t("2026-10-28T10:10:00Z"))
    );
    assert_eq!(
        summary.turn_ends,
        [
            turn_end(
                "2026-10-28T10:00:10Z",
                TurnTrigger::Human,
                "Placeholder reply one."
            ),
            turn_end(
                "2026-10-28T10:05:40.500Z",
                TurnTrigger::TaskNotification,
                "Placeholder reply two."
            ),
        ]
    );
    let pending = summary.pending_ask.clone().unwrap();
    let question = pending.questions().first().unwrap();
    assert_eq!(question.question.as_str(), "Placeholder pending question?");
    assert_eq!(question.multi_select, MultiSelect::Many);
    assert_eq!(summary.line_errors, 0);
    assert_eq!(summary.last_record_at, Some(t("2026-10-28T10:10:05Z")));
    assert_eq!(
        summary.bytes_read,
        u64::try_from(INTERACTIVE.len()).unwrap()
    );
    assert_eq!(summary.unterminated_tail_bytes, 0);
}

/// Mutation: end turns only on `turn_duration` (no end at all for this file).
#[test]
fn a_headless_transcript_ends_its_turn_on_the_end_turn_text_block() {
    let summary = scan(&fixture_path("headless.jsonl")).unwrap();
    assert_eq!(
        summary.turn_ends,
        [turn_end(
            "2026-10-28T11:00:12Z",
            TurnTrigger::Unknown("sdk".to_owned()),
            "Placeholder headless reply."
        )]
    );
    assert_eq!(summary.entrypoint.as_deref(), Some("sdk-cli"));
    assert_eq!(summary.last_human_prompt_at, None);
    assert_eq!(summary.bytes_read, u64::try_from(HEADLESS.len()).unwrap());
}

#[test]
fn every_line_of_the_record_fixture_reads_without_error() {
    let summary = scan(&fixture_path("records.jsonl")).unwrap();
    assert_eq!(summary.line_errors, 0);
    assert_eq!(summary.bytes_read, u64::try_from(RECORDS.len()).unwrap());
}

// --- scratch files -------------------------------------------------------------------------

fn append(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

// --- scan: failures ------------------------------------------------------------------------

/// Mutation: return `Ok(Summary::default())` for a missing file.
#[test]
fn a_missing_file_is_unreadable_not_empty() {
    let scratch = Scratch::new("transcript");
    let error = scan(&scratch.join("absent.jsonl")).unwrap_err();
    assert!(matches!(error, TranscriptError::Unreadable { .. }));
}

/// Mutation: stop on the first bad line; or drop the count for non-UTF-8 lines.
#[test]
fn garbage_is_counted_and_the_rest_is_read() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("garbage.jsonl");
    append(&path, b"not json\n");
    append(&path, HEADLESS.as_bytes());
    append(&path, b"\xff\xfe\n");
    append(&path, b"{\"type\":\"system\",\"subtype\":\"x\"}\n");
    let summary = scan(&path).unwrap();
    assert_eq!(summary.line_errors, 3);
    assert_eq!(summary.turn_ends.len(), 1);
}

/// Mutation: report only bytes up to the last record (the tail is lost silently).
#[test]
fn a_line_without_its_newline_is_reported_not_read() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("torn.jsonl");
    append(&path, HEADLESS.as_bytes());
    append(&path, b"{\"type\":\"mo");
    let summary = scan(&path).unwrap();
    assert_eq!(summary.line_errors, 0);
    assert_eq!(summary.bytes_read, u64::try_from(HEADLESS.len()).unwrap());
    assert_eq!(summary.unterminated_tail_bytes, 11);
}

/// Mutation: buffer the line (peak memory is not observable here, so the check is that the
/// scan finishes with one counted error and the records on both sides intact).
#[test]
fn an_overlong_line_in_a_file_is_one_counted_error() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("long.jsonl");
    append(&path, HEADLESS.as_bytes());
    append(&path, &vec![b'x'; LINE_BYTES_MAX + 10]);
    append(&path, b"\n");
    append(&path, b"{\"type\":\"mode\"}\n");
    let summary = scan(&path).unwrap();
    assert_eq!(summary.line_errors, 1);
    assert_eq!(summary.turn_ends.len(), 1);
    assert_eq!(
        summary.bytes_read,
        u64::try_from(HEADLESS.len() + LINE_BYTES_MAX + 10 + 1 + 16).unwrap()
    );
}

// --- tail ----------------------------------------------------------------------------------

fn records_of(events: &[TailEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, TailEvent::Record(_)))
        .count()
}

/// Mutation: seek to 0 on every poll (records repeat); or return the partial line as an error.
#[test]
fn a_poll_returns_only_what_was_appended_and_carries_a_partial_line() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("live.jsonl");
    append(&path, b"{\"type\":\"mode\"}\n{\"type\":\"mo");
    let mut tailer = Tailer::new(&path);
    let first = tailer.poll().unwrap();
    assert_eq!((records_of(&first), first.len()), (1, 1));
    assert_eq!(tailer.unterminated(), 11);
    assert_eq!(tailer.poll().unwrap(), []);
    append(&path, b"de\"}\n{\"type\":\"x\"}\n");
    let second = tailer.poll().unwrap();
    assert_eq!((records_of(&second), second.len()), (2, 2));
    assert_eq!(tailer.unterminated(), 0);
    assert_eq!(tailer.consumed(), fs::metadata(&path).unwrap().len());
}

/// Mutation: compare `len <= offset` (a same-length poll reports truncation) or drop the reset
/// of `offset` (nothing is read after the truncation).
#[test]
fn a_shorter_file_is_reported_as_truncated_and_read_again_from_the_start() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("rewritten.jsonl");
    append(&path, b"{\"type\":\"mode\"}\n{\"type\":\"mode\"}\n");
    let mut tailer = Tailer::new(&path);
    assert_eq!(tailer.poll().unwrap().len(), 2);
    fs::write(&path, b"{\"type\":\"x\"}\n").unwrap();
    let events = tailer.poll().unwrap();
    assert_eq!(events.first(), Some(&TailEvent::Truncated));
    assert_eq!(records_of(&events), 1);
    assert_eq!(tailer.poll().unwrap(), []);
}

/// Mutation: map an open error to an empty poll.
#[test]
fn a_vanished_file_is_an_error_not_an_empty_poll() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("gone.jsonl");
    append(&path, b"{\"type\":\"mode\"}\n");
    let mut tailer = Tailer::new(&path);
    tailer.poll().unwrap();
    fs::remove_file(&path).unwrap();
    assert!(matches!(
        tailer.poll(),
        Err(TranscriptError::Unreadable { .. })
    ));
}

/// Mutation: drop the `.min(POLL_BYTES_MAX)` (the first poll reads the whole file).
#[test]
fn one_poll_reads_at_most_the_poll_bound() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("big.jsonl");
    let megabyte = vec![b'\n'; 1024 * 1024];
    for _ in 0..65 {
        append(&path, &megabyte);
    }
    let mut tailer = Tailer::new(&path);
    assert_eq!(tailer.poll().unwrap(), []);
    assert_eq!(tailer.consumed(), tail::POLL_BYTES_MAX);
    assert_eq!(tailer.poll().unwrap(), []);
    assert_eq!(tailer.consumed(), 65 * 1024 * 1024);
}

/// Mutation: ignore the offset in `starting_at` (the first record repeats).
#[test]
fn a_tailer_can_resume_where_a_scan_stopped() {
    let scratch = Scratch::new("transcript");
    let path = scratch.join("resume.jsonl");
    append(&path, INTERACTIVE.as_bytes());
    let mut summary = scan(&path).unwrap();
    let mut tailer = Tailer::starting_at(&path, summary.bytes_read);
    assert_eq!(tailer.poll().unwrap(), []);
    let answer = carrier("2026-10-28T10:11:00Z", "toolu_placeholder_ask2");
    append(&path, format!("{answer}\n").as_bytes());
    for event in tailer.poll().unwrap() {
        fold_event(&mut summary, &event);
    }
    assert_eq!(summary.pending_ask, None);
}

// --- scan and tail agree -------------------------------------------------------------------

/// Everything the fixtures hold, with junk lines between, and no final newline: the stream
/// the equivalence properties cut at arbitrary places.
fn stream(junk_every: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for (n, line) in INTERACTIVE
        .lines()
        .chain(HEADLESS.lines())
        .chain(RECORDS.lines())
        .enumerate()
    {
        if junk_every > 0 && n % junk_every == 0 {
            out.extend_from_slice(b"junk\n\n\xff\n");
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"{\"type\":\"torn");
    out
}

fn cut_points(total: usize, sizes: &[usize]) -> Vec<usize> {
    let mut points = Vec::new();
    let mut at = 0;
    for size in sizes.iter().cycle() {
        at += size;
        if at >= total {
            break;
        }
        points.push(at);
    }
    points
}

proptest! {
    /// Mutation: decide the line-length bound per chunk instead of per line; split a line's
    /// bytes when a chunk ends inside it (`finish_line` forgets the held part).
    #[test]
    fn feeding_in_any_chunks_gives_the_same_lines(
        sizes in prop::collection::vec(1_usize..400, 1..40),
        junk_every in 0_usize..7,
    ) {
        let bytes = stream(junk_every);
        let mut whole = Feed::new();
        let expected = whole.push(&bytes);
        let mut pieces = Feed::new();
        let mut actual = Vec::new();
        let mut start = 0;
        for point in cut_points(bytes.len(), &sizes).into_iter().chain([bytes.len()]) {
            actual.extend(pieces.push(&bytes[start..point]));
            start = point;
        }
        prop_assert_eq!(actual, expected);
        prop_assert_eq!(pieces, whole);
    }

    /// The equivalence the live poller relies on: `scan` of a file equals a `Tailer` that
    /// followed the same bytes as they were appended in arbitrary chunks, polling after each,
    /// folded with `fold_event`.
    ///
    /// Mutation: make `Tailer::poll` skip the carried partial line (`Feed::starting_at` after
    /// every poll); count `Malformed` events in only one of the two readers.
    #[test]
    fn a_tailer_that_followed_the_file_matches_a_scan_of_it(
        sizes in prop::collection::vec(1_usize..2_000, 1..30),
        junk_every in 0_usize..7,
    ) {
        let bytes = stream(junk_every);
        let scratch = Scratch::new("transcript");
        let path = scratch.join("followed.jsonl");
        fs::write(&path, b"").unwrap();
        let mut tailer = Tailer::new(&path);
        let mut followed = Summary::default();
        let mut start = 0;
        for point in cut_points(bytes.len(), &sizes).into_iter().chain([bytes.len()]) {
            append(&path, &bytes[start..point]);
            start = point;
            for event in tailer.poll().unwrap() {
                fold_event(&mut followed, &event);
            }
        }
        followed.set_extent(tailer.consumed(), tailer.unterminated());
        prop_assert_eq!(followed, scan(&path).unwrap());
    }
}

fn tool_call(at: &str, message: &str, id: &str, name: &str, input: &Value) -> Value {
    assistant(
        at,
        message,
        Some("tool_use"),
        &json!({"type": "tool_use", "id": id, "name": name, "input": input}),
    )
}

fn failed_carrier(at: &str, tool_use_id: &str) -> Value {
    with(
        base("user", at),
        &json!({"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": "denied", "is_error": true}]}}),
    )
}

/// A Jira write or a Slack post counts once its call is answered without an error, at the time
/// it was asked; a refused one, an unanswered one, a read, and a call naming no ticket do not.
/// Mutation: keep a write whose result is an error, keep it at its answer's time, or match a
/// read tool as a write.
#[test]
fn a_write_counts_once_its_call_succeeds() {
    use crate::types::outward::{Target, WriteKind};
    let jira = "mcp__claude_ai_Atlassian__";
    let summary = fold_values(&[
        tool_call(
            "2026-11-05T10:00:00Z",
            "m1",
            "c1",
            &format!("{jira}addCommentToJiraIssue"),
            &json!({"issueIdOrKey": "ABC-7", "commentBody": "Moving   to\nProd"}),
        ),
        carrier("2026-11-05T10:00:05Z", "c1"),
        tool_call(
            "2026-11-05T10:01:00Z",
            "m2",
            "c2",
            &format!("{jira}transitionJiraIssue"),
            &json!({"issueIdOrKey": "ABC-7", "transition": {"id": "7"}}),
        ),
        failed_carrier("2026-11-05T10:01:05Z", "c2"),
        tool_call(
            "2026-11-05T10:02:00Z",
            "m3",
            "c3",
            &format!("{jira}getJiraIssue"),
            &json!({"issueIdOrKey": "ABC-7"}),
        ),
        carrier("2026-11-05T10:02:05Z", "c3"),
        tool_call(
            "2026-11-05T10:03:00Z",
            "m4",
            "c4",
            &format!("{jira}editJiraIssue"),
            &json!({"issueIdOrKey": "10042"}),
        ),
        carrier("2026-11-05T10:03:05Z", "c4"),
        tool_call(
            "2026-11-05T10:04:00Z",
            "m5",
            "c5",
            "mcp__claude_ai_Slack__slack_send_message",
            &json!({"channel_id": "C1", "thread_ts": "171.5", "message": "Deployed"}),
        ),
        carrier("2026-11-05T10:04:05Z", "c5"),
        tool_call(
            "2026-11-05T10:04:30Z",
            "m7",
            "c7",
            &format!("{jira}addCommentToJiraIssue"),
            &json!({"issueIdOrKey": "ABC-7", "commentBody": r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"mention","attrs":{"id":"x","text":"@Ada"}},{"type":"text","text":" please file it"}]}]}"#}),
        ),
        carrier("2026-11-05T10:04:35Z", "c7"),
        tool_call(
            "2026-11-05T10:05:00Z",
            "m6",
            "c6",
            &format!("{jira}addCommentToJiraIssue"),
            &json!({"issueIdOrKey": "ABC-8", "commentBody": "never answered"}),
        ),
    ]);
    let seen: Vec<(Instant, String, WriteKind, Option<&str>)> = summary
        .writes
        .iter()
        .map(|w| {
            (
                w.at,
                w.write.target.to_string(),
                w.write.kind,
                w.write.excerpt.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                t("2026-11-05T10:00:00Z"),
                "ABC-7".to_owned(),
                WriteKind::Comment,
                Some("Moving to Prod")
            ),
            (
                t("2026-11-05T10:04:00Z"),
                "slack:C1/171.5".to_owned(),
                WriteKind::Post,
                Some("Deployed")
            ),
            (
                t("2026-11-05T10:04:30Z"),
                "ABC-7".to_owned(),
                WriteKind::Comment,
                Some("@Ada please file it")
            ),
        ]
    );
    assert!(
        matches!(&summary.writes[1].write.target, Target::Slack { thread: Some(ts), .. } if ts == "171.5")
    );
}
