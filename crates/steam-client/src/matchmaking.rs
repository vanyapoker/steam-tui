//! SteamMatchMaking009: lobbies are real (the account backend talks to Steam's
//! matchmaking servers); favorites and the server browser are not supported.
use super::*;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::time::{Duration, Instant};
use steam_ipc::{LobbyFilter, LobbyInfo, LobbyOp, Operation, Response};

// Callback ids and the EResult values we report ourselves.
const LOBBY_MATCH_LIST: i32 = 510;
const LOBBY_CREATED: i32 = 513;
const LOBBY_ENTER: i32 = 504;
const LOBBY_KICKED: i32 = 512;
const RESULT_NO_CONNECTION: i32 = 3;
const ENTER_ERROR: u32 = 5;

fn op(op: LobbyOp) -> Option<Response> {
    bridge::request(current_app_id(), Operation::Lobby(op)).ok()
}
fn flag(response: Option<Response>) -> bool {
    matches!(
        response,
        Some(Response::LobbyFlag(true) | Response::LobbyOk)
    )
}
fn text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: game-supplied NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Strings handed back to the game must outlive the call; keep the last few.
fn keep(value: &str) -> *const c_char {
    static RING: Mutex<VecDeque<CString>> = Mutex::new(VecDeque::new());
    let owned = CString::new(value.replace('\0', "")).unwrap_or_default();
    let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() >= 256 {
        ring.pop_front();
    }
    ring.push_back(owned);
    ring.back().map_or(c"".as_ptr(), |c| c.as_ptr())
}

/// Lobby snapshots: games read lobby data every frame. Every change Steam
/// reports arrives as a lobby callback, which drops the entry (`observe`), as
/// do our own writes (`forget`); the age limit only bounds anything missed.
type InfoCache = Mutex<Option<HashMap<u64, (Instant, Option<LobbyInfo>)>>>;
static CACHE: InfoCache = Mutex::new(None);
const CACHE_AGE: Duration = Duration::from_secs(2);

pub(super) fn info(lobby: u64) -> Option<LobbyInfo> {
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, found)) = cache.as_ref().and_then(|map| map.get(&lobby))
            && at.elapsed() < CACHE_AGE
        {
            return found.clone();
        }
    }
    // Not holding the lock across the round trip keeps other threads' reads
    // and invalidations from waiting on it.
    let requested = Instant::now();
    let found = match op(LobbyOp::Get { lobby }) {
        Some(Response::LobbyInfo(found)) => found,
        _ => None,
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    // A change announced while this read was in flight wins over the read.
    if INVALIDATED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|invalidated| invalidated.get(&lobby))
        .is_none_or(|at| *at < requested)
    {
        if map.len() >= 1024 {
            map.clear();
        }
        map.insert(lobby, (Instant::now(), found.clone()));
    }
    found
}
static INVALIDATED: Mutex<Option<HashMap<u64, Instant>>> = Mutex::new(None);
fn forget(lobby: u64) {
    // Writes must be visible to the very next read.
    {
        let mut invalidated = INVALIDATED.lock().unwrap_or_else(|e| e.into_inner());
        let invalidated = invalidated.get_or_insert_with(HashMap::new);
        if invalidated.len() >= 1024 {
            invalidated.retain(|_, at| at.elapsed() < CACHE_AGE);
        }
        invalidated.insert(lobby, Instant::now());
    }
    if let Some(map) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.remove(&lobby);
    }
}
fn forget_all() {
    let lobbies: Vec<u64> = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|map| map.keys().copied().collect())
        .unwrap_or_default();
    for lobby in lobbies {
        forget(lobby);
    }
}
/// Lobby callbacks (LobbyEnter_t through LobbyKicked_t) start with the lobby id.
pub(super) fn observe(id: i32, data: &[u8]) {
    if (LOBBY_ENTER..=LOBBY_KICKED).contains(&id)
        && let Some(lobby) = data.get(..8)
    {
        forget(u64::from_le_bytes(lobby.try_into().unwrap_or_default()));
    }
}

/// Asynchronous matchmaking calls (list requests, create, join, leave) run here
/// in the order the game made them, so a Steam round trip never blocks the game
/// thread and a leave always reaches Steam before a later join.
fn in_order(job: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static QUEUE: OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let sender = QUEUE.get_or_init(|| {
        let (sender, jobs) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("steam-matchmaking".into())
            .spawn(move || {
                for job in jobs {
                    job();
                }
            });
        if spawned.is_err() {
            trace("matchmaking worker thread could not start");
        }
        Mutex::new(sender)
    });
    let job: Job = Box::new(job);
    if let Err(std::sync::mpsc::SendError(job)) =
        sender.lock().unwrap_or_else(|e| e.into_inner()).send(job)
    {
        // No worker: run inline rather than drop the call.
        job();
    }
}

// ---- list search state -------------------------------------------------------

static FILTERS: Mutex<Vec<LobbyFilter>> = Mutex::new(Vec::new());
static MAX_RESULTS: Mutex<i32> = Mutex::new(0);
static LIST: Mutex<Vec<u64>> = Mutex::new(Vec::new());

fn add_filter(key: String, value: String, comparison: i32, kind: i32) {
    FILTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(LobbyFilter {
            key,
            value,
            comparison,
            kind,
        });
}

unsafe extern "C" fn favorite_count(_this: *mut c_void) -> i32 {
    0
}
unsafe extern "C" fn favorite_game() -> bool {
    false
}
unsafe extern "C" fn add_favorite() -> i32 {
    trace("matchmaking.AddFavoriteGame (favorites are not supported)");
    0
}
unsafe extern "C" fn remove_favorite() -> bool {
    false
}

unsafe extern "C" fn request_list(_this: *mut c_void) -> u64 {
    let filters = std::mem::take(&mut *FILTERS.lock().unwrap_or_else(|e| e.into_inner()));
    let max_results = std::mem::take(&mut *MAX_RESULTS.lock().unwrap_or_else(|e| e.into_inner()));
    trace(format_args!(
        "matchmaking.RequestLobbyList ({} filters)",
        filters.len()
    ));
    let call = callresults::reserve();
    in_order(move || {
        let ids = match op(LobbyOp::RequestList {
            filters,
            max_results: if max_results > 0 { max_results } else { 50 },
        }) {
            Some(Response::LobbyIds(ids)) => ids,
            _ => Vec::new(),
        };
        let count = ids.len() as u32;
        *LIST.lock().unwrap_or_else(|e| e.into_inner()) = ids;
        // The results refreshed every listed lobby's data.
        forget_all();
        callresults::complete_for(call, LOBBY_MATCH_LIST, count.to_le_bytes().to_vec());
    });
    call
}
unsafe extern "C" fn filter_string(
    _this: *mut c_void,
    key: *const c_char,
    value: *const c_char,
    comparison: i32,
) {
    add_filter(text(key), text(value), comparison, 0);
}
unsafe extern "C" fn filter_numeric(
    _this: *mut c_void,
    key: *const c_char,
    value: i32,
    comparison: i32,
) {
    add_filter(text(key), value.to_string(), comparison, 1);
}
unsafe extern "C" fn filter_near(_this: *mut c_void, key: *const c_char, value: i32) {
    add_filter(text(key), value.to_string(), 0, 3);
}
unsafe extern "C" fn filter_slots(_this: *mut c_void, slots: i32) {
    add_filter(String::new(), slots.to_string(), 0, 2);
}
unsafe extern "C" fn filter_distance(_this: *mut c_void, distance: i32) {
    add_filter(String::new(), distance.to_string(), 0, 4);
}
unsafe extern "C" fn filter_count(_this: *mut c_void, max: i32) {
    *MAX_RESULTS.lock().unwrap_or_else(|e| e.into_inner()) = max;
}
unsafe extern "C" fn filter_compatible(_this: *mut c_void, _lobby: u64) {
    trace("matchmaking.AddRequestLobbyListCompatibleMembersFilter (ignored)");
}
unsafe extern "C" fn by_index(_this: *mut c_void, index: i32) -> u64 {
    LIST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(index.max(0) as usize)
        .copied()
        .unwrap_or(0)
}

unsafe extern "C" fn create(_this: *mut c_void, kind: i32, max_members: i32) -> u64 {
    trace(format_args!(
        "matchmaking.CreateLobby(type {kind}, max {max_members})"
    ));
    let call = callresults::reserve();
    in_order(move || {
        let (result, lobby, callbacks) = match op(LobbyOp::Create { kind, max_members }) {
            Some(Response::LobbyResult {
                result,
                lobby,
                callbacks,
            }) => (result, lobby, callbacks),
            _ => (RESULT_NO_CONNECTION, 0, Vec::new()),
        };
        forget(lobby);
        // LobbyCreated_t { i32 result; u64 lobby } (pack 4 => 12 bytes).
        let mut payload = result.to_le_bytes().to_vec();
        payload.extend(lobby.to_le_bytes());
        // Like Valve's client: the call result, then LobbyCreated_t broadcast
        // whatever the result (games set up their lobby from it), then the
        // backend's LobbyEnter_t on success.
        callresults::complete_for(call, LOBBY_CREATED, payload.clone());
        auth::push_callback(LOBBY_CREATED, payload);
        for (id, hex) in callbacks {
            if let Some(data) = steam_ipc::hex_decode(&hex) {
                auth::push_callback(id, data);
            }
        }
    });
    call
}
unsafe extern "C" fn join(_this: *mut c_void, lobby: u64) -> u64 {
    trace(format_args!("matchmaking.JoinLobby({lobby})"));
    let call = callresults::reserve();
    in_order(move || {
        let (result, callbacks) = match op(LobbyOp::Join { lobby }) {
            Some(Response::LobbyResult {
                result, callbacks, ..
            }) => (result, callbacks),
            _ => (ENTER_ERROR as i32, Vec::new()),
        };
        forget(lobby);
        // The enter payload is both the call result and a regular callback.
        let enter = callbacks
            .iter()
            .find(|(id, _)| *id == LOBBY_ENTER)
            .and_then(|(_, hex)| steam_ipc::hex_decode(hex))
            .unwrap_or_else(|| {
                let mut v = lobby.to_le_bytes().to_vec();
                v.extend(0u32.to_le_bytes());
                v.extend([0, 0, 0, 0]);
                v.extend((result as u32).to_le_bytes());
                v
            });
        callresults::complete_for(call, LOBBY_ENTER, enter.clone());
        auth::push_callback(LOBBY_ENTER, enter);
    });
    call
}
unsafe extern "C" fn leave(_this: *mut c_void, lobby: u64) {
    trace(format_args!("matchmaking.LeaveLobby({lobby})"));
    // Leaving is immediate for the game; Steam hears about it in order.
    forget(lobby);
    in_order(move || {
        op(LobbyOp::Leave { lobby });
        forget(lobby);
    });
}
unsafe extern "C" fn invite(_this: *mut c_void, lobby: u64, user: u64) -> bool {
    flag(op(LobbyOp::Invite { lobby, user }))
}
unsafe extern "C" fn member_count(_this: *mut c_void, lobby: u64) -> i32 {
    info(lobby).map_or(0, |l| l.num_members)
}
unsafe extern "C" fn member_by_index(_this: *mut c_void, lobby: u64, index: i32) -> u64 {
    info(lobby)
        .and_then(|l| l.members.get(index.max(0) as usize).map(|m| m.steam_id))
        .unwrap_or(0)
}
unsafe extern "C" fn get_data(_this: *mut c_void, lobby: u64, key: *const c_char) -> *const c_char {
    let key = text(key);
    let value = info(lobby)
        .and_then(|l| {
            l.data
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v)
        })
        .unwrap_or_default();
    keep(&value)
}
unsafe extern "C" fn set_data(
    _this: *mut c_void,
    lobby: u64,
    key: *const c_char,
    value: *const c_char,
) -> bool {
    let ok = flag(op(LobbyOp::SetData {
        lobby,
        key: text(key),
        value: text(value),
    }));
    forget(lobby);
    ok
}
unsafe extern "C" fn data_count(_this: *mut c_void, lobby: u64) -> i32 {
    info(lobby).map_or(0, |l| l.data.len() as i32)
}
unsafe extern "C" fn data_by_index(
    _this: *mut c_void,
    lobby: u64,
    index: i32,
    key: *mut u8,
    key_size: i32,
    value: *mut u8,
    value_size: i32,
) -> bool {
    let Some((k, v)) = info(lobby).and_then(|l| l.data.get(index.max(0) as usize).cloned()) else {
        return false;
    };
    copy_out(key, key_size, &k);
    copy_out(value, value_size, &v);
    true
}
/// NUL-terminated, truncated copy into a caller buffer.
fn copy_out(buffer: *mut u8, size: i32, text: &str) {
    if buffer.is_null() || size <= 0 {
        return;
    }
    let n = text.len().min(size as usize - 1);
    // SAFETY: `size` writable bytes at `buffer`; n + 1 <= size.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), buffer, n);
        buffer.add(n).write(0);
    }
}
unsafe extern "C" fn delete_data(_this: *mut c_void, lobby: u64, key: *const c_char) -> bool {
    let ok = flag(op(LobbyOp::DeleteData {
        lobby,
        key: text(key),
    }));
    forget(lobby);
    ok
}
unsafe extern "C" fn member_data(
    _this: *mut c_void,
    lobby: u64,
    user: u64,
    key: *const c_char,
) -> *const c_char {
    let key = text(key);
    let value = info(lobby)
        .and_then(|l| l.members.into_iter().find(|m| m.steam_id == user))
        .and_then(|m| {
            m.data
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v)
        })
        .unwrap_or_default();
    keep(&value)
}
unsafe extern "C" fn set_member_data(
    _this: *mut c_void,
    lobby: u64,
    key: *const c_char,
    value: *const c_char,
) {
    op(LobbyOp::SetMemberData {
        lobby,
        key: text(key),
        value: text(value),
    });
    forget(lobby);
}
unsafe extern "C" fn chat_send(_this: *mut c_void, lobby: u64, data: *const u8, size: i32) -> bool {
    if data.is_null() || size <= 0 || size > 4096 {
        return false;
    }
    // SAFETY: game-supplied buffer of `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data, size as usize) };
    flag(op(LobbyOp::Chat {
        lobby,
        data: steam_ipc::hex_encode(bytes),
    }))
}
unsafe extern "C" fn chat_entry(
    _this: *mut c_void,
    lobby: u64,
    id: i32,
    user: *mut u64,
    data: *mut u8,
    capacity: i32,
    kind: *mut i32,
) -> i32 {
    let Some(Response::LobbyChat { sender, data: hex }) = op(LobbyOp::ChatEntry {
        lobby,
        id: id.max(0) as u32,
    }) else {
        return 0;
    };
    let bytes = steam_ipc::hex_decode(&hex).unwrap_or_default();
    // SAFETY: caller-supplied out-pointers; `data` has `capacity` bytes.
    unsafe {
        if !user.is_null() {
            user.write(sender);
        }
        if !kind.is_null() {
            kind.write(1); // k_EChatEntryTypeChatMsg
        }
        let n = bytes.len().min(capacity.max(0) as usize);
        if !data.is_null() && n > 0 {
            ptr::copy_nonoverlapping(bytes.as_ptr(), data, n);
        }
        n as i32
    }
}
unsafe extern "C" fn request_data(_this: *mut c_void, lobby: u64) -> bool {
    // The backend asks Steam and posts LobbyDataUpdate_t when the answer lands.
    let ok = flag(op(LobbyOp::RequestData { lobby }));
    forget(lobby);
    ok
}
unsafe extern "C" fn set_game_server(
    _this: *mut c_void,
    lobby: u64,
    ip: u32,
    port: u16,
    server: u64,
) {
    op(LobbyOp::SetGameServer {
        lobby,
        ip,
        port,
        server,
    });
    forget(lobby);
}
unsafe extern "C" fn get_game_server(
    _this: *mut c_void,
    lobby: u64,
    ip: *mut u32,
    port: *mut u16,
    server: *mut u64,
) -> bool {
    let Some((id, address, p)) = info(lobby).and_then(|l| l.game_server) else {
        return false;
    };
    // SAFETY: caller-supplied out-pointers (each may be null).
    unsafe {
        if !ip.is_null() {
            ip.write(address);
        }
        if !port.is_null() {
            port.write(p);
        }
        if !server.is_null() {
            server.write(id);
        }
    }
    true
}
unsafe extern "C" fn set_limit(_this: *mut c_void, lobby: u64, max: i32) -> bool {
    let ok = flag(op(LobbyOp::SetMemberLimit {
        lobby,
        max_members: max,
    }));
    forget(lobby);
    ok
}
unsafe extern "C" fn get_limit(_this: *mut c_void, lobby: u64) -> i32 {
    info(lobby).map_or(0, |l| l.max_members)
}
unsafe extern "C" fn set_type(_this: *mut c_void, lobby: u64, kind: i32) -> bool {
    let ok = flag(op(LobbyOp::SetType { lobby, kind }));
    forget(lobby);
    ok
}
unsafe extern "C" fn set_joinable(_this: *mut c_void, lobby: u64, joinable: bool) -> bool {
    let ok = flag(op(LobbyOp::SetJoinable { lobby, joinable }));
    forget(lobby);
    ok
}
unsafe extern "C" fn owner(_this: *mut c_void, lobby: u64) -> u64 {
    info(lobby).map_or(0, |l| l.owner)
}
unsafe extern "C" fn set_owner(_this: *mut c_void, lobby: u64, new_owner: u64) -> bool {
    let ok = flag(op(LobbyOp::SetOwner { lobby, new_owner }));
    forget(lobby);
    ok
}
unsafe extern "C" fn set_linked() -> bool {
    trace("matchmaking.SetLinkedLobby (not supported)");
    false
}
unsafe extern "C" fn psn_boot() {}

pub(super) static TABLE: VTable<39> = VTable([
    favorite_count as *const (),
    favorite_game as *const (),
    add_favorite as *const (),
    remove_favorite as *const (),
    request_list as *const (),
    filter_string as *const (),
    filter_numeric as *const (),
    filter_near as *const (),
    filter_slots as *const (),
    filter_distance as *const (),
    filter_count as *const (),
    filter_compatible as *const (),
    by_index as *const (),
    create as *const (),
    join as *const (),
    leave as *const (),
    invite as *const (),
    member_count as *const (),
    member_by_index as *const (),
    get_data as *const (),
    set_data as *const (),
    data_count as *const (),
    data_by_index as *const (),
    delete_data as *const (),
    member_data as *const (),
    set_member_data as *const (),
    chat_send as *const (),
    chat_entry as *const (),
    request_data as *const (),
    set_game_server as *const (),
    get_game_server as *const (),
    set_limit as *const (),
    get_limit as *const (),
    set_type as *const (),
    set_joinable as *const (),
    owner as *const (),
    set_owner as *const (),
    set_linked as *const (),
    psn_boot as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

/// SteamMatchGameSearch001: every call reports k_EGameSearchErrorCode_Failed_Offline.
const SEARCH_OFFLINE: i32 = 7;
macro_rules! search_stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> i32 {
            trace(concat!("gamesearch.", $label, " = Failed_Offline"));
            SEARCH_OFFLINE
        })*
    };
}
search_stubs! {
    s_add => "AddGameSearchParams";
    s_lobby => "SearchForGameWithLobby";
    s_solo => "SearchForGameSolo";
    s_accept => "AcceptGame";
    s_decline => "DeclineGame";
    s_details => "RetrieveConnectionDetails";
    s_end => "EndGameSearch";
    s_host => "SetGameHostParams";
    s_conn => "SetConnectionDetails";
    s_request => "RequestPlayersForGame";
    s_confirm => "HostConfirmGameStart";
    s_cancel => "CancelRequestPlayersForGame";
    s_result => "SubmitPlayerResult";
    s_game_end => "EndGame";
}
static SEARCH_TABLE: VTable<14> = VTable([
    s_add as *const (),
    s_lobby as *const (),
    s_solo as *const (),
    s_accept as *const (),
    s_decline as *const (),
    s_details as *const (),
    s_end as *const (),
    s_host as *const (),
    s_conn as *const (),
    s_request as *const (),
    s_confirm as *const (),
    s_cancel as *const (),
    s_result as *const (),
    s_game_end as *const (),
]);
pub(super) static SEARCH: Object = Object {
    table: SEARCH_TABLE.0.as_ptr(),
};

/// SteamMatchMakingServers002: request calls return the null handle (the SDK's
/// failure value) until master-server queries are implemented.
macro_rules! server_stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("servers.", $label, " (no server browser)"));
            0
        })*
    };
}
server_stubs! {
    v_internet => "RequestInternetServerList";
    v_lan => "RequestLANServerList";
    v_friends => "RequestFriendsServerList";
    v_favorites => "RequestFavoritesServerList";
    v_history => "RequestHistoryServerList";
    v_spectator => "RequestSpectatorServerList";
    v_release => "ReleaseRequest";
    v_details => "GetServerDetails";
    v_cancel => "CancelQuery";
    v_refresh => "RefreshQuery";
    v_refreshing => "IsRefreshing";
    v_count => "GetServerCount";
    v_refresh_server => "RefreshServer";
    v_cancel_server => "CancelServerQuery";
}
// HServerQuery uses -1 for failure; zero is a potentially valid query ID.
macro_rules! server_queries {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name(_this: *mut c_void, _ip: u32, _port: u16, _response: *mut c_void) -> i32 {
            trace(concat!("servers.", $label, " (no server browser)"));
            -1
        })*
    };
}
server_queries! {
    v_ping => "PingServer";
    v_players => "PlayerDetails";
    v_rules => "ServerRules";
}
pub(super) static SERVERS_TABLE: VTable<17> = VTable([
    v_internet as *const (),
    v_lan as *const (),
    v_friends as *const (),
    v_favorites as *const (),
    v_history as *const (),
    v_spectator as *const (),
    v_release as *const (),
    v_details as *const (),
    v_cancel as *const (),
    v_refresh as *const (),
    v_refreshing as *const (),
    v_count as *const (),
    v_refresh_server as *const (),
    v_ping as *const (),
    v_players as *const (),
    v_rules as *const (),
    v_cancel_server as *const (),
]);
pub(super) static SERVERS: Object = Object {
    table: SERVERS_TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// SteamMatchMakingServers003 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_servers003_16 => "servers.ServerFriends";
}
static TABLE_SERVERS003: VTable<18> = VTable([
    SERVERS_TABLE.0[0],
    SERVERS_TABLE.0[1],
    SERVERS_TABLE.0[2],
    SERVERS_TABLE.0[3],
    SERVERS_TABLE.0[4],
    SERVERS_TABLE.0[5],
    SERVERS_TABLE.0[6],
    SERVERS_TABLE.0[7],
    SERVERS_TABLE.0[8],
    SERVERS_TABLE.0[9],
    SERVERS_TABLE.0[10],
    SERVERS_TABLE.0[11],
    SERVERS_TABLE.0[12],
    SERVERS_TABLE.0[13],
    SERVERS_TABLE.0[14],
    SERVERS_TABLE.0[15],
    n_servers003_16 as *const (),
    SERVERS_TABLE.0[16],
]);
pub(super) static OBJECT_SERVERS003: Object = Object {
    table: TABLE_SERVERS003.0.as_ptr(),
};
