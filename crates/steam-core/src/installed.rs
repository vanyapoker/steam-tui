//! Offline inventory from app manifests. Sizes are Steam's recorded SizeOnDisk,
//! not a fresh filesystem walk; absent account history stays unknown.
mod metadata;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub(crate) type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
pub(crate) use metadata::{Node, app_metadata, read_vdf, text, text_preserving_keys};
pub(crate) fn failure(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(message.into()))
}

#[derive(Debug, Clone)]
pub struct InstalledGame {
    pub app_id: u32,
    pub name: String,
    /// Manifest SizeOnDisk; no recursive disk usage scan is performed.
    pub size_bytes: Option<u64>,
    pub last_played: Option<u64>,
    pub playtime_minutes: Option<u32>,
    pub install_dir: PathBuf,
    pub library_root: PathBuf,
    pub steam_root: PathBuf,
    /// Only from the explicitly selected account's localconfig.
    pub launch_options: Option<String>,
    pub build_id: Option<u64>,
    pub branch: String,
    pub language: String,
    pub platform: Option<crate::catalog::InstallPlatform>,
    pub installed_depots: BTreeMap<u32, u64>,
}

#[derive(Debug)]
pub struct Inventory {
    pub games: Vec<InstalledGame>,
    pub warnings: Vec<String>,
}

pub fn discover_root(override_root: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = override_root {
        let root = root
            .canonicalize()
            .map_err(|e| failure(format!("Steam root {}: {e}", root.display())))?;
        if !root.join("steamapps").is_dir() {
            return Err(failure(format!(
                "{} has no steamapps directory",
                root.display()
            )));
        }
        return Ok(root);
    }
    let mut candidates = Vec::new();
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        let data = PathBuf::from(data);
        if data.is_absolute() {
            candidates.push(data.join("Steam"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.extend([
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".steam/root"),
        ]);
    }
    for root in candidates {
        if root.join("steamapps").is_dir() {
            return Ok(root.canonicalize()?);
        }
    }
    Err(failure(
        "Steam installation not found; provide an explicit Steam root containing steamapps",
    ))
}

pub(crate) fn library_roots(root: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut roots = BTreeSet::from([root.to_path_buf()]);
    let primary = root.join("steamapps/libraryfolders.vdf");
    let path = if primary.is_file() {
        primary
    } else {
        root.join("config/libraryfolders.vdf")
    };
    match read_vdf(&path) {
        Ok(doc) => {
            if let Some(folders) = doc.child("libraryfolders") {
                for (key, value) in &folders.children {
                    if key.parse::<u32>().is_err() {
                        continue;
                    }
                    let path = value.value.as_deref().or_else(|| value.text("path"));
                    if let Some(path) = path {
                        let path = PathBuf::from(path);
                        if !path.is_absolute() {
                            warnings.push(format!(
                                "Ignoring nonabsolute library path {}",
                                path.display()
                            ));
                        } else {
                            match path.canonicalize() {
                                Ok(path) => {
                                    roots.insert(path);
                                }
                                Err(e) => warnings.push(format!("Library {}: {e}", path.display())),
                            }
                        }
                    }
                }
            } else {
                warnings.push(format!("{} has no libraryfolders table", path.display()));
            }
        }
        Err(e) => warnings.push(format!(
            "{}: {e}; scanning the primary library only",
            path.display()
        )),
    }
    // Prefer the selected root when a stale duplicate exists in another library.
    let mut ordered = vec![root.to_path_buf()];
    ordered.extend(roots.into_iter().filter(|p| p != root));
    ordered
}

/// Canonicalize metadata paths and reject traversal or symlinks outside their base.
pub(crate) fn contained(base: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(failure(format!(
            "unsafe relative install path: {}",
            relative.display()
        )));
    }
    let base = base.canonicalize()?;
    let path = base.join(relative).canonicalize()?;
    if !path.starts_with(&base) {
        return Err(failure("metadata path escapes its install directory"));
    }
    Ok(path)
}

pub fn scan(steam_root: Option<&Path>, account: Option<&str>) -> Result<Inventory> {
    let root = discover_root(steam_root)?;
    let mut warnings = Vec::new();
    let roots = library_roots(&root, &mut warnings);
    let history = account_history(&root, account, &mut warnings);
    let mut games = BTreeMap::new();
    for library in &roots {
        let steamapps = library.join("steamapps");
        let entries = match fs::read_dir(&steamapps) {
            Ok(entries) => entries,
            Err(e) => {
                warnings.push(format!("{}: {e}", steamapps.display()));
                continue;
            }
        };
        let mut manifests = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("appmanifest_") && name.ends_with(".acf") {
                        manifests.push(entry.path());
                    }
                }
                Err(e) => warnings.push(format!("{} directory entry: {e}", steamapps.display())),
            }
        }
        manifests.sort();
        for path in manifests {
            match manifest_game(&path, library, &root, &history) {
                Ok(Some(game)) => {
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        games.entry(game.app_id)
                    {
                        entry.insert(game);
                    } else {
                        warnings.push(format!(
                            "Duplicate installed app {} in {}; keeping first valid library",
                            game.app_id,
                            library.display()
                        ));
                    }
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("{}: {e}", path.display())),
            }
        }
    }
    let ids = games.keys().copied().collect();
    match app_metadata(&root, &ids) {
        Ok(metadata) => {
            games.retain(|id, game| {
                if let Some(app) = metadata.get(id) {
                    if let Some(kind) = app.child("common").and_then(|n| n.text("type"))
                        && !["game", "demo", "mod"]
                            .iter()
                            .any(|game_type| kind.eq_ignore_ascii_case(game_type))
                    {
                        return false;
                    }
                    if let Some(name) = app.child("common").and_then(|n| n.text("name")) {
                        game.name = name.to_owned();
                    }
                }
                true
            });
        }
        Err(e) => warnings.push(format!(
            "App type metadata unavailable: {e}; unclassified manifests remain visible"
        )),
    }
    Ok(Inventory {
        games: games.into_values().collect(),
        warnings,
    })
}

pub(crate) fn manifest_game(
    path: &Path,
    library: &Path,
    root: &Path,
    history: &BTreeMap<u32, Node>,
) -> Result<Option<InstalledGame>> {
    let doc = read_vdf(path)?;
    let app = doc
        .child("AppState")
        .ok_or_else(|| failure("missing AppState"))?;
    let id = app
        .text("appid")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|id| *id != 0)
        .ok_or_else(|| failure("invalid app ID"))?;
    if path.file_name().and_then(|s| s.to_str()) != Some(format!("appmanifest_{id}.acf").as_str()) {
        return Err(failure("manifest filename/app ID mismatch"));
    }
    let flags = app
        .text("StateFlags")
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| failure("invalid StateFlags"))?;
    // FullyInstalled (4) is required; an update-needed installed game remains browsable.
    if flags & 4 == 0 || flags & (1 | 2048) != 0 {
        return Ok(None);
    }
    let install = app
        .text("installdir")
        .ok_or_else(|| failure("missing installdir"))?;
    let install_dir = contained(&library.join("steamapps/common"), install)?;
    if !install_dir.is_dir() {
        return Err(failure("install path is not a directory"));
    }
    if let Ok(tool) = read_vdf(&install_dir.join("toolmanifest.vdf"))
        && tool
            .child("manifest")
            .and_then(|m| m.text("compatmanager_layer_name"))
            .is_some_and(|layer| matches!(layer, "proton" | "container-runtime"))
    {
        return Ok(None);
    }
    let history = history.get(&id);
    Ok(Some(InstalledGame {
        app_id: id,
        name: app
            .text("name")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| failure("missing app name"))?
            .to_owned(),
        size_bytes: app.text("SizeOnDisk").and_then(|s| s.parse().ok()),
        last_played: history
            .and_then(|h| h.text("LastPlayed"))
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 86_400),
        playtime_minutes: history
            .and_then(|h| h.text("Playtime"))
            .and_then(|s| s.parse().ok()),
        launch_options: history
            .and_then(|h| h.text("LaunchOptions"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        build_id: app
            .text("buildid")
            .and_then(|s| s.parse().ok())
            .filter(|id| *id != 0),
        branch: app
            .at(&["UserConfig"])
            .and_then(|n| n.text("BetaKey"))
            .or_else(|| app.at(&["MountedConfig"]).and_then(|n| n.text("BetaKey")))
            .filter(|s| !s.is_empty())
            .unwrap_or("public")
            .to_owned(),
        language: app
            .at(&["UserConfig"])
            .and_then(|n| n.text("language"))
            .or_else(|| app.at(&["MountedConfig"]).and_then(|n| n.text("language")))
            .filter(|s| !s.is_empty())
            .unwrap_or("english")
            .to_owned(),
        platform: app
            .at(&["UserConfig"])
            .and_then(|n| {
                n.text("platform_override_dest")
                    .or_else(|| n.text("os_override"))
                    .or_else(|| n.text("platform_override"))
            })
            .or_else(|| {
                app.at(&["MountedConfig"])
                    .and_then(|n| n.text("platform_override_dest"))
            })
            .or_else(|| {
                history.and_then(|n| {
                    n.text("platform_override_dest")
                        .or_else(|| n.text("os_override"))
                })
            })
            .and_then(|s| {
                if s.eq_ignore_ascii_case("linux") {
                    Some(crate::catalog::InstallPlatform::Linux)
                } else if s.eq_ignore_ascii_case("windows") {
                    Some(crate::catalog::InstallPlatform::Windows)
                } else {
                    None
                }
            }),
        installed_depots: app
            .child("InstalledDepots")
            .map(|depots| {
                depots
                    .children
                    .iter()
                    .filter_map(|(id, n)| {
                        Some((id.parse().ok()?, n.text("manifest")?.parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        install_dir,
        library_root: library.to_path_buf(),
        steam_root: root.to_path_buf(),
    }))
}

pub(crate) fn account_history(
    root: &Path,
    account: Option<&str>,
    warnings: &mut Vec<String>,
) -> BTreeMap<u32, Node> {
    let Some(account) = account.filter(|s| !s.is_empty()) else {
        return BTreeMap::new();
    };
    let result = (|| -> Result<BTreeMap<u32, Node>> {
        let users = read_vdf(&root.join("config/loginusers.vdf"))?;
        let users = users
            .child("users")
            .ok_or_else(|| failure("missing loginusers users table"))?;
        let matching: Vec<_> = users
            .children
            .iter()
            .filter(|(_, value)| {
                value
                    .text("AccountName")
                    .is_some_and(|s| s.eq_ignore_ascii_case(account))
            })
            .collect();
        if matching.len() != 1 {
            return Err(failure(format!(
                "no unique disk login record for account {account}; history left unknown"
            )));
        }
        let steam_id = matching[0]
            .0
            .parse::<u64>()
            .map_err(|_| failure("invalid Steam ID in matching login record"))?;
        // Individual/public SteamID64: never take an unrelated user's directory.
        if steam_id >> 32 != 0x01100001 {
            return Err(failure(
                "matching login record is not an individual public Steam ID",
            ));
        }
        let mut apps = read_vdf(&root.join(format!(
            "userdata/{}/config/localconfig.vdf",
            steam_id as u32
        )))?;
        for key in ["userlocalconfigstore", "software", "valve", "steam", "apps"] {
            apps = apps
                .children
                .remove(key)
                .ok_or_else(|| failure("matching account has no app history table"))?;
        }
        Ok(apps
            .children
            .into_iter()
            .filter_map(|(id, node)| id.parse().ok().map(|id| (id, node)))
            .collect())
    })();
    match result {
        Ok(history) => history,
        Err(e) => {
            warnings.push(format!("Account history: {e}"));
            BTreeMap::new()
        }
    }
}

pub(crate) fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(failure(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            max
        )));
    }
    Ok(bytes)
}
