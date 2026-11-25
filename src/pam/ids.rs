//! The names PAM gives things, each parsed once where it enters: from the config, from gcloud's
//! output, or from a form field.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The location every entitlement read here lives in. PAM has regional locations too; an
/// entitlement in one is refused at the parse and logged, since every command here names this.
pub const LOCATION: &str = "global";

/// A folder's or an organization's number: digits only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NumericId(String);

/// A project's id (or number): lower-case letters, digits and hyphens.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectId(String);

/// An entitlement's own id within its parent, which PAM writes like a project id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntitlementId(String);

/// A name that is not the form it must be.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not {expected}")]
pub struct IdError {
    pub text: String,
    pub expected: &'static str,
}

/// The longest id of any kind accepted: GCP's own limit for an entitlement id is 63.
const ID_CHARS_MAX: usize = 63;

fn slug(text: &str, expected: &'static str) -> Result<String, IdError> {
    let fits = (1..=ID_CHARS_MAX).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if fits {
        Ok(text.to_owned())
    } else {
        Err(IdError {
            text: text.to_owned(),
            expected,
        })
    }
}

impl NumericId {
    pub fn parse(text: &str) -> Result<Self, IdError> {
        if (1..=ID_CHARS_MAX).contains(&text.len()) && text.chars().all(|c| c.is_ascii_digit()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(IdError {
                text: text.to_owned(),
                expected: "a number",
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProjectId {
    pub fn parse(text: &str) -> Result<Self, IdError> {
        slug(text, "a project id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl EntitlementId {
    pub fn parse(text: &str) -> Result<Self, IdError> {
        slug(text, "an entitlement id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! via_string {
    ($t:ty) => {
        impl TryFrom<String> for $t {
            type Error = IdError;
            fn try_from(text: String) -> Result<Self, IdError> {
                Self::parse(&text)
            }
        }
        impl From<$t> for String {
            fn from(id: $t) -> Self {
                id.0
            }
        }
    };
}
via_string!(NumericId);
via_string!(ProjectId);

/// Where an entitlement is defined, which is also what it grants access on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Parent {
    Project(ProjectId),
    Folder(NumericId),
    Organization(NumericId),
}

impl Parent {
    /// `projects/<id>`, `folders/<n>` or `organizations/<n>`.
    pub fn parse(text: &str) -> Result<Self, IdError> {
        let refuse = || IdError {
            text: text.to_owned(),
            expected: "projects/<id>, folders/<n> or organizations/<n>",
        };
        let (kind, id) = text.split_once('/').ok_or_else(refuse)?;
        match kind {
            "projects" => ProjectId::parse(id).map(Self::Project),
            "folders" => NumericId::parse(id).map(Self::Folder),
            "organizations" => NumericId::parse(id).map(Self::Organization),
            _ => Err(refuse()),
        }
    }

    /// The gcloud flag that names this parent, with its value: `--project=<id>`.
    pub fn flag(&self) -> String {
        match self {
            Self::Project(id) => format!("--project={}", id.as_str()),
            Self::Folder(id) => format!("--folder={}", id.as_str()),
            Self::Organization(id) => format!("--organization={}", id.as_str()),
        }
    }
}

impl fmt::Display for Parent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(id) => write!(f, "projects/{}", id.as_str()),
            Self::Folder(id) => write!(f, "folders/{}", id.as_str()),
            Self::Organization(id) => write!(f, "organizations/{}", id.as_str()),
        }
    }
}

/// An entitlement's full name: `<parent>/locations/global/entitlements/<id>`. The page, the
/// config's `break_glass` and a request's form field all write it this way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EntitlementName {
    pub parent: Parent,
    pub id: EntitlementId,
}

impl EntitlementName {
    pub fn parse(text: &str) -> Result<Self, IdError> {
        let refuse = || IdError {
            text: text.to_owned(),
            expected: "<parent>/locations/global/entitlements/<id>",
        };
        let (parent, rest) = text.split_once("/locations/").ok_or_else(refuse)?;
        let (location, id) = rest.split_once("/entitlements/").ok_or_else(refuse)?;
        if location != LOCATION {
            return Err(refuse());
        }
        Ok(Self {
            parent: Parent::parse(parent)?,
            id: EntitlementId::parse(id)?,
        })
    }
}

impl fmt::Display for EntitlementName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/locations/{LOCATION}/entitlements/{}",
            self.parent,
            self.id.as_str()
        )
    }
}

impl TryFrom<String> for EntitlementName {
    type Error = IdError;
    fn try_from(text: String) -> Result<Self, IdError> {
        Self::parse(&text)
    }
}

impl From<EntitlementName> for String {
    fn from(name: EntitlementName) -> Self {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each parent form, written back as it was read.
    /// Mutation: swap the folder and organization arms (the round trip changes kind).
    #[test]
    fn names_round_trip_and_name_their_flag() {
        for (text, flag) in [
            (
                "projects/example-project/locations/global/entitlements/admin",
                "--project=example-project",
            ),
            (
                "folders/123/locations/global/entitlements/net-admin",
                "--folder=123",
            ),
            (
                "organizations/456/locations/global/entitlements/emergency",
                "--organization=456",
            ),
        ] {
            let name = EntitlementName::parse(text).unwrap();
            assert_eq!(name.to_string(), text);
            assert_eq!(name.parent.flag(), flag);
        }
    }

    /// Mutation: skip the location check, or accept upper case in a slug.
    #[test]
    fn other_shapes_are_refused() {
        for text in [
            "projects/p/locations/us-east1/entitlements/admin",
            "projects/P/locations/global/entitlements/admin",
            "folders/abc/locations/global/entitlements/admin",
            "billingAccounts/1/locations/global/entitlements/admin",
            "projects/p/locations/global/entitlements/",
            "projects/p/entitlements/admin",
        ] {
            assert!(EntitlementName::parse(text).is_err(), "{text}");
        }
    }
}
