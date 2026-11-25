//! Things a card can point at, and the witnesses that make them safe to render as links.

use std::fmt;
use std::num::NonZeroU32;

use serde::Deserialize;

/// The host the GitHub links point at; GitHub.com is the only forge supported.
const GITHUB_HOST: &str = "github.com";

/// A URL that may be used as an `href`.
///
/// Witnesses that the text is an `https://` URL, or an `http://` URL to a local host (see
/// [`is_local_host`]), with a non-empty authority, no userinfo (`https://good.example@evil.example`
/// shows one host and goes to another) and no whitespace, control or backslash characters.
/// Third-party text becomes a link only by passing [`SafeUrl::parse`]; `javascript:` and
/// `data:` URLs cannot, and nor can `http://` to a host that is not local.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SafeUrl(String);

/// The text is not acceptable as an `href`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a safe https URL: {0:?}")]
pub struct SafeUrlError(String);

impl SafeUrl {
    pub fn parse(text: &str) -> Result<Self, SafeUrlError> {
        let refuse = || SafeUrlError(text.to_owned());
        let (rest, secure) = match (text.strip_prefix("https://"), text.strip_prefix("http://")) {
            (Some(rest), _) => (rest, true),
            (None, Some(rest)) => (rest, false),
            (None, None) => return Err(refuse()),
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let clean = !authority.is_empty()
            && (secure || is_local_host(authority))
            && !authority.contains('@')
            && !text
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\\');
        if clean {
            Ok(Self(text.to_owned()))
        } else {
            Err(refuse())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether an authority (`host` or `host:port`, no userinfo) names this machine: `localhost`,
/// a name under `.localhost` or `.test`, a `127.x.x.x` address or `[::1]`. Plain `http://` is
/// safe to follow only there, because nothing crosses a network.
fn is_local_host(authority: &str) -> bool {
    let named_host = |host: &str| {
        let host = host.to_ascii_lowercase();
        host == "localhost"
            || host
                .rsplit_once('.')
                .is_some_and(|(_, last)| last == "localhost" || last == "test")
            || host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|address| address.is_loopback())
    };
    let port_ok = |port: Option<&str>| {
        port.is_none_or(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
    };
    if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split_once(']').is_some_and(|(address, tail)| {
            let port = if tail.is_empty() {
                Some(None)
            } else {
                tail.strip_prefix(':').map(Some)
            };
            address == "::1" && port.is_some_and(port_ok)
        })
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        named_host(host) && port_ok(port)
    }
}

impl fmt::Display for SafeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bare hostname from configuration, lowercased: no scheme, path or port.
///
/// Witnesses that interpolating it after `https://` yields exactly that host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Host(String);

/// The text is not a bare hostname.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a bare hostname (letters, digits, '-' and '.' only, no scheme or path): {0:?}")]
pub struct HostError(String);

impl Host {
    pub fn parse(text: &str) -> Result<Self, HostError> {
        let edge = |c: char| c == '.' || c == '-';
        let valid = !text.is_empty()
            && text.chars().all(|c| c.is_ascii_alphanumeric() || edge(c))
            && !text.starts_with(edge)
            && !text.ends_with(edge)
            && !text.contains("..");
        if valid {
            Ok(Self(text.to_ascii_lowercase()))
        } else {
            Err(HostError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Host {
    type Error = HostError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

/// A configured Jira project prefix such as `ABC`.
///
/// Witnesses the shape of a Jira project key: an uppercase letter, then uppercase letters,
/// digits or `_`, 2 to 10 characters in all.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct JiraPrefix(String);

/// The text is not a Jira project prefix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a Jira project prefix (2-10 chars: A-Z first, then A-Z, 0-9 or '_'): {0:?}")]
pub struct JiraPrefixError(String);

impl JiraPrefix {
    const LEN: std::ops::RangeInclusive<usize> = 2..=10;

    pub fn parse(text: &str) -> Result<Self, JiraPrefixError> {
        let mut chars = text.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_uppercase());
        let rest_ok = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if first_ok && rest_ok && Self::LEN.contains(&text.len()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(JiraPrefixError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for JiraPrefix {
    type Error = JiraPrefixError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

/// A Jira issue key such as `ABC-123`.
///
/// Witnesses the shape of a key: a [`JiraPrefix`] and a positive number, so `ABC-0` and `abc-1`
/// are not keys. Whether the prefix is one the user configured is decided where keys are
/// found in text (the extractor), not here. Written to the ledger as `ABC-123`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JiraKey {
    prefix: JiraPrefix,
    number: NonZeroU32,
}

impl JiraKey {
    pub const fn new(prefix: JiraPrefix, number: NonZeroU32) -> Self {
        Self { prefix, number }
    }

    /// `ABC-123` as written: a prefix, a hyphen, digits, no other text. `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let (prefix, number) = text.rsplit_once('-')?;
        let prefix = JiraPrefix::parse(prefix).ok()?;
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(Self::new(prefix, number.parse().ok()?))
    }

    pub const fn prefix(&self) -> &JiraPrefix {
        &self.prefix
    }

    pub const fn number(&self) -> NonZeroU32 {
        self.number
    }
}

impl fmt::Display for JiraKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.prefix.as_str(), self.number)
    }
}

impl TryFrom<String> for JiraKey {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or_else(|| format!("{text:?} is not a ticket key such as ABC-123"))
    }
}

impl From<JiraKey> for String {
    fn from(key: JiraKey) -> Self {
        key.to_string()
    }
}

/// An artefact mentioned in a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Link {
    Jira {
        key: JiraKey,
    },
    /// A Slack message; `ts` is the message timestamp in Slack's `1700000000.123456` form.
    SlackThread {
        channel: String,
        ts: String,
    },
    PullRequest {
        owner: String,
        repo: String,
        number: u32,
    },
    /// A document the human is likely to read (design doc, wiki page, spec).
    Doc {
        url: SafeUrl,
    },
    Other {
        url: SafeUrl,
    },
}

/// The short form a person recognises: `ABC-123`, `owner/repo#42`, `slack:C123/1700000000.000100`.
impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jira { key } => write!(f, "{key}"),
            Self::SlackThread { channel, ts } => write!(f, "slack:{channel}/{ts}"),
            Self::PullRequest {
                owner,
                repo,
                number,
            } => write!(f, "{owner}/{repo}#{number}"),
            Self::Doc { url } | Self::Other { url } => write!(f, "{url}"),
        }
    }
}

/// The text is none of the short forms [`Link`] displays as.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "not a link: {0:?} (expected ABC-123, owner/repo#42, slack:CHANNEL/1700000000.000100 or an https URL, or an http URL to a local host)"
)]
pub struct LinkParseError(String);

impl std::str::FromStr for Link {
    type Err = LinkParseError;

    /// The inverse of [`fmt::Display`], needing no configuration. A URL that is not one of the
    /// short forms is [`Link::Other`]: which URLs count as documents is configuration's
    /// to say, so a document link reads back as `Other`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let refuse = || LinkParseError(text.to_owned());
        if text.starts_with("https://") || text.starts_with("http://") {
            return Ok(Self::Other {
                url: SafeUrl::parse(text).map_err(|_| refuse())?,
            });
        }
        if let Some(rest) = text.strip_prefix("slack:") {
            let (channel, ts) = rest.split_once('/').ok_or_else(refuse)?;
            let channel_ok = !channel.is_empty()
                && channel
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            let ts_ok = ts.split_once('.').is_some_and(|(seconds, micros)| {
                !seconds.is_empty()
                    && micros.len() == SLACK_MICRO_DIGITS
                    && seconds
                        .bytes()
                        .chain(micros.bytes())
                        .all(|b| b.is_ascii_digit())
            });
            return if channel_ok && ts_ok {
                Ok(Self::SlackThread {
                    channel: channel.to_owned(),
                    ts: ts.to_owned(),
                })
            } else {
                Err(refuse())
            };
        }
        if let Some((repository, number)) = text.split_once('#') {
            let (owner, repo) = repository.split_once('/').ok_or_else(refuse)?;
            let owner_ok = owner.starts_with(|c: char| c.is_ascii_alphanumeric())
                && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            let repo_ok = !repo.is_empty()
                && repo
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
            let number = number.parse::<NonZeroU32>().map_err(|_| refuse())?;
            return if owner_ok && repo_ok {
                Ok(Self::PullRequest {
                    owner: owner.to_owned(),
                    repo: repo.to_owned(),
                    number: number.get(),
                })
            } else {
                Err(refuse())
            };
        }
        let (prefix, number) = text.rsplit_once('-').ok_or_else(refuse)?;
        Ok(Self::Jira {
            key: JiraKey::new(
                JiraPrefix::parse(prefix).map_err(|_| refuse())?,
                number.parse().map_err(|_| refuse())?,
            ),
        })
    }
}

/// A Slack timestamp's fractional part is six digits.
const SLACK_MICRO_DIGITS: usize = 6;

/// The hosts that links to the configured services are built against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTargets {
    /// `None` where there is no Jira.
    pub jira: Option<Host>,
    /// `None` where there is no Slack.
    pub slack: Option<Host>,
}

/// A link could not be turned into a URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkRenderError {
    #[error("link parts do not form a safe URL: {0}")]
    Url(#[from] SafeUrlError),
    /// A stored Jira key, read back where no Jira is configured.
    #[error("no Jira host is configured for {0}")]
    NoJira(JiraKey),
    /// A stored Slack thread, read back where no Slack is configured.
    #[error("no Slack host is configured for slack:{channel}/{ts}")]
    NoSlack { channel: String, ts: String },
}

impl Link {
    /// Where the link goes. The only way to obtain an `href` for a link.
    pub fn url(&self, targets: &LinkTargets) -> Result<SafeUrl, LinkRenderError> {
        let built = match self {
            Self::Jira { key } => match &targets.jira {
                Some(host) => format!("https://{}/browse/{key}", host.as_str()),
                None => return Err(LinkRenderError::NoJira(key.clone())),
            },
            Self::SlackThread { channel, ts } => match &targets.slack {
                Some(host) => format!(
                    "https://{}/archives/{channel}/p{}",
                    host.as_str(),
                    ts.replace('.', "")
                ),
                None => {
                    return Err(LinkRenderError::NoSlack {
                        channel: channel.clone(),
                        ts: ts.clone(),
                    });
                }
            },
            Self::PullRequest {
                owner,
                repo,
                number,
            } => {
                format!("https://{GITHUB_HOST}/{owner}/{repo}/pull/{number}")
            }
            Self::Doc { url } | Self::Other { url } => return Ok(url.clone()),
        };
        Ok(SafeUrl::parse(&built)?)
    }
}

/// A link that names one thing: a pull request, a Slack thread, a Jira issue.
///
/// Witnesses that the link is specific enough to be evidence; documents and other URLs are
/// not, because many things link them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identifier(Link);

impl Identifier {
    pub fn of(link: &Link) -> Option<Self> {
        match link {
            Link::Jira { .. } | Link::SlackThread { .. } | Link::PullRequest { .. } => {
                Some(Self(link.clone()))
            }
            Link::Doc { .. } | Link::Other { .. } => None,
        }
    }

    pub const fn link(&self) -> &Link {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> LinkTargets {
        LinkTargets {
            jira: Some(Host::parse("example.atlassian.net").unwrap()),
            slack: Some(Host::parse("example.slack.com").unwrap()),
        }
    }

    #[test]
    fn safe_url_accepts_https() {
        for ok in [
            "https://example.com",
            "https://example.com/a?b=c#d",
            "https://example.com:8443/x",
        ] {
            assert_eq!(SafeUrl::parse(ok).unwrap().as_str(), ok);
        }
    }

    /// Mutation: drop the `.test` suffix rule (the `app.test` row goes red; ran red).
    #[test]
    fn safe_url_accepts_http_to_a_local_host_only() {
        for ok in [
            "http://localhost:5173/x",
            "http://127.0.0.1:8082/graphql",
            "http://[::1]:3000/",
            "http://app.test/",
            "http://dev.localhost/",
            "http://localhost",
            "http://127.1.2.3/",
        ] {
            assert_eq!(SafeUrl::parse(ok).unwrap().as_str(), ok);
        }
    }

    /// Mutation: accept every `http://` (`example.com` row; ran red), match `localhost` by
    /// prefix (the `localhost.evil.com` row; ran red), accept any `[...]` address (the `[::2]`
    /// row; ran red) or any IPv4 address instead of loopback (the `10.0.0.1` row).
    #[test]
    fn safe_url_refuses_http_to_any_other_host() {
        for bad in [
            "http://example.com/",
            "http://127.0.0.1.evil.com/",
            "http://localhost.evil.com/",
            "http://user@localhost/",
            "http://localhost@evil.example/",
            "http://localhost:80@evil.example/",
            "http://[::1]x/",
            "http://localhost:port/",
            "http://localhost:/",
            "http://[::2]/",
            "http://10.0.0.1/",
            "http://128.0.0.1/",
            "http://test/",
            "http://localhost/a b",
            "ftp://localhost/",
        ] {
            assert!(SafeUrl::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn safe_url_refuses_everything_else() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<b>x</b>",
            "//example.com",
            "https://",
            "https:///path",
            "https://?x=1",
            "https://good.example@evil.example/",
            "https://example.com/a b",
            "https://example.com/\n",
            "https://example.com\\@evil.example",
            "https://example.com\\evil",
            "",
        ] {
            assert!(SafeUrl::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn host_accepts_bare_hostnames_lowercased() {
        assert_eq!(
            Host::parse("Example.Atlassian.NET").unwrap().as_str(),
            "example.atlassian.net"
        );
    }

    #[test]
    fn host_refuses_schemes_paths_ports_and_dots_at_the_edge() {
        for bad in [
            "",
            "https://example.com",
            "example.com/x",
            "example.com:80",
            ".example.com",
            "example.com.",
            "a..b",
            "-a.com",
        ] {
            assert!(Host::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn jira_prefix_accepts_project_key_shapes() {
        for ok in ["AB", "ABC", "ABC1", "A_B", "ABCDEFGHIJ"] {
            assert_eq!(JiraPrefix::parse(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn jira_prefix_refuses_other_shapes() {
        for bad in [
            "",
            "A",
            "abc",
            "aBC",
            "Abc",
            "1AB",
            "AB-",
            "AB CD",
            "ABCDEFGHIJK",
        ] {
            assert!(JiraPrefix::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn jira_key_displays_as_prefix_dash_number() {
        let key = JiraKey::new(
            JiraPrefix::parse("ABC").unwrap(),
            NonZeroU32::new(123).unwrap(),
        );
        assert_eq!(key.to_string(), "ABC-123");
    }

    #[test]
    fn links_render_against_configured_hosts() {
        let jira = Link::Jira {
            key: JiraKey::new(
                JiraPrefix::parse("ABC").unwrap(),
                NonZeroU32::new(7).unwrap(),
            ),
        };
        assert_eq!(
            jira.url(&targets()).unwrap().as_str(),
            "https://example.atlassian.net/browse/ABC-7"
        );
        let no_jira = LinkTargets {
            jira: None,
            ..targets()
        };
        assert!(matches!(
            jira.url(&no_jira),
            Err(LinkRenderError::NoJira(_))
        ));

        let slack = Link::SlackThread {
            channel: "C0123ABCD".into(),
            ts: "1700000000.123456".into(),
        };
        assert_eq!(
            slack.url(&targets()).unwrap().as_str(),
            "https://example.slack.com/archives/C0123ABCD/p1700000000123456"
        );
        let no_slack = LinkTargets {
            slack: None,
            ..targets()
        };
        assert!(matches!(
            slack.url(&no_slack),
            Err(LinkRenderError::NoSlack { .. })
        ));

        let pr = Link::PullRequest {
            owner: "octo".into(),
            repo: "widgets".into(),
            number: 42,
        };
        assert_eq!(
            pr.url(&targets()).unwrap().as_str(),
            "https://github.com/octo/widgets/pull/42"
        );

        let url = SafeUrl::parse("https://docs.example.com/spec").unwrap();
        assert_eq!(Link::Doc { url: url.clone() }.url(&targets()).unwrap(), url);
    }

    /// The short form is written out for each variant and read back to the same link, with no
    /// configuration.
    /// Mutation: drop the `SLACK_MICRO_DIGITS` check, accept `#0`, or let `Doc` read back as
    /// something other than `Other`.
    #[test]
    fn the_short_form_reads_back() {
        let url = SafeUrl::parse("https://docs.example.com/spec").unwrap();
        let table = [
            (
                "ABC-123",
                Link::Jira {
                    key: JiraKey::new(
                        JiraPrefix::parse("ABC").unwrap(),
                        NonZeroU32::new(123).unwrap(),
                    ),
                },
            ),
            (
                "A_1-7",
                Link::Jira {
                    key: JiraKey::new(
                        JiraPrefix::parse("A_1").unwrap(),
                        NonZeroU32::new(7).unwrap(),
                    ),
                },
            ),
            (
                "example-org/example.repo#42",
                Link::PullRequest {
                    owner: "example-org".into(),
                    repo: "example.repo".into(),
                    number: 42,
                },
            ),
            (
                "slack:C0123ABCD/1700000000.000100",
                Link::SlackThread {
                    channel: "C0123ABCD".into(),
                    ts: "1700000000.000100".into(),
                },
            ),
            (
                "https://docs.example.com/spec",
                Link::Other { url: url.clone() },
            ),
        ];
        for (text, link) in table {
            assert_eq!(text.parse::<Link>().as_ref(), Ok(&link), "{text}");
            assert_eq!(link.to_string(), text);
        }
        assert_eq!(
            Link::Doc { url }
                .to_string()
                .parse::<Link>()
                .unwrap()
                .to_string(),
            "https://docs.example.com/spec"
        );
    }

    #[test]
    fn text_that_is_no_short_form_is_refused() {
        for bad in [
            "",
            "ABC",
            "ABC-0",
            "abc-1",
            "ABC-1x",
            "A-1",
            "owner/repo#0",
            "owner/repo#",
            "owner#4",
            "/repo#4",
            "-o/repo#4",
            "slack:c1/1700000000.000100",
            "slack:C1/1700000000.0001",
            "slack:C1/1700000000",
            "slack:/1700000000.000100",
            "http://example.com",
            "javascript:alert(1)",
        ] {
            assert!(bad.parse::<Link>().is_err(), "accepted {bad:?}");
        }
    }

    /// Mutation: add `Link::Doc` or `Link::Other` to the arm that makes an identifier (the
    /// matching row goes red), or drop `Link::SlackThread` from it.
    #[test]
    fn only_links_that_name_one_thing_are_identifiers() {
        let url = SafeUrl::parse("https://example.com/design").unwrap();
        let named = [
            "ABC-123".parse::<Link>().unwrap(),
            "slack:C123/1700000000.000100".parse::<Link>().unwrap(),
            "owner/repo#42".parse::<Link>().unwrap(),
        ];
        for link in &named {
            assert_eq!(
                Identifier::of(link).as_ref().map(Identifier::link),
                Some(link)
            );
        }
        for link in [Link::Doc { url: url.clone() }, Link::Other { url }] {
            assert_eq!(Identifier::of(&link), None, "{link}");
        }
    }

    #[test]
    fn a_link_whose_parts_break_the_url_is_refused() {
        let pr = Link::PullRequest {
            owner: "a b".into(),
            repo: "r".into(),
            number: 1,
        };
        assert!(pr.url(&targets()).is_err());
    }
}
