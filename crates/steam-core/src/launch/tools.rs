//! Compatibility-tool selection and bounded runtime chains. Valve's local
//! appinfo is optional; our own full PICS cache supplies the tool registry.
use super::*;

const REGISTRY: u32 = 891390;
const MAX_TOOLS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Dependency {
    Tool(MissingTool),
    Additional {
        app_id: u32,
    },
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
            Self::Additional { app_id } => write!(
                f,
                "Additional dependency app {app_id} needs installation or update"
            ),
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
            Self::Additional { app_id } => *app_id,
            Self::Shared { app_id, .. } => *app_id,
        }
    }
    pub async fn install_request(
        &self,
        connection: &steam_vent::Connection,
        game: &InstalledGame,
        account: &str,
    ) -> Result<crate::install::InstallRequest> {
        if let Self::Additional { app_id } = self {
            let existing = crate::installed::scan(Some(&game.steam_root), Some(account))?
                .games
                .into_iter()
                .find(|g| g.app_id == *app_id);
            let plan = additional_plan(connection, *app_id, existing.as_ref()).await?;
            return Ok(crate::install::InstallRequest {
                plan,
                destination: existing
                    .as_ref()
                    .map(|g| g.library_root.clone())
                    .unwrap_or_else(|| game.library_root.clone()),
                steam_root: game.steam_root.clone(),
                account: account.into(),
                existing,
            });
        }
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

/// Ghidra 011a0300: extended/additional_dependencies entries contain appid,
/// src_os and dest_os. The selected game platform is the source; Linux is the
/// destination. These apps are installations, not command-line wrappers.
fn additional_ids(document: &Node, source: &str) -> Result<BTreeSet<u32>> {
    let mut result = BTreeSet::new();
    for entry in document
        .at(&["extended", "additional_dependencies"])
        .into_iter()
        .flat_map(|n| n.children.values())
    {
        if entry.text("src_os") != Some(source) || entry.text("dest_os") != Some("linux") {
            continue;
        }
        let id = entry
            .text("appid")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id != 0)
            .ok_or_else(|| failure("Invalid additional dependency app ID"))?;
        result.insert(id);
        if result.len() > MAX_TOOLS {
            return Err(failure("Too many additional dependencies"));
        }
    }
    Ok(result)
}
async fn additional_plan(
    connection: &steam_vent::Connection,
    app: u32,
    existing: Option<&InstalledGame>,
) -> Result<crate::catalog::InstallPlan> {
    if crate::library::cached_ownership(connection).is_some_and(|(owned, _)| owned.contains(&app)) {
        return crate::catalog::plan(connection, app, existing)
            .await
            .map_err(|e| failure(e.to_string()));
    }
    let document = crate::library::cached_app(app)?
        .ok_or_else(|| failure("Additional dependency metadata is unavailable"))?;
    if document
        .at(&["common"])
        .and_then(|n| n.text("type"))
        .is_none_or(|t| !t.eq_ignore_ascii_case("tool"))
    {
        return Err(failure(format!(
            "Additional app {app} requires its own account license"
        )));
    }
    crate::catalog::plan_tool(connection, app)
        .await
        .map_err(|e| failure(e.to_string()))
}

pub(super) async fn prepare_additional(
    game: &InstalledGame,
    plan: &LaunchPlan,
    connection: Option<&steam_vent::Connection>,
) -> Result<()> {
    let root = app_document(game)?;
    let os = if plan.environment.contains_key("STEAM_COMPAT_DATA_PATH") {
        "windows"
    } else {
        "linux"
    };
    if additional_ids(&root, os)?.is_empty() {
        return Ok(());
    }
    let connection =
        connection.ok_or_else(|| failure("Connect to verify additional launch dependencies"))?;
    let inventory = crate::installed::scan(Some(&game.steam_root), None)?;
    let installs: BTreeMap<_, _> = inventory.games.into_iter().map(|g| (g.app_id, g)).collect();
    let mut docs = BTreeMap::from([(game.app_id, root)]);
    let mut graph = BTreeMap::new();
    let mut frontier = vec![(game.app_id, os.to_owned())];
    while let Some((id, source)) = frontier.pop() {
        if graph.contains_key(&id) {
            continue;
        }
        if graph.len() >= MAX_TOOLS {
            return Err(failure("Additional dependency graph exceeds 32 apps"));
        }
        let dependencies = additional_ids(&docs[&id], &source)?;
        let unknown: BTreeSet<_> = dependencies
            .iter()
            .filter(|id| !docs.contains_key(id))
            .copied()
            .collect();
        if docs.len() + unknown.len() > MAX_TOOLS {
            return Err(failure("Additional dependency graph exceeds 32 apps"));
        }
        if !unknown.is_empty() {
            let (found, missing) = crate::library::inspect_apps(connection, &unknown)
                .await
                .map_err(|e| failure(e.to_string()))?;
            if !missing.is_empty() {
                return Err(failure(
                    "Additional dependency metadata was denied or unavailable",
                ));
            }
            for (id, app) in found {
                docs.insert(
                    id,
                    app.document
                        .ok_or_else(|| failure("Additional dependency has no metadata"))?,
                );
            }
        }
        for &child in &dependencies {
            let doc = docs
                .get(&child)
                .ok_or_else(|| failure("Additional dependency has no metadata"))?;
            let platform = installs
                .get(&child)
                .and_then(|g| g.platform)
                .unwrap_or_else(|| {
                    if doc
                        .at(&["common"])
                        .and_then(|n| n.text("oslist"))
                        .is_some_and(|s| {
                            s.split(',').any(|s| s == "windows")
                                && !s.split(',').any(|s| s == "linux")
                        })
                    {
                        crate::catalog::InstallPlatform::Windows
                    } else {
                        crate::catalog::InstallPlatform::Linux
                    }
                });
            frontier.push((
                child,
                if platform == crate::catalog::InstallPlatform::Windows {
                    "windows".into()
                } else {
                    "linux".into()
                },
            ));
        }
        graph.insert(id, dependencies);
    }
    let order = dependency_order(game.app_id, &graph)?;
    for id in order.into_iter().filter(|id| *id != game.app_id) {
        let selected = additional_plan(connection, id, installs.get(&id)).await?;
        if installs.get(&id).is_none_or(|installed| {
            !matches!(
                crate::catalog::compare(installed, &selected),
                crate::catalog::UpdateStatus::UpToDate
            )
        }) {
            return Err(Box::new(Dependency::Additional { app_id: id }));
        }
    }
    Ok(())
}
fn dependency_order(root: u32, graph: &BTreeMap<u32, BTreeSet<u32>>) -> Result<Vec<u32>> {
    fn visit(
        id: u32,
        graph: &BTreeMap<u32, BTreeSet<u32>>,
        active: &mut BTreeSet<u32>,
        seen: &mut BTreeSet<u32>,
        order: &mut Vec<u32>,
    ) -> Result<()> {
        if seen.contains(&id) {
            return Ok(());
        }
        if !active.insert(id) {
            return Err(failure(format!("Additional dependency cycle at app {id}")));
        }
        if active.len() > MAX_TOOLS {
            return Err(failure("Additional dependency graph exceeds 32 apps"));
        }
        for &next in graph
            .get(&id)
            .ok_or_else(|| failure("Incomplete dependency graph"))?
        {
            visit(next, graph, active, seen, order)?;
        }
        active.remove(&id);
        seen.insert(id);
        order.push(id);
        Ok(())
    }
    let mut order = Vec::new();
    visit(
        root,
        graph,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
        &mut order,
    )?;
    Ok(order)
}
#[cfg(test)]
mod additional_tests {
    use super::*;
    #[test]
    fn additional_dependencies_filter_platform_and_sort_before_parent_and_reject_cycles() {
        let doc = crate::installed::text(
            r#""extended" { "additional_dependencies" {
            "1" { "appid" "20" "src_os" "windows" "dest_os" "linux" }
            "2" { "appid" "30" "src_os" "windows" "dest_os" "macos" }
            "3" { "appid" "40" "src_os" "linux" "dest_os" "linux" }
        } }"#,
        )
        .unwrap();
        assert_eq!(
            additional_ids(&doc, "windows").unwrap(),
            BTreeSet::from([20])
        );
        assert_eq!(additional_ids(&doc, "linux").unwrap(), BTreeSet::from([40]));
        let mut graph = BTreeMap::from([
            (10, BTreeSet::from([20, 30])),
            (20, BTreeSet::from([30])),
            (30, BTreeSet::new()),
        ]);
        assert_eq!(dependency_order(10, &graph).unwrap(), [30, 20, 10]);
        graph.insert(30, BTreeSet::from([10]));
        assert!(dependency_order(10, &graph).is_err());
    }
}
