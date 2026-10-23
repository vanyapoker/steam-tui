// SteamApps008 slot order from Valve public headers.
use super::*;
use steam_ipc::{AppsOp, AppsSnapshot, Operation, Response};

type SnapshotCache = Option<(
    std::time::Instant,
    u32,
    std::ffi::OsString,
    Option<std::sync::Arc<AppsSnapshot>>,
)>;
static SNAPSHOT: Mutex<SnapshotCache> = Mutex::new(None);

fn invalidate_snapshot() {
    *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn snapshot() -> Option<std::sync::Arc<AppsSnapshot>> {
    let app = current_app_id();
    let socket = std::env::var_os("STEAM_TUI_BACKEND_SOCKET").unwrap_or_default();
    let mut cached = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, id, path, snapshot)) = &*cached
        && *id == app
        && *path == socket
        && at.elapsed() < std::time::Duration::from_millis(100)
    {
        return snapshot.clone();
    }
    let snapshot = match bridge::request(app, Operation::Apps(AppsOp::Snapshot)) {
        Ok(Response::Apps(snapshot)) => Some(std::sync::Arc::new(snapshot)),
        _ => None,
    };
    *cached = Some((std::time::Instant::now(), app, socket, snapshot.clone()));
    snapshot
}

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
unsafe extern "C" fn slot_7(_this: *mut c_void, app_id: u32) -> bool {
    snapshot().is_some_and(|s| s.dlc.iter().any(|d| d.app_id == app_id && d.installed))
        && bridge::subscribed_app(current_app_id(), app_id)
}
unsafe extern "C" fn slot_8(_this: *mut c_void, app_id: u32) -> u32 {
    let time = match bridge::request(current_app_id(), Operation::PurchaseTime { app_id }) {
        Ok(Response::PurchaseTime(time)) => time,
        _ => 0,
    };
    trace(format_args!(
        "apps.GetEarliestPurchaseUnixTime({app_id}) = {time}"
    ));
    time
}
unsafe extern "C" fn slot_9(_this: *mut c_void) -> bool {
    trace("apps.BIsSubscribedFromFreeWeekend = false");
    false
}
unsafe extern "C" fn slot_10(_this: *mut c_void) -> i32 {
    snapshot().map_or(0, |s| s.dlc.len().min(i32::MAX as usize) as i32)
}
unsafe extern "C" fn slot_11(
    _this: *mut c_void,
    index: i32,
    app: *mut u32,
    available: *mut bool,
    name: *mut c_char,
    capacity: i32,
) -> bool {
    if app.is_null() || available.is_null() || name.is_null() || capacity <= 0 {
        return false;
    }
    unsafe {
        app.write(0);
        available.write(false);
        name.write(0);
    }
    let Some(s) = snapshot() else {
        return false;
    };
    let Some(dlc) = usize::try_from(index).ok().and_then(|i| s.dlc.get(i)) else {
        return false;
    };
    if dlc.name.is_empty() || dlc.name.contains('\0') {
        return false;
    }
    let mut count = dlc.name.len().min(capacity as usize - 1);
    while !dlc.name.is_char_boundary(count) {
        count -= 1;
    }
    unsafe {
        app.write(dlc.app_id);
        available.write(dlc.available);
        ptr::copy_nonoverlapping(dlc.name.as_ptr(), name.cast::<u8>(), count);
        name.add(count).write(0);
    }
    true
}
unsafe extern "C" fn slot_12(_this: *mut c_void, app_id: u32) {
    invalidate_snapshot();
    if let Err(error) = bridge::request(
        current_app_id(),
        Operation::Apps(AppsOp::InstallDlc { app_id }),
    ) {
        trace(format_args!("apps.InstallDLC({app_id}) failed: {error}"));
    }
    invalidate_snapshot();
}
unsafe extern "C" fn slot_13(_this: *mut c_void, app_id: u32) {
    invalidate_snapshot();
    if let Err(error) = bridge::request(
        current_app_id(),
        Operation::Apps(AppsOp::UninstallDlc { app_id }),
    ) {
        trace(format_args!("apps.UninstallDLC({app_id}) failed: {error}"));
    }
    invalidate_snapshot();
}
unsafe extern "C" fn slot_14(_this: *mut c_void, _app_id: u32) {
    trace("apps.RequestAppProofOfPurchaseKey (ignored)");
}
unsafe extern "C" fn slot_15(_this: *mut c_void, buffer: *mut u8, capacity: i32) -> bool {
    if buffer.is_null() || capacity <= 0 {
        return false;
    }
    unsafe {
        buffer.write(0);
    }
    let Some(s) = snapshot() else {
        return false;
    };
    if s.branch.eq_ignore_ascii_case("public")
        || s.branch.len() >= capacity as usize
        || s.branch.contains('\0')
    {
        return false;
    }
    unsafe {
        ptr::copy_nonoverlapping(s.branch.as_ptr(), buffer, s.branch.len());
        buffer.add(s.branch.len()).write(0);
    }
    true
}
unsafe extern "C" fn slot_16(_this: *mut c_void, _missing_only: bool) -> bool {
    trace("apps.MarkContentCorrupt = false");
    false
}
unsafe extern "C" fn slot_17(_this: *mut c_void, app_id: u32, depots: *mut u32, max: u32) -> u32 {
    if depots.is_null() || max == 0 {
        return 0;
    }
    let Some(s) = snapshot() else {
        return 0;
    };
    let ids = if app_id == current_app_id() {
        &s.depots
    } else {
        let Some(dlc) = s.dlc.iter().find(|d| d.app_id == app_id && d.installed) else {
            return 0;
        };
        &dlc.depots
    };
    let count = ids.len().min(max as usize);
    unsafe {
        ptr::copy_nonoverlapping(ids.as_ptr(), depots, count);
    }
    count as u32
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
unsafe extern "C" fn slot_22(
    _this: *mut c_void,
    app_id: u32,
    downloaded: *mut u64,
    total: *mut u64,
) -> bool {
    if downloaded.is_null() || total.is_null() {
        return false;
    }
    unsafe {
        downloaded.write(0);
        total.write(0);
    }
    let Some(s) = snapshot() else {
        return false;
    };
    let Some(dlc) = s.dlc.iter().find(|d| d.app_id == app_id && d.downloading) else {
        return false;
    };
    unsafe {
        downloaded.write(dlc.downloaded);
        total.write(dlc.total);
    }
    true
}
unsafe extern "C" fn slot_23(_this: *mut c_void) -> i32 {
    snapshot().map_or(0, |s| s.build_id as i32)
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
    trace("apps.SetDlcContext is unavailable");
    false
}

#[cfg(test)]
mod tests;

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
