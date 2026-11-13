//! Local-only game removal. A roll-forward marker (`uninstall.json`) in the
//! app's stage directory makes every step idempotent; `recover_locked` resumes
//! an interrupted removal. Proton prefixes, workshop content and preserved
//! originals are deliberately kept.

use super::filesystem::{self as fsafe, Lock};
use super::transaction::{bounded_read, encode_vdf, recover_locked, state_directory};
use super::{Result, failure};
use crate::installed::{InstalledGame, read_vdf};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u32,
    app_id: u32,
    steam_root: PathBuf,
    install_name: String,
}

pub(super) fn run(game: &InstalledGame) -> Result<()> {
    run_in(game, Path::new("/proc"))
}

fn run_in(game: &InstalledGame, processes: &Path) -> Result<()> {
    let library = game.library_root.canonicalize()?;
    let steam_root = game.steam_root.canonicalize()?;
    fsafe::checked(&steam_root, Path::new("steamapps"), true)?;

    let name = game
        .install_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| failure("Install directory name is not valid UTF-8"))?;
    fsafe::relative(Path::new(name))?;
    if Path::new(name).components().count() != 1 {
        return Err(failure("Install directory must be a single path component"));
    }
    let parent = game
        .install_dir
        .parent()
        .ok_or_else(|| failure("Install directory has no parent"))?
        .canonicalize()?;
    if parent != library.join("steamapps/common") {
        return Err(failure(
            "Install directory is not inside the library's steamapps/common",
        ));
    }

    let state = state_directory(&library)?;
    let _library_lock = Lock::acquire(&state.join("library.lock"))?;
    recover_locked(&library, &state)?;
    let _root_lock = Lock::acquire(&steam_root.join("steamapps/.steam-tui-install.lock"))?;
    let _app_lock = Lock::acquire(&state.join(format!("{}.lock", game.app_id)))?;

    fsafe::check_processes_in(&steam_root, game.app_id, &game.install_dir, false, processes)?;

    let steamapps = library.join("steamapps");
    fsafe::checked(&steamapps, &Path::new("common").join(name), true)?;
    let manifest_name = format!("appmanifest_{}.acf", game.app_id);
    let manifest = fsafe::checked(&steamapps, Path::new(&manifest_name), true)?;
    if !fsafe::exists(&manifest)? {
        return Err(failure("Appmanifest is missing; nothing was removed"));
    }
    let doc = read_vdf(&manifest)?;
    let identifies = doc.child("AppState").is_some_and(|app| {
        app.text("appid").and_then(|v| v.parse::<u32>().ok()) == Some(game.app_id)
            && app.text("installdir") == Some(name)
    });
    if !identifies {
        return Err(failure(
            "Appmanifest does not identify this installation; nothing was removed",
        ));
    }

    let stage = state.join(game.app_id.to_string());
    fsafe::private_directory(&stage)?;
    let marker = Marker {
        version: 1,
        app_id: game.app_id,
        steam_root,
        install_name: name.into(),
    };
    fsafe::atomic_write(&stage.join("uninstall.json"), &serde_json::to_vec(&marker)?)?;
    fsafe::sync_dir(&stage)?;
    finish(&library, &marker.steam_root, &stage, &marker)
}

pub(super) fn resume(library: &Path, stage: &Path, app_id: u32) -> Result<()> {
    let bytes = bounded_read(&stage.join("uninstall.json"), 1024 * 1024)?;
    let marker: Marker =
        serde_json::from_slice(&bytes).map_err(|_| failure("Invalid uninstall marker"))?;
    let name = Path::new(&marker.install_name);
    if marker.version != 1
        || marker.app_id != app_id
        || !marker.steam_root.is_absolute()
        || fsafe::relative(name).is_err()
        || name.components().count() != 1
    {
        return Err(failure("Invalid uninstall marker"));
    }
    let root = marker.steam_root.canonicalize()?;
    fsafe::checked(&root, Path::new("steamapps"), true)?;
    let _root_lock = Lock::acquire(&root.join("steamapps/.steam-tui-install.lock"))?;
    fsafe::refuse_running(
        &root,
        app_id,
        &library.join("steamapps/common").join(&marker.install_name),
    )?;
    finish(library, &root, stage, &marker)
}

fn finish(library: &Path, steam_root: &Path, stage: &Path, marker: &Marker) -> Result<()> {
    let app_id = marker.app_id;
    let steamapps = library.join("steamapps");
    let install = steamapps.join("common").join(&marker.install_name);
    let removing = stage.join("removing");

    if fsafe::exists(&install)? {
        if fsafe::exists(&removing)? {
            return Err(failure(
                "Removal staging and install directory both exist; refusing destructive recovery",
            ));
        }
        fs::rename(&install, &removing)?;
        fsafe::sync_filesystem(stage)?;
    }

    let manifest = steamapps.join(format!("appmanifest_{app_id}.acf"));
    if fsafe::exists(&manifest)? {
        fs::remove_file(&manifest)?;
        fsafe::sync_dir(&steamapps)?;
    }

    unregister_library(steam_root, library, app_id)?;

    if fsafe::exists(&removing)? {
        fs::remove_dir_all(&removing)?;
    }

    let shadercache = Path::new("shadercache").join(app_id.to_string());
    if fsafe::exists(&steamapps.join(&shadercache))? {
        let path = fsafe::checked(&steamapps, &shadercache, true)?;
        fs::remove_dir_all(path)?;
    }

    for dir in ["content", "metadata", "backup"] {
        let path = stage.join(dir);
        if fsafe::exists(&path)? {
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(failure("Unsafe previous installer stage"));
            }
            fs::remove_dir_all(&path)?;
        }
    }
    for file in ["plan.key", "registry.before"] {
        let path = stage.join(file);
        if fsafe::exists(&path)? {
            fs::remove_file(&path)?;
        }
    }
    fs::remove_file(stage.join("uninstall.json"))?;
    fsafe::sync_dir(stage)?;
    match fs::remove_dir(stage) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn unregister_library(steam_root: &Path, library: &Path, app_id: u32) -> Result<()> {
    let primary = steam_root.join("steamapps/libraryfolders.vdf");
    let relative = if primary.exists() || !steam_root.join("config/libraryfolders.vdf").exists() {
        PathBuf::from("steamapps/libraryfolders.vdf")
    } else {
        PathBuf::from("config/libraryfolders.vdf")
    };
    let path = fsafe::checked(steam_root, &relative, true)?;
    if !fsafe::exists(&path)? {
        return Ok(());
    }
    let mut doc = read_vdf(&path)?;
    let Some(folders) = doc.children.get_mut("libraryfolders") else {
        return Ok(());
    };
    let key = app_id.to_string();
    let mut removed = false;
    for (index, entry) in folders.children.iter_mut() {
        if index.parse::<u32>().is_err() {
            continue;
        }
        let candidate = entry.value.as_deref().or_else(|| entry.text("path"));
        let matches = candidate
            .and_then(|c| Path::new(c).canonicalize().ok())
            .is_some_and(|c| c == library);
        if matches && let Some(apps) = entry.children.get_mut("apps") {
            removed |= apps.children.remove(&key).is_some();
        }
    }
    if !removed {
        return Ok(());
    }
    fsafe::atomic_write(&path, &encode_vdf(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::tests::Temp;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn setup(root: &Path) -> InstalledGame {
        write(&root.join("steamapps/common/game/bin"), "b");
        write(&root.join("steamapps/common/game/data/x"), "x");
        write(
            &root.join("steamapps/appmanifest_30.acf"),
            "\"AppState\" { \"appid\" \"30\" \"installdir\" \"game\" }",
        );
        write(
            &root.join("steamapps/libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" \"apps\" {{ \"30\" \"3\" \"31\" \"4\" }} }} }}",
                root.display()
            ),
        );
        write(&root.join("steamapps/compatdata/30/pfx/save"), "s");
        write(&root.join("steamapps/shadercache/30/c"), "c");
        write(&root.join("steamapps/.steam-tui/30/content/partial"), "p");
        write(&root.join("steamapps/.steam-tui/30/plan.key"), "k");
        write(&root.join("steamapps/.steam-tui/preserved/30/old"), "o");
        fs::create_dir_all(root.join("proc-fixture")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for dir in ["steamapps/.steam-tui", "steamapps/.steam-tui/30"] {
            fs::set_permissions(root.join(dir), fs::Permissions::from_mode(0o700)).unwrap();
        }
        InstalledGame {
            app_id: 30,
            name: "Game".into(),
            size_bytes: None,
            last_played: None,
            playtime_minutes: None,
            install_dir: root.join("steamapps/common/game"),
            library_root: root.to_path_buf(),
            steam_root: root.to_path_buf(),
            launch_options: None,
            build_id: None,
            branch: "public".into(),
            language: "english".into(),
            platform: None,
            installed_depots: Default::default(),
        }
    }

    fn assert_removed(root: &Path) {
        assert!(!root.join("steamapps/common/game").exists());
        assert!(!root.join("steamapps/appmanifest_30.acf").exists());
        assert!(!root.join("steamapps/shadercache/30").exists());
        assert!(!root.join("steamapps/.steam-tui/30").exists());
        let vdf = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")).unwrap();
        assert!(!vdf.contains("\"30\""), "{vdf}");
        assert!(vdf.contains("\"31\""), "{vdf}");
        assert!(root.join("steamapps/compatdata/30/pfx/save").exists());
        assert!(root.join("steamapps/.steam-tui/preserved/30/old").exists());
    }

    #[test]
    fn uninstall_removes_game_manifest_registry_entry_stage_and_shadercache_but_keeps_prefix_and_preserved()
     {
        let temp = Temp::new();
        let root = temp.path().canonicalize().unwrap();
        let game = setup(&root);
        run_in(&game, &root.join("proc-fixture")).unwrap();
        assert_removed(&root);
    }

    #[test]
    fn uninstall_refuses_while_the_game_runs_and_changes_nothing() {
        let temp = Temp::new();
        let root = temp.path().canonicalize().unwrap();
        let game = setup(&root);
        write(&root.join("proc-fixture/42/environ"), "SteamAppId=30\0");
        let before = fs::read(root.join("steamapps/libraryfolders.vdf")).unwrap();
        assert!(run_in(&game, &root.join("proc-fixture")).is_err());
        assert!(root.join("steamapps/common/game/bin").exists());
        assert!(root.join("steamapps/appmanifest_30.acf").exists());
        assert_eq!(
            fs::read(root.join("steamapps/libraryfolders.vdf")).unwrap(),
            before
        );
    }

    #[test]
    fn uninstall_refuses_a_manifest_that_names_another_install_dir() {
        let temp = Temp::new();
        let root = temp.path().canonicalize().unwrap();
        let game = setup(&root);
        write(
            &root.join("steamapps/appmanifest_30.acf"),
            "\"AppState\" { \"appid\" \"30\" \"installdir\" \"other\" }",
        );
        assert!(run_in(&game, &root.join("proc-fixture")).is_err());
        assert!(root.join("steamapps/common/game/bin").exists());
    }

    #[test]
    fn uninstall_refuses_a_symlinked_install_dir() {
        let temp = Temp::new();
        let root = temp.path().canonicalize().unwrap();
        let game = setup(&root);
        let target = root.join("elsewhere");
        fs::rename(root.join("steamapps/common/game"), &target).unwrap();
        std::os::unix::fs::symlink(&target, root.join("steamapps/common/game")).unwrap();
        assert!(run_in(&game, &root.join("proc-fixture")).is_err());
        assert!(target.join("bin").exists());
        assert!(target.join("data/x").exists());
    }

    #[test]
    fn interrupted_uninstall_is_rolled_forward_by_recover() {
        let temp = Temp::new();
        let root = temp.path().canonicalize().unwrap();
        setup(&root);
        let stage = root.join("steamapps/.steam-tui/30");
        let marker = Marker {
            version: 1,
            app_id: 30,
            steam_root: root.clone(),
            install_name: "game".into(),
        };
        fs::write(stage.join("uninstall.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
        fs::rename(root.join("steamapps/common/game"), stage.join("removing")).unwrap();
        crate::install::recover(&root).unwrap();
        assert_removed(&root);
        crate::install::recover(&root).unwrap();
    }
}
