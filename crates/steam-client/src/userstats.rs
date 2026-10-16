//! ISteamUserStats backed by the account backend: leaderboards, stats,
//! achievements and global statistics. Requests go to the backend over IPC on
//! worker threads (a Steam API call must not block the game thread) and finish
//! as call results or callbacks. Without a backend socket every call fails the
//! way an offline Steam does. Overloads keep header order (Itanium ABI).
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use steam_ipc::schema::{Schema, StatKind};
use steam_ipc::{BoardEntry, Operation, Response, StatsOp};

// k_iSteamUserStatsCallbacks = 1100.
const USER_STATS_RECEIVED: i32 = 1101;
const USER_STATS_STORED: i32 = 1102;
const USER_ACHIEVEMENT_STORED: i32 = 1103;
const LEADERBOARD_FIND_RESULT: i32 = 1104;
const LEADERBOARD_SCORES_DOWNLOADED: i32 = 1105;
const LEADERBOARD_SCORE_UPLOADED: i32 = 1106;
const NUMBER_OF_CURRENT_PLAYERS: i32 = 1107;
const GLOBAL_ACHIEVEMENT_PERCENTAGES_READY: i32 = 1110;
const LEADERBOARD_UGC_SET: i32 = 1111;
const GLOBAL_STATS_RECEIVED: i32 = 1112;

const E_OK: i32 = 1;
const E_FAIL: i32 = 2;
const MAX_WORKERS: usize = 32;
const MAX_DOWNLOADS: usize = 16;
const MAX_OTHER_USERS: usize = 32;
const MAX_DETAILS_INTS: usize = 64;
const MAX_SCHEMA_STRINGS: usize = 20_000;

/// Little-endian payload builder for the packed (pack 4) callback structs.
#[derive(Default)]
struct Payload(Vec<u8>);
impl Payload {
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
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
    fn pad_to(mut self, size: usize) -> Vec<u8> {
        self.0.resize(size, 0);
        self.0
    }
}

struct Board {
    name: &'static CStr,
    sort: i32,
    display: i32,
    entries: i32,
}

/// One account's stats as the server last reported them plus local edits.
#[derive(Default)]
struct Local {
    schema: Schema,
    values: BTreeMap<u32, u32>,
    /// Stat id -> unlock time (unix seconds) per achievement bit.
    unlocks: BTreeMap<u32, Vec<u32>>,
    dirty: BTreeSet<u32>,
    /// Achievements set since the last store (announced once stored).
    unlocked: Vec<String>,
    /// Running (count, seconds) of UpdateAvgRateStat per stat id, this session.
    avg: BTreeMap<u32, (f64, f64)>,
    reset: bool,
}

#[derive(Default)]
struct Global {
    totals: BTreeMap<String, i64>,
    history: BTreeMap<String, Vec<i64>>,
}

#[derive(Default)]
struct State {
    boards: HashMap<i32, Board>,
    downloads: VecDeque<(u64, Vec<BoardEntry>)>,
    next_handle: u64,
    own: Option<Local>,
    others: HashMap<u64, Local>,
    percentages: Vec<(String, f32)>,
    global: Global,
    strings: HashMap<String, &'static CStr>,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Stable NUL-terminated copy of `text`; the SDK returns these pointers to the
/// game, which may keep them. Deduplicated, so growth is bounded by the schema.
fn stable(state: &mut State, text: &str) -> *const c_char {
    if let Some(found) = state.strings.get(text) {
        return found.as_ptr();
    }
    if state.strings.len() >= MAX_SCHEMA_STRINGS {
        return c"".as_ptr();
    }
    let owned = CString::new(text.replace('\0', "")).unwrap_or_default();
    let leaked: &'static CStr = Box::leak(owned.into_boxed_c_str());
    state.strings.insert(text.to_owned(), leaked);
    leaked.as_ptr()
}

unsafe fn text<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

fn app_id() -> u32 {
    current_app_id()
}

fn online() -> bool {
    std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_some()
}

fn ipc(op: StatsOp) -> Result<Response, String> {
    bridge::request(app_id(), Operation::Stats(op)).map_err(|e| e.to_string())
}

fn now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32)
}

/// Runs `work` off the game thread and completes a new call with its payload.
/// `failure` is delivered if the worker cannot start.
fn spawn_call(
    callback: i32,
    failure: Vec<u8>,
    work: impl FnOnce() -> Vec<u8> + Send + 'static,
) -> u64 {
    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    let call = callresults::reserve();
    if IN_FLIGHT.fetch_add(1, Ordering::AcqRel) >= MAX_WORKERS {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        callresults::complete_for(call, callback, failure);
        return call;
    }
    let spawned = std::thread::Builder::new()
        .name("steam-stats".into())
        .spawn(move || {
            let payload = work();
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            callresults::complete_for(call, callback, payload);
        });
    if spawned.is_err() {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        callresults::complete_for(call, callback, failure);
    }
    call
}

// ---- stats loading ---------------------------------------------------------

fn local_from(data: Response, previous: Option<&Local>) -> Option<(i32, Local)> {
    let Response::StatsData {
        result,
        schema,
        stats,
        unlocks,
        ..
    } = data
    else {
        return None;
    };
    let parsed = steam_ipc::hex_decode(&schema)
        .filter(|b| !b.is_empty())
        .and_then(|b| Schema::parse(&b, "english"));
    let schema = parsed
        .or_else(|| previous.map(|p| p.schema.clone()))
        .unwrap_or_default();
    let mut values: BTreeMap<u32, u32> = schema.stats.iter().map(|s| (s.id, s.default)).collect();
    for (id, value) in stats {
        values.insert(id, value);
    }
    Some((
        result,
        Local {
            schema,
            values,
            unlocks: unlocks.into_iter().collect(),
            ..Default::default()
        },
    ))
}

/// Fetches `user`'s stats (0 = this account) and files them; returns the EResult.
fn load_stats(user: u64) -> i32 {
    let own_id = bridge::identity(app_id()).0;
    let is_own = user == 0 || user == own_id;
    let response = match ipc(StatsOp::GetStats {
        user: if is_own { 0 } else { user },
    }) {
        Ok(response) => response,
        Err(_) => return E_FAIL,
    };
    let mut state = state();
    let previous = if is_own {
        state.own.as_ref()
    } else {
        state.others.get(&user)
    };
    let Some((result, local)) = local_from(response, previous) else {
        return E_FAIL;
    };
    if result == E_OK {
        if is_own {
            // Local edits made while the request was in flight stay.
            state.own = Some(local);
        } else {
            if state.others.len() >= MAX_OTHER_USERS
                && let Some(oldest) = state.others.keys().next().copied()
            {
                state.others.remove(&oldest);
            }
            state.others.insert(user, local);
        }
    }
    result
}

fn stats_received(user: u64, result: i32) {
    let payload = Payload::default()
        .u64(u64::from(app_id()))
        .i32(result)
        .u64(user)
        .pad_to(20);
    auth::push_callback(USER_STATS_RECEIVED, payload);
}

/// SteamUserStats013 has no RequestCurrentStats: the client loads the stats at
/// startup. Called when a game obtains that interface.
pub(super) fn auto_request() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    if !online() {
        return;
    }
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("steam-stats".into())
            .spawn(|| {
                let result = load_stats(0);
                stats_received(bridge::identity(app_id()).0, result);
            });
    });
}

// ---- leaderboards ----------------------------------------------------------

fn board_found(id: i32, name: String, sort: i32, display: i32, entries: i32) -> u64 {
    let mut state = state();
    let name = stable(&mut state, &name);
    // SAFETY: `stable` returns a NUL-terminated string that is never freed.
    let name = unsafe { CStr::from_ptr(name) };
    state.boards.insert(
        id,
        Board {
            name,
            sort,
            display,
            entries,
        },
    );
    id as u64
}

fn find(name: String, create: bool, sort: i32, display: i32) -> u64 {
    let failure = Payload::default().u64(0).u8(0).pad_to(12);
    spawn_call(LEADERBOARD_FIND_RESULT, failure, move || {
        match ipc(StatsOp::FindLeaderboard {
            name,
            create,
            sort,
            display,
        }) {
            Ok(Response::Board {
                found: true,
                id,
                name,
                entries,
                sort,
                display,
            }) if id > 0 => {
                let handle = board_found(id, name, sort, display, entries);
                Payload::default().u64(handle).u8(1).pad_to(12)
            }
            _ => Payload::default().u64(0).u8(0).pad_to(12),
        }
    })
}

unsafe extern "C" fn f22(_: *mut c_void, name: *const c_char, sort: i32, display: i32) -> u64 {
    let Some(name) = (unsafe { text(name) }) else {
        return 0;
    };
    trace(format_args!("userstats.FindOrCreateLeaderboard {name}"));
    find(name.to_owned(), true, sort, display)
}
unsafe extern "C" fn f23(_: *mut c_void, name: *const c_char) -> u64 {
    let Some(name) = (unsafe { text(name) }) else {
        return 0;
    };
    trace(format_args!("userstats.FindLeaderboard {name}"));
    find(name.to_owned(), false, 0, 0)
}
unsafe extern "C" fn f24(_: *mut c_void, board: u64) -> *const c_char {
    state()
        .boards
        .get(&(board as i32))
        .map_or(c"".as_ptr(), |b| b.name.as_ptr())
}
unsafe extern "C" fn f25(_: *mut c_void, board: u64) -> i32 {
    state().boards.get(&(board as i32)).map_or(0, |b| b.entries)
}
unsafe extern "C" fn f26(_: *mut c_void, board: u64) -> i32 {
    state().boards.get(&(board as i32)).map_or(0, |b| b.sort)
}
unsafe extern "C" fn f27(_: *mut c_void, board: u64) -> i32 {
    state().boards.get(&(board as i32)).map_or(0, |b| b.display)
}

fn download(board: u64, request: i32, start: i32, end: i32, users: Vec<u64>) -> u64 {
    let failure = Payload::default().u64(board).u64(0).i32(0).pad_to(20);
    if !online() || board == 0 || board > i32::MAX as u64 {
        let call = callresults::reserve();
        callresults::complete_for(call, LEADERBOARD_SCORES_DOWNLOADED, failure);
        return call;
    }
    spawn_call(LEADERBOARD_SCORES_DOWNLOADED, failure.clone(), move || {
        let response = ipc(StatsOp::LeaderboardEntries {
            board: board as i32,
            request,
            start,
            end,
            users,
        });
        match response {
            Ok(Response::BoardEntries {
                result: E_OK,
                total,
                entries,
            }) => {
                let mut state = state();
                if let Some(known) = state.boards.get_mut(&(board as i32)) {
                    known.entries = total;
                }
                state.next_handle += 1;
                let handle = state.next_handle;
                let count = entries.len() as i32;
                if state.downloads.len() >= MAX_DOWNLOADS {
                    state.downloads.pop_front();
                }
                state.downloads.push_back((handle, entries));
                Payload::default()
                    .u64(board)
                    .u64(handle)
                    .i32(count)
                    .pad_to(20)
            }
            _ => failure,
        }
    })
}

unsafe extern "C" fn f28(_: *mut c_void, board: u64, request: i32, start: i32, end: i32) -> u64 {
    trace(format_args!(
        "userstats.DownloadLeaderboardEntries {board} kind {request}"
    ));
    download(board, request, start, end, Vec::new())
}
unsafe extern "C" fn f29(_: *mut c_void, board: u64, users: *const u64, count: i32) -> u64 {
    let users = if users.is_null() || !(0..=4096).contains(&count) {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(users, count as usize) }.to_vec()
    };
    trace(format_args!(
        "userstats.DownloadLeaderboardEntriesForUsers {board}"
    ));
    download(board, 3, 0, 0, users)
}
unsafe extern "C" fn f30(
    _: *mut c_void,
    entries: u64,
    index: i32,
    out: *mut u8,
    details: *mut i32,
    max_details: i32,
) -> bool {
    if out.is_null() || index < 0 {
        return false;
    }
    let state = state();
    let Some((_, list)) = state.downloads.iter().find(|(h, _)| *h == entries) else {
        return false;
    };
    let Some(entry) = list.get(index as usize) else {
        return false;
    };
    let ints = entry.details.len() / 4;
    let record = Payload::default()
        .u64(entry.steam_id)
        .i32(entry.rank)
        .i32(entry.score)
        .i32(ints as i32)
        .u64(entry.ugc)
        .0;
    // SAFETY: LeaderboardEntry_t is 28 bytes (pack 4); the SDK passes one.
    unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), out, record.len()) };
    if !details.is_null() && max_details > 0 {
        for (i, chunk) in entry
            .details
            .chunks_exact(4)
            .take(max_details as usize)
            .enumerate()
        {
            let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            unsafe { details.add(i).write(value) };
        }
    }
    true
}
unsafe extern "C" fn f31(
    _: *mut c_void,
    board: u64,
    method: i32,
    score: i32,
    details: *const i32,
    count: i32,
) -> u64 {
    let failure = Payload::default()
        .u8(0)
        .u64(board)
        .i32(score)
        .u8(0)
        .i32(0)
        .i32(0)
        .pad_to(28);
    if !online() || board == 0 || board > i32::MAX as u64 {
        let call = callresults::reserve();
        callresults::complete_for(call, LEADERBOARD_SCORE_UPLOADED, failure);
        return call;
    }
    let count = count.clamp(0, MAX_DETAILS_INTS as i32) as usize;
    let bytes: Vec<u8> = if details.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(details, count) }
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    };
    trace(format_args!(
        "userstats.UploadLeaderboardScore {board} score {score}"
    ));
    spawn_call(
        LEADERBOARD_SCORE_UPLOADED,
        failure.clone(),
        move || match ipc(StatsOp::UploadScore {
            board: board as i32,
            method,
            score,
            details: bytes,
        }) {
            Ok(Response::ScoreUploaded {
                success,
                changed,
                total,
                rank_new,
                rank_previous,
            }) => {
                if let Some(known) = state().boards.get_mut(&(board as i32)) {
                    known.entries = total;
                }
                Payload::default()
                    .u8(u8::from(success))
                    .u64(board)
                    .i32(score)
                    .u8(u8::from(changed))
                    .i32(rank_new)
                    .i32(rank_previous)
                    .pad_to(28)
            }
            _ => failure,
        },
    )
}
unsafe extern "C" fn f32(_: *mut c_void, board: u64, ugc: u64) -> u64 {
    let failure = Payload::default().i32(E_FAIL).u64(board).pad_to(12);
    if !online() || board == 0 || board > i32::MAX as u64 {
        let call = callresults::reserve();
        callresults::complete_for(call, LEADERBOARD_UGC_SET, failure);
        return call;
    }
    spawn_call(LEADERBOARD_UGC_SET, failure.clone(), move || {
        match ipc(StatsOp::AttachUgc {
            board: board as i32,
            ugc,
        }) {
            Ok(Response::StatsResult { result }) => {
                Payload::default().i32(result).u64(board).pad_to(12)
            }
            _ => failure,
        }
    })
}

// ---- local stats and achievements -----------------------------------------

fn with_own<R>(f: impl FnOnce(&mut Local) -> Option<R>) -> Option<R> {
    f(state().own.as_mut()?)
}

fn user_local<R>(user: u64, f: impl FnOnce(&Local) -> Option<R>) -> Option<R> {
    let own_id = bridge::identity(app_id()).0;
    let state = state();
    let local = if user == own_id || user == 0 {
        state.own.as_ref()
    } else {
        state.others.get(&user)
    }?;
    f(local)
}

fn read_stat(local: &Local, name: &str, float: bool) -> Option<u32> {
    let stat = local.schema.stat(name)?;
    let is_float = matches!(stat.kind, StatKind::Float | StatKind::AvgRate);
    (is_float == float).then(|| local.values.get(&stat.id).copied().unwrap_or(stat.default))
}

fn read_achievement(local: &Local, name: &str) -> Option<(bool, u32)> {
    let (stat, ach) = local.schema.achievement(name)?;
    let value = local.values.get(&stat).copied().unwrap_or(0);
    let set = (value >> ach.bit) & 1 == 1;
    let time = local
        .unlocks
        .get(&stat)
        .and_then(|t| t.get(ach.bit as usize))
        .copied()
        .unwrap_or(0);
    Some((set, time))
}

unsafe extern "C" fn f0(_: *mut c_void) -> bool {
    if !online() {
        trace("userstats.RequestCurrentStats (offline)");
        return false;
    }
    trace("userstats.RequestCurrentStats");
    let _ = std::thread::Builder::new()
        .name("steam-stats".into())
        .spawn(|| {
            let result = load_stats(0);
            stats_received(bridge::identity(app_id()).0, result);
        })
        .map_err(|_| stats_received(bridge::identity(app_id()).0, E_FAIL));
    true
}
unsafe extern "C" fn f1(_: *mut c_void, name: *const c_char, out: *mut i32) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match with_own(|l| read_stat(l, name, false)) {
        Some(raw) => {
            unsafe { out.write(raw as i32) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f2(_: *mut c_void, name: *const c_char, out: *mut f32) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match with_own(|l| read_stat(l, name, true)) {
        Some(raw) => {
            unsafe { out.write(f32::from_bits(raw)) };
            true
        }
        None => false,
    }
}

/// Applies a validated new raw value; false when the schema forbids it.
fn write_stat(local: &mut Local, name: &str, float: bool, raw: u32) -> bool {
    let Some(stat) = local.schema.stat(name) else {
        return false;
    };
    let is_float = matches!(stat.kind, StatKind::Float | StatKind::AvgRate);
    if is_float != float {
        return false;
    }
    let current = local.values.get(&stat.id).copied().unwrap_or(stat.default);
    let as_f64 = |v: u32| {
        if float {
            f64::from(f32::from_bits(v))
        } else {
            f64::from(v as i32)
        }
    };
    let new = as_f64(raw);
    if new.is_nan()
        || stat.min.is_some_and(|m| new < m)
        || stat.max.is_some_and(|m| new > m)
        || (stat.increment_only && new < as_f64(current))
    {
        return false;
    }
    if raw != current {
        let id = stat.id;
        local.values.insert(id, raw);
        local.dirty.insert(id);
    }
    true
}
unsafe extern "C" fn f3(_: *mut c_void, name: *const c_char, value: i32) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    with_own(|l| Some(write_stat(l, name, false, value as u32))).unwrap_or(false)
}
unsafe extern "C" fn f4(_: *mut c_void, name: *const c_char, value: f32) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    with_own(|l| Some(write_stat(l, name, true, value.to_bits()))).unwrap_or(false)
}
unsafe extern "C" fn f5(_: *mut c_void, name: *const c_char, count: f32, seconds: f64) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    if !(seconds.is_finite() && seconds > 0.0) || !count.is_finite() {
        return false;
    }
    with_own(|l| {
        let id = l.schema.stat(name)?.id;
        let (total, length) = l.avg.entry(id).or_default();
        *total += f64::from(count);
        *length += seconds;
        let average = (*total / *length) as f32;
        Some(write_stat(l, name, true, average.to_bits()))
    })
    .unwrap_or(false)
}
unsafe extern "C" fn f6(_: *mut c_void, name: *const c_char, out: *mut bool) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match with_own(|l| read_achievement(l, name)) {
        Some((set, _)) => {
            unsafe { out.write(set) };
            true
        }
        None => false,
    }
}
fn set_achievement(local: &mut Local, name: &str, on: bool) -> bool {
    let Some((stat, ach)) = local.schema.achievement(name) else {
        return false;
    };
    let (bit, owned) = (ach.bit, ach.name.clone());
    let current = local.values.get(&stat).copied().unwrap_or(0);
    let updated = if on {
        current | (1 << bit)
    } else {
        current & !(1 << bit)
    };
    if updated != current {
        local.values.insert(stat, updated);
        local.dirty.insert(stat);
        if on {
            local.unlocked.push(owned);
            let times = local.unlocks.entry(stat).or_default();
            if times.len() <= bit as usize {
                times.resize(bit as usize + 1, 0);
            }
            times[bit as usize] = now();
        } else {
            local.unlocked.retain(|n| n != &owned);
        }
    }
    true
}
unsafe extern "C" fn f7(_: *mut c_void, name: *const c_char) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    trace(format_args!("userstats.SetAchievement {name}"));
    with_own(|l| Some(set_achievement(l, name, true))).unwrap_or(false)
}
unsafe extern "C" fn f8(_: *mut c_void, name: *const c_char) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    with_own(|l| Some(set_achievement(l, name, false))).unwrap_or(false)
}
unsafe extern "C" fn f9(
    _: *mut c_void,
    name: *const c_char,
    out: *mut bool,
    time: *mut u32,
) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    match with_own(|l| read_achievement(l, name)) {
        Some((set, at)) => {
            if !out.is_null() {
                unsafe { out.write(set) };
            }
            if !time.is_null() {
                unsafe { time.write(at) };
            }
            true
        }
        None => false,
    }
}

fn store_payloads(local: &mut Local) -> (Vec<(u32, u32)>, bool, Vec<String>) {
    let stats = if local.reset {
        local.values.iter().map(|(k, v)| (*k, *v)).collect()
    } else {
        local
            .dirty
            .iter()
            .filter_map(|id| local.values.get(id).map(|v| (*id, *v)))
            .collect()
    };
    (stats, local.reset, local.unlocked.clone())
}

unsafe extern "C" fn f10(_: *mut c_void) -> bool {
    if !online() {
        return false;
    }
    let Some((stats, reset, unlocked)) = with_own(|l| Some(store_payloads(l))) else {
        return false;
    };
    trace(format_args!("userstats.StoreStats {} stats", stats.len()));
    let game = u64::from(app_id());
    let _ = std::thread::Builder::new()
        .name("steam-stats".into())
        .spawn(move || {
            let response = ipc(StatsOp::StoreStats {
                stats: stats.clone(),
                reset,
            });
            let result = match response {
                Ok(Response::StatsStored { result, failed, .. }) => {
                    if result == E_OK {
                        with_own(|l| {
                            for (id, value) in &failed {
                                l.values.insert(*id, *value);
                            }
                            for (id, _) in &stats {
                                l.dirty.remove(id);
                            }
                            l.reset = false;
                            l.unlocked.retain(|n| !unlocked.contains(n));
                            Some(())
                        });
                        for name in &unlocked {
                            let mut field = [0u8; 128];
                            let bytes = name.as_bytes();
                            let n = bytes.len().min(127);
                            field[..n].copy_from_slice(&bytes[..n]);
                            let mut payload = Payload::default().u64(game).u8(0).0;
                            payload.extend(field);
                            let payload = Payload(payload).u32(0).u32(0).pad_to(148);
                            auth::push_callback(USER_ACHIEVEMENT_STORED, payload);
                        }
                    }
                    result
                }
                _ => E_FAIL,
            };
            let payload = Payload::default().u64(game).i32(result).pad_to(12);
            auth::push_callback(USER_STATS_STORED, payload);
        })
        .map_err(|_| {
            auth::push_callback(
                USER_STATS_STORED,
                Payload::default().u64(game).i32(E_FAIL).pad_to(12),
            )
        });
    true
}
unsafe extern "C" fn f11(_: *mut c_void, name: *const c_char) -> i32 {
    trace(format_args!(
        "userstats.GetAchievementIcon {:?} (no image pipeline)",
        unsafe { text(name) }
    ));
    0
}
unsafe extern "C" fn f12(_: *mut c_void, name: *const c_char, key: *const c_char) -> *const c_char {
    let (Some(name), Some(key)) = (unsafe { text(name) }, unsafe { text(key) }) else {
        return c"".as_ptr();
    };
    let mut state = state();
    let value = state.own.as_ref().and_then(|l| {
        let (_, ach) = l.schema.achievement(name)?;
        match key {
            "name" => Some(ach.display_name.clone()),
            "desc" => Some(ach.description.clone()),
            "hidden" => Some(if ach.hidden { "1" } else { "0" }.to_owned()),
            _ => None,
        }
    });
    match value {
        Some(value) => stable(&mut state, &value),
        None => c"".as_ptr(),
    }
}
unsafe extern "C" fn f13(_: *mut c_void, name: *const c_char, current: u32, max: u32) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    let known = with_own(|l| {
        let (_, a) = l.schema.achievement(name)?;
        (current < max).then(|| a.name.clone())
    });
    let Some(known) = known else {
        return false;
    };
    let mut field = [0u8; 128];
    let n = known.len().min(127);
    field[..n].copy_from_slice(&known.as_bytes()[..n]);
    let mut payload = Payload::default().u64(u64::from(app_id())).u8(0).0;
    payload.extend(field);
    let payload = Payload(payload).u32(current).u32(max).pad_to(148);
    auth::push_callback(USER_ACHIEVEMENT_STORED, payload);
    true
}
unsafe extern "C" fn f14(_: *mut c_void) -> u32 {
    with_own(|l| Some(l.schema.achievements().count() as u32)).unwrap_or(0)
}
unsafe extern "C" fn f15(_: *mut c_void, index: u32) -> *const c_char {
    let mut state = state();
    let name = state.own.as_ref().and_then(|l| {
        l.schema
            .achievements()
            .nth(index as usize)
            .map(|(_, a)| a.name.clone())
    });
    match name {
        Some(name) => stable(&mut state, &name),
        None => c"".as_ptr(),
    }
}

// ---- other users -----------------------------------------------------------

unsafe extern "C" fn f16(_: *mut c_void, user: u64) -> u64 {
    let failure = Payload::default()
        .u64(u64::from(app_id()))
        .i32(E_FAIL)
        .u64(user)
        .pad_to(20);
    if !online() {
        let call = callresults::reserve();
        callresults::complete_for(call, USER_STATS_RECEIVED, failure);
        return call;
    }
    trace(format_args!("userstats.RequestUserStats {user}"));
    spawn_call(USER_STATS_RECEIVED, failure, move || {
        let result = load_stats(user);
        Payload::default()
            .u64(u64::from(app_id()))
            .i32(result)
            .u64(user)
            .pad_to(20)
    })
}
unsafe extern "C" fn f17(_: *mut c_void, user: u64, name: *const c_char, out: *mut i32) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match user_local(user, |l| read_stat(l, name, false)) {
        Some(raw) => {
            unsafe { out.write(raw as i32) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f18(_: *mut c_void, user: u64, name: *const c_char, out: *mut f32) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match user_local(user, |l| read_stat(l, name, true)) {
        Some(raw) => {
            unsafe { out.write(f32::from_bits(raw)) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f19(_: *mut c_void, user: u64, name: *const c_char, out: *mut bool) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match user_local(user, |l| read_achievement(l, name)) {
        Some((set, _)) => {
            unsafe { out.write(set) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f20(
    _: *mut c_void,
    user: u64,
    name: *const c_char,
    out: *mut bool,
    time: *mut u32,
) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    match user_local(user, |l| read_achievement(l, name)) {
        Some((set, at)) => {
            if !out.is_null() {
                unsafe { out.write(set) };
            }
            if !time.is_null() {
                unsafe { time.write(at) };
            }
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f21(_: *mut c_void, achievements_too: bool) -> bool {
    with_own(|l| {
        let keep: BTreeMap<u32, u32> = if achievements_too {
            BTreeMap::new()
        } else {
            l.schema
                .stats
                .iter()
                .filter(|s| s.kind == StatKind::Bits)
                .filter_map(|s| l.values.get(&s.id).map(|v| (s.id, *v)))
                .collect()
        };
        for stat in &l.schema.stats {
            let value = keep.get(&stat.id).copied().unwrap_or(stat.default);
            l.values.insert(stat.id, value);
            l.dirty.insert(stat.id);
        }
        if achievements_too {
            l.unlocks.clear();
            l.unlocked.clear();
        }
        l.reset = true;
        Some(())
    })
    .is_some()
}

// ---- global statistics -----------------------------------------------------

unsafe extern "C" fn f33(_: *mut c_void) -> u64 {
    let failure = Payload::default().u8(0).i32(0).pad_to(8);
    if !online() {
        let call = callresults::reserve();
        callresults::complete_for(call, NUMBER_OF_CURRENT_PLAYERS, failure);
        return call;
    }
    spawn_call(
        NUMBER_OF_CURRENT_PLAYERS,
        failure.clone(),
        move || match ipc(StatsOp::CurrentPlayers) {
            Ok(Response::Players {
                result: E_OK,
                count,
            }) => Payload::default().u8(1).i32(count).pad_to(8),
            _ => failure,
        },
    )
}
unsafe extern "C" fn f34(_: *mut c_void) -> u64 {
    let failure = Payload::default()
        .u64(u64::from(app_id()))
        .i32(E_FAIL)
        .pad_to(12);
    if !online() {
        let call = callresults::reserve();
        callresults::complete_for(call, GLOBAL_ACHIEVEMENT_PERCENTAGES_READY, failure);
        return call;
    }
    spawn_call(
        GLOBAL_ACHIEVEMENT_PERCENTAGES_READY,
        failure.clone(),
        move || match ipc(StatsOp::GlobalAchievementPercentages) {
            Ok(Response::Percentages {
                result: E_OK,
                mut entries,
            }) => {
                entries.sort_by(|a, b| b.1.total_cmp(&a.1));
                state().percentages = entries;
                Payload::default()
                    .u64(u64::from(app_id()))
                    .i32(E_OK)
                    .pad_to(12)
            }
            _ => failure,
        },
    )
}
unsafe fn copy_name(out: *mut c_char, capacity: u32, name: &str) {
    if out.is_null() || capacity == 0 {
        return;
    }
    let bytes = name.as_bytes();
    let n = bytes.len().min(capacity as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), out, n);
        out.add(n).write(0);
    }
}
unsafe fn achieved_info(
    index: usize,
    name: *mut c_char,
    capacity: u32,
    percent: *mut f32,
    achieved: *mut bool,
) -> i32 {
    let (entry, set) = {
        let state = state();
        let Some(entry) = state.percentages.get(index).cloned() else {
            return -1;
        };
        let set = state
            .own
            .as_ref()
            .and_then(|l| read_achievement(l, &entry.0))
            .is_some_and(|(set, _)| set);
        (entry, set)
    };
    unsafe { copy_name(name, capacity, &entry.0) };
    if !percent.is_null() {
        unsafe { percent.write(entry.1) };
    }
    if !achieved.is_null() {
        unsafe { achieved.write(set) };
    }
    index as i32 + 1
}
unsafe extern "C" fn f35(
    _: *mut c_void,
    name: *mut c_char,
    capacity: u32,
    percent: *mut f32,
    achieved: *mut bool,
) -> i32 {
    unsafe { achieved_info(0, name, capacity, percent, achieved) }
}
unsafe extern "C" fn f36(
    _: *mut c_void,
    previous: i32,
    name: *mut c_char,
    capacity: u32,
    percent: *mut f32,
    achieved: *mut bool,
) -> i32 {
    if previous < 0 {
        return -1;
    }
    unsafe { achieved_info(previous as usize, name, capacity, percent, achieved) }
}
unsafe extern "C" fn f37(_: *mut c_void, name: *const c_char, out: *mut f32) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match state().percentages.iter().find(|(n, _)| n == name) {
        Some((_, percent)) => {
            unsafe { out.write(*percent) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f38(_: *mut c_void, days: i32) -> u64 {
    let failure = Payload::default()
        .u64(u64::from(app_id()))
        .i32(E_FAIL)
        .pad_to(12);
    if !online() {
        let call = callresults::reserve();
        callresults::complete_for(call, GLOBAL_STATS_RECEIVED, failure);
        return call;
    }
    let names: Vec<String> = state()
        .own
        .as_ref()
        .map(|l| {
            l.schema
                .stats
                .iter()
                .filter(|s| s.kind != StatKind::Bits)
                .map(|s| s.name.clone())
                .collect()
        })
        .unwrap_or_default();
    spawn_call(GLOBAL_STATS_RECEIVED, failure.clone(), move || {
        match ipc(StatsOp::GlobalStats {
            names,
            days: days.clamp(0, 60) as u32,
        }) {
            Ok(Response::GlobalStats {
                result: E_OK,
                totals,
                history,
            }) => {
                state().global = Global {
                    totals: totals.into_iter().collect(),
                    history: history.into_iter().collect(),
                };
                Payload::default()
                    .u64(u64::from(app_id()))
                    .i32(E_OK)
                    .pad_to(12)
            }
            _ => failure,
        }
    })
}
unsafe extern "C" fn f39(_: *mut c_void, name: *const c_char, out: *mut i64) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match state().global.totals.get(name) {
        Some(total) => {
            unsafe { out.write(*total) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f40(_: *mut c_void, name: *const c_char, out: *mut f64) -> bool {
    let (Some(name), false) = (unsafe { text(name) }, out.is_null()) else {
        return false;
    };
    match state().global.totals.get(name) {
        Some(total) => {
            unsafe { out.write(*total as f64) };
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f41(_: *mut c_void, name: *const c_char, out: *mut i64, bytes: u32) -> i32 {
    let Some(name) = (unsafe { text(name) }) else {
        return 0;
    };
    let state = state();
    let Some(days) = state.global.history.get(name) else {
        return 0;
    };
    let count = days.len().min(bytes as usize / 8);
    if !out.is_null() {
        for (i, value) in days.iter().take(count).enumerate() {
            unsafe { out.add(i).write(*value) };
        }
    }
    count as i32
}
unsafe extern "C" fn f42(_: *mut c_void, name: *const c_char, out: *mut f64, bytes: u32) -> i32 {
    let Some(name) = (unsafe { text(name) }) else {
        return 0;
    };
    let state = state();
    let Some(days) = state.global.history.get(name) else {
        return 0;
    };
    let count = days.len().min(bytes as usize / 8);
    if !out.is_null() {
        for (i, value) in days.iter().take(count).enumerate() {
            unsafe { out.add(i).write(*value as f64) };
        }
    }
    count as i32
}
unsafe extern "C" fn f43(
    _: *mut c_void,
    name: *const c_char,
    min: *mut i32,
    max: *mut i32,
) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    match with_own(|l| l.schema.achievement(name)?.1.progress) {
        Some((lo, hi)) => {
            if !min.is_null() {
                unsafe { min.write(lo as i32) };
            }
            if !max.is_null() {
                unsafe { max.write(hi as i32) };
            }
            true
        }
        None => false,
    }
}
unsafe extern "C" fn f44(
    _: *mut c_void,
    name: *const c_char,
    min: *mut f32,
    max: *mut f32,
) -> bool {
    let Some(name) = (unsafe { text(name) }) else {
        return false;
    };
    match with_own(|l| l.schema.achievement(name)?.1.progress) {
        Some((lo, hi)) => {
            if !min.is_null() {
                unsafe { min.write(lo as f32) };
            }
            if !max.is_null() {
                unsafe { max.write(hi as f32) };
            }
            true
        }
        None => false,
    }
}

pub(super) static TABLE: VTable<45> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
    f6 as *const (),
    f7 as *const (),
    f8 as *const (),
    f9 as *const (),
    f10 as *const (),
    f11 as *const (),
    f12 as *const (),
    f13 as *const (),
    f14 as *const (),
    f15 as *const (),
    f16 as *const (),
    f17 as *const (),
    f18 as *const (),
    f19 as *const (),
    f20 as *const (),
    f21 as *const (),
    f22 as *const (),
    f23 as *const (),
    f24 as *const (),
    f25 as *const (),
    f26 as *const (),
    f27 as *const (),
    f28 as *const (),
    f29 as *const (),
    f30 as *const (),
    f31 as *const (),
    f32 as *const (),
    f33 as *const (),
    f34 as *const (),
    f35 as *const (),
    f36 as *const (),
    f37 as *const (),
    f38 as *const (),
    f39 as *const (),
    f40 as *const (),
    f41 as *const (),
    f42 as *const (),
    f43 as *const (),
    f44 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// STEAMUSERSTATS_INTERFACE_VERSION013 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
static TABLE_013: VTable<44> = VTable([
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    TABLE.0[6],
    TABLE.0[7],
    TABLE.0[8],
    TABLE.0[9],
    TABLE.0[10],
    TABLE.0[11],
    TABLE.0[12],
    TABLE.0[13],
    TABLE.0[14],
    TABLE.0[15],
    TABLE.0[16],
    TABLE.0[17],
    TABLE.0[18],
    TABLE.0[19],
    TABLE.0[20],
    TABLE.0[21],
    TABLE.0[22],
    TABLE.0[23],
    TABLE.0[24],
    TABLE.0[25],
    TABLE.0[26],
    TABLE.0[27],
    TABLE.0[28],
    TABLE.0[29],
    TABLE.0[30],
    TABLE.0[31],
    TABLE.0[32],
    TABLE.0[33],
    TABLE.0[34],
    TABLE.0[35],
    TABLE.0[36],
    TABLE.0[37],
    TABLE.0[38],
    TABLE.0[39],
    TABLE.0[40],
    TABLE.0[41],
    TABLE.0[42],
    TABLE.0[43],
    TABLE.0[44],
]);
pub(super) static OBJECT_013: Object = Object {
    table: TABLE_013.0.as_ptr(),
};
