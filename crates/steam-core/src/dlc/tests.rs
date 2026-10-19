use super::*;
use crate::install::tests::Temp;
use std::fs;
use std::os::unix::fs::symlink;

fn game(root: &Path) -> InstalledGame {
    InstalledGame {
        app_id: 30,
        name: "Parent".into(),
        size_bytes: None,
        last_played: None,
        playtime_minutes: None,
        install_dir: root.join("steamapps/common/game"),
        library_root: root.into(),
        steam_root: root.into(),
        launch_options: None,
        build_id: Some(40),
        branch: "public".into(),
        language: "english".into(),
        platform: Some(InstallPlatform::Linux),
        installed_depots: BTreeMap::from([(10, 100), (200, 201)]),
    }
}

fn document() -> Node {
    installed::text(r#"
        "appid" "30" "common" { "oslist" "linux,windows" }
        "extended" { "listofdlc" "20,21,22,23,24,25,26,20" }
        "depots" {
            "10" { "config" { "oslist" "linux" "osarch" "64" } "manifests" { "public" { "gid" "100" } } }
            "200" { "dlcappid" "20" "config" { "oslist" "linux" "osarch" "64" } "manifests" { "public" { "gid" "201" } } }
            "202" { "dlcappid" "20" "config" { "language" "english" } "manifests" { "public" { "gid" "203" } } }
            "204" { "dlcappid" "20" "config" { "oslist" "windows" } "manifests" { "public" { "gid" "205" } } }
            "21" { "NoContentDLC" "1" }
            "240" { "dlcappid" "24" "config" { "oslist" "windows" } "manifests" { "public" { "gid" "241" } } }
            "250" { "dlcappid" "25" "config" { "osarch" "32" } "manifests" { "public" { "gid" "251" } } }
            "260" { "dlcappid" "26" "config" { "language" "german" } "manifests" { "public" { "gid" "261" } } }
            "branches" { "public" { "buildid" "40" } }
        }
    "#).unwrap()
}

fn children() -> BTreeMap<u32, Node> {
    BTreeMap::from([(22, installed::text(r#""common" { "type" "DLC" "name" "Bundled DLC" } "extended" { "DLCAvailableOnStore" "1" }"#).unwrap())])
}

#[test]
fn dlc_state_requires_complete_applicable_content_and_keeps_selection_separate() {
    let mut game = game(Path::new("/library"));
    let entries = entries(&game, &document(), &children()).unwrap();
    assert_eq!(entries.len(), 7);
    let snapshot = snapshot(&game, &entries, &Selection::default());
    assert_eq!(snapshot.dlc[0].depots, [200, 202]);
    assert!(!snapshot.dlc[0].installed); // A partial DLC is not installed.
    game.installed_depots.insert(202, 203);
    assert!(super::snapshot(&game, &entries, &Selection::default()).dlc[0].installed);
    let mut selection = Selection::default();
    selection.disabled.insert(20);
    assert!(!super::snapshot(&game, &entries, &selection).dlc[0].installed);
    assert!(snapshot.dlc[1].installed); // Explicit NoContentDLC.
    assert!(snapshot.dlc[2].installed); // Described DLC bundled in base files.
    assert!(snapshot.dlc[2].available);
    for index in 3..7 {
        assert!(!snapshot.dlc[index].installed);
    }
    assert_eq!(snapshot.build_id, 40);
    assert_eq!(snapshot.depots, [10, 200]);
}

#[test]
fn missing_child_metadata_is_not_a_contentless_entitlement_and_store_flag_is_not_ownership() {
    let game = game(Path::new("/library"));
    let mut children = children();
    children.get_mut(&22).unwrap().children.remove("extended");
    let entries = entries(&game, &document(), &children).unwrap();
    let snapshot = snapshot(&game, &entries, &Selection::default());
    assert!(snapshot.dlc[2].installed);
    assert!(!snapshot.dlc[2].available); // Hidden store page does not mean uninstalled.
    assert!(!snapshot.dlc[3].installed); // Missing metadata is unknown, not bundled.
}

#[test]
fn disabled_depots_are_removed_even_when_a_package_directly_grants_them() {
    let mut doc = document();
    let selection = Selection {
        disabled: BTreeSet::from([20]),
        ..Default::default()
    };
    filter(&mut doc, &selection);
    assert!(doc.at(&["depots", "200"]).is_none());
    assert!(doc.at(&["depots", "204"]).is_none());
    assert!(doc.at(&["depots", "10"]).is_some());
    assert_eq!(
        depot_scope(&document(), 20),
        BTreeSet::from([200, 202, 204])
    );
}

#[test]
fn selections_and_pending_jobs_survive_restart_without_crossing_accounts_or_games() {
    let temp = Temp::new();
    fs::create_dir(temp.path().join("steamapps")).unwrap();
    Selection::update(temp.path(), 123, 30, |s| {
        s.disabled.insert(20);
        s.pending.insert(20, false);
    })
    .unwrap();
    let read = Selection::read(temp.path(), 123, 30).unwrap();
    assert!(read.disabled.contains(&20));
    assert_eq!(read.pending.get(&20), Some(&false));
    assert!(
        Selection::read(temp.path(), 124, 30)
            .unwrap()
            .disabled
            .is_empty()
    );
    assert!(
        Selection::read(temp.path(), 123, 31)
            .unwrap()
            .disabled
            .is_empty()
    );
    assert!(Selection::read(temp.path(), 0, 30).is_err());
    assert!(
        Selection::update(temp.path(), 123, 30, |s| {
            s.disabled.insert(0);
        })
        .is_err()
    );
    assert!(
        !Selection::read(temp.path(), 123, 30)
            .unwrap()
            .disabled
            .contains(&0)
    );
}

#[test]
fn selection_files_reject_symlink_traversal_and_corrupt_data() {
    let temp = Temp::new();
    fs::create_dir(temp.path().join("steamapps")).unwrap();
    Selection::update(temp.path(), 123, 30, |_| {}).unwrap();
    let directory = temp.path().join("steamapps/.steam-tui/dlc/123");
    fs::write(directory.join("30.json"), b"not json").unwrap();
    assert!(Selection::read(temp.path(), 123, 30).is_err());
    fs::remove_file(directory.join("30.json")).unwrap();
    symlink("/tmp/not-a-dlc-settings-file", directory.join("30.json")).unwrap();
    assert!(Selection::read(temp.path(), 123, 30).is_err());
}
