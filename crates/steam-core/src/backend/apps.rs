//! Parent-scoped DLC service. Selection is durable; files become installed only
//! after the scoped installer commits. Downloads never block callback polling.
use super::*;
use crate::dlc::{self, Entry, Selection};
use crate::installed::{InstalledGame, Node};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use steam_ipc::{AppsOp, AppsSnapshot};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) struct Context {
    game: InstalledGame,
    document: Node,
    entries: Vec<Entry>,
}

impl Context {
    pub(crate) async fn prepare(
        connection: &Connection,
        mut game: InstalledGame,
        document: Node,
        platform: crate::catalog::InstallPlatform,
    ) -> crate::installed::Result<Self> {
        game.platform = Some(platform);
        let ids = dlc::ids(game.app_id, &document)?;
        let mut children = BTreeMap::new();
        for &id in &ids {
            if let Ok(Some(doc)) = crate::library::cached_app(id) {
                children.insert(id, doc);
            }
        }
        if !ids.is_empty()
            && let Ok(Ok((found, _))) = timeout(
                Duration::from_secs(120),
                crate::library::inspect_apps(connection, &ids),
            )
            .await
        {
            for (id, app) in found {
                if let Some(doc) = app.document {
                    children.insert(id, doc);
                }
            }
        }
        let entries = dlc::entries(&game, &document, &children)?;
        Ok(Self {
            game,
            document,
            entries,
        })
    }

    fn installed(&self) -> crate::installed::Result<InstalledGame> {
        let game = &self.game;
        let file = game
            .library_root
            .join(format!("steamapps/appmanifest_{}.acf", game.app_id));
        crate::install::filesystem::checked(
            &game.library_root,
            std::path::Path::new(&format!("steamapps/appmanifest_{}.acf", game.app_id)),
            true,
        )?;
        let mut current = crate::installed::manifest_game(
            &file,
            &game.library_root,
            &game.steam_root,
            &BTreeMap::new(),
        )?
        .ok_or_else(|| crate::installed::failure("Parent game is no longer installed"))?;
        if current.install_dir != game.install_dir
            || current.build_id != game.build_id
            || current.branch != game.branch
            || current.language != game.language
            || current
                .platform
                .is_some_and(|platform| Some(platform) != game.platform)
        {
            return Err(crate::installed::failure(
                "Parent installation changed; restart the game/backend",
            ));
        }
        current.platform = game.platform;
        Ok(current)
    }
}

#[derive(Default)]
struct Active {
    id: u32,
    enabled: bool,
    downloading: bool,
    done: u64,
    total: u64,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<(u32, bool)>,
    active: Option<Active>,
    callbacks: Vec<(i32, Vec<u8>)>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) struct Service {
    connection: Connection,
    context: Option<Arc<Context>>,
    shared: Arc<Mutex<Shared>>,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    started: bool,
    events: Option<EventSender>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.notify.notify_one();
    }
}

impl Service {
    pub(super) fn new(
        connection: &Connection,
        context: Option<Arc<Context>>,
        events: Option<EventSender>,
    ) -> Self {
        Self {
            connection: connection.clone(),
            context,
            shared: Arc::default(),
            notify: Arc::new(Notify::new()),
            cancel: CancellationToken::new(),
            started: false,
            events,
        }
    }

    pub(super) fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        std::mem::take(&mut lock(&self.shared).callbacks)
    }

    fn start(&mut self) -> crate::installed::Result<Arc<Context>> {
        let context = self.context.clone().ok_or_else(|| {
            crate::installed::failure("No parent installation supplied to the backend")
        })?;
        if self.started {
            return Ok(context);
        }
        let selection = Selection::read(
            &context.game.library_root,
            self.connection.steam_id().steam64(),
            context.game.app_id,
        )?;
        {
            let mut shared = lock(&self.shared);
            for (id, enabled) in selection.pending {
                if context.entries.iter().any(|e| e.app_id == id) {
                    shared.queue.push_back((id, enabled));
                }
            }
        }
        self.started = true;
        let (connection, context_task, shared, notify, cancel, events) = (
            self.connection.clone(),
            context.clone(),
            self.shared.clone(),
            self.notify.clone(),
            self.cancel.clone(),
            self.events.clone(),
        );
        tokio::task::spawn_local(async move {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let next = {
                    let mut state = lock(&shared);
                    let next = state.queue.pop_front();
                    if let Some((id, enabled)) = next {
                        state.active = Some(Active {
                            id,
                            enabled,
                            ..Default::default()
                        });
                    }
                    next
                };
                let Some((id, enabled)) = next else {
                    tokio::select! { _ = cancel.cancelled() => break, _ = notify.notified() => {} }
                    continue;
                };
                let result = run_job(
                    &connection,
                    &context_task,
                    id,
                    enabled,
                    &shared,
                    &cancel,
                    &events,
                )
                .await;
                lock(&shared).active = None;
                if let Err(error) = result {
                    events::emit(
                        &events,
                        context_task.game.app_id,
                        Direction::Local,
                        "DLC",
                        format!(
                            "DLC {id} {} failed",
                            if enabled { "install" } else { "removal" }
                        ),
                        error.to_string(),
                    );
                    // Intent remains durable for retry after restart or another
                    // explicit request. Never spin on a failing job.
                }
            }
        });
        self.notify.notify_one();
        Ok(context)
    }

    pub(super) async fn handle(&mut self, operation: AppsOp) -> Response {
        let result = self.handle_inner(operation).await;
        result.unwrap_or_else(|error| Response::Error {
            message: error.to_string(),
        })
    }

    async fn handle_inner(&mut self, operation: AppsOp) -> crate::installed::Result<Response> {
        let context = self.start()?;
        let account = self.connection.steam_id().steam64();
        match operation {
            AppsOp::Snapshot => {
                let (ctx, path) = (context.clone(), context.game.library_root.clone());
                let mut snapshot = tokio::task::spawn_blocking(move || {
                    let game = ctx.installed()?;
                    let selection = Selection::read(&path, account, game.app_id)?;
                    Ok::<_, crate::install::Error>(dlc::snapshot(&game, &ctx.entries, &selection))
                })
                .await??;
                let shared = lock(&self.shared);
                overlay_progress(&mut snapshot, &shared);
                Ok(Response::Apps(snapshot))
            }
            AppsOp::InstallDlc { app_id } | AppsOp::UninstallDlc { app_id } => {
                let enabled = matches!(operation, AppsOp::InstallDlc { .. });
                let entry = context
                    .entries
                    .iter()
                    .find(|e| e.app_id == app_id)
                    .ok_or_else(|| {
                        crate::installed::failure("DLC is not associated with the running game")
                    })?;
                if enabled && !entry.known {
                    return Err(crate::installed::failure(
                        "Applicable DLC content metadata is unavailable",
                    ));
                }
                if lock(&self.shared).callbacks.len() >= dlc::MAX_DLC {
                    return Err(crate::installed::failure(
                        "Pump callbacks before queuing more DLC requests",
                    ));
                }
                let ctx = context.clone();
                tokio::task::spawn_blocking(move || {
                    Selection::update(
                        &ctx.game.library_root,
                        account,
                        ctx.game.app_id,
                        |selection| {
                            if enabled {
                                selection.disabled.remove(&app_id);
                            } else {
                                selection.disabled.insert(app_id);
                            }
                            selection.pending.insert(app_id, enabled);
                        },
                    )
                })
                .await??;
                let mut shared = lock(&self.shared);
                if shared
                    .active
                    .as_ref()
                    .is_none_or(|active| active.id != app_id || active.enabled != enabled)
                {
                    shared.queue.retain(|(id, _)| *id != app_id);
                    shared.queue.push_back((app_id, enabled));
                }
                drop(shared);
                self.notify.notify_one();
                Ok(Response::AppsAccepted)
            }
        }
    }
}

fn overlay_progress(snapshot: &mut AppsSnapshot, shared: &Shared) {
    if let Some(active) = &shared.active
        && let Some(dlc) = snapshot.dlc.iter_mut().find(|d| d.app_id == active.id)
    {
        dlc.downloading = active.enabled && active.downloading;
        dlc.downloaded = active.done.min(active.total);
        dlc.total = active.total;
    }
}

fn installed_callback(
    target: u32,
    enabled: bool,
    owned: bool,
    snapshot: &AppsSnapshot,
) -> Option<(i32, Vec<u8>)> {
    (enabled
        && owned
        && snapshot
            .dlc
            .iter()
            .any(|d| d.app_id == target && d.enabled && d.installed))
    .then(|| (1005, target.to_le_bytes().to_vec()))
}

async fn run_job(
    connection: &Connection,
    context: &Context,
    target: u32,
    enabled: bool,
    shared: &Mutex<Shared>,
    cancel: &CancellationToken,
    events: &Option<EventSender>,
) -> crate::installed::Result<()> {
    let failure = crate::installed::failure;
    let account = connection.steam_id().steam64();
    let entry = context
        .entries
        .iter()
        .find(|e| e.app_id == target)
        .ok_or_else(|| failure("Unknown DLC"))?;
    let mut ownership = super::ownership::Service::default();
    if enabled
        && !matches!(
            ownership
                .query(connection, context.game.app_id, target, events)
                .await,
            Response::Ownership { owned: true }
        )
    {
        return Err(failure("DLC ownership could not be confirmed"));
    }
    if cancel.is_cancelled() {
        return Err(failure("DLC job cancelled"));
    }
    let mut game = context.installed()?;
    let selection = Selection::read(&game.library_root, account, game.app_id)?;
    // Requests superseded before work begins must not alter content or emit a callback.
    if selection.pending.get(&target) != Some(&enabled) {
        return Ok(());
    }
    if enabled {
        install_shared(connection, context, &game, target, cancel).await?;
    }
    let scope = dlc::depot_scope(&context.document, target);
    if !scope.is_empty() {
        let plan = tokio::select! {
            _ = cancel.cancelled() => return Err(failure("DLC job cancelled")),
            result = crate::catalog::plan_dlc(connection, &game, context.document.clone(), &selection, target) =>
                result.map_err(|e| crate::installed::failure(e.to_string()))?,
        };
        if enabled
            && entry
                .depots
                .iter()
                .any(|id| !plan.depots.iter().any(|d| d.depot_id == *id))
        {
            return Err(failure(
                "Current licenses do not grant every required DLC depot",
            ));
        }
        let request = crate::install::InstallRequest {
            plan,
            destination: game.library_root.clone(),
            steam_root: game.steam_root.clone(),
            account: account.to_string(),
            existing: Some(game.clone()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let job = crate::install::run_dlc(connection.clone(), request, tx, cancel.clone(), scope);
        tokio::pin!(job);
        loop {
            tokio::select! {
                result = &mut job => { game = result?; break; }
                Some(progress) = rx.recv() => {
                    if let Some(active) = &mut lock(shared).active {
                        active.done = progress.completed_bytes.min(progress.total_bytes);
                        active.total = progress.total_bytes;
                        active.downloading = matches!(progress.phase, crate::install::InstallPhase::Downloading | crate::install::InstallPhase::Verifying | crate::install::InstallPhase::Committing);
                    }
                }
            }
        }
    }
    if cancel.is_cancelled() {
        return Err(failure(
            "DLC job cancelled after commit; completion will reconcile on restart",
        ));
    }
    let final_selection = Selection::read(&game.library_root, account, game.app_id)?;
    let mut owned_after_commit = false;
    if enabled && !final_selection.disabled.contains(&target) {
        let snapshot = dlc::snapshot(&game, &context.entries, &final_selection);
        if !snapshot
            .dlc
            .iter()
            .any(|d| d.app_id == target && d.installed)
        {
            return Err(failure("DLC content did not reach installed state"));
        }
        ownership.clear();
        if !matches!(
            ownership
                .query(connection, game.app_id, target, events)
                .await,
            Response::Ownership { owned: true }
        ) {
            return Err(failure("DLC ownership changed before completion"));
        }
        owned_after_commit = true;
    }
    if cancel.is_cancelled() {
        return Err(failure("DLC completion cancelled"));
    }
    let final_selection =
        Selection::update(&game.library_root, account, game.app_id, |selection| {
            if selection.pending.get(&target) == Some(&enabled) {
                selection.pending.remove(&target);
            }
        })?;
    if let Some(callback) = installed_callback(
        target,
        enabled,
        owned_after_commit,
        &dlc::snapshot(&game, &context.entries, &final_selection),
    ) {
        lock(shared).callbacks.push(callback);
    }
    events::emit(
        events,
        game.app_id,
        Direction::Local,
        "DLC",
        format!(
            "DLC {target} {}",
            if enabled { "installed" } else { "removed" }
        ),
        "Verified content transaction completed",
    );
    Ok(())
}

async fn install_shared(
    connection: &Connection,
    context: &Context,
    game: &InstalledGame,
    target: u32,
    cancel: &CancellationToken,
) -> crate::installed::Result<()> {
    let mut doc = context.document.clone();
    if let Some((_, depots)) = doc
        .children
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("depots"))
    {
        depots
            .children
            .retain(|_, d| d.text("dlcappid").and_then(|s| s.parse::<u32>().ok()) == Some(target));
    }
    let platform = game
        .platform
        .ok_or_else(|| crate::installed::failure("Unknown DLC platform"))?;
    let requirements = crate::catalog::shared::requirements(&doc, platform, &game.language)?;
    for (source, requested) in requirements {
        if source == game.app_id {
            return Err(crate::installed::failure(
                "DLC shared prerequisite references its parent game",
            ));
        }
        let existing = crate::catalog::shared::installed(game, source)?;
        let plan = tokio::select! {
            _ = cancel.cancelled() => return Err(crate::installed::failure("DLC prerequisite installation cancelled")),
            plan = crate::catalog::shared::plan(connection, source, &requested, platform, existing.as_ref()) =>
                plan.map_err(|e| crate::installed::failure(e.to_string()))?,
        };
        if existing.as_ref().is_some_and(|old| {
            plan.depots
                .iter()
                .all(|d| old.installed_depots.get(&d.depot_id) == Some(&d.manifest_id))
        }) {
            continue;
        }
        let request = crate::install::InstallRequest {
            plan,
            destination: game.library_root.clone(),
            steam_root: game.steam_root.clone(),
            account: connection.steam_id().steam64().to_string(),
            existing,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        crate::install::run(
            connection.clone(),
            request,
            tx,
            cancel.clone(),
            crate::install::PauseGate::default(),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_require_ownership_and_committed_selected_content_and_have_sdk_layout() {
        let mut snapshot = AppsSnapshot {
            dlc: vec![steam_ipc::DlcInfo {
                app_id: 123,
                enabled: true,
                installed: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(installed_callback(123, true, true, &snapshot).is_none());
        snapshot.dlc[0].installed = true;
        assert!(installed_callback(123, true, false, &snapshot).is_none());
        assert!(installed_callback(123, false, true, &snapshot).is_none());
        assert!(installed_callback(124, true, true, &snapshot).is_none());
        assert_eq!(
            installed_callback(123, true, true, &snapshot),
            Some((1005, vec![123, 0, 0, 0]))
        );
        snapshot.dlc[0].enabled = false;
        assert!(installed_callback(123, true, true, &snapshot).is_none());
    }

    #[test]
    fn queued_preparing_and_removal_jobs_do_not_report_active_downloads() {
        let mut snapshot = AppsSnapshot {
            dlc: vec![steam_ipc::DlcInfo {
                app_id: 123,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut shared = Shared::default();
        shared.queue.push_back((123, true));
        overlay_progress(&mut snapshot, &shared);
        assert!(!snapshot.dlc[0].downloading);
        shared.active = Some(Active {
            id: 123,
            enabled: true,
            ..Default::default()
        });
        overlay_progress(&mut snapshot, &shared);
        assert!(!snapshot.dlc[0].downloading);
        let active = shared.active.as_mut().unwrap();
        active.downloading = true;
        active.done = 11;
        active.total = 10;
        overlay_progress(&mut snapshot, &shared);
        assert!(snapshot.dlc[0].downloading);
        assert_eq!(
            (snapshot.dlc[0].downloaded, snapshot.dlc[0].total),
            (10, 10)
        );
        shared.active.as_mut().unwrap().enabled = false;
        overlay_progress(&mut snapshot, &shared);
        assert!(!snapshot.dlc[0].downloading);
    }
}
