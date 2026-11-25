//! Secrets taken out of text before a page shows it.
//!
//! Best effort: the patterns name the credential shapes that turn up in terminal sessions, and
//! text that carries a secret in another shape passes through. [`Redacted`] is the only form
//! in which third-party text reaches a template, so a secret of a known shape cannot be
//! shown by a page that forgot to ask.

use std::sync::LazyLock;

use regex::{Regex, RegexSet};

/// What replaces a match.
pub const MASK: &str = "[redacted]";

/// The credential shapes, each as a regular expression.
///
/// `\b` before `sk-` keeps hyphenated words such as `task-notification-handler-name` from
/// reading as a key.
const PATTERNS: [&str; 8] = [
    // An AWS access key id.
    r"AKIA[0-9A-Z]{16}",
    // A GitHub token: personal, OAuth, user-to-server, server-to-server, refresh.
    r"gh[pousr]_[A-Za-z0-9]{36,}",
    // An API secret key in the `sk-` style.
    r"\bsk-[A-Za-z0-9_-]{20,}",
    // A Slack token.
    r"xox[abprs]-[A-Za-z0-9-]{10,}",
    // An HTTP bearer credential.
    r"Bearer\s+[A-Za-z0-9._~+/=-]{20,}",
    // A JSON web token: header, payload and signature.
    r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    // A PEM private key; a block whose end is missing runs to the end of the text.
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
    // An environment assignment whose name says it is a secret.
    r"[A-Z_]{3,}(?:KEY|TOKEN|SECRET|PASSWORD)=\S+",
];

/// The compiled patterns: the set answers "does anything match" in one pass, and the
/// individual expressions give the spans a set cannot.
struct Redactor {
    set: RegexSet,
    each: Vec<Regex>,
}

impl Redactor {
    fn build() -> Result<Self, regex::Error> {
        Ok(Self {
            set: RegexSet::new(PATTERNS)?,
            each: PATTERNS
                .iter()
                .map(|pattern| Regex::new(pattern))
                .collect::<Result<_, _>>()?,
        })
    }

    fn redact(&self, text: &str) -> String {
        let matching = self.set.matches(text);
        if !matching.matched_any() {
            return text.to_owned();
        }
        let mut out = text.to_owned();
        for index in &matching {
            if let Some(pattern) = self.each.get(index) {
                out = pattern.replace_all(&out, MASK).into_owned();
            }
        }
        out
    }
}

/// `Err` only if a constant pattern fails to compile, which the tests rule out; the text is
/// then masked whole rather than passed through unchecked.
static REDACTOR: LazyLock<Result<Redactor, regex::Error>> = LazyLock::new(Redactor::build);

/// Text that has been through the redaction patterns.
///
/// Witnesses that no credential of a known shape is left in the text. The only constructor is
/// [`Redacted::of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted<T>(T);

impl Redacted<String> {
    pub fn of(text: &str) -> Self {
        Self(match &*REDACTOR {
            Ok(redactor) => redactor.redact(text),
            Err(error) => {
                tracing::error!(%error, "the redaction patterns do not compile; text is masked whole");
                MASK.to_owned()
            }
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first `max` characters. A prefix of redacted text holds no credential the whole did
    /// not, so the result is as redacted as the text it was cut from; the cut after the
    /// redaction (never before) is what keeps a token that straddles it from escaping.
    #[must_use]
    pub fn prefix(&self, max: usize) -> Self {
        Self(self.0.chars().take(max).collect())
    }
}

#[cfg(test)]
mod tests {
    //! The table is hand-written: each input holds one credential shape, or a near miss, and
    //! the output is what a reader of the page should see. The secrets are made-up strings of
    //! the right shape.

    use super::*;

    fn shown(text: &str) -> String {
        Redacted::of(text).as_str().to_owned()
    }

    /// Mutation: drop any one pattern from `PATTERNS` (its row goes red), or replace
    /// `replace_all` by `replace` (the two-token row keeps the second).
    #[test]
    fn each_credential_shape_is_masked_and_the_text_around_it_is_not() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r";
        let rows: Vec<(String, &str)> = vec![
            (
                "key AKIAABCDEFGHIJKLMNOP in use".to_owned(),
                "key [redacted] in use",
            ),
            (format!("token {gh}."), "token [redacted]."),
            (
                "use sk-abcdefghijklmnopqrstuvwx now".to_owned(),
                "use [redacted] now",
            ),
            (
                "slack xoxb-1234567890-abcdefghij ok".to_owned(),
                "slack [redacted] ok",
            ),
            (
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123".to_owned(),
                "Authorization: [redacted]",
            ),
            (format!("jwt {jwt} end"), "jwt [redacted] end"),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nIBAAKC\n-----END RSA PRIVATE KEY-----\nafter"
                    .to_owned(),
                "[redacted]\nafter",
            ),
            (
                "-----BEGIN PRIVATE KEY-----\nMIIEow\ncut off".to_owned(),
                "[redacted]",
            ),
            (
                "export AWS_SECRET_ACCESS_KEY=abc/def+ghi then run".to_owned(),
                "export [redacted] then run",
            ),
            (
                "DB_PASSWORD=hunter2 API_TOKEN=xyz".to_owned(),
                "[redacted] [redacted]",
            ),
        ];
        for (input, expected) in rows {
            assert_eq!(shown(&input), expected, "{input:?}");
        }
    }

    /// Mutation: drop the `\b` before `sk-` (the hyphenated word row goes red), or shorten a
    /// length floor (a short-token row goes red).
    #[test]
    fn near_misses_pass_through_unchanged() {
        for text in [
            "plain prose with ABC-123 and https://example.atlassian.net/browse/ABC-123",
            "the task-notification-handler-for-everything is fine",
            "sk-short",
            "ghp_tooshort",
            "Bearer short",
            "KEY=value and TOKEN=x (names under three letters are not secrets)",
            "xoxb-short",
            "",
        ] {
            assert_eq!(shown(text), text);
        }
    }

    /// A token that straddles the cut is masked whole: 175 characters, then a 40-character
    /// token, cut at 200 would leave 25 characters of it if the cut came first.
    /// Mutation: take the characters of `text` before redacting it, then redact the cut.
    #[test]
    fn a_prefix_of_redacted_text_never_shows_part_of_a_token() {
        let token = format!("ghp_{}", "a1B2".repeat(9));
        let text = format!("{} {token} tail", "x".repeat(174));
        let cut = Redacted::of(&text).prefix(200);
        assert_eq!(cut.as_str(), format!("{} [redacted] tail", "x".repeat(174)));
        let shorter = Redacted::of(&text).prefix(180);
        assert_eq!(shorter.as_str(), format!("{} [reda", "x".repeat(174)));
    }

    /// A pattern that cannot compile would mask every page whole, so this is the one test that
    /// proves the others exercise the real patterns.
    #[test]
    fn every_pattern_compiles() {
        assert!(Redactor::build().is_ok());
    }
}
