//! Who may talk to the server: two per-launch tokens, and the names the request used.
//!
//! The server listens on loopback only, so the one thing a hostile page in the user's own
//! browser can do is send requests to it. Each launch has two tokens, one per role: the
//! [`Read`] token, printed in the sign-in URL and kept as the cookie, admits reads; the
//! [`Write`] token exists only in the pages the server renders, never in a cookie, and admits
//! changes. A request that carries the cookie alone, or the cookie and the read token, cannot
//! change anything. Three checks stop a hostile page, in this order:
//!
//! 1. `Host` is one of the two loopback names with the port: a page reached through a
//!    rebinding DNS name carries its own name there.
//! 2. The `conn` cookie equals the read token: only a browser that was sent the printed URL
//!    has it, and `SameSite=Strict` keeps it off cross-site requests.
//! 3. A POST also carries an `Origin` that is the server's own and the write token in
//!    `X-Conn-Token`, which only the page's script (reading it from the page) can set.
//!
//! [`admit`] is the one function that decides, and says why it refused.

use std::fmt::Write as _;
use std::marker::PhantomData;

use axum::http::header::COOKIE;
use axum::http::{HeaderMap, Method, StatusCode};
use subtle::ConstantTimeEq;

/// The cookie the token travels in.
pub const COOKIE_NAME: &str = "conn";
/// The header a POST carries the write token in.
pub const TOKEN_HEADER: &str = "x-conn-token";

/// The bytes of randomness in a token.
const TOKEN_BYTES: usize = 32;

/// The role of the token that signs a browser in and admits reads.
#[derive(Debug, Clone, Copy)]
pub enum Read {}

/// The role of the token that admits changes.
#[derive(Debug, Clone, Copy)]
pub enum Write {}

/// A per-launch secret of one role, `R`: a [`Token<Read>`] cannot be offered where a
/// [`Token<Write>`] is required.
///
/// Witnesses that the text came from the operating system's random source. Equality is not
/// offered: the only comparison is [`Token::matches`], which takes the same time wherever the
/// texts differ.
#[derive(Clone)]
pub struct Token<R>(String, PhantomData<R>);

/// Both tokens of one launch.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub read: Token<Read>,
    pub write: Token<Write>,
}

impl Tokens {
    pub fn generate() -> Result<Self, TokenError> {
        Ok(Self {
            read: Token::generate()?,
            write: Token::generate()?,
        })
    }

    /// Tokens with known values, for tests that must send them.
    #[cfg(test)]
    pub fn from_texts(read: &str, write: &str) -> Self {
        Self {
            read: Token(read.to_owned(), PhantomData),
            write: Token(write.to_owned(), PhantomData),
        }
    }
}

/// The operating system could not supply random bytes.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the system's random source: {0}")]
pub struct TokenError(getrandom::Error);

impl<R> Token<R> {
    /// Thirty-two random bytes, as lowercase hex.
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(TokenError)?;
        let hex = bytes.iter().fold(String::new(), |mut hex, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        Ok(Self(hex, PhantomData))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `offered` is the token. The comparison does not stop at the first difference.
    pub fn matches(&self, offered: &str) -> bool {
        self.0.as_bytes().ct_eq(offered.as_bytes()).into()
    }
}

impl<R> std::fmt::Debug for Token<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What the request is, as far as admission goes: it reads the page, or it changes something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Read,
    Write,
}

impl Intent {
    pub fn of(method: &Method) -> Self {
        if *method == Method::GET || *method == Method::HEAD {
            Self::Read
        } else {
            Self::Write
        }
    }
}

/// The verdict on one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Admitted(Credential),
    Refused(Refusal),
}

/// What admitted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// The `conn` cookie.
    Cookie,
    /// The token in the URL of a read: the sign-in, which the caller answers with the cookie.
    UrlToken,
}

/// Why a request was refused. Each reason has one status and one line for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `Host` is missing or not a loopback name with this port.
    WrongHost,
    /// No `conn` cookie, or one that is not the token.
    NoSession,
    /// A POST without this server's own `Origin`.
    WrongOrigin,
    /// A POST without the write token in its header: the read token is not it.
    NoWriteToken,
}

impl Refusal {
    pub const fn status(self) -> StatusCode {
        match self {
            Self::WrongHost => StatusCode::BAD_REQUEST,
            Self::NoSession => StatusCode::UNAUTHORIZED,
            Self::WrongOrigin | Self::NoWriteToken => StatusCode::FORBIDDEN,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::WrongHost => "Wrong Host header: use http://127.0.0.1 with the printed port.",
            Self::NoSession => "Not signed in: open the URL that `conn serve` printed.",
            Self::WrongOrigin => "Refused: a change must come from this site's own page.",
            Self::NoWriteToken => {
                "Refused: a change needs the write token, which only the page's script sends."
            }
        }
    }
}

/// The names this server answers to, built from its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    hosts: [String; 2],
    origins: [String; 2],
}

impl Names {
    pub fn on_port(port: u16) -> Self {
        let host = |name: &str| format!("{name}:{port}");
        Self {
            hosts: [host("127.0.0.1"), host("localhost")],
            origins: [
                format!("http://{}", host("127.0.0.1")),
                format!("http://{}", host("localhost")),
            ],
        }
    }

    fn host_is_ours(&self, host: Option<&str>) -> bool {
        host.is_some_and(|host| self.hosts.iter().any(|ours| ours == host))
    }

    fn origin_is_ours(&self, origin: Option<&str>) -> bool {
        origin.is_some_and(|origin| self.origins.iter().any(|ours| ours == origin))
    }
}

/// Decides whether the request may proceed. `url_token` is the `t` of the sign-in URL, given
/// only to the request that may sign in; it admits a read and nothing else.
pub fn admit(
    names: &Names,
    tokens: &Tokens,
    intent: Intent,
    url_token: Option<&str>,
    headers: &HeaderMap,
) -> Verdict {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if !names.host_is_ours(text("host")) {
        return Verdict::Refused(Refusal::WrongHost);
    }
    if intent == Intent::Read && url_token.is_some_and(|offered| tokens.read.matches(offered)) {
        return Verdict::Admitted(Credential::UrlToken);
    }
    let has_cookie = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(name, value)| name == COOKIE_NAME && tokens.read.matches(value));
    if !has_cookie {
        return Verdict::Refused(Refusal::NoSession);
    }
    match intent {
        Intent::Read => Verdict::Admitted(Credential::Cookie),
        Intent::Write => {
            if !names.origin_is_ours(text("origin")) {
                Verdict::Refused(Refusal::WrongOrigin)
            } else if !text(TOKEN_HEADER).is_some_and(|offered| tokens.write.matches(offered)) {
                Verdict::Refused(Refusal::NoWriteToken)
            } else {
                Verdict::Admitted(Credential::Cookie)
            }
        }
    }
}

/// The `Set-Cookie` value that signs the browser in.
pub fn session_cookie(token: &Token<Read>) -> String {
    format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
        token.as_str()
    )
}

#[cfg(test)]
mod tests {
    //! The admission matrix with hand-written headers: every refusal reason, and the one
    //! combination that admits each intent.

    use axum::http::HeaderValue;

    use super::*;

    const PORT: u16 = 7357;

    fn tokens() -> Tokens {
        Tokens::from_texts("abc123", "wr1te9")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn verdict(intent: Intent, pairs: &[(&str, &str)]) -> Verdict {
        admit(
            &Names::on_port(PORT),
            &tokens(),
            intent,
            None,
            &headers(pairs),
        )
    }

    const HOST_OK: (&str, &str) = ("host", "127.0.0.1:7357");
    const COOKIE_OK: (&str, &str) = ("cookie", "other=1; conn=abc123");
    const ORIGIN_OK: (&str, &str) = ("origin", "http://127.0.0.1:7357");
    const TOKEN_OK: (&str, &str) = ("x-conn-token", "wr1te9");
    const READ_TOKEN_AS_WRITE: (&str, &str) = ("x-conn-token", "abc123");

    /// Mutation: accept any `Host`, compare the host by prefix, or check the cookie before the
    /// host (the wrong-host row without a cookie answers 401).
    #[test]
    fn the_host_must_be_a_loopback_name_with_the_port() {
        for host in ["127.0.0.1:7357", "localhost:7357"] {
            assert_eq!(
                verdict(Intent::Read, &[("host", host), COOKIE_OK]),
                Verdict::Admitted(Credential::Cookie),
                "{host}"
            );
        }
        for host in [
            "127.0.0.1",
            "127.0.0.1:7358",
            "evil.example:7357",
            "127.0.0.1:7357.evil.example",
            "localhost.evil.example:7357",
            "[::1]:7357",
        ] {
            assert_eq!(
                verdict(Intent::Read, &[("host", host)]),
                Verdict::Refused(Refusal::WrongHost),
                "{host}"
            );
        }
        assert_eq!(
            verdict(Intent::Read, &[COOKIE_OK]),
            Verdict::Refused(Refusal::WrongHost)
        );
    }

    /// Mutation: accept a cookie that merely starts with the token, or ignore the cookie name.
    #[test]
    fn a_read_needs_the_cookie_to_be_the_token() {
        assert_eq!(
            verdict(Intent::Read, &[HOST_OK]),
            Verdict::Refused(Refusal::NoSession)
        );
        for cookie in [
            "conn=abc12",
            "conn=abc1234",
            "other=abc123",
            "conn=",
            "abc123",
        ] {
            assert_eq!(
                verdict(Intent::Read, &[HOST_OK, ("cookie", cookie)]),
                Verdict::Refused(Refusal::NoSession),
                "{cookie}"
            );
        }
        assert_eq!(
            verdict(Intent::Read, &[HOST_OK, COOKIE_OK]),
            Verdict::Admitted(Credential::Cookie)
        );
    }

    /// Mutation: skip the origin check, skip the header check, compare the origin by
    /// substring (the `evil` row), or compare the header with the read token (the read-token
    /// row is admitted and the write-token row is refused).
    #[test]
    fn a_write_also_needs_our_origin_and_the_header_token() {
        assert_eq!(
            verdict(Intent::Write, &[HOST_OK, COOKIE_OK, TOKEN_OK]),
            Verdict::Refused(Refusal::WrongOrigin)
        );
        for origin in [
            "http://evil.example",
            "https://127.0.0.1:7357",
            "http://127.0.0.1:7357.evil.example",
            "null",
        ] {
            assert_eq!(
                verdict(
                    Intent::Write,
                    &[HOST_OK, COOKIE_OK, ("origin", origin), TOKEN_OK]
                ),
                Verdict::Refused(Refusal::WrongOrigin),
                "{origin}"
            );
        }
        assert_eq!(
            verdict(Intent::Write, &[HOST_OK, COOKIE_OK, ORIGIN_OK]),
            Verdict::Refused(Refusal::NoWriteToken)
        );
        assert_eq!(
            verdict(
                Intent::Write,
                &[HOST_OK, COOKIE_OK, ORIGIN_OK, ("x-conn-token", "wr1te8")]
            ),
            Verdict::Refused(Refusal::NoWriteToken)
        );
        assert_eq!(
            verdict(
                Intent::Write,
                &[HOST_OK, COOKIE_OK, ORIGIN_OK, READ_TOKEN_AS_WRITE]
            ),
            Verdict::Refused(Refusal::NoWriteToken),
            "the cookie's token is not the write token"
        );
        assert_eq!(
            verdict(Intent::Write, &[HOST_OK, COOKIE_OK, ORIGIN_OK, TOKEN_OK]),
            Verdict::Admitted(Credential::Cookie)
        );
        assert_eq!(
            verdict(
                Intent::Write,
                &[
                    ("host", "localhost:7357"),
                    COOKIE_OK,
                    ("origin", "http://localhost:7357"),
                    TOKEN_OK
                ]
            ),
            Verdict::Admitted(Credential::Cookie)
        );
    }

    /// Mutation: let the URL token admit a write, or skip the host check before it.
    #[test]
    fn the_url_token_signs_in_a_read_and_nothing_else() {
        let admit_with = |intent, url_token: Option<&str>, pairs: &[(&str, &str)]| {
            admit(
                &Names::on_port(PORT),
                &tokens(),
                intent,
                url_token,
                &headers(pairs),
            )
        };
        assert_eq!(
            admit_with(Intent::Read, Some("abc123"), &[HOST_OK]),
            Verdict::Admitted(Credential::UrlToken)
        );
        assert_eq!(
            admit_with(Intent::Read, Some("wrong"), &[HOST_OK]),
            Verdict::Refused(Refusal::NoSession)
        );
        assert_eq!(
            admit_with(Intent::Write, Some("abc123"), &[HOST_OK]),
            Verdict::Refused(Refusal::NoSession)
        );
        assert_eq!(
            admit_with(Intent::Read, Some("abc123"), &[("host", "evil.example")]),
            Verdict::Refused(Refusal::WrongHost)
        );
    }

    #[test]
    fn each_refusal_has_its_status() {
        assert_eq!(Refusal::WrongHost.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Refusal::NoSession.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Refusal::WrongOrigin.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::NoWriteToken.status(), StatusCode::FORBIDDEN);
    }

    /// The token is 64 lowercase hex digits and two launches differ, and the two roles of one
    /// launch differ.
    /// Mutation: fill fewer bytes, return a constant, or generate the write token as a copy of
    /// the read token.
    #[test]
    fn a_generated_token_is_sixty_four_hex_digits_and_not_repeated() {
        let (a, b) = (Tokens::generate().unwrap(), Tokens::generate().unwrap());
        assert!(!a.read.matches(a.write.as_str()));
        let (a, b) = (a.read, b.read);
        assert_eq!(a.as_str().len(), 64);
        assert!(
            a.as_str()
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert!(!a.matches(b.as_str()));
        assert!(a.matches(a.as_str()));
    }

    #[test]
    fn the_session_cookie_has_the_three_attributes() {
        assert_eq!(
            session_cookie(&tokens().read),
            "conn=abc123; HttpOnly; SameSite=Strict; Path=/"
        );
    }
}
