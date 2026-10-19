use super::*;
use crate::install::tests::Temp;
#[test]
fn shader_buckets_reject_extra_paths_links_and_wrong_sizes() {
    let file = DepotFile {
        path: ARCHIVE.into(),
        size: 100,
        sha1: Some([1; 20]),
        flags: 0,
        chunks: vec![],
        link_target: None,
    };
    let mut manifest = DepotManifest {
        depot_id: 42,
        manifest_id: 1000,
        files: vec![file.clone()],
    };
    assert!(archive(&manifest, 100).is_ok());
    assert!(archive(&manifest, 99).is_err());
    manifest.files[0].link_target = Some("elsewhere".into());
    assert!(archive(&manifest, 100).is_err());
    manifest.files[0] = file.clone();
    manifest.files.push(DepotFile {
        path: "game.exe".into(),
        ..file
    });
    assert!(archive(&manifest, 100).is_err());
}
#[test]
fn atomic_shader_state_and_prune_keep_current_and_unknown_content() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let root = root(&q, 42).unwrap();
    let state = State {
        manifest: 8,
        processed: Some("a".repeat(40)),
        ..Default::default()
    };
    save(&root, &state).unwrap();
    assert_eq!(load(&root).unwrap().manifest, 8);
    for name in ["8.foz", "7.foz", "6.part", "my-save", "unknown.foz"] {
        fs::write(root.join(name), "keep").unwrap();
    }
    fs::create_dir(root.join(format!("replay-{}", "a".repeat(40)))).unwrap();
    fs::create_dir(root.join(format!("replay-{}", "b".repeat(40)))).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("9.foz")).unwrap();
    prune_locked(&root, &load(&root).unwrap(), true).unwrap();
    assert!(root.join("8.foz").exists());
    assert!(!root.join("7.foz").exists());
    assert!(!root.join("6.part").exists());
    assert!(root.join("my-save").exists());
    assert!(root.join("unknown.foz").exists());
    assert!(outside.exists());
    assert!(root.join(format!("replay-{}", "a".repeat(40))).exists());
    assert!(!root.join(format!("replay-{}", "b".repeat(40))).exists());
    fs::remove_file(root.join("state.json")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("state.json")).unwrap();
    assert!(load(&root).is_err());
}
#[test]
fn shader_jobs_are_separate_and_obey_preferences() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let intent = crate::updates::Intent {
        app_id: 42,
        steam_root: "/tmp/library".into(),
        destination: "/tmp/library".into(),
        dependency: None,
        revision: None,
        shader: false,
    };
    let game = q.enqueue(intent.clone(), false).unwrap();
    let mut shader = intent;
    shader.shader = true;
    let shader_id = q.enqueue(shader, false).unwrap();
    assert_ne!(game, shader_id);
    let mut settings = q.settings().unwrap();
    settings.shaders_enabled = false;
    q.save_settings(settings).unwrap();
    q.control(game, crate::updates::JobState::Cancelled)
        .unwrap();
    assert!(q.ready(0, false).unwrap().is_none());
}
#[tokio::test]
async fn helper_skip_kills_and_reaps_without_marking_processed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let helper = temp.path().join("fake-replay");
    fs::write(&helper, "#!/bin/sh\nexec sleep 60\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let directory = root(&q, 42).unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let environment = Default::default();
    let run = replay::run_helper(
        &helper,
        Path::new("unused.foz"),
        &directory,
        &environment,
        &q,
        42,
        &cancel,
        None,
    );
    let skip = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    };
    let (result, _) = tokio::join!(run, skip);
    assert!(result.unwrap_err().to_string().contains("Skipped"));
    assert!(load(&directory).unwrap().processed.is_none());
}
#[tokio::test]
async fn helper_failure_is_not_success_and_context_is_not_saved() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let directory = root(&q, 42).unwrap();
    let result = replay::run_helper(
        Path::new("/bin/false"),
        Path::new("unused.foz"),
        &directory,
        &Default::default(),
        &q,
        42,
        &CancellationToken::new(),
        None,
    )
    .await;
    assert!(result.is_err());
    assert!(load(&directory).unwrap().processed.is_none());
}

#[test]
fn replay_identity_invalidates_for_gpu_driver_game_tool_and_helper_changes() {
    let temp = Temp::new();
    let tool = temp.path().join("tool");
    let helper = temp.path().join("helper");
    fs::write(&tool, "version 1").unwrap();
    fs::write(&helper, "helper 1").unwrap();
    let mut game = InstalledGame {
        app_id: 42,
        name: "test".into(),
        size_bytes: None,
        last_played: None,
        playtime_minutes: None,
        install_dir: temp.path().into(),
        library_root: temp.path().into(),
        steam_root: temp.path().into(),
        launch_options: None,
        build_id: Some(1),
        branch: "public".into(),
        language: "english".into(),
        platform: None,
        installed_depots: Default::default(),
    };
    let mut plan = crate::launch::LaunchPlan {
        executable: tool.clone(),
        arguments: vec![],
        working_dir: temp.path().into(),
        environment: Default::default(),
        needs_steam: false,
    };
    let first = replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap();
    assert_eq!(
        first,
        replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap()
    );
    assert_ne!(
        first,
        replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver2").unwrap()
    );
    fs::write(&helper, "helper 2").unwrap();
    let helper_change = replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap();
    assert_ne!(first, helper_change);
    fs::write(&tool, "version 2").unwrap();
    let tool_change = replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap();
    assert_ne!(helper_change, tool_change);
    game.build_id = Some(2);
    let build_change = replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap();
    assert_ne!(tool_change, build_change);
    plan.environment
        .insert("STEAM_COMPAT_TOOL_PATHS".into(), "/different/proton".into());
    assert_ne!(
        build_change,
        replay::context_with_gpu(&game, &plan, &helper, b"GPU+driver1").unwrap()
    );
}
#[test]
fn archive_budget_counts_managed_files_and_does_not_follow_links() {
    let temp = Temp::new();
    let q = Queue::in_directory(temp.path(), "account").unwrap();
    let root = root(&q, 42).unwrap();
    fs::write(root.join("1.foz"), vec![0; 100]).unwrap();
    let staging = root.join("2.part");
    fs::write(&staging, vec![0; 40]).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, vec![0; 200]).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("3.foz")).unwrap();
    let parent = root.parent().unwrap();
    assert!(reserve(parent, &staging, 80, 179).is_err());
    assert!(reserve(parent, &staging, 80, 180).is_ok());
    assert_eq!(fs::metadata(outside).unwrap().len(), 200);
}
