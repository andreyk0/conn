//! The plan's rate-limit windows: how much of the five-hour and weekly allowances is used, and
//! when each resets.
//!
//! Read from Anthropic's OAuth usage endpoint with the access token Claude Code keeps in its
//! credentials file, through `curl`. The token is written to `curl`'s stdin, never to its
//! arguments, so it is not in the process table. An expired token is left for Claude Code to
//! renew: refreshing it here would spend the single-use refresh token a running session relies
//! on.
//!
//! `curl` prints the HTTP status and the `Retry-After` header after the body, so a refusal as
//! too frequent (429) is told apart from every other failure and slows the reads down: see
//! [`Reading::is_due`].
//!
//! "Could not tell" and "told" stay apart: every [`UsageError`] is a failure to find out, and
//! [`Reading`] keeps the last windows that were read beside the latest attempt, so a failed
//! refresh shows the older numbers marked with their age rather than none.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::SignedDuration;
use serde::Deserialize;

use crate::model::SourceState;
use crate::types::Instant;
use crate::util::runner::{CommandRunner, ToolError, run_tool_with_input};

/// Claude Code's credentials file, in its own directory.
pub const CREDENTIALS_FILE: &str = ".credentials.json";

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// `curl` gives up after this many seconds. The endpoint answers in well under a second; a
/// read is never what a page waits on, so a slow link costs one late refresh.
const CURL_MAX_TIME_SECS: &str = "8";
/// The run's own deadline, past `curl`'s, so `curl` reports its timeout itself.
const CURL_TIMEOUT: Duration = Duration::from_secs(10);

/// What `curl` prints after the body: a line with the HTTP status and the `Retry-After`
/// header, empty when the header is absent. `%header{}` needs curl 7.84 or later.
const CURL_WRITE_OUT: &str = "\n%{http_code} %header{retry-after}";

/// How often the windows are read while a page is watched. The five-hour window moves by a
/// percent in minutes of heavy use, so a minute is as fresh as the numbers are worth. A failed
/// read is retried at the same pace, unless it was refused as too frequent.
pub const READ_EVERY: SignedDuration = SignedDuration::from_mins(1);

/// The longest the reads' own backoff grows to: each refusal in a row doubles the gap from
/// [`READ_EVERY`], so the fourth and later refusals wait this long.
const BACKOFF_GAP_MAX: SignedDuration = SignedDuration::from_mins(16);

/// The longest a refusal holds the reads back, whatever `Retry-After` asks: a wrong header
/// must not freeze the gauges for a day. An hour is how long a page view keeps the reads going.
const REFUSED_WAIT_MAX: SignedDuration = SignedDuration::from_hours(1);

/// The most of a `Retry-After` value kept when it is not a wait: enough to recognise it.
const RETRY_AFTER_CHARS_MAX: usize = 64;

/// A rate-limit window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    FiveHours,
    Week,
    /// A weekly allowance of its own for one model, by the model's display name.
    WeekOf(String),
}

impl Span {
    /// How its reset is worth showing: a five-hour window resets within the working day, so a
    /// countdown is what one acts on; a weekly one lands on a day that needs naming.
    pub const fn resets_within_the_day(&self) -> bool {
        match self {
            Self::FiveHours => true,
            Self::Week | Self::WeekOf(_) => false,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FiveHours => f.write_str("5h"),
            Self::Week => f.write_str("7d"),
            Self::WeekOf(model) => write!(f, "7d {model}"),
        }
    }
}

/// How much of one window is used. A window whose use is not reported is left out rather than
/// read as zero: "not known" and "nothing used" must not look alike.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub span: Span,
    /// Percent of the allowance used; above 100 once it is exceeded.
    pub used_percent: f64,
    pub resets_at: Option<Instant>,
}

/// Why the windows could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    #[error("Claude Code is not signed in to a plan ({path} holds no sign-in)")]
    NotSignedIn { path: PathBuf },
    #[error("Claude Code's sign-in has expired; the next session to run renews it")]
    Expired,
    #[error("cannot read {path}: {reason}")]
    Credentials { path: PathBuf, reason: String },
    #[error(transparent)]
    Tool(#[from] ToolError),
    /// HTTP 429: the endpoint read the request and turned it away as too frequent.
    #[error("the usage endpoint turned the read away as too frequent (HTTP 429); {retry_after}")]
    Refused { retry_after: RetryAfter },
    #[error("the usage endpoint answered HTTP {0}")]
    Status(u16),
    #[error("the usage endpoint answered with something unexpected: {0}")]
    Malformed(String),
}

/// What a refusal's `Retry-After` header asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryAfter {
    NotSaid,
    /// From the moment of the answer; a date already past is no wait.
    Wait(SignedDuration),
    /// The header's value, cut to [`RETRY_AFTER_CHARS_MAX`], when it is neither seconds nor a date.
    Unreadable(String),
}

impl RetryAfter {
    /// A header value, as delay seconds or an HTTP date (RFC 9110), as of `now`.
    fn parse(value: &str, now: Instant) -> Self {
        let value = value.trim();
        if value.is_empty() {
            return Self::NotSaid;
        }
        if let Ok(secs) = value.parse::<u32>() {
            return Self::Wait(SignedDuration::from_secs(i64::from(secs)));
        }
        match jiff::fmt::rfc2822::DateTimeParser::new().parse_timestamp(value) {
            Ok(at) => Self::Wait(at.duration_since(now).max(SignedDuration::ZERO)),
            Err(_) => Self::Unreadable(value.chars().take(RETRY_AFTER_CHARS_MAX).collect()),
        }
    }

    const fn wait(&self) -> Option<SignedDuration> {
        match self {
            Self::Wait(wait) => Some(*wait),
            Self::NotSaid | Self::Unreadable(_) => None,
        }
    }
}

impl fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSaid => f.write_str("it named no wait"),
            Self::Wait(wait) => write!(f, "it asked for a wait of {} s", wait.as_secs()),
            Self::Unreadable(value) => write!(f, "its Retry-After is not a wait: {value}"),
        }
    }
}

/// The OAuth access token. Its text leaves this module only on `curl`'s stdin.
struct AccessToken(String);

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(..)")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialsFile {
    claude_ai_oauth: Option<OAuth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuth {
    access_token: String,
    /// Unix epoch milliseconds.
    expires_at: i64,
}

/// The access token in the credentials file at `path`, when it is still valid at `now`.
fn read_token(path: &Path, now: Instant) -> Result<AccessToken, UsageError> {
    let unreadable = |reason: String| UsageError::Credentials {
        path: path.to_owned(),
        reason,
    };
    let text = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            UsageError::NotSignedIn {
                path: path.to_owned(),
            }
        } else {
            unreadable(error.to_string())
        }
    })?;
    // The parse error is not shown: serde quotes the text it stopped at, which may be a token.
    let file: CredentialsFile = serde_json::from_str(&text)
        .map_err(|_error| unreadable("it is not the expected JSON".to_owned()))?;
    let Some(oauth) = file.claude_ai_oauth.filter(|o| !o.access_token.is_empty()) else {
        return Err(UsageError::NotSignedIn {
            path: path.to_owned(),
        });
    };
    let expires_at = Instant::from_millisecond(oauth.expires_at)
        .map_err(|error| unreadable(format!("its expiry is not a time: {error}")))?;
    if expires_at <= now {
        return Err(UsageError::Expired);
    }
    Ok(AccessToken(oauth.access_token))
}

/// Reads the windows now, with the sign-in in `credentials` as of `now`.
pub async fn fetch(
    runner: &dyn CommandRunner,
    credentials: &Path,
    now: Instant,
) -> Result<Vec<Window>, UsageError> {
    let token = read_token(credentials, now)?;
    let headers = format!(
        "Authorization: Bearer {}\nanthropic-beta: oauth-2025-04-20\n",
        token.0
    );
    // No `--fail`: an HTTP error status is an answer, read from the line `--write-out` adds;
    // a non-zero exit is a transfer that did not happen.
    let args = [
        "--silent",
        "--show-error",
        "--max-time",
        CURL_MAX_TIME_SECS,
        "--header",
        "@-",
        "--write-out",
        CURL_WRITE_OUT,
        USAGE_URL,
    ];
    let out = run_tool_with_input(runner, "curl", &args, headers.as_bytes(), CURL_TIMEOUT).await?;
    let answer = Answer::of(&out.stdout)?;
    match answer.status {
        200 => parse(answer.body),
        429 => Err(UsageError::Refused {
            retry_after: RetryAfter::parse(answer.retry_after, now),
        }),
        status => Err(UsageError::Status(status)),
    }
}

/// What `curl` printed: the body, then the line [`CURL_WRITE_OUT`] asks for.
struct Answer<'a> {
    body: &'a [u8],
    status: u16,
    retry_after: &'a str,
}

impl<'a> Answer<'a> {
    fn of(stdout: &'a [u8]) -> Result<Self, ToolError> {
        let unparseable = |reason: &str| ToolError::Unparseable {
            program: "curl".to_owned(),
            reason: reason.to_owned(),
        };
        let no_status = || unparseable("no status line after the body");
        let newline = stdout
            .iter()
            .rposition(|b| *b == b'\n')
            .ok_or_else(no_status)?;
        let (body, line) = stdout.split_at_checked(newline).ok_or_else(no_status)?;
        let line = line.strip_prefix(b"\n").ok_or_else(no_status)?;
        let line = std::str::from_utf8(line)
            .map_err(|_error| unparseable("a status line not in UTF-8"))?;
        let (status, retry_after) = line.split_once(' ').ok_or_else(no_status)?;
        let status = status.parse().map_err(|_error| {
            unparseable(&format!("an HTTP status that is not a number: {status}"))
        })?;
        Ok(Self {
            body,
            status,
            retry_after,
        })
    }
}

/// The endpoint's body. Beyond the fixed fields it lists weekly allowances scoped to one
/// model under `limits`; those are read too.
#[derive(Deserialize)]
struct Body {
    five_hour: Option<Fixed>,
    seven_day: Option<Fixed>,
    seven_day_opus: Option<Fixed>,
    seven_day_sonnet: Option<Fixed>,
    #[serde(default)]
    limits: Vec<Limit>,
}

#[derive(Deserialize)]
struct Fixed {
    utilization: Option<f64>,
    resets_at: Option<Instant>,
}

#[derive(Deserialize)]
struct Limit {
    kind: String,
    percent: Option<f64>,
    resets_at: Option<Instant>,
    scope: Option<Scope>,
}

#[derive(Deserialize)]
struct Scope {
    model: Option<ScopeModel>,
}

#[derive(Deserialize)]
struct ScopeModel {
    display_name: Option<String>,
}

/// The models whose weekly allowance has a fixed field (`seven_day_opus`, `seven_day_sonnet`);
/// a scoped entry for one of them is the same window and is left out, so none is shown twice.
const FIXED_MODELS: [&str; 2] = ["Opus", "Sonnet"];

/// The windows in an endpoint body: five hours, the week, then each model's week.
pub fn parse(body: &[u8]) -> Result<Vec<Window>, UsageError> {
    let body: Body =
        serde_json::from_slice(body).map_err(|error| UsageError::Malformed(error.to_string()))?;
    let fixed = |span: Span, field: Option<&Fixed>| {
        field.and_then(|f| {
            f.utilization.map(|used_percent| Window {
                span,
                used_percent,
                resets_at: f.resets_at,
            })
        })
    };
    let scoped = body
        .limits
        .iter()
        .filter(|limit| limit.kind == "weekly_scoped");
    let scoped = scoped.filter_map(|limit| {
        let model = limit
            .scope
            .as_ref()?
            .model
            .as_ref()?
            .display_name
            .as_ref()?;
        if FIXED_MODELS.contains(&model.as_str()) {
            return None;
        }
        Some(Window {
            span: Span::WeekOf(model.clone()),
            used_percent: limit.percent?,
            resets_at: limit.resets_at,
        })
    });
    let [opus, sonnet] = FIXED_MODELS.map(|name| Span::WeekOf(name.to_owned()));
    Ok([
        fixed(Span::FiveHours, body.five_hour.as_ref()),
        fixed(Span::Week, body.seven_day.as_ref()),
        fixed(opus, body.seven_day_opus.as_ref()),
        fixed(sonnet, body.seven_day_sonnet.as_ref()),
    ]
    .into_iter()
    .flatten()
    .chain(scoped)
    .collect())
}

/// Windows as read at a moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    pub at: Instant,
    pub windows: Vec<Window>,
}

/// How far apart the reads are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// [`READ_EVERY`]: no refusal since the last read that succeeded.
    Steady,
    /// Refused as too frequent since the last success. `gap` is the reads' own backoff, doubled
    /// at each refusal; the next read is due `until`, counted from the refusal's answer as
    /// `Retry-After` is, and later than `gap` when `Retry-After` asks for longer.
    BackingOff { gap: SignedDuration, until: Instant },
}

impl Pace {
    /// The pace after one more refusal, answered at `now`.
    fn refused(self, retry_after: &RetryAfter, now: Instant) -> Self {
        let gap = match self {
            Self::Steady => READ_EVERY,
            Self::BackingOff { gap, .. } => gap,
        };
        let gap = gap.saturating_mul(2).min(BACKOFF_GAP_MAX);
        let asked = retry_after.wait().unwrap_or(SignedDuration::ZERO);
        let wait = gap.max(asked).min(REFUSED_WAIT_MAX);
        Self::BackingOff {
            gap,
            until: now.checked_add(wait).unwrap_or(Instant::MAX),
        }
    }
}

/// The last windows that were read, and how the latest attempt went.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    last_good: Option<Read>,
    latest: SourceState,
    attempted: Option<Instant>,
    pace: Pace,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            last_good: None,
            latest: SourceState::NeverRead,
            attempted: None,
            pace: Pace::Steady,
        }
    }
}

impl Reading {
    /// The windows of the latest read that succeeded, however old.
    pub const fn last_good(&self) -> Option<&Read> {
        self.last_good.as_ref()
    }

    /// How the latest attempt went.
    pub const fn state(&self) -> &SourceState {
        &self.latest
    }

    /// Whether a read is due at `now`: none was attempted within [`READ_EVERY`], whatever it
    /// came to, and no refusal holds the reads back. Once the endpoint refuses a read as too
    /// frequent, and until a read succeeds, each refusal doubles the wait, up to
    /// [`BACKOFF_GAP_MAX`], and a longer `Retry-After` is honoured up to [`REFUSED_WAIT_MAX`].
    /// Any other failure leaves the pace as it is.
    pub fn is_due(&self, now: Instant) -> bool {
        let a_minute_on = self
            .attempted
            .is_none_or(|at| now.duration_since(at) >= READ_EVERY);
        a_minute_on && self.backed_off_until(now).is_none()
    }

    /// When the next read is due, while a refusal holds the reads back at `now`.
    pub fn backed_off_until(&self, now: Instant) -> Option<Instant> {
        match self.pace {
            Pace::Steady => None,
            Pace::BackingOff { until, .. } => (until > now).then_some(until),
        }
    }

    pub const fn started(&mut self, now: Instant) {
        self.attempted = Some(now);
    }

    pub fn record(&mut self, outcome: Result<Vec<Window>, UsageError>, now: Instant) {
        match outcome {
            Ok(windows) => {
                self.latest = SourceState::Ok { at: now };
                self.last_good = Some(Read { at: now, windows });
                self.pace = Pace::Steady;
            }
            Err(error) => {
                if let UsageError::Refused { retry_after } = &error {
                    self.pace = self.pace.refused(retry_after, now);
                }
                self.latest = SourceState::Failed {
                    at: now,
                    reason: error.to_string(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests;
