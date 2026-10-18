use super::*;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "steam-tui-launch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("steamapps/common/game/bin")).unwrap();
        Self(path)
    }
    fn game(&self) -> InstalledGame {
        InstalledGame {
            app_id: 123,
            name: "Fixture".into(),
            size_bytes: None,
            last_played: None,
            playtime_minutes: None,
            install_dir: self.0.join("steamapps/common/game"),
            library_root: self.0.clone(),
            steam_root: self.0.clone(),
            launch_options: None,
            build_id: None,
            branch: "public".into(),
            language: "english".into(),
            platform: None,
            installed_depots: BTreeMap::new(),
        }
    }
    fn metadata(&self, executable: &str) -> Node {
        let path = self.0.join("launch.vdf");
        fs::write(&path, format!(r#""config" {{ "launch" {{ "0" {{ "executable" "{executable}" "arguments" "--label 'two words'" "workingdir" "bin" "config" {{ "oslist" "linux" "osarch" "64" }} }} }} }}"#)).unwrap();
        read_vdf(&path).unwrap()
    }
    fn executable(&self, relative: &str) -> PathBuf {
        let path = self.game().install_dir.join(relative);
        fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn running_apps_follow_processes_and_ignore_unrelated_environment() {
    let fixture = Fixture::new();
    let proc_root = fixture.0.join("proc");
    for (pid, environment) in [
        ("10", b"SteamAppId=3124540\0OTHER=value\0".as_slice()),
        ("11", b"SteamAppId=3124540\0".as_slice()),
        ("12", b"SteamAppId=440\0".as_slice()),
        (
            "13",
            b"OtherSteamAppId=123\0SteamAppId=invalid\0".as_slice(),
        ),
        ("14", b"SteamAppId=0\0".as_slice()),
        ("self", b"SteamAppId=999\0".as_slice()),
    ] {
        fs::create_dir_all(proc_root.join(pid)).unwrap();
        fs::write(proc_root.join(pid).join("environ"), environment).unwrap();
    }
    // A process that disappears during a scan must not abort the scan.
    fs::create_dir_all(proc_root.join("15")).unwrap();
    assert_eq!(
        running_app_ids_in(&proc_root),
        BTreeSet::from([440, 3124540])
    );
    fs::remove_dir_all(proc_root.join("10")).unwrap();
    assert_eq!(
        running_app_ids_in(&proc_root),
        BTreeSet::from([440, 3124540])
    );
    fs::remove_dir_all(proc_root.join("11")).unwrap();
    assert_eq!(running_app_ids_in(&proc_root), BTreeSet::from([440]));
    fs::remove_dir_all(proc_root.join("12")).unwrap();
    assert!(running_app_ids_in(&proc_root).is_empty());
}
#[test]
fn resolves_exact_native_entry_with_safe_arguments_and_environment() {
    let fixture = Fixture::new();
    let executable = fixture.executable("bin/game");
    let mut game = fixture.game();
    game.launch_options = Some("env FOO='two words' %command% --user-option".into());
    let plan = resolve_app(&game, &fixture.metadata("bin/game")).unwrap();
    assert_eq!(plan.executable, executable.canonicalize().unwrap());
    assert_eq!(
        plan.working_dir,
        executable.parent().unwrap().canonicalize().unwrap()
    );
    assert_eq!(plan.arguments, ["--label", "two words", "--user-option"]);
    assert_eq!(
        plan.environment.get("FOO").map(String::as_str),
        Some("two words")
    );
    assert_eq!(
        plan.environment.get("SteamAppId").map(String::as_str),
        Some("123")
    );
}
#[test]
fn linux_launch_entry_stays_native_when_app_supports_linux() {
    let fixture = Fixture::new();
    let executable = fixture.executable("bin/game");
    let path = fixture.0.join("launch.vdf");
    fs::write(&path, r#""common" { "oslist" "windows,linux" } "config" { "launch" { "0" { "executable" "bin/game" "config" { "oslist" "linux" } } } }"#).unwrap();
    let app = read_vdf(&path).unwrap();
    assert!(!uses_proton(&fixture.game(), &app).unwrap());
    assert_eq!(
        resolve_app(&fixture.game(), &app).unwrap().executable,
        executable.canonicalize().unwrap()
    );
}
#[test]
fn refuses_missing_paths_escape_symlink_and_client_helpers() {
    let fixture = Fixture::new();
    let game = fixture.game();
    assert!(resolve_app(&game, &fixture.metadata("bin/missing")).is_err());
    assert!(resolve_app(&game, &fixture.metadata("../outside")).is_err());
    let outside = fixture.0.join("outside");
    fs::write(&outside, "not a game").unwrap();
    symlink(&outside, game.install_dir.join("bin/link")).unwrap();
    assert!(resolve_app(&game, &fixture.metadata("bin/link")).is_err());
    fixture.executable("bin/steam");
    assert!(resolve_app(&game, &fixture.metadata("bin/steam")).is_err());
    let plain = fixture.executable("bin/plain");
    fs::set_permissions(plain, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(resolve_app(&game, &fixture.metadata("bin/plain")).is_err());
}
#[test]
fn parsing_preserves_empty_quotes_and_rejects_shell_constructs() {
    assert_eq!(
        split_arguments("-x 'two words' \"\" C:\\game\\data").unwrap(),
        ["-x", "two words", "", "C:\\game\\data"]
    );
    for input in [
        "; steam",
        "$(steam)",
        "x | steam",
        "x > file",
        "`steam`",
        "'unterminated",
        "a\nb",
    ] {
        assert!(split_arguments(input).is_err(), "{input}");
    }
    let fixture = Fixture::new();
    fixture.executable("bin/game");
    for options in [
        "steam %command%",
        "%command% %command%",
        "x%command%",
        "PATH=/tmp %command%",
        "LD_PRELOAD=x %command%",
    ] {
        let mut game = fixture.game();
        game.launch_options = Some(options.into());
        assert!(
            resolve_app(&game, &fixture.metadata("bin/game")).is_err(),
            "{options}"
        );
    }
}
#[test]
fn rejects_unknown_platform_and_windows_without_real_proton() {
    let fixture = Fixture::new();
    fixture.executable("bin/game");
    let path = fixture.0.join("launch.vdf");
    fs::write(
        &path,
        r#""config" { "launch" { "0" { "executable" "bin/game" } } }"#,
    )
    .unwrap();
    assert!(resolve_app(&fixture.game(), &read_vdf(&path).unwrap()).is_err());
    fs::write(&path, r#""config" { "launch" { "0" { "executable" "bin/game" "config" { "oslist" "windows" } } } }"#).unwrap();
    assert!(resolve_app(&fixture.game(), &read_vdf(&path).unwrap()).is_err());
}
#[test]
fn tool_manifest_requires_real_executable_and_known_tokens() {
    let fixture = Fixture::new();
    let directory = fixture.game().install_dir;
    fixture.executable("bin/proton");
    let path = fixture.0.join("tool.vdf");
    fs::write(
        &path,
        r#""manifest" { "version" "2" "commandline" "/bin/proton %verb%" }"#,
    )
    .unwrap();
    let document = read_vdf(&path).unwrap();
    let command = tool_command(
        &directory,
        document.child("manifest").unwrap(),
        "waitforexitandrun",
    )
    .unwrap();
    assert_eq!(command[1], "waitforexitandrun");
    fs::write(
        &path,
        r#""manifest" { "version" "2" "commandline" "/../outside %verb%" }"#,
    )
    .unwrap();
    assert!(
        tool_command(
            &directory,
            read_vdf(&path).unwrap().child("manifest").unwrap(),
            "waitforexitandrun"
        )
        .is_err()
    );
    fs::write(
        &path,
        r#""manifest" { "version" "2" "commandline" "/bin/proton %unknown%" }"#,
    )
    .unwrap();
    assert!(
        tool_command(
            &directory,
            read_vdf(&path).unwrap().child("manifest").unwrap(),
            "waitforexitandrun"
        )
        .is_err()
    );
}

fn encode_object(node: &Node, bytes: &mut Vec<u8>) {
    for (key, value) in &node.children {
        bytes.push(if value.value.is_some() { 1 } else { 0 });
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(0);
        if let Some(value) = &value.value {
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        } else {
            encode_object(value, bytes);
        }
    }
    bytes.push(8);
}
fn write_cache(root: &Path, documents: &[(u32, Node)]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x07564428u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    for (id, node) in documents {
        let mut data = Vec::new();
        encode_object(node, &mut data);
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&((60 + data.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&[0; 60]);
        bytes.extend(data);
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    fs::create_dir_all(root.join("appcache")).unwrap();
    fs::write(root.join("appcache/appinfo.vdf"), bytes).unwrap();
}
#[test]
fn resolves_cached_windows_entry_through_configured_proton_and_required_runtime() {
    let fixture = Fixture::new();
    let game_exe = fixture.game().install_dir.join("bin/game.exe");
    fs::write(&game_exe, b"Windows game without Unix execute permission").unwrap();
    fs::set_permissions(&game_exe, fs::Permissions::from_mode(0o644)).unwrap();
    let proton = fixture.0.join("steamapps/common/Proton");
    let runtime = fixture.0.join("steamapps/common/Runtime");
    fs::create_dir_all(proton.join("files/bin")).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    for path in [
        proton.join("proton"),
        proton.join("files/bin/wine"),
        proton.join("files/bin/wineserver"),
        runtime.join("_v2-entry-point"),
    ] {
        fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(proton.join("toolmanifest.vdf"), r#""manifest" { "version" "2" "commandline" "/proton %verb%" "compatmanager_layer_name" "proton" "require_tool_appid" "300" }"#).unwrap();
    fs::write(runtime.join("toolmanifest.vdf"), r#""manifest" { "version" "2" "commandline" "/_v2-entry-point --verb=%verb% --" "compatmanager_layer_name" "container-runtime" }"#).unwrap();
    for (id, dir) in [(200, "Proton"), (300, "Runtime")] {
        fs::write(
            fixture.0.join(format!("steamapps/appmanifest_{id}.acf")),
            format!(r#""AppState" {{ "appid" "{id}" "StateFlags" "4" "installdir" "{dir}" }}"#),
        )
        .unwrap();
    }
    let prefix = fixture.0.join("steamapps/compatdata/123");
    fs::create_dir_all(prefix.join("pfx/drive_c")).unwrap();
    fs::write(prefix.join("pfx/system.reg"), "").unwrap();
    fs::create_dir_all(fixture.0.join("config")).unwrap();
    fs::write(fixture.0.join("config/config.vdf"), r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" { "123" { "name" "fixture_proton" } } } } } }"#).unwrap();
    let game_path = fixture.0.join("game.vdf");
    fs::write(&game_path, r#""appinfo" { "appid" "123" "common" { "oslist" "windows" } "config" { "launch" { "0" { "executable" "bin/game.exe" } } } }"#).unwrap();
    let registry_path = fixture.0.join("registry.vdf");
    fs::write(&registry_path, r#""appinfo" { "appid" "891390" "extended" { "compat_tools" { "fixture_proton" { "appid" "200" "aliases" "test-proton" } } } }"#).unwrap();
    write_cache(
        &fixture.0,
        &[
            (123, read_vdf(&game_path).unwrap()),
            (891390, read_vdf(&registry_path).unwrap()),
        ],
    );
    let plan = resolve(&fixture.game()).unwrap();
    assert_eq!(
        plan.executable,
        runtime.join("_v2-entry-point").canonicalize().unwrap()
    );
    assert_eq!(
        plan.environment
            .get("STEAM_COMPAT_DATA_PATH")
            .map(String::as_str),
        prefix.to_str()
    );
    // A game that never ran has no prefix yet: resolve creates Proton's data
    // directory and Proton builds `pfx` on first launch.
    fs::remove_dir_all(&prefix).unwrap();
    resolve(&fixture.game()).unwrap();
    assert!(prefix.is_dir() && !prefix.join("pfx").exists());

    // Far Far West advertises only Windows, but marks its DX11 launch option
    // for Linux hosts. Keep that option while running the Windows build in Proton.
    fs::write(&game_path, r#""appinfo" { "appid" "123" "common" { "oslist" "windows" } "config" { "launch" { "0" { "executable" "bin/game.exe" } "1" { "executable" "bin/game.exe" "arguments" "-dx11" "config" { "oslist" "linux" } } } } }"#).unwrap();
    let document = read_vdf(&game_path).unwrap();
    let app = document.child("appinfo").unwrap();
    // Remove the per-game forced Proton setting to exercise automatic selection.
    fs::remove_file(fixture.0.join("config/config.vdf")).unwrap();
    assert!(uses_proton(&fixture.game(), app).unwrap());
    let linux_plan = resolve_app(&fixture.game(), app).unwrap();
    assert_eq!(linux_plan.executable, plan.executable);
    assert_eq!(
        &linux_plan.arguments[linux_plan.arguments.len() - 2..],
        [game_exe.to_string_lossy().as_ref(), "-dx11"]
    );
    assert_eq!(
        fs::metadata(game_exe).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

fn dependency_tool(fixture: &Fixture, id: u32, required: Option<u32>, layer: &str) -> PathBuf {
    let path = fixture.0.join(format!("steamapps/common/tool-{id}"));
    fs::create_dir_all(path.join("files/bin")).unwrap();
    for binary in ["run", "files/bin/wine", "files/bin/wineserver"] {
        let executable = path.join(binary);
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let required = required
        .map(|id| format!("\"require_tool_appid\" \"{id}\""))
        .unwrap_or_default();
    fs::write(path.join("toolmanifest.vdf"), format!(
        "\"manifest\" {{ \"version\" \"2\" \"commandline\" \"/run %verb% --\" \"compatmanager_layer_name\" \"{layer}\" {required} }}"
    )).unwrap();
    path
}

fn dependency_plan(fixture: &Fixture) -> LaunchPlan {
    LaunchPlan {
        executable: fixture.game().install_dir.join("bin/game.exe"),
        arguments: vec!["--game-option".into()],
        working_dir: fixture.game().install_dir,
        environment: BTreeMap::new(),
        needs_steam: false,
    }
}

fn dependency_registry(source: &str) -> BTreeMap<u32, Node> {
    BTreeMap::from([(891390, crate::installed::text(source).unwrap())])
}

#[test]
fn missing_configured_proton_is_installable_without_valve_appinfo() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("config")).unwrap();
    fs::write(
        fixture.0.join("config/config.vdf"),
        r#"
        "InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" {
            "123" { "name" "selected" } "0" { "name" "other" }
        } } } } }
    "#,
    )
    .unwrap();
    let metadata = dependency_registry(
        r#""extended" { "compat_tools" {
        "selected" { "appid" "200" } "other" { "appid" "201" }
    } }"#,
    );
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &BTreeMap::new(),
        &metadata,
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<MissingTool>().unwrap().app_id, 200);
    assert!(!fixture.0.join("appcache/appinfo.vdf").exists());
}

#[test]
fn fresh_windows_install_selects_registry_stable_alias_not_a_hardcoded_app_id() {
    let fixture = Fixture::new();
    let metadata = dependency_registry(
        r#""extended" { "compat_tools" {
        "future_proton" { "appid" "98765" "aliases" "old-name,proton-stable" }
    } }"#,
    );
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &BTreeMap::new(),
        &metadata,
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<MissingTool>().unwrap().app_id, 98765);
    let missing_registry = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(!missing_registry.is::<MissingTool>());
    assert!(missing_registry.to_string().contains("registry"));
}

#[test]
fn app_mapping_selects_its_missing_proton_instead_of_an_unrelated_installed_one() {
    let fixture = Fixture::new();
    let installed = BTreeMap::from([(200, dependency_tool(&fixture, 200, None, "proton"))]);
    let metadata = dependency_registry(
        r#""extended" {
        "app_mappings" { "123" { "platform" "linux" "tool" "recommended" } }
        "compat_tools" { "recommended" { "appid" "201" } }
    }"#,
    );
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &installed,
        &metadata,
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<MissingTool>().unwrap().app_id, 201);
}

#[test]
fn proton_missing_runtime_uses_the_same_install_retry_error_as_native() {
    let fixture = Fixture::new();
    let installed = BTreeMap::from([(200, dependency_tool(&fixture, 200, Some(300), "proton"))]);
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &installed,
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<MissingTool>().unwrap().app_id, 300);
}

#[test]
fn nested_runtime_chain_keeps_wrapper_order_and_all_tool_paths() {
    let fixture = Fixture::new();
    let installed = BTreeMap::from([
        (200, dependency_tool(&fixture, 200, Some(300), "proton")),
        (
            300,
            dependency_tool(&fixture, 300, Some(400), "container-runtime"),
        ),
        (
            400,
            dependency_tool(&fixture, 400, None, "container-runtime"),
        ),
    ]);
    let plan = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &installed,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(plan.executable, installed[&400].join("run"));
    assert_eq!(
        plan.arguments,
        [
            "run".into(),
            "--".into(),
            installed[&300].join("run").to_string_lossy().into_owned(),
            "run".into(),
            "--".into(),
            installed[&200].join("run").to_string_lossy().into_owned(),
            "run".into(),
            "--".into(),
            fixture
                .game()
                .install_dir
                .join("bin/game.exe")
                .to_string_lossy()
                .into_owned(),
            "--game-option".into(),
        ]
    );
    let paths: Vec<_> =
        std::env::split_paths(&plan.environment["STEAM_COMPAT_TOOL_PATHS"]).collect();
    assert_eq!(
        paths,
        [
            installed[&200].clone(),
            installed[&300].clone(),
            installed[&400].clone()
        ]
    );
}

#[test]
fn runtime_dependencies_reject_cycles_invalid_ids_and_wrong_layers() {
    let fixture = Fixture::new();
    let metadata = dependency_registry(
        r#""extended" {
        "app_mappings" { "123" { "platform" "linux" "tool" "selected" } }
        "compat_tools" { "selected" { "appid" "200" } }
    }"#,
    );
    for (required, layer, expected) in [
        (200, "container-runtime", "cycle"),
        (0, "container-runtime", "Invalid"),
        (300, "proton", "Unsupported runtime layer"),
    ] {
        let installed = BTreeMap::from([
            (
                200,
                dependency_tool(&fixture, 200, Some(required), "proton"),
            ),
            (300, dependency_tool(&fixture, 300, Some(200), layer)),
        ]);
        let error = proton_plan_with_metadata(
            &fixture.game(),
            dependency_plan(&fixture),
            &installed,
            &metadata,
        )
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    let installed = BTreeMap::from([
        (200, dependency_tool(&fixture, 200, Some(300), "proton")),
        (
            300,
            dependency_tool(&fixture, 300, Some(200), "container-runtime"),
        ),
    ]);
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &installed,
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cycle"));
}

#[test]
fn dependency_chain_is_bounded_even_without_a_cycle() {
    let fixture = Fixture::new();
    let installed = (200..233)
        .map(|id| {
            (
                id,
                dependency_tool(
                    &fixture,
                    id,
                    (id < 232).then_some(id + 1),
                    if id == 200 {
                        "proton"
                    } else {
                        "container-runtime"
                    },
                ),
            )
        })
        .collect();
    let error = proton_plan_with_metadata(
        &fixture.game(),
        dependency_plan(&fixture),
        &installed,
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("exceeds 32"));
}

#[test]
fn nonexistent_tool_directory_is_not_an_installed_dependency() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("steamapps/appmanifest_200.acf"),
        r#"
        "AppState" { "appid" "200" "StateFlags" "4" "installdir" "removed-tool" }
    "#,
    )
    .unwrap();
    assert!(!installed_apps(&fixture.game()).contains_key(&200));
}

#[test]
fn native_runtime_bootstrap_and_nested_wrapping_do_not_require_valve_appinfo() {
    let fixture = Fixture::new();
    let app = crate::installed::text(
        r#""config" { "app_mappings" {
        "0" { "platform" "linux" "tool" "SteamLinuxRuntime_sniper" }
    } }"#,
    )
    .unwrap();
    let error = runtime_plan(&fixture.game(), &app, dependency_plan(&fixture)).unwrap_err();
    assert_eq!(error.downcast_ref::<MissingTool>().unwrap().app_id, 1628350);
    for (id, required) in [(1628350, Some(300)), (300, None)] {
        dependency_tool(&fixture, id, required, "container-runtime");
        fs::write(fixture.0.join(format!("steamapps/appmanifest_{id}.acf")), format!(
            "\"AppState\" {{ \"appid\" \"{id}\" \"StateFlags\" \"4\" \"installdir\" \"tool-{id}\" }}"
        )).unwrap();
    }
    let plan = runtime_plan(&fixture.game(), &app, dependency_plan(&fixture)).unwrap();
    assert_eq!(
        plan.executable,
        fixture.0.join("steamapps/common/tool-300/run")
    );
    assert_eq!(
        std::env::split_paths(&plan.environment["STEAM_COMPAT_TOOL_PATHS"]).count(),
        2
    );
    assert!(!fixture.0.join("appcache/appinfo.vdf").exists());
}

#[test]
fn presence_reports_only_selected_accounts_builtin_games() {
    let fixture = Fixture::new();
    let root = fixture.0.join("proc");
    for (pid, env) in [
        (
            "1",
            b"SteamAppId=440\0STEAM_TUI_ACCOUNT_ID=10\0STEAM_TUI_VALVE_HOSTED=1\0".as_slice(),
        ),
        ("2", b"SteamAppId=3124540\0STEAM_TUI_ACCOUNT_ID=11\0"),
        ("3", b"SteamAppId=480\0STEAM_TUI_ACCOUNT_ID=10\0"),
        ("4", b"SteamAppId=570\0"),
    ] {
        fs::create_dir_all(root.join(pid)).unwrap();
        fs::write(root.join(pid).join("environ"), env).unwrap();
    }
    assert_eq!(
        running_app_ids_filtered(&root, Some(10)),
        BTreeSet::from([480])
    );
    assert_eq!(
        running_app_ids_filtered(&root, Some(11)),
        BTreeSet::from([3124540])
    );
    assert_eq!(
        running_app_ids_filtered(&root, None),
        BTreeSet::from([440, 480, 570, 3124540])
    );
}

#[test]
fn vac_games_require_explicit_host_even_outside_the_tui() {
    for id in [440, 570, 730, 1422450] {
        assert!(requires_valve_host(id));
        assert!(
            validate_backend(id, LaunchBackend::BuiltIn)
                .unwrap_err()
                .to_string()
                .contains("Ctrl+Enter")
        );
        validate_backend(id, LaunchBackend::ValveHost).unwrap();
    }
    for id in [480, 3124540, 2321470, 10, 0] {
        assert!(!requires_valve_host(id));
        validate_backend(id, LaunchBackend::BuiltIn).unwrap();
        validate_backend(id, LaunchBackend::ValveHost).unwrap();
    }
    let fixture = Fixture::new();
    fixture.executable("bin/game");
    let path = fixture.0.join("online.vdf");
    fs::write(&path, r#""common" { "category" { "category_8" "1" "category_36" "1" } } "config" { "launch" { "0" { "executable" "bin/game" "config" { "oslist" "linux" } } } }"#).unwrap();
    assert!(
        resolve_app(&fixture.game(), &read_vdf(&path).unwrap())
            .unwrap()
            .needs_steam
    );
}

#[tokio::test]
async fn built_in_vac_launch_is_rejected_before_account_or_host_work() {
    let fixture = Fixture::new();
    let mut game = fixture.game();
    game.app_id = 440;
    let error = run_for_account(&game, None, None, None)
        .await
        .err()
        .expect("must refuse VAC without host");
    assert!(error.to_string().contains("Ctrl+Enter"), "{error}");
}

#[test]
fn native_tf2_selects_valves_sdl2_classic_without_affecting_other_games_or_overrides() {
    let fixture = Fixture::new();
    fixture.executable("bin/game");
    let metadata = fixture.metadata("bin/game");
    let mut game = fixture.game();
    assert!(
        !resolve_app(&game, &metadata)
            .unwrap()
            .environment
            .contains_key("STEAM_COMPAT_RUNTIME_SDL2")
    );
    game.app_id = 440;
    let plan = resolve_app(&game, &metadata).unwrap();
    assert_eq!(plan.environment["STEAM_COMPAT_RUNTIME_SDL2"], "classic");
    assert!(plan.needs_steam);
    assert!(!plan.arguments.iter().any(|arg| arg == "-insecure"));
    game.launch_options = Some("STEAM_COMPAT_RUNTIME_SDL2=compat %command%".into());
    assert_eq!(
        resolve_app(&game, &metadata).unwrap().environment["STEAM_COMPAT_RUNTIME_SDL2"],
        "compat"
    );
}

#[test]
fn missing_execute_bit_is_fixed_for_elf_binaries_only() {
    let fixture = Fixture::new();
    let elf = fixture.0.join("game.x86_64");
    fs::write(&elf, b"\x7fELF....").unwrap();
    fs::set_permissions(&elf, fs::Permissions::from_mode(0o644)).unwrap();
    make_game_executable(&elf).unwrap();
    assert_eq!(
        fs::metadata(&elf).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let text = fixture.0.join("notes.txt");
    fs::write(&text, b"hello").unwrap();
    fs::set_permissions(&text, fs::Permissions::from_mode(0o644)).unwrap();
    make_game_executable(&text).unwrap();
    assert!(require_executable(&text).is_err());
}
