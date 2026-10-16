// SteamFriends017 slot order from Valve public headers.
use super::*;
use std::ffi::CString;
use steam_ipc::{Operation, Response};

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
unsafe extern "C" fn slot_3() -> i32 {
    trace("friends.GetFriendCount (no social graph)");
    0
}
unsafe extern "C" fn slot_4() -> u64 {
    trace("friends.GetFriendByIndex (no social graph)");
    0
}
unsafe extern "C" fn slot_5() -> i32 {
    trace("friends.GetFriendRelationship (no social graph)");
    0
}
unsafe extern "C" fn slot_6() -> i32 {
    trace("friends.GetFriendPersonaState (no social graph)");
    0
}
unsafe extern "C" fn slot_7(_this: *mut c_void, id: u64) -> *const c_char {
    trace("friends.GetFriendPersonaName");
    let (own, _) = bridge::identity(current_app_id());
    if id == own && own != 0 {
        persona().0.as_ptr()
    } else {
        c"[unknown]".as_ptr()
    }
}
unsafe extern "C" fn slot_8() -> bool {
    trace("friends.GetFriendGamePlayed (no social graph)");
    false
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
unsafe extern "C" fn slot_17() -> bool {
    trace("friends.HasFriend (no social graph)");
    false
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
unsafe extern "C" fn slot_24() -> i32 {
    trace("friends.GetFriendCountFromSource (no social graph)");
    0
}
unsafe extern "C" fn slot_25() -> u64 {
    trace("friends.GetFriendFromSourceByIndex (not supported)");
    0
}
unsafe extern "C" fn slot_26() -> bool {
    trace("friends.IsUserInSource (no social graph)");
    false
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
unsafe extern "C" fn slot_32() {
    trace("friends.SetPlayedWith (no social graph)");
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
unsafe extern "C" fn slot_37() -> bool {
    trace("friends.RequestUserInformation (no social graph)");
    false
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
unsafe extern "C" fn slot_43() -> bool {
    trace("friends.SetRichPresence (no social graph)");
    false
}
unsafe extern "C" fn slot_44() {
    trace("friends.ClearRichPresence (no social graph)");
}
unsafe extern "C" fn slot_45() -> *const c_char {
    trace("friends.GetFriendRichPresence (no social graph)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_46() -> i32 {
    trace("friends.GetFriendRichPresenceKeyCount (no social graph)");
    0
}
unsafe extern "C" fn slot_47() -> *const c_char {
    trace("friends.GetFriendRichPresenceKeyByIndex (not supported)");
    c"".as_ptr()
}
unsafe extern "C" fn slot_48() {
    trace("friends.RequestFriendRichPresence (not supported)");
}
unsafe extern "C" fn slot_49() -> bool {
    trace("friends.InviteUserToGame (not supported)");
    false
}
unsafe extern "C" fn slot_50() -> i32 {
    trace("friends.GetCoplayFriendCount (no social graph)");
    0
}
unsafe extern "C" fn slot_51() -> u64 {
    trace("friends.GetCoplayFriend (not supported)");
    0
}
unsafe extern "C" fn slot_52() -> i32 {
    trace("friends.GetFriendCoplayTime (not supported)");
    0
}
unsafe extern "C" fn slot_53() -> u32 {
    trace("friends.GetFriendCoplayGame (not supported)");
    0
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
