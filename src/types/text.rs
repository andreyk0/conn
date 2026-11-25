//! Free text a person wrote that must not be empty.

use serde::{Deserialize, Serialize};

/// Text with at least one non-whitespace character, trimmed.
///
/// Witnesses that a name or a reason says something. Trimming happens in the constructor,
/// so `" x "` and `"x"` are one value everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NonBlank(String);

/// The text was empty or only whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("text is empty or only whitespace")]
pub struct NonBlankError;

impl NonBlank {
    pub fn parse(text: &str) -> Result<Self, NonBlankError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            Err(NonBlankError)
        } else {
            Ok(Self(trimmed.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NonBlank {
    type Error = NonBlankError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<NonBlank> for String {
    fn from(text: NonBlank) -> Self {
        text.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_keeps_content() {
        assert_eq!(
            NonBlank::parse("  auth rework \n").unwrap().as_str(),
            "auth rework"
        );
    }

    #[test]
    fn rejects_blank() {
        assert_eq!(NonBlank::parse(" \t\n"), Err(NonBlankError));
        assert_eq!(NonBlank::parse(""), Err(NonBlankError));
    }
}
