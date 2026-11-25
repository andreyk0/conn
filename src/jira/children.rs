//! How far an epic has come: how many of its children are done, out of how many.
//!
//! `acli jira workitem search` refuses the `parent` field, so a child cannot name its epic in
//! the list of work items; each epic's children are asked for on their own, with JQL
//! `parent = <epic>`, which Jira answers for company- and team-managed projects alike.

use serde::Deserialize;

use super::item::StatusCategory;
use super::{ACLI, ACLI_TIMEOUT, JiraError, classify_failure};
use crate::types::JiraKey;
use crate::util::runner::{CommandRunner, run_tool};

/// The most children one read counts. An epic holds a few to a few dozen; at this many the
/// count says it may be cut off.
const LIMIT: usize = 100;

/// How old an epic's count may grow before it is read again: children move on the scale of
/// hours, and the page states the reads' age on the health page.
pub const READ_EVERY: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

/// An epic's children by whether they are done.
///
/// Witnesses `done <= total`: its one constructor counts both from the same list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    done: usize,
    total: usize,
}

impl Progress {
    /// The count of `categories`, one per child; a child is done when its status is in Jira's
    /// done category.
    pub fn count<'c>(categories: impl Iterator<Item = &'c StatusCategory>) -> Self {
        let (done, total) = categories.fold((0, 0), |(done, total), category| {
            let done_here = usize::from(*category == StatusCategory::Done);
            (done + done_here, total + 1)
        });
        Self { done, total }
    }

    pub const fn done(self) -> usize {
        self.done
    }

    pub const fn total(self) -> usize {
        self.total
    }

    /// The read returned a full page, so there may be more children than counted.
    pub const fn may_be_cut(self) -> bool {
        self.total >= LIMIT
    }
}

/// The children of `epic`, counted.
pub async fn fetch(runner: &dyn CommandRunner, epic: &JiraKey) -> Result<Progress, JiraError> {
    let jql = format!("parent = {epic}");
    let limit = LIMIT.to_string();
    let args = [
        "jira",
        "workitem",
        "search",
        "--jql",
        jql.as_str(),
        "--json",
        "--limit",
        limit.as_str(),
        "--fields",
        "key,status",
    ];
    let output = run_tool(runner, ACLI, &args, ACLI_TIMEOUT)
        .await
        .map_err(classify_failure)?;
    parse(&output.stdout)
}

#[derive(Deserialize)]
struct WireChild {
    fields: WireFields,
}

#[derive(Deserialize)]
struct WireFields {
    status: WireStatus,
}

#[derive(Deserialize)]
struct WireStatus {
    #[serde(rename = "statusCategory")]
    status_category: Option<WireCategory>,
}

#[derive(Deserialize)]
struct WireCategory {
    key: String,
}

/// The count in a `search --json --fields key,status` output: a list of children.
pub fn parse(stdout: &[u8]) -> Result<Progress, JiraError> {
    let children: Vec<WireChild> =
        serde_json::from_slice(stdout).map_err(|e| JiraError::Unparseable {
            detail: e.to_string(),
        })?;
    let categories: Vec<StatusCategory> = children
        .iter()
        .map(|child| {
            StatusCategory::from_key(
                child
                    .fields
                    .status
                    .status_category
                    .as_ref()
                    .map(|c| c.key.as_str()),
            )
        })
        .collect();
    Ok(Progress::count(categories.iter()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::runner::{Call, Exit, FakeRunner, Output};
    use std::time::Duration;

    fn child(category: &str) -> String {
        format!(
            r#"{{"key":"ABC-9","fields":{{"status":{{"name":"x","statusCategory":{{"key":"{category}"}}}}}}}}"#
        )
    }

    /// Mutation: count `indeterminate` as done, or count only the done children as the total.
    #[test]
    fn done_children_are_counted_out_of_all() {
        let stdout = format!(
            "[{},{},{},{}]",
            child("done"),
            child("indeterminate"),
            child("new"),
            child("done")
        );
        let progress = parse(stdout.as_bytes()).unwrap();
        assert_eq!((progress.done(), progress.total()), (2, 4));
        assert!(!progress.may_be_cut());
        let none = parse(b"[]").unwrap();
        assert_eq!((none.done(), none.total()), (0, 0));
        assert!(matches!(
            parse(b"not json"),
            Err(JiraError::Unparseable { .. })
        ));
    }

    /// Mutation: say "cut" below the limit, or never.
    #[test]
    fn a_full_page_may_be_cut() {
        let full = vec![StatusCategory::ToDo; LIMIT];
        assert!(Progress::count(full.iter()).may_be_cut());
        let below = vec![StatusCategory::ToDo; LIMIT - 1];
        assert!(!Progress::count(below.iter()).may_be_cut());
    }

    /// Mutation: ask for another field, drop the limit, or query by `"Epic Link"`.
    #[tokio::test]
    async fn asks_acli_for_the_children_of_the_epic() {
        let runner = FakeRunner::new([Ok(Output {
            exit: Exit::Success,
            stdout: b"[]".to_vec(),
            stderr: String::new(),
        })]);
        fetch(&runner, &JiraKey::parse("ABC-7").unwrap())
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            [Call {
                program: "acli".to_owned(),
                args: [
                    "jira",
                    "workitem",
                    "search",
                    "--jql",
                    "parent = ABC-7",
                    "--json",
                    "--limit",
                    "100",
                    "--fields",
                    "key,status",
                ]
                .map(str::to_owned)
                .to_vec(),
                input: Vec::new(),
                timeout: Duration::from_secs(20),
            }]
        );
    }
}
