//! ISteamUGC beyond queries: subscriptions, installed-item state and downloads,
//! votes, favorites, playtime tracking, dependencies and deletion. The backend
//! owns the state (`steam-core/src/backend/workshop.rs`); the game side keeps a
//! short-lived snapshot so the synchronous getters do not each cost a round trip.
//! Publishing (CreateItem, SubmitItemUpdate, ...) is not supported and reports failure.
use super::*;
use std::time::{Duration, Instant};
use steam_ipc::{Operation, Response, UgcItem, UgcOp, WorkshopItem};

// Callback ids: k_iSteamUGCCallbacks = 3400, k_iSteamRemoteStorageCallbacks = 1300.
const REQUEST_DETAILS: i32 = 3402;
const CREATE_ITEM: i32 = 3403;
const SUBMIT_UPDATE: i32 = 3404;
const FAVORITES_CHANGED: i32 = 3407;
const SET_VOTE: i32 = 3408;
const GET_VOTE: i32 = 3409;
const START_PLAYTIME: i32 = 3410;
const STOP_PLAYTIME: i32 = 3411;
const ADD_DEPENDENCY: i32 = 3412;
const REMOVE_DEPENDENCY: i32 = 3413;
const ADD_APP_DEPENDENCY: i32 = 3414;
const REMOVE_APP_DEPENDENCY: i32 = 3415;
const GET_APP_DEPENDENCIES: i32 = 3416;
const DELETE_ITEM: i32 = 3417;
const SUBSCRIBED_LIST_CHANGED: i32 = 3418;
const EULA_STATUS: i32 = 3420;
const SUBSCRIBE_RESULT: i32 = 1313;
const UNSUBSCRIBE_RESULT: i32 = 1315;

const E_OK: i32 = 1;
const E_FAIL: i32 = 2;
/// Snapshots older than this are refreshed (downloads progress between calls).
const SNAPSHOT_TTL: Duration = Duration::from_millis(250);
const MAX_IDS: u32 = 1000;

// EItemState flags.
const STATE_SUBSCRIBED: u32 = 1;
const STATE_LEGACY: u32 = 2;
const STATE_INSTALLED: u32 = 4;
const STATE_NEEDS_UPDATE: u32 = 8;
const STATE_DOWNLOADING: u32 = 16;
const STATE_PENDING: u32 = 32;

static SNAPSHOT: Mutex<Option<(Instant, Vec<WorkshopItem>)>> = Mutex::new(None);

fn backend_available() -> bool {
    std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_some()
}

fn invalidate() {
    *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The install service's view of items, at most `SNAPSHOT_TTL` old.
fn snapshot() -> Vec<WorkshopItem> {
    if !backend_available() {
        return Vec::new();
    }
    if let Some((at, items)) = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && at.elapsed() < SNAPSHOT_TTL
    {
        return items.clone();
    }
    let items = match bridge::request(current_app_id(), Operation::Ugc(UgcOp::Snapshot)) {
        Ok(Response::UgcSnapshot { items, .. }) => items,
        _ => Vec::new(),
    };
    *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), items.clone()));
    items
}

fn item(id: u64) -> Option<WorkshopItem> {
    snapshot().into_iter().find(|i| i.id == id)
}

fn state_of(item: &WorkshopItem) -> u32 {
    let mut state = 0;
    if item.subscribed {
        state |= STATE_SUBSCRIBED;
    }
    if item.legacy {
        state |= STATE_LEGACY;
    }
    if item.installed {
        state |= STATE_INSTALLED;
    }
    if item.needs_update {
        state |= STATE_NEEDS_UPDATE;
    }
    if item.downloading {
        state |= STATE_DOWNLOADING;
    }
    if item.pending {
        state |= STATE_PENDING;
    }
    state
}

/// Little-endian callback payload builder (pack 4: no padding before 8-byte fields).
#[derive(Default)]
struct Wire(Vec<u8>);
impl Wire {
    fn i32(mut self, v: i32) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn bool(mut self, v: bool) -> Self {
        self.0.push(u8::from(v));
        self
    }
    fn size(mut self, n: usize) -> Vec<u8> {
        self.0.resize(n, 0);
        self.0
    }
}

/// Starts `op` on a worker; `done` turns the backend's reply into the call result payload.
fn call(
    callback: i32,
    failure: Vec<u8>,
    op: UgcOp,
    done: impl FnOnce(Response) -> Vec<u8> + Send + 'static,
) -> u64 {
    if !backend_available() {
        return callresults::complete(callback, failure);
    }
    userstats::spawn_call(callback, failure.clone(), move || {
        match bridge::request(current_app_id(), Operation::Ugc(op)) {
            Ok(response) => done(response),
            Err(_) => failure,
        }
    })
}

fn result_of(response: &Response) -> Option<i32> {
    match response {
        Response::UgcResult { result } => Some(*result),
        _ => None,
    }
}

fn read_ids(ids: *const u64, count: u32) -> Option<Vec<u64>> {
    if ids.is_null() || count == 0 || count > MAX_IDS {
        return None;
    }
    // SAFETY: the SDK passes `count` ids.
    Some(unsafe { std::slice::from_raw_parts(ids, count as usize) }.to_vec())
}

// ---- RequestUGCDetails -------------------------------------------------------

pub(super) unsafe extern "C" fn request_details(_: *mut c_void, id: u64, _max_age: u32) -> u64 {
    trace(format_args!("ugc.RequestUGCDetails {id}"));
    let failure = ugc::details_result(
        &UgcItem {
            id,
            result: E_FAIL,
            ..Default::default()
        },
        false,
    );
    call(
        REQUEST_DETAILS,
        failure.clone(),
        UgcOp::Details {
            ids: vec![id],
            long_description: true,
        },
        move |response| match response {
            Response::UgcDetails { items, .. } => items
                .into_iter()
                .find(|i| i.id == id)
                .map_or(failure, |item| ugc::details_result(&item, false)),
            _ => failure,
        },
    )
}

// ---- publishing (not supported) ---------------------------------------------

pub(super) unsafe extern "C" fn create_item(_: *mut c_void, _app: u32, _kind: i32) -> u64 {
    trace("ugc.CreateItem (publishing Workshop items is not supported)");
    callresults::complete(CREATE_ITEM, Wire::default().i32(E_FAIL).size(16))
}
pub(super) unsafe extern "C" fn start_item_update(_: *mut c_void, _app: u32, _id: u64) -> u64 {
    trace("ugc.StartItemUpdate (publishing Workshop items is not supported)");
    0
}
pub(super) unsafe extern "C" fn submit_item_update(
    _: *mut c_void,
    _handle: u64,
    _note: *const c_char,
) -> u64 {
    trace("ugc.SubmitItemUpdate (publishing Workshop items is not supported)");
    callresults::complete(SUBMIT_UPDATE, Wire::default().i32(E_FAIL).size(16))
}
pub(super) unsafe extern "C" fn item_update_progress(
    _: *mut c_void,
    _handle: u64,
    processed: *mut u64,
    total: *mut u64,
) -> i32 {
    unsafe {
        if !processed.is_null() {
            processed.write(0);
        }
        if !total.is_null() {
            total.write(0);
        }
    }
    0 // k_EItemUpdateStatusInvalid
}

// ---- votes and favorites -----------------------------------------------------

pub(super) unsafe extern "C" fn set_vote(_: *mut c_void, id: u64, up: bool) -> u64 {
    trace(format_args!("ugc.SetUserItemVote {id} up={up}"));
    let failure = Wire::default().u64(id).i32(E_FAIL).bool(up).size(16);
    call(
        SET_VOTE,
        failure.clone(),
        UgcOp::Vote { id, up },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().u64(id).i32(result).bool(up).size(16),
            None => failure,
        },
    )
}

pub(super) unsafe extern "C" fn get_vote(_: *mut c_void, id: u64) -> u64 {
    let failure = Wire::default().u64(id).i32(E_FAIL).size(16);
    call(
        GET_VOTE,
        failure.clone(),
        UgcOp::GetVote { id },
        move |r| match r {
            Response::UgcVote {
                result,
                voted_up,
                voted_down,
                skipped,
            } => Wire::default()
                .u64(id)
                .i32(result)
                .bool(voted_up)
                .bool(voted_down)
                .bool(skipped)
                .size(16),
            _ => failure,
        },
    )
}

fn favorite(app: u32, id: u64, add: bool) -> u64 {
    trace(format_args!(
        "ugc.{}Favorites {id}",
        if add { "AddItemTo" } else { "RemoveItemFrom" }
    ));
    let failure = Wire::default().u64(id).i32(E_FAIL).bool(add).size(16);
    call(
        FAVORITES_CHANGED,
        failure.clone(),
        UgcOp::Favorite { app, id, add },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().u64(id).i32(result).bool(add).size(16),
            None => failure,
        },
    )
}
pub(super) unsafe extern "C" fn add_favorite(_: *mut c_void, app: u32, id: u64) -> u64 {
    favorite(app, id, true)
}
pub(super) unsafe extern "C" fn remove_favorite(_: *mut c_void, app: u32, id: u64) -> u64 {
    favorite(app, id, false)
}

// ---- subscriptions -----------------------------------------------------------

fn subscription(id: u64, subscribe: bool) -> u64 {
    trace(format_args!(
        "ugc.{}Item {id}",
        if subscribe {
            "Subscribe"
        } else {
            "Unsubscribe"
        }
    ));
    let callback = if subscribe {
        SUBSCRIBE_RESULT
    } else {
        UNSUBSCRIBE_RESULT
    };
    let failure = Wire::default().i32(E_FAIL).u64(id).size(12);
    call(
        callback,
        failure.clone(),
        UgcOp::Subscribe {
            app: 0,
            id,
            subscribe,
        },
        move |r| match result_of(&r) {
            Some(result) => {
                invalidate();
                if result == E_OK {
                    auth::push_callback(
                        SUBSCRIBED_LIST_CHANGED,
                        Wire::default().u32(current_app_id()).size(4),
                    );
                }
                Wire::default().i32(result).u64(id).size(12)
            }
            None => failure,
        },
    )
}
pub(super) unsafe extern "C" fn subscribe(_: *mut c_void, id: u64) -> u64 {
    subscription(id, true)
}
pub(super) unsafe extern "C" fn unsubscribe(_: *mut c_void, id: u64) -> u64 {
    subscription(id, false)
}

pub(super) unsafe extern "C" fn num_subscribed(_: *mut c_void) -> u32 {
    snapshot().iter().filter(|i| i.subscribed).count() as u32
}

pub(super) unsafe extern "C" fn subscribed_items(_: *mut c_void, out: *mut u64, max: u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    let mut count = 0;
    for entry in snapshot()
        .iter()
        .filter(|i| i.subscribed)
        .take(max as usize)
    {
        // SAFETY: the caller's array holds `max` ids.
        unsafe { out.add(count).write(entry.id) };
        count += 1;
    }
    count as u32
}

// ---- install state -----------------------------------------------------------

pub(super) unsafe extern "C" fn item_state(_: *mut c_void, id: u64) -> u32 {
    item(id).map_or(0, |i| state_of(&i))
}

/// GetItemInstallInfo(id, *sizeOnDisk, folder, folderSize, *timestamp).
pub(super) unsafe extern "C" fn install_info(
    _: *mut c_void,
    id: u64,
    size: *mut u64,
    folder: *mut c_char,
    capacity: u32,
    timestamp: *mut u32,
) -> bool {
    let Some(found) = item(id).filter(|i| i.installed && !i.path.is_empty()) else {
        return false;
    };
    if folder.is_null() || capacity as usize <= found.path.len() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(found.path.as_ptr(), folder.cast::<u8>(), found.path.len());
        folder.add(found.path.len()).write(0);
        if !size.is_null() {
            size.write(found.size);
        }
        if !timestamp.is_null() {
            timestamp.write(found.time_updated);
        }
    }
    true
}

pub(super) unsafe extern "C" fn download_info(
    _: *mut c_void,
    id: u64,
    done: *mut u64,
    total: *mut u64,
) -> bool {
    let Some(found) = item(id).filter(|i| i.downloading || i.pending) else {
        return false;
    };
    unsafe {
        if !done.is_null() {
            done.write(found.done);
        }
        if !total.is_null() {
            total.write(found.total);
        }
    }
    true
}

pub(super) unsafe extern "C" fn download_item(_: *mut c_void, id: u64, high: bool) -> bool {
    trace(format_args!("ugc.DownloadItem {id}"));
    if !backend_available() {
        return false;
    }
    let ok = matches!(
        bridge::request(
            current_app_id(),
            Operation::Ugc(UgcOp::Download {
                id,
                high_priority: high,
            }),
        ),
        Ok(Response::UgcResult { result: E_OK })
    );
    invalidate();
    ok
}

pub(super) unsafe extern "C" fn suspend_downloads(_: *mut c_void, on: bool) {
    if backend_available() {
        let _ = bridge::request(current_app_id(), Operation::Ugc(UgcOp::Suspend { on }));
    }
}

// ---- playtime, dependencies, deletion ----------------------------------------

fn playtime(ids: Vec<u64>, start: bool) -> u64 {
    let callback = if start { START_PLAYTIME } else { STOP_PLAYTIME };
    let failure = Wire::default().i32(E_FAIL).size(4);
    call(
        callback,
        failure.clone(),
        UgcOp::Playtime { app: 0, ids, start },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().i32(result).size(4),
            None => failure,
        },
    )
}
pub(super) unsafe extern "C" fn start_playtime(_: *mut c_void, ids: *const u64, count: u32) -> u64 {
    match read_ids(ids, count) {
        Some(ids) => playtime(ids, true),
        None => callresults::complete(START_PLAYTIME, Wire::default().i32(E_FAIL).size(4)),
    }
}
pub(super) unsafe extern "C" fn stop_playtime(_: *mut c_void, ids: *const u64, count: u32) -> u64 {
    match read_ids(ids, count) {
        Some(ids) => playtime(ids, false),
        None => callresults::complete(STOP_PLAYTIME, Wire::default().i32(E_FAIL).size(4)),
    }
}
pub(super) unsafe extern "C" fn stop_playtime_all(_: *mut c_void) -> u64 {
    playtime(Vec::new(), false)
}

fn dependency(parent: u64, child: u64, add: bool) -> u64 {
    let callback = if add {
        ADD_DEPENDENCY
    } else {
        REMOVE_DEPENDENCY
    };
    let failure = Wire::default().i32(E_FAIL).u64(parent).u64(child).size(20);
    call(
        callback,
        failure.clone(),
        UgcOp::Dependency { parent, child, add },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().i32(result).u64(parent).u64(child).size(20),
            None => failure,
        },
    )
}
pub(super) unsafe extern "C" fn add_dependency(_: *mut c_void, parent: u64, child: u64) -> u64 {
    dependency(parent, child, true)
}
pub(super) unsafe extern "C" fn remove_dependency(_: *mut c_void, parent: u64, child: u64) -> u64 {
    dependency(parent, child, false)
}

fn app_dependency(id: u64, app: u32, add: bool) -> u64 {
    let callback = if add {
        ADD_APP_DEPENDENCY
    } else {
        REMOVE_APP_DEPENDENCY
    };
    let failure = Wire::default().i32(E_FAIL).u64(id).u32(app).size(16);
    call(
        callback,
        failure.clone(),
        UgcOp::AppDependency { id, app, add },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().i32(result).u64(id).u32(app).size(16),
            None => failure,
        },
    )
}
pub(super) unsafe extern "C" fn add_app_dependency(_: *mut c_void, id: u64, app: u32) -> u64 {
    app_dependency(id, app, true)
}
pub(super) unsafe extern "C" fn remove_app_dependency(_: *mut c_void, id: u64, app: u32) -> u64 {
    app_dependency(id, app, false)
}

pub(super) unsafe extern "C" fn get_app_dependencies(_: *mut c_void, id: u64) -> u64 {
    // GetAppDependenciesResult_t: result, id, AppId_t[32], count, total.
    let failure = Wire::default().i32(E_FAIL).u64(id).size(148);
    call(
        GET_APP_DEPENDENCIES,
        failure.clone(),
        UgcOp::GetAppDependencies { id },
        move |r| match r {
            Response::UgcAppDependencies { result, apps } => {
                let mut wire = Wire::default().i32(result).u64(id);
                for app in apps.iter().take(32) {
                    wire = wire.u32(*app);
                }
                let shown = apps.len().min(32) as u32;
                let mut payload = wire.size(140);
                payload.extend(shown.to_le_bytes());
                payload.extend((apps.len() as u32).to_le_bytes());
                payload
            }
            _ => failure,
        },
    )
}

pub(super) unsafe extern "C" fn delete_item(_: *mut c_void, id: u64) -> u64 {
    trace(format_args!("ugc.DeleteItem {id}"));
    let failure = Wire::default().i32(E_FAIL).u64(id).size(12);
    call(
        DELETE_ITEM,
        failure.clone(),
        UgcOp::Delete { app: 0, id },
        move |r| match result_of(&r) {
            Some(result) => Wire::default().i32(result).u64(id).size(12),
            None => failure,
        },
    )
}

pub(super) unsafe extern "C" fn eula_status(_: *mut c_void) -> u64 {
    // WorkshopEULAStatus_t: result, app, version, action time, accepted, needs action.
    callresults::complete(
        EULA_STATUS,
        Wire::default().i32(E_FAIL).u32(current_app_id()).size(20),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_state_flags() {
        let mut item = WorkshopItem {
            id: 1,
            subscribed: true,
            installed: true,
            ..Default::default()
        };
        assert_eq!(state_of(&item), STATE_SUBSCRIBED | STATE_INSTALLED);
        item.needs_update = true;
        item.downloading = true;
        item.legacy = true;
        assert_eq!(
            state_of(&item),
            STATE_SUBSCRIBED
                | STATE_INSTALLED
                | STATE_NEEDS_UPDATE
                | STATE_DOWNLOADING
                | STATE_LEGACY
        );
        assert_eq!(state_of(&WorkshopItem::default()), 0);
    }

    #[test]
    fn wire_payload_sizes() {
        assert_eq!(Wire::default().i32(1).u64(2).size(12).len(), 12);
        assert_eq!(Wire::default().u64(2).i32(1).bool(true).size(16)[12], 1);
    }
}
