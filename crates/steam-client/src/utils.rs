// Slot order: Valve Steamworks SteamUtils010.
use super::*;

unsafe extern "C" fn slot_0() -> u32 {
    // Process uptime stands in for both: there is no desktop session tracking here.
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let secs = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs() as u32;
    trace(format_args!("utils.GetSecondsSinceAppActive = {secs}"));
    secs
}
unsafe extern "C" fn slot_1() -> u32 {
    // Process uptime stands in for both: there is no desktop session tracking here.
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let secs = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs() as u32;
    trace(format_args!("utils.GetSecondsSinceComputerActive = {secs}"));
    secs
}
unsafe extern "C" fn slot_2(_this: *mut c_void) -> i32 {
    // This backend connects to the public Steam CM directory, including when
    // temporarily disconnected; it never selects Beta/Internal universes.
    trace("utils.GetConnectedUniverse = Public");
    1
}
// Local UTC clock; the real client corrects it by the CM-reported offset, which we
// do not track yet, so a badly skewed system clock will skew this too.
unsafe extern "C" fn slot_3(_this: *mut c_void) -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);
    trace(format_args!("utils.GetServerRealTime = {now}"));
    now
}
unsafe extern "C" fn slot_4(_this: *mut c_void) -> *const c_char {
    static COUNTRY: OnceLock<std::ffi::CString> = OnceLock::new();
    static EMPTY: OnceLock<std::ffi::CString> = OnceLock::new();
    if let Some(country) = COUNTRY.get() {
        return country.as_ptr();
    }
    if let Ok(steam_ipc::Response::Country { code }) =
        bridge::request(current_app_id(), steam_ipc::Operation::Country)
        && code.len() == 2
        && code.bytes().all(|b| b.is_ascii_uppercase())
    {
        return COUNTRY
            .get_or_init(|| std::ffi::CString::new(code).expect("validated ASCII"))
            .as_ptr();
    }
    EMPTY.get_or_init(std::ffi::CString::default).as_ptr()
}
unsafe extern "C" fn slot_5(
    _this: *mut c_void,
    image: i32,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    // Handles come from achievement icon fetches; an unknown handle (including
    // the no-image handle 0) is an ordinary SDK query failure.
    let found = images::size(image);
    let (w, h) = found.unwrap_or((0, 0));
    if !width.is_null() {
        unsafe { width.write(w) };
    }
    if !height.is_null() {
        unsafe { height.write(h) };
    }
    found.is_some()
}
unsafe extern "C" fn slot_6(
    _this: *mut c_void,
    image: i32,
    destination: *mut u8,
    size: i32,
) -> bool {
    let ok = unsafe { images::copy(image, destination, size) };
    if !ok {
        trace(format_args!("utils.GetImageRGBA({image}) = false"));
    }
    ok
}
unsafe extern "C" fn slot_7() -> ! {
    unsupported("utils.GetCSERIPPort")
}
unsafe extern "C" fn slot_8() -> u8 {
    trace("utils.GetCurrentBatteryPower");
    255
}
unsafe extern "C" fn slot_10() {
    trace("utils.SetOverlayNotificationPosition");
}
unsafe extern "C" fn slot_11(_this: *mut c_void, call: u64, failed: *mut bool) -> bool {
    if !failed.is_null() {
        // SAFETY: caller-supplied out-pointer.
        unsafe { failed.write(false) };
    }
    callresults::is_completed(call)
}
// k_ESteamAPICallFailureNone: results we hold never failed.
unsafe extern "C" fn slot_12(_this: *mut c_void, _call: u64) -> i32 {
    -1
}
unsafe extern "C" fn slot_13(
    _this: *mut c_void,
    call: u64,
    buffer: *mut c_void,
    size: i32,
    expected: i32,
    failed: *mut bool,
) -> bool {
    let found = callresults::take(call, expected, buffer.cast(), size);
    if !failed.is_null() {
        // SAFETY: caller-supplied out-pointer.
        unsafe { failed.write(!found) };
    }
    found
}
unsafe extern "C" fn slot_14(_this: *mut c_void) {}
unsafe extern "C" fn slot_15() -> u32 {
    trace("utils.GetIPCCallCount");
    0
}
unsafe extern "C" fn slot_16() {
    trace("utils.SetWarningMessageHook");
}
unsafe extern "C" fn slot_17() -> bool {
    trace("utils.IsOverlayEnabled");
    false
}
unsafe extern "C" fn slot_18() -> bool {
    trace("utils.BOverlayNeedsPresent");
    false
}
unsafe extern "C" fn slot_19() -> ! {
    unsupported("utils.CheckFileSignature")
}
// There is no overlay to host the Big Picture keyboard; `false` is Steam's own
// answer when it cannot show it, and games then use their built-in text entry.
unsafe extern "C" fn slot_20() -> bool {
    trace("utils.ShowGamepadTextInput = false (no overlay)");
    false
}
unsafe extern "C" fn slot_21() -> u32 {
    trace("utils.GetEnteredGamepadTextLength = 0");
    0
}
unsafe extern "C" fn slot_22(_this: *mut c_void, text: *mut c_char, capacity: u32) -> bool {
    trace("utils.GetEnteredGamepadTextInput = false");
    if !text.is_null() && capacity > 0 {
        // SAFETY: the caller supplied `capacity` writable bytes at `text`.
        unsafe { text.write(0) };
    }
    false
}
unsafe extern "C" fn slot_23() -> *const c_char {
    trace("utils.GetSteamUILanguage");
    c"english".as_ptr()
}
unsafe extern "C" fn slot_24() -> bool {
    trace("utils.IsSteamRunningInVR");
    false
}
unsafe extern "C" fn slot_25() {
    trace("utils.SetOverlayNotificationInset");
}
unsafe extern "C" fn slot_26() -> bool {
    trace("utils.IsSteamInBigPictureMode");
    false
}
unsafe extern "C" fn slot_27() -> ! {
    unsupported("utils.StartVRDashboard")
}
unsafe extern "C" fn slot_28() -> bool {
    trace("utils.IsVRHeadsetStreamingEnabled");
    false
}
unsafe extern "C" fn slot_29() -> ! {
    unsupported("utils.SetVRHeadsetStreamingEnabled")
}
unsafe extern "C" fn slot_30() -> bool {
    trace("utils.IsSteamChinaLauncher");
    false
}
unsafe extern "C" fn slot_31(_this: *mut c_void, _options: u32) -> bool {
    // No dictionaries/settings are loaded yet. Steamworks defines false as
    // filtering unavailable, with FilterText then acting as a passthrough.
    trace("utils.InitFilterText = false (filtering unavailable)");
    false
}
unsafe extern "C" fn slot_32(
    _this: *mut c_void,
    _context: i32,
    _source: u64,
    input: *const c_char,
    output: *mut c_char,
    capacity: u32,
) -> i32 {
    if output.is_null() || capacity == 0 {
        return 0;
    }
    if input.is_null() {
        unsafe {
            output.write(0);
        }
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    let mut count = bytes.len().min(capacity as usize - 1);
    if std::str::from_utf8(bytes).is_ok() {
        while count > 0 && count < bytes.len() && bytes[count] & 0xc0 == 0x80 {
            count -= 1;
        }
    }
    unsafe {
        ptr::copy(input, output, count);
        output.add(count).write(0);
    }
    0
}
unsafe extern "C" fn slot_33() -> ! {
    unsupported("utils.GetIPv6ConnectivityState")
}
unsafe extern "C" fn slot_34() -> bool {
    trace("utils.IsSteamRunningOnSteamDeck");
    false
}
unsafe extern "C" fn slot_35() -> bool {
    trace("utils.ShowFloatingGamepadTextInput = false (no overlay)");
    false
}
unsafe extern "C" fn slot_36() {
    trace("utils.SetGameLauncherMode");
}
unsafe extern "C" fn slot_37() -> bool {
    trace("utils.DismissFloatingGamepadTextInput = false");
    false
}

unsafe extern "C" fn slot_38() -> bool {
    trace("utils.DismissGamepadTextInput = false");
    false
}

pub(super) static TABLE: VTable<39> = VTable([
    slot_0 as *const (),  // 0: GetSecondsSinceAppActive
    slot_1 as *const (),  // 1: GetSecondsSinceComputerActive
    slot_2 as *const (),  // 2: GetConnectedUniverse
    slot_3 as *const (),  // 3: GetServerRealTime
    slot_4 as *const (),  // 4: GetIPCountry
    slot_5 as *const (),  // 5: GetImageSize
    slot_6 as *const (),  // 6: GetImageRGBA
    slot_7 as *const (),  // 7: GetCSERIPPort
    slot_8 as *const (),  // 8: GetCurrentBatteryPower
    app_id as *const (),  // 9: GetAppID
    slot_10 as *const (), // 10: SetOverlayNotificationPosition
    slot_11 as *const (), // 11: IsAPICallCompleted
    slot_12 as *const (), // 12: GetAPICallFailureReason
    slot_13 as *const (), // 13: GetAPICallResult
    slot_14 as *const (), // 14: RunFrame
    slot_15 as *const (), // 15: GetIPCCallCount
    slot_16 as *const (), // 16: SetWarningMessageHook
    slot_17 as *const (), // 17: IsOverlayEnabled
    slot_18 as *const (), // 18: BOverlayNeedsPresent
    slot_19 as *const (), // 19: CheckFileSignature
    slot_20 as *const (), // 20: ShowGamepadTextInput
    slot_21 as *const (), // 21: GetEnteredGamepadTextLength
    slot_22 as *const (), // 22: GetEnteredGamepadTextInput
    slot_23 as *const (), // 23: GetSteamUILanguage
    slot_24 as *const (), // 24: IsSteamRunningInVR
    slot_25 as *const (), // 25: SetOverlayNotificationInset
    slot_26 as *const (), // 26: IsSteamInBigPictureMode
    slot_27 as *const (), // 27: StartVRDashboard
    slot_28 as *const (), // 28: IsVRHeadsetStreamingEnabled
    slot_29 as *const (), // 29: SetVRHeadsetStreamingEnabled
    slot_30 as *const (), // 30: IsSteamChinaLauncher
    slot_31 as *const (), // 31: InitFilterText
    slot_32 as *const (), // 32: FilterText
    slot_33 as *const (), // 33: GetIPv6ConnectivityState
    slot_34 as *const (), // 34: IsSteamRunningOnSteamDeck
    slot_35 as *const (), // 35: ShowFloatingGamepadTextInput
    slot_36 as *const (), // 36: SetGameLauncherMode
    slot_37 as *const (), // 37: DismissFloatingGamepadTextInput
    slot_38 as *const (), // 38: DismissGamepadTextInput
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// SteamUtils011 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_011_37 => "utils.IsRunningOnSteamHardware";
    n_011_38 => "utils.GetSteamHardwareDefaultConfig";
    n_011_39 => "utils.IsRunningUnderProton";
}
static TABLE_011: VTable<40> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    TABLE.0[6],
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
    TABLE.0[35],
    TABLE.0[36],
    TABLE.0[37],
    TABLE.0[38],
    n_011_37 as *const (),
    n_011_38 as *const (),
    n_011_39 as *const (),
]);
pub(super) static OBJECT_011: Object = Object {
    table: TABLE_011.0.as_ptr(),
};
