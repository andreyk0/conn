//! What a parent's id means to a person: its place in the organization, as the names of the
//! folders from the top down (`platform / prod / orchard`), headed by the organization's
//! name when several are searched.
//!
//! Each organization's folders, projects and itself are read in one asset search, each with
//! its display name and its parent, and a path is built for each parent an entitlement sits
//! on.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::ids::Parent;

/// The prefix the asset inventory puts before a resource manager resource's own name.
pub const ASSET_PREFIX: &str = "//cloudresourcemanager.googleapis.com/";

/// The asset types the search asks for.
pub const ASSET_TYPES: &str = "cloudresourcemanager.googleapis.com/Folder,\
    cloudresourcemanager.googleapis.com/Project,\
    cloudresourcemanager.googleapis.com/Organization";

/// GCP nests folders at most ten deep under an organization; a walk longer than this is a loop
/// in the answer, and stops.
const DEPTH_MAX: usize = 12;

/// One resource as the asset search prints it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawNode {
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    parent_full_resource_name: Option<String>,
}

struct Node {
    display: String,
    parent: Option<Parent>,
}

fn parent_of_asset(name: &str) -> Option<Parent> {
    Parent::parse(name.strip_prefix(ASSET_PREFIX)?).ok()
}

/// Where a path starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    /// Under the organization: it is the same on every row, so named only where it is the
    /// parent itself.
    UnderOrganization,
    /// At the organization, on every row: there are several.
    FromOrganization,
}

/// The path of each of `wanted`, down to it as `naming` starts it: folders by name, a project
/// by its name only where that differs from its id, an organization by its own name. A parent
/// the answer does not hold, or whose path is empty, gets none.
pub fn paths(
    nodes: &[RawNode],
    wanted: &BTreeSet<Parent>,
    naming: Naming,
) -> BTreeMap<Parent, String> {
    let tree: BTreeMap<Parent, Node> = nodes
        .iter()
        .filter_map(|raw| {
            let id = parent_of_asset(&raw.name)?;
            let parent = raw
                .parent_full_resource_name
                .as_deref()
                .and_then(parent_of_asset);
            Some((
                id,
                Node {
                    display: raw.display_name.clone(),
                    parent,
                },
            ))
        })
        .collect();
    wanted
        .iter()
        .filter_map(|start| {
            let path = path(&tree, start, naming);
            (!path.is_empty()).then(|| (start.clone(), path))
        })
        .collect()
}

fn path(tree: &BTreeMap<Parent, Node>, start: &Parent, naming: Naming) -> String {
    let mut names = Vec::new();
    let mut at = Some(start.clone());
    for _ in 0..DEPTH_MAX {
        let Some(current) = at.take() else { break };
        let Some(node) = tree.get(&current) else {
            break;
        };
        let shown = match &current {
            Parent::Folder(_) => Some(node.display.as_str()),
            Parent::Project(id) => (node.display != id.as_str()).then_some(node.display.as_str()),
            Parent::Organization(_) => match naming {
                Naming::FromOrganization => Some(node.display.as_str()),
                Naming::UnderOrganization => (&current == start).then_some(node.display.as_str()),
            },
        };
        names.extend(shown.filter(|name| !name.is_empty()));
        at.clone_from(&node.parent);
    }
    names.reverse();
    names.join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, display: &str, parent: Option<&str>) -> RawNode {
        RawNode {
            name: format!("{ASSET_PREFIX}{name}"),
            display_name: display.to_owned(),
            parent_full_resource_name: parent.map(|p| format!("{ASSET_PREFIX}{p}")),
        }
    }

    /// A folder reads as its path from under the organization; a project adds its name only
    /// where it differs from its id; the organization is named only as a parent itself; a
    /// parent the answer lacks has no path; a loop stops.
    /// Mutation: include the organization in every path (red), or keep a project name equal to
    /// its id (red), or drop the depth bound (the loop row hangs).
    #[test]
    fn paths_run_from_under_the_organization_down() {
        let nodes = [
            node("organizations/1", "example.com", None),
            node("folders/10", "platform", Some("organizations/1")),
            node("folders/11", "prod", Some("folders/10")),
            node("folders/12", "orchard", Some("folders/11")),
            node("projects/svc-p-1", "svc-p-1", Some("folders/11")),
            node("projects/named-1", "Report archive", Some("folders/10")),
            node("folders/98", "a", Some("folders/99")),
            node("folders/99", "b", Some("folders/98")),
        ];
        let parent = |text: &str| Parent::parse(text).unwrap();
        let wanted: BTreeSet<Parent> = [
            "folders/12",
            "projects/svc-p-1",
            "projects/named-1",
            "organizations/1",
            "folders/404",
            "folders/98",
        ]
        .into_iter()
        .map(parent)
        .collect();
        let found = paths(&nodes, &wanted, Naming::UnderOrganization);
        assert_eq!(found[&parent("folders/12")], "platform / prod / orchard");
        assert_eq!(found[&parent("projects/svc-p-1")], "platform / prod");
        assert_eq!(
            found[&parent("projects/named-1")],
            "platform / Report archive"
        );
        assert_eq!(found[&parent("organizations/1")], "example.com");
        assert!(!found.contains_key(&parent("folders/404")));
        assert!(found.contains_key(&parent("folders/98")));
    }

    /// With several organizations each path starts at its own, so they sort apart.
    /// Mutation: name the organization only where it is the parent (red).
    #[test]
    fn several_organizations_head_every_path() {
        let nodes = [
            node("organizations/1", "example.com", None),
            node("folders/10", "platform", Some("organizations/1")),
            node("organizations/2", "example.org", None),
            node("projects/named-2", "Legacy", Some("organizations/2")),
        ];
        let parent = |text: &str| Parent::parse(text).unwrap();
        let wanted: BTreeSet<Parent> = ["folders/10", "projects/named-2", "organizations/2"]
            .into_iter()
            .map(parent)
            .collect();
        let found = paths(&nodes, &wanted, Naming::FromOrganization);
        assert_eq!(found[&parent("folders/10")], "example.com / platform");
        assert_eq!(found[&parent("projects/named-2")], "example.org / Legacy");
        assert_eq!(found[&parent("organizations/2")], "example.org");
    }
}
