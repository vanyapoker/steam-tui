//! Workshop install service of the per-launch backend: tracks the account's
//! subscribed items, downloads missing or outdated ones one at a time in the
//! background, and reports state to the game (`ISteamUGC::GetItemState`,
//! `DownloadItem`, `ItemInstalled_t`, `DownloadItemResult_t`).
//! Items live in the game's library under `steamapps/workshop/content/<app>/`.
//! Nothing is started until the game first asks about Workshop items.
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use steam_ipc::{Response, UgcItem, UgcOp, WorkshopItem};
use steam_vent::Connection;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::events::{self, Direction, EventSender};
use super::ugc;
use crate::catalog::DepotSpec;
use crate::content::ContentClient;
use crate::install::PauseGate;
use crate::install::workshop::{self, InstalledItem, Store};

/// Items kept in memory for one launch.
const MAX_TRACKED: usize = 4096;
const E_OK: i32 = 1;
const E_FAIL: i32 = 2;
const E_INVALID_PARAM: i32 = 8;
/// k_iSteamUGCCallbacks + 5 and + 6.
const ITEM_INSTALLED: i32 = 3405;
const DOWNLOAD_ITEM_RESULT: i32 = 3406;
/// Seconds to wait before asking Steam for the subscription list again after a failure.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

struct Tracked {
    item: UgcItem,
    subscribed: bool,
    installed: Option<InstalledItem>,
    /// EResult of the last failed download.
    failed: i32,
}

impl Tracked {
    fn legacy(&self) -> bool {
        !self.item.file_url.is_empty()
    }

    /// An installed copy is stale when it is not the version the service now publishes.
    fn needs_update(&self) -> bool {
        match &self.installed {
            None => false,
            Some(installed) if self.legacy() => {
                self.item.updated != 0 && installed.time_updated != self.item.updated
            }
            Some(installed) => {
                self.item.file_handle != 0 && installed.manifest != self.item.file_handle
            }
        }
    }

    /// Whether a download is wanted: nothing installed, or an outdated copy.
    fn wants_download(&self) -> bool {
        self.installed.is_none() || self.needs_update()
    }
}

#[derive(Default)]
struct Active {
    id: u64,
    done: u64,
    total: u64,
}

#[derive(Default)]
struct Shared {
    loaded: bool,
    ready: bool,
    last_attempt: Option<std::time::Instant>,
    tracked: BTreeMap<u64, Tracked>,
    queue: VecDeque<u64>,
    active: Option<Active>,
    callbacks: Vec<(i32, Vec<u8>)>,
}

pub struct Service {
    connection: Connection,
    app_id: u32,
    store: Option<Arc<Store>>,
    shared: Arc<Mutex<Shared>>,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    gate: PauseGate,
    events: Option<EventSender>,
    started: std::cell::Cell<bool>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    /// `steamapps` is the game's library `steamapps` directory; without it Workshop installs are unavailable.
    pub fn new(
        connection: &Connection,
        app_id: u32,
        steamapps: Option<PathBuf>,
        events: Option<EventSender>,
    ) -> Self {
        Self {
            connection: connection.clone(),
            app_id,
            store: steamapps.map(|dir| Arc::new(Store::new(&dir, app_id))),
            shared: Arc::default(),
            notify: Arc::new(Notify::new()),
            cancel: CancellationToken::new(),
            gate: PauseGate::default(),
            events,
            started: std::cell::Cell::new(false),
        }
    }

    /// Callbacks queued for the game since the last poll.
    pub fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        std::mem::take(&mut lock(&self.shared).callbacks)
    }

    fn emit(&self, title: impl Into<String>, detail: impl Into<String>) {
        events::emit(
            &self.events,
            self.app_id,
            Direction::Local,
            "Workshop",
            title,
            detail,
        );
    }

    pub async fn handle(&self, op: UgcOp) -> Response {
        match op {
            UgcOp::Snapshot => {
                self.ensure_loaded().await;
                self.snapshot()
            }
            UgcOp::Subscribe { app, id, subscribe } => self.subscribe(app, id, subscribe).await,
            UgcOp::Download { id, high_priority } => self.download(id, high_priority).await,
            UgcOp::Suspend { on } => {
                self.gate.set(on);
                self.notify.notify_one();
                Response::UgcResult { result: E_OK }
            }
            _ => Response::Error {
                message: "Not a Workshop install operation".into(),
            },
        }
    }

    /// Lists the subscriptions once (retrying after failures) and queues what is missing or outdated.
    async fn ensure_loaded(&self) {
        {
            let mut shared = lock(&self.shared);
            if shared.loaded {
                return;
            }
            if shared
                .last_attempt
                .is_some_and(|at| at.elapsed() < RETRY_AFTER)
            {
                return;
            }
            shared.last_attempt = Some(std::time::Instant::now());
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let listed = ugc::subscribed_items(&self.connection, self.app_id).await;
        let items = match listed {
            Ok(items) => items,
            // An app without a Workshop answers with an invalid-parameter error.
            Err(E_INVALID_PARAM) => Vec::new(),
            Err(code) => {
                self.emit("subscription list unavailable", format!("eresult {code}"));
                return;
            }
        };
        let installed = match store.installed() {
            Ok(installed) => installed,
            Err(error) => {
                self.emit("installed items unreadable", error.to_string());
                BTreeMap::new()
            }
        };
        let mut queued = 0;
        {
            let mut shared = lock(&self.shared);
            if shared.loaded {
                return;
            }
            shared.loaded = true;
            shared.ready = true;
            for item in items.into_iter().take(MAX_TRACKED) {
                let id = item.id;
                let tracked = Tracked {
                    installed: installed.get(&id).cloned(),
                    item,
                    subscribed: true,
                    failed: 0,
                };
                if tracked.wants_download() {
                    shared.queue.push_back(id);
                    queued += 1;
                }
                shared.tracked.insert(id, tracked);
            }
            // Items installed earlier (explicit downloads) stay visible, unsubscribed ones
            // marked for removal are cleaned up now that the game is not using them yet.
            for (id, item) in installed {
                if shared.tracked.contains_key(&id) {
                    continue;
                }
                if item.remove_pending {
                    let _ = store.remove(id);
                    continue;
                }
                if shared.tracked.len() >= MAX_TRACKED {
                    break;
                }
                shared.tracked.insert(
                    id,
                    Tracked {
                        item: UgcItem {
                            id,
                            ..Default::default()
                        },
                        subscribed: false,
                        installed: Some(item),
                        failed: 0,
                    },
                );
            }
        }
        self.emit(
            "subscriptions loaded",
            format!("{queued} item(s) to download"),
        );
        self.start_worker();
        self.notify.notify_one();
    }

    fn snapshot(&self) -> Response {
        let shared = lock(&self.shared);
        let items = shared
            .tracked
            .values()
            .map(|t| {
                let id = t.item.id;
                let active = shared.active.as_ref().filter(|a| a.id == id);
                WorkshopItem {
                    id,
                    subscribed: t.subscribed,
                    installed: t.installed.is_some(),
                    legacy: t.legacy(),
                    needs_update: t.needs_update(),
                    downloading: active.is_some(),
                    pending: active.is_none() && shared.queue.contains(&id),
                    size: t.installed.as_ref().map_or(0, |i| i.size),
                    time_updated: t.installed.as_ref().map_or(0, |i| i.time_updated),
                    path: self
                        .store
                        .as_ref()
                        .filter(|_| t.installed.is_some())
                        .map(|s| s.item_dir(id).to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    done: active.map_or(0, |a| a.done),
                    total: active.map_or(0, |a| a.total),
                    failed: t.failed,
                }
            })
            .collect();
        Response::UgcSnapshot {
            ready: shared.ready,
            items,
        }
    }

    async fn subscribe(&self, app: u32, id: u64, subscribe: bool) -> Response {
        self.ensure_loaded().await;
        let reply = ugc::handle(
            &self.connection,
            self.app_id,
            UgcOp::Subscribe {
                app: if app == 0 { self.app_id } else { app },
                id,
                subscribe,
            },
        )
        .await;
        let Response::UgcResult { result } = reply else {
            return reply;
        };
        if result != E_OK {
            return Response::UgcResult { result };
        }
        if subscribe {
            let details = ugc::item_details(&self.connection, self.app_id, vec![id])
                .await
                .ok()
                .and_then(|items| items.into_iter().find(|i| i.id == id));
            self.track(id, details, true);
        } else {
            let mut shared = lock(&self.shared);
            shared.queue.retain(|queued| *queued != id);
            let installed = shared.tracked.get_mut(&id).map(|t| {
                t.subscribed = false;
                t.installed.is_some()
            });
            drop(shared);
            // Deleting under a running game could break it; the files go on the next launch.
            if installed == Some(true)
                && let Some(store) = &self.store
                && let Err(error) = store.mark_remove(id)
            {
                self.emit("could not mark item for removal", error.to_string());
            }
        }
        Response::UgcResult { result: E_OK }
    }

    async fn download(&self, id: u64, high_priority: bool) -> Response {
        self.ensure_loaded().await;
        if self.store.is_none() {
            return Response::UgcResult { result: E_FAIL };
        }
        let known = lock(&self.shared)
            .tracked
            .get(&id)
            .is_some_and(|t| t.item.file_handle != 0 || !t.item.file_url.is_empty());
        let details = if known {
            None
        } else {
            match ugc::item_details(&self.connection, self.app_id, vec![id]).await {
                Ok(items) => items.into_iter().find(|i| i.id == id && i.result == E_OK),
                Err(_) => None,
            }
        };
        if !known && details.is_none() {
            return Response::UgcResult { result: E_FAIL };
        }
        let subscribed = lock(&self.shared)
            .tracked
            .get(&id)
            .is_some_and(|t| t.subscribed);
        self.track(id, details, subscribed);
        if high_priority {
            let mut shared = lock(&self.shared);
            if let Some(position) = shared.queue.iter().position(|queued| *queued == id)
                && position > 0
            {
                shared.queue.remove(position);
                shared.queue.push_front(id);
            }
        }
        Response::UgcResult { result: E_OK }
    }

    /// Starts tracking `id` (with fresh `details` when known) and queues a download when one is due.
    fn track(&self, id: u64, details: Option<UgcItem>, subscribed: bool) {
        let mut shared = lock(&self.shared);
        if shared.tracked.len() >= MAX_TRACKED && !shared.tracked.contains_key(&id) {
            return;
        }
        let installed = self
            .store
            .as_ref()
            .and_then(|s| s.installed().ok())
            .and_then(|mut all| all.remove(&id));
        let tracked = shared.tracked.entry(id).or_insert_with(|| Tracked {
            item: UgcItem {
                id,
                ..Default::default()
            },
            subscribed,
            installed: None,
            failed: 0,
        });
        if let Some(details) = details {
            tracked.item = details;
        }
        tracked.subscribed = subscribed;
        tracked.installed = installed;
        tracked.failed = 0;
        let wanted = tracked.wants_download();
        if wanted && !shared.queue.contains(&id) {
            shared.queue.push_back(id);
        }
        drop(shared);
        self.start_worker();
        self.notify.notify_one();
    }

    fn start_worker(&self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.started.replace(true) {
            return;
        }
        tokio::task::spawn_local(worker(Worker {
            connection: self.connection.clone(),
            app_id: self.app_id,
            store,
            shared: self.shared.clone(),
            notify: self.notify.clone(),
            cancel: self.cancel.clone(),
            gate: self.gate.clone(),
            events: self.events.clone(),
        }));
    }
}

struct Worker {
    connection: Connection,
    app_id: u32,
    store: Arc<Store>,
    shared: Arc<Mutex<Shared>>,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    gate: PauseGate,
    events: Option<EventSender>,
}

/// ItemInstalled_t (pack 4): app id, published file id, legacy content handle, manifest id.
fn installed_payload(app: u32, id: u64, manifest: u64) -> Vec<u8> {
    let mut out = app.to_le_bytes().to_vec();
    out.extend(id.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend(manifest.to_le_bytes());
    out
}

/// DownloadItemResult_t (pack 4): app id, published file id, EResult.
fn result_payload(app: u32, id: u64, result: i32) -> Vec<u8> {
    let mut out = app.to_le_bytes().to_vec();
    out.extend(id.to_le_bytes());
    out.extend(result.to_le_bytes());
    out
}

async fn worker(w: Worker) {
    let mut client: Option<ContentClient> = None;
    loop {
        let next = {
            let mut shared = lock(&w.shared);
            if w.gate.is_paused() {
                None
            } else {
                shared.queue.pop_front().inspect(|id| {
                    shared.active = Some(Active {
                        id: *id,
                        ..Default::default()
                    });
                })
            }
        };
        let Some(id) = next else {
            tokio::select! {
                _ = w.cancel.cancelled() => return,
                _ = w.notify.notified() => {}
            }
            continue;
        };
        let item = lock(&w.shared).tracked.get(&id).map(|t| t.item.clone());
        let outcome = match item {
            Some(item) => install_one(&w, &mut client, &item).await,
            None => Err(E_FAIL),
        };
        let mut shared = lock(&w.shared);
        shared.active = None;
        match outcome {
            Ok(installed) => {
                let manifest = installed.manifest;
                if let Some(tracked) = shared.tracked.get_mut(&id) {
                    tracked.installed = Some(installed);
                    tracked.failed = 0;
                }
                shared
                    .callbacks
                    .push((DOWNLOAD_ITEM_RESULT, result_payload(w.app_id, id, E_OK)));
                shared
                    .callbacks
                    .push((ITEM_INSTALLED, installed_payload(w.app_id, id, manifest)));
            }
            Err(code) => {
                if let Some(tracked) = shared.tracked.get_mut(&id) {
                    tracked.failed = code;
                }
                shared
                    .callbacks
                    .push((DOWNLOAD_ITEM_RESULT, result_payload(w.app_id, id, code)));
            }
        }
        drop(shared);
        if w.cancel.is_cancelled() {
            return;
        }
    }
}

/// Downloads one item; the error is the EResult reported to the game.
async fn install_one(
    w: &Worker,
    client: &mut Option<ContentClient>,
    item: &UgcItem,
) -> Result<InstalledItem, i32> {
    let subscriber = w.connection.steam_id().account_id();
    let id = item.id;
    let shared = w.shared.clone();
    let mut progress = move |done: u64, total: u64| {
        if let Some(active) = lock(&shared).active.as_mut() {
            active.done = done;
            active.total = total;
        }
    };
    events::emit(
        &w.events,
        w.app_id,
        Direction::Local,
        "Workshop",
        format!("downloading item {id}"),
        item.title.clone(),
    );
    let result = if !item.file_url.is_empty() {
        workshop::install_legacy(
            &w.store,
            id,
            &item.file_url,
            item.file_size.max(0) as u64,
            item.updated,
            subscriber,
            &w.cancel,
            &mut progress,
        )
        .await
    } else if item.file_handle != 0 {
        if client.is_none() {
            match ContentClient::new(w.connection.clone()).await {
                Ok(created) => *client = Some(created),
                Err(error) => {
                    events::emit(
                        &w.events,
                        w.app_id,
                        Direction::Local,
                        "Workshop",
                        format!("item {id}: content servers unavailable"),
                        error.to_string(),
                    );
                    return Err(E_FAIL);
                }
            }
        }
        let spec = DepotSpec {
            depot_id: w.app_id,
            manifest_id: item.file_handle,
            source_app_id: w.app_id,
        };
        match client.as_ref() {
            Some(client) => {
                workshop::install(
                    client,
                    &w.store,
                    id,
                    &spec,
                    item.updated,
                    subscriber,
                    &w.cancel,
                    &w.gate,
                    &mut progress,
                )
                .await
            }
            None => return Err(E_FAIL),
        }
    } else {
        return Err(E_FAIL);
    };
    match result {
        Ok(installed) => {
            events::emit(
                &w.events,
                w.app_id,
                Direction::Local,
                "Workshop",
                format!("item {id} installed"),
                format!("{} bytes", installed.size),
            );
            Ok(installed)
        }
        Err(error) => {
            events::emit(
                &w.events,
                w.app_id,
                Direction::Local,
                "Workshop",
                format!("item {id} failed"),
                error.to_string(),
            );
            Err(E_FAIL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(legacy: bool, installed: Option<(u64, u32)>, handle: u64, updated: u32) -> Tracked {
        Tracked {
            item: UgcItem {
                id: 1,
                file_handle: handle,
                updated,
                file_url: if legacy {
                    "https://x".into()
                } else {
                    String::new()
                },
                ..Default::default()
            },
            subscribed: true,
            installed: installed.map(|(manifest, time_updated)| InstalledItem {
                id: 1,
                manifest,
                size: 1,
                time_updated,
                latest_manifest: manifest,
                remove_pending: false,
            }),
            failed: 0,
        }
    }

    #[test]
    fn downloads_are_wanted_when_missing_or_outdated() {
        assert!(tracked(false, None, 5, 9).wants_download());
        assert!(!tracked(false, Some((5, 9)), 5, 9).wants_download());
        assert!(tracked(false, Some((4, 9)), 5, 9).needs_update());
        // Legacy items compare publication times, not manifests.
        assert!(!tracked(true, Some((0, 9)), 5, 9).needs_update());
        assert!(tracked(true, Some((0, 8)), 5, 9).needs_update());
        // Unknown latest version: keep what is installed.
        assert!(!tracked(false, Some((4, 9)), 0, 0).needs_update());
    }

    #[test]
    fn callback_payloads_match_the_sdk_layouts() {
        assert_eq!(installed_payload(4000, 7, 9).len(), 28);
        assert_eq!(result_payload(4000, 7, 1).len(), 16);
        assert_eq!(&result_payload(4000, 7, 1)[12..], &1i32.to_le_bytes());
    }
}
