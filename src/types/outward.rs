//! A session's writes that other people read: a Jira ticket changed, a Slack message posted.
//!
//! Read out of the transcript's tool calls, so they say what a session did, where the
//! coordination ledger says what it declared. Two sessions writing to one ticket is the
//! collision the ledger exists to prevent; these are how it is seen when it happens anyway.

use std::fmt;

use super::{Instant, JiraKey};

/// Where a write lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Ticket(JiraKey),
    /// A Slack conversation, and the thread when the message was a reply.
    Slack {
        channel: String,
        thread: Option<String>,
    },
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ticket(key) => write!(f, "{key}"),
            Self::Slack {
                channel,
                thread: Some(thread),
            } => write!(f, "slack:{channel}/{thread}"),
            Self::Slack {
                channel,
                thread: None,
            } => write!(f, "slack:{channel}"),
        }
    }
}

/// What the write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Comment,
    Transition,
    Edit,
    Worklog,
    Post,
}

impl WriteKind {
    pub const fn words(self) -> &'static str {
        match self {
            Self::Comment => "commented",
            Self::Transition => "moved",
            Self::Edit => "edited",
            Self::Worklog => "logged work",
            Self::Post => "posted",
        }
    }
}

/// One write a tool call asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outward {
    pub target: Target,
    pub kind: WriteKind,
    /// The start of what was written, when the call carried text; at most
    /// [`Outward::EXCERPT_CHARS`] characters, cut on a character boundary.
    pub excerpt: Option<String>,
}

impl Outward {
    pub const EXCERPT_CHARS: usize = 240;

    /// The write a tool call named `tool` with `input` asks for; `None` for any other call, or
    /// one whose input does not name its target.
    pub fn of_call(tool: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let (_, name) = tool.rsplit_once("__")?;
        let input = input?;
        let text = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
        let ticket = || {
            text("issueIdOrKey")
                .and_then(|key| JiraKey::parse(key.trim()))
                .map(Target::Ticket)
        };
        let (target, kind, body) = match name {
            "addCommentToJiraIssue" => (ticket()?, WriteKind::Comment, text("commentBody")),
            "transitionJiraIssue" => (ticket()?, WriteKind::Transition, None),
            "editJiraIssue" => (ticket()?, WriteKind::Edit, None),
            "addWorklogToJiraIssue" => (ticket()?, WriteKind::Worklog, None),
            "slack_send_message" => (
                Target::Slack {
                    channel: text("channel_id")?.to_owned(),
                    thread: text("thread_ts").map(str::to_owned),
                },
                WriteKind::Post,
                text("message"),
            ),
            _ => return None,
        };
        Some(Self {
            target,
            kind,
            excerpt: body.map(excerpt).filter(|text| !text.is_empty()),
        })
    }
}

/// The start of `text` on one line: whitespace runs become one space. A comment written in
/// Jira's document format (JSON) gives the text of its nodes, not the JSON.
fn excerpt(text: &str) -> String {
    let document = text
        .trim_start()
        .starts_with('{')
        .then(|| serde_json::from_str::<serde_json::Value>(text).ok())
        .flatten();
    let plain = match &document {
        Some(document) => {
            let mut parts = Vec::new();
            document_text(document, &mut parts);
            parts.join(" ")
        }
        None => text.to_owned(),
    };
    let words: Vec<&str> = plain.split_whitespace().collect();
    let joined = words.join(" ");
    match joined.char_indices().nth(Outward::EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", joined.get(..cut).unwrap_or_default()),
        None => joined,
    }
}

/// The text a Jira document node holds, depth first: its `text`, a mention's `attrs.text`, and
/// its `content`. Bounded by the comment the call carried.
fn document_text<'v>(node: &'v serde_json::Value, parts: &mut Vec<&'v str>) {
    if let Some(text) = node.get("text").and_then(serde_json::Value::as_str) {
        parts.push(text);
    }
    if let Some(text) = node
        .get("attrs")
        .and_then(|attrs| attrs.get("text"))
        .and_then(serde_json::Value::as_str)
    {
        parts.push(text);
    }
    if let Some(children) = node.get("content").and_then(serde_json::Value::as_array) {
        for child in children {
            document_text(child, parts);
        }
    }
}

/// A write that happened: its call was answered without an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub at: Instant,
    pub write: Outward,
}
