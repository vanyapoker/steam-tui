use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn document(depots: &str, os: &str) -> Node {
    let text = format!(
        r#""appinfo" {{ "appid" "10" "common" {{ "name" "Fixture" "type" "game" "oslist" "{os}" }} "config" {{ "installdir" "Fixture" }} "depots" {{ "branches" {{ "public" {{ "buildid" "20" }} "beta" {{ "buildid" "10" }} }} {depots} }} }}"#
    );
    installed::text(&text)
        .unwrap()
        .children
        .remove("appinfo")
        .unwrap()
}
fn game() -> InstalledGame {
    InstalledGame {
        app_id: 10,
        name: "Fixture".into(),
        size_bytes: Some(10),
        last_played: None,
        playtime_minutes: None,
        install_dir: PathBuf::from("/fixture/common/Fixture"),
        library_root: PathBuf::from("/fixture"),
        steam_root: PathBuf::from("/fixture"),
        launch_options: None,
        build_id: Some(20),
        branch: "public".into(),
        language: "english".into(),
        platform: Some(InstallPlatform::Linux),
        installed_depots: BTreeMap::from([(101, 1001)]),
    }
}
fn choose(
    doc: Node,
    existing: Option<&InstalledGame>,
    owned: &[u32],
    depots: &[u32],
) -> Result<InstallPlan> {
    metadata::select(
        10,
        existing,
        &owned.iter().copied().collect(),
        &depots.iter().copied().collect(),
        &BTreeMap::from([(10, doc)]),
    )
}

#[test]
fn native_platform_language_arch_and_owned_dlc_boundaries() {
    let doc = document(
        r#"
        "101" { "config" { "oslist" "linux" "osarch" "64" } "manifests" { "public" { "gid" "1001" "size" "90" "download" "50" } } }
        "102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" } } }
        "103" { "config" { "language" "german" } "manifests" { "public" { "gid" "1003" } } }
        "104" { "config" { "oslist" "linux" "osarch" "32" } "manifests" { "public" { "gid" "1004" } } }
        "105" { "dlcappid" "50" "manifests" { "public" { "gid" "1005" } } }
        "106" { "config" { "lowviolence" "1" } "manifests" { "public" { "gid" "1006" } } }
        "107" { "manifests" { "public" "1007" } }
    "#,
        "linux,windows",
    );
    let plan = choose(doc.clone(), None, &[10], &[]).unwrap();
    assert_eq!(plan.platform, InstallPlatform::Linux);
    assert_eq!(
        plan.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
        [101, 107]
    );
    assert_eq!(plan.download_bytes, 50);
    assert_eq!(plan.installed_bytes, 90);
    let plan = choose(doc.clone(), None, &[10, 50], &[]).unwrap();
    assert!(plan.depots.iter().any(|d| d.depot_id == 105));
    // Explicit package depot grants exclude unlicensed content, including DLC.
    let plan = choose(doc, None, &[10], &[101, 105]).unwrap();
    assert_eq!(
        plan.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
        [101, 105]
    );
}

#[test]
fn windows_only_and_installed_windows_are_retained() {
    let windows = document(
        r#""102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" } } }"#,
        "windows",
    );
    assert_eq!(
        choose(windows, None, &[10], &[]).unwrap().platform,
        InstallPlatform::Windows
    );
    let doc = document(
        r#"
        "101" { "config" { "oslist" "linux" } "manifests" { "public" { "gid" "1001" } } }
        "102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" } } }
    "#,
        "linux,windows",
    );
    let mut installed = game();
    installed.platform = Some(InstallPlatform::Windows);
    assert_eq!(
        choose(doc.clone(), Some(&installed), &[10], &[])
            .unwrap()
            .depots[0]
            .depot_id,
        102
    );
    installed.platform = None;
    installed.installed_depots = BTreeMap::from([(102, 1002)]);
    assert_eq!(
        choose(doc.clone(), Some(&installed), &[10], &[])
            .unwrap()
            .platform,
        InstallPlatform::Windows
    );
    installed.installed_depots.clear();
    assert!(choose(doc, Some(&installed), &[10], &[]).is_err());
}

#[test]
fn shared_depot_references_use_source_keys_and_reject_cycles() {
    let doc = document(r#""101" { "depotfromapp" "11" }"#, "linux");
    let source = document(
        r#""101" { "manifests" { "public" { "gid" "1001" } } }"#,
        "linux",
    );
    let mut docs = BTreeMap::from([(10, doc), (11, source)]);
    let plan = metadata::select(
        10,
        None,
        &BTreeSet::from([10]),
        &BTreeSet::from([101]),
        &docs,
    )
    .unwrap();
    assert_eq!(plan.depots[0].source_app_id, 11);
    docs.insert(11, document(r#""101" { "depotfromapp" "10" }"#, "linux"));
    assert!(metadata::select(10, None, &BTreeSet::from([10]), &BTreeSet::new(), &docs).is_err());
}

#[test]
fn branches_are_not_guessed_and_encryption_is_explicit() {
    let doc = document(
        r#""101" { "manifests" { "public" { "gid" "1001" } } "encryptedmanifests" { "beta" { "encrypted_gid" "secret" } } }"#,
        "linux",
    );
    let mut installed = game();
    installed.branch = "beta".into();
    assert!(
        choose(doc.clone(), Some(&installed), &[10], &[])
            .unwrap_err()
            .to_string()
            .contains("encrypted")
    );
    installed.branch = "private".into();
    assert!(
        choose(doc, Some(&installed), &[10], &[])
            .unwrap_err()
            .to_string()
            .contains("private")
    );
}

#[test]
fn rollback_and_same_build_depot_changes_are_updates_not_greater_than() {
    let doc = document(
        r#""101" { "manifests" { "public" { "gid" "1001" } } }"#,
        "linux",
    );
    let plan = choose(doc, None, &[10], &[]).unwrap();
    let mut installed = game();
    assert!(matches!(compare(&installed, &plan), UpdateStatus::UpToDate));
    installed.build_id = Some(30);
    assert!(matches!(
        compare(&installed, &plan),
        UpdateStatus::Available {
            installed_build: 30,
            latest_build: 20
        }
    ));
    installed.build_id = Some(20);
    installed.installed_depots.insert(101, 999);
    assert!(matches!(
        compare(&installed, &plan),
        UpdateStatus::Available {
            installed_build: 20,
            latest_build: 20
        }
    ));
    installed.installed_depots.clear();
    assert!(matches!(compare(&installed, &plan), UpdateStatus::Unknown));
    installed.build_id = None;
    assert!(matches!(compare(&installed, &plan), UpdateStatus::Unknown));
}

#[test]
fn partial_online_rows_and_uninstalled_account_history_are_retained() {
    let app = library::LibraryApp {
        app_id: 10,
        name: None,
        app_type: None,
        document: None,
    };
    let history = BTreeMap::from([(
        10,
        installed::text(r#""LastPlayed" "1700000000" "Playtime" "42""#).unwrap(),
    )]);
    let row = item(
        Some(app.clone()),
        None,
        None,
        UpdateStatus::Unknown,
        Some("Offline".into()),
        &history,
    );
    assert_eq!(row.app_id, 10);
    assert_eq!(row.last_played, Some(1700000000));
    assert_eq!(row.playtime_minutes, Some(42));
    assert!(row.installed.is_none());
    let row = item(
        Some(app),
        Some(game()),
        None,
        UpdateStatus::Unknown,
        Some("Offline".into()),
        &history,
    );
    assert_eq!(row.name, "Fixture");
    assert!(row.installed.is_some());
}

#[test]
fn install_directory_traversal_is_never_accepted() {
    let mut doc = document(
        r#""101" { "manifests" { "public" { "gid" "1001" } } }"#,
        "linux",
    );
    doc.children
        .get_mut("config")
        .unwrap()
        .children
        .get_mut("installdir")
        .unwrap()
        .value = Some("../escape".into());
    assert!(choose(doc, None, &[10], &[]).is_err());
}

#[test]
fn installed_32_bit_architecture_is_not_silently_changed_to_64_bit() {
    let doc = document(
        r#"
        "101" { "config" { "oslist" "linux" "osarch" "32" } "manifests" { "public" { "gid" "1001" } } }
        "102" { "config" { "oslist" "linux" "osarch" "64" } "manifests" { "public" { "gid" "1002" } } }
    "#,
        "linux",
    );
    assert_eq!(
        choose(doc.clone(), None, &[10], &[]).unwrap().depots[0].depot_id,
        102
    );
    assert_eq!(
        choose(doc, Some(&game()), &[10], &[]).unwrap().depots[0].depot_id,
        101
    );
}

#[test]
fn current_branch_and_language_have_exact_manifests_and_build() {
    let doc = document(
        r#"
        "101" { "config" { "language" "german" } "manifests" { "public" { "gid" "1001" } "beta" { "gid" "2001" } } }
        "102" { "config" { "language" "english" } "manifests" { "public" { "gid" "1002" } "beta" { "gid" "2002" } } }
    "#,
        "linux",
    );
    let mut installed = game();
    installed.branch = "beta".into();
    installed.language = "german".into();
    let plan = choose(doc, Some(&installed), &[10], &[]).unwrap();
    assert_eq!(plan.branch, "beta");
    assert_eq!(plan.language, "german");
    assert_eq!(plan.build_id, 10);
    assert_eq!(plan.depots.len(), 1);
    assert_eq!(plan.depots[0].manifest_id, 2001);
    assert!(matches!(
        compare(&installed, &plan),
        UpdateStatus::Available {
            installed_build: 20,
            latest_build: 10
        }
    ));
}

#[test]
fn shared_install_depots_are_skipped_not_blocking() {
    // Steamworks Common Redistributables: its source app 228980 is not even held.
    let doc = document(
        r#"
        "101" { "config" { "oslist" "linux" } "manifests" { "public" { "gid" "1001" "size" "7" } } }
        "228984" { "config" { "oslist" "linux" } "depotfromapp" "228980" "sharedinstall" "1" }
        "228985" { "depotfromapp" "228980" "sharedinstall" "true" }
    "#,
        "linux",
    );
    let plan = choose(doc.clone(), None, &[10], &[]).unwrap();
    assert_eq!(
        plan.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
        [101]
    );
    // Nor are they fetched as shared sources.
    let docs = BTreeMap::from([(10, doc)]);
    assert!(references(&docs, 10, &BTreeSet::from([10]), &BTreeSet::new()).is_empty());

    // Only a redistributable left: the ordinary "no depots" error remains.
    let only_shared = document(
        r#""228984" { "depotfromapp" "228980" "sharedinstall" "1" }"#,
        "linux",
    );
    let error = choose(only_shared, None, &[10], &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("No licensed linux/english depots"),
        "{error}"
    );

    // The same game as a Windows/Proton install.
    let windows = document(
        r#"
        "102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" } } }
        "228984" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "9" } } "sharedinstall" "1" }
    "#,
        "windows",
    );
    let plan = choose(windows, None, &[10], &[]).unwrap();
    assert_eq!(plan.platform, InstallPlatform::Windows);
    assert_eq!(
        plan.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
        [102]
    );
}

#[test]
fn shared_depots_recorded_by_steam_do_not_make_installed_games_updatable() {
    let doc = document(
        r#"
        "101" { "manifests" { "public" { "gid" "1001" } } }
        "228984" { "depotfromapp" "228980" "sharedinstall" "1" }
    "#,
        "linux",
    );
    let plan = choose(doc, None, &[10], &[]).unwrap();
    let mut installed = game();
    installed.installed_depots.insert(228984, 555);
    assert!(matches!(compare(&installed, &plan), UpdateStatus::UpToDate));
    installed.installed_depots.insert(101, 1);
    assert!(matches!(
        compare(&installed, &plan),
        UpdateStatus::Available { .. }
    ));
}

#[test]
fn new_installs_fall_back_to_windows_when_linux_has_no_content() {
    // Linux is advertised but its depot has no manifest; Windows has content.
    let doc = document(
        r#"
        "101" { "config" { "oslist" "linux" } }
        "102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" "size" "5" "download" "3" } } }
        "228984" { "config" { "oslist" "windows" } "depotfromapp" "228980" "sharedinstall" "1" }
    "#,
        "linux,windows",
    );
    let plan = choose(doc.clone(), None, &[10], &[]).unwrap();
    assert_eq!(plan.platform, InstallPlatform::Windows);
    assert_eq!(plan.depots.len(), 1);
    assert_eq!(plan.depots[0].depot_id, 102);
    assert_eq!((plan.download_bytes, plan.installed_bytes), (3, 5));

    // Installed games never switch platform to find content.
    let mut installed = game();
    installed.installed_depots = BTreeMap::from([(101, 1)]);
    assert!(choose(doc, Some(&installed), &[10], &[]).is_err());

    // Linux content is still preferred when it exists.
    let both = document(
        r#"
        "101" { "config" { "oslist" "linux" } "manifests" { "public" { "gid" "1001" } } }
        "102" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "1002" } } }
    "#,
        "linux,windows",
    );
    assert_eq!(
        choose(both, None, &[10], &[]).unwrap().platform,
        InstallPlatform::Linux
    );

    // Both platforms empty: one error naming each.
    let none = document(
        r#""101" { "config" { "oslist" "linux" } } "102" { "config" { "oslist" "windows" } }"#,
        "linux,windows",
    );
    let error = choose(none, None, &[10], &[]).unwrap_err().to_string();
    assert!(
        error.contains("linux") && error.contains("windows"),
        "{error}"
    );
}

#[test]
fn apps_without_any_os_metadata_default_to_windows_depots() {
    let doc = document(r#""101" { "manifests" { "public" { "gid" "1001" } } }"#, "");
    let mut doc = doc;
    doc.children
        .get_mut("common")
        .unwrap()
        .children
        .remove("oslist");
    let plan = choose(doc, None, &[10], &[]).unwrap();
    assert_eq!(plan.platform, InstallPlatform::Windows);
    assert_eq!(plan.depots[0].depot_id, 101);
}

#[test]
fn non_public_branch_skips_absent_depots_unless_installed() {
    let doc = document(
        r#"
        "101" { "manifests" { "public" { "gid" "1001" } "beta" { "gid" "2001" } } }
        "103" { "manifests" { "public" { "gid" "1003" } } }
    "#,
        "linux",
    );
    let mut installed = game();
    installed.branch = "beta".into();
    installed.installed_depots = BTreeMap::from([(101, 2000)]);
    let plan = choose(doc.clone(), Some(&installed), &[10], &[]).unwrap();
    assert_eq!(
        plan.depots.iter().map(|d| d.depot_id).collect::<Vec<_>>(),
        [101]
    );
    installed.installed_depots.insert(103, 1);
    let error = choose(doc, Some(&installed), &[10], &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("Installed depot 103"), "{error}");
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("steam-tui-catalog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn plan_for(app_id: u32, platform: InstallPlatform, manifest: u64) -> InstallPlan {
    InstallPlan {
        app_id,
        name: format!("Game {app_id}"),
        install_dir_name: format!("Game{app_id}"),
        build_id: 20,
        platform,
        branch: "public".into(),
        language: "english".into(),
        depots: vec![DepotSpec {
            depot_id: 101,
            manifest_id: manifest,
            source_app_id: app_id,
        }],
        download_bytes: 5,
        installed_bytes: 9,
    }
}

fn row(app_id: u32, plan: Option<InstallPlan>, update: UpdateStatus) -> cache::Row {
    cache::Row {
        app_id,
        name: format!("Game {app_id}"),
        reason: plan.is_none().then(|| "No plan".to_owned()),
        plan,
        update,
    }
}

#[test]
fn cache_round_trips_and_rejects_corrupt_or_foreign_files() {
    let scratch = Scratch::new("roundtrip");
    let dir = scratch.0.as_path();
    let rows = vec![
        row(
            20,
            Some(plan_for(20, InstallPlatform::Windows, 7)),
            UpdateStatus::Unknown,
        ),
        row(
            10,
            Some(plan_for(10, InstallPlatform::Linux, 6)),
            UpdateStatus::UpToDate,
        ),
        row(30, None, UpdateStatus::Unavailable("No plan".into())),
    ];
    cache::write(dir, "Alice", rows.clone(), &BTreeSet::new()).unwrap();
    let read = cache::read(dir, "alice").unwrap();
    assert_eq!(
        read.iter().map(|r| r.app_id).collect::<Vec<_>>(),
        [10, 20, 30]
    );
    assert_eq!(read[1], rows[0]);
    assert_eq!(
        read[1].plan.as_ref().unwrap().platform,
        InstallPlatform::Windows
    );
    assert!(cache::read(dir, "someone_else").is_none());
    let path = dir.join("catalog-alice.json");
    let good = std::fs::read(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for bad in [
        good[..good.len() / 2].to_vec(),
        b"".to_vec(),
        b"not json".to_vec(),
        String::from_utf8(good.clone())
            .unwrap()
            .replace("\"version\":1", "\"version\":2")
            .into_bytes(),
        String::from_utf8(good.clone())
            .unwrap()
            .replace("\"app_id\":20", "\"app_id\":10")
            .into_bytes(),
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(cache::read(dir, "alice").is_none());
    }
    // A plan for another app than its row is inconsistent.
    let mut wrong = rows.clone();
    wrong[0].plan = Some(plan_for(99, InstallPlatform::Linux, 1));
    cache::write(dir, "alice", wrong, &BTreeSet::new()).unwrap();
    assert!(cache::read(dir, "alice").is_none());
    // Unsafe account names are neither read nor written.
    for account in ["", "../x", "a/b", ".hidden"] {
        assert!(cache::read(dir, account).is_none());
        assert!(cache::write(dir, account, rows.clone(), &BTreeSet::new()).is_err());
    }
    // Only the account file exists; no temporary files are left.
    cache::write(dir, "alice", rows, &BTreeSet::new()).unwrap();
    let names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["catalog-alice.json"]);
}

#[test]
fn cache_keeps_last_known_rows_for_apps_unresolved_this_time() {
    let scratch = Scratch::new("carry");
    let dir = scratch.0.as_path();
    let known = row(
        10,
        Some(plan_for(10, InstallPlatform::Linux, 6)),
        UpdateStatus::Unknown,
    );
    cache::write(dir, "acct", vec![known.clone()], &BTreeSet::new()).unwrap();
    let transient = row(10, None, UpdateStatus::Unknown);
    let fresh = row(
        20,
        Some(plan_for(20, InstallPlatform::Linux, 8)),
        UpdateStatus::Unknown,
    );
    cache::write(
        dir,
        "acct",
        vec![transient.clone(), fresh.clone()],
        &BTreeSet::from([10, 30]),
    )
    .unwrap();
    assert_eq!(cache::read(dir, "acct").unwrap(), [known, fresh]);
    // An app never cached before has nothing to carry; the new row is used.
    let never = row(50, None, UpdateStatus::Unknown);
    cache::write(dir, "acct", vec![never.clone()], &BTreeSet::from([50])).unwrap();
    assert_eq!(cache::read(dir, "acct").unwrap(), [never]);
}

#[test]
fn cached_catalog_merges_fresh_inventory_and_never_trusts_cached_installs() {
    let scratch = Scratch::new("merge");
    let dir = scratch.0.as_path();
    let mut plan = plan_for(10, InstallPlatform::Linux, 1001);
    plan.build_id = 20;
    cache::write(
        dir,
        "acct",
        vec![
            // Cached as installed and outdated, but now uninstalled.
            row(
                30,
                Some(plan_for(30, InstallPlatform::Windows, 3)),
                UpdateStatus::Available {
                    installed_build: 1,
                    latest_build: 2,
                },
            ),
            row(10, Some(plan), UpdateStatus::Unknown),
            row(20, None, UpdateStatus::Unavailable("No plan".into())),
        ],
        &BTreeSet::new(),
    )
    .unwrap();
    let mut local = game();
    local.size_bytes = Some(123);
    let mut other = game();
    other.app_id = 40;
    other.name = "Uncached".into();
    let snapshot = cached_in(
        dir,
        "acct",
        Inventory {
            games: vec![local, other],
            warnings: vec!["inventory warning".into()],
        },
    )
    .unwrap();
    assert_eq!(
        snapshot.items.iter().map(|i| i.app_id).collect::<Vec<_>>(),
        [10, 20, 30, 40]
    );
    let first = &snapshot.items[0];
    assert_eq!(first.size_bytes, Some(123)); // from the fresh inventory
    assert!(first.installed.is_some());
    assert!(first.plan.is_some());
    // Fresh install state against the cached plan: build 20, manifest 1001.
    assert_eq!(first.update, UpdateStatus::UpToDate);
    assert_eq!(snapshot.items[1].plan, None);
    assert_eq!(
        snapshot.items[1].unavailable_reason.as_deref(),
        Some("No plan")
    );
    let uninstalled = &snapshot.items[2];
    assert!(uninstalled.installed.is_none());
    assert_eq!(uninstalled.update, UpdateStatus::Unknown);
    assert_eq!(
        uninstalled.plan.as_ref().unwrap().platform,
        InstallPlatform::Windows
    );
    let local_only = &snapshot.items[3];
    assert_eq!(local_only.name, "Uncached");
    assert_eq!(local_only.update, UpdateStatus::Unknown);
    assert!(local_only.plan.is_none() && local_only.unavailable_reason.is_none());
    assert_eq!(snapshot.warnings[0], "inventory warning");
    assert!(snapshot.warnings.last().unwrap().contains("last known"));
    // No cache for another account.
    assert!(
        cached_in(
            dir,
            "nobody",
            Inventory {
                games: vec![],
                warnings: vec![]
            }
        )
        .is_none()
    );
}
