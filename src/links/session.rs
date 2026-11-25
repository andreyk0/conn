//! The links one session mentions, read from its transcript summary and turn ends.
//!
//! Everything is extracted from [`Redacted`] text, so a credential in a URL is masked before
//! the URL becomes a link: the link then points at the masked URL, or at nothing.

use std::collections::HashSet;

use super::{Extractor, LinkContext, RepoRef};
use crate::transcript::{PrLink, Summary};
use crate::types::{AskUserQuestion, Identifier, Link, TurnEnd};
use crate::util::redact::Redacted;

/// The links of one session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// Everything it mentions, distinct, the pending card's text first.
    pub links: Vec<Link>,
    /// The specific identifiers in its own human prompts, distinct, oldest first.
    pub identifiers: Vec<Identifier>,
}

/// The links of a session: its pending card's text, its latest turn ends (newest first), its
/// last human prompts (newest first) and the pull requests it announced. The identifiers come
/// from the human prompts alone.
pub fn extract(extractor: &Extractor, summary: &Summary, turn_ends: &[TurnEnd]) -> Extracted {
    let ctx = LinkContext {
        repo: single_repo(&summary.pr_links),
    };
    let mut links: Vec<Link> = Vec::new();
    let mut seen: HashSet<Link> = HashSet::new();
    let mut take = |text: &Redacted<String>| {
        for found in extractor.extract(text.as_str(), &ctx).found {
            if seen.insert(found.link.clone()) {
                links.push(found.link);
            }
        }
    };
    if let Some(asked) = &summary.pending_ask {
        take(&question_text(asked));
    }
    for end in turn_ends {
        take(end.text.redacted());
    }
    let prompts: Vec<Redacted<String>> = summary
        .human_prompts
        .iter()
        .map(|prompt| Redacted::of(&prompt.text))
        .collect();
    for prompt in prompts.iter().rev() {
        take(prompt);
    }
    for announced in &summary.pr_links {
        let Some(link) = pull_request(announced) else {
            tracing::warn!(url = %announced.url, "an announced pull request has a repository that is not owner/repo");
            continue;
        };
        if seen.insert(link.clone()) {
            links.push(link);
        }
    }

    let mut identifiers: Vec<Identifier> = Vec::new();
    for prompt in &prompts {
        for found in extractor.extract(prompt.as_str(), &ctx).found {
            if let Some(id) = Identifier::of(&found.link)
                && !identifiers.contains(&id)
            {
                identifiers.push(id);
            }
        }
    }
    Extracted { links, identifiers }
}

fn question_text(asked: &AskUserQuestion) -> Redacted<String> {
    Redacted::of(
        &asked
            .questions()
            .iter()
            .map(|q| q.question.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn pull_request(announced: &PrLink) -> Option<Link> {
    let (owner, repo) = announced.repo.split_once('/')?;
    Some(Link::PullRequest {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: announced.number,
    })
}

/// The repository a bare `#123` means: the one every announced pull request is in.
fn single_repo(announced: &[PrLink]) -> Option<RepoRef> {
    let first = announced.first()?;
    let (owner, repo) = first.repo.split_once('/')?;
    announced
        .iter()
        .all(|pr| pr.repo == first.repo)
        .then(|| RepoRef {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::testutil::{asked, at};
    use crate::transcript::HumanPrompt;
    use crate::types::{AssistantText, BackgroundTasks, TurnTrigger};
    use std::path::Path;

    /// A made-up credential in the shape the redaction patterns know.
    const SECRET: &str = "ghp_aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";

    fn extractor() -> Extractor {
        let config = Config::parse(
            "[jira]\nhost = \"example.atlassian.net\"\nprefixes = [\"ABC\"]\n[slack]\nhost = \"example.slack.com\"\n",
            Path::new("/cfg.toml"),
            Path::new("/home/dev"),
        )
        .unwrap();
        Extractor::from_config(&config).unwrap()
    }

    fn urls(extracted: &Extracted) -> Vec<String> {
        extracted.links.iter().map(ToString::to_string).collect()
    }

    /// A secret inside a URL never reaches a link: in a turn end, in a human prompt and in a
    /// question, the link is the masked URL, and the text a turn end shows says `[redacted]`.
    /// Mutation: extract from the raw prompt text (the prompt row shows the token), or from a
    /// question's raw join.
    #[test]
    fn a_secret_inside_a_url_is_masked_before_it_becomes_a_link() {
        let mut summary = Summary::default();
        summary.human_prompts.push(HumanPrompt {
            at: at("2026-10-28T10:00:00Z"),
            text: format!("see https://docs.example.com/a?t={SECRET} and ABC-7"),
        });
        let mut ask = asked();
        let question = ask.questions().first().unwrap().clone();
        ask = AskUserQuestion::new(vec![crate::types::Question {
            question: Redacted::of(&format!("is https://example.com/q?k={SECRET} right?")),
            ..question
        }])
        .unwrap();
        summary.pending_ask = Some(ask);
        let end = TurnEnd {
            at: at("2026-10-28T10:01:00Z"),
            trigger: TurnTrigger::Human,
            text: AssistantText::of(&format!("done: https://example.com/e?x={SECRET}")),
            background_tasks: BackgroundTasks::Unknown,
            asked: None,
        };
        assert_eq!(
            end.text.as_str(),
            "done: https://example.com/e?x=[redacted]"
        );

        let extracted = extract(&extractor(), &summary, &[end]);
        assert_eq!(
            urls(&extracted),
            [
                "https://example.com/q?k=[redacted]",
                "https://example.com/e?x=[redacted]",
                "https://docs.example.com/a?t=[redacted]",
                "ABC-7",
            ]
        );
        assert!(!format!("{extracted:?}").contains("ghp_"));
    }
}
