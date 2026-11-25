//! The regular expressions behind [`super::Extractor`], compiled once, and the
//! classification of one URL into a [`Link`].
//!
//! The configured Jira prefix list is the whole filter for issue keys: no pattern here
//! matches a generic `[A-Z]+-[0-9]+`, so `CVE-2026-1`, `SHA-256` and `UTF-8` are text
//! unless the configuration names their prefix. Without Jira there are no prefixes, no key
//! and no Jira URL pattern, and a Jira URL is any other URL. Without Slack, likewise, a
//! Slack URL is any other URL.

use std::num::NonZeroU32;
use std::ops::Range;

use regex::{Captures, Regex, escape};

use super::LinksError;
use crate::types::{JiraKey, JiraPrefix, Link, LinkTargets, SafeUrl, SafeUrlError};

/// A GitHub account name.
const OWNER: &str = "[A-Za-z0-9][A-Za-z0-9-]*";
/// A GitHub repository name.
const REPO: &str = "[A-Za-z0-9._-]+";
/// What may precede a bare reference such as `#123`: nothing, whitespace or an opening paren.
const REF_START: &str = r"(?:^|[\s(])";
/// Hosts are case-insensitive; paths and keys are not.
const GITHUB_HOST: &str = r"github\.com";

pub(super) struct Patterns {
    prefixes: Vec<JiraPrefix>,
    default_owner: Option<String>,
    markdown: Regex,
    url_token: Regex,
    /// `None` without prefixes, so no text is searched for keys there are none of.
    jira_key: Option<Regex>,
    key_list_next: Regex,
    owner_repo_ref: Regex,
    repo_ref: Regex,
    bare_ref: Regex,
    /// `None` without a Jira host or without prefixes.
    jira_browse: Option<Regex>,
    /// `None` without a Slack host.
    slack_message: Option<Regex>,
    github_pr: Regex,
    docs: Regex,
    local_file: Regex,
}

/// The pieces of a text reference, before they become a [`Link`].
pub(super) enum TextRef<'t> {
    Jira(Link),
    /// `owner/repo#n`
    OwnerRepo {
        owner: &'t str,
        repo: &'t str,
        number: u32,
    },
    /// `repo#n`, resolved against the configured default owner.
    Repo {
        repo: &'t str,
        number: u32,
    },
    /// `#n`, resolved against the session's repository.
    Bare {
        number: u32,
    },
}

impl Patterns {
    pub(super) fn new(
        targets: &LinkTargets,
        prefixes: &[JiraPrefix],
        default_owner: Option<&str>,
    ) -> Result<Self, LinksError> {
        let jira_host = targets.jira.as_ref().map(|host| escape(host.as_str()));
        let slack_host = targets.slack.as_ref().map(|host| escape(host.as_str()));
        let alternatives = (!prefixes.is_empty()).then(|| {
            prefixes
                .iter()
                .map(|p| escape(p.as_str()))
                .collect::<Vec<_>>()
                .join("|")
        });
        let owner = match default_owner {
            None => None,
            Some(owner) if Regex::new(&format!("^{OWNER}$"))?.is_match(owner) => {
                Some(owner.to_owned())
            }
            Some(owner) => return Err(LinksError::InvalidOwner(owner.to_owned())),
        };
        let docs = [
            Some(r"https://(?i:docs\.google\.com)/".to_owned()),
            Some(r"https://(?i:claude\.ai)/artifact/".to_owned()),
            jira_host
                .as_ref()
                .map(|host| format!("https://(?i:{host})/wiki/")),
            slack_host
                .as_ref()
                .map(|host| format!("https://(?i:{host})/docs/")),
        ]
        .into_iter()
        .flatten()
        .map(|prefix| format!("^{prefix}"))
        .collect::<Vec<_>>()
        .join("|");
        let jira_key = alternatives
            .as_ref()
            .map(|alternatives| Regex::new(&format!(r"\b({alternatives})-([0-9]+)\b")))
            .transpose()?;
        let jira_browse = jira_host
            .as_ref()
            .zip(alternatives.as_ref())
            .map(|(host, alternatives)| {
                Regex::new(&format!(
                    "^https://(?i:{host})/browse/({alternatives})-([0-9]+)(?:[/?#].*)?$"
                ))
            })
            .transpose()?;
        let slack_message = slack_host
            .as_ref()
            .map(|host| {
                Regex::new(&format!(
                    "^https://(?i:{host})/archives/([A-Z0-9]+)/p([0-9]+)(?:\\?([^#]*))?(?:#.*)?$"
                ))
            })
            .transpose()?;
        Ok(Self {
            prefixes: prefixes.to_vec(),
            default_owner: owner,
            markdown: Regex::new(r"\[([^\]\n]*)\]\((https?://[^\s)]+)\)")?,
            url_token: Regex::new(r#"https?://[^\s<>"`]+"#)?,
            jira_key,
            key_list_next: Regex::new(r"^(?:[ \t]*[,/&][ \t]*|,?[ \t]+(?:and|or)[ \t]+)([0-9]+)")?,
            owner_repo_ref: Regex::new(&format!(r"{REF_START}({OWNER})/({REPO})#([0-9]+)\b"))?,
            repo_ref: Regex::new(&format!(r"{REF_START}({REPO})#([0-9]+)\b"))?,
            bare_ref: Regex::new(&format!(r"{REF_START}#([0-9]{{2,6}})\b"))?,
            jira_browse,
            slack_message,
            github_pr: Regex::new(&format!(
                "^https://(?i:{GITHUB_HOST})/({OWNER})/({REPO})/pull/([0-9]+)(?:/[^?#]*)?(?:[?#].*)?$"
            ))?,
            docs: Regex::new(&docs)?,
            // A path runs to whitespace, a quote, a backtick, an angle bracket or a bracket,
            // and starts after one or at the start of the text.
            local_file: Regex::new(r#"(?:^|[\s(\["'`<])(/[^\s"'`<>()\[\]]+\.md)\b"#)?,
        })
    }

    /// Absolute paths ending in `.md`: the span of the path itself, without the character
    /// before it. Whether one is a file the site shows is the caller's question.
    pub(super) fn local_files<'t>(&self, text: &'t str) -> impl Iterator<Item = regex::Match<'t>> {
        self.local_file
            .captures_iter(text)
            .filter_map(|caps| caps.get(1))
    }

    /// `[label](https://…)` or `[label](http://…)`: the whole match, the label and the URL.
    pub(super) fn markdown_links<'t>(&self, text: &'t str) -> impl Iterator<Item = Captures<'t>> {
        self.markdown.captures_iter(text)
    }

    /// Every `http(s)://` run of non-space characters, trailing punctuation included.
    pub(super) fn url_tokens<'t>(&self, text: &'t str) -> impl Iterator<Item = regex::Match<'t>> {
        self.url_token.find_iter(text)
    }

    /// Jira keys, `owner/repo#n`, `repo#n` and `#n`,
    /// each with the span of the reference itself, without the character before it.
    pub(super) fn text_refs<'t>(&self, text: &'t str) -> Vec<(Range<usize>, TextRef<'t>)> {
        let mut found = Vec::new();
        let keys = self
            .jira_key
            .iter()
            .flat_map(|pattern| pattern.captures_iter(text));
        for caps in keys {
            let (Some(whole), Some(prefix), Some(number)) = (caps.get(0), caps.get(1), caps.get(2))
            else {
                continue;
            };
            let Some(link) = self.jira_link(prefix.as_str(), number.as_str()) else {
                continue;
            };
            found.push((whole.range(), TextRef::Jira(link)));
            found.extend(
                self.key_list_tail(text, whole.end(), number.len())
                    .into_iter()
                    .filter_map(|span| {
                        let link = self.jira_link(prefix.as_str(), text.get(span.clone())?)?;
                        Some((span, TextRef::Jira(link)))
                    }),
            );
        }
        found.extend(self.owner_repo_ref.captures_iter(text).filter_map(|caps| {
            let owner = caps.get(1)?;
            Some((
                owner.start()..caps.get(0)?.end(),
                TextRef::OwnerRepo {
                    owner: owner.as_str(),
                    repo: caps.get(2)?.as_str(),
                    number: caps.get(3)?.as_str().parse().ok()?,
                },
            ))
        }));
        found.extend(self.repo_ref.captures_iter(text).filter_map(|caps| {
            let repo = caps.get(1)?;
            Some((
                repo.start()..caps.get(0)?.end(),
                TextRef::Repo {
                    repo: repo.as_str(),
                    number: caps.get(2)?.as_str().parse().ok()?,
                },
            ))
        }));
        found.extend(self.bare_ref.captures_iter(text).filter_map(|caps| {
            let digits = caps.get(1)?;
            // The `#` is the one byte before the digits.
            Some((
                digits.start().saturating_sub(1)..caps.get(0)?.end(),
                TextRef::Bare {
                    number: digits.as_str().parse().ok()?,
                },
            ))
        }));
        found
    }

    pub(super) fn default_owner(&self) -> Option<&str> {
        self.default_owner.as_deref()
    }

    /// The link a URL stands for, or why it is not safe to link at all.
    pub(super) fn classify(&self, url: &str) -> Result<Link, SafeUrlError> {
        let safe = SafeUrl::parse(url)?;
        if let Some(link) = self.known(url) {
            return Ok(link);
        }
        Ok(if self.docs.is_match(url) {
            Link::Doc { url: safe }
        } else {
            Link::Other { url: safe }
        })
    }

    fn known(&self, url: &str) -> Option<Link> {
        if let Some(caps) = self
            .jira_browse
            .as_ref()
            .and_then(|pattern| pattern.captures(url))
        {
            return self.jira_link(caps.get(1)?.as_str(), caps.get(2)?.as_str());
        }
        if let Some(caps) = self
            .slack_message
            .as_ref()
            .and_then(|pattern| pattern.captures(url))
        {
            return slack_link(&caps);
        }
        let caps = self.github_pr.captures(url)?;
        let number: u32 = caps.get(3)?.as_str().parse().ok().filter(|&n| n > 0)?;
        Some(Link::PullRequest {
            owner: caps.get(1)?.as_str().to_owned(),
            repo: caps.get(2)?.as_str().to_owned(),
            number,
        })
    }

    /// The key whose prefix is the configured one named `prefix`; `None` for an issue
    /// number zero or beyond `u32`.
    fn jira_link(&self, prefix: &str, number: &str) -> Option<Link> {
        let prefix = self.prefixes.iter().find(|p| p.as_str() == prefix)?;
        let number = NonZeroU32::new(number.parse().ok()?)?;
        Some(Link::Jira {
            key: JiraKey::new(prefix.clone(), number),
        })
    }

    /// The bare issue numbers continuing a key's list from byte `start`, as in
    /// `ABC-123, 134 and 345` or `ABC-581/694`: each a span of `text`, read under the
    /// key's prefix.
    ///
    /// A number counts only with as many digits as the key's own and when neither a word
    /// character, a `-` nor a `.` and a digit follows it, so a date (`ABC-906, 2026-03-14`),
    /// a count (`ABC-123, 2 files`) or a version (`ABC-123, 1.2`) ends the list instead of
    /// joining it.
    fn key_list_tail(&self, text: &str, start: usize, digits: usize) -> Vec<Range<usize>> {
        let mut found = Vec::new();
        let mut cursor = start;
        while let Some(number) = text
            .get(cursor..)
            .and_then(|rest| self.key_list_next.captures(rest))
            .and_then(|caps| caps.get(1))
        {
            let span = cursor + number.start()..cursor + number.end();
            let mut after = text.get(span.end..).unwrap_or_default().chars();
            let ends_cleanly = match (after.next(), after.next()) {
                (Some(c), _) if c.is_alphanumeric() || c == '_' || c == '-' => false,
                (Some('.'), Some(d)) if d.is_ascii_digit() => false,
                _ => true,
            };
            if number.len() != digits || !ends_cleanly {
                break;
            }
            cursor = span.end;
            found.push(span);
        }
        found
    }
}

/// A message permalink. A reply's permalink carries `thread_ts`, the parent's timestamp;
/// the thread is the parent, so that wins over the reply's own `p` timestamp.
fn slack_link(caps: &Captures<'_>) -> Option<Link> {
    let channel = caps.get(1)?.as_str().to_owned();
    let own = permalink_ts(caps.get(2)?.as_str())?;
    let parent = caps
        .get(3)
        .and_then(|query| {
            query
                .as_str()
                .split('&')
                .find_map(|pair| pair.strip_prefix("thread_ts="))
        })
        .filter(|ts| is_slack_ts(ts));
    Some(Link::SlackThread {
        channel,
        ts: parent.map_or(own, str::to_owned),
    })
}

/// `1788350000000100` is `1788350000.000100`: the last six digits are microseconds.
fn permalink_ts(digits: &str) -> Option<String> {
    let (seconds, micros) = digits.split_at_checked(digits.len().checked_sub(MICRO_DIGITS)?)?;
    (!seconds.is_empty()).then(|| format!("{seconds}.{micros}"))
}

const MICRO_DIGITS: usize = 6;

/// A Slack timestamp as it appears in a query: seconds, a dot, six digits.
fn is_slack_ts(text: &str) -> bool {
    text.split_once('.').is_some_and(|(seconds, micros)| {
        !seconds.is_empty()
            && seconds.bytes().all(|b| b.is_ascii_digit())
            && micros.len() == MICRO_DIGITS
            && micros.bytes().all(|b| b.is_ascii_digit())
    })
}

/// A URL found in prose without the punctuation that surrounds it there: `.,;:` and a
/// closing quote or `>` are never part of it, and `)` or `]` only when they close a
/// bracket the URL itself opened.
pub(super) fn trim_url(raw: &str) -> &str {
    let count = |s: &str, c: char| s.matches(c).count();
    let mut url = raw;
    while let Some(last) = url.chars().next_back() {
        let surplus = match last {
            '.' | ',' | ';' | ':' | '>' | '\'' | '"' => true,
            ')' => count(url, ')') > count(url, '('),
            ']' => count(url, ']') > count(url, '['),
            _ => false,
        };
        match url.strip_suffix(last) {
            Some(shorter) if surplus => url = shorter,
            _ => break,
        }
    }
    url
}
