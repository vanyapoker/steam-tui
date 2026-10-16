//! Launch environment for games served by steam-tui's own Steam API library
//! (`libsteamclient.so`) and per-launch account backend, instead of Valve's
//! client. The game gets a private HOME whose `.steam/sdk64/steamclient.so`
//! is our library; XDG directories keep pointing at the real ones. Native HOME
//! is persistent per account/app so non-XDG saves survive launch cleanup.
use super::{LaunchPlan, Result, failure};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Loader home: temporary for Proton, retained for native game saves.
pub(super) struct Sandbox {
    directory: PathBuf,
    persistent: bool,
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if !self.persistent {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

/// `STEAM_TUI_CLIENT_LIB`, else `libsteamclient.so` beside the executable (where
/// `cargo build` puts it) or in `../lib/steam-tui`.
pub(super) fn client_library() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("STEAM_TUI_CLIENT_LIB") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("libsteamclient.so"));
        candidates.push(dir.join("../lib/steam-tui/libsteamclient.so"));
    }
    candidates
        .into_iter()
        .find_map(|path| fs::canonicalize(path).ok().filter(|p| p.is_file()))
        .ok_or_else(|| {
            failure(
                "The built-in Steam backend library (libsteamclient.so) was not found next to steam-tui; \
                 build it with `cargo build --release -p steam-client`, set STEAM_TUI_CLIENT_LIB, \
                 or launch with Ctrl+Enter to use Valve host",
            )
        })
}

/// Proton plans carry the prefix in `STEAM_COMPAT_DATA_PATH`; its `lsteamclient`
/// loads the host library from `$HOME/.steam/sdk64/steamclient.so`, so the same
/// sandbox works, but Proton still needs the real Steam install path.
fn is_proton(plan: &LaunchPlan) -> bool {
    plan.environment.contains_key("STEAM_COMPAT_DATA_PATH")
}

pub(super) fn apply(
    plan: &mut LaunchPlan,
    app_id: u32,
    socket: &Path,
    library: &Path,
    native_home: Option<&Path>,
) -> Result<Sandbox> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = native_home.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "steam-tui-client-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    });
    DirBuilder::new()
        .recursive(native_home.is_some())
        .mode(0o700)
        .create(&directory)?;
    let sandbox = Sandbox {
        directory,
        persistent: native_home.is_some(),
    };
    for relative in [
        ".steam/sdk64/steamclient.so",
        ".steam/sdk64/steamclient64.so",
        "linux64/steamclient.so",
        "linux64/steamclient64.so",
        "steamclient.so",
    ] {
        let link = sandbox.directory.join(relative);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(target) = fs::read_link(&link) {
            if target == library {
                continue;
            }
            fs::remove_file(&link)?;
        }
        symlink(library, link)?;
    }
    let proton = is_proton(plan);
    if !proton && let Some(base) = plan.environment.get("STEAM_TUI_CLOUD_DOCUMENTS_BASE") {
        let base = Path::new(base);
        if !base.is_absolute() {
            return Err(failure("SteamCloudDocuments base is not absolute"));
        }
        fs::create_dir_all(base)?;
        let link = sandbox.directory.join(".SteamCloud");
        match fs::symlink_metadata(&link) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => symlink(base, link)?,
            Ok(_) if fs::read_link(&link).ok().as_deref() == Some(base) => {}
            _ => {
                return Err(failure(
                    "private HOME has a different .SteamCloud path; preserve and reconcile it before launch",
                ));
            }
        }
    }
    let env = &mut plan.environment;
    // Keep per-user data where it is: only HOME (where the Steam API looks for
    // steamclient.so) is replaced.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for (key, fallback) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            let value = std::env::var_os(key)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback));
            env.entry(key.into())
                .or_insert_with(|| value.to_string_lossy().into_owned());
        }
    }
    let dir = sandbox.directory.to_string_lossy().into_owned();
    env.insert("HOME".into(), dir.clone());
    if !proton {
        env.insert("STEAM_COMPAT_CLIENT_INSTALL_PATH".into(), dir.clone());
    }
    env.insert(
        "STEAM_TUI_BACKEND_SOCKET".into(),
        socket.to_string_lossy().into_owned(),
    );
    for key in ["SteamAppId", "SteamGameId", "SteamOverlayGameId"] {
        env.insert(key.into(), app_id.to_string());
    }
    let library_dir = library.parent().map(|p| p.to_string_lossy().into_owned());
    let socket_dir = socket.parent().map(|p| p.to_string_lossy().into_owned());
    extend_list(
        env,
        "PRESSURE_VESSEL_FILESYSTEMS_RO",
        library_dir.into_iter(),
    );
    let remote_dir = env.get("STEAM_TUI_REMOTE_STORAGE").cloned();
    extend_list(
        env,
        "PRESSURE_VESSEL_FILESYSTEMS_RW",
        [Some(dir), socket_dir, remote_dir].into_iter().flatten(),
    );

    Ok(sandbox)
}

fn extend_list(
    env: &mut std::collections::BTreeMap<String, String>,
    key: &str,
    values: impl Iterator<Item = String>,
) {
    let entry = env.entry(key.into()).or_default();
    for value in values {
        if !entry.split(':').any(|existing| existing == value) {
            if !entry.is_empty() {
                entry.push(':');
            }
            entry.push_str(&value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn plan(proton: bool) -> LaunchPlan {
        let mut environment = BTreeMap::new();
        environment.insert(
            "STEAM_COMPAT_CLIENT_INSTALL_PATH".into(),
            "/real/steam".into(),
        );
        if proton {
            environment.insert("STEAM_COMPAT_DATA_PATH".into(), "/prefix".into());
        }
        LaunchPlan {
            executable: "/tool/run".into(),
            arguments: vec!["game".into()],
            working_dir: "/game".into(),
            environment,
            needs_steam: false,
        }
    }

    #[test]
    fn native_games_get_a_private_client_path_and_proton_keeps_the_real_one() {
        let library = std::env::current_exe().unwrap();
        for proton in [false, true] {
            let mut plan = plan(proton);
            let socket = Path::new("/run/backend/client.sock");
            let sandbox = apply(&mut plan, 480, socket, &library, None).unwrap();
            let env = &plan.environment;
            assert_eq!(env["STEAM_TUI_BACKEND_SOCKET"], "/run/backend/client.sock");
            assert_eq!(env["SteamAppId"], "480");
            assert_eq!(env["HOME"], sandbox.directory.to_string_lossy());
            let client_path = &env["STEAM_COMPAT_CLIENT_INSTALL_PATH"];
            if proton {
                assert_eq!(client_path, "/real/steam");
            } else {
                assert_eq!(client_path, &env["HOME"]);
            }
            assert!(
                sandbox
                    .directory
                    .join(".steam/sdk64/steamclient.so")
                    .exists()
            );
            assert!(env["PRESSURE_VESSEL_FILESYSTEMS_RW"].contains("/run/backend"));
            // Launch setup never adds an insecure flag.
            assert_eq!(
                plan.arguments.iter().filter(|a| *a == "-insecure").count(),
                0
            );
            let directory = sandbox.directory.clone();
            drop(sandbox);
            assert!(!directory.exists(), "sandbox is removed on drop");
        }
    }
    #[test]
    fn native_home_saves_survive_cleanup_and_next_launch() {
        let library = std::env::current_exe().unwrap();
        let home =
            std::env::temp_dir().join(format!("steam-tui-native-home-test-{}", std::process::id()));
        fs::create_dir_all(&home).unwrap();
        let mut plan = plan(false);
        for iteration in 0..2 {
            let sandbox = apply(
                &mut plan,
                30,
                Path::new("/run/test.sock"),
                &library,
                Some(&home),
            )
            .unwrap();
            assert_eq!(plan.environment["HOME"], home.to_string_lossy());
            if iteration == 0 {
                fs::write(home.join("save.dat"), b"progress").unwrap();
            }
            drop(sandbox);
            assert_eq!(fs::read(home.join("save.dat")).unwrap(), b"progress");
        }
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn native_cloud_documents_follow_the_shared_client_root() {
        let base =
            std::env::temp_dir().join(format!("steam-tui-cloud-docs-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let home = base.join("home");
        let cloud_base = base.join("real-home/.SteamCloud");
        let mut plan = plan(false);
        plan.environment.insert(
            "STEAM_TUI_CLOUD_DOCUMENTS_BASE".into(),
            cloud_base.to_string_lossy().into_owned(),
        );
        let library = std::env::current_exe().unwrap();
        let sandbox = apply(
            &mut plan,
            30,
            Path::new("/run/test.sock"),
            &library,
            Some(&home),
        )
        .unwrap();
        fs::create_dir_all(home.join(".SteamCloud/tester/Test Game")).unwrap();
        fs::write(
            home.join(".SteamCloud/tester/Test Game/save.dat"),
            b"progress",
        )
        .unwrap();
        assert_eq!(
            fs::read(cloud_base.join("tester/Test Game/save.dat")).unwrap(),
            b"progress"
        );
        drop(sandbox);
        assert!(cloud_base.join("tester/Test Game/save.dat").exists());
        fs::remove_dir_all(base).unwrap();
    }
}

/// A hosted game uses only Valve's runtime. Reject explicit preload hooks and
/// shim overrides instead of accidentally mixing the two client implementations.
pub(super) fn prepare_hosted(plan: &mut LaunchPlan) -> Result<()> {
    if plan.arguments.iter().any(|argument| {
        argument.eq_ignore_ascii_case("-insecure")
            || argument.to_ascii_lowercase().starts_with("-insecure=")
    }) {
        return Err(failure(
            "Valve-host launches cannot use -insecure; remove it from launch options",
        ));
    }
    if [
        "LD_PRELOAD",
        "LD_AUDIT",
        "STEAM_TUI_BACKEND_SOCKET",
        "STEAM_TUI_CLIENT_LIB",
    ]
    .iter()
    .any(|key| plan.environment.contains_key(*key))
    {
        return Err(failure(
            "Valve-host game launch contains a preload or built-in client override; remove it before secure play",
        ));
    }
    plan.environment.retain(|key, _| {
        key == super::reaper::TOKEN_VAR
            || key == "STEAM_TUI_ACCOUNT_ID"
            || !key.starts_with("STEAM_TUI_")
    });
    plan.environment
        .insert("STEAM_TUI_VALVE_HOSTED".into(), "1".into());
    Ok(())
}
