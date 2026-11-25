//! Finding Jira keys, Slack permalinks, GitHub PRs and document URLs in session text.
//!
//! Rendering a [`Link`] to an `href` is [`Link::url`]; this module only recognises them.
//! What it recognises is fixed by the configuration: the Jira prefixes and hosts name the
//! issues and message permalinks that count, and every other `https://` URL, and `http://` to a
//! local host, is a [`Link::Other`]. Any other URL [`SafeUrl`] refuses is not a link and is
//! counted in `dropped`, so a renderer can say that something was left out.
//!
//! An absolute path to a `.md` file under the configured [`FileRoots`] is a
//! [`Segment::File`] for a renderer to open; it is not a [`Link`], so it is no chip.

mod patterns;
mod session;

use std::collections::HashSet;
use std::ops::Range;

use crate::config::Config;
use crate::types::{FileRoots, JiraPrefix, Link, LinkTargets, LocalFile, SafeUrlError};
use patterns::{Patterns, TextRef, trim_url};

pub use session::{Extracted, extract};

/// The extractor could not be built from its configuration.
#[derive(Debug, thiserror::Error)]
pub enum LinksError {
    #[error("a pattern built from the configuration does not compile: {0}")]
    Pattern(#[from] regex::Error),
    #[error("not a GitHub owner name: {0:?}")]
    InvalidOwner(String),
}

/// The repository a session works in; a bare `#123` in its text refers to a pull request there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

/// What the text is known to be about, beyond the text itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkContext {
    pub repo: Option<RepoRef>,
}

/// A link and where it first appears: a byte range of the text it was found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub link: Link,
    pub span: Range<usize>,
}

/// The distinct links in a text, in order of first appearance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    pub found: Vec<Found>,
    /// URLs that were recognised and refused: `http://` to a host that is not local, or not safe to use as an `href`.
    pub dropped: u32,
}

/// A piece of the text. The pieces are in order and together cover the whole text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'t> {
    Text(&'t str),
    Link {
        link: Link,
        /// What to display: the label of a markdown link, else the same as `source`.
        shown: &'t str,
        /// The text this segment replaces, exactly as written.
        source: &'t str,
    },
    /// A local markdown file under the configured roots, written as its path.
    File {
        file: LocalFile,
        source: &'t str,
    },
}

/// The whole text as segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmented<'t> {
    pub segments: Vec<Segment<'t>>,
    /// As in [`Extraction::dropped`].
    pub dropped: u32,
}

pub struct Extractor {
    patterns: Patterns,
    roots: FileRoots,
}

/// What an occurrence points at.
enum Target {
    Link(Link),
    File(LocalFile),
}

/// One accepted occurrence.
struct Occurrence {
    target: Target,
    /// The whole matched text, `[label](url)` for markdown.
    span: Range<usize>,
    /// The part to display.
    shown: Range<usize>,
}

struct Scan {
    occurrences: Vec<Occurrence>,
    dropped: u32,
}

impl Extractor {
    /// `prefixes` is the whole filter for issue keys; see [`patterns`]. A local file is
    /// found only under `roots`.
    pub fn new(
        targets: &LinkTargets,
        prefixes: &[JiraPrefix],
        default_owner: Option<&str>,
        roots: FileRoots,
    ) -> Result<Self, LinksError> {
        Ok(Self {
            patterns: Patterns::new(targets, prefixes, default_owner)?,
            roots,
        })
    }

    pub fn from_config(config: &Config) -> Result<Self, LinksError> {
        Self::new(
            &config.link_targets(),
            config.jira_prefixes(),
            config.github.default_owner.as_deref(),
            config.files.clone(),
        )
    }

    /// The links of a text; local files are not links and are left out.
    pub fn extract(&self, text: &str, ctx: &LinkContext) -> Extraction {
        let Scan {
            occurrences,
            dropped,
        } = self.scan(text, ctx);
        let mut seen = HashSet::new();
        let found = occurrences
            .into_iter()
            .filter_map(|o| match o.target {
                Target::Link(link) => Some(Found { link, span: o.span }),
                Target::File(_) => None,
            })
            .filter(|found| seen.insert(found.link.clone()))
            .collect();
        Extraction { found, dropped }
    }

    /// The directories local files are found under; a reader shows files from these alone.
    pub const fn roots(&self) -> &FileRoots {
        &self.roots
    }

    /// The local file that `text` is, exactly, when it lies under the roots: how a code span
    /// holding only a path becomes a link to it.
    pub fn local_file(&self, text: &str) -> Option<LocalFile> {
        LocalFile::parse(text)
            .ok()
            .filter(|file| self.roots.contain(file.path()))
    }

    /// The link that `text` is, exactly: no words around it, nothing left over. How a stored
    /// link string, which is a rendered URL, becomes a [`Link`] again.
    pub fn parse_exact(&self, text: &str) -> Option<Link> {
        match self.extract(text, &LinkContext::default()).found.as_slice() {
            [only] if only.span == (0..text.len()) => Some(only.link.clone()),
            _ => None,
        }
    }

    /// Every occurrence, repeats included: a renderer links each one.
    pub fn segments<'t>(&self, text: &'t str, ctx: &LinkContext) -> Segmented<'t> {
        let Scan {
            occurrences,
            dropped,
        } = self.scan(text, ctx);
        let mut segments = Vec::new();
        let mut cursor = 0;
        for o in occurrences {
            push_text(&mut segments, text, cursor..o.span.start);
            cursor = o.span.end;
            let source = slice(text, o.span);
            segments.push(match o.target {
                Target::Link(link) => Segment::Link {
                    link,
                    shown: slice(text, o.shown),
                    source,
                },
                Target::File(file) => Segment::File { file, source },
            });
        }
        push_text(&mut segments, text, cursor..text.len());
        Segmented { segments, dropped }
    }

    fn scan(&self, text: &str, ctx: &LinkContext) -> Scan {
        let mut candidates = Vec::new();
        for caps in self.patterns.markdown_links(text) {
            let (Some(whole), Some(label), Some(url)) = (caps.get(0), caps.get(1), caps.get(2))
            else {
                continue;
            };
            if let Ok(link) = self.patterns.classify(url.as_str()) {
                candidates.push(Occurrence {
                    target: Target::Link(link),
                    span: whole.range(),
                    shown: label.range(),
                });
            }
        }
        let mut refused = Vec::new();
        for token in self.patterns.url_tokens(text) {
            let url = trim_url(token.as_str());
            let span = token.start()..token.start() + url.len();
            match self.patterns.classify(url) {
                Ok(link) => candidates.push(Occurrence {
                    target: Target::Link(link),
                    shown: span.clone(),
                    span,
                }),
                Err(SafeUrlError { .. }) => refused.push(span),
            }
        }
        for (span, reference) in self.patterns.text_refs(text) {
            if let Some(link) = self.text_link(reference, ctx) {
                candidates.push(Occurrence {
                    target: Target::Link(link),
                    shown: span.clone(),
                    span,
                });
            }
        }
        for path in self.patterns.local_files(text) {
            if let Some(file) = self.local_file(path.as_str()) {
                candidates.push(Occurrence {
                    target: Target::File(file),
                    span: path.range(),
                    shown: path.range(),
                });
            }
        }
        let occurrences = disjoint(candidates);
        // A refused URL inside an accepted markdown label is already accounted for by it.
        let dropped = refused
            .iter()
            .filter(|r| {
                !occurrences
                    .iter()
                    .any(|o| o.span.start <= r.start && r.end <= o.span.end)
            })
            .count();
        Scan {
            occurrences,
            dropped: u32::try_from(dropped).unwrap_or(u32::MAX),
        }
    }

    fn text_link(&self, reference: TextRef<'_>, ctx: &LinkContext) -> Option<Link> {
        let pull_request = |owner: &str, repo: &str, number: u32| {
            (number > 0).then(|| Link::PullRequest {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number,
            })
        };
        match reference {
            TextRef::Jira(link) => Some(link),
            TextRef::OwnerRepo {
                owner,
                repo,
                number,
            } => pull_request(owner, repo, number),
            TextRef::Repo { repo, number } => {
                pull_request(self.patterns.default_owner()?, repo, number)
            }
            TextRef::Bare { number } => {
                let RepoRef { owner, repo } = ctx.repo.as_ref()?;
                pull_request(owner, repo, number)
            }
        }
    }
}

/// The candidates that do not overlap, earliest first. Of two that start together the one
/// found first wins, and `scan` finds markdown, then URLs, then keys, then references, then
/// local files: the more specific reading of the same text comes first.
fn disjoint(mut candidates: Vec<Occurrence>) -> Vec<Occurrence> {
    // A stable sort, which is what makes the order of discovery the tie-break.
    candidates.sort_by_key(|c| c.span.start);
    let mut end = 0;
    candidates.retain(|c| {
        let free = c.span.start >= end;
        if free {
            end = c.span.end;
        }
        free
    });
    candidates
}

fn push_text<'t>(segments: &mut Vec<Segment<'t>>, text: &'t str, range: Range<usize>) {
    let piece = slice(text, range);
    if !piece.is_empty() {
        segments.push(Segment::Text(piece));
    }
}

/// Every range passed here comes from a regex match over this same `text`, so it is always
/// on character boundaries; the round-trip property test pins that.
fn slice(text: &str, range: Range<usize>) -> &str {
    text.get(range).unwrap_or_default()
}

#[cfg(test)]
mod tests;
