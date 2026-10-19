//! User-facing content choices use the same ownership and DLC selection as the
//! game backend. A desired choice never claims that bytes have been installed.
use crate::catalog::ContentSelection;
use crate::installed::{InstalledGame, Result, failure};
use crate::updates::{Intent, Queue};
use std::collections::BTreeMap;
use steam_vent::Connection;

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub password_required: bool,
}
#[derive(Clone, Debug)]
pub struct Dlc {
    pub app_id: u32,
    pub name: String,
    pub owned: bool,
    pub enabled: bool,
    pub installed: bool,
}
#[derive(Clone, Debug)]
pub struct Properties {
    pub app_id: u32,
    pub branches: Vec<Branch>,
    pub languages: Vec<String>,
    pub dlc: Vec<Dlc>,
}

pub async fn properties(connection: &Connection, game: &InstalledGame) -> Result<Properties> {
    let library = crate::library::load(connection)
        .await
        .map_err(|e| failure(e.to_string()))?;
    if !library.apps.iter().any(|a| a.app_id == game.app_id) {
        return Err(failure("No current license for this game"));
    }
    let app = crate::library::inspect_app(connection, game.app_id)
        .await
        .map_err(|e| failure(e.to_string()))?
        .and_then(|a| a.document)
        .ok_or_else(|| failure("App metadata unavailable"))?;
    let branches = app
        .at(&["depots", "branches"])
        .into_iter()
        .flat_map(|n| &n.children)
        .map(|(name, branch)| Branch {
            name: name.clone(),
            password_required: branch
                .text("pwdrequired")
                .is_some_and(|s| s != "0" && !s.eq_ignore_ascii_case("false")),
        })
        .collect();
    let ids = crate::dlc::ids(game.app_id, &app)?;
    let (children, _) = crate::library::inspect_apps(connection, &ids)
        .await
        .map_err(|e| failure(e.to_string()))?;
    let docs: BTreeMap<_, _> = children
        .into_iter()
        .filter_map(|(id, a)| a.document.map(|d| (id, d)))
        .collect();
    let entries = crate::dlc::entries(game, &app, &docs)?;
    let selection = crate::dlc::Selection::read(
        &game.library_root,
        connection.steam_id().steam64(),
        game.app_id,
    )?;
    let snapshot = crate::dlc::snapshot(game, &entries, &selection);
    let dlc = snapshot
        .dlc
        .into_iter()
        .map(|d| {
            let owned = library.apps.iter().any(|a| a.app_id == d.app_id)
                || !d.depots.is_empty()
                    && d.depots.iter().all(|id| library.owned_depots.contains(id));
            Dlc {
                app_id: d.app_id,
                name: d.name,
                owned,
                enabled: d.enabled,
                installed: owned && d.installed,
            }
        })
        .collect();
    Ok(Properties {
        app_id: game.app_id,
        branches,
        languages: crate::catalog::languages(&app).into_iter().collect(),
        dlc,
    })
}

pub async fn select(
    connection: &Connection,
    queue: &Queue,
    game: &InstalledGame,
    selection: ContentSelection,
) -> Result<u64> {
    if crate::launch::is_running(game.app_id) {
        return Err(failure(
            "Close the game before changing its branch or language",
        ));
    }
    // Resolve the complete desired target before changing persisted intent.
    crate::catalog::plan_selected(connection, game.app_id, Some(game), &selection)
        .await
        .map_err(|e| failure(e.to_string()))?;
    let mut settings = queue.settings()?;
    settings.selections.insert(game.app_id, selection);
    settings.policies.entry(game.app_id).or_default();
    queue.save_settings(settings)?;
    queue.enqueue(Intent::installed(game), true)
}

pub async fn select_dlc(
    connection: &Connection,
    queue: &Queue,
    game: &InstalledGame,
    dlc: u32,
    enabled: bool,
) -> Result<u64> {
    if crate::launch::is_running(game.app_id) {
        return Err(failure(
            "Close the game before changing DLC in content properties",
        ));
    }
    let props = properties(connection, game).await?;
    let entry = props
        .dlc
        .iter()
        .find(|d| d.app_id == dlc)
        .ok_or_else(|| failure("DLC does not belong to this game"))?;
    if enabled && !entry.owned {
        return Err(failure("No current license for this DLC"));
    }
    crate::dlc::Selection::update(
        &game.library_root,
        connection.steam_id().steam64(),
        game.app_id,
        |selection| {
            if enabled {
                selection.disabled.remove(&dlc);
            } else {
                selection.disabled.insert(dlc);
            }
        },
    )?;
    queue.enqueue(Intent::installed(game), true)
}
