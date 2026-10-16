//! Licensed games, installed inventory, and current Steam depot/build metadata.
//! Ownership is session-scoped; PICS is refreshed on every load. Service failures
//! retain known licensed rows but never turn stale metadata into an update claim.
//!
//! Load cost is dominated by PICS round trips, so every stage batches and runs a
//! bounded number of requests concurrently (`library::CONCURRENT_REQUESTS`): token
//! and product-info batches, and the shared-source app documents that `depotfromapp`
//! references need, which are fetched once, deduplicated, in waves rather than one
//! app at a time per referencing game. All concurrency uses ordered `buffered`
//! streams and sorted maps, so output never depends on response timing.
//!
//! Limits: at most `MAX_SOURCE_APPS` shared-source apps are fetched per load and
//! `MAX_SOURCE_WAVES` reference levels followed; a game whose graph is larger than
//! `MAX_GRAPH_APPS` apps is reported blocked. A source app Steam will not describe
//! (token denied) blocks only the games whose selected depots need it.
mod cache;
mod metadata;
#[cfg(test)]
mod tests;

use crate::installed::{self, InstalledGame, Inventory, Node};
use crate::library;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::PathBuf;
use steam_vent::Connection;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

const MAX_SOURCE_APPS: usize = 512;
const MAX_SOURCE_WAVES: usize = 8;
const MAX_GRAPH_APPS: usize = 64;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum InstallPlatform {
    Linux,
    /// Windows depots, run through Proton.
    Windows,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DepotSpec {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub source_app_id: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallPlan {
    pub app_id: u32,
    pub name: String,
    pub install_dir_name: String,
    pub build_id: u64,
    pub platform: InstallPlatform,
    pub branch: String,
    pub language: String,
    pub depots: Vec<DepotSpec>,
    pub download_bytes: u64,
    pub installed_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UpdateStatus {
    Unknown,
    UpToDate,
    Available {
        installed_build: u64,
        latest_build: u64,
    },
    Unavailable(String),
}
#[derive(Debug, Clone)]
pub struct CatalogItem {
    pub app_id: u32,
    pub name: String,
    pub installed: Option<InstalledGame>,
    pub plan: Option<InstallPlan>,
    pub update: UpdateStatus,
    pub last_played: Option<u64>,
    pub playtime_minutes: Option<u32>,
    pub size_bytes: Option<u64>,
    pub unavailable_reason: Option<String>,
}
#[derive(Debug)]
pub struct CatalogSnapshot {
    pub items: Vec<CatalogItem>,
    pub warnings: Vec<String>,
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

/// Account play history from the selected account's `localconfig.vdf`. Blocking.
fn read_history(
    root: Option<PathBuf>,
    account: Option<&str>,
) -> (BTreeMap<u32, Node>, Vec<String>) {
    let mut warnings = Vec::new();
    let root = root.or_else(|| installed::discover_root(None).ok());
    let history = root
        .map(|root| installed::account_history(&root, account, &mut warnings))
        .unwrap_or_default();
    (history, warnings)
}

/// The last successful owned-catalog snapshot for `account`, merged with the fresh
/// local `inventory`, or `None` when no valid cache exists. Synchronous disk I/O:
/// call from `spawn_blocking`.
///
/// Installed state, sizes, play history and last-played always come from the fresh
/// inventory/localconfig, never the cache. Plans and blocked reasons are as of the
/// last full load; an installed row's update status is recomputed from the fresh
/// installed build against the cached plan, so it can lag Steam until `load`
/// finishes. Installed games absent from the cache appear with unknown update
/// status and no ownership claim. This is a display aid, not an install authority:
/// `plan` always resolves against live ownership.
pub fn cached(account: &str, inventory: Inventory) -> Option<CatalogSnapshot> {
    cached_in(&cache::directory().ok()?, account, inventory)
}

fn cached_in(
    dir: &std::path::Path,
    account: &str,
    inventory: Inventory,
) -> Option<CatalogSnapshot> {
    let rows = cache::read(dir, account)?;
    let mut warnings = inventory.warnings;
    let root = inventory.games.first().map(|g| g.steam_root.clone());
    let (history, history_warnings) = read_history(root, Some(account));
    warnings.extend(history_warnings);
    let mut installed: BTreeMap<_, _> =
        inventory.games.into_iter().map(|g| (g.app_id, g)).collect();
    let mut items = Vec::with_capacity(rows.len() + installed.len());
    for row in rows {
        let existing = installed.remove(&row.app_id);
        let update = match (&existing, &row.plan) {
            (Some(game), Some(plan)) => compare(game, plan),
            // An uninstalled game cannot have an update, whatever it was cached as.
            (None, _) if matches!(row.update, UpdateStatus::Available { .. }) => {
                UpdateStatus::Unknown
            }
            _ => row.update,
        };
        let app = library::LibraryApp {
            app_id: row.app_id,
            name: Some(row.name),
            app_type: None,
            document: None,
        };
        items.push(item(
            Some(app),
            existing,
            row.plan,
            update,
            row.reason,
            &history,
        ));
    }
    items.extend(installed.into_values().map(|game| {
        item(
            None,
            Some(game),
            None,
            UpdateStatus::Unknown,
            None,
            &history,
        )
    }));
    items.sort_by_key(|item| item.app_id);
    warnings.push("Showing the last known catalog; Steam metadata is refreshing".to_owned());
    Some(CatalogSnapshot { items, warnings })
}

pub async fn load(
    connection: &Connection,
    inventory: Inventory,
    account: Option<&str>,
) -> Result<CatalogSnapshot> {
    let mut warnings = inventory.warnings;
    let history_root = inventory.games.first().map(|g| g.steam_root.clone());
    // Disk history overlaps with the network load instead of preceding it.
    let history_account = account.map(str::to_owned);
    let history_task =
        tokio::task::spawn_blocking(move || read_history(history_root, history_account.as_deref()));
    let mut installed: BTreeMap<_, _> =
        inventory.games.into_iter().map(|g| (g.app_id, g)).collect();
    let library = library::load(connection).await;
    let (history, history_warnings) = history_task.await?;
    warnings.extend(history_warnings);
    let library = match library {
        Ok(library) => library,
        Err(error) => {
            warnings.push(format!("Licensed catalog unavailable: {error}"));
            if installed.is_empty() {
                return Err(error);
            }
            return Ok(CatalogSnapshot {
                items: installed
                    .into_values()
                    .map(|game| {
                        item(
                            None,
                            Some(game),
                            None,
                            UpdateStatus::Unknown,
                            Some(error.to_string()),
                            &history,
                        )
                    })
                    .collect(),
                warnings,
            });
        }
    };
    warnings.extend(library.warnings);
    if !library.unresolved_packages.is_empty() {
        warnings.push(format!("{} license packages could not be refreshed; known membership is retained, additional games may be missing", library.unresolved_packages.len()));
    }
    let owned: BTreeSet<_> = library.apps.iter().map(|a| a.app_id).collect();
    let unresolved: BTreeSet<_> = library.unresolved_apps.into_iter().collect();
    let mut apps = library.apps;
    let mut docs = BTreeMap::new();
    for app in &mut apps {
        if !unresolved.contains(&app.app_id)
            && let Some(document) = app.document.take()
        {
            docs.insert(app.app_id, document);
        }
    }
    // DLC, tools, soundtracks and other non-games are not rows unless installed.
    let apps: Vec<_> = apps
        .into_iter()
        .filter(|app| {
            installed.contains_key(&app.app_id)
                || app
                    .app_type
                    .as_deref()
                    .is_none_or(|kind| kind.eq_ignore_ascii_case("game"))
        })
        .collect();
    let wanted: Vec<u32> = apps
        .iter()
        .map(|app| app.app_id)
        .filter(|id| docs.contains_key(id))
        .collect();
    warnings.extend(
        fetch_sources(
            connection,
            &wanted,
            &owned,
            &library.owned_depots,
            &mut docs,
        )
        .await,
    );
    let mut items = Vec::new();
    let mut rows = Vec::new();
    for app in apps {
        let existing = installed.remove(&app.app_id);
        let (plan, update, reason) = if unresolved.contains(&app.app_id) {
            (
                None,
                UpdateStatus::Unknown,
                Some("Steam app metadata is currently unavailable".to_owned()),
            )
        } else {
            match resolve(
                app.app_id,
                existing.as_ref(),
                &owned,
                &library.owned_depots,
                &docs,
            ) {
                Ok(plan) => {
                    let update = existing
                        .as_ref()
                        .map(|g| compare(g, &plan))
                        .unwrap_or(UpdateStatus::Unknown);
                    (Some(plan), update, None)
                }
                Err(error) => {
                    let reason = error.to_string();
                    (
                        None,
                        UpdateStatus::Unavailable(reason.clone()),
                        Some(reason),
                    )
                }
            }
        };
        let item = item(Some(app), existing, plan, update, reason, &history);
        rows.push(cache::Row {
            app_id: item.app_id,
            name: item.name.clone(),
            plan: item.plan.clone(),
            update: item.update.clone(),
            reason: item.unavailable_reason.clone(),
        });
        items.push(item);
    }
    let incomplete = !library.unresolved_packages.is_empty();
    // Cache any genuinely refreshed catalog with at least one resolved app. Real
    // accounts almost always have a few unrefreshable legacy packages; those
    // games are simply absent from the quick-start cache until the live load.
    // The library's cached-ownership fallback marks every app unresolved and must
    // not overwrite good rows.
    let cacheable = !rows.is_empty() && unresolved.len() < owned.len();
    items.extend(installed.into_values().map(|game| {
        let reason = if incomplete {
            "Steam package metadata is incomplete; ownership and update status are unknown"
        } else {
            "No current license was resolved for this installed game"
        }
        .to_owned();
        let update = if incomplete {
            UpdateStatus::Unknown
        } else {
            UpdateStatus::Unavailable(reason.clone())
        };
        item(None, Some(game), None, update, Some(reason), &history)
    }));
    items.sort_by_key(|item| item.app_id);
    if cacheable && let Some(account) = account.map(str::to_owned) {
        let carry = unresolved.clone();
        match tokio::task::spawn_blocking(move || {
            cache::write(&cache::directory()?, &account, rows, &carry)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warnings.push(format!("Catalog cache not saved: {error}")),
            Err(error) => warnings.push(format!("Catalog cache not saved: {error}")),
        }
    }
    Ok(CatalogSnapshot { items, warnings })
}

fn item(
    app: Option<library::LibraryApp>,
    installed: Option<InstalledGame>,
    plan: Option<InstallPlan>,
    update: UpdateStatus,
    unavailable_reason: Option<String>,
    history: &BTreeMap<u32, Node>,
) -> CatalogItem {
    let app_id = app
        .as_ref()
        .map(|a| a.app_id)
        .or_else(|| installed.as_ref().map(|g| g.app_id))
        .expect("catalog row has identity");
    let record = history.get(&app_id);
    CatalogItem {
        app_id,
        name: app
            .and_then(|a| a.name)
            .or_else(|| installed.as_ref().map(|g| g.name.clone()))
            .unwrap_or_else(|| format!("App {app_id}")),
        last_played: installed.as_ref().and_then(|g| g.last_played).or_else(|| {
            record
                .and_then(|n| n.text("LastPlayed"))
                .and_then(|s| s.parse().ok())
                .filter(|n| *n > 86_400)
        }),
        playtime_minutes: installed
            .as_ref()
            .and_then(|g| g.playtime_minutes)
            .or_else(|| {
                record
                    .and_then(|n| n.text("Playtime"))
                    .and_then(|s| s.parse().ok())
            }),
        size_bytes: installed
            .as_ref()
            .and_then(|g| g.size_bytes)
            .or_else(|| plan.as_ref().map(|p| p.installed_bytes)),
        installed,
        plan,
        update,
        unavailable_reason,
    }
}

/// Plan for a free Steam Play tool/runtime, which no package of the account grants.
pub async fn plan_tool(connection: &Connection, app_id: u32) -> Result<InstallPlan> {
    let (mut found, _) = library::inspect_apps(connection, &BTreeSet::from([app_id])).await?;
    let document = found
        .remove(&app_id)
        .and_then(|app| app.document)
        .ok_or_else(|| failure("Steam app metadata is unavailable"))?;
    let docs = BTreeMap::from([(app_id, document)]);
    resolve(
        app_id,
        None,
        &BTreeSet::from([app_id]),
        &BTreeSet::new(),
        &docs,
    )
}

pub async fn plan(
    connection: &Connection,
    app_id: u32,
    existing: Option<&InstalledGame>,
) -> Result<InstallPlan> {
    let (owned, depots) = match library::cached_ownership(connection) {
        Some(ownership) => ownership,
        None => {
            let library = library::load(connection).await?;
            (
                library.apps.iter().map(|a| a.app_id).collect(),
                library.owned_depots,
            )
        }
    };
    if !owned.contains(&app_id) {
        return Err(failure("No current license was resolved for this app"));
    }
    let (mut found, _) = library::inspect_apps(connection, &BTreeSet::from([app_id])).await?;
    let document = found
        .remove(&app_id)
        .and_then(|app| app.document)
        .ok_or_else(|| failure("Steam app metadata is unavailable"))?;
    let mut docs = BTreeMap::from([(app_id, document)]);
    // Source-fetch failures only matter if a selected depot needs the source.
    fetch_sources(connection, &[app_id], &owned, &depots, &mut docs).await;
    resolve(app_id, existing, &owned, &depots, &docs)
}

/// Shared-depot source apps named by `source`'s selectable depots. Redistributable
/// (`sharedinstall`) depots are never installed, so their sources are never
/// fetched. Sorted and deduplicated.
fn references(
    docs: &BTreeMap<u32, Node>,
    source: u32,
    owned: &BTreeSet<u32>,
    licensed_depots: &BTreeSet<u32>,
) -> BTreeSet<u32> {
    docs.get(&source)
        .and_then(|doc| doc.child("depots"))
        .into_iter()
        .flat_map(|d| d.children.iter())
        .filter(|(id, d)| {
            let id = id.parse::<u32>().unwrap_or(0);
            let dlc = d.text("dlcappid").and_then(|s| s.parse::<u32>().ok());
            !metadata::shared_install(d)
                && dlc.is_none_or(|dlc| owned.contains(&dlc) || licensed_depots.contains(&id))
                && (licensed_depots.is_empty()
                    || licensed_depots.contains(&id)
                    || owned.contains(&id))
        })
        .filter_map(|(_, d)| d.text("depotfromapp").and_then(|s| s.parse::<u32>().ok()))
        .filter(|id| *id != source && *id != 0)
        .collect()
}

/// Fetch every shared-source app document reachable from `roots`, once each, in
/// bounded concurrent waves: all unseen references of a wave go out as one
/// batched request set. Failures and unresolved apps are not retried within the
/// call; they are omitted from `docs`, which later blocks only the games that need
/// them. Returns warnings for failed fetches.
async fn fetch_sources(
    connection: &Connection,
    roots: &[u32],
    owned: &BTreeSet<u32>,
    licensed_depots: &BTreeSet<u32>,
    docs: &mut BTreeMap<u32, Node>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut tried = BTreeSet::new();
    let mut expand: BTreeSet<u32> = roots.iter().copied().collect();
    for _ in 0..MAX_SOURCE_WAVES {
        let mut wanted = BTreeSet::new();
        for app in &expand {
            for id in references(docs, *app, owned, licensed_depots) {
                if !docs.contains_key(&id) && !tried.contains(&id) {
                    wanted.insert(id);
                }
            }
        }
        // Deterministic bound: the lowest IDs win, the rest stay unfetched.
        let room = MAX_SOURCE_APPS.saturating_sub(tried.len());
        if wanted.len() > room {
            warnings.push(format!(
                "{} shared-depot source apps exceed the {MAX_SOURCE_APPS} app limit and were not fetched",
                wanted.len() - room
            ));
            wanted = wanted.into_iter().take(room).collect();
        }
        if wanted.is_empty() {
            break;
        }
        tried.extend(wanted.iter().copied());
        expand.clear();
        match library::inspect_apps(connection, &wanted).await {
            Ok((found, _unresolved)) => {
                for (id, app) in found {
                    if let Some(document) = app.document {
                        docs.insert(id, document);
                        expand.insert(id);
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "Shared-depot source metadata unavailable for {} apps: {error}",
                wanted.len()
            )),
        }
        if expand.is_empty() {
            break;
        }
    }
    warnings
}

/// Shared-depot references form a bounded graph, not a recursive chain.
fn check_graph(
    docs: &BTreeMap<u32, Node>,
    app_id: u32,
    owned: &BTreeSet<u32>,
    licensed_depots: &BTreeSet<u32>,
) -> Result<()> {
    let mut frontier = vec![app_id];
    let mut seen = BTreeSet::new();
    while let Some(source) = frontier.pop() {
        if !seen.insert(source) {
            continue;
        }
        if seen.len() > MAX_GRAPH_APPS {
            return Err(failure(format!(
                "Shared-depot graph exceeds {MAX_GRAPH_APPS} apps"
            )));
        }
        frontier.extend(
            references(docs, source, owned, licensed_depots)
                .into_iter()
                .filter(|id| docs.contains_key(id)),
        );
    }
    Ok(())
}

fn resolve(
    app_id: u32,
    existing: Option<&InstalledGame>,
    owned: &BTreeSet<u32>,
    licensed_depots: &BTreeSet<u32>,
    docs: &BTreeMap<u32, Node>,
) -> Result<InstallPlan> {
    check_graph(docs, app_id, owned, licensed_depots)?;
    metadata::select(app_id, existing, owned, licensed_depots, docs)
}

/// Compares the installed build and the manifests of the depots the plan names.
/// Installed depots the plan does not name (shared redistributables Steam recorded
/// in the manifest, depots no longer offered, unowned DLC) are not update triggers:
/// this installer cannot change them.
fn compare(installed: &InstalledGame, plan: &InstallPlan) -> UpdateStatus {
    let Some(build) = installed.build_id else {
        return UpdateStatus::Unknown;
    };
    if build != plan.build_id {
        return UpdateStatus::Available {
            installed_build: build,
            latest_build: plan.build_id,
        };
    }
    if installed.installed_depots.is_empty() {
        return UpdateStatus::Unknown;
    }
    let desired: BTreeMap<_, _> = plan
        .depots
        .iter()
        .map(|d| (d.depot_id, d.manifest_id))
        .collect();
    let present: BTreeMap<_, _> = installed
        .installed_depots
        .iter()
        .filter(|(id, _)| desired.contains_key(id))
        .map(|(id, manifest)| (*id, *manifest))
        .collect();
    if desired != present {
        UpdateStatus::Available {
            installed_build: build,
            latest_build: plan.build_id,
        }
    } else {
        UpdateStatus::UpToDate
    }
}
