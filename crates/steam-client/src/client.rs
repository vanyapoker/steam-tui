use super::*;

/// Interface versions this library serves; Proton's lsteamclient asks
/// `Steam_IsKnownInterface` before requesting one.
pub(super) const KNOWN_INTERFACES: &[&CStr] = &[
    c"SteamUser021",
    c"SteamUser023",
    c"SteamFriends017",
    c"SteamUtils010",
    c"SteamUtils009",
    c"STEAMAPPS_INTERFACE_VERSION008",
    c"SteamMatchMaking009",
    c"SteamMatchGameSearch001",
    c"SteamMatchMakingServers002",
    c"STEAMUSERSTATS_INTERFACE_VERSION012",
    c"SteamNetworking006",
    c"STEAMREMOTESTORAGE_INTERFACE_VERSION016",
    c"STEAMSCREENSHOTS_INTERFACE_VERSION003",
    c"STEAMHTTP_INTERFACE_VERSION003",
    c"SteamController008",
    c"STEAMUGC_INTERFACE_VERSION017",
    c"STEAMUGC_INTERFACE_VERSION016",
    c"STEAMAPPLIST_INTERFACE_VERSION001",
    c"STEAMMUSIC_INTERFACE_VERSION001",
    c"STEAMMUSICREMOTE_INTERFACE_VERSION001",
    c"STEAMHTMLSURFACE_INTERFACE_VERSION_005",
    c"STEAMINVENTORY_INTERFACE_V003",
    c"STEAMVIDEO_INTERFACE_V007",
    c"STEAMVIDEO_INTERFACE_V002",
    c"SteamParties002",
    c"STEAMPARENTALSETTINGS_INTERFACE_VERSION001",
    c"SteamInput006",
    c"STEAMREMOTEPLAY_INTERFACE_VERSION001",
    c"SteamGameCoordinator001",
    c"SteamNetworkingSockets012",
    c"SteamNetworkingMessages002",
    c"SteamNetworkingUtils004",
    c"STEAMTIMELINE_INTERFACE_V001",
];

unsafe extern "C" fn create_pipe(_this: *mut c_void) -> i32 {
    let mut state = pipes();
    let Some(id) = state.next.checked_add(1) else {
        return 0;
    };
    state.next = id;
    state.users.insert(id, false);
    trace(format_args!("client.CreateSteamPipe = {id}"));
    id
}
unsafe extern "C" fn release_pipe(_this: *mut c_void, pipe: i32) -> bool {
    trace(format_args!("client.BReleaseSteamPipe {pipe}"));
    let removed = pipes().users.remove(&pipe).is_some();
    auth::release_pipe(pipe);
    let detached = pipes().users.is_empty();
    if detached {
        remotestorage::shutdown();
    }
    removed
}
unsafe extern "C" fn connect(_this: *mut c_void, pipe: i32) -> i32 {
    trace(format_args!("client.ConnectToGlobalUser {pipe}"));
    let mut state = pipes();
    let Some(connected) = state.users.get_mut(&pipe) else {
        return 0;
    };
    // Local SDK context only; BLoggedOn remains false until there is a CM bridge.
    *connected = true;
    1
}
unsafe extern "C" fn create_local(_this: *mut c_void, _pipe: *mut i32, _kind: i32) -> i32 {
    trace("client.CreateLocalUser unsupported");
    0
}
unsafe extern "C" fn release_user(_this: *mut c_void, pipe: i32, user: i32) {
    trace(format_args!("client.ReleaseUser {pipe} {user}"));
    if user == 1 {
        auth::release_pipe(pipe);
    }
    if user == 1
        && let Some(connected) = pipes().users.get_mut(&pipe)
    {
        *connected = false;
    }
}
/// Interface families: each typed SteamClient getter only serves its own family.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
    User,
    Utils,
    Apps,
    Friends,
    Matchmaking,
    Servers,
    UserStats,
    Networking,
    RemoteStorage,
    Screenshots,
    Search,
    Http,
    Controller,
    Ugc,
    AppList,
    Music,
    MusicRemote,
    Html,
    Inventory,
    Video,
    Parties,
    Parental,
    Input,
    RemotePlay,
    /// Reachable through the generic getter only.
    Generic,
}
struct Entry {
    name: &'static CStr,
    family: Family,
    /// Needs a connected global user (everything except Utils and NetworkingUtils).
    session: bool,
    object: fn() -> *mut c_void,
}
macro_rules! registry {
    ($($family:ident, $session:literal, $name:literal => $object:expr;)*) => {
        &[$(Entry {
            name: $name,
            family: Family::$family,
            session: $session,
            object: || $object,
        }),*]
    };
}
// Every served interface version. 009/010/011-style versions only differ by
// appended or removed slots, so each has its own table (see the generated tables
// at the end of each interface module).
static REGISTRY: &[Entry] = registry! {
    Utils, false, c"SteamUtils009" => object(&utils::OBJECT);
    Utils, false, c"SteamUtils010" => object(&utils::OBJECT);
    Utils, false, c"SteamUtils011" => object(&utils::OBJECT_011);
    User, true, c"SteamUser021" => object(&user::OBJECT);
    // 022 has the 021 slot order; only GetAuthSessionTicket gained a trailing argument.
    User, true, c"SteamUser022" => object(&user::OBJECT);
    User, true, c"SteamUser023" => object(&user::OBJECT023);
    Friends, true, c"SteamFriends017" => object(&friends::OBJECT);
    Friends, true, c"SteamFriends018" => object(&friends::OBJECT_018);
    Generic, true, c"STEAMTIMELINE_INTERFACE_V001" => object(&timeline::OBJECT);
    Generic, true, c"STEAMTIMELINE_INTERFACE_V004" => object(&timeline::OBJECT_004);
    Apps, true, c"STEAMAPPS_INTERFACE_VERSION008" => object(&apps::OBJECT);
    Apps, true, c"STEAMAPPS_INTERFACE_VERSION009" => object(&apps::OBJECT_009);
    Matchmaking, true, c"SteamMatchMaking009" => object(&matchmaking::OBJECT);
    Search, true, c"SteamMatchGameSearch001" => object(&matchmaking::SEARCH);
    Servers, true, c"SteamMatchMakingServers002" => object(&matchmaking::SERVERS);
    Servers, true, c"SteamMatchMakingServers003" => object(&matchmaking::OBJECT_SERVERS003);
    UserStats, true, c"STEAMUSERSTATS_INTERFACE_VERSION012" => object(&userstats::OBJECT);
    UserStats, true, c"STEAMUSERSTATS_INTERFACE_VERSION013" => object(&userstats::OBJECT_013);
    Networking, true, c"SteamNetworking006" => object(&networking::NETWORKING);
    // 013 keeps 012's slot order (SendMessages gained a trailing bool).
    Generic, true, c"SteamNetworkingSockets012" => sockets::object();
    Generic, true, c"SteamNetworkingSockets013" => sockets::object();
    Generic, true, c"SteamNetworkingMessages002" => sockets::messages_object();
    RemoteStorage, true, c"STEAMREMOTESTORAGE_INTERFACE_VERSION016" => object(&remotestorage::OBJECT);
    RemoteStorage, true, c"STEAMREMOTESTORAGE_INTERFACE_VERSION014" => object(&remotestorage::OBJECT_014);
    Screenshots, true, c"STEAMSCREENSHOTS_INTERFACE_VERSION003" => object(&screenshots::OBJECT);
    Http, true, c"STEAMHTTP_INTERFACE_VERSION003" => object(&http::OBJECT);
    Generic, true, c"SteamGameCoordinator001" => object(&gc::OBJECT);
    Controller, true, c"SteamController008" => object(&controller::OBJECT);
    Ugc, true, c"STEAMUGC_INTERFACE_VERSION016" => object(&ugc::OBJECT016);
    Ugc, true, c"STEAMUGC_INTERFACE_VERSION017" => object(&ugc::OBJECT);
    Ugc, true, c"STEAMUGC_INTERFACE_VERSION018" => object(&ugc::OBJECT_018);
    Ugc, true, c"STEAMUGC_INTERFACE_VERSION020" => object(&ugc::OBJECT_020);
    Ugc, true, c"STEAMUGC_INTERFACE_VERSION021" => object(&ugc::OBJECT_021);
    AppList, true, c"STEAMAPPLIST_INTERFACE_VERSION001" => object(&applist::OBJECT);
    Music, true, c"STEAMMUSIC_INTERFACE_VERSION001" => object(&music::OBJECT);
    MusicRemote, true, c"STEAMMUSICREMOTE_INTERFACE_VERSION001" => object(&musicremote::OBJECT);
    Html, true, c"STEAMHTMLSURFACE_INTERFACE_VERSION_005" => object(&htmlsurface::OBJECT);
    Inventory, true, c"STEAMINVENTORY_INTERFACE_V003" => object(&inventory::OBJECT);
    Video, true, c"STEAMVIDEO_INTERFACE_V002" => object(&video::OBJECT);
    Video, true, c"STEAMVIDEO_INTERFACE_V007" => object(&video::OBJECT);
    Parental, true, c"STEAMPARENTALSETTINGS_INTERFACE_VERSION001" => object(&parental::OBJECT);
    Input, true, c"SteamInput005" => object(&input::OBJECT_005);
    Input, true, c"SteamInput006" => object(&input::OBJECT);
    // 007 is 006's layout (only the final DualSense slot differs in name).
    Input, true, c"SteamInput007" => object(&input::OBJECT);
    RemotePlay, true, c"STEAMREMOTEPLAY_INTERFACE_VERSION001" => object(&remoteplay::OBJECT);
    RemotePlay, true, c"STEAMREMOTEPLAY_INTERFACE_VERSION002" => object(&remoteplay::OBJECT_002);
    RemotePlay, true, c"STEAMREMOTEPLAY_INTERFACE_VERSION003" => object(&remoteplay::OBJECT_003);
    RemotePlay, true, c"STEAMREMOTEPLAY_INTERFACE_VERSION004" => object(&remoteplay::OBJECT_004);
    Parties, true, c"SteamParties002" => object(&parties::OBJECT);
    Generic, false, c"SteamNetworkingUtils004" => object(&networking::UTILS);
};
struct Found {
    family: Family,
    session: bool,
    object: *mut c_void,
}
fn entry(name: Option<&CStr>) -> Option<Found> {
    let name = name?;
    if let Some(e) = REGISTRY.iter().find(|e| e.name == name) {
        return Some(Found {
            family: e.family,
            session: e.session,
            object: (e.object)(),
        });
    }
    // Older versions, generated from the SDK headers (see compat.rs).
    compat::COMPAT
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|&(_, family, session, obj)| Found {
            family,
            session,
            object: object(obj),
        })
}
unsafe fn lookup(user: i32, pipe: i32, name: *const c_char) -> *mut c_void {
    let name = unsafe { version(name) };
    trace(format_args!(
        "client.GetInterface user={user} pipe={pipe} {name:?}"
    ));
    let state = pipes();
    if !state.users.contains_key(&pipe) {
        return ptr::null_mut();
    }
    if let Some(entry) = entry(name)
        && (!entry.session || (user == 1 && state.users.get(&pipe) == Some(&true)))
    {
        return entry.object;
    }
    trace(format_args!("UNSUPPORTED interface {name:?}"));
    ptr::null_mut()
}
/// Typed factory slots use the same version and session validation as the
/// generic factory, and each only serves its own interface family.
unsafe fn typed(family: Family, user: i32, pipe: i32, name: *const c_char) -> *mut c_void {
    if !entry(unsafe { version(name) }).is_some_and(|e| e.family == family) {
        trace(format_args!("UNSUPPORTED interface {:?}", unsafe {
            version(name)
        }));
        return ptr::null_mut();
    }
    unsafe { lookup(user, pipe, name) }
}
unsafe extern "C" fn get_user_interface(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    unsafe { typed(Family::User, user, pipe, name) }
}
unsafe extern "C" fn get_utils(_this: *mut c_void, pipe: i32, name: *const c_char) -> *mut c_void {
    unsafe { typed(Family::Utils, 0, pipe, name) }
}
unsafe extern "C" fn generic(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    unsafe { lookup(user, pipe, name) }
}
macro_rules! typed_getters {
    ($($getter:ident => $family:ident;)*) => {
        $(unsafe extern "C" fn $getter(
            _this: *mut c_void, user: i32, pipe: i32, name: *const c_char,
        ) -> *mut c_void {
            unsafe { typed(Family::$family, user, pipe, name) }
        })*
    };
}
typed_getters! {
    get_apps => Apps;
    get_friends => Friends;
    get_matchmaking => Matchmaking;
    get_servers => Servers;
    get_userstats => UserStats;
    get_networking => Networking;
    get_remotestorage => RemoteStorage;
    get_screenshots => Screenshots;
    get_search => Search;
    get_http => Http;
    get_controller => Controller;
    get_ugc => Ugc;
    get_applist => AppList;
    get_music => Music;
    get_musicremote => MusicRemote;
    get_html => Html;
    get_inventory => Inventory;
    get_video => Video;
    get_parties => Parties;
    get_parental => Parental;
    get_input => Input;
    get_remoteplay => RemotePlay;
}
unsafe extern "C" fn missing(
    _this: *mut c_void,
    _user: i32,
    _pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    trace(format_args!("UNSUPPORTED interface {:?}", unsafe {
        version(name)
    }));
    ptr::null_mut()
}
unsafe extern "C" fn bind(_this: *mut c_void, _ip: *const c_void, _port: u16) {
    trace("client.SetLocalIPBinding (no local networking)");
}
unsafe extern "C" fn run_frame(_this: *mut c_void) {}
unsafe extern "C" fn ipc_count(_this: *mut c_void) -> u32 {
    0
}
unsafe extern "C" fn hook(_this: *mut c_void, _hook: *const c_void) {
    trace("client.SetCallbackHook");
}
unsafe extern "C" fn shutdown(_this: *mut c_void) -> bool {
    let empty = pipes().users.is_empty();
    trace(format_args!("client.BShutdownIfAllPipesClosed = {empty}"));
    empty
}
unsafe extern "C" fn destroy(_this: *mut c_void) {
    trace("client.DestroyAllInterfaces");
    let ids: Vec<_> = pipes().users.keys().copied().collect();
    for pipe in ids {
        auth::release_pipe(pipe);
    }
    pipes().users.clear();
    remotestorage::shutdown();
}

// Exact SteamClient020 order, including private/deprecated slots. Never alias
// SteamClient017/023: their layouts differ after the common leading methods.
pub(super) static TABLE: VTable<42> = VTable([
    create_pipe as *const (), // 0
    release_pipe as *const (),
    connect as *const (),
    create_local as *const (),
    release_user as *const (),
    get_user_interface as *const (),
    missing as *const (), // GameServer
    bind as *const (),
    get_friends as *const (), // Friends
    get_utils as *const (),
    get_matchmaking as *const (), // Matchmaking
    get_servers as *const (),     // MatchmakingServers
    generic as *const (),
    get_userstats as *const (),     // UserStats
    missing as *const (),           // GameServerStats
    get_apps as *const (),          // Apps
    get_networking as *const (),    // Networking
    get_remotestorage as *const (), // RemoteStorage
    get_screenshots as *const (),   // Screenshots
    get_search as *const (),        // GameSearch
    run_frame as *const (),
    ipc_count as *const (),
    hook as *const (), // WarningMessageHook
    shutdown as *const (),
    get_http as *const (),        // HTTP
    missing as *const (),         // deprecated UnifiedMessages
    get_controller as *const (),  // Controller
    get_ugc as *const (),         // UGC
    get_applist as *const (),     // AppList
    get_music as *const (),       // Music
    get_musicremote as *const (), // MusicRemote
    get_html as *const (),        // HTMLSurface
    hook as *const (),            // deprecated Set CPostAPIResult
    hook as *const (),            // deprecated Remove CPostAPIResult
    hook as *const (),            // CCheckCallbackRegistered
    get_inventory as *const (),   // Inventory
    get_video as *const (),       // Video
    get_parental as *const (),    // ParentalSettings
    get_input as *const (),       // Input
    get_parties as *const (),     // Parties
    get_remoteplay as *const (),  // RemotePlay
    destroy as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// SteamClient021 (SDK 1.58+, libsteam_api.so slots checked against a shipped
// build): SteamClient020 without the deprecated UnifiedMessages and AppList slots.
pub(super) static TABLE021: VTable<40> = VTable([
    create_pipe as *const (), // 0
    release_pipe as *const (),
    connect as *const (),
    create_local as *const (),
    release_user as *const (),
    get_user_interface as *const (),
    missing as *const (), // GameServer
    bind as *const (),
    get_friends as *const (),
    get_utils as *const (),
    get_matchmaking as *const (), // 10
    get_servers as *const (),
    generic as *const (),
    get_userstats as *const (),
    missing as *const (),  // GameServerStats
    get_apps as *const (), // 15
    get_networking as *const (),
    get_remotestorage as *const (),
    get_screenshots as *const (),
    get_search as *const (), // GameSearch
    run_frame as *const (),  // 20
    ipc_count as *const (),
    hook as *const (), // WarningMessageHook
    shutdown as *const (),
    get_http as *const (),       // 24
    get_controller as *const (), // 25
    get_ugc as *const (),
    get_music as *const (),
    get_musicremote as *const (),
    get_html as *const (),      // 29
    hook as *const (),          // deprecated Set CPostAPIResult
    hook as *const (),          // deprecated Remove CPostAPIResult
    hook as *const (),          // CCheckCallbackRegistered
    get_inventory as *const (), // 33
    get_video as *const (),
    get_parental as *const (),
    get_input as *const (), // 36
    get_parties as *const (),
    get_remoteplay as *const (),
    destroy as *const (), // 39
]);
pub(super) static OBJECT021: Object = Object {
    table: TABLE021.0.as_ptr(),
};

// SteamClient017 (what Proton's lsteamclient talks to on the host): 37 slots, no
// GameSearch/Input/Parties/RemotePlay and RunFrame right after Screenshots. The
// typed and generic getters both route through the versioned lookup.
static TABLE017: VTable<37> = VTable([
    create_pipe as *const (), // 0
    release_pipe as *const (),
    connect as *const (),
    create_local as *const (),
    release_user as *const (),
    get_user_interface as *const (), // 5 GetISteamUser
    missing as *const (),            // GameServer
    bind as *const (),
    get_friends as *const (),
    get_utils as *const (),
    get_matchmaking as *const (),   // 10
    get_servers as *const (),       // MatchmakingServers
    generic as *const (),           // 12 GetISteamGenericInterface
    get_userstats as *const (),     // UserStats
    missing as *const (),           // GameServerStats
    get_apps as *const (),          // 15
    get_networking as *const (),    // Networking
    get_remotestorage as *const (), // RemoteStorage
    get_screenshots as *const (),   // Screenshots
    run_frame as *const (),         // 19
    ipc_count as *const (),
    hook as *const (), // 21 SetWarningMessageHook
    shutdown as *const (),
    get_http as *const (),        // 23 HTTP
    missing as *const (),         // UnifiedMessages
    get_controller as *const (),  // Controller
    get_ugc as *const (),         // UGC
    get_applist as *const (),     // AppList
    get_music as *const (),       // Music
    get_musicremote as *const (), // MusicRemote
    get_html as *const (),        // 30 HTMLSurface
    hook as *const (),            // 31 Set CPostAPIResult
    hook as *const (),            // 32 Remove CPostAPIResult
    hook as *const (),            // 33 CCheckCallbackRegistered
    get_inventory as *const (),   // Inventory
    get_video as *const (),       // Video
    get_parental as *const (),    // ParentalSettings
]);
pub(super) static OBJECT017: Object = Object {
    table: TABLE017.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// SteamClient023 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
static TABLE023: VTable<38> = VTable([
    TABLE021.0[0],
    TABLE021.0[1],
    TABLE021.0[2],
    TABLE021.0[3],
    TABLE021.0[4],
    TABLE021.0[5],
    TABLE021.0[6],
    TABLE021.0[7],
    TABLE021.0[8],
    TABLE021.0[9],
    TABLE021.0[10],
    TABLE021.0[11],
    TABLE021.0[12],
    TABLE021.0[13],
    TABLE021.0[14],
    TABLE021.0[15],
    TABLE021.0[16],
    TABLE021.0[17],
    TABLE021.0[18],
    TABLE021.0[20],
    TABLE021.0[21],
    TABLE021.0[22],
    TABLE021.0[23],
    TABLE021.0[24],
    TABLE021.0[25],
    TABLE021.0[26],
    TABLE021.0[27],
    TABLE021.0[29],
    TABLE021.0[30],
    TABLE021.0[31],
    TABLE021.0[32],
    TABLE021.0[33],
    TABLE021.0[34],
    TABLE021.0[35],
    TABLE021.0[36],
    TABLE021.0[37],
    TABLE021.0[38],
    TABLE021.0[39],
]);
pub(super) static OBJECT023: Object = Object {
    table: TABLE023.0.as_ptr(),
};
