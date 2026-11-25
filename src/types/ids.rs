//! Identifiers that name things on disk or in a process table.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Claude Code session id.
///
/// Witnesses that the text is UUID-shaped (8-4-4-4-12 hex digits), lowercased. The only
/// constructor is [`SessionId::parse`], so two ids for one session compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

/// The text was not a UUID-shaped session id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a UUID-shaped session id: {0:?}")]
pub struct SessionIdError(String);

impl SessionId {
    const LEN: usize = 36;
    /// Byte offsets of the hyphens in the canonical form.
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];

    pub fn parse(text: &str) -> Result<Self, SessionIdError> {
        let shaped = text.len() == Self::LEN
            && text.chars().enumerate().all(|(at, c)| {
                if Self::HYPHENS.contains(&at) {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            });
        if shaped {
            Ok(Self(text.to_ascii_lowercase()))
        } else {
            Err(SessionIdError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionId {
    type Error = SessionIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

/// An operating-system process id of a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(u32);

impl Pid {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The name of a directory under `~/.claude/projects/`.
///
/// It is a lossy encoding of the session's working directory (path separators and dots
/// both become `-`), so it must never be decoded back into a path: the session's real cwd
/// comes from the transcript. Witnesses that the text is one plain path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectDirName(String);

/// The text cannot be a single directory name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a single directory name: {0:?}")]
pub struct ProjectDirNameError(String);

impl ProjectDirName {
    pub fn new(name: &str) -> Result<Self, ProjectDirNameError> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            Err(ProjectDirNameError(name.to_owned()))
        } else {
            Ok(Self(name.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    #[test]
    fn session_id_accepts_canonical_uuid() {
        assert_eq!(SessionId::parse(ID).unwrap().as_str(), ID);
    }

    #[test]
    fn session_id_lowercases_so_one_session_has_one_id() {
        let upper = SessionId::parse(&ID.to_uppercase()).unwrap();
        assert_eq!(upper, SessionId::parse(ID).unwrap());
    }

    #[test]
    fn session_id_rejects_malformed_text() {
        for bad in [
            "",
            "not-a-uuid",
            "0a1b2c3d4e5f4a6b8c7d9e0f1a2b3c4d",
            "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4",
            "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4g",
            "0a1b2c3d_4e5f_4a6b_8c7d_9e0f1a2b3c4d",
            "../../etc/passwd-0000000000000000000",
        ] {
            assert!(SessionId::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn session_id_deserialises_through_the_constructor() {
        assert!(serde_json::from_str::<SessionId>("\"nope\"").is_err());
        assert!(serde_json::from_str::<SessionId>(&format!("\"{ID}\"")).is_ok());
    }

    #[test]
    fn project_dir_name_is_one_component() {
        assert!(ProjectDirName::new("-home-dev-proj").is_ok());
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(ProjectDirName::new(bad).is_err(), "accepted {bad:?}");
        }
    }
}
