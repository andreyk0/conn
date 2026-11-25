//! `~/.config/conn/config.toml`: the only place a host or a project prefix is written down.
//!
//! Jira and Slack have no defaults: a link built against a guessed host would point
//! somewhere the user never chose. A missing file is an error that shows an example.
//! Without Jira (no `[jira]` section, or an empty one) no issue key is recognised and no
//! work item is read; a section that names only one of its fields is an error, not that.
//! Without Slack (no `[slack]` section, or an empty one) no Slack URL is a thread or a
//! document; `[reviews]` needs Slack, since each review card links the message that asked.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::pam::ids::{EntitlementName, NumericId, ProjectId};
use crate::pam::{Organizations, PamConfig};
use crate::reviews::ReviewsConfig;
use crate::types::{FileRootError, FileRoots, Host, JiraPrefix, LinkTargets};

/// The documented example, shown when the file is missing and checked by the tests.
const EXAMPLE: &str = include_str!("../docs/config.example.toml");

/// The environment variable that overrides the config path.
pub const PATH_ENV: &str = "CONN_CONFIG";

/// Config path relative to the home directory.
const DEFAULT_PATH: &str = ".config/conn/config.toml";
const DEFAULT_CLAUDE_DIR: &str = ".claude";
/// The state directory relative to the home directory when the config names none. The hook
/// falls back to the same place when the config cannot be had, so a configured console and an
/// unconfigured hook agree.
pub const DEFAULT_STATE_DIR: &str = ".local/state/conn";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `None` where there is no Jira.
    pub jira: Option<JiraConfig>,
    /// `None` where there is no Slack.
    pub slack: Option<SlackConfig>,
    pub github: GithubConfig,
    pub paths: Paths,
    /// Where the local markdown files a session names may be shown from.
    pub files: FileRoots,
    /// Where review requests are listened for; `None` turns the review queue off.
    pub reviews: Option<ReviewsConfig>,
    /// `None` turns the Follow-ups page off.
    pub followups: Option<FollowUpsConfig>,
    /// `None` turns the PAM page off, and no gcloud process is started.
    pub pam: Option<PamConfig>,
}

/// The `[pam]` section as written: `organizations` and `quota_project` both, or neither (and
/// then no `break_glass` either).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPam {
    #[serde(default)]
    organizations: BTreeSet<NumericId>,
    quota_project: Option<ProjectId>,
    #[serde(default)]
    break_glass: BTreeSet<EntitlementName>,
}

/// The `[followups]` section: present, the Follow-ups page is on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpsConfig {
    /// Work item statuses that mean "shipped, waiting for its close check", as Jira writes
    /// them; an item in one is due. Absent, no status is.
    #[serde(default)]
    pub close_statuses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraConfig {
    pub host: Host,
    /// Project prefixes whose keys are recognised as issues; never empty, which
    /// [`Config::parse`] checks.
    pub prefixes: Vec<JiraPrefix>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConfig {
    pub host: Host,
}

/// The whole section may be absent, and so may the owner: `None` means "no default owner".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    pub default_owner: Option<String>,
}

/// Absolute directories the tool reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Claude Code's own directory; read-only to this tool.
    pub claude_dir: PathBuf,
    /// Where the event log and the other state files live.
    pub state_dir: PathBuf,
    /// The home directory the defaults are relative to; a session working there says nothing
    /// about its work.
    pub home: PathBuf,
}

/// The file as written; the optional paths are resolved against the home directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    jira: RawJira,
    #[serde(default)]
    slack: RawSlack,
    #[serde(default)]
    github: GithubConfig,
    #[serde(default)]
    paths: RawPaths,
    #[serde(default)]
    files: RawFiles,
    reviews: Option<ReviewsConfig>,
    followups: Option<FollowUpsConfig>,
    #[serde(default)]
    pam: RawPam,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPaths {
    claude_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

/// Both fields, or neither.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJira {
    host: Option<Host>,
    prefixes: Option<Vec<JiraPrefix>>,
}

/// Absent or empty, there is no Slack.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlack {
    host: Option<Host>,
}

/// Absent, no local file is shown.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFiles {
    roots: Vec<PathBuf>,
}

/// Why the config could not be had. `Missing` and `Unreadable` mean the file was not
/// read; the rest mean it was read and is wrong.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "the home directory is unknown, so the default config path cannot be formed; set {PATH_ENV}"
    )]
    NoHomeDir,
    #[error("config file not found: {}\n\nCreate it, for example:\n\n{EXAMPLE}", path.display())]
    Missing { path: PathBuf },
    #[error("cannot read config file {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config file {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("config file {}: `jira.prefixes` is empty, so no issue key could be recognised", path.display())]
    NoJiraPrefixes { path: PathBuf },
    #[error(
        "config file {}: `[jira]` has no `{missing}`; give both `host` and `prefixes`, or leave the section out where there is no Jira",
        path.display()
    )]
    IncompleteJira {
        path: PathBuf,
        missing: &'static str,
    },
    #[error(
        "config file {}: `[reviews]` needs `[slack]` with a `host`, so each review card can link the message that asked",
        path.display()
    )]
    ReviewsWithoutSlack { path: PathBuf },
    #[error(
        "config file {}: `[pam]` has no `{missing}`; give `organizations` and `quota_project`, or leave the section out where there is no PAM",
        path.display()
    )]
    IncompletePam {
        path: PathBuf,
        missing: &'static str,
    },
    #[error("config file {}: `paths.{field}` must be absolute, got {}", path.display(), value.display())]
    RelativePath {
        path: PathBuf,
        field: &'static str,
        value: PathBuf,
    },
    #[error("config file {}: `files.roots`: {source}", path.display())]
    FileRoot {
        path: PathBuf,
        source: FileRootError,
    },
}

impl Config {
    /// Loads from `$CONN_CONFIG`, else `~/.config/conn/config.toml`.
    pub fn load() -> Result<Self, ConfigError> {
        let home = std::env::home_dir().ok_or(ConfigError::NoHomeDir)?;
        let path = config_path(&home, std::env::var_os(PATH_ENV));
        Self::load_from(&path, &home)
    }

    pub fn load_from(path: &Path, home: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            let path = path.to_path_buf();
            if source.kind() == ErrorKind::NotFound {
                ConfigError::Missing { path }
            } else {
                ConfigError::Unreadable { path, source }
            }
        })?;
        Self::parse(&text, path, home)
    }

    /// `path` only labels errors; `home` anchors the default directories.
    pub fn parse(text: &str, path: &Path, home: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Invalid {
            path: path.to_path_buf(),
            source,
        })?;
        let jira = jira_config(raw.jira, path)?;
        let pam = pam_config(raw.pam, path)?;
        let slack = raw.slack.host.map(|host| SlackConfig { host });
        if raw.reviews.is_some() && slack.is_none() {
            return Err(ConfigError::ReviewsWithoutSlack {
                path: path.to_path_buf(),
            });
        }
        let absolute = |field: &'static str, chosen: Option<PathBuf>, default: &str| {
            let value = chosen.unwrap_or_else(|| home.join(default));
            if value.is_absolute() {
                Ok(value)
            } else {
                Err(ConfigError::RelativePath {
                    path: path.to_path_buf(),
                    field,
                    value,
                })
            }
        };
        Ok(Self {
            paths: Paths {
                claude_dir: absolute("claude_dir", raw.paths.claude_dir, DEFAULT_CLAUDE_DIR)?,
                state_dir: absolute("state_dir", raw.paths.state_dir, DEFAULT_STATE_DIR)?,
                home: home.to_path_buf(),
            },
            files: FileRoots::new(raw.files.roots).map_err(|source| ConfigError::FileRoot {
                path: path.to_path_buf(),
                source,
            })?,
            jira,
            slack,
            github: raw.github,
            reviews: raw.reviews,
            followups: raw.followups,
            pam,
        })
    }

    pub fn link_targets(&self) -> LinkTargets {
        LinkTargets {
            jira: self.jira.as_ref().map(|jira| jira.host.clone()),
            slack: self.slack.as_ref().map(|slack| slack.host.clone()),
        }
    }

    /// The prefixes whose keys are issues; none where there is no Jira.
    pub fn jira_prefixes(&self) -> &[JiraPrefix] {
        self.jira
            .as_ref()
            .map_or(&[], |jira| jira.prefixes.as_slice())
    }
}

/// The `[jira]` section as written: both fields, with at least one prefix, or neither.
fn jira_config(raw: RawJira, path: &Path) -> Result<Option<JiraConfig>, ConfigError> {
    let incomplete = |missing| ConfigError::IncompleteJira {
        path: path.to_path_buf(),
        missing,
    };
    match (raw.host, raw.prefixes) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(incomplete("prefixes")),
        (None, Some(_)) => Err(incomplete("host")),
        (Some(_), Some(prefixes)) if prefixes.is_empty() => Err(ConfigError::NoJiraPrefixes {
            path: path.to_path_buf(),
        }),
        (Some(host), Some(prefixes)) => Ok(Some(JiraConfig { host, prefixes })),
    }
}

/// The `[pam]` section as written: absent or empty is no PAM; half of it is an error naming
/// the missing field.
fn pam_config(raw: RawPam, path: &Path) -> Result<Option<PamConfig>, ConfigError> {
    let incomplete = |missing| ConfigError::IncompletePam {
        path: path.to_path_buf(),
        missing,
    };
    let organizations = Organizations::of(raw.organizations);
    match (organizations, raw.quota_project) {
        (None, None) if raw.break_glass.is_empty() => Ok(None),
        (None, _) => Err(incomplete("organizations")),
        (Some(_), None) => Err(incomplete("quota_project")),
        (Some(organizations), Some(quota_project)) => Ok(Some(PamConfig {
            organizations,
            quota_project,
            break_glass: raw.break_glass,
        })),
    }
}

fn config_path(home: &Path, overridden: Option<OsString>) -> PathBuf {
    overridden.map_or_else(|| home.join(DEFAULT_PATH), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/dev";

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/cfg.toml"), Path::new(HOME))
    }

    const MINIMAL: &str = r#"
        [jira]
        host = "example.atlassian.net"
        prefixes = ["ABC"]
        [slack]
        host = "example.slack.com"
    "#;

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(
            config.jira.map(|jira| jira.host.as_str().to_owned()),
            Some("example.atlassian.net".to_owned())
        );
        assert_eq!(
            config.slack.map(|slack| slack.host.as_str().to_owned()),
            Some("example.slack.com".to_owned())
        );
        assert_eq!(
            config.followups,
            Some(FollowUpsConfig {
                close_statuses: vec!["Prod".to_owned()]
            })
        );
    }

    /// The Follow-ups page is on only with its section; an empty section turns it on with no
    /// closing status, and a misspelt key is refused rather than ignored.
    /// Mutation: default the section to present (red, the minimal config has follow-ups).
    #[test]
    fn followups_are_on_only_with_their_section() {
        assert_eq!(parse(MINIMAL).unwrap().followups, None);
        assert_eq!(
            parse(&format!("{MINIMAL}\n[followups]\n"))
                .unwrap()
                .followups,
            Some(FollowUpsConfig::default())
        );
        assert!(
            parse(&format!(
                "{MINIMAL}\n[followups]\nclose_status = [\"Prod\"]\n"
            ))
            .is_err()
        );
    }

    /// No `[pam]` section, or an empty one, is no PAM; organizations and a quota project turn
    /// it on, one without the other (an empty list is none) is an error naming the missing
    /// one, and a malformed id, break-glass name or the old single `organization` fails the
    /// load.
    /// Mutation: read `(Some, None)` as no PAM (the organization-only row parses), or skip the
    /// break-glass parse (the bad-name row parses).
    #[test]
    fn pam_is_both_ids_or_neither() {
        for without in [MINIMAL.to_owned(), format!("{MINIMAL}\n[pam]\n")] {
            assert_eq!(parse(&without).unwrap().pam, None, "{without}");
        }
        let on = parse(&format!(
            "{MINIMAL}\n[pam]\norganizations = [\"123\", \"45\"]\nquota_project = \"example-project\"\nbreak_glass = [\"organizations/123/locations/global/entitlements/emergency\"]\n"
        ))
        .unwrap()
        .pam
        .unwrap();
        assert_eq!(
            on.organizations
                .iter()
                .map(NumericId::as_str)
                .collect::<Vec<_>>(),
            ["123", "45"]
        );
        assert_eq!(on.quota_project.as_str(), "example-project");
        assert_eq!(
            on.break_glass
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["organizations/123/locations/global/entitlements/emergency"]
        );
        assert!(matches!(
            parse(&format!("{MINIMAL}\n[pam]\norganizations = [\"123\"]\n")),
            Err(ConfigError::IncompletePam {
                missing: "quota_project",
                ..
            })
        ));
        assert!(matches!(
            parse(&format!(
                "{MINIMAL}\n[pam]\nbreak_glass = [\"organizations/1/locations/global/entitlements/e\"]\n"
            )),
            Err(ConfigError::IncompletePam {
                missing: "organizations",
                ..
            })
        ));
        assert!(matches!(
            parse(&format!(
                "{MINIMAL}\n[pam]\norganizations = []\nquota_project = \"p\"\n"
            )),
            Err(ConfigError::IncompletePam {
                missing: "organizations",
                ..
            })
        ));
        for bad in [
            "organizations = [\"org-1\"]\nquota_project = \"p\"",
            "organizations = [\"1\"]\nquota_project = \"P\"",
            "organizations = [\"1\"]\nquota_project = \"p\"\nbreak_glass = [\"emergency\"]",
            "organization = \"1\"\nquota_project = \"p\"",
        ] {
            assert!(
                matches!(
                    parse(&format!("{MINIMAL}\n[pam]\n{bad}\n")),
                    Err(ConfigError::Invalid { .. })
                ),
                "{bad}"
            );
        }
    }

    /// No `[jira]` section, or an empty one, is a config without Jira; half a section is an
    /// error naming the missing field, and an empty prefix list is still refused.
    /// Mutation: read `(Some, None)` as no Jira (the host-only row parses), or drop the
    /// empty-prefixes arm (the `prefixes = []` row parses).
    #[test]
    fn jira_is_both_fields_or_neither() {
        let slack = "[slack]\nhost = \"example.slack.com\"\n";
        for without in [slack.to_owned(), format!("[jira]\n{slack}")] {
            let config = parse(&without).unwrap();
            assert_eq!(config.jira, None, "{without}");
            assert_eq!(config.jira_prefixes(), Vec::<JiraPrefix>::new());
            assert_eq!(config.link_targets().jira, None);
        }
        assert!(matches!(
            parse(&format!(
                "[jira]\nhost = \"example.atlassian.net\"\n{slack}"
            )),
            Err(ConfigError::IncompleteJira {
                missing: "prefixes",
                ..
            })
        ));
        assert!(matches!(
            parse(&format!("[jira]\nprefixes = [\"ABC\"]\n{slack}")),
            Err(ConfigError::IncompleteJira {
                missing: "host",
                ..
            })
        ));
        assert!(matches!(
            parse(&format!(
                "[jira]\nhost = \"example.atlassian.net\"\nprefixes = []\n{slack}"
            )),
            Err(ConfigError::NoJiraPrefixes { .. })
        ));
    }

    #[test]
    fn omitted_sections_resolve_against_home() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(
            config.github,
            GithubConfig {
                default_owner: None
            }
        );
        assert_eq!(config.paths.claude_dir, Path::new("/home/dev/.claude"));
        assert_eq!(
            config.paths.state_dir,
            Path::new("/home/dev/.local/state/conn")
        );
        assert_eq!(config.files, FileRoots::default());
    }

    /// Mutation: skip `FileRoots::new` (the relative root is accepted).
    #[test]
    fn file_roots_are_read_and_must_be_absolute() {
        let config = parse(&format!(
            "{MINIMAL}\n[files]\nroots = [\"/home/dev\", \"/run/x\"]\n"
        ))
        .unwrap();
        assert_eq!(
            config.files.as_slice(),
            [PathBuf::from("/home/dev"), PathBuf::from("/run/x")]
        );
        assert!(matches!(
            parse(&format!("{MINIMAL}\n[files]\nroots = [\"~/a\"]\n")),
            Err(ConfigError::FileRoot { .. })
        ));
    }

    #[test]
    fn explicit_paths_win() {
        let config = parse(&format!("{MINIMAL}\n[paths]\nstate_dir = \"/var/conn\"\n")).unwrap();
        assert_eq!(config.paths.state_dir, Path::new("/var/conn"));
        assert_eq!(config.paths.claude_dir, Path::new("/home/dev/.claude"));
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        assert!(matches!(
            parse(&format!("{MINIMAL}\n[extra]\nx = 1\n")),
            Err(ConfigError::Invalid { .. })
        ));
        let nested = MINIMAL.replace("prefixes = [\"ABC\"]", "prefixes = [\"ABC\"]\ntypo = 1");
        assert!(matches!(parse(&nested), Err(ConfigError::Invalid { .. })));
    }

    /// No `[slack]` section, or an empty one, is a config without Slack, and an empty file is
    /// a config with neither service; `[reviews]` without Slack is refused.
    /// Mutation: drop the reviews check (the reviews row parses), or make `RawSlack.host`
    /// required again (the empty-section row fails).
    #[test]
    fn slack_is_optional_unless_reviews_need_it() {
        for without in ["", "[slack]\n"] {
            let config = parse(without).unwrap();
            assert_eq!(config.slack, None, "{without:?}");
            assert_eq!(config.link_targets().slack, None);
        }
        assert!(matches!(
            parse("[reviews]\nchannels = [\"C0123ABCD\"]\n"),
            Err(ConfigError::ReviewsWithoutSlack { .. })
        ));
    }

    #[test]
    fn bad_hosts_and_prefixes_fail_at_load() {
        let url_host = MINIMAL.replace("example.slack.com", "https://example.slack.com");
        assert!(matches!(parse(&url_host), Err(ConfigError::Invalid { .. })));
        let lower = MINIMAL.replace("\"ABC\"", "\"abc\"");
        assert!(matches!(parse(&lower), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn an_empty_prefix_list_is_refused() {
        let none = MINIMAL.replace("[\"ABC\"]", "[]");
        assert!(matches!(
            parse(&none),
            Err(ConfigError::NoJiraPrefixes { .. })
        ));
    }

    #[test]
    fn relative_paths_are_refused() {
        let relative = format!("{MINIMAL}\n[paths]\nclaude_dir = \"~/.claude\"\n");
        assert!(matches!(
            parse(&relative),
            Err(ConfigError::RelativePath {
                field: "claude_dir",
                ..
            })
        ));
    }

    #[test]
    fn a_missing_file_names_the_path_and_shows_the_example() {
        let err =
            Config::load_from(Path::new("/nonexistent/conn.toml"), Path::new(HOME)).unwrap_err();
        assert!(matches!(err, ConfigError::Missing { .. }));
        let shown = err.to_string();
        assert!(shown.contains("/nonexistent/conn.toml"));
        assert!(shown.contains("example.atlassian.net"));
    }

    #[test]
    fn the_env_override_replaces_the_default_path() {
        let home = Path::new(HOME);
        assert_eq!(
            config_path(home, None),
            Path::new("/home/dev/.config/conn/config.toml")
        );
        assert_eq!(
            config_path(home, Some("/etc/conn.toml".into())),
            Path::new("/etc/conn.toml")
        );
    }
}
