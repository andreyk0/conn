//! The PAM state against a scripted gcloud: when reads happen, what a request checks before it
//! asks, and how the rows come out. Fixtures are under `tests/fixtures/pam`: the entitlements
//! are masked live answers; the grants were written from the PAM API reference, and a live
//! answer since has the same shape in every field the parser reads.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;
use crate::testutil::{Scratch, at};
use crate::util::runner::{Exit, FakeRunner, Output};

const AUTO: &str = "folders/111/locations/global/entitlements/admin";
const MANUAL: &str = "projects/example-project/locations/global/entitlements/become-network-admin";
const ENTITLEMENTS: &str = include_str!("../../tests/fixtures/pam/entitlements.json");

fn name(text: &str) -> EntitlementName {
    EntitlementName::parse(text).unwrap()
}

fn organizations(ids: &[&str]) -> Organizations {
    Organizations::of(ids.iter().map(|id| NumericId::parse(id).unwrap()).collect()).unwrap()
}

fn config(break_glass: &[&str]) -> PamConfig {
    PamConfig {
        organizations: organizations(&["999"]),
        quota_project: ProjectId::parse("quota-project").unwrap(),
        break_glass: break_glass.iter().map(|text| name(text)).collect(),
    }
}

/// A scripted answer: a successful run that printed `stdout`.
#[allow(clippy::unnecessary_wraps, reason = "the script holds results")]
fn printed(stdout: &str) -> Result<Output, crate::util::runner::RunError> {
    Ok(Output {
        exit: Exit::Success,
        stdout: stdout.as_bytes().to_vec(),
        stderr: String::new(),
    })
}

/// A state whose saved inventory, read at `read_at`, holds the two fixture entitlements.
fn loaded(scratch: &Scratch, read_at: &str, break_glass: &[&str]) -> Pam {
    let stored = format!(
        r#"{{"read_at": "{read_at}", "entitlements": {ENTITLEMENTS}, "unread": [], "organizations": ["999"]}}"#
    );
    std::fs::write(scratch.join(FILE_NAME), stored).unwrap();
    Pam::load(config(break_glass), scratch)
}

fn runner(fake: FakeRunner) -> (Arc<FakeRunner>, Arc<dyn CommandRunner>) {
    let fake = Arc::new(fake);
    let dyn_runner: Arc<dyn CommandRunner> = fake.clone();
    (fake, dyn_runner)
}

/// Nothing runs before the page is viewed; then the inventory is read with the hierarchy,
/// saved and used, and a restart loads it, folder names included, without asking gcloud again.
/// Mutation: drop the `watched` check (gcloud is asked before the view), skip the save (the
/// reloaded state has no inventory), or save no places (the reloaded path is missing).
#[tokio::test]
async fn the_inventory_is_read_only_once_viewed_and_survives_a_restart() {
    let scratch = Scratch::new("pam-inventory");
    let now = at("2026-11-19T10:00:00Z");
    let (fake, dyn_runner) = runner(FakeRunner::new([
        printed(
            r#"[{"name": "//privilegedaccessmanager.googleapis.com/folders/111/locations/global/entitlements/admin"}]"#,
        ),
        printed(
            r#"[{"name": "//cloudresourcemanager.googleapis.com/organizations/999", "displayName": "example.com"},
                {"name": "//cloudresourcemanager.googleapis.com/folders/110", "displayName": "platform",
                 "parentFullResourceName": "//cloudresourcemanager.googleapis.com/organizations/999"},
                {"name": "//cloudresourcemanager.googleapis.com/folders/111", "displayName": "orchard",
                 "parentFullResourceName": "//cloudresourcemanager.googleapis.com/folders/110"}]"#,
        ),
        printed(ENTITLEMENTS),
    ]));
    let pam = Pam::load(config(&[]), &scratch);
    assert!(!pam.read_inventory_if_due(&dyn_runner, now).await);
    assert_eq!(fake.calls(), []);
    pam.viewed(now);
    assert!(pam.read_inventory_if_due(&dyn_runner, now).await);
    assert_eq!(fake.calls().len(), 3);
    assert!(fake.lines()[1].contains("--read-mask=name,displayName,parentFullResourceName"));
    let folder = Parent::parse("folders/111").unwrap();
    let reading = pam.snapshot();
    let rows = reading.rows(now);
    let groups: Vec<(String, Option<&str>, Vec<String>)> = rows
        .regular
        .iter()
        .map(|group| {
            (
                group.parent.to_string(),
                group.place,
                group
                    .rows
                    .iter()
                    .map(|row| row.entitlement.name.id.as_str().to_owned())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            (
                "folders/111".to_owned(),
                Some("platform / orchard"),
                vec!["admin".to_owned()]
            ),
            (
                "projects/example-project".to_owned(),
                None,
                vec!["become-network-admin".to_owned()]
            ),
        ],
        "a parent with a path sorts by it; one without, by its id"
    );

    let restarted = Pam::load(config(&[]), &scratch);
    let months_later = at("2027-03-01T10:00:00Z");
    restarted.viewed(months_later);
    assert!(
        !restarted.snapshot().inventory_due(months_later),
        "a saved inventory is used however old, until asked for again"
    );
    let reloaded = restarted.snapshot().inventory.good.unwrap().value;
    assert_eq!(reloaded.places.of(&folder), Some("platform / orchard"));
    restarted.request_inventory();
    assert!(restarted.snapshot().inventory_due(months_later));
}

/// Several organizations are each searched: one whose search fails is named as unread while
/// the other's entitlements are listed, every path starts at its organization, and a restart
/// with other organizations configured reads the list again.
/// Mutation: fail the whole read on one organization's failure (red), name the organization
/// only where it is the parent (the folder's path loses its head), or skip the organizations
/// check on load (the last row is not due).
#[tokio::test]
async fn several_organizations_are_each_searched_and_a_failure_is_named() {
    let scratch = Scratch::new("pam-orgs");
    let now = at("2026-11-19T10:00:00Z");
    let denied = || {
        Ok(Output {
            exit: Exit::Code(1),
            stdout: Vec::new(),
            stderr: "ERROR: (gcloud.asset.search-all-resources) PERMISSION_DENIED".to_owned(),
        })
    };
    let (fake, dyn_runner) = runner(FakeRunner::new([
        printed(
            r#"[{"name": "//privilegedaccessmanager.googleapis.com/folders/111/locations/global/entitlements/admin"}]"#,
        ),
        denied(),
        printed(
            r#"[{"name": "//cloudresourcemanager.googleapis.com/organizations/888", "displayName": "example.com"},
                {"name": "//cloudresourcemanager.googleapis.com/folders/111", "displayName": "platform",
                 "parentFullResourceName": "//cloudresourcemanager.googleapis.com/organizations/888"}]"#,
        ),
        printed(
            r#"[{"name": "//cloudresourcemanager.googleapis.com/organizations/999", "displayName": "example.org"}]"#,
        ),
        printed(ENTITLEMENTS),
    ]));
    let two = PamConfig {
        organizations: organizations(&["888", "999"]),
        ..config(&[])
    };
    let pam = Pam::load(two, &scratch);
    pam.viewed(now);
    assert!(pam.read_inventory_if_due(&dyn_runner, now).await);
    let lines = fake.lines();
    assert!(lines[0].contains("--scope=organizations/888"), "{lines:?}");
    assert!(lines[1].contains("--scope=organizations/999"), "{lines:?}");
    let reading = pam.snapshot();
    let inventory = reading.inventory.good.as_ref().unwrap();
    assert!(reading.inventory.failure.is_none());
    assert_eq!(
        inventory
            .value
            .unread
            .iter()
            .map(|u| u.parent.as_str())
            .collect::<Vec<_>>(),
        ["organizations/999"]
    );
    assert_eq!(
        inventory
            .value
            .places
            .of(&Parent::parse("folders/111").unwrap()),
        Some("example.com / platform")
    );
    assert_eq!(
        inventory.value.entitlements.len(),
        2,
        "the answering organization's"
    );

    let restarted = Pam::load(config(&[]), &scratch);
    restarted.viewed(now);
    assert!(restarted.snapshot().inventory_due(now));
}

/// A list saved before folder names were read is used, and read again once to get them.
/// Mutation: load such a file without asking for a read (red).
#[test]
fn a_list_saved_without_names_is_read_again_once() {
    let scratch = Scratch::new("pam-legacy");
    let now = at("2026-11-20T10:00:00Z");
    let pam = loaded(&scratch, "2026-11-19T10:00:00Z", &[]);
    pam.viewed(now);
    let reading = pam.snapshot();
    assert!(reading.inventory.good.is_some());
    assert!(reading.inventory_due(now));
}

/// A refresh that fails keeps the old list and is not retried on its own; with no list at all,
/// a failed read is retried after ten minutes, not on every pass.
/// Mutation: retry on every pass (the five-minute row goes due), retry a failed refresh while a
/// list is held (the held-list row goes due), or drop the old list on failure.
#[tokio::test]
async fn a_failed_inventory_read_keeps_the_old_list_and_waits_to_retry() {
    let reauth = || {
        Ok(Output {
            exit: Exit::Code(1),
            stdout: Vec::new(),
            stderr:
                "ERROR: Reauthentication failed. cannot prompt during non-interactive execution."
                    .to_owned(),
        })
    };
    let scratch = Scratch::new("pam-retry");
    let now = at("2026-11-20T10:30:00Z");
    let pam = loaded(&scratch, "2026-11-19T10:00:00Z", &[]);
    pam.viewed(now);
    pam.request_inventory();
    let (_fake, dyn_runner) = runner(FakeRunner::new([reauth()]));
    assert!(pam.read_inventory_if_due(&dyn_runner, now).await);
    let reading = pam.snapshot();
    assert!(reading.inventory.good.is_some());
    assert_eq!(
        reading.inventory.failure.map(|f| f.value),
        Some(PamError::NotSignedIn)
    );
    pam.viewed(at("2026-11-20T11:30:00Z"));
    assert!(!pam.snapshot().inventory_due(at("2026-11-20T11:30:00Z")));

    let empty = Scratch::new("pam-retry-empty");
    let pam = Pam::load(config(&[]), &empty);
    pam.viewed(now);
    let (_fake, dyn_runner) = runner(FakeRunner::new([reauth()]));
    assert!(pam.read_inventory_if_due(&dyn_runner, now).await);
    pam.viewed(at("2026-11-20T10:35:00Z"));
    assert!(!pam.snapshot().inventory_due(at("2026-11-20T10:35:00Z")));
    pam.viewed(at("2026-11-20T10:41:00Z"));
    assert!(pam.snapshot().inventory_due(at("2026-11-20T10:41:00Z")));
}

fn grants_answer(state: &str, created: &str) -> String {
    format!(
        r#"[{{"state": "{state}", "createTime": "{created}", "requestedDuration": "3600s",
              "timeline": {{"events": [{{"eventTime": "{created}", "activated": {{}}}}]}}}}]"#
    )
}

/// A request is refused before gcloud is asked when the length is not offered or a required
/// reason is missing; refused after a fresh check when a grant is already live; and otherwise
/// made, and shown at once.
/// Mutation: skip the fresh check (the live row calls `create`), skip the justification check
/// (the reasonless row calls gcloud), or forget to record the created grant (the row stays
/// free).
#[tokio::test]
async fn a_request_is_checked_before_it_is_made() {
    let scratch = Scratch::new("pam-request");
    let now = Instant::now();
    let pam = loaded(&scratch, &now.to_string(), &[]);
    let manual = name(MANUAL);

    let fake = FakeRunner::new([]);
    assert_eq!(
        pam.request(&fake, &manual, 9 * 3600, Justified::parse("why").unwrap())
            .await,
        Err(RequestError::Length {
            name: manual.clone(),
            secs: 9 * 3600
        })
    );
    assert_eq!(
        pam.request(&fake, &manual, 3600, None).await,
        Err(RequestError::JustificationRequired(manual.clone()))
    );
    assert_eq!(
        pam.request(
            &fake,
            &name("folders/222/locations/global/entitlements/admin"),
            3600,
            None
        )
        .await,
        Err(RequestError::NotRequestable(name(
            "folders/222/locations/global/entitlements/admin"
        )))
    );
    assert_eq!(fake.calls(), []);

    let live = FakeRunner::new([printed(&grants_answer(
        "APPROVAL_AWAITED",
        &now.to_string(),
    ))]);
    assert_eq!(
        pam.request(&live, &manual, 3600, Justified::parse("why").unwrap())
            .await,
        Err(RequestError::AlreadyLive {
            name: manual.clone(),
            state: "Awaiting approval".to_owned()
        })
    );
    assert_eq!(live.calls().len(), 1, "no create after a live grant");

    let free = FakeRunner::new([
        printed(&grants_answer("EXPIRED", "2026-01-01T00:00:00Z")),
        printed(&format!(
            r#"{{"state": "APPROVAL_AWAITED", "createTime": "{now}", "requestedDuration": "7200s"}}"#
        )),
    ]);
    let created = pam
        .request(&free, &manual, 7200, Justified::parse("deploy").unwrap())
        .await
        .unwrap();
    assert_eq!(created.requested.as_secs(), 7200);
    assert!(free.lines()[1].contains("--requested-duration=7200s --justification=deploy"));
    let reading = pam.snapshot();
    let rows = reading.rows(now);
    let row = rows
        .held
        .iter()
        .find(|row| row.entitlement.name == manual)
        .unwrap();
    assert!(matches!(row.status, Status::Live(_)), "{:?}", row.status);
}

/// Every live grant is held, break-glass or not; the rest are grouped by parent, break-glass
/// kept apart; a denial within the day shows, an older one does not.
/// Mutation: leave live rows in their groups (held is empty), or drop the break-glass
/// partition (both groups in `regular`).
#[test]
fn rows_put_live_grants_first_and_break_glass_apart() {
    let scratch = Scratch::new("pam-rows");
    let now = at("2026-11-19T12:00:00Z");
    let pam = loaded(&scratch, "2026-11-19T10:00:00Z", &[MANUAL]);
    let observed = |answer: &str| Observed {
        good: Some(Read {
            at: now,
            value: grant::parse_grants(answer.as_bytes()).unwrap(),
        }),
        failure: None,
    };
    {
        let mut reading = pam.lock();
        reading.grants.insert(
            name(AUTO),
            observed(&grants_answer("ACTIVE", "2026-11-19T11:30:00Z")),
        );
        reading.grants.insert(
            name(MANUAL),
            observed(&grants_answer("DENIED", "2026-11-19T09:00:00Z")),
        );
    }
    let reading = pam.snapshot();
    let rows = reading.rows(now);
    assert_eq!(rows.held.len(), 1);
    assert!(rows.regular.is_empty(), "{:?}", rows.regular);
    let Status::Live(grant) = &rows.held[0].status else {
        panic!("live")
    };
    assert_eq!(grant.ends_at(), Some(at("2026-11-19T12:30:00Z")));
    assert_eq!(rows.break_glass.len(), 1);
    let glass = &rows.break_glass[0].rows[0];
    assert!(matches!(glass.status, Status::Over(_)));
    assert!(glass.status.requestable());

    let next_day = at("2026-11-20T10:00:00Z");
    let reading = pam.snapshot();
    assert_eq!(
        reading.rows(next_day).break_glass[0].rows[0].status,
        Status::Free
    );

    // A break-glass grant held is held like any other.
    pam.lock().grants.insert(
        name(MANUAL),
        observed(&grants_answer("ACTIVE", "2026-11-19T11:45:00Z")),
    );
    let reading = pam.snapshot();
    let rows = reading.rows(now);
    assert_eq!(rows.held.len(), 2);
    assert!(rows.break_glass.is_empty());
    let names: BTreeSet<String> = reading
        .config
        .break_glass
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, BTreeSet::from([MANUAL.to_owned()]));
}
