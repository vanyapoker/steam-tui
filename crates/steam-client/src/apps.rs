// SteamApps008 slot order from Valve public headers.
use super::*;

unsafe extern "C" fn subscribed(_this: *mut c_void) -> bool {
    trace("apps.BIsSubscribed");
    bridge::owned(current_app_id())
}
unsafe extern "C" fn low_violence(_this: *mut c_void) -> bool {
    trace("apps.BIsLowViolence = false");
    false
}
unsafe extern "C" fn cybercafe(_this: *mut c_void) -> bool {
    trace("apps.BIsCybercafe = false");
    false
}
// Language is a local setting, not account state; STEAM_TUI_LANGUAGE overrides it.
fn language() -> &'static CStr {
    static LANG: OnceLock<std::ffi::CString> = OnceLock::new();
    LANG.get_or_init(|| {
        let name = std::env::var("STEAM_TUI_LANGUAGE").unwrap_or_else(|_| "english".into());
        std::ffi::CString::new(name).unwrap_or_else(|_| c"english".into())
    })
}
unsafe extern "C" fn current_language(_this: *mut c_void) -> *const c_char {
    trace(format_args!(
        "apps.GetCurrentGameLanguage = {:?}",
        language()
    ));
    language().as_ptr()
}
unsafe extern "C" fn available_languages(_this: *mut c_void) -> *const c_char {
    trace("apps.GetAvailableGameLanguages");
    language().as_ptr()
}
unsafe extern "C" fn subscribed_app(_this: *mut c_void, app_id: u32) -> bool {
    trace(format_args!("apps.BIsSubscribedApp {app_id}"));
    bridge::subscribed_app(current_app_id(), app_id)
}
unsafe extern "C" fn slot_3() -> ! {
    unsupported("apps.BIsVACBanned")
}
unsafe extern "C" fn slot_7(_this: *mut c_void, _app_id: u32) -> bool {
    trace("apps.BIsDlcInstalled = false");
    false
}
unsafe extern "C" fn slot_8(_this: *mut c_void, _app_id: u32) -> u32 {
    trace("apps.GetEarliestPurchaseUnixTime = 0 (unknown)");
    0
}
unsafe extern "C" fn slot_9(_this: *mut c_void) -> bool {
    trace("apps.BIsSubscribedFromFreeWeekend = false");
    false
}
unsafe extern "C" fn slot_10(_this: *mut c_void) -> i32 {
    trace("apps.GetDLCCount = 0");
    0
}
unsafe extern "C" fn slot_11(_this: *mut c_void) -> bool {
    trace("apps.BGetDLCDataByIndex = false");
    false
}
unsafe extern "C" fn slot_12(_this: *mut c_void, _app_id: u32) {
    trace("apps.InstallDLC (ignored)");
}
unsafe extern "C" fn slot_13(_this: *mut c_void, _app_id: u32) {
    trace("apps.UninstallDLC (ignored)");
}
unsafe extern "C" fn slot_14(_this: *mut c_void, _app_id: u32) {
    trace("apps.RequestAppProofOfPurchaseKey (ignored)");
}
// Direct launches always run the public branch.
unsafe extern "C" fn slot_15(_this: *mut c_void, _buffer: *mut u8, _capacity: i32) -> bool {
    trace("apps.GetCurrentBetaName = false (public branch)");
    false
}
unsafe extern "C" fn slot_16(_this: *mut c_void, _missing_only: bool) -> bool {
    trace("apps.MarkContentCorrupt = false");
    false
}
unsafe extern "C" fn slot_17(
    _this: *mut c_void,
    _app_id: u32,
    _depots: *mut u32,
    _max: u32,
) -> u32 {
    trace("apps.GetInstalledDepots = 0");
    0
}
// Known only for the running app (STEAM_COMPAT_INSTALL_PATH set by the launcher).
unsafe extern "C" fn slot_18(
    _this: *mut c_void,
    app_id: u32,
    folder: *mut u8,
    capacity: u32,
) -> u32 {
    let path = (app_id == current_app_id())
        .then(|| std::env::var("STEAM_COMPAT_INSTALL_PATH").ok())
        .flatten();
    let Some(path) = path.filter(|p| !p.is_empty() && p.len() < capacity as usize) else {
        trace(format_args!(
            "apps.GetAppInstallDir({app_id}) = 0 (unknown)"
        ));
        return 0;
    };
    trace(format_args!("apps.GetAppInstallDir({app_id}) = {path}"));
    if !folder.is_null() {
        // SAFETY: capacity was checked above (path plus NUL fits).
        unsafe {
            ptr::copy_nonoverlapping(path.as_ptr(), folder, path.len());
            folder.add(path.len()).write(0);
        }
    }
    path.len() as u32 + 1
}
// Only the running app is known to be installed (its files are being used).
unsafe extern "C" fn slot_19(_this: *mut c_void, app_id: u32) -> bool {
    let installed = app_id == current_app_id();
    trace(format_args!("apps.BIsAppInstalled({app_id}) = {installed}"));
    installed
}
// CSteamID returned in RAX: the signed-in account (no family sharing here).
unsafe extern "C" fn slot_20(_this: *mut c_void) -> u64 {
    let (steam_id, _) = bridge::identity(current_app_id());
    trace("apps.GetAppOwner = own account");
    steam_id
}
unsafe extern "C" fn slot_21(_this: *mut c_void, _key: *const c_char) -> *const c_char {
    trace("apps.GetLaunchQueryParam = \"\"");
    c"".as_ptr()
}
unsafe extern "C" fn slot_22(_this: *mut c_void) -> bool {
    trace("apps.GetDlcDownloadProgress = false");
    false
}
unsafe extern "C" fn slot_23(_this: *mut c_void) -> i32 {
    trace("apps.GetAppBuildId = 0 (unknown)");
    0
}
unsafe extern "C" fn slot_24(_this: *mut c_void, _app_id: u32) {
    trace("apps.RequestAllProofOfPurchaseKeys (ignored)");
}
unsafe extern "C" fn slot_25(_this: *mut c_void, _name: *const c_char) -> u64 {
    trace("apps.GetFileDetails = invalid call handle");
    0
}
// A directly launched game has no Steam-supplied launch command line (that comes
// from steam:// URLs and invite links), so report an empty one.
unsafe extern "C" fn slot_26(_this: *mut c_void, buffer: *mut u8, capacity: i32) -> i32 {
    trace("apps.GetLaunchCommandLine = \"\"");
    if !buffer.is_null() && capacity > 0 {
        // SAFETY: caller supplied a writable buffer of `capacity` bytes.
        unsafe { buffer.write(0) };
    }
    0
}
unsafe extern "C" fn slot_27(_this: *mut c_void) -> bool {
    trace("apps.BIsSubscribedFromFamilySharing = false");
    false
}
unsafe extern "C" fn slot_28(_this: *mut c_void, _allowed: *mut u32, _played: *mut u32) -> bool {
    trace("apps.BIsTimedTrial = false");
    false
}
unsafe extern "C" fn slot_29(_this: *mut c_void, _app_id: u32) -> bool {
    trace("apps.SetDlcContext = true");
    true
}

unsafe extern "C" fn slot_30(_this: *mut c_void, available: *mut i32, private: *mut i32) -> i32 {
    trace("apps.GetNumBetas = 0");
    for out in [available, private] {
        if !out.is_null() {
            unsafe { out.write(0) };
        }
    }
    0
}
unsafe extern "C" fn slot_31() -> bool {
    trace("apps.GetBetaInfo = false");
    false
}
unsafe extern "C" fn slot_32() -> bool {
    trace("apps.SetActiveBeta = false");
    false
}

pub(super) static TABLE: VTable<33> = VTable([
    subscribed as *const (),          // 0: BIsSubscribed
    low_violence as *const (),        // 1: BIsLowViolence
    cybercafe as *const (),           // 2: BIsCybercafe
    slot_3 as *const (),              // 3: BIsVACBanned
    current_language as *const (),    // 4: GetCurrentGameLanguage
    available_languages as *const (), // 5: GetAvailableGameLanguages
    subscribed_app as *const (),      // 6: BIsSubscribedApp
    slot_7 as *const (),              // 7: BIsDlcInstalled
    slot_8 as *const (),              // 8: GetEarliestPurchaseUnixTime
    slot_9 as *const (),              // 9: BIsSubscribedFromFreeWeekend
    slot_10 as *const (),             // 10: GetDLCCount
    slot_11 as *const (),             // 11: BGetDLCDataByIndex
    slot_12 as *const (),             // 12: InstallDLC
    slot_13 as *const (),             // 13: UninstallDLC
    slot_14 as *const (),             // 14: RequestAppProofOfPurchaseKey
    slot_15 as *const (),             // 15: GetCurrentBetaName
    slot_16 as *const (),             // 16: MarkContentCorrupt
    slot_17 as *const (),             // 17: GetInstalledDepots
    slot_18 as *const (),             // 18: GetAppInstallDir
    slot_19 as *const (),             // 19: BIsAppInstalled
    slot_20 as *const (),             // 20: GetAppOwner
    slot_21 as *const (),             // 21: GetLaunchQueryParam
    slot_22 as *const (),             // 22: GetDlcDownloadProgress
    slot_23 as *const (),             // 23: GetAppBuildId
    slot_24 as *const (),             // 24: RequestAllProofOfPurchaseKeys
    slot_25 as *const (),             // 25: GetFileDetails
    slot_26 as *const (),             // 26: GetLaunchCommandLine
    slot_27 as *const (),             // 27: BIsSubscribedFromFamilySharing
    slot_28 as *const (),             // 28: BIsTimedTrial
    slot_29 as *const (),             // 29: SetDlcContext
    slot_30 as *const (),             // 30: GetNumBetas
    slot_31 as *const (),             // 31: GetBetaInfo
    slot_32 as *const (),             // 32: SetActiveBeta
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// STEAMAPPS_INTERFACE_VERSION009 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_009_33 => "apps.SetGamePerformanceSetting";
    n_009_34 => "apps.SetGameRenderResolution";
}
static TABLE_009: VTable<35> = VTable([
    TABLE.0[0],
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
    n_009_33 as *const (),
    n_009_34 as *const (),
]);
pub(super) static OBJECT_009: Object = Object {
    table: TABLE_009.0.as_ptr(),
};
