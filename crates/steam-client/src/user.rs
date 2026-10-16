// Slot order: Valve Steamworks SteamUser021 (before WebAPI ticket insertion).
use super::*;

unsafe extern "C" fn slot_3() -> ! {
    unsupported("user.InitiateGameConnection_DEPRECATED")
}
unsafe extern "C" fn slot_4() -> ! {
    unsupported("user.TerminateGameConnection_DEPRECATED")
}
unsafe extern "C" fn slot_5() {
    trace("user.TrackAppUsageEvent");
}
unsafe extern "C" fn slot_6(_this: *mut c_void, _buffer: *mut u8, _capacity: i32) -> bool {
    trace("user.GetUserDataFolder = false");
    false
}
unsafe extern "C" fn slot_7(_this: *mut c_void) {
    trace("user.StartVoiceRecording (no voice support)");
}
unsafe extern "C" fn slot_8(_this: *mut c_void) {
    trace("user.StopVoiceRecording (no voice support)");
}
unsafe extern "C" fn slot_9() -> u32 {
    trace("user.GetAvailableVoice = NotRecording");
    2
}
unsafe extern "C" fn slot_10() -> u32 {
    trace("user.GetVoice = NotRecording");
    2
}
unsafe extern "C" fn slot_11() -> u32 {
    trace("user.DecompressVoice = NotInitialized");
    1
}
unsafe extern "C" fn slot_12(_this: *mut c_void) -> u32 {
    trace("user.GetVoiceOptimalSampleRate = 11025");
    11025
}
unsafe extern "C" fn slot_14() -> ! {
    unsupported("user.BeginAuthSession")
}
unsafe extern "C" fn slot_15() -> ! {
    unsupported("user.EndAuthSession")
}
// Only the signed-in account on the running app can be answered (from the backend's
// ownership check); anything else has no auth session here.
unsafe extern "C" fn slot_17(_this: *mut c_void, steam_id: u64, app_id: u32) -> u32 {
    let (own, _) = bridge::identity(current_app_id());
    let result = if steam_id == own && app_id == current_app_id() {
        if bridge::owned(app_id) { 0 } else { 1 }
    } else {
        2
    };
    trace(format_args!(
        "user.UserHasLicenseForApp({app_id}) = {result}"
    ));
    result
}
unsafe extern "C" fn slot_18() -> bool {
    trace("user.BIsBehindNAT");
    false
}
unsafe extern "C" fn slot_19() {
    trace("user.AdvertiseGame");
}
unsafe extern "C" fn slot_20() -> u64 {
    trace("user.RequestEncryptedAppTicket = invalid call (not supported)");
    0
}
unsafe extern "C" fn slot_21(
    _this: *mut c_void,
    _ticket: *mut u8,
    _max: i32,
    size: *mut u32,
) -> bool {
    trace("user.GetEncryptedAppTicket = false");
    if !size.is_null() {
        // SAFETY: caller-supplied out-pointer.
        unsafe { size.write(0) };
    }
    false
}
unsafe extern "C" fn slot_22() -> i32 {
    trace("user.GetGameBadgeLevel = 0 (unknown)");
    0
}
unsafe extern "C" fn slot_23() -> i32 {
    trace("user.GetPlayerSteamLevel = 0 (unknown)");
    0
}
unsafe extern "C" fn slot_24() -> u64 {
    trace("user.RequestStoreAuthURL = invalid call (not supported)");
    0
}
unsafe extern "C" fn slot_25() -> bool {
    trace("user.BIsPhoneVerified = false (unknown)");
    false
}
unsafe extern "C" fn slot_26() -> bool {
    trace("user.BIsTwoFactorEnabled = false (unknown)");
    false
}
unsafe extern "C" fn slot_27() -> bool {
    trace("user.BIsPhoneIdentifying = false (unknown)");
    false
}
unsafe extern "C" fn slot_28() -> bool {
    trace("user.BIsPhoneRequiringVerification = false (unknown)");
    false
}
unsafe extern "C" fn slot_29() -> u64 {
    trace("user.GetMarketEligibility = invalid call (not supported)");
    0
}
unsafe extern "C" fn slot_30() -> u64 {
    trace("user.GetDurationControl = invalid call (not supported)");
    0
}
unsafe extern "C" fn slot_31() -> bool {
    trace("user.BSetDurationControlOnlineState = false (unknown)");
    false
}

pub(super) static TABLE: VTable<32> = VTable([
    get_user as *const (),     // 0: GetHSteamUser
    logged_on as *const (),    // 1: BLoggedOn
    steam_id as *const (),     // 2: GetSteamID
    slot_3 as *const (),       // 3: InitiateGameConnection_DEPRECATED
    slot_4 as *const (),       // 4: TerminateGameConnection_DEPRECATED
    slot_5 as *const (),       // 5: TrackAppUsageEvent
    slot_6 as *const (),       // 6: GetUserDataFolder
    slot_7 as *const (),       // 7: StartVoiceRecording
    slot_8 as *const (),       // 8: StopVoiceRecording
    slot_9 as *const (),       // 9: GetAvailableVoice
    slot_10 as *const (),      // 10: GetVoice
    slot_11 as *const (),      // 11: DecompressVoice
    slot_12 as *const (),      // 12: GetVoiceOptimalSampleRate
    auth::ticket as *const (), // 13: GetAuthSessionTicket
    slot_14 as *const (),      // 14: BeginAuthSession
    slot_15 as *const (),      // 15: EndAuthSession
    auth::cancel as *const (), // 16: CancelAuthTicket
    slot_17 as *const (),      // 17: UserHasLicenseForApp
    slot_18 as *const (),      // 18: BIsBehindNAT
    slot_19 as *const (),      // 19: AdvertiseGame
    slot_20 as *const (),      // 20: RequestEncryptedAppTicket
    slot_21 as *const (),      // 21: GetEncryptedAppTicket
    slot_22 as *const (),      // 22: GetGameBadgeLevel
    slot_23 as *const (),      // 23: GetPlayerSteamLevel
    slot_24 as *const (),      // 24: RequestStoreAuthURL
    slot_25 as *const (),      // 25: BIsPhoneVerified
    slot_26 as *const (),      // 26: BIsTwoFactorEnabled
    slot_27 as *const (),      // 27: BIsPhoneIdentifying
    slot_28 as *const (),      // 28: BIsPhoneRequiringVerification
    slot_29 as *const (),      // 29: GetMarketEligibility
    slot_30 as *const (),      // 30: GetDurationControl
    slot_31 as *const (),      // 31: BSetDurationControlOnlineState
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

unsafe extern "C" fn auth023(
    this: *mut c_void,
    buffer: *mut c_void,
    capacity: i32,
    size: *mut u32,
    remote_identity: *const c_void,
) -> u32 {
    if !remote_identity.is_null() {
        if !size.is_null() {
            unsafe { size.write(0) };
        }
        trace("SteamUser023: remote-bound auth tickets are unsupported");
        return 0;
    }
    unsafe { auth::ticket(this, buffer, capacity, size) }
}
// SteamUser023 adds GetAuthTicketForWebApi at slot 14 and extends the auth
// session signature. Use its own table; never alias the SteamUser021 object.
static TABLE023: VTable<33> = {
    let mut entries = [auth::web_ticket as *const (); 33];
    let mut slot = 0;
    while slot < 14 {
        entries[slot] = TABLE.0[slot];
        slot += 1;
    }
    entries[13] = auth023 as *const ();
    slot = 14;
    while slot < 32 {
        entries[slot + 1] = TABLE.0[slot];
        slot += 1;
    }
    VTable(entries)
};
pub(super) static OBJECT023: Object = Object {
    table: TABLE023.0.as_ptr(),
};
