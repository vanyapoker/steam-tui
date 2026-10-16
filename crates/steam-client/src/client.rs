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
unsafe fn lookup(user: i32, pipe: i32, name: *const c_char) -> *mut c_void {
    let name = unsafe { version(name) };
    trace(format_args!(
        "client.GetInterface user={user} pipe={pipe} {name:?}"
    ));
    let state = pipes();
    if !state.users.contains_key(&pipe) {
        return ptr::null_mut();
    }
    // 009 is a strict prefix of 010 (010 only appends methods).
    if matches!(name, Some(n) if n == c"SteamUtils010" || n == c"SteamUtils009") {
        return object(&utils::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamUser021") {
        return object(&user::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamUser023") {
        return object(&user::OBJECT023);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamFriends017") {
        return object(&friends::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMTIMELINE_INTERFACE_V001")
    {
        return object(&timeline::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMAPPS_INTERFACE_VERSION008")
    {
        return object(&apps::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamMatchMaking009") {
        return object(&matchmaking::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"SteamMatchGameSearch001")
    {
        return object(&matchmaking::SEARCH);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"SteamMatchMakingServers002")
    {
        return object(&matchmaking::SERVERS);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMUSERSTATS_INTERFACE_VERSION012")
    {
        return object(&userstats::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamNetworking006") {
        return object(&networking::NETWORKING);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"SteamNetworkingSockets012")
    {
        return sockets::object();
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"SteamNetworkingMessages002")
    {
        return sockets::messages_object();
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMREMOTESTORAGE_INTERFACE_VERSION016")
    {
        return object(&remotestorage::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMSCREENSHOTS_INTERFACE_VERSION003")
    {
        return object(&screenshots::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMHTTP_INTERFACE_VERSION003")
    {
        return object(&http::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"SteamGameCoordinator001")
    {
        return object(&gc::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamController008") {
        return object(&controller::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMUGC_INTERFACE_VERSION017")
    {
        return object(&ugc::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMUGC_INTERFACE_VERSION016")
    {
        return object(&ugc::OBJECT016);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMAPPLIST_INTERFACE_VERSION001")
    {
        return object(&applist::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMMUSIC_INTERFACE_VERSION001")
    {
        return object(&music::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMMUSICREMOTE_INTERFACE_VERSION001")
    {
        return object(&musicremote::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMHTMLSURFACE_INTERFACE_VERSION_005")
    {
        return object(&htmlsurface::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMINVENTORY_INTERFACE_V003")
    {
        return object(&inventory::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && matches!(name, Some(n) if n == c"STEAMVIDEO_INTERFACE_V007" || n == c"STEAMVIDEO_INTERFACE_V002")
    {
        return object(&video::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMPARENTALSETTINGS_INTERFACE_VERSION001")
    {
        return object(&parental::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamInput006") {
        return object(&input::OBJECT);
    }
    if user == 1
        && state.users.get(&pipe) == Some(&true)
        && name == Some(c"STEAMREMOTEPLAY_INTERFACE_VERSION001")
    {
        return object(&remoteplay::OBJECT);
    }
    if user == 1 && state.users.get(&pipe) == Some(&true) && name == Some(c"SteamParties002") {
        return object(&parties::OBJECT);
    }
    if name == Some(c"SteamNetworkingUtils004") {
        return object(&networking::UTILS);
    }
    trace(format_args!("UNSUPPORTED interface {name:?}"));
    ptr::null_mut()
}
unsafe extern "C" fn get_user_interface(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    if !matches!(unsafe { version(name) }, Some(n) if n == c"SteamUser021" || n == c"SteamUser023")
    {
        trace(format_args!("UNSUPPORTED user interface {:?}", unsafe {
            version(name)
        }));
        return ptr::null_mut();
    }
    unsafe { lookup(user, pipe, name) }
}
unsafe extern "C" fn get_utils(_this: *mut c_void, pipe: i32, name: *const c_char) -> *mut c_void {
    if !matches!(unsafe { version(name) }, Some(n) if n == c"SteamUtils010" || n == c"SteamUtils009")
    {
        trace(format_args!("UNSUPPORTED utils interface {:?}", unsafe {
            version(name)
        }));
        return ptr::null_mut();
    }
    unsafe { lookup(0, pipe, name) }
}
unsafe extern "C" fn generic(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    unsafe { lookup(user, pipe, name) }
}
unsafe extern "C" fn get_apps(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    if unsafe { version(name) } != Some(c"STEAMAPPS_INTERFACE_VERSION008") {
        trace("UNSUPPORTED apps interface");
        return ptr::null_mut();
    }
    unsafe { lookup(user, pipe, name) }
}
unsafe extern "C" fn get_friends(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    if unsafe { version(name) } != Some(c"SteamFriends017") {
        trace("UNSUPPORTED friends interface");
        return ptr::null_mut();
    }
    unsafe { lookup(user, pipe, name) }
}
unsafe extern "C" fn get_matchmaking(
    _this: *mut c_void,
    user: i32,
    pipe: i32,
    name: *const c_char,
) -> *mut c_void {
    if unsafe { version(name) } != Some(c"SteamMatchMaking009") {
        trace("UNSUPPORTED matchmaking interface");
        return ptr::null_mut();
    }
    unsafe { lookup(user, pipe, name) }
}
// Typed factory slots use the same version and session validation as the
// generic factory. Keep each slot restricted to its own interface family.
macro_rules! typed_getters {
    ($($getter:ident => $($name:literal)|+;)*) => {
        $(unsafe extern "C" fn $getter(
            _this: *mut c_void, user: i32, pipe: i32, name: *const c_char,
        ) -> *mut c_void {
            if !matches!(unsafe { version(name) }, Some(n) if false $(|| n == $name)+) {
                trace(format_args!("UNSUPPORTED {} interface {:?}", stringify!($getter), unsafe { version(name) }));
                return ptr::null_mut();
            }
            unsafe { lookup(user, pipe, name) }
        })*
    };
}
typed_getters! {
    get_servers => c"SteamMatchMakingServers002";
    get_userstats => c"STEAMUSERSTATS_INTERFACE_VERSION012";
    get_networking => c"SteamNetworking006";
    get_remotestorage => c"STEAMREMOTESTORAGE_INTERFACE_VERSION016";
    get_screenshots => c"STEAMSCREENSHOTS_INTERFACE_VERSION003";
    get_search => c"SteamMatchGameSearch001";
    get_http => c"STEAMHTTP_INTERFACE_VERSION003";
    get_controller => c"SteamController008";
    get_ugc => c"STEAMUGC_INTERFACE_VERSION017" | c"STEAMUGC_INTERFACE_VERSION016";
    get_applist => c"STEAMAPPLIST_INTERFACE_VERSION001";
    get_music => c"STEAMMUSIC_INTERFACE_VERSION001";
    get_musicremote => c"STEAMMUSICREMOTE_INTERFACE_VERSION001";
    get_html => c"STEAMHTMLSURFACE_INTERFACE_VERSION_005";
    get_inventory => c"STEAMINVENTORY_INTERFACE_V003";
    get_video => c"STEAMVIDEO_INTERFACE_V007" | c"STEAMVIDEO_INTERFACE_V002";
    get_parties => c"SteamParties002";
    get_parental => c"STEAMPARENTALSETTINGS_INTERFACE_VERSION001";
    get_input => c"SteamInput006";
    get_remoteplay => c"STEAMREMOTEPLAY_INTERFACE_VERSION001";
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
static TABLE: VTable<42> = VTable([
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
