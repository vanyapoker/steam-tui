//! Compatibility-tool selection and bounded runtime chains. Valve's local
//! appinfo is optional; our own full PICS cache supplies the tool registry.
use super::*;

const REGISTRY: u32 = 891390;
const MAX_TOOLS: usize = 32;

#[derive(Debug, Clone)]
pub enum Dependency {
    Tool(MissingTool),
    Shared {
        app_id: u32,
        depots: BTreeSet<u32>,
        platform: crate::catalog::InstallPlatform,
    },
}
impl std::fmt::Display for Dependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool(tool) => tool.fmt(f),
            Self::Shared { app_id, .. } => write!(
                f,
                "Shared prerequisites (app {app_id}) need installation or update"
            ),
        }
    }
}
impl std::error::Error for Dependency {}
impl Dependency {
    pub fn from_error(error: &(dyn std::error::Error + 'static)) -> Option<Self> {
        error
            .downcast_ref::<Self>()
            .cloned()
            .or_else(|| error.downcast_ref::<MissingTool>().cloned().map(Self::Tool))
    }
    pub fn app_id(&self) -> u32 {
        match self {
            Self::Tool(tool) => tool.app_id,
            Self::Shared { app_id, .. } => *app_id,
        }
    }
    pub async fn install_request(
        &self,
        connection: &steam_vent::Connection,
        game: &InstalledGame,
        account: &str,
    ) -> Result<crate::install::InstallRequest> {
        let Self::Shared {
            app_id,
            depots,
            platform,
        } = self
        else {
            let Self::Tool(tool) = self else {
                unreachable!()
            };
            return tool.install_request(connection, game, account).await;
        };
        if *app_id == 0
            || *app_id == game.app_id
            || depots.is_empty()
            || depots.len() > 128
            || depots.contains(&0)
        {
            return Err(failure("Invalid shared prerequisite request"));
        }
        let existing = crate::catalog::shared::installed(game, *app_id)?;
        let plan =
            crate::catalog::shared::plan(connection, *app_id, depots, *platform, existing.as_ref())
                .await
                .map_err(|e| failure(e.to_string()))?;
        Ok(crate::install::InstallRequest {
            plan,
            destination: game.library_root.clone(),
            steam_root: game.steam_root.clone(),
            account: account.into(),
            existing,
        })
    }
}

#[derive(Debug)]
struct RegistryUnavailable(String);
impl std::fmt::Display for RegistryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "No unambiguous Steam Play registry entry for {}; refresh compatibility-tool metadata or select a supported tool",
            self.0
        )
    }
}
impl std::error::Error for RegistryUnavailable {}

pub(super) fn registry_error(name: &str) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(RegistryUnavailable(name.into()))
}

pub(super) fn metadata(
    game: &InstalledGame,
    wanted: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, Node>> {
    let mut docs = app_metadata(&game.steam_root, wanted).unwrap_or_default();
    if let Some(cached) = crate::library::cached_app(REGISTRY)? {
        // Preserve locally known custom entries, with fresh PICS taking precedence
        // for official names and app mappings. Neither source must be installed.
        let registry = docs.entry(REGISTRY).or_default();
        let extended = registry.children.entry("extended".into()).or_default();
        if let Some(source) = cached.child("extended") {
            for section in ["compat_tools", "app_mappings"] {
                if let Some(entries) = source.child(section) {
                    extended
                        .children
                        .entry(section.into())
                        .or_default()
                        .children
                        .extend(entries.children.clone());
                }
            }
        }
    }
    Ok(docs)
}

pub(super) fn proton_choice(
    game: &InstalledGame,
    metadata: &BTreeMap<u32, Node>,
) -> Result<Option<String>> {
    if let Some(tool) = configured_tool(&game.steam_root, game.app_id, true)? {
        return Ok(Some(tool));
    }
    Ok(metadata
        .get(&REGISTRY)
        .and_then(|doc| doc.at(&["extended", "app_mappings", &game.app_id.to_string()]))
        .filter(|mapping| {
            mapping
                .text("platform")
                .is_none_or(|p| p.eq_ignore_ascii_case("linux"))
        })
        .and_then(|mapping| mapping.text("tool"))
        .filter(|tool| !tool.is_empty())
        .map(str::to_owned))
}

/// Wrap the inner command in successive outer container runtimes. Include all
/// tool paths for Proton/pressure-vessel and reject cycles before execution.
pub(super) fn wrap_dependencies(
    installs: &BTreeMap<u32, PathBuf>,
    root_id: u32,
    root: &Node,
    mut command: Vec<String>,
) -> Result<(Vec<String>, Vec<PathBuf>)> {
    let mut seen = BTreeSet::from([root_id]);
    let mut paths = vec![
        installs
            .get(&root_id)
            .ok_or_else(|| failure("Root tool is not installed"))?
            .clone(),
    ];
    let mut required = root.text("require_tool_appid").map(str::to_owned);
    while let Some(value) = required {
        let id = value
            .parse::<u32>()
            .ok()
            .filter(|id| *id != 0)
            .ok_or_else(|| failure("Invalid required runtime app ID"))?;
        if !seen.insert(id) {
            return Err(failure(format!(
                "Compatibility-tool dependency cycle at app {id}"
            )));
        }
        if seen.len() > MAX_TOOLS {
            return Err(failure(format!(
                "Compatibility-tool dependency chain exceeds {MAX_TOOLS} tools"
            )));
        }
        let directory = installs.get(&id).ok_or_else(|| MissingTool {
            app_id: id,
            name: format!("Steam runtime {id}"),
        })?;
        let document = read_vdf(&directory.join("toolmanifest.vdf"))?;
        let manifest = document
            .child("manifest")
            .ok_or_else(|| failure("Missing runtime manifest"))?;
        if manifest.text("compatmanager_layer_name") != Some("container-runtime") {
            return Err(failure(format!("Unsupported runtime layer for app {id}")));
        }
        let mut wrapper = tool_command(directory, manifest, "run")?;
        wrapper.extend(command);
        command = wrapper;
        paths.push(directory.clone());
        required = manifest.text("require_tool_appid").map(str::to_owned);
    }
    Ok((command, paths))
}

pub(super) async fn resolve_online(
    game: &InstalledGame,
    connection: Option<&steam_vent::Connection>,
) -> Result<LaunchPlan> {
    let attempt = async || {
        let game = game.clone();
        tokio::task::spawn_blocking(move || resolve(&game))
            .await
            .map_err(|e| failure(format!("Launch worker failed: {e}")))?
    };
    let result = attempt().await;
    if result
        .as_ref()
        .err()
        .is_some_and(|e| e.is::<RegistryUnavailable>())
        && let Some(connection) = connection
    {
        // inspect_apps persists full documents before pruning its returned view.
        let (_, unresolved) = crate::library::inspect_apps(connection, &BTreeSet::from([REGISTRY]))
            .await
            .map_err(|e| failure(format!("Could not fetch compatibility-tool metadata: {e}")))?;
        if unresolved.contains(&REGISTRY) {
            return Err(failure("Steam did not provide compatibility-tool metadata"));
        }
        return attempt().await;
    }
    result
}

impl MissingTool {
    /// Build the same verified install request for TUI and CLI launches.
    pub async fn install_request(
        &self,
        connection: &steam_vent::Connection,
        game: &InstalledGame,
        account: &str,
    ) -> Result<crate::install::InstallRequest> {
        if self.app_id == 0 || self.app_id == game.app_id {
            return Err(failure("Invalid compatibility-tool dependency"));
        }
        let plan = crate::catalog::plan_tool(connection, self.app_id)
            .await
            .map_err(|e| {
                failure(format!(
                    "Could not plan compatibility-tool installation: {e}"
                ))
            })?;
        let library = game.library_root.clone();
        let destination =
            tokio::task::spawn_blocking(move || crate::install::destination(&library))
                .await
                .map_err(|e| failure(format!("Dependency destination worker failed: {e}")))??;
        Ok(crate::install::InstallRequest {
            plan,
            destination: destination.path,
            steam_root: game.steam_root.clone(),
            account: account.into(),
            existing: None,
        })
    }
}
