//! Free-text search over what the model holds: every session, live and dormant.
//!
//! The search reads only the model, never a transcript: a session's title, working
//! directory and links, its held human prompts ([`Model::prompts`]) and its held turn ends
//! ([`SessionView::last_turn_ends`]). Every text it matches against is already redacted, so
//! a snippet cannot show a credential the page would have masked, and a credential cannot be
//! searched for.
//!
//! A session is found when any field matches; its [`Found`] carries one [`Hit`] per matched
//! field, strongest field first. Sessions rank by their strongest field ([`Matched`] declares
//! the order), then active before dormant, then newest activity first.

use std::cmp::Reverse;

use super::{Model, SessionView};
use crate::types::{Instant, Link, SessionId};
use crate::util::redact::Redacted;

/// The most sessions one search returns; the rest are counted ([`Results::matched`]).
///
/// A page of two hundred results is already more than a person reads; the bound keeps the
/// page and the work to render it independent of how many sessions the model holds.
pub const RESULTS_MAX: usize = 200;

/// The longest query, in characters.
pub const QUERY_CHARS_MAX: usize = 200;

/// How many characters of context a snippet shows on each side of the match.
pub const CONTEXT_CHARS: usize = 60;

/// What a person typed, trimmed.
///
/// Witnesses that the text says something and is at most [`QUERY_CHARS_MAX`] characters, and
/// carries the case-folded form every match compares against, so no match folds the query
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    text: String,
    folded: Vec<char>,
}

/// Why a text is not a [`Query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("is empty or only whitespace")]
    Blank,
    #[error("is {chars} characters; at most {max} are allowed")]
    TooLong { chars: usize, max: usize },
}

impl Query {
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(QueryError::Blank);
        }
        let chars = text.chars().count();
        if chars > QUERY_CHARS_MAX {
            return Err(QueryError::TooLong {
                chars,
                max: QUERY_CHARS_MAX,
            });
        }
        Ok(Self {
            folded: text.chars().map(fold).collect(),
            text: text.to_owned(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// How many characters the query has, which is how many a match covers.
    fn chars(&self) -> usize {
        self.folded.len()
    }
}

/// The lower case of a character, or the character itself when its lower case is not one
/// character, so a folded text has as many characters as the text and indexes agree.
fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(only), None) => only,
        _ => c,
    }
}

/// The text of the model a query matched in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched {
    Title,
    Cwd,
    /// The link whose text, or whose Jira key or pull request number, the query names.
    Link(Link),
    /// The latest held prompt that contains the query.
    Prompt,
    /// The latest held turn end that contains the query.
    Reply,
}

/// How strongly a field vouches that the session is the one looked for, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    Title,
    Cwd,
    Link,
    Prompt,
    Reply,
}

impl Matched {
    const fn strength(&self) -> Strength {
        match self {
            Self::Title => Strength::Title,
            Self::Cwd => Strength::Cwd,
            Self::Link(_) => Strength::Link,
            Self::Prompt => Strength::Prompt,
            Self::Reply => Strength::Reply,
        }
    }
}

/// The matched text with its surroundings, each part redacted.
///
/// Built from text that was redacted before it was matched, so the parts hold what the page
/// already shows. `cut_before` and `cut_after` say that the text goes on beyond the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub before: Redacted<String>,
    pub matched: Redacted<String>,
    pub after: Redacted<String>,
    pub cut_before: bool,
    pub cut_after: bool,
}

impl Snippet {
    /// The characters `start..start + len` of `text` with [`CONTEXT_CHARS`] around them.
    ///
    /// A prefix, a suffix or a middle of redacted text holds no credential the whole did not,
    /// and each part is redacted again because it is a new string; a cut inside a mask is
    /// harmless. Whitespace becomes a space, one for one, so a snippet is one line.
    fn of(text: &Redacted<String>, start: usize, len: usize) -> Self {
        let from = start.saturating_sub(CONTEXT_CHARS);
        let end = start.saturating_add(len);
        let part = |skip: usize, take: usize| {
            Redacted::of(
                &text
                    .as_str()
                    .chars()
                    .skip(skip)
                    .take(take)
                    .map(|c| if c.is_whitespace() { ' ' } else { c })
                    .collect::<String>(),
            )
        };
        Self {
            before: part(from, start.saturating_sub(from)),
            matched: part(start, len),
            after: part(end, CONTEXT_CHARS),
            cut_before: from > 0,
            cut_after: text
                .as_str()
                .chars()
                .nth(end.saturating_add(CONTEXT_CHARS))
                .is_some(),
        }
    }
}

/// One matched field of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub field: Matched,
    pub snippet: Snippet,
}

/// A session the query matched in, with every field it matched in, strongest first.
///
/// Never empty: it is built only from at least one hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    session: SessionId,
    hits: Vec<Hit>,
}

impl Found {
    fn of(session: SessionId, hits: Vec<Hit>) -> Option<Self> {
        (!hits.is_empty()).then_some(Self { session, hits })
    }

    pub const fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    fn strongest(&self) -> Option<Strength> {
        self.hits.iter().map(|hit| hit.field.strength()).min()
    }
}

/// What a search found: at most [`RESULTS_MAX`] sessions, and how many matched in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Results {
    found: Vec<Found>,
    matched: usize,
}

impl Results {
    pub fn found(&self) -> &[Found] {
        &self.found
    }

    /// How many sessions matched, whether or not they are in [`Results::found`].
    pub const fn matched(&self) -> usize {
        self.matched
    }

    pub const fn truncated(&self) -> bool {
        self.matched > self.found.len()
    }
}

/// Whether a session is in the model's active list or its dormant one; active sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Listed {
    Active,
    Dormant,
}

/// The sessions of `model` in which `query` occurs, best first.
pub fn search(model: &Model, query: &Query) -> Results {
    let listed = model
        .sessions
        .iter()
        .map(|view| (Listed::Active, view))
        .chain(model.dormant.iter().map(|view| (Listed::Dormant, view)));
    let mut ranked: Vec<(Strength, Listed, Reverse<Option<Instant>>, Found)> = listed
        .filter_map(|(listed, view)| {
            let found = Found::of(view.session.id.clone(), hits_in(model, view, query))?;
            Some((
                found.strongest()?,
                listed,
                Reverse(view.last_activity),
                found,
            ))
        })
        .collect();
    // A stable sort: sessions that tie keep the model's order.
    ranked.sort_by_key(|(strength, listed, activity, _)| (*strength, *listed, *activity));
    let matched = ranked.len();
    Results {
        found: ranked
            .into_iter()
            .take(RESULTS_MAX)
            .map(|(_, _, _, found)| found)
            .collect(),
        matched,
    }
}

fn hits_in(model: &Model, view: &SessionView, query: &Query) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut push = |field: Matched, snippet: Option<Snippet>| {
        hits.extend(snippet.map(|snippet| Hit { field, snippet }));
    };
    push(
        Matched::Title,
        view.title.as_ref().and_then(|title| in_text(title, query)),
    );
    let cwd = view
        .session
        .cwd
        .as_ref()
        .map(|cwd| Redacted::of(&cwd.to_string_lossy()));
    push(
        Matched::Cwd,
        cwd.as_ref().and_then(|cwd| in_text(cwd, query)),
    );
    for link_view in &view.links {
        push(
            Matched::Link(link_view.link.clone()),
            link_snippet(&link_view.link, query),
        );
    }
    let prompts = model
        .prompts
        .get(&view.session.id)
        .map_or(&[][..], |prompts| &prompts[..]);
    push(
        Matched::Prompt,
        prompts
            .iter()
            .rev()
            .find_map(|prompt| in_text(&prompt.text, query)),
    );
    push(
        Matched::Reply,
        view.last_turn_ends
            .iter()
            .find_map(|end| in_text(end.text.redacted(), query)),
    );
    hits
}

/// The first case-insensitive occurrence of the query in the text, with its context.
fn in_text(text: &Redacted<String>, query: &Query) -> Option<Snippet> {
    let start = find(text.as_str(), query)?;
    Some(Snippet::of(text, start, query.chars()))
}

/// The character index of the first case-insensitive occurrence of the query in `text`.
fn find(text: &str, query: &Query) -> Option<usize> {
    let (first, rest) = query.folded.split_first()?;
    let mut chars = text.chars().enumerate();
    while let Some((at, c)) = chars.next() {
        if fold(c) == *first {
            let mut probe = chars.clone();
            if rest
                .iter()
                .all(|want| probe.next().is_some_and(|(_, got)| fold(got) == *want))
            {
                return Some(at);
            }
        }
    }
    None
}

/// Whether `text` is the query, ignoring case.
fn is_query(text: &str, query: &Query) -> bool {
    text.chars().map(fold).eq(query.folded.iter().copied())
}

/// How a link is named by a query.
///
/// A Jira key matches only as a whole, so `ABC-1` does not find `ABC-12`. A pull request
/// matches as `owner/repo#12` in full, as `#12` by its number alone, or by a part of
/// `owner/repo`. A link that is a URL matches by any part of it.
fn link_snippet(link: &Link, query: &Query) -> Option<Snippet> {
    let shown = Redacted::of(&link.to_string());
    let whole = || is_query(shown.as_str(), query).then(|| (0, query.chars()));
    let (start, len) = match link {
        Link::Jira { .. } => whole()?,
        Link::PullRequest {
            owner,
            repo,
            number,
        } => {
            let repository_chars = owner.chars().count() + 1 + repo.chars().count();
            match query.as_str().strip_prefix('#') {
                Some(digits) => {
                    (digits == number.to_string()).then_some((repository_chars, query.chars()))?
                }
                None if query.as_str().contains('#') => whole()?,
                None => {
                    let start = find(shown.as_str(), query)?;
                    (start.saturating_add(query.chars()) <= repository_chars)
                        .then_some((start, query.chars()))?
                }
            }
        }
        Link::SlackThread { .. } | Link::Doc { .. } | Link::Other { .. } => {
            (find(shown.as_str(), query)?, query.chars())
        }
    };
    Some(Snippet::of(&shown, start, len))
}

#[cfg(test)]
mod tests {
    //! Goal: each field kind is found and shown with its context, the order is the one the
    //! page promises, a Jira key matches as a whole, and the result is bounded.
    //!
    //! Method: hand-built models of a few sessions whose text is written out in the test; every
    //! expected snippet, order and count is written by hand from the spec, never produced by
    //! the code under test.

    use std::sync::Arc;

    use super::*;
    use crate::model::HeldPrompt;
    use crate::testutil::{ago, session_id as id, session_view};
    use crate::types::{
        Activity, AssistantText, BackgroundTasks, JiraKey, JiraPrefix, ListingStatus, SafeUrl,
        TurnEnd, TurnTrigger,
    };

    /// A made-up credential in the shape the redaction patterns know.
    const SECRET: &str = "ghp_aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";

    fn query(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    fn running() -> Activity {
        Activity::Running { since: ago(5) }
    }

    fn idle() -> Activity {
        Activity::Idle {
            ended_at: ago(5),
            listing: ListingStatus::Seen,
        }
    }

    fn dormant() -> Activity {
        Activity::Dormant {
            last_seen: ago(500),
            background: None,
        }
    }

    fn jira(n: u32) -> Link {
        Link::Jira {
            key: JiraKey::new(
                JiraPrefix::parse("ABC").unwrap(),
                std::num::NonZeroU32::new(n).unwrap(),
            ),
        }
    }

    fn pr(number: u32) -> Link {
        Link::PullRequest {
            owner: "example-org".to_owned(),
            repo: "example-repo".to_owned(),
            number,
        }
    }

    fn with_link(mut view: SessionView, link: Link) -> SessionView {
        view.links.push(crate::model::LinkView {
            url: link.url(&crate::testutil::link_targets()).unwrap(),
            link,
            pr: None,
        });
        view
    }

    fn with_reply(mut view: SessionView, secs_before_now: i64, text: &str) -> SessionView {
        view.last_turn_ends.push(TurnEnd {
            at: ago(secs_before_now),
            trigger: TurnTrigger::Human,
            text: AssistantText::of(text),
            background_tasks: BackgroundTasks::Unknown,
            asked: None,
        });
        view
    }

    fn model_of(sessions: Vec<SessionView>, dormant: Vec<SessionView>) -> Model {
        let mut model = Model::unread(ago(0));
        model.sessions = sessions;
        model.dormant = dormant;
        model
    }

    fn hold_prompts(model: &mut Model, session: u8, texts: &[&str]) {
        let held: Vec<HeldPrompt> = texts
            .iter()
            .enumerate()
            .map(|(at, text)| HeldPrompt {
                at: ago(1000 - i64::try_from(at).unwrap()),
                text: Redacted::of(text),
            })
            .collect();
        model.prompts.insert(id(session), Arc::from(held));
    }

    fn sessions_found(results: &Results) -> Vec<SessionId> {
        results
            .found()
            .iter()
            .map(|found| found.session().clone())
            .collect()
    }

    fn fields(found: &Found) -> Vec<Matched> {
        found.hits().iter().map(|hit| hit.field.clone()).collect()
    }

    fn shown(snippet: &Snippet) -> (String, String, String) {
        (
            snippet.before.as_str().to_owned(),
            snippet.matched.as_str().to_owned(),
            snippet.after.as_str().to_owned(),
        )
    }

    /// Mutation: skip the `trim` (ran red).
    #[test]
    fn a_query_is_trimmed_nonblank_and_at_most_two_hundred_characters() {
        assert_eq!(query("  Parser \n").as_str(), "Parser");
        assert_eq!(Query::parse(""), Err(QueryError::Blank));
        assert_eq!(Query::parse(" \t "), Err(QueryError::Blank));
        assert!(Query::parse(&"x".repeat(200)).is_ok());
        assert!(Query::parse(&"é".repeat(200)).is_ok());
        assert_eq!(
            Query::parse(&"x".repeat(201)),
            Err(QueryError::TooLong {
                chars: 201,
                max: 200
            })
        );
    }

    /// One session per field; each is found by its own text and by nothing else.
    /// Mutation: compare case-sensitively (ran red on the `PARSER` row).
    #[test]
    fn every_field_kind_is_found_and_says_which() {
        let mut model = model_of(
            vec![
                session_view(1, Some("Parser rework"), running()),
                session_view(2, Some("two"), running()),
                with_link(session_view(3, Some("three"), running()), jira(12)),
                session_view(4, Some("four"), running()),
                with_reply(
                    session_view(5, Some("five"), running()),
                    60,
                    "Done: the lexer is fixed.",
                ),
            ],
            vec![],
        );
        hold_prompts(&mut model, 4, &["please look at the tokenizer"]);

        let one = |text: &str| -> (Vec<SessionId>, Vec<Matched>) {
            let results = search(&model, &query(text));
            let fields = results.found().iter().flat_map(fields).collect();
            (sessions_found(&results), fields)
        };
        assert_eq!(one("PARSER"), (vec![id(1)], vec![Matched::Title]));
        assert_eq!(one("proj-2"), (vec![id(2)], vec![Matched::Cwd]));
        assert_eq!(one("abc-12"), (vec![id(3)], vec![Matched::Link(jira(12))]));
        assert_eq!(one("tokenizer"), (vec![id(4)], vec![Matched::Prompt]));
        assert_eq!(one("lexer"), (vec![id(5)], vec![Matched::Reply]));
        assert_eq!(one("nothing like this"), (vec![], vec![]));
    }

    /// The Jira key is exact: `ABC-1` is not in `ABC-12`, and the key is not a substring of a
    /// longer one in any direction.
    /// Mutation: match a Jira key by substring (ran red).
    #[test]
    fn a_jira_key_matches_its_link_exactly() {
        let model = model_of(
            vec![
                with_link(session_view(1, Some("one"), running()), jira(12)),
                with_link(session_view(2, Some("two"), running()), jira(123)),
            ],
            vec![],
        );
        let found = |text: &str| sessions_found(&search(&model, &query(text)));
        assert_eq!(found("ABC-12"), vec![id(1)]);
        assert_eq!(found("abc-12"), vec![id(1)]);
        assert_eq!(found("ABC-123"), vec![id(2)]);
        assert_eq!(found("ABC-1"), vec![]);
        assert_eq!(found("BC-12"), vec![]);
        // The whole key is the emphasised match.
        let results = search(&model, &query("abc-12"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            (String::new(), "ABC-12".to_owned(), String::new())
        );
    }

    /// A pull request is found as `owner/repo#12`, as `#12` and by a part of `owner/repo`;
    /// `#12` is not `#123`, and the repository part does not match the `#number` part.
    /// Mutation, each ran red: let the repository search reach into the number (drop the bound
    /// on the match end), or drop the whole-text branch for queries with a `#`.
    #[test]
    fn a_pull_request_matches_by_number_by_full_name_or_by_repository() {
        let model = model_of(
            vec![
                with_link(session_view(1, Some("one"), running()), pr(12)),
                with_link(session_view(2, Some("two"), running()), pr(123)),
            ],
            vec![],
        );
        let found = |text: &str| sessions_found(&search(&model, &query(text)));
        assert_eq!(found("#12"), vec![id(1)]);
        assert_eq!(found("#123"), vec![id(2)]);
        assert_eq!(found("#1"), vec![]);
        assert_eq!(found("example-org/example-repo#12"), vec![id(1)]);
        assert_eq!(found("Example-Org/Example-Repo#12"), vec![id(1)]);
        assert_eq!(found("example-org/example-repo#1"), vec![]);
        assert_eq!(found("org/example"), vec![id(1), id(2)]);
        assert_eq!(found("po#1"), vec![]);
        assert_eq!(found("12"), vec![]);
        let results = search(&model, &query("#12"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            (
                "example-org/example-repo".to_owned(),
                "#12".to_owned(),
                String::new()
            )
        );
    }

    /// A link that is a URL matches by any part of it.
    /// Mutation: search the URL with a differently folded query (ran red).
    #[test]
    fn a_url_link_matches_by_any_part() {
        let doc = Link::Doc {
            url: SafeUrl::parse("https://example.com/design/parser-notes").unwrap(),
        };
        let model = model_of(
            vec![with_link(
                session_view(1, Some("one"), running()),
                doc.clone(),
            )],
            vec![],
        );
        let results = search(&model, &query("PARSER-notes"));
        let found = results.found().first().unwrap();
        assert_eq!(fields(found), vec![Matched::Link(doc)]);
        assert_eq!(
            shown(&found.hits().first().unwrap().snippet),
            (
                "https://example.com/design/".to_owned(),
                "parser-notes".to_owned(),
                String::new()
            )
        );
    }

    /// The snippet is the match with sixty characters each side, and says when text goes on.
    /// Mutation: 59 characters of context (ran red), or the oldest matching prompt instead of
    /// the newest (ran red).
    #[test]
    fn a_snippet_is_the_match_with_sixty_characters_of_context() {
        let text = format!("{}NEEDLE{}", "é".repeat(100), "b".repeat(100));
        let mut model = model_of(vec![session_view(1, Some("one"), running())], vec![]);
        hold_prompts(&mut model, 1, &[&text, "a NEEDLE here"]);

        // The newest prompt that matches is the last one held.
        let results = search(&model, &query("needle"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            ("a ".to_owned(), "NEEDLE".to_owned(), " here".to_owned())
        );
        assert!(!hit.snippet.cut_before && !hit.snippet.cut_after);

        let mut model = model_of(vec![session_view(1, Some("one"), running())], vec![]);
        hold_prompts(&mut model, 1, &[&text]);
        let results = search(&model, &query("needle"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            ("é".repeat(60), "NEEDLE".to_owned(), "b".repeat(60))
        );
        assert!(hit.snippet.cut_before && hit.snippet.cut_after);
    }

    /// Whitespace in a snippet is a space, so a multi-line prompt is one line.
    /// Mutation: keep whitespace as it is (ran red).
    #[test]
    fn a_snippet_is_one_line() {
        let mut model = model_of(vec![session_view(1, Some("one"), running())], vec![]);
        hold_prompts(&mut model, 1, &["first line\nsecond\tline"]);
        let results = search(&model, &query("second"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            (
                "first line ".to_owned(),
                "second".to_owned(),
                " line".to_owned()
            )
        );
    }

    /// Matching and showing both use the redacted text: a credential is neither found nor
    /// shown, and the context around it is.
    /// No mutation: matching unredacted text does not compile (the search reads `Redacted`
    /// only), and each snippet part is redacted again as a new string.
    #[test]
    fn a_credential_is_neither_searchable_nor_shown() {
        let mut model = model_of(
            vec![with_reply(
                session_view(1, Some("one"), running()),
                60,
                &format!("export it: {SECRET} and then run the build"),
            )],
            vec![],
        );
        hold_prompts(&mut model, 1, &[&format!("use {SECRET} please")]);
        assert_eq!(search(&model, &query(SECRET)).matched(), 0);
        assert_eq!(search(&model, &query("ghp_aaaa")).matched(), 0);
        let results = search(&model, &query("then run"));
        let hit = results.found().first().unwrap().hits().first().unwrap();
        assert_eq!(
            shown(&hit.snippet),
            (
                "export it: [redacted] and ".to_owned(),
                "then run".to_owned(),
                " the build".to_owned()
            )
        );
    }

    /// One session matched in several fields has one `Found` with a hit per field, strongest
    /// first, and the prompt and the reply it shows are the newest that match.
    /// Mutation: show the oldest matching prompt instead of the newest (ran red).
    #[test]
    fn a_session_has_one_hit_per_matched_field_strongest_first() {
        let mut model = model_of(
            vec![with_reply(
                with_reply(
                    with_link(session_view(1, Some("parser rework"), running()), jira(7)),
                    600,
                    "old parser reply",
                ),
                60,
                "new parser reply",
            )],
            vec![],
        );
        // `with_reply` pushed oldest first; the model holds turn ends newest first.
        model.sessions.first_mut().unwrap().last_turn_ends.reverse();
        hold_prompts(&mut model, 1, &["old parser prompt", "new parser prompt"]);
        let results = search(&model, &query("parser"));
        let found = results.found().first().unwrap();
        assert_eq!(
            fields(found),
            vec![Matched::Title, Matched::Prompt, Matched::Reply]
        );
        let texts: Vec<String> = found
            .hits()
            .iter()
            .map(|hit| {
                format!(
                    "{}|{}|{}",
                    hit.snippet.before.as_str(),
                    hit.snippet.matched.as_str(),
                    hit.snippet.after.as_str()
                )
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "|parser| rework".to_owned(),
                "new |parser| prompt".to_owned(),
                "new |parser| reply".to_owned(),
            ]
        );
    }

    /// The order: strongest field, then active before dormant, then newest activity first.
    /// Mutation: swap `Link` and `Prompt` in `Strength` (ran red).
    #[test]
    fn results_rank_by_field_then_liveness_then_newest_activity() {
        let at = |mut view: SessionView, secs_before_now: i64| {
            view.last_activity = Some(ago(secs_before_now));
            view
        };
        let doc = Link::Doc {
            url: SafeUrl::parse("https://example.com/hit").unwrap(),
        };
        let mut model = model_of(
            vec![
                // 1: a reply only. 2, 3: titles, 3 the newer. 4: a link. 5: a prompt.
                at(
                    with_reply(session_view(1, Some("x1"), running()), 60, "hit"),
                    1,
                ),
                at(session_view(2, Some("hit two"), running()), 100),
                at(session_view(3, Some("hit three"), idle()), 50),
                at(with_link(session_view(4, Some("x4"), running()), doc), 10),
                at(session_view(5, Some("x5"), running()), 20),
            ],
            vec![
                // 6: a dormant title, newer than any active one. 7: a dormant reply.
                at(session_view(6, Some("dormant hit"), dormant()), 2),
                at(
                    with_reply(session_view(7, Some("x7"), dormant()), 60, "hit"),
                    3,
                ),
            ],
        );
        hold_prompts(&mut model, 5, &["a hit in a prompt"]);

        let results = search(&model, &query("hit"));
        assert_eq!(
            sessions_found(&results),
            vec![id(3), id(2), id(6), id(4), id(5), id(1), id(7)]
        );
        assert_eq!(results.matched(), 7);
        assert!(!results.truncated());
    }

    /// At most 200 sessions come back; the count says how many matched.
    /// Mutation: a bound of 201 (ran red).
    #[test]
    fn at_most_two_hundred_results_and_the_rest_are_counted() {
        let sessions: Vec<SessionView> = (1..=201)
            .map(|n| session_view(n, Some("a hit"), running()))
            .collect();
        let model = model_of(sessions, vec![]);
        let results = search(&model, &query("hit"));
        assert_eq!(results.found().len(), 200);
        assert_eq!(results.matched(), 201);
        assert!(results.truncated());
        // The model's order is kept among ties, so the first 200 are the first 200.
        assert_eq!(results.found().last().map(Found::session), Some(&id(200)));

        let few = model_of(vec![session_view(1, Some("a hit"), running())], vec![]);
        let results = search(&few, &query("hit"));
        assert_eq!((results.found().len(), results.matched()), (1, 1));
        assert!(!results.truncated());
    }
}
