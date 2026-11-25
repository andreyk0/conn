//! `acli jira workitem search --json` output, parsed into [`WorkItem`]s.
//!
//! Parsing is strict about what a row cannot do without (key, summary, status, type) and lenient about what Jira is free to extend: unknown fields are ignored, an unknown
//! status category is kept as text, a key outside the configured prefixes is kept as
//! [`WorkItemKey::Other`], and a description of any shape is flattened to plain text.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::JiraError;
use crate::types::{Instant, JiraKey, JiraPrefix};

/// The most characters of a description kept. A card shows a teaser, and Jira descriptions run
/// to pages; 300 characters is a few lines.
pub const DESCRIPTION_CHARS_MAX: usize = 300;

/// A work item's key, by whether its project is one the user configured.
///
/// The user's assignments include projects that are not in the configuration, so such a key is kept
/// as the text Jira wrote rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkItemKey {
    /// A well-formed key whose prefix is configured: it joins to a session's `Link::Jira`.
    Configured(JiraKey),
    /// Any other key, as Jira wrote it.
    Other(String),
}

impl WorkItemKey {
    /// `Configured` when `text` is a well-formed key whose prefix is in `prefixes`, otherwise
    /// `Other`.
    pub fn classify(text: &str, prefixes: &[JiraPrefix]) -> Self {
        let configured = text.rsplit_once('-').and_then(|(prefix, number)| {
            let prefix = JiraPrefix::parse(prefix).ok()?;
            let number = number.parse::<NonZeroU32>().ok()?;
            prefixes
                .contains(&prefix)
                .then(|| JiraKey::new(prefix, number))
        });
        configured.map_or_else(|| Self::Other(text.to_owned()), Self::Configured)
    }
}

impl fmt::Display for WorkItemKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured(key) => write!(f, "{key}"),
            Self::Other(text) => f.write_str(text),
        }
    }
}

/// A status category, the three-way grouping Jira puts every status in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusCategory {
    ToDo,
    InProgress,
    Done,
    /// A category this tool does not know (Jira's `undefined`, or a future one), as Jira wrote
    /// its key; `unspecified` when the row carried none.
    Other(String),
}

impl StatusCategory {
    pub(super) fn from_key(key: Option<&str>) -> Self {
        match key {
            Some("new") => Self::ToDo,
            Some("indeterminate") => Self::InProgress,
            Some("done") => Self::Done,
            Some(other) => Self::Other(other.to_owned()),
            None => Self::Other("unspecified".to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub category: StatusCategory,
}

/// Where a work item's type sits in Jira's hierarchy: an epic holds other items as its
/// children; everything else is an item.
///
/// Read from the type's `hierarchyLevel` (`1` is Jira's epic level, and any custom level above
/// it holds children too), never from its name, which a site can rename. A type that states no
/// level is an item: an epic needs Jira to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Level {
    Epic,
    #[default]
    Item,
}

impl Level {
    fn of(hierarchy_level: Option<i64>) -> Self {
        match hierarchy_level {
            Some(level) if level >= 1 => Self::Epic,
            Some(_) | None => Self::Item,
        }
    }
}

/// The epic a work item belongs to, as the latest view of the item said.
///
/// `acli jira workitem search` refuses the `parent` field, so the epic comes from the one
/// view of each item the comment reader already makes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EpicOf {
    /// The view did not ask: a read saved before the epic was asked for.
    #[default]
    NotAsked,
    /// The item has no parent, or a parent that is not an epic (a sub-task's story).
    NoEpic,
    /// `summary` is as Jira wrote it.
    Epic { key: JiraKey, summary: String },
}

impl EpicOf {
    /// The epic in a `view --json` output's `fields`; an absent or null `parent` is none.
    pub(super) fn parse(fields: &Value) -> Result<Self, JiraError> {
        let unparseable = |detail: &str| JiraError::Unparseable {
            detail: detail.to_owned(),
        };
        let parent = match fields.get("parent") {
            None | Some(Value::Null) => return Ok(Self::NoEpic),
            Some(parent) => parent,
        };
        let key = parent
            .get("key")
            .and_then(Value::as_str)
            .and_then(JiraKey::parse)
            .ok_or_else(|| unparseable("the parent has no key"))?;
        let parent_fields = parent.get("fields");
        let level = parent_fields
            .and_then(|fields| fields.get("issuetype"))
            .and_then(|issue_type| issue_type.get("hierarchyLevel"))
            .and_then(Value::as_i64);
        match Level::of(level) {
            Level::Item => Ok(Self::NoEpic),
            Level::Epic => {
                let summary = parent_fields
                    .and_then(|fields| fields.get("summary"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| unparseable("the parent epic has no summary"))?;
                Ok(Self::Epic {
                    key,
                    summary: summary.to_owned(),
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub key: WorkItemKey,
    pub summary: String,
    pub status: Status,
    pub issue_type: String,
    pub level: Level,
    pub priority: Option<String>,
    pub creator: Option<Person>,
    pub assignee: Option<Person>,
    /// Never filled from `acli`: its `--fields` whitelist refuses `updated` and `created`, so
    /// ordering comes from the query's `order by` and freshness from the cache's read time.
    pub updated: Option<Instant>,
    /// Never filled from `acli`, for the reason given at `updated`.
    pub created: Option<Instant>,
    /// Plain text, at most [`DESCRIPTION_CHARS_MAX`] characters; `None` when there is none.
    pub description: Option<String>,
}

/// The items by their key text (`ABC-123`), for joining to the keys found in sessions.
///
/// The join: the web layer takes each session's `Link::Jira { key }`, looks up `key.to_string()`
/// here, and lists the session under that ticket. Only a [`WorkItemKey::Configured`] key can
/// match a session, since only configured prefixes are extracted from transcripts; the
/// `Other` rows are listed without sessions. Should Jira return one key twice, the later row
/// wins.
pub fn by_key(items: &[WorkItem]) -> HashMap<String, &WorkItem> {
    items
        .iter()
        .map(|item| (item.key.to_string(), item))
        .collect()
}

/// The wire shape, kept private: the nesting Jira uses becomes the flat domain type here.
#[derive(Deserialize)]
struct WireItem {
    key: String,
    fields: WireFields,
}

#[derive(Deserialize)]
struct WireFields {
    summary: String,
    status: WireStatus,
    issuetype: WireType,
    priority: Option<WireNamed>,
    creator: Option<WirePerson>,
    assignee: Option<WirePerson>,
    description: Option<Value>,
}

#[derive(Deserialize)]
struct WireNamed {
    name: String,
}

#[derive(Deserialize)]
struct WireType {
    name: String,
    #[serde(rename = "hierarchyLevel")]
    hierarchy_level: Option<i64>,
}

#[derive(Deserialize)]
struct WirePerson {
    #[serde(rename = "displayName")]
    display_name: String,
}

#[derive(Deserialize)]
struct WireStatus {
    name: String,
    #[serde(rename = "statusCategory")]
    status_category: Option<WireCategory>,
}

#[derive(Deserialize)]
struct WireCategory {
    key: String,
}

/// Parses the stdout of `acli jira workitem search --json`: a list of items, or an object
/// holding them under `issues`. Keys are classified against `prefixes`.
pub fn parse_work_items(
    stdout: &[u8],
    prefixes: &[JiraPrefix],
) -> Result<Vec<WorkItem>, JiraError> {
    let unparseable = |detail: String| JiraError::Unparseable { detail };
    let value: Value = serde_json::from_slice(stdout).map_err(|e| unparseable(e.to_string()))?;
    let rows = match value {
        Value::Array(rows) => rows,
        Value::Object(mut object) => match object.remove("issues") {
            Some(Value::Array(rows)) => rows,
            Some(_) | None => {
                return Err(unparseable("an object without an `issues` list".to_owned()));
            }
        },
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            return Err(unparseable("neither a list nor an object".to_owned()));
        }
    };
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let wire: WireItem = serde_json::from_value(row)
                .map_err(|e| unparseable(format!("item {index}: {e}")))?;
            let key = wire.key.clone();
            to_work_item(wire, prefixes)
                .map_err(|detail| unparseable(format!("item {key}: {detail}")))
        })
        .collect()
}

fn to_work_item(wire: WireItem, prefixes: &[JiraPrefix]) -> Result<WorkItem, String> {
    let WireItem { key, fields } = wire;
    if key.is_empty() {
        return Err("empty key".to_owned());
    }
    Ok(WorkItem {
        key: WorkItemKey::classify(&key, prefixes),
        summary: fields.summary,
        status: Status {
            name: fields.status.name,
            category: StatusCategory::from_key(
                fields
                    .status
                    .status_category
                    .as_ref()
                    .map(|c| c.key.as_str()),
            ),
        },
        level: Level::of(fields.issuetype.hierarchy_level),
        issue_type: fields.issuetype.name,
        priority: fields.priority.map(|p| p.name),
        creator: fields.creator.map(person),
        assignee: fields.assignee.map(person),
        updated: None,
        created: None,
        description: fields.description.as_ref().and_then(flatten_description),
    })
}

fn person(wire: WirePerson) -> Person {
    Person {
        display_name: wire.display_name,
    }
}

/// Plain text from a description: a string is kept as written, an Atlassian Document Format
/// object has its `text` nodes joined with a newline after each block. Any other shape, and
/// text that is empty once trimmed, is no description.
fn flatten_description(description: &Value) -> Option<String> {
    let text = match description {
        Value::String(text) => text.clone(),
        Value::Object(_) => {
            let mut out = String::new();
            adf_text(description, &mut out);
            out
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(DESCRIPTION_CHARS_MAX).collect())
}

/// The ADF node types that end a line.
const ADF_BLOCKS: [&str; 6] = [
    "paragraph",
    "heading",
    "listItem",
    "codeBlock",
    "blockquote",
    "rule",
];

fn adf_text(node: &Value, out: &mut String) {
    let kind = node.get("type").and_then(Value::as_str);
    if kind == Some("text") {
        if let Some(text) = node.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
        return;
    }
    if kind == Some("hardBreak") {
        out.push('\n');
        return;
    }
    for child in node
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        adf_text(child, out);
    }
    if kind.is_some_and(|k| ADF_BLOCKS.contains(&k)) && !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    //! Each test parses a fixture under `tests/fixtures/jira`, or a one-line variation of it,
    //! and compares against values written out by hand from the fixture's contents.

    use super::*;

    const SEARCH: &[u8] = include_bytes!("../../tests/fixtures/jira/search_result.json");
    const UNPARSEABLE: &[u8] = include_bytes!("../../tests/fixtures/jira/unparseable.txt");

    fn prefixes(names: &[&str]) -> Vec<JiraPrefix> {
        names
            .iter()
            .map(|n| JiraPrefix::parse(n).unwrap())
            .collect()
    }

    fn key(prefix: &str, number: u32) -> WorkItemKey {
        WorkItemKey::Configured(JiraKey::new(
            JiraPrefix::parse(prefix).unwrap(),
            NonZeroU32::new(number).unwrap(),
        ))
    }

    fn parsed() -> Vec<WorkItem> {
        parse_work_items(SEARCH, &prefixes(&["EX"])).unwrap()
    }

    /// Mutation: read `summary` from the wrong field, or map `new` to `InProgress`.
    #[test]
    fn the_fixture_parses_into_three_items() {
        let items = parsed();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items.first().unwrap(),
            &WorkItem {
                key: key("EX", 1),
                summary: "Placeholder summary one".to_owned(),
                status: Status {
                    name: "In Progress".to_owned(),
                    category: StatusCategory::InProgress,
                },
                issue_type: "Task".to_owned(),
                level: Level::Item,
                priority: Some("High".to_owned()),
                creator: Some(Person {
                    display_name: "Person One".to_owned()
                }),
                assignee: Some(Person {
                    display_name: "Person Two".to_owned()
                }),
                updated: None,
                created: None,
                description: Some("Placeholder wiki text description one.".to_owned()),
            }
        );
        let second = items.get(1).unwrap();
        assert_eq!(second.status.category, StatusCategory::ToDo);
        assert_eq!(second.issue_type, "Bug");
    }

    /// Mutation: read the level from the type's name, or count level 0 as an epic.
    #[test]
    fn an_epic_is_a_type_at_hierarchy_level_one_or_above() {
        let levels: Vec<Level> = parsed().iter().map(|item| item.level).collect();
        assert_eq!(levels, [Level::Item, Level::Item, Level::Epic]);
        let cases = [
            (None, Level::Item),
            (Some(-1), Level::Item),
            (Some(0), Level::Item),
            (Some(1), Level::Epic),
            (Some(2), Level::Epic),
        ];
        for (level, expected) in cases {
            assert_eq!(Level::of(level), expected, "{level:?}");
        }
    }

    /// Mutation: skip the `Option` for `assignee` (`null` -> parse failure), or fill an
    /// absent priority with a default.
    #[test]
    fn nulls_are_absence_and_unknown_fields_are_ignored() {
        let third = parsed().remove(2);
        assert_eq!(third.priority, None);
        assert_eq!(third.assignee, None);
        assert_eq!(third.description, None);
        assert_eq!(
            third.status,
            Status {
                name: "Parked".to_owned(),
                category: StatusCategory::Other("undefined".to_owned()),
            }
        );
    }

    /// Mutation: map every unknown category to `ToDo`; or `None` to something other than the
    /// `unspecified` marker.
    #[test]
    fn status_categories_are_mapped_by_key() {
        let cases = [
            (Some("new"), StatusCategory::ToDo),
            (Some("indeterminate"), StatusCategory::InProgress),
            (Some("done"), StatusCategory::Done),
            (Some("waiting"), StatusCategory::Other("waiting".to_owned())),
            (None, StatusCategory::Other("unspecified".to_owned())),
        ];
        for (key, expected) in cases {
            assert_eq!(StatusCategory::from_key(key), expected, "{key:?}");
        }
    }

    /// Mutation: classify every key `Configured`, or compare prefixes case-insensitively.
    #[test]
    fn a_key_is_configured_only_with_a_configured_prefix() {
        let configured = prefixes(&["EX", "ABC"]);
        assert_eq!(WorkItemKey::classify("ABC-12", &configured), key("ABC", 12));
        assert_eq!(
            WorkItemKey::classify("OTH-7", &configured),
            WorkItemKey::Other("OTH-7".to_owned())
        );
        // Not key-shaped at all: kept, never dropped and never promoted.
        for odd in ["EX-0", "EX-x", "EX", "ex-1", "TOOLONGPREFIX-1", "A-1"] {
            assert_eq!(
                WorkItemKey::classify(odd, &configured),
                WorkItemKey::Other(odd.to_owned()),
                "{odd}"
            );
        }
        assert_eq!(
            WorkItemKey::classify("EX-3", &[]),
            WorkItemKey::Other("EX-3".to_owned())
        );
    }

    /// Mutation: drop the `Other` arm (filter such rows out) -> the length assertion fails.
    #[test]
    fn items_outside_the_configured_prefixes_are_kept_as_other() {
        let items = parse_work_items(SEARCH, &prefixes(&["ABC"])).unwrap();
        let keys: Vec<_> = items.iter().map(|i| i.key.clone()).collect();
        assert_eq!(
            keys,
            ["EX-1", "EX-2", "EX-3"].map(|k| WorkItemKey::Other(k.to_owned()))
        );
    }

    /// Mutation: join the paragraphs without a newline, or skip nested list content.
    #[test]
    fn an_adf_description_is_flattened_to_its_text() {
        assert_eq!(
            parsed().remove(1).description.as_deref(),
            Some("Placeholder first paragraph.\nPlaceholder item.")
        );
    }

    /// Mutation: take 301 characters, or cut by bytes (the multibyte text panics or splits).
    #[test]
    fn a_description_is_cut_at_300_characters() {
        let long = "é".repeat(400);
        let json = format!(
            r#"[{{"key":"EX-9","fields":{{"summary":"s","status":{{"name":"n"}},"issuetype":{{"name":"t"}},"description":"{long}"}}}}]"#
        );
        let items = parse_work_items(json.as_bytes(), &prefixes(&["EX"])).unwrap();
        let description = items.first().unwrap().description.clone().unwrap();
        assert_eq!(description.chars().count(), 300);
        assert!(description.chars().all(|c| c == 'é'));
    }

    /// Mutation: keep a whitespace-only description as `Some`.
    #[test]
    fn a_blank_description_is_none() {
        for blank in [r#""  \n ""#, r#"{"type":"doc","content":[]}"#, "7"] {
            let json = format!(
                r#"[{{"key":"EX-9","fields":{{"summary":"s","status":{{"name":"n"}},"issuetype":{{"name":"t"}},"description":{blank}}}}}]"#
            );
            let items = parse_work_items(json.as_bytes(), &prefixes(&["EX"])).unwrap();
            assert_eq!(items.first().unwrap().description, None, "{blank}");
        }
    }

    /// Mutation: treat an unparseable body as an empty list (`unwrap_or_default`).
    #[test]
    fn output_that_is_not_the_expected_json_is_unparseable_never_empty() {
        let cases: [&[u8]; 6] = [
            UNPARSEABLE,
            b"",
            b"null",
            b"{}",
            br#"[{"key":"EX-1"}]"#,
            br#"[{"key":"EX-1","fields":{"summary":"s","status":{"name":1},"issuetype":{"name":"t"}}}]"#,
        ];
        for stdout in cases {
            let result = parse_work_items(stdout, &prefixes(&["EX"]));
            assert!(
                matches!(result, Err(JiraError::Unparseable { .. })),
                "{}: {result:?}",
                String::from_utf8_lossy(stdout)
            );
        }
    }

    /// An empty result is a real answer: no open items.
    #[test]
    fn an_empty_list_is_an_empty_answer() {
        assert_eq!(parse_work_items(b"[]", &prefixes(&["EX"])).unwrap(), []);
        assert_eq!(
            parse_work_items(br#"{"issues":[]}"#, &prefixes(&["EX"])).unwrap(),
            []
        );
    }

    /// Nothing but the display name leaves the parser: the fixture carries each person's email
    /// address, account id and avatar URLs, and none of them reaches a `Person`.
    /// Mutation: add an `email_address` field to `Person` and fill it from the wire.
    #[test]
    fn only_a_display_name_is_taken_from_a_person() {
        let items = parsed();
        let first = items.first().unwrap();
        assert_eq!(
            first.creator,
            Some(Person {
                display_name: "Person One".to_owned()
            })
        );
        let shown = format!("{items:?}");
        for private in [
            "example.test",
            "00000000000000000000000",
            "avatar",
            "one@",
            "two@",
        ] {
            assert!(!shown.contains(private), "{private} leaked into {shown}");
        }
        // The fixture does carry them, so the absence above is the parser's doing.
        let raw = std::str::from_utf8(SEARCH).unwrap();
        for private in [
            "emailAddress",
            "accountId",
            "avatarUrls",
            "one@example.test",
        ] {
            assert!(raw.contains(private), "{private} missing from the fixture");
        }
    }

    /// Mutation: key the map by the summary, or drop the `Other` rows.
    #[test]
    fn by_key_indexes_configured_and_other_keys_by_their_text() {
        let items = parse_work_items(SEARCH, &prefixes(&["EX"])).unwrap();
        let map = by_key(&items);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("EX-2").unwrap().summary, "Placeholder summary two");
        let other = parse_work_items(SEARCH, &[]).unwrap();
        assert_eq!(by_key(&other).get("EX-1").unwrap().issue_type, "Task");
    }
}
