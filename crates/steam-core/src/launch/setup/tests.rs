use super::*;

#[test]
fn windows_command_lines_preserve_paths_empty_arguments_and_quotes() {
    assert_eq!(
        script::windows_arguments(r#"/q /log "C:\space path\setup.log" "" "a\"b""#).unwrap(),
        ["/q", "/log", "C:\\space path\\setup.log", "", "a\"b"]
    );
    assert!(script::windows_arguments("\"unterminated").is_err());
}

#[test]
fn real_common_redist_shape_is_a_prefix_scoped_versioned_process() {
    let recipe = script::parse(r#""installscript" { "Run Process" {
        "x86 14.51.36247.0" { "hasrunkey" "HKEY_LOCAL_MACHINE\\Software\\Valve\\Steam\\Apps\\CommonRedist\\vcredist\\2022"
            "process 1" "%INSTALLDIR%\\_CommonRedist\\vcredist\\2022\\Microsoft Visual C++ 2022 x86.cmd" "nocleanup" "1" }
        "x64 14.51.36247.0" { "process 1" "%INSTALLDIR%\\redist\\x64.exe" "Requirement_OS" { "Is64BitWindows" "1" } }
    } } "kvsignatures" { "installscript" "signature" }"#, Path::new("/shared"), "english").unwrap();
    assert_eq!(recipe.processes.len(), 2);
    let x64 = recipe
        .processes
        .iter()
        .find(|p| p.name.starts_with("x64"))
        .unwrap();
    assert_eq!(x64.requirement.win64, Some(true));
    assert!(recipe.processes.iter().any(|p| p.has_run_key.is_some()));
}

#[test]
fn unknown_directives_fail_before_execution_and_language_registry_values_are_selected() {
    assert!(
        script::parse(
            r#""InstallScript" { "Run Process" { "x" { "process 1" "x.exe" "unknown" "1" } } }"#,
            Path::new("/game"),
            "english"
        )
        .is_err()
    );
    assert!(
        script::parse(
            r#""InstallScript" { "Firewall" { "x" "x.exe" } }"#,
            Path::new("/game"),
            "english"
        )
        .is_err()
    );
    let recipe = script::parse(r#""InstallScript" { "Registry" { "HKEY_CURRENT_USER\\Software\\Example" {
        "string" { "english" { "Name" "English" } "french" { "Name" "French" } "Path" "%INSTALLDIR%" }
        "dword" { "Version" "1" }
    } } }"#, Path::new("/game"), "french").unwrap();
    assert_eq!(recipe.registry.len(), 3);
    assert!(recipe.registry.iter().any(|v| v.value == "French"));
    assert!(!recipe.registry.iter().any(|v| v.value == "English"));
}

#[test]
fn shared_requirements_are_grouped_and_filtered_by_the_game_target() {
    let app = crate::installed::text(
        r#""depots" {
        "10" { "sharedinstall" "1" "depotfromapp" "228980" "config" { "oslist" "windows" } }
        "11" { "sharedinstall" "true" "depotfromapp" "228980" "config" { "language" "french" } }
        "12" { "depotfromapp" "999" }
    }"#,
    )
    .unwrap();
    assert_eq!(
        shared::requirements(&app, InstallPlatform::Windows, "english").unwrap(),
        BTreeMap::from([(228980, BTreeSet::from([10]))])
    );
    assert!(
        shared::requirements(&app, InstallPlatform::Linux, "english")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn script_values_expand_in_windows_space_without_requiring_steam() {
    assert_eq!(
        script::expand("%INSTALLDIR%\\redist", Path::new("/games/Example")).unwrap(),
        "Z:\\games\\Example\\redist"
    );
    assert_eq!(
        script::expand("%WinDir%\\system32", Path::new("/game")).unwrap(),
        "C:\\windows\\system32"
    );
    assert!(script::expand("%STEAMPATH%", Path::new("/game")).is_err());
    assert!(script::expand("%missing", Path::new("/game")).is_err());
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "steam-tui-setup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(directory.join("steamapps/common/game")).unwrap();
        fs::create_dir_all(directory.join("prefix/pfx")).unwrap();
        fs::create_dir_all(directory.join("logs")).unwrap();
        Self(directory)
    }
    fn game(&self) -> InstalledGame {
        InstalledGame {
            app_id: 30,
            name: "Fixture".into(),
            size_bytes: None,
            last_played: None,
            playtime_minutes: None,
            install_dir: self.0.join("steamapps/common/game"),
            library_root: self.0.clone(),
            steam_root: self.0.clone(),
            launch_options: None,
            build_id: Some(40),
            branch: "public".into(),
            language: "english".into(),
            platform: Some(InstallPlatform::Windows),
            installed_depots: BTreeMap::from([(10, 20)]),
        }
    }
    fn fake_proton(&self) -> LaunchPlan {
        let tool = self.0.join("tool");
        fs::create_dir(&tool).unwrap();
        fs::write(tool.join("toolmanifest.vdf"), r#""manifest" { "version" "2" "commandline" "/fake-proton %verb%" "compatmanager_layer_name" "proton" }"#).unwrap();
        // This test tool never runs Wine, Steam, installers, or a GUI. It models
        // reg.exe exit codes/markers and one process failure using shell builtins.
        fs::write(
            tool.join("fake-proton"),
            r#"#!/bin/sh
shift
printf '%s\n' "$*" >> "$SETUP_TEST_TRACE"
case "$1" in
*reg.exe)
    case "$2" in
    query)
        if [ -f "$SETUP_TEST_MARKER" ]; then
            printf 'Fixture REG_DWORD 0x1\n'
            exit 0
        fi
        exit 1
        ;;
    add)
        case "$3" in *HasRun*) : > "$SETUP_TEST_MARKER" ;; esac
        exit 0
        ;;
    esac
    ;;
*)
    printf 'install\n' >> "$SETUP_TEST_CALLS"
    if [ "$SETUP_TEST_BLOCK" = "1" ]; then
        while :; do sleep 1; done
    fi
    if [ -f "$SETUP_TEST_FAIL" ]; then exit 7; fi
    exit 0
    ;;
esac
exit 9
"#,
        )
        .unwrap();
        fs::set_permissions(tool.join("fake-proton"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut environment = BTreeMap::from([
            (
                "STEAM_COMPAT_DATA_PATH".into(),
                self.0.join("prefix").to_string_lossy().into_owned(),
            ),
            (
                "STEAM_COMPAT_TOOL_PATHS".into(),
                tool.to_string_lossy().into_owned(),
            ),
        ]);
        for (key, file) in [
            ("SETUP_TEST_MARKER", "prefix/pfx/marker"),
            ("SETUP_TEST_TRACE", "trace"),
            ("SETUP_TEST_CALLS", "calls"),
            ("SETUP_TEST_FAIL", "fail"),
        ] {
            environment.insert(key.into(), self.0.join(file).to_string_lossy().into_owned());
        }
        LaunchPlan {
            executable: "unused".into(),
            arguments: Vec::new(),
            working_dir: self.game().install_dir,
            environment,
            needs_steam: false,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn setup_failure_never_marks_success_retry_skips_only_after_success_and_prefix_reset_repeats() {
    let fixture = Fixture::new();
    let game = fixture.game();
    fs::write(game.install_dir.join("setup.exe"), "fake installer payload").unwrap();
    let plan = fixture.fake_proton();
    let recipe = script::parse(
        r#""InstallScript" { "Run Process" { "Fixture" {
        "HasRunKey" "HKEY_LOCAL_MACHINE\\Software\\HasRun"
        "Process 1" "%INSTALLDIR%\\setup.exe" "Command 1" "/quiet /norestart"
    } } }"#,
        &game.install_dir,
        "english",
    )
    .unwrap();
    let recipes = [recipe];
    fs::write(fixture.0.join("fail"), "fail once").unwrap();
    let error = runner::execute(
        &game,
        &plan,
        &recipes,
        &fixture.0.join("logs"),
        CancellationToken::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("code 7"));
    assert!(!fixture.0.join("prefix/pfx/marker").exists());
    fs::remove_file(fixture.0.join("fail")).unwrap();
    runner::execute(
        &game,
        &plan,
        &recipes,
        &fixture.0.join("logs"),
        CancellationToken::new(),
    )
    .unwrap();
    runner::execute(
        &game,
        &plan,
        &recipes,
        &fixture.0.join("logs"),
        CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    fs::remove_dir_all(fixture.0.join("prefix/pfx")).unwrap();
    fs::create_dir(fixture.0.join("prefix/pfx")).unwrap();
    runner::execute(
        &game,
        &plan,
        &recipes,
        &fixture.0.join("logs"),
        CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.join("calls"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}

#[test]
fn a_modified_or_escaping_script_receipt_is_rejected_before_parsing() {
    let fixture = Fixture::new();
    let game = fixture.game();
    let content = b"\"InstallScript\" {}";
    fs::write(game.install_dir.join("setup.vdf"), content).unwrap();
    let mut receipt = Receipt {
        path: "setup.vdf".into(),
        sha1: hex(&Sha1::digest(content)),
        depot: 10,
        manifest: 20,
    };
    assert!(load_recipe(&game, &receipt, "english").is_ok());
    fs::write(game.install_dir.join("setup.vdf"), b"changed").unwrap();
    assert!(
        load_recipe(&game, &receipt, "english")
            .unwrap_err()
            .to_string()
            .contains("modified")
    );
    receipt.path = "../outside.vdf".into();
    assert!(load_recipe(&game, &receipt, "english").is_err());
    receipt.path = "setup.vdf".into();
    receipt.manifest = 21;
    assert!(validate_receipt(&game, &receipt).is_err());
}

#[test]
fn unsupported_payload_is_validated_before_any_registry_changes() {
    let fixture = Fixture::new();
    let game = fixture.game();
    let plan = fixture.fake_proton();
    fs::write(game.install_dir.join("payload.sh"), "never execute").unwrap();
    let recipe = script::parse(
        r#""InstallScript" {
        "Registry" { "HKEY_CURRENT_USER\\Software\\Example" { "dword" { "Version" "1" } } }
        "Run Process" { "x" { "Process 1" "%INSTALLDIR%\\payload.sh" } }
    }"#,
        &game.install_dir,
        "english",
    )
    .unwrap();
    assert!(
        runner::execute(
            &game,
            &plan,
            &[recipe],
            &fixture.0.join("logs"),
            CancellationToken::new()
        )
        .is_err()
    );
    assert!(!fixture.0.join("trace").exists());
}

#[tokio::test]
async fn legacy_script_cache_is_bound_to_the_installed_manifest_and_validated_paths() {
    let fixture = Fixture::new();
    let game = fixture.game();
    let directory = fixture.0.join("logs");
    fs::write(
        game.library_root.join("steamapps/appmanifest_30.acf"),
        "\"AppState\" { \"appid\" \"30\" }",
    )
    .unwrap();
    let identity =
        serde_json::to_vec(&(game.app_id, &game.branch, &game.installed_depots)).unwrap();
    let cache = directory.join(format!("scripts-{}.json", hex(&Sha1::digest(identity))));
    let receipt = Receipt {
        path: "../escape".into(),
        sha1: "00".repeat(20),
        depot: 10,
        manifest: 20,
    };
    fs::write(&cache, serde_json::to_vec(&vec![receipt]).unwrap()).unwrap();
    assert!(discover(&game, None, &directory).await.is_err());
    fs::write(&cache, "[]").unwrap();
    assert!(discover(&game, None, &directory).await.unwrap().is_empty());
    let mut changed = game.clone();
    changed.installed_depots.insert(10, 21);
    assert!(
        discover(&changed, None, &directory)
            .await
            .unwrap_err()
            .to_string()
            .contains("Connect once")
    );
}

#[tokio::test]
async fn cancelling_setup_stops_an_active_fake_installer_without_marking_success() {
    let fixture = Fixture::new();
    let game = fixture.game();
    fs::write(game.install_dir.join("setup.exe"), "fake payload").unwrap();
    let mut plan = fixture.fake_proton();
    plan.environment
        .insert("SETUP_TEST_BLOCK".into(), "1".into());
    let recipe = script::parse(
        r#""InstallScript" { "Run Process" { "Fixture" {
        "HasRunKey" "HKEY_LOCAL_MACHINE\\Software\\HasRun" "Process 1" "%INSTALLDIR%\\setup.exe"
    } } }"#,
        &game.install_dir,
        "english",
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let directory = fixture.0.join("logs");
    let worker = tokio::task::spawn_blocking(move || {
        runner::execute(&game, &plan, &[recipe], &directory, token)
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !fixture.0.join("calls").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(!fixture.0.join("prefix/pfx/marker").exists());
}

#[test]
fn dropping_a_launch_cancels_its_setup_worker() {
    let token = CancellationToken::new();
    let guard = CancelOnDrop(token.clone());
    drop(guard);
    assert!(token.is_cancelled());
}
