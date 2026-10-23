// SteamFriends017 slot order from Valve public headers.
use super::*;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::time::{Duration, Instant};
use steam_ipc::{FriendInfo, FriendsOp, Operation, Response};

fn persona() -> &'static (CString, Option<i32>) {
    static PERSONA: OnceLock<(CString, Option<i32>)> = OnceLock::new();
    static EMPTY: OnceLock<(CString, Option<i32>)> = OnceLock::new();
    if let Some(persona) = PERSONA.get() {
        return persona;
    }
    match bridge::request(current_app_id(), Operation::Persona) {
        Ok(Response::Persona {
            steam_id,
            name,
            state,
        }) if steam_id != 0 && steam_id == bridge::identity(current_app_id()).0 => {
            PERSONA.get_or_init(|| (CString::new(name).unwrap_or_default(), state))
        }
        _ => EMPTY.get_or_init(|| (CString::default(), None)),
    }
}
/// Friends snapshot: games walk GetFriendByIndex every frame. Changes arrive
/// as PersonaStateChange_t / FriendRichPresenceUpdate_t, which drop it.
static SOCIAL: Mutex<Option<(Instant, Arc<Vec<FriendInfo>>)>> = Mutex::new(None);
static USERS: UserCache = Mutex::new(None);
const PERSONA_STATE_CHANGE: i32 = 304;
const FRIEND_RICH_PRESENCE_UPDATE: i32 = 336;

pub(super) fn observe(id: i32) {
    if id == PERSONA_STATE_CHANGE || id == FRIEND_RICH_PRESENCE_UPDATE {
        *SOCIAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *USERS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn social() -> Arc<Vec<FriendInfo>> {
    if let Some((at, friends)) = SOCIAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && at.elapsed() < Duration::from_secs(1)
    {
        return friends.clone();
    }
    // Not holding the lock across the round trip keeps the callback thread's
    // invalidation from waiting on it.
    let friends = match bridge::request(current_app_id(), Operation::Friends(FriendsOp::Snapshot)) {
        Ok(Response::Friends(friends)) => Arc::new(friends),
        _ => return Arc::default(),
    };
    *SOCIAL.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), friends.clone()));
    friends
}
fn own_id() -> u64 {
    bridge::identity(current_app_id()).0
}
/// Recent `FriendsOp::User` answers, including "not known".
type UserCache = Mutex<Option<HashMap<u64, (Instant, Option<FriendInfo>)>>>;
/// A user with a relationship, else one the backend knows (lobby members,
/// RequestUserInformation answers). Never triggers a fetch.
fn known_user(id: u64) -> Option<FriendInfo> {
    if let Some(friend) = social().iter().find(|f| f.steam_id == id) {
        return Some(friend.clone());
    }
    if let Some((at, user)) = USERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|users| users.get(&id))
        && at.elapsed() < Duration::from_secs(1)
    {
        return user.clone();
    }
    let user = match bridge::request(
        current_app_id(),
        Operation::Friends(FriendsOp::User {
            steam_id: id,
            request: false,
        }),
    ) {
        Ok(Response::User(user)) => user,
        _ => None,
    };
    let mut users = USERS.lock().unwrap_or_else(|e| e.into_inner());
    let users = users.get_or_insert_with(HashMap::new);
    if users.len() >= 4096 {
        users.clear();
    }
    users.insert(id, (Instant::now(), user.clone()));
    user
}
/// Stable C strings per (user, field): a pointer stays valid until that value
/// changes, and replaced values are kept a while longer, like Valve's client.
fn intern(user: u64, field: &str, value: &str) -> *const c_char {
    static STRINGS: Mutex<Option<HashMap<(u64, String), CString>>> = Mutex::new(None);
    static RETIRED: Mutex<VecDeque<CString>> = Mutex::new(VecDeque::new());
    let mut strings = STRINGS.lock().unwrap_or_else(|e| e.into_inner());
    let strings = strings.get_or_insert_with(HashMap::new);
    let key = (user, field.to_owned());
    if let Some(existing) = strings.get(&key)
        && existing.as_bytes() == value.as_bytes()
    {
        return existing.as_ptr();
    }
    let owned = CString::new(value.replace('\0', "")).unwrap_or_default();
    if let Some(old) = strings.insert(key.clone(), owned) {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.len() >= 1024 {
            retired.pop_front();
        }
        retired.push_back(old);
    }
    strings.get(&key).map_or(c"".as_ptr(), |c| c.as_ptr())
}
/// EFriendFlags bit for an EFriendRelationship.
fn relationship_flag(relationship: i32) -> i32 {
    match relationship {
        1 => 0x01,  // Blocked
        2 => 0x02,  // FriendshipRequested (they asked us)
        3 => 0x04,  // Immediate
        4 => 0x80,  // RequestingFriendship (we asked them)
        5 => 0x200, // Ignored
        6 => 0x400, // IgnoredFriend
        _ => 0,
    }
}
fn with_flags(flags: i32) -> Vec<FriendInfo> {
    social()
        .iter()
        .filter(|f| relationship_flag(f.relationship) & flags != 0)
        .cloned()
        .collect()
}
/// This account's rich presence as last set by the game.
static OWN_RICH_PRESENCE: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
fn rich_presence(id: u64) -> Vec<(String, String)> {
    if id == own_id() && id != 0 {
        return OWN_RICH_PRESENCE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
    }
    known_user(id).map(|u| u.rich_presence).unwrap_or_default()
}
/// SetPlayedWith records, as Valve's client keeps them locally: user, time, app.
static COPLAY: Mutex<Vec<(u64, u32, u32)>> = Mutex::new(Vec::new());
fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32)
}
unsafe extern "C" fn name(_this: *mut c_void) -> *const c_char {
    trace("friends.GetPersonaName");
    persona().0.as_ptr()
}
unsafe extern "C" fn persona_state(_this: *mut c_void) -> i32 {
    trace("friends.GetPersonaState");
    persona()
        .1
        .unwrap_or_else(|| unsupported("friends.GetPersonaState: CM state unavailable"))
}
unsafe extern "C" fn slot_1() -> u64 {
    trace("friends.SetPersonaName (not supported)");
    0
}
unsafe extern "C" fn slot_3(_this: *mut c_void, flags: i32) -> i32 {
    with_flags(flags).len() as i32
}
unsafe extern "C" fn slot_4(_this: *mut c_void, index: i32, flags: i32) -> u64 {
    usize::try_from(index)
        .ok()
        .and_then(|i| with_flags(flags).get(i).map(|f| f.steam_id))
        .unwrap_or(0)
}
unsafe extern "C" fn slot_5(_this: *mut c_void, id: u64) -> i32 {
    social()
        .iter()
        .find(|f| f.steam_id == id)
        .map_or(0, |f| f.relationship)
}
unsafe extern "C" fn slot_6(_this: *mut c_void, id: u64) -> i32 {
    known_user(id).map_or(0, |u| u.state)
}
unsafe extern "C" fn slot_7(_this: *mut c_void, id: u64) -> *const c_char {
    let own = own_id();
    if id == own && own != 0 {
        return persona().0.as_ptr();
    }
    match known_user(id).filter(|u| !u.name.is_empty()) {
        Some(user) => intern(id, "name", &user.name),
        None => c"[unknown]".as_ptr(),
    }
}
// FriendGameInfo_t { u64 gameID; u32 ip; u16 port; u16 queryPort; u64 lobby } (24 bytes).
unsafe extern "C" fn slot_8(_this: *mut c_void, id: u64, info: *mut u8) -> bool {
    let Some(user) = known_user(id).filter(|u| u.game_id != 0) else {
        return false;
    };
    if !info.is_null() {
        let mut block = [0u8; 24];
        block[..8].copy_from_slice(&user.game_id.to_le_bytes());
        block[8..12].copy_from_slice(&user.game_ip.to_le_bytes());
        block[12..14].copy_from_slice(&user.game_port.to_le_bytes());
        block[14..16].copy_from_slice(&user.query_port.to_le_bytes());
        block[16..].copy_from_slice(&user.lobby.to_le_bytes());
        // SAFETY: the caller supplied a FriendGameInfo_t.
        unsafe { ptr::copy_nonoverlapping(block.as_ptr(), info, block.len()) };
    }
    true
}
unsafe extern "C" fn slot_9() -> *const c_char {
    trace("friends.GetFriendPersonaNameHistory (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_10() -> i32 {
    trace("friends.GetFriendSteamLevel (no social graph)");
    0
}
unsafe extern "C" fn slot_11() -> *const c_char {
    trace("friends.GetPlayerNickname (no social graph)");
    ptr::null()
}
unsafe extern "C" fn slot_12() -> i32 {
    trace("friends.GetFriendsGroupCount (no social graph)");
    0
}
unsafe extern "C" fn slot_13() -> i16 {
    trace("friends.GetFriendsGroupIDByIndex (not supported)");
    -1
}
unsafe extern "C" fn slot_14() -> *const c_char {
    trace("friends.GetFriendsGroupName (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_15() -> i32 {
    trace("friends.GetFriendsGroupMembersCount (not supported)");
    0
}
unsafe extern "C" fn slot_16() {
    trace("friends.GetFriendsGroupMembersList (not supported)");
}
unsafe extern "C" fn slot_17(_this: *mut c_void, id: u64, flags: i32) -> bool {
    social()
        .iter()
        .any(|f| f.steam_id == id && relationship_flag(f.relationship) & flags != 0)
}
unsafe extern "C" fn slot_18() -> i32 {
    trace("friends.GetClanCount (no social graph)");
    0
}
unsafe extern "C" fn slot_19() -> u64 {
    trace("friends.GetClanByIndex (not supported)");
    0
}
unsafe extern "C" fn slot_20() -> *const c_char {
    trace("friends.GetClanName (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_21() -> *const c_char {
    trace("friends.GetClanTag (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_22() -> bool {
    trace("friends.GetClanActivityCounts (not supported)");
    false
}
unsafe extern "C" fn slot_23() -> u64 {
    trace("friends.DownloadClanActivityCounts (not supported)");
    0
}
// Sources are lobbies here: their members, once we are in them.
unsafe extern "C" fn slot_24(_this: *mut c_void, source: u64) -> i32 {
    matchmaking::info(source).map_or(0, |l| l.members.len() as i32)
}
unsafe extern "C" fn slot_25(_this: *mut c_void, source: u64, index: i32) -> u64 {
    matchmaking::info(source)
        .and_then(|l| {
            l.members
                .get(usize::try_from(index).ok()?)
                .map(|m| m.steam_id)
        })
        .unwrap_or(0)
}
unsafe extern "C" fn slot_26(_this: *mut c_void, user: u64, source: u64) -> bool {
    matchmaking::info(source).is_some_and(|l| l.members.iter().any(|m| m.steam_id == user))
}
unsafe extern "C" fn slot_27() {
    trace("friends.SetInGameVoiceSpeaking (no social graph)");
}
unsafe extern "C" fn slot_28() {
    trace("friends.ActivateGameOverlay (no social graph)");
}
unsafe extern "C" fn slot_29() {
    trace("friends.ActivateGameOverlayToUser (no social graph)");
}
unsafe extern "C" fn slot_30() {
    trace("friends.ActivateGameOverlayToWebPage (no social graph)");
}
unsafe extern "C" fn slot_31() {
    trace("friends.ActivateGameOverlayToStore (no social graph)");
}
unsafe extern "C" fn slot_32(_this: *mut c_void, user: u64) {
    if (user >> 52) & 0xF != 1 || user == own_id() {
        return;
    }
    let mut coplay = COPLAY.lock().unwrap_or_else(|e| e.into_inner());
    coplay.retain(|(id, _, _)| *id != user);
    if coplay.len() >= 1024 {
        coplay.remove(0);
    }
    coplay.push((user, unix_now(), current_app_id()));
}
unsafe extern "C" fn slot_33() {
    trace("friends.ActivateGameOverlayInviteDialog (no social graph)");
}
unsafe extern "C" fn slot_34() -> i32 {
    trace("friends.GetSmallFriendAvatar (no social graph)");
    0
}
unsafe extern "C" fn slot_35() -> i32 {
    trace("friends.GetMediumFriendAvatar (no social graph)");
    0
}
unsafe extern "C" fn slot_36() -> i32 {
    trace("friends.GetLargeFriendAvatar (no social graph)");
    0
}
// True while Steam is being asked; PersonaStateChange_t follows.
unsafe extern "C" fn slot_37(_this: *mut c_void, id: u64, _name_only: bool) -> bool {
    if known_user(id).is_some_and(|u| !u.name.is_empty()) {
        return false;
    }
    let pending = matches!(
        bridge::request(
            current_app_id(),
            Operation::Friends(FriendsOp::User {
                steam_id: id,
                request: true
            }),
        ),
        Ok(Response::User(None))
    );
    trace(format_args!(
        "friends.RequestUserInformation({id}) = {pending}"
    ));
    pending
}
unsafe extern "C" fn slot_38() -> u64 {
    trace("friends.RequestClanOfficerList (not supported)");
    0
}
unsafe extern "C" fn slot_39() -> u64 {
    trace("friends.GetClanOwner (not supported)");
    0
}
unsafe extern "C" fn slot_40() -> i32 {
    trace("friends.GetClanOfficerCount (not supported)");
    0
}
unsafe extern "C" fn slot_41() -> u64 {
    trace("friends.GetClanOfficerByIndex (not supported)");
    0
}
unsafe extern "C" fn slot_42() -> i32 {
    trace("friends.GetUserRestrictions (no social graph)");
    0
}
unsafe extern "C" fn slot_43(_this: *mut c_void, key: *const c_char, value: *const c_char) -> bool {
    if key.is_null() {
        return false;
    }
    // SAFETY: game-supplied NUL-terminated strings.
    let Ok(key) = unsafe { CStr::from_ptr(key) }.to_str() else {
        return false;
    };
    let value = if value.is_null() {
        Ok("")
    } else {
        unsafe { CStr::from_ptr(value) }.to_str()
    };
    let Ok(value) = value else {
        return false;
    };
    let mut own = OWN_RICH_PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = own.clone();
    next.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
    // An empty value deletes the key.
    if !value.is_empty() {
        next.push((key.to_owned(), value.to_owned()));
    }
    if key.is_empty() || !steam_ipc::valid_rich_presence(&next) {
        trace(format_args!("friends.SetRichPresence({key}) rejected"));
        return false;
    }
    let ok = matches!(
        bridge::request(
            current_app_id(),
            Operation::Friends(FriendsOp::SetRichPresence {
                pairs: next.clone()
            }),
        ),
        Ok(Response::FriendsFlag(true))
    );
    if ok {
        *own = next;
    }
    ok
}
unsafe extern "C" fn slot_44(_this: *mut c_void) {
    let mut own = OWN_RICH_PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(
        bridge::request(
            current_app_id(),
            Operation::Friends(FriendsOp::SetRichPresence { pairs: Vec::new() }),
        ),
        Ok(Response::FriendsFlag(true))
    ) {
        own.clear();
    }
}
unsafe extern "C" fn slot_45(_this: *mut c_void, id: u64, key: *const c_char) -> *const c_char {
    if key.is_null() {
        return c"".as_ptr();
    }
    // SAFETY: game-supplied NUL-terminated string.
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    let value = rich_presence(id)
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&key))
        .map(|(_, v)| v)
        .unwrap_or_default();
    intern(id, &format!("rp:{}", key.to_ascii_lowercase()), &value)
}
unsafe extern "C" fn slot_46(_this: *mut c_void, id: u64) -> i32 {
    rich_presence(id).len() as i32
}
unsafe extern "C" fn slot_47(_this: *mut c_void, id: u64, index: i32) -> *const c_char {
    let pairs = rich_presence(id);
    match usize::try_from(index).ok().and_then(|i| pairs.get(i)) {
        Some((key, _)) => intern(id, &format!("rpkey:{index}"), key),
        None => c"".as_ptr(),
    }
}
unsafe extern "C" fn slot_48(_this: *mut c_void, id: u64) {
    bridge::request(
        current_app_id(),
        Operation::Friends(FriendsOp::RequestRichPresence { steam_id: id }),
    )
    .ok();
}
unsafe extern "C" fn slot_49() -> bool {
    trace("friends.InviteUserToGame (not supported)");
    false
}
unsafe extern "C" fn slot_50(_this: *mut c_void) -> i32 {
    COPLAY.lock().unwrap_or_else(|e| e.into_inner()).len() as i32
}
unsafe extern "C" fn slot_51(_this: *mut c_void, index: i32) -> u64 {
    let coplay = COPLAY.lock().unwrap_or_else(|e| e.into_inner());
    usize::try_from(index)
        .ok()
        .and_then(|i| coplay.get(i).map(|(id, _, _)| *id))
        .unwrap_or(0)
}
unsafe extern "C" fn slot_52(_this: *mut c_void, id: u64) -> i32 {
    let coplay = COPLAY.lock().unwrap_or_else(|e| e.into_inner());
    coplay
        .iter()
        .find(|(u, _, _)| *u == id)
        .map_or(0, |(_, t, _)| *t as i32)
}
unsafe extern "C" fn slot_53(_this: *mut c_void, id: u64) -> u32 {
    let coplay = COPLAY.lock().unwrap_or_else(|e| e.into_inner());
    coplay
        .iter()
        .find(|(u, _, _)| *u == id)
        .map_or(0, |(_, _, app)| *app)
}
unsafe extern "C" fn slot_54() -> u64 {
    trace("friends.JoinClanChatRoom (not supported)");
    0
}
unsafe extern "C" fn slot_55() -> bool {
    trace("friends.LeaveClanChatRoom (not supported)");
    false
}
unsafe extern "C" fn slot_56() -> i32 {
    trace("friends.GetClanChatMemberCount (not supported)");
    0
}
unsafe extern "C" fn slot_57() -> u64 {
    trace("friends.GetChatMemberByIndex (not supported)");
    0
}
unsafe extern "C" fn slot_58() -> bool {
    trace("friends.SendClanChatMessage (not supported)");
    false
}
unsafe extern "C" fn slot_59() -> i32 {
    trace("friends.GetClanChatMessage (not supported)");
    0
}
unsafe extern "C" fn slot_60() -> bool {
    trace("friends.IsClanChatAdmin (not supported)");
    false
}
unsafe extern "C" fn slot_61() -> bool {
    trace("friends.IsClanChatWindowOpenInSteam (not supported)");
    false
}
unsafe extern "C" fn slot_62() -> bool {
    trace("friends.OpenClanChatWindowInSteam (not supported)");
    false
}
unsafe extern "C" fn slot_63() -> bool {
    trace("friends.CloseClanChatWindowInSteam (not supported)");
    false
}
unsafe extern "C" fn slot_64() -> bool {
    trace("friends.SetListenForFriendsMessages (no social graph)");
    false
}
unsafe extern "C" fn slot_65() -> bool {
    trace("friends.ReplyToFriendMessage (not supported)");
    false
}
unsafe extern "C" fn slot_66() -> i32 {
    trace("friends.GetFriendMessage (not supported)");
    0
}
unsafe extern "C" fn slot_67() -> u64 {
    trace("friends.GetFollowerCount (not supported)");
    0
}
unsafe extern "C" fn slot_68() -> u64 {
    trace("friends.IsFollowing (not supported)");
    0
}
unsafe extern "C" fn slot_69() -> u64 {
    trace("friends.EnumerateFollowingList (not supported)");
    0
}
unsafe extern "C" fn slot_70() -> bool {
    trace("friends.IsClanPublic (not supported)");
    false
}
unsafe extern "C" fn slot_71() -> bool {
    trace("friends.IsClanOfficialGameGroup (not supported)");
    false
}
unsafe extern "C" fn slot_72() -> i32 {
    trace("friends.GetNumChatsWithUnreadPriorityMessages (no social graph)");
    0
}
unsafe extern "C" fn slot_73() {
    trace("friends.ActivateGameOverlayRemotePlayTogetherInviteDialog (not supported)");
}
unsafe extern "C" fn slot_74() -> bool {
    trace("friends.RegisterProtocolInOverlayBrowser (not supported)");
    false
}
unsafe extern "C" fn slot_75() {
    trace("friends.ActivateGameOverlayInviteDialogConnectString (no social graph)");
}
unsafe extern "C" fn slot_76() -> u64 {
    trace("friends.RequestEquippedProfileItems (not supported)");
    0
}
unsafe extern "C" fn slot_77() -> bool {
    trace("friends.BHasEquippedProfileItem (not supported)");
    false
}
unsafe extern "C" fn slot_78() -> *const c_char {
    trace("friends.GetProfileItemPropertyString (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_79() -> u32 {
    trace("friends.GetProfileItemPropertyUint (not supported)");
    0
}

pub(super) static TABLE: VTable<80> = VTable([
    name as *const (),          // 0: GetPersonaName
    slot_1 as *const (),        // 1: SetPersonaName
    persona_state as *const (), // 2: GetPersonaState
    slot_3 as *const (),        // 3: GetFriendCount
    slot_4 as *const (),        // 4: GetFriendByIndex
    slot_5 as *const (),        // 5: GetFriendRelationship
    slot_6 as *const (),        // 6: GetFriendPersonaState
    slot_7 as *const (),        // 7: GetFriendPersonaName
    slot_8 as *const (),        // 8: GetFriendGamePlayed
    slot_9 as *const (),        // 9: GetFriendPersonaNameHistory
    slot_10 as *const (),       // 10: GetFriendSteamLevel
    slot_11 as *const (),       // 11: GetPlayerNickname
    slot_12 as *const (),       // 12: GetFriendsGroupCount
    slot_13 as *const (),       // 13: GetFriendsGroupIDByIndex
    slot_14 as *const (),       // 14: GetFriendsGroupName
    slot_15 as *const (),       // 15: GetFriendsGroupMembersCount
    slot_16 as *const (),       // 16: GetFriendsGroupMembersList
    slot_17 as *const (),       // 17: HasFriend
    slot_18 as *const (),       // 18: GetClanCount
    slot_19 as *const (),       // 19: GetClanByIndex
    slot_20 as *const (),       // 20: GetClanName
    slot_21 as *const (),       // 21: GetClanTag
    slot_22 as *const (),       // 22: GetClanActivityCounts
    slot_23 as *const (),       // 23: DownloadClanActivityCounts
    slot_24 as *const (),       // 24: GetFriendCountFromSource
    slot_25 as *const (),       // 25: GetFriendFromSourceByIndex
    slot_26 as *const (),       // 26: IsUserInSource
    slot_27 as *const (),       // 27: SetInGameVoiceSpeaking
    slot_28 as *const (),       // 28: ActivateGameOverlay
    slot_29 as *const (),       // 29: ActivateGameOverlayToUser
    slot_30 as *const (),       // 30: ActivateGameOverlayToWebPage
    slot_31 as *const (),       // 31: ActivateGameOverlayToStore
    slot_32 as *const (),       // 32: SetPlayedWith
    slot_33 as *const (),       // 33: ActivateGameOverlayInviteDialog
    slot_34 as *const (),       // 34: GetSmallFriendAvatar
    slot_35 as *const (),       // 35: GetMediumFriendAvatar
    slot_36 as *const (),       // 36: GetLargeFriendAvatar
    slot_37 as *const (),       // 37: RequestUserInformation
    slot_38 as *const (),       // 38: RequestClanOfficerList
    slot_39 as *const (),       // 39: GetClanOwner
    slot_40 as *const (),       // 40: GetClanOfficerCount
    slot_41 as *const (),       // 41: GetClanOfficerByIndex
    slot_42 as *const (),       // 42: GetUserRestrictions
    slot_43 as *const (),       // 43: SetRichPresence
    slot_44 as *const (),       // 44: ClearRichPresence
    slot_45 as *const (),       // 45: GetFriendRichPresence
    slot_46 as *const (),       // 46: GetFriendRichPresenceKeyCount
    slot_47 as *const (),       // 47: GetFriendRichPresenceKeyByIndex
    slot_48 as *const (),       // 48: RequestFriendRichPresence
    slot_49 as *const (),       // 49: InviteUserToGame
    slot_50 as *const (),       // 50: GetCoplayFriendCount
    slot_51 as *const (),       // 51: GetCoplayFriend
    slot_52 as *const (),       // 52: GetFriendCoplayTime
    slot_53 as *const (),       // 53: GetFriendCoplayGame
    slot_54 as *const (),       // 54: JoinClanChatRoom
    slot_55 as *const (),       // 55: LeaveClanChatRoom
    slot_56 as *const (),       // 56: GetClanChatMemberCount
    slot_57 as *const (),       // 57: GetChatMemberByIndex
    slot_58 as *const (),       // 58: SendClanChatMessage
    slot_59 as *const (),       // 59: GetClanChatMessage
    slot_60 as *const (),       // 60: IsClanChatAdmin
    slot_61 as *const (),       // 61: IsClanChatWindowOpenInSteam
    slot_62 as *const (),       // 62: OpenClanChatWindowInSteam
    slot_63 as *const (),       // 63: CloseClanChatWindowInSteam
    slot_64 as *const (),       // 64: SetListenForFriendsMessages
    slot_65 as *const (),       // 65: ReplyToFriendMessage
    slot_66 as *const (),       // 66: GetFriendMessage
    slot_67 as *const (),       // 67: GetFollowerCount
    slot_68 as *const (),       // 68: IsFollowing
    slot_69 as *const (),       // 69: EnumerateFollowingList
    slot_70 as *const (),       // 70: IsClanPublic
    slot_71 as *const (),       // 71: IsClanOfficialGameGroup
    slot_72 as *const (),       // 72: GetNumChatsWithUnreadPriorityMessages
    slot_73 as *const (),       // 73: ActivateGameOverlayRemotePlayTogetherInviteDialog
    slot_74 as *const (),       // 74: RegisterProtocolInOverlayBrowser
    slot_75 as *const (),       // 75: ActivateGameOverlayInviteDialogConnectString
    slot_76 as *const (),       // 76: RequestEquippedProfileItems
    slot_77 as *const (),       // 77: BHasEquippedProfileItem
    slot_78 as *const (),       // 78: GetProfileItemPropertyString
    slot_79 as *const (),       // 79: GetProfileItemPropertyUint
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// SteamFriends018 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
static TABLE_018: VTable<78> = VTable([
    TABLE.0[0],
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
    TABLE.0[43],
    TABLE.0[44],
    TABLE.0[45],
    TABLE.0[46],
    TABLE.0[47],
    TABLE.0[48],
    TABLE.0[49],
    TABLE.0[50],
    TABLE.0[51],
    TABLE.0[52],
    TABLE.0[53],
    TABLE.0[54],
    TABLE.0[55],
    TABLE.0[56],
    TABLE.0[57],
    TABLE.0[58],
    TABLE.0[59],
    TABLE.0[60],
    TABLE.0[61],
    TABLE.0[62],
    TABLE.0[63],
    TABLE.0[64],
    TABLE.0[65],
    TABLE.0[66],
    TABLE.0[67],
    TABLE.0[68],
    TABLE.0[69],
    TABLE.0[70],
    TABLE.0[71],
    TABLE.0[72],
    TABLE.0[73],
    TABLE.0[74],
    TABLE.0[75],
    TABLE.0[76],
    TABLE.0[77],
    TABLE.0[78],
    TABLE.0[79],
]);
pub(super) static OBJECT_018: Object = Object {
    table: TABLE_018.0.as_ptr(),
};
