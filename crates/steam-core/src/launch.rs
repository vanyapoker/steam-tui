//! Direct, metadata-driven game launch. Optional account-checked Valve host;
//! never launches games through a Steam URI, -applaunch, or shell.
//! A spawned PID is not a compatibility/DRM or successful-gameplay guarantee.
mod client_env;
mod desktop;
pub use desktop::internal as internal_desktop_command;
mod host;
pub use host::{HostAccount, internal as internal_host_command};
mod reaper;
#[cfg(test)]
mod tests;
mod tools;
pub use tools::Dependency;
mod setup;

use crate::installed::{
    InstalledGame, Node, Result, app_metadata, contained, failure, library_roots, read_vdf,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
    /// A listed Valve game that requires Valve's optional headless host.
    pub needs_steam: bool,
}
#[derive(Debug)]
pub struct LaunchResult {
    pub pid: u32,
    pub description: String,
}

/// Native built-in-client HOME: account-isolated and retained between launches.
/// Cloud's LinuxHome root must use exactly the same directory.
pub(crate) fn native_home(app_id: u32, account_id: u32) -> Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".local/share"))
        })
        .ok_or_else(|| failure("HOME is not set"))?;
    Ok(data
        .join("steam-tui/homes")
        .join(account_id.to_string())
        .join(app_id.to_string()))
}

/// A required Steam Play tool/runtime that is not installed yet. Callers install
/// it through steam-tui's content service and retry the launch.
#[derive(Debug, Clone)]
pub struct MissingTool {
    pub app_id: u32,
    pub name: String,
}
impl std::fmt::Display for MissingTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (app {}) is not installed", self.name, self.app_id)
    }
}
impl std::error::Error for MissingTool {}

/// The app's PICS document as direct launch uses it. PICS cached by this
/// application is authoritative for content we installed or updated ourselves.
/// Keep an older Steam document when it matches older installed content; a
/// newer available build is not yet an installed build.
pub(crate) fn app_document(game: &InstalledGame) -> Result<Node> {
    let mut cached = if game.build_id.is_some() {
        crate::library::cached_app(game.app_id)?
    } else {
        None
    };
    if let (Some(build), Some(app)) = (game.build_id, cached.as_ref())
        && app
            .at(&["depots", "branches", &game.branch])
            .and_then(|branch| branch.text("buildid"))
            .and_then(|value| value.parse::<u64>().ok())
            == Some(build)
    {
        return Ok(cached.take().expect("checked above"));
    }
    if let Ok(mut metadata) = app_metadata(&game.steam_root, &BTreeSet::from([game.app_id]))
        && let Some(app) = metadata.remove(&game.app_id)
    {
        return Ok(app);
    }
    if game.build_id.is_none() {
        cached = crate::library::cached_app(game.app_id)?;
    }
    cached.ok_or_else(|| {
        failure(format!(
            "No launch metadata for app {}; refresh the online library to fetch it",
            game.app_id
        ))
    })
}

pub fn resolve(game: &InstalledGame) -> Result<LaunchPlan> {
    resolve_app(game, &app_document(game)?)
}

/// Valve games whose secure online mode is served by Valve's unmodified host.
/// These games always require an explicit Valve-host launch.
pub fn requires_valve_host(app_id: u32) -> bool {
    matches!(app_id, 440 | 570 | 730 | 1_422_450)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchBackend {
    BuiltIn,
    ValveHost,
}

/// Enforce the same policy for library, CLI, and internal callers.
pub fn validate_backend(app_id: u32, backend: LaunchBackend) -> Result<()> {
    if requires_valve_host(app_id) && backend != LaunchBackend::ValveHost {
        return Err(failure(
            "This VAC game requires Valve host: press Ctrl+Enter (CLI: --valve-host)",
        ));
    }
    Ok(())
}

/// Automatic UI launches resolve VAC metadata before cloud sync and runtime
/// installation, then carry this immutable choice through the launch pipeline.
pub fn default_backend_for_game(game: &InstalledGame) -> Result<LaunchBackend> {
    if requires_valve_host(game.app_id)
        || metadata_requires_valve_host(game.app_id, &app_document(game)?)
    {
        Ok(LaunchBackend::ValveHost)
    } else {
        Ok(LaunchBackend::BuiltIn)
    }
}

fn metadata_requires_valve_host(app_id: u32, app: &Node) -> bool {
    requires_valve_host(app_id)
        || app
            .at(&["common", "category"])
            .and_then(|category| category.text("category_8"))
            == Some("1")
}

fn pids_named(name: &'static str) -> impl Iterator<Item = i32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(move |entry| {
            fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
        })
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
}

/// The only optional Valve Steam startup path. Account and lifetime ownership
/// are enforced by the host lease manager; built-in launches never enter it.
fn acquire_steam_host(steam_root: &Path, account: &HostAccount) -> Result<host::Lease> {
    host::acquire(steam_root, account)
}

/// Picks the default launch entry and whether it runs through Proton.
fn select_entry<'a>(game: &InstalledGame, app: &'a Node) -> Result<(&'a Node, bool)> {
    let launches = app
        .at(&["config", "launch"])
        .ok_or_else(|| failure("Cached app metadata has no launch entries"))?;
    let mut entries: Vec<_> = launches
        .children
        .iter()
        .filter_map(|(key, node)| key.parse::<u32>().ok().map(|index| (index, node)))
        .collect();
    entries.sort_by_key(|(index, _)| *index);
    // Launch entries marked DLC/demo/server or conditional account/beta options
    // cannot be selected correctly without Steam's launch-option dialog.
    let supported: Vec<_> = entries
        .into_iter()
        .filter(|(_, n)| {
            n.text("type")
                .is_none_or(|t| t.eq_ignore_ascii_case("default"))
                && n.child("config").is_none_or(|c| {
                    c.children
                        .keys()
                        .all(|k| matches!(k.to_ascii_lowercase().as_str(), "oslist" | "osarch"))
                })
        })
        .collect();
    let native = supported
        .iter()
        .find(|(_, node)| platform(node, app, "linux"));
    let windows = supported
        .iter()
        .find(|(_, node)| platform(node, app, "windows"));
    let forced = configured_tool(&game.steam_root, game.app_id, false)?.is_some();
    if forced {
        Ok((windows.ok_or_else(|| failure("This app explicitly selects a compatibility tool, but has no supported Windows launch entry"))?.1, true))
    } else if let Some((_, entry)) = native {
        // A Windows-only app can label a launch option for Linux hosts (e.g.
        // Far Far West's DX11 option). It still needs Proton to run its build.
        let windows_only = app
            .child("common")
            .and_then(|common| common.text("oslist"))
            .is_some_and(|oslist| supports_os(oslist, "windows") && !supports_os(oslist, "linux"));
        Ok((*entry, windows_only))
    } else if let Some((_, entry)) = windows {
        Ok((*entry, true))
    } else {
        Err(failure(
            "No supported default Linux or Windows launch entry in local metadata; conditional launch options require Steam and are not guessed",
        ))
    }
}

/// True when direct launch would run this game as a Windows build through Proton.
pub(crate) fn uses_proton(game: &InstalledGame, app: &Node) -> Result<bool> {
    Ok(select_entry(game, app)?.1)
}

/// True while any process of this user carries the game's `SteamAppId` (the
/// environment direct launch sets), including games started in an earlier session.
pub fn is_running(app_id: u32) -> bool {
    running_app_ids().contains(&app_id)
}

/// Apps carried by this user's processes, including games from earlier TUI sessions.
pub fn running_app_ids() -> BTreeSet<u32> {
    running_app_ids_in(Path::new("/proc"))
}

fn running_app_ids_in(proc_root: &Path) -> BTreeSet<u32> {
    running_app_ids_filtered(proc_root, None)
}

/// Only this account's built-in launches are advertised by our CM session.
/// Valve-hosted games advertise themselves through Valve's client.
pub fn presence_app_ids(steam_id: u64) -> BTreeSet<u32> {
    running_app_ids_filtered(Path::new("/proc"), Some(steam_id))
}
fn running_app_ids_filtered(proc_root: &Path, account: Option<u64>) -> BTreeSet<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::read_dir(proc_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|entry| {
            entry
                .metadata()
                .is_ok_and(|metadata| metadata.uid() == unsafe { libc::getuid() })
        })
        .filter_map(|entry| {
            let environment = fs::read(entry.path().join("environ")).ok()?;
            if let Some(account) = account {
                let marker = format!("STEAM_TUI_ACCOUNT_ID={account}");
                if !environment
                    .split(|b| *b == 0)
                    .any(|v| v == marker.as_bytes())
                    || environment
                        .split(|b| *b == 0)
                        .any(|v| v == b"STEAM_TUI_VALVE_HOSTED=1")
                {
                    return None;
                }
            }
            environment.split(|&b| b == 0).find_map(|var| {
                let value = var.strip_prefix(b"SteamAppId=")?;
                std::str::from_utf8(value)
                    .ok()?
                    .parse::<u32>()
                    .ok()
                    .filter(|id| *id != 0)
            })
        })
        .collect()
}

fn resolve_app(game: &InstalledGame, app: &Node) -> Result<LaunchPlan> {
    let (entry, is_windows) = select_entry(game, app)?;
    let exe = entry
        .text("executable")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failure("Launch entry has no executable"))?;
    let executable = contained(&game.install_dir, &exe.replace('\\', "/")).map_err(|e| {
        failure(format!(
            "Configured executable {exe:?} is unavailable or unsafe: {e}"
        ))
    })?;
    reject_steam(&executable)?;
    if !executable.is_file() {
        return Err(failure("Configured executable is not a regular file"));
    }
    let working_dir = match entry
        .text("workingdir")
        .filter(|s| !s.is_empty() && *s != ".")
    {
        Some(path) => contained(&game.install_dir, &path.replace('\\', "/"))?,
        None => game.install_dir.canonicalize()?,
    };
    if !working_dir.is_dir() {
        return Err(failure("Configured working directory is not a directory"));
    }
    let arguments = split_arguments(entry.text("arguments").unwrap_or(""))?;
    if arguments.iter().any(|s| s.contains('%')) {
        return Err(failure(
            "Launch metadata contains unresolved substitution tokens; refusing to guess their values",
        ));
    }
    let mut plan = LaunchPlan {
        executable,
        arguments,
        working_dir,
        needs_steam: metadata_requires_valve_host(game.app_id, app),
        environment: BTreeMap::from([
            ("SteamAppId".into(), game.app_id.to_string()),
            ("SteamGameId".into(), game.app_id.to_string()),
            ("SteamOverlayGameId".into(), game.app_id.to_string()),
        ]),
    };
    if is_windows {
        plan = proton_plan(game, plan)?;
    } else {
        make_game_executable(&plan.executable)?;
        require_executable(&plan.executable)?;
        // Source 1 TF2 uses SDL2's relative mouse API. Select Valve's shipped
        // SDL2 Classic through the runtime's supported compatibility option,
        // rather than SDL2-on-SDL3, for this app only. Explicit launch options
        // below can override it; no preloads or replacement libraries are used.
        if game.app_id == 440 {
            plan.environment
                .insert("STEAM_COMPAT_RUNTIME_SDL2".into(), "classic".into());
        }
        plan = runtime_plan(game, app, plan)?;
    }
    apply_options(&mut plan, game.launch_options.as_deref())?;
    Ok(plan)
}

fn supports_os(oslist: &str, os: &str) -> bool {
    oslist
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(os))
}

fn platform(node: &Node, app: &Node, os: &str) -> bool {
    let config = node.child("config");
    let oslist = config
        .and_then(|c| c.text("oslist"))
        .or_else(|| node.text("oslist"))
        .or_else(|| app.child("common").and_then(|common| common.text("oslist")));
    // Use explicit launch/app platform metadata, never an executable-name guess.
    let Some(oslist) = oslist else {
        return false;
    };
    if !supports_os(oslist, os) {
        return false;
    }
    config
        .and_then(|c| c.text("osarch"))
        .is_none_or(|arch| match std::env::consts::ARCH {
            "x86_64" => matches!(arch, "64" | "32" | ""),
            "x86" => matches!(arch, "32" | ""),
            _ => false,
        })
}

fn reject_steam(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "steam" | "steam.sh" | "steam.exe" | "steamwebhelper" | "xdg-open" | "gio"
    ) {
        return Err(failure(
            "Refusing Steam/client/URI-helper executable; only direct game launch is supported",
        ));
    }
    Ok(())
}
/// Some depots ship the launch binary without its executable bit; Steam's own
/// client fixes that at launch. Only the game's own ELF binary, owned by this
/// user, gets the execute bits that match its existing read bits.
fn make_game_executable(path: &Path) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    reject_steam(path)?;
    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode();
    if !metadata.is_file() || mode & 0o111 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        return Ok(());
    }
    let mut magic = [0u8; 4];
    if fs::File::open(path)?.read_exact(&mut magic).is_err() || &magic != b"\x7fELF" {
        return Ok(());
    }
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(mode | ((mode & 0o444) >> 2)),
    )?;
    Ok(())
}

fn require_executable(path: &Path) -> Result<()> {
    reject_steam(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(failure(format!(
            "{} is not executable; no permissions were changed",
            path.display()
        )));
    }
    Ok(())
}

/// No shell is involved. Quotes group arguments; expansions, redirections,
/// command substitution and command chaining are rejected even when quoted.
fn split_arguments(input: &str) -> Result<Vec<String>> {
    if input.chars().any(|c| {
        matches!(
            c,
            '\0' | '\n' | '\r' | ';' | '|' | '&' | '<' | '>' | '`' | '$'
        )
    }) {
        return Err(failure(
            "Unsupported shell syntax in launch arguments/options; use plain quoted arguments without shell expansions or operators",
        ));
    }
    let mut args = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                started = true;
            }
            c if quote == Some(c) => {
                quote = None;
            }
            '\\' if quote != Some('\'') => {
                if chars
                    .peek()
                    .is_some_and(|next| next.is_whitespace() || matches!(next, '\\' | '\'' | '"'))
                {
                    token.push(
                        chars
                            .next()
                            .ok_or_else(|| failure("missing escaped character"))?,
                    );
                } else {
                    token.push(c);
                }
                started = true;
            }
            c if c.is_whitespace() && quote.is_none() => {
                if started {
                    args.push(std::mem::take(&mut token));
                    started = false;
                }
            }
            c => {
                token.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(failure("Unclosed quote in launch arguments/options"));
    }
    if started {
        args.push(token);
    }
    Ok(args)
}

fn apply_options(plan: &mut LaunchPlan, options: Option<&str>) -> Result<()> {
    let Some(options) = options else {
        return Ok(());
    };
    let options = split_arguments(options)?;
    let mut commands = options
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "%command%")
        .map(|(i, _)| i);
    let command = commands.next();
    if commands.next().is_some() {
        return Err(failure("Launch options must contain at most one %command%"));
    }
    if options.iter().any(|s| s.contains('%') && s != "%command%") {
        return Err(failure(
            "Unsupported launch-option substitution; only a standalone %command% is supported",
        ));
    }
    let Some(index) = command else {
        plan.arguments.extend(options);
        return Ok(());
    };
    let prefix = &options[..index];
    let prefix = if prefix.first().is_some_and(|s| s == "env") {
        &prefix[1..]
    } else {
        prefix
    };
    for assignment in prefix {
        let (key, value) = assignment.split_once('=').ok_or_else(|| failure("Command wrappers before %command% are unsupported; only NAME=value environment assignments are allowed"))?;
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()))
        {
            return Err(failure(
                "Invalid environment variable name in launch options",
            ));
        }
        if matches!(
            key,
            "LD_PRELOAD"
                | "LD_LIBRARY_PATH"
                | "PATH"
                | "STEAM_COMPAT_CLIENT_INSTALL_PATH"
                | "STEAM_COMPAT_DATA_PATH"
                | "STEAM_COMPAT_TOOL_PATHS"
                | "STEAM_COMPAT_INSTALL_PATH"
        ) {
            return Err(failure(format!(
                "Launch-option override of {key} is unsupported for safe direct launch"
            )));
        }
        plan.environment.insert(key.to_owned(), value.to_owned());
    }
    plan.arguments.extend_from_slice(&options[index + 1..]);
    Ok(())
}

fn installed_apps(game: &InstalledGame) -> BTreeMap<u32, PathBuf> {
    let mut warnings = Vec::new();
    let roots = library_roots(&game.steam_root, &mut warnings);
    let mut installs = BTreeMap::new();
    for root in roots {
        let directory = root.join("steamapps");
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut paths = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("appmanifest_") && s.ends_with(".acf"))
            {
                continue;
            }
            let Ok(doc) = read_vdf(&path) else {
                continue;
            };
            let Some(app) = doc.child("AppState") else {
                continue;
            };
            let Some(id) = app.text("appid").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if !app
                .text("StateFlags")
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|n| n & 4 != 0)
            {
                continue;
            }
            let Some(dir) = app.text("installdir") else {
                continue;
            };
            if let Ok(dir) = contained(&root.join("steamapps/common"), dir)
                && dir.is_dir()
            {
                installs.entry(id).or_insert(dir);
            }
        }
    }
    installs
}

fn proton_plan(game: &InstalledGame, game_plan: LaunchPlan) -> Result<LaunchPlan> {
    let installs = installed_apps(game);
    // Steam Play's registry maps internal tool names to app IDs. These are
    // metadata IDs, not executable guesses (same schema used by protontricks).
    let mut wanted: BTreeSet<_> = installs.keys().copied().collect();
    wanted.insert(891390);
    let metadata = tools::metadata(game, &wanted)?;
    proton_plan_with_metadata(game, game_plan, &installs, &metadata)
}

fn proton_plan_with_metadata(
    game: &InstalledGame,
    game_plan: LaunchPlan,
    installs: &BTreeMap<u32, PathBuf>,
    metadata: &BTreeMap<u32, Node>,
) -> Result<LaunchPlan> {
    let configured = tools::proton_choice(game, metadata)?;
    let configured_id = configured
        .as_deref()
        .map(|name| tool_app_id(metadata, name).ok_or_else(|| tools::registry_error(name)))
        .transpose()?;
    if let Some(id) = configured_id
        && !installs.contains_key(&id)
    {
        return Err(Box::new(MissingTool {
            app_id: id,
            name: configured.clone().expect("configured ID has a name"),
        }));
    }
    let mut proton = Vec::new();
    for (id, directory) in installs {
        let Ok(doc) = read_vdf(&directory.join("toolmanifest.vdf")) else {
            continue;
        };
        let Some(manifest) = doc.child("manifest") else {
            continue;
        };
        if manifest.text("compatmanager_layer_name") != Some("proton") {
            continue;
        }
        if configured_id.is_some_and(|selected| selected != *id) {
            continue;
        }
        proton.push((*id, directory, doc));
    }
    if proton.len() != 1 && configured.is_none() {
        let id = tool_app_id(metadata, "proton-stable")
            .ok_or_else(|| tools::registry_error("proton-stable"))?;
        if !installs.contains_key(&id) {
            return Err(Box::new(MissingTool {
                app_id: id,
                name: "Proton Stable".into(),
            }));
        }
        proton.retain(|(candidate, _, _)| *candidate == id);
    }
    if proton.len() != 1 {
        return Err(failure(format!(
            "Windows launch needs one usable Proton tool{}; found {}. Check the selected tool's installation and manifest",
            configured
                .as_ref()
                .map(|s| format!(" matching {s}"))
                .unwrap_or_default(),
            proton.len()
        )));
    }
    let (root_id, directory, document) = proton.remove(0);
    let manifest = document
        .child("manifest")
        .ok_or_else(|| failure("Selected Proton has no tool manifest"))?;
    let prefix_base = game.library_root.join("steamapps/compatdata");
    // Proton builds the `pfx` prefix itself on first run, provided its data directory
    // exists. Create only that directory; containment is checked after creation.
    let app_directory = prefix_base.join(game.app_id.to_string());
    fs::create_dir_all(&app_directory).map_err(|e| {
        failure(format!(
            "Cannot create Proton data directory {}: {e}",
            app_directory.display()
        ))
    })?;
    let prefix = contained(&prefix_base, &game.app_id.to_string())
        .map_err(|e| failure(format!("Proton data directory is unsafe: {e}")))?;
    let distribution = ["files", "dist"].into_iter().find_map(|name| contained(directory, name).ok().filter(|path| path.is_dir()))
        .ok_or_else(|| failure("Selected Proton has no safe extracted files/dist distribution; finish its installation first"))?;
    for binary in ["wine", "wineserver"] {
        require_executable(
            &contained(&distribution, &format!("bin/{binary}"))
                .map_err(|e| failure(format!("Proton {binary} is unavailable: {e}")))?,
        )?;
    }
    // Standalone launch must not wait indefinitely for pre-existing prefix
    // services. Proton's `run` also configures its game/client drive mappings.
    let mut command = tool_command(directory, manifest, "run")?;
    command.push(game_plan.executable.to_string_lossy().into_owned());
    command.extend(game_plan.arguments);
    let (mut command, tool_paths) = tools::wrap_dependencies(installs, root_id, manifest, command)?;
    let executable = PathBuf::from(command.remove(0));
    let mut environment = game_plan.environment;
    environment.insert(
        "STEAM_COMPAT_CLIENT_INSTALL_PATH".into(),
        game.steam_root.to_string_lossy().into_owned(),
    );
    environment.insert(
        "STEAM_COMPAT_DATA_PATH".into(),
        prefix.to_string_lossy().into_owned(),
    );
    environment.insert(
        "STEAM_COMPAT_INSTALL_PATH".into(),
        game.install_dir.to_string_lossy().into_owned(),
    );
    environment.insert("STEAM_COMPAT_APP_ID".into(), game.app_id.to_string());
    environment.insert(
        "STEAM_COMPAT_LIBRARY_PATHS".into(),
        game.library_root
            .join("steamapps")
            .to_string_lossy()
            .into_owned(),
    );
    environment.insert(
        "STEAM_COMPAT_TOOL_PATHS".into(),
        std::env::join_paths(tool_paths)?
            .to_string_lossy()
            .into_owned(),
    );
    environment.insert("SteamLinuxRuntime_ALLOW_FALLBACK".into(), "0".into());
    Ok(LaunchPlan {
        needs_steam: game_plan.needs_steam,
        executable,
        arguments: command,
        working_dir: game_plan.working_dir,
        environment,
    })
}

/// Native apps may name a container runtime (e.g. SteamLinuxRuntime_sniper) in
/// `config/app_mappings`; Steam launches them inside it via pressure-vessel.
fn runtime_plan(game: &InstalledGame, app: &Node, plan: LaunchPlan) -> Result<LaunchPlan> {
    let Some(tool) = app
        .at(&["config", "app_mappings"])
        .into_iter()
        .flat_map(|m| m.children.values())
        .filter(|n| {
            n.text("platform")
                .is_none_or(|p| p.eq_ignore_ascii_case("linux"))
        })
        .filter_map(|n| n.text("tool").map(str::to_owned))
        .next()
    else {
        return Ok(plan);
    };
    let installs = installed_apps(game);
    let mut wanted: BTreeSet<_> = installs.keys().copied().collect();
    wanted.insert(891390);
    let metadata = tools::metadata(game, &wanted)?;
    // Older registries omit container runtimes. Keep their established app IDs
    // as a fallback; prefer names and aliases from current PICS.
    let known = match tool.as_str() {
        "SteamLinuxRuntime_scout" => Some(1070560),
        "SteamLinuxRuntime_soldier" => Some(1391110),
        "SteamLinuxRuntime_sniper" => Some(1628350),
        "SteamLinuxRuntime_4" | "SteamLinuxRuntime_medic" => Some(4183110),
        _ => None,
    };
    let id = tool_app_id(&metadata, &tool)
        .or(known)
        .ok_or_else(|| tools::registry_error(&tool))?;
    let directory = installs.get(&id).ok_or_else(|| MissingTool {
        app_id: id,
        name: tool.clone(),
    })?;
    let doc = read_vdf(&directory.join("toolmanifest.vdf"))?;
    let manifest = doc
        .child("manifest")
        .ok_or_else(|| failure("Missing runtime manifest"))?;
    if manifest.text("compatmanager_layer_name") != Some("container-runtime") {
        return Err(failure("Unsupported runtime layer"));
    }
    let mut command = tool_command(directory, manifest, "run")?;
    command.push(plan.executable.to_string_lossy().into_owned());
    command.extend(plan.arguments);
    let (mut command, tool_paths) = tools::wrap_dependencies(&installs, id, manifest, command)?;
    let executable = PathBuf::from(command.remove(0));
    let mut environment = plan.environment;
    for (key, value) in [
        ("STEAM_COMPAT_CLIENT_INSTALL_PATH", game.steam_root.clone()),
        ("STEAM_COMPAT_INSTALL_PATH", game.install_dir.clone()),
        (
            "STEAM_COMPAT_LIBRARY_PATHS",
            game.library_root.join("steamapps"),
        ),
        (
            "STEAM_COMPAT_TOOL_PATHS",
            std::env::join_paths(tool_paths)?.into(),
        ),
    ] {
        environment.insert(key.into(), value.to_string_lossy().into_owned());
    }
    environment.insert("STEAM_COMPAT_APP_ID".into(), game.app_id.to_string());
    environment.insert("SteamLinuxRuntime_ALLOW_FALLBACK".into(), "0".into());
    Ok(LaunchPlan {
        needs_steam: plan.needs_steam,
        executable,
        arguments: command,
        working_dir: plan.working_dir,
        environment,
    })
}

fn tool_app_id(metadata: &BTreeMap<u32, Node>, name: &str) -> Option<u32> {
    let mut exact = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for app in metadata.values() {
        let Some(tools) = app.at(&["extended", "compat_tools"]) else {
            continue;
        };
        for (key, tool) in &tools.children {
            let Some(id) = tool
                .text("appid")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|id| *id != 0)
            else {
                continue;
            };
            if key == name {
                exact.insert(id);
            }
            if tool
                .text("aliases")
                .is_some_and(|s| s.split(',').any(|alias| alias.trim() == name))
            {
                aliases.insert(id);
            }
        }
    }
    let ids = if exact.is_empty() { aliases } else { exact };
    if ids.len() == 1 {
        ids.first().copied()
    } else {
        None
    }
}

fn configured_tool(root: &Path, app_id: u32, include_default: bool) -> Result<Option<String>> {
    let path = root.join("config/config.vdf");
    if !path.exists() {
        return Ok(None);
    }
    let doc = read_vdf(&path)?;
    let mapping = doc.at(&[
        "InstallConfigStore",
        "Software",
        "Valve",
        "Steam",
        "CompatToolMapping",
    ]);
    Ok(mapping
        .and_then(|m| {
            m.child(&app_id.to_string())
                .or_else(|| include_default.then(|| m.child("0")).flatten())
        })
        .and_then(|n| n.text("name"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}
fn tool_command(directory: &Path, manifest: &Node, verb: &str) -> Result<Vec<String>> {
    if manifest.text("version") != Some("2") {
        return Err(failure("Unsupported compatibility tool manifest version"));
    }
    let mut command = split_arguments(
        manifest
            .text("commandline")
            .ok_or_else(|| failure("Tool manifest has no commandline"))?,
    )?;
    if command.is_empty() {
        return Err(failure("Empty compatibility tool command"));
    }
    let relative = command[0].strip_prefix('/').ok_or_else(|| {
        failure("Tool executable must be relative to tool root using its documented leading slash")
    })?;
    let executable = contained(directory, relative)?;
    require_executable(&executable)?;
    command[0] = executable.to_string_lossy().into_owned();
    for arg in &mut command[1..] {
        *arg = arg.replace("%verb%", verb);
        if arg.contains('%') {
            return Err(failure(
                "Unsupported compatibility tool command substitution",
            ));
        }
    }
    Ok(command)
}

pub async fn run(
    game: &InstalledGame,
    connection: Option<&steam_vent::Connection>,
    events: Option<crate::backend::EventSender>,
) -> Result<LaunchResult> {
    run_for_account(game, connection, None, events).await
}

/// Resolves on a worker, starts a detached process, checks its first 600ms, and
/// preserves stdout/stderr in an exclusive mode-0600 log. Optional hosted
/// launches acquire an account-checked Valve host lease.
///
/// With an account `connection`, this built-in default launches directly: the game runs on
/// steam-tui's own Steam API library and a per-launch account backend that lives
/// exactly as long as the game. Must be called inside a tokio `LocalSet`.
pub async fn run_for_account(
    game: &InstalledGame,
    connection: Option<&steam_vent::Connection>,
    account_name: Option<&str>,
    events: Option<crate::backend::EventSender>,
) -> Result<LaunchResult> {
    run_with_backend(
        game,
        connection,
        account_name,
        LaunchBackend::BuiltIn,
        events,
    )
    .await
}

/// Explicit per-launch backend; never inferred from app metadata or global settings.
pub async fn run_with_backend(
    game: &InstalledGame,
    connection: Option<&steam_vent::Connection>,
    account_name: Option<&str>,
    backend_kind: LaunchBackend,
    events: Option<crate::backend::EventSender>,
) -> Result<LaunchResult> {
    validate_backend(game.app_id, backend_kind)?;
    let save_lease = crate::cloud::launch_lease(game.app_id)?;
    if is_running(game.app_id) {
        return Err(failure(
            "this game is already running; close it before launching with another account or mode",
        ));
    }
    let owned = game.clone();
    let mut plan = tools::resolve_online(&owned, connection).await?;
    if plan.needs_steam && backend_kind != LaunchBackend::ValveHost {
        return Err(failure(
            "This VAC game requires Valve host: press Ctrl+Enter (CLI: --valve-host)",
        ));
    }
    let host_account = if backend_kind == LaunchBackend::ValveHost {
        client_env::prepare_hosted(&mut plan)?;
        let connection = connection.ok_or_else(|| failure("Valve-host launches require the selected account identity; reconnect steam-tui first"))?;
        Some(HostAccount {
            name: account_name
                .ok_or_else(|| failure("Valve-host launches require an explicit account name"))?
                .to_owned(),
            steam_id: connection.steam_id().steam64(),
        })
    } else {
        None
    };
    let mut backend = None;
    if backend_kind == LaunchBackend::BuiltIn {
        let connection = connection.ok_or_else(|| {
            failure(
                "The built-in Steam backend needs a signed-in account; sign in with `steam-tui login`",
            )
        })?;
        let library = client_env::client_library()?;
        let remote = owned
            .steam_root
            .join("userdata")
            .join(connection.steam_id().account_id().to_string())
            .join(owned.app_id.to_string())
            .join("remote");
        fs::create_dir_all(&remote)?;
        steam_ipc::storage::Store::open(&remote)?.cleanup_stages()?;
        plan.environment.insert(
            "STEAM_TUI_REMOTE_STORAGE".into(),
            remote.to_string_lossy().into_owned(),
        );
        let metadata = app_document(&owned)?;
        plan.environment.insert(
            "STEAM_TUI_REMOTE_CLOUD".into(),
            if metadata.child("ufs").is_some() {
                "1"
            } else {
                "0"
            }
            .into(),
        );
        if let Some(ufs) = metadata.child("ufs") {
            let uses_cloud_documents = ufs.child("savefiles").is_some_and(|r| {
                r.children
                    .values()
                    .any(|n| n.text("root") == Some("SteamCloudDocuments"))
            }) || ufs.child("rootoverrides").is_some_and(|r| {
                r.children
                    .values()
                    .any(|n| n.text("useinstead") == Some("SteamCloudDocuments"))
            });
            if uses_cloud_documents
                && let Some(home) = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
            {
                plan.environment.insert(
                    "STEAM_TUI_CLOUD_DOCUMENTS_BASE".into(),
                    home.join(".SteamCloud").to_string_lossy().into_owned(),
                );
            }
            for (field, env) in [
                ("quota", "STEAM_TUI_REMOTE_QUOTA"),
                ("maxnumfiles", "STEAM_TUI_REMOTE_MAX_FILES"),
            ] {
                if let Some(value) = ufs.text(field).filter(|v| v.parse::<u64>().is_ok()) {
                    plan.environment.insert(env.into(), value.into());
                }
            }
        }
        let endpoint =
            crate::backend::Endpoint::create()?.with_workshop(owned.library_root.join("steamapps"));
        let home = (!plan.environment.contains_key("STEAM_COMPAT_DATA_PATH"))
            .then(|| native_home(owned.app_id, connection.steam_id().account_id()))
            .transpose()?;
        let sandbox = client_env::apply(
            &mut plan,
            owned.app_id,
            &endpoint.path(),
            &library,
            home.as_deref(),
        )?;
        backend = Some((endpoint, sandbox, connection.clone()));
    }
    if let Some(connection) = connection {
        plan.environment.insert(
            "STEAM_TUI_ACCOUNT_ID".into(),
            connection.steam_id().steam64().to_string(),
        );
    }
    let token = reaper::new_token();
    plan.environment
        .insert(reaper::TOKEN_VAR.into(), token.clone());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    if let Some((endpoint, sandbox, connection)) = backend {
        let app_id = owned.app_id;
        let backend_events = events.clone();
        tokio::task::spawn_local(async move {
            let _sandbox = sandbox;
            tokio::select! {
                _ = endpoint.run_with_events(&connection, app_id, backend_events) => {}
                _ = done_rx => {}
            }
        });
    }
    let mut done_tx = Some(done_tx);
    // Setup executables may call Steam APIs too. Serve them with the same
    // account backend/private HOME, and close it on any setup failure.
    setup::prepare(&owned, &plan, connection, events.as_ref()).await?;
    let watched_token = token.clone();
    let (mut child, log, mut status_log, mut host_lease) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let host_lease = host_account
                .as_ref()
                .map(|account| acquire_steam_host(&owned.steam_root, account))
                .transpose()?;
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .ok_or_else(|| failure("HOME/XDG_STATE_HOME unset; cannot create launch log"))?;
            let logs = state.join("steam-tui/launches");
            fs::create_dir_all(&logs)?;
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = logs.join(format!(
                "{}-{stamp}-{}.log",
                owned.app_id,
                std::process::id()
            ));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            let status_log = file.try_clone()?;
            let mut command = Command::new(&plan.executable);
            if host_lease.is_some() {
                for (key, _) in std::env::vars_os() {
                    if key.as_encoded_bytes().starts_with(b"STEAM_TUI_") {
                        command.env_remove(key);
                    }
                }
                command.env_remove("LD_PRELOAD").env_remove("LD_AUDIT");
            }
            command
                .args(&plan.arguments)
                .current_dir(&plan.working_dir)
                .envs(&plan.environment)
                .stdin(Stdio::null())
                .stdout(Stdio::from(file.try_clone()?))
                .stderr(Stdio::from(file));
            // setsid disconnects the game from TUI terminal signals; all inherited
            // standard descriptors are replaced above. No shell or client fallback.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().map_err(|e| {
                failure(format!(
                    "Cannot execute {}: {e}; log {}",
                    plan.executable.display(),
                    path.display()
                ))
            })?;
            if let Err(error) = desktop::watch(child.id(), &watched_token, &status_log) {
                let _ = writeln!(
                    &status_log,
                    "[steam-tui] game workspace placement unavailable: {error}"
                );
            }
            if let Some(lease) = &host_lease {
                if child.try_wait()?.is_none() {
                    if let Err(error) = lease.watch(child.id(), &watched_token) {
                        reaper::terminate(&watched_token, Duration::from_secs(2));
                        let _ = child.wait();
                        return Err(error);
                    }
                }
            }
            Ok((child, path, status_log, host_lease))
        })
        .await
        .map_err(|e| failure(format!("Launch worker failed: {e}")))??;
    let pid = child.id();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let exited = child.try_wait()?;
    if exited.is_some() {
        drop(host_lease.take());
        drop(done_tx.take());
        let token = token.clone();
        let _ = std::thread::Builder::new()
            .name(format!("game-cleanup-{pid}"))
            .spawn(move || reaper::terminate(&token, Duration::from_secs(5)));
    }
    match exited {
        Some(status) if !status.success() => Err(failure(format!(
            "Process {pid} exited immediately with {status}; inspect {}. No Steam fallback was attempted",
            log.display()
        ))),
        Some(status) => Ok(LaunchResult {
            pid,
            description: format!(
                "Process {pid} exited with {status}; log {}. This does not establish successful gameplay",
                log.display()
            ),
        }),
        None => {
            // Reap if the TUI stays open; the game remains detached on TUI exit.
            let finished = std::sync::Arc::new(AtomicBool::new(false));
            {
                // When the game's main command exits, nothing it left behind may linger.
                let (token, finished) = (token.clone(), finished.clone());
                let _ = std::thread::Builder::new()
                    .name(format!("game-reaper-{pid}"))
                    .spawn(move || reaper::watch(&token, || finished.load(Ordering::SeqCst)));
            }
            let reaper = std::thread::Builder::new()
                .name(format!("game-{pid}"))
                .spawn(move || {
                    let _save_lease = save_lease;
                    let waited = child.wait();
                    finished.store(true, Ordering::SeqCst);
                    drop(host_lease);
                    // Ends the per-launch backend (and removes its sandbox).
                    drop(done_tx);
                    reaper::terminate(&token, Duration::from_secs(10));
                    match waited {
                        Ok(status) => {
                            let _ = writeln!(
                                status_log,
                                "\n[steam-tui] PID {pid} exited with {status}"
                            );
                        }
                        Err(error) => {
                            let _ = writeln!(
                                status_log,
                                "\n[steam-tui] PID {pid} wait failed: {error}"
                            );
                        }
                    }
                });
            let warning = if reaper.is_err() {
                "; child reaper unavailable"
            } else {
                ""
            };
            Ok(LaunchResult {
                pid,
                description: format!(
                    "Started PID {pid}{warning}; log {}. Process running, gameplay/DRM compatibility unverified",
                    log.display()
                ),
            })
        }
    }
}
