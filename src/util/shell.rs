//! POSIX shell quoting, for commands the user pastes into a terminal.

use std::borrow::Cow;

/// Characters that never need quoting in a POSIX shell word.
const fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | ',' | ':' | '=' | '@' | '%' | '+')
}

/// `text` as one shell word: unchanged when plain, otherwise single-quoted.
pub fn quote(text: &str) -> Cow<'_, str> {
    if !text.is_empty() && text.chars().all(is_plain) {
        Cow::Borrowed(text)
    } else {
        // Inside single quotes nothing is special except the quote itself, which is
        // closed, escaped and reopened.
        Cow::Owned(format!("'{}'", text.replace('\'', r"'\''")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_are_untouched() {
        assert_eq!(quote("/home/dev/my-proj_1.2"), "/home/dev/my-proj_1.2");
    }

    #[test]
    fn empty_becomes_empty_quotes() {
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn spaces_and_metacharacters_are_single_quoted() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    #[test]
    fn embedded_single_quote_is_escaped() {
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
