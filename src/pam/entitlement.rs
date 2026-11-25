//! An entitlement as `gcloud pam entitlements search` prints it, parsed into what the page and
//! a request need: how long a grant may last, who approves it, and whether a reason is asked.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::ids::{EntitlementName, IdError};

/// The grant length a request offers first: four hours, or the entitlement's maximum when that
/// is shorter.
pub const DEFAULT_LENGTH: Duration = Duration::from_hours(4);

const HOUR_SECS: u64 = 3600;

/// Who decides on a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// No approval workflow: PAM grants it on request.
    Auto,
    /// Any one of these principals (`group:…`, `user:…`), as PAM writes them, approves it.
    Manual { approvers: Vec<String> },
}

/// Whether a request must say why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Justification {
    Optional,
    /// Free text is required.
    Required,
    /// A kind this tool does not know, by its key; a request sends the text and PAM decides.
    Other(String),
}

/// How long a grant is asked for: one of [`Entitlement::lengths`], so never more than PAM
/// allows. Its one constructor is [`Entitlement::length`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantLength(Duration);

impl GrantLength {
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    pub const fn secs(self) -> u64 {
        self.0.as_secs()
    }
}

/// One entitlement the user may request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entitlement {
    pub name: EntitlementName,
    /// The longest grant PAM accepts.
    pub max: Duration,
    pub approval: Approval,
    pub justification: Justification,
    /// The roles a grant binds, as PAM writes them.
    pub roles: Vec<String>,
}

impl Entitlement {
    /// The lengths a request offers: each whole hour up to the maximum, or the maximum alone
    /// when it is under an hour.
    pub fn lengths(&self) -> Vec<GrantLength> {
        let hours = self.max.as_secs().div_euclid(HOUR_SECS);
        if hours == 0 {
            vec![GrantLength(self.max)]
        } else {
            (1..=hours)
                .map(|hour| GrantLength(Duration::from_secs(hour.saturating_mul(HOUR_SECS))))
                .collect()
        }
    }

    /// The length a request offers first: the longest of [`Self::lengths`] not over
    /// [`DEFAULT_LENGTH`], or the shortest when every one is over it.
    pub fn default_length(&self) -> Option<GrantLength> {
        let lengths = self.lengths();
        lengths
            .iter()
            .copied()
            .filter(|length| length.0 <= DEFAULT_LENGTH)
            .max()
            .or_else(|| lengths.first().copied())
    }

    /// `secs` as a length this entitlement offers; `None` when it offers no such length.
    pub fn length(&self, secs: u64) -> Option<GrantLength> {
        self.lengths()
            .into_iter()
            .find(|length| length.secs() == secs)
    }
}

/// The entitlement as gcloud prints it, keeping the fields read here. It is also the stored
/// form of the inventory, so a saved file is parsed by the same code as a fresh answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEntitlement {
    name: String,
    max_request_duration: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_workflow: Option<RawWorkflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requester_justification_config: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    privileged_access: Option<RawAccess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorkflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual_approvals: Option<RawManual>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawManual {
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawStep {
    #[serde(default)]
    approvers: Vec<RawApprovers>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawApprovers {
    #[serde(default)]
    principals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAccess {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gcp_iam_access: Option<RawIamAccess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIamAccess {
    #[serde(default)]
    role_bindings: Vec<RawBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawBinding {
    role: String,
}

/// Why one entitlement in an answer could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntitlementError {
    #[error(transparent)]
    Name(#[from] IdError),
    #[error("{name}: maxRequestDuration `{value}` is not whole seconds")]
    Duration { name: String, value: String },
}

impl RawEntitlement {
    pub fn parse(&self) -> Result<Entitlement, EntitlementError> {
        let name = EntitlementName::parse(&self.name)?;
        let max =
            seconds(&self.max_request_duration).ok_or_else(|| EntitlementError::Duration {
                name: self.name.clone(),
                value: self.max_request_duration.clone(),
            })?;
        let approval = match self
            .approval_workflow
            .as_ref()
            .and_then(|w| w.manual_approvals.as_ref())
        {
            None => Approval::Auto,
            Some(manual) => Approval::Manual {
                approvers: manual
                    .steps
                    .iter()
                    .flat_map(|step| &step.approvers)
                    .flat_map(|approvers| approvers.principals.iter().cloned())
                    .collect(),
            },
        };
        let justification = match &self.requester_justification_config {
            None => Justification::Optional,
            Some(config) if config.contains_key("notMandatory") => Justification::Optional,
            Some(config) if config.contains_key("unstructured") => Justification::Required,
            Some(config) => {
                Justification::Other(config.keys().cloned().collect::<Vec<_>>().join(","))
            }
        };
        let roles = self
            .privileged_access
            .as_ref()
            .and_then(|access| access.gcp_iam_access.as_ref())
            .map_or_else(Vec::new, |iam| {
                iam.role_bindings.iter().map(|b| b.role.clone()).collect()
            });
        Ok(Entitlement {
            name,
            max,
            approval,
            justification,
            roles,
        })
    }
}

/// A protobuf duration as gcloud's JSON writes it: whole seconds, then `s` (`"3600s"`).
pub fn seconds(text: &str) -> Option<Duration> {
    text.strip_suffix('s')?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entitlement(max_secs: u64) -> Entitlement {
        Entitlement {
            name: EntitlementName::parse("projects/p/locations/global/entitlements/admin").unwrap(),
            max: Duration::from_secs(max_secs),
            approval: Approval::Auto,
            justification: Justification::Optional,
            roles: Vec::new(),
        }
    }

    /// Lengths are whole hours up to the maximum; the default is four hours, or the maximum
    /// when shorter; a length not offered is refused.
    /// Mutation: offer one hour past the maximum, or default to the maximum (the 8 h row).
    #[test]
    fn lengths_never_pass_the_maximum_and_default_to_four_hours() {
        let hours =
            |e: &Entitlement| -> Vec<u64> { e.lengths().iter().map(|l| l.secs() / 3600).collect() };
        assert_eq!(hours(&entitlement(3600)), [1]);
        assert_eq!(hours(&entitlement(7200)), [1, 2]);
        assert_eq!(hours(&entitlement(28_800)), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            entitlement(28_800).default_length().map(GrantLength::secs),
            Some(14_400)
        );
        assert_eq!(
            entitlement(7200).default_length().map(GrantLength::secs),
            Some(7200)
        );
        assert_eq!(
            entitlement(1800)
                .lengths()
                .iter()
                .map(|l| l.secs())
                .collect::<Vec<_>>(),
            [1800]
        );
        assert_eq!(
            entitlement(1800).default_length().map(GrantLength::secs),
            Some(1800)
        );
        assert_eq!(
            entitlement(7200).length(7200).map(GrantLength::secs),
            Some(7200)
        );
        assert_eq!(entitlement(7200).length(10_800), None);
        assert_eq!(entitlement(7200).length(5400), None);
    }

    /// The fields gcloud prints, shaped as the live answers were (2026-11-19), with masked
    /// names. Mutation: read a present workflow as automatic, or `unstructured` as optional.
    #[test]
    fn gcloud_entitlements_parse_into_approval_and_justification() {
        let raw: Vec<RawEntitlement> =
            serde_json::from_str(include_str!("../../tests/fixtures/pam/entitlements.json"))
                .unwrap();
        let parsed: Vec<Entitlement> = raw.iter().map(|r| r.parse().unwrap()).collect();
        let [auto, manual] = parsed.as_slice() else {
            panic!("two entitlements")
        };
        assert_eq!(
            auto.name.to_string(),
            "folders/111/locations/global/entitlements/admin"
        );
        assert_eq!(auto.max, Duration::from_secs(3600));
        assert_eq!(auto.approval, Approval::Auto);
        assert_eq!(auto.justification, Justification::Optional);
        assert_eq!(auto.roles, ["roles/admin"]);
        assert_eq!(
            manual.approval,
            Approval::Manual {
                approvers: vec!["group:team-a@example.com".to_owned()]
            }
        );
        assert_eq!(manual.justification, Justification::Required);
        assert_eq!(manual.max, Duration::from_secs(28_800));
    }

    #[test]
    fn durations_are_whole_seconds() {
        assert_eq!(seconds("3600s"), Some(Duration::from_secs(3600)));
        assert_eq!(seconds("3600"), None);
        assert_eq!(seconds("1.5s"), None);
    }
}
