use super::*;
use crate::catalog::{InstallPlan, InstallPlatform};
use crate::content::DepotManifest;
use crate::install::tests::{Temp, depot, file};
use crate::install::{Layout, PlannedFile};
use std::collections::HashMap;

fn request(root: &Path) -> InstallRequest {
    InstallRequest {
        plan: InstallPlan {
            app_id: 30,
            name: "Isolated game".into(),
            install_dir_name: "game".into(),
            build_id: 40,
            platform: InstallPlatform::Linux,
            branch: "public".into(),
            language: "english".into(),
            shared_depots: Default::default(),
            dlc_depots: Default::default(),
            depots: vec![depot()],
            download_bytes: 3,
            installed_bytes: 3,
        },
        destination: root.to_path_buf(),
        steam_root: root.to_path_buf(),
        account: "isolated-unit-account".into(),
        existing: None,
    }
}

fn session(root: &Path) -> Session {
    fs::create_dir_all(root.join("proc-fixture")).unwrap();
    fs::create_dir_all(root.join("steamapps/common/game")).unwrap();
    let state = state_directory(root).unwrap();
    let stage = state.join("30");
    fsafe::private_directory(&stage).unwrap();
    for name in ["content", "metadata", "backup"] {
        fsafe::private_directory(&stage.join(name)).unwrap();
    }
    Session {
        request: request(root),
        library: root.to_path_buf(),
        steamapps: root.join("steamapps"),
        install: root.join("steamapps/common/game"),
        stage,
        owner: 123,
        old_manifest_hash: None,
        dlc_scope: None,
        receipts: Mutex::new(BTreeMap::new()),
        process_fixture: Some(root.join("proc-fixture")),
        _library_lock: Lock::acquire(&state.join("library.lock")).unwrap(),
        _root_lock: Lock::acquire(&root.join("root.lock")).unwrap(),
        _app_lock: Lock::acquire(&root.join("app.lock")).unwrap(),
    }
}

fn journal(root: &Path) -> Journal {
    Journal {
        version: 1,
        app_id: 30,
        steam_root: root.to_path_buf(),
        install_name: "game".into(),
        committed: false,
        operations: vec![Operation {
            target: "common/game/binary".into(),
            source: "content/binary".into(),
            existed: true,
            preserve: false,
            remove: false,
        }],
        created_dirs: Vec::new(),
        registry: None,
        preserved_name: "40-isolated".into(),
    }
}

#[test]
fn removal_journal_recovers_each_crash_window_without_deleting_a_reappeared_file() {
    for moved in [false, true] {
        let temp = Temp::new();
        let s = session(temp.path());
        let original = s.install.join("binary");
        fs::write(&original, b"DLC").unwrap();
        let mut j = journal(temp.path());
        j.operations[0].remove = true;
        save_journal(&s.stage, &j).unwrap();
        if moved {
            fs::rename(&original, s.stage.join("backup/0")).unwrap();
        }
        rollback(temp.path(), &s.stage, &j).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"DLC");
        save_journal(&s.stage, &j).unwrap();
        rollback(temp.path(), &s.stage, &j).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"DLC");
        fs::rename(&original, s.stage.join("backup/0")).unwrap();
        fs::write(&original, b"user replacement").unwrap();
        assert!(rollback(temp.path(), &s.stage, &j).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"user replacement");
    }
}

fn staged(s: &Session, layout: &Layout, contents: &[(&str, &[u8])]) {
    for (name, bytes) in contents {
        let path = s.stage.join("content").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    s.preflight(layout, &CancellationToken::new()).unwrap();
    s.finish_stage(
        layout,
        &CancellationToken::new(),
        &VerifyProgress::default(),
    )
    .unwrap();
}

#[test]
fn ordinary_update_removes_only_unchanged_obsolete_managed_files() {
    let temp = Temp::new();
    let s = session(temp.path());
    let mut config = file("config.ini", b"default");
    config.flags = 1;
    let old = DepotManifest {
        depot_id: 10,
        manifest_id: 19,
        files: vec![
            file("obsolete", b"official"),
            file("changed", b"old"),
            config,
        ],
    };
    fs::write(s.install.join("obsolete"), b"official").unwrap();
    fs::write(s.install.join("changed"), b"modded").unwrap();
    fs::write(s.install.join("config.ini"), b"default").unwrap();
    fs::write(s.install.join("save.dat"), b"save").unwrap();
    let mut layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![file("new", b"new")],
            },
        )],
        vec![old],
    )
    .unwrap();
    let managed = layout.old_depots.values().copied().collect();
    layout.reconcile_removals(&managed);
    staged(&s, &layout, &[("new", b"new")]);
    s.commit(&layout).unwrap();
    assert!(!s.install.join("obsolete").exists());
    assert_eq!(fs::read(s.install.join("changed")).unwrap(), b"modded");
    assert_eq!(fs::read(s.install.join("config.ini")).unwrap(), b"default");
    assert_eq!(fs::read(s.install.join("save.dat")).unwrap(), b"save");
}

#[test]
fn commit_reports_every_operation_and_installs_many_files() {
    let temp = Temp::new();
    fs::create_dir(temp.path().join("steamapps")).unwrap();
    let s = session(temp.path());
    let names: Vec<String> = (0..300).map(|i| format!("d{}/f{i}", i % 7)).collect();
    let files: Vec<_> = names.iter().map(|n| file(n, n.as_bytes())).collect();
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files,
            },
        )],
        vec![],
    )
    .unwrap();
    let content: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), n.as_bytes())).collect();
    staged(&s, &layout, &content);
    let progress = CommitProgress::default();
    s.commit_with_progress(&layout, &progress).unwrap();
    let (done, total) = progress.snapshot();
    // One set-aside step for every operation (including the app manifest) and
    // one move for each of them.
    assert_eq!(done, total);
    assert_eq!(total, 2 * (names.len() as u64 + 1));
    for name in &names {
        assert_eq!(fs::read(s.install.join(name)).unwrap(), name.as_bytes());
    }
}

#[test]
fn dlc_install_and_remove_restore_overlays_preserve_mods_and_leave_base_inode_untouched() {
    let temp = Temp::new();
    fs::create_dir(temp.path().join("steamapps")).unwrap();
    let base = DepotManifest {
        depot_id: 10,
        manifest_id: 20,
        files: vec![
            file("base.bin", b"base"),
            file("shared.bin", b"base shared"),
        ],
    };
    let mut config = file("config.ini", b"default");
    config.flags = 1;
    let addon = DepotManifest {
        depot_id: 11,
        manifest_id: 21,
        files: vec![
            file("shared.bin", b"DLC shared"),
            file("addon.bin", b"DLC"),
            file("mod.bin", b"original"),
            config,
        ],
    };
    let addon_spec = crate::catalog::DepotSpec {
        depot_id: 11,
        manifest_id: 21,
        source_app_id: 30,
    };
    let initial = request(temp.path());
    let s = session(temp.path());
    let layout = Layout::new(vec![(depot(), base.clone())], vec![]).unwrap();
    s.remember_manifest(&depot(), &base).unwrap();
    staged(
        &s,
        &layout,
        &[("base.bin", b"base"), ("shared.bin", b"base shared")],
    );
    let (game, _) = s.commit(&layout).unwrap();
    let base_inode = fs::metadata(game.install_dir.join("base.bin"))
        .unwrap()
        .ino();
    drop(s);
    let mut add = initial.clone();
    add.plan.depots.push(addon_spec.clone());
    add.plan.dlc_depots.insert(11, 200);
    add.existing = Some(game.clone());
    let mut s = session(temp.path());
    validate_dlc_plan(&add, &std::collections::BTreeSet::from([11])).unwrap();
    s.request = add;
    s.dlc_scope = Some(std::collections::BTreeSet::from([11]));
    s.old_manifest_hash = Some(hash_bytes(
        &fs::read(s.steamapps.join("appmanifest_30.acf")).unwrap(),
    ));
    assert!(s.cached_manifest(&depot()).unwrap().is_some());
    s.remember_manifest(&addon_spec, &addon).unwrap();
    let mut layout = Layout::new(
        vec![(depot(), base.clone()), (addon_spec.clone(), addon.clone())],
        vec![base.clone()],
    )
    .unwrap();
    layout
        .restrict_to_dlc(&std::collections::BTreeSet::from([11]))
        .unwrap();
    assert!(
        !layout
            .active
            .as_ref()
            .unwrap()
            .contains(Path::new("base.bin"))
    );
    staged(
        &s,
        &layout,
        &[
            ("shared.bin", b"DLC shared"),
            ("addon.bin", b"DLC"),
            ("mod.bin", b"original"),
            ("config.ini", b"default"),
        ],
    );
    let (game, _) = s.commit(&layout).unwrap();
    assert_eq!(
        fs::metadata(game.install_dir.join("base.bin"))
            .unwrap()
            .ino(),
        base_inode
    );
    assert_eq!(
        fs::read(game.install_dir.join("shared.bin")).unwrap(),
        b"DLC shared"
    );
    fs::write(game.install_dir.join("mod.bin"), b"user mod").unwrap();
    fs::write(game.install_dir.join("config.ini"), b"user setting").unwrap();
    fs::write(game.install_dir.join("save.dat"), b"user save").unwrap();
    drop(s);
    let mut remove = initial;
    remove.existing = Some(game.clone());
    let mut s = session(temp.path());
    validate_dlc_plan(&remove, &std::collections::BTreeSet::from([11])).unwrap();
    s.request = remove;
    s.dlc_scope = Some(std::collections::BTreeSet::from([11]));
    s.old_manifest_hash = Some(hash_bytes(
        &fs::read(s.steamapps.join("appmanifest_30.acf")).unwrap(),
    ));
    assert!(s.cached_manifest(&depot()).unwrap().is_some());
    assert!(s.cached_manifest(&addon_spec).unwrap().is_some());
    let mut layout = Layout::new(vec![(depot(), base.clone())], vec![base, addon]).unwrap();
    layout
        .restrict_to_dlc(&std::collections::BTreeSet::from([11]))
        .unwrap();
    staged(&s, &layout, &[("shared.bin", b"base shared")]);
    let (game, _) = s.commit(&layout).unwrap();
    assert_eq!(
        fs::read(game.install_dir.join("shared.bin")).unwrap(),
        b"base shared"
    );
    assert!(!game.install_dir.join("addon.bin").exists());
    for (name, expected) in [
        ("mod.bin", b"user mod".as_slice()),
        ("config.ini", b"user setting"),
        ("save.dat", b"user save"),
    ] {
        assert_eq!(fs::read(game.install_dir.join(name)).unwrap(), expected);
    }
    assert_eq!(
        fs::metadata(game.install_dir.join("base.bin"))
            .unwrap()
            .ino(),
        base_inode
    );
    assert_eq!(game.installed_depots, BTreeMap::from([(10, 20)]));
}

#[test]
fn cached_manifests_are_bound_to_installed_identity_and_hash_and_reject_tampering() {
    let temp = Temp::new();
    let mut s = session(temp.path());
    let manifest = DepotManifest {
        depot_id: 10,
        manifest_id: 20,
        files: vec![file("base", b"data")],
    };
    s.remember_manifest(&depot(), &manifest).unwrap();
    let layout = Layout::new(vec![(depot(), manifest)], vec![]).unwrap();
    let bytes = encode_vdf(&s.build_manifest(&layout).unwrap()).unwrap();
    fs::write(s.steamapps.join("appmanifest_30.acf"), &bytes).unwrap();
    s.old_manifest_hash = Some(hash_bytes(&bytes));
    assert!(s.cached_manifest(&depot()).unwrap().is_some());
    let mut different = depot();
    different.manifest_id += 1;
    assert!(s.cached_manifest(&different).unwrap().is_none());
    fs::write(s.receipt_path(&depot()).unwrap(), b"{}").unwrap();
    assert!(s.cached_manifest(&depot()).is_err());
    fs::write(s.steamapps.join("appmanifest_30.acf"), b"changed").unwrap();
    assert!(s.cached_manifest(&depot()).is_err());
}

#[test]
fn dlc_plan_and_layout_reject_unrelated_changes_before_mutating_files() {
    let temp = Temp::new();
    let s = session(temp.path());
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![file("base", b"old")],
            },
        )],
        vec![],
    )
    .unwrap();
    let mut request = s.request.clone();
    let mut game = InstalledGame {
        app_id: 30,
        name: "game".into(),
        size_bytes: None,
        last_played: None,
        playtime_minutes: None,
        install_dir: s.install.clone(),
        library_root: s.library.clone(),
        steam_root: s.library.clone(),
        launch_options: None,
        build_id: Some(40),
        branch: "public".into(),
        language: "english".into(),
        platform: Some(InstallPlatform::Linux),
        installed_depots: BTreeMap::from([(10, 20)]),
    };
    request.existing = Some(game.clone());
    let scope = std::collections::BTreeSet::from([11]);
    assert!(validate_dlc_plan(&request, &scope).is_ok());
    request.plan.depots[0].manifest_id += 1;
    assert!(validate_dlc_plan(&request, &scope).is_err());
    request.plan.depots[0].manifest_id -= 1;
    game.build_id = Some(41);
    request.existing = Some(game);
    assert!(validate_dlc_plan(&request, &scope).is_err());
    let mut bad = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![file("base", b"new")],
            },
        )],
        vec![DepotManifest {
            depot_id: 10,
            manifest_id: 20,
            files: vec![file("base", b"old")],
        }],
    )
    .unwrap();
    assert!(bad.restrict_to_dlc(&scope).is_err());
    assert_eq!(layout.files.len(), 1);
    assert!(!s.install.join("base").exists());
}

#[test]
fn rollback_handles_each_rename_crash_window_and_can_repeat() {
    for window in 0..=2 {
        let temp = Temp::new();
        let s = session(temp.path());
        let original = s.install.join("binary");
        let staged = s.stage.join("content/binary");
        fs::write(&original, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        fs::write(s.install.join("save.user"), b"save").unwrap();
        let j = journal(temp.path());
        save_journal(&s.stage, &j).unwrap();
        if window >= 1 {
            fs::rename(&original, s.stage.join("backup/0")).unwrap();
        }
        if window == 2 {
            fs::rename(&staged, &original).unwrap();
        }
        rollback(temp.path(), &s.stage, &j).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"old");
        assert_eq!(fs::read(&staged).unwrap(), b"new");
        assert_eq!(fs::read(s.install.join("save.user")).unwrap(), b"save");
        assert!(!s.stage.join("journal.json").exists());
        // A crash after the last restore but before journal unlink is safe.
        save_journal(&s.stage, &j).unwrap();
        rollback(temp.path(), &s.stage, &j).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"old");
    }
}

#[test]
fn new_install_rollback_retains_resume_and_unmanaged_files() {
    let temp = Temp::new();
    let s = session(temp.path());
    let mut j = journal(temp.path());
    j.operations[0].existed = false;
    j.created_dirs = vec!["common/game".into()];
    fs::write(s.install.join("binary"), b"new").unwrap();
    fs::write(s.install.join("user-mod"), b"mod").unwrap();
    save_journal(&s.stage, &j).unwrap();
    rollback(temp.path(), &s.stage, &j).unwrap();
    assert!(!s.install.join("binary").exists());
    assert_eq!(fs::read(s.stage.join("content/binary")).unwrap(), b"new");
    assert_eq!(fs::read(s.install.join("user-mod")).unwrap(), b"mod");
}

#[test]
fn committed_finalization_keeps_modified_originals_without_reverting_new_files() {
    let temp = Temp::new();
    let s = session(temp.path());
    let mut j = journal(temp.path());
    j.committed = true;
    j.operations[0].preserve = true;
    fs::write(s.install.join("binary"), b"verified-new").unwrap();
    fs::write(s.stage.join("backup/0"), b"local-modded-original").unwrap();
    save_journal(&s.stage, &j).unwrap();
    let preserved = finalize(temp.path(), &s.stage, &j).unwrap().unwrap();
    assert_eq!(
        fs::read(preserved.join("common/game/binary")).unwrap(),
        b"local-modded-original"
    );
    assert_eq!(fs::read(s.install.join("binary")).unwrap(), b"verified-new");
    assert!(!s.stage.join("journal.json").exists());
}

#[test]
fn registry_registration_preserves_unknown_fields_and_rolls_back_exactly() {
    let temp = Temp::new();
    let s = session(temp.path());
    let registry = temp.path().join("steamapps/libraryfolders.vdf");
    let before = format!(
        "\"libraryfolders\" {{ \"custom\" \"keep\" \"0\" {{ \"path\" \"{}\" \"opaque\" {{ \"future\" \"value\" }} }} }}",
        temp.path().display()
    );
    fs::write(&registry, &before).unwrap();
    let mut j = journal(temp.path());
    j.operations.clear();
    register_library(&s.request, &s.stage, &mut j, 3).unwrap();
    let doc = read_vdf(&registry).unwrap();
    assert_eq!(
        doc.at(&["libraryfolders", "0", "opaque"])
            .unwrap()
            .text("future"),
        Some("value")
    );
    assert_eq!(
        doc.child("libraryfolders").unwrap().text("custom"),
        Some("keep")
    );
    assert_eq!(
        doc.at(&["libraryfolders", "0", "apps"]).unwrap().text("30"),
        Some("3")
    );
    rollback(temp.path(), &s.stage, &j).unwrap();
    assert_eq!(fs::read(&registry).unwrap(), before.as_bytes());
}

#[test]
fn external_registry_changes_are_never_overwritten_by_recovery() {
    let temp = Temp::new();
    let s = session(temp.path());
    let mut j = journal(temp.path());
    j.operations.clear();
    register_library(&s.request, &s.stage, &mut j, 3).unwrap();
    let registry = temp.path().join("steamapps/libraryfolders.vdf");
    fs::write(&registry, b"external editor's data").unwrap();
    assert!(rollback(temp.path(), &s.stage, &j).is_err());
    assert_eq!(fs::read(registry).unwrap(), b"external editor's data");
    assert!(s.stage.join("journal.json").exists());
}

#[test]
fn recovery_validates_paths_before_touching_files() {
    let temp = Temp::new();
    let mut j = journal(temp.path());
    j.operations[0].target = "../outside".into();
    assert!(validate_journal(&j, 30).is_err());
    j.operations[0].target = "common/another-app/file".into();
    assert!(validate_journal(&j, 30).is_err());
    j.operations[0].target = "common/game/file".into();
    j.operations[0].source = "../outside".into();
    assert!(validate_journal(&j, 30).is_err());
}

#[test]
fn cancelled_preflight_cannot_change_existing_install() {
    let temp = Temp::new();
    let s = session(temp.path());
    fs::write(s.install.join("binary"), b"original").unwrap();
    let f = file("binary", b"new");
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![f],
            },
        )],
        vec![],
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(s.preflight(&layout, &cancel).is_err());
    assert_eq!(fs::read(s.install.join("binary")).unwrap(), b"original");
    assert!(!s.stage.join("journal.json").exists());
}

#[test]
fn preflight_rejects_unmanaged_file_and_directory_collisions() {
    let temp = Temp::new();
    let s = session(temp.path());
    fs::write(s.install.join("binary"), b"user mod").unwrap();
    let f = file("binary", b"new");
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![f.clone()],
            },
        )],
        vec![],
    )
    .unwrap();
    assert!(
        s.preflight(&layout, &CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("unmanaged")
    );
    fs::remove_file(s.install.join("binary")).unwrap();
    fs::create_dir(s.install.join("binary")).unwrap();
    assert!(
        s.preflight(&layout, &CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("directory")
    );
    assert!(fsafe::require_space(101, 100).is_err());
    assert!(fsafe::require_space(100, 100).is_ok());
}

#[test]
fn unchanged_existing_chunks_can_move_and_staging_can_resume() {
    let temp = Temp::new();
    let s = session(temp.path());
    fs::write(s.install.join("old-file"), b"prefixverified").unwrap();
    let descriptor = file("new-file", b"verified");
    let chunk = &descriptor.chunks[0];
    s.prepare_file(&descriptor).unwrap();
    let old = ReusableChunk {
        path: "old-file".into(),
        offset: 6,
        size: 8,
    };
    assert!(s.reuse_chunk(&descriptor.path, chunk, Some(&old)).unwrap());
    verify(&s, &descriptor).unwrap();
    fs::write(s.install.join("old-file"), b"prefixcorrupt!").unwrap();
    assert!(s.reuse_chunk(&descriptor.path, chunk, None).unwrap());
    assert_eq!(
        fs::read(s.install.join("old-file")).unwrap(),
        b"prefixcorrupt!"
    );
    fs::write(s.stage.join("content/new-file"), b"tampered").unwrap();
    assert!(!s.reuse_chunk(&descriptor.path, chunk, Some(&old)).unwrap());
}

#[test]
fn executable_and_readonly_permissions_are_applied_after_verification() {
    let temp = Temp::new();
    let s = session(temp.path());
    let mut descriptor = file("native", b"binary");
    descriptor.flags = 32 | 8;
    s.prepare_file(&descriptor).unwrap();
    s.write_chunk(&descriptor.path, &descriptor.chunks[0], b"binary")
        .unwrap();
    verify(&s, &descriptor).unwrap();
    assert_eq!(
        fs::metadata(s.stage.join("content/native"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o555
    );
    // Resume reopens a verified readonly stage safely, then restores final mode.
    s.prepare_file(&descriptor).unwrap();
    verify(&s, &descriptor).unwrap();
    assert_eq!(
        fs::metadata(s.stage.join("content/native"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o555
    );
}

#[test]
fn user_config_and_unknown_manifest_keys_survive_manifest_rewrite() {
    let temp = Temp::new();
    let mut s = session(temp.path());
    let old = b"\"AppState\" { \"appid\" \"30\" \"installdir\" \"game\" \"opaque\" \"keep\" \"UserConfig\" { \"custom\" \"keep-too\" } }";
    fs::write(s.steamapps.join("appmanifest_30.acf"), old).unwrap();
    s.old_manifest_hash = Some(hash_bytes(old));
    let layout = Layout {
        files: vec![PlannedFile {
            depot: depot(),
            file: file("file", b"new"),
        }],
        old_files: BTreeMap::new(),
        reuse: HashMap::new(),
        bytes: 3,
        download_bytes: 3,
        install_scripts: false,
        active: None,
        removed: Vec::new(),
        old_depots: BTreeMap::new(),
    };
    let rewritten = s.build_manifest(&layout).unwrap();
    let app = rewritten.child("AppState").unwrap();
    assert_eq!(app.text("opaque"), Some("keep"));
    assert_eq!(app.text("Universe"), Some("1"));
    assert_eq!(
        app.child("UserConfig").unwrap().text("custom"),
        Some("keep-too")
    );
    assert_eq!(
        app.at(&["InstalledDepots", "10"]).unwrap().text("manifest"),
        Some("20")
    );
}

#[test]
fn appmanifest_separates_shared_depots_and_records_only_flagged_install_scripts() {
    let temp = Temp::new();
    let mut session = session(temp.path());
    session.request.plan.shared_depots.insert(228990, 228980);
    let mut script = file("redist/installscript.vdf", b"\"InstallScript\" {}");
    script.flags = 256;
    let script_hash = script
        .sha1
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![script, file("ordinary.vdf", b"not a script")],
            },
        )],
        vec![],
    )
    .unwrap();
    let document = session.build_manifest(&layout).unwrap();
    let app = document.child("AppState").unwrap();
    assert_eq!(
        app.at(&["SharedDepots"]).unwrap().text("228990"),
        Some("228980")
    );
    assert!(app.at(&["InstalledDepots", "228990"]).is_none());
    assert_eq!(
        app.at(&["InstallScripts"]).unwrap().text("10"),
        Some("redist/installscript.vdf")
    );
    let receipt = app.at(&["SteamTuiInstallScripts", "files", "0"]).unwrap();
    assert_eq!(receipt.text("sha1"), Some(script_hash.as_str()));
    assert_eq!(receipt.text("manifest"), Some("20"));
    assert_eq!(
        app.at(&["SteamTuiInstallScripts", "files"])
            .unwrap()
            .children
            .len(),
        1
    );
}

fn verify(s: &Session, descriptor: &DepotFile) -> Result<()> {
    s.verify_file(
        descriptor,
        &CancellationToken::new(),
        &VerifyProgress::default(),
    )
}

/// A file made of several chunks, one per part.
fn chunked(path: &str, parts: &[&[u8]]) -> DepotFile {
    let mut offset = 0;
    let chunks = parts
        .iter()
        .map(|part| {
            let chunk = DepotChunk {
                id: hash_bytes(part),
                offset,
                uncompressed_size: part.len() as u32,
                compressed_size: part.len() as u32,
                adler: crate::content::adler32(part),
            };
            offset += part.len() as u64;
            chunk
        })
        .collect();
    let whole = parts.concat();
    DepotFile {
        path: path.into(),
        size: whole.len() as u64,
        sha1: Some(hash_bytes(&whole)),
        flags: 0,
        chunks,
        link_target: None,
    }
}

fn layout_of(files: Vec<DepotFile>) -> Layout {
    Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files,
            },
        )],
        vec![],
    )
    .unwrap()
}

fn stage_whole(s: &Session, descriptor: &DepotFile, parts: &[&[u8]]) {
    s.prepare_file(descriptor).unwrap();
    for (chunk, part) in descriptor.chunks.iter().zip(parts) {
        s.write_chunk(&descriptor.path, chunk, part).unwrap();
    }
}

fn flip_byte(path: &Path, offset: u64) {
    use std::os::unix::fs::FileExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0u8];
    file.read_exact_at(&mut byte, offset).unwrap();
    file.write_all_at(&[byte[0] ^ 0xff], offset).unwrap();
}

#[test]
fn parallel_verification_accepts_many_files_and_counts_every_byte() {
    let temp = Temp::new();
    let s = session(temp.path());
    let big: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut descriptors = vec![chunked("big.bin", &[&big])];
    let mut contents = vec![big.clone()];
    for index in 0..80 {
        let bytes = format!("small file number {index}").into_bytes();
        descriptors.push(chunked(&format!("dir{}/f{index}", index % 7), &[&bytes]));
        contents.push(bytes);
    }
    for (descriptor, bytes) in descriptors.iter().zip(&contents) {
        stage_whole(&s, descriptor, &[bytes]);
    }
    let total: u64 = descriptors.iter().map(|d| d.size).sum();
    let layout = layout_of(descriptors);
    let progress = VerifyProgress::default();
    let done = std::sync::atomic::AtomicBool::new(false);
    let mut samples = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            s.finish_stage(&layout, &CancellationToken::new(), &progress)
                .unwrap();
            done.store(true, Ordering::SeqCst);
        });
        while !done.load(Ordering::SeqCst) {
            samples.push(progress.snapshot().0);
            std::thread::yield_now();
        }
    });
    assert!(
        samples.windows(2).all(|pair| pair[0] <= pair[1]),
        "monotonic"
    );
    let (bytes, files, _) = progress.snapshot();
    assert_eq!((bytes, files), (total, 81));
    assert_eq!(
        fs::metadata(s.stage.join("content/big.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[test]
fn a_flipped_byte_in_any_staged_file_fails_verification() {
    let temp = Temp::new();
    let s = session(temp.path());
    let parts: [&[u8]; 3] = [b"first chunk..", b"second chunk.", b"third chunk.."];
    let descriptor = chunked("data/large", &parts);
    let small = chunked("small", &[b"tiny"]);
    stage_whole(&s, &descriptor, &parts);
    stage_whole(&s, &small, &[b"tiny"]);
    let layout = layout_of(vec![descriptor, small]);
    s.finish_stage(
        &layout,
        &CancellationToken::new(),
        &VerifyProgress::default(),
    )
    .unwrap();
    // Restage and corrupt one byte in the middle chunk, keeping the length.
    s.prepare_file(&layout.files[0].file).unwrap();
    flip_byte(&s.stage.join("content/data/large"), 15);
    let error = s
        .finish_stage(
            &layout,
            &CancellationToken::new(),
            &VerifyProgress::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("SHA1 mismatch"), "{error}");
}

#[test]
fn cancellation_stops_verification() {
    let temp = Temp::new();
    let s = session(temp.path());
    let descriptor = chunked("file", &[b"content"]);
    stage_whole(&s, &descriptor, &[b"content"]);
    let layout = layout_of(vec![descriptor]);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = s
        .finish_stage(&layout, &cancelled, &VerifyProgress::default())
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
}

#[test]
fn cancellation_is_checked_between_buffers_of_one_large_file() {
    let temp = Temp::new();
    let path = temp.path().join("large");
    fs::write(&path, vec![7u8; 3 * 1024 * 1024]).unwrap();
    let cancel = CancellationToken::new();
    let buffers = std::cell::Cell::new(0u32);
    let error = fsafe::hash_file_progress(&path, 3 * 1024 * 1024, &cancel, &|_| {
        buffers.set(buffers.get() + 1);
        cancel.cancel();
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(buffers.get(), 1, "stopped after the first buffer");
    assert_eq!(
        fsafe::hash_file_progress(&path, 5, &CancellationToken::new(), &|_| {}).unwrap(),
        None,
        "a length mismatch is reported without hashing"
    );
}

fn scan(
    s: &Session,
    file: &DepotFile,
    range: std::ops::Range<usize>,
    reuse: &HashMap<[u8; 20], ReusableChunk>,
    token: &CancellationToken,
) -> Result<Vec<bool>> {
    Ok(s.reuse_scan(&[(file, range)], reuse, token)?.remove(0))
}

#[test]
fn resume_scan_keeps_valid_chunks_and_rejects_corrupt_partial_or_missing_ones() {
    let temp = Temp::new();
    let s = session(temp.path());
    let parts: [&[u8]; 5] = [
        b"alpha-----",
        b"bravo-----",
        b"charlie---",
        b"delta-----",
        b"echo------",
    ];
    let descriptor = chunked("resumed", &parts);
    let none = HashMap::new();
    let token = CancellationToken::new();
    s.prepare_file(&descriptor).unwrap();
    // Only chunks 0 and 2 were written before the "crash".
    for index in [0, 2] {
        s.write_chunk(&descriptor.path, &descriptor.chunks[index], parts[index])
            .unwrap();
    }
    assert_eq!(
        scan(&s, &descriptor, 0..5, &none, &token).unwrap(),
        [true, false, true, false, false]
    );
    // Sub-ranges are indexed from their start.
    assert_eq!(
        scan(&s, &descriptor, 1..4, &none, &token).unwrap(),
        [false, true, false]
    );
    // A torn/corrupted write is not trusted: it is detected and refetched.
    flip_byte(&s.stage.join("content/resumed"), 21);
    assert_eq!(
        scan(&s, &descriptor, 0..3, &none, &token).unwrap(),
        [true, false, false]
    );
    assert!(
        !s.reuse_chunk(&descriptor.path, &descriptor.chunks[2], None)
            .unwrap()
    );
    // A truncated stage (shorter than the chunk's end) is invalid too.
    let staged = s.stage.join("content/resumed");
    fs::OpenOptions::new()
        .write(true)
        .open(&staged)
        .unwrap()
        .set_len(12)
        .unwrap();
    assert_eq!(
        scan(&s, &descriptor, 0..2, &none, &token).unwrap(),
        [true, false]
    );
    // The previous install supplies unchanged chunks, found through the reuse map.
    s.prepare_file(&descriptor).unwrap();
    fs::write(s.install.join("old"), b"xxxxdelta-----yy").unwrap();
    let mut reuse = HashMap::new();
    reuse.insert(
        descriptor.chunks[3].id,
        ReusableChunk {
            path: "old".into(),
            offset: 4,
            size: 10,
        },
    );
    assert_eq!(
        scan(&s, &descriptor, 3..5, &reuse, &token).unwrap(),
        [true, false]
    );
    assert!(
        s.reuse_chunk(&descriptor.path, &descriptor.chunks[3], None)
            .unwrap()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(scan(&s, &descriptor, 0..5, &none, &cancelled).is_err());
}

#[test]
fn unsynced_chunk_writes_still_complete_only_after_whole_file_verification() {
    let temp = Temp::new();
    let s = session(temp.path());
    let parts: [&[u8]; 2] = [b"one-", b"two"];
    let descriptor = chunked("late", &parts);
    stage_whole(&s, &descriptor, &parts);
    // Completeness is decided by the whole-file hash, not by chunk writes.
    flip_byte(&s.stage.join("content/late"), 5);
    assert!(
        verify(&s, &descriptor)
            .unwrap_err()
            .to_string()
            .contains("mismatch")
    );
    s.write_chunk(&descriptor.path, &descriptor.chunks[1], parts[1])
        .unwrap();
    verify(&s, &descriptor).unwrap();
}

#[test]
fn new_manifest_declares_public_universe_for_steam_discovery() {
    let temp = Temp::new();
    let s = session(temp.path());
    let layout = Layout::new(
        vec![(
            depot(),
            DepotManifest {
                depot_id: 10,
                manifest_id: 20,
                files: vec![file("file", b"new")],
            },
        )],
        vec![],
    )
    .unwrap();
    let bytes = encode_vdf(&s.build_manifest(&layout).unwrap()).unwrap();
    let path = temp.path().join("result.acf");
    fs::write(&path, &bytes).unwrap();
    let parsed = crate::installed::read_vdf(&path).unwrap();
    let app = parsed.child("AppState").unwrap();
    assert_eq!(app.text("Universe"), Some("1"));
    assert_eq!(app.text("appid"), Some("30"));
    assert_eq!(app.text("StateFlags"), Some("4"));
}

#[test]
fn delta_source_is_hashed_at_read_time_and_never_follows_external_links() {
    let temp = Temp::new();
    let s = session(temp.path());
    let source = ReusableChunk {
        path: "old".into(),
        offset: 1,
        size: 3,
    };
    fs::write(s.install.join("old"), b"_old").unwrap();
    assert_eq!(
        s.patch_source(&source, &hash_bytes(b"old"))
            .unwrap()
            .unwrap(),
        b"old"
    );
    fs::write(s.install.join("old"), b"_mod").unwrap();
    assert!(
        s.patch_source(&source, &hash_bytes(b"old"))
            .unwrap()
            .is_none()
    );
    fs::remove_file(s.install.join("old")).unwrap();
    symlink(temp.path().join("outside"), s.install.join("old")).unwrap();
    assert!(s.patch_source(&source, &hash_bytes(b"old")).is_err());
}
