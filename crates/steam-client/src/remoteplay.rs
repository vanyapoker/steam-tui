//! STEAMREMOTEPLAY_INTERFACE_VERSION001: no remote play sessions.
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("remoteplay.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "GetSessionCount";
    f1 => "GetSessionID";
    f2 => "GetSessionSteamID";
    f3 => "GetSessionClientName";
    f4 => "GetSessionClientFormFactor";
    f5 => "BGetSessionClientResolution";
    f6 => "BSendRemotePlayTogetherInvite";
}
static TABLE: VTable<7> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
    f6 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// STEAMREMOTEPLAY_INTERFACE_VERSION002 (SDK 161 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_002_6 => "remoteplay.BStartRemotePlayTogether";
}
static TABLE_002: VTable<8> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    n_002_6 as *const (),
    TABLE.0[6],
]);
pub(super) static OBJECT_002: Object = Object {
    table: TABLE_002.0.as_ptr(),
};

// STEAMREMOTEPLAY_INTERFACE_VERSION003 (SDK 163 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_003_6 => "remoteplay.ShowRemotePlayTogetherUI";
    n_003_8 => "remoteplay.BEnableRemotePlayTogetherDirectInput";
    n_003_9 => "remoteplay.DisableRemotePlayTogetherDirectInput";
    n_003_10 => "remoteplay.GetInput";
    n_003_11 => "remoteplay.SetMouseVisibility";
    n_003_12 => "remoteplay.SetMousePosition";
    n_003_13 => "remoteplay.CreateMouseCursor";
    n_003_14 => "remoteplay.SetMouseCursor";
}
static TABLE_003: VTable<15> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    n_003_6 as *const (),
    TABLE.0[6],
    n_003_8 as *const (),
    n_003_9 as *const (),
    n_003_10 as *const (),
    n_003_11 as *const (),
    n_003_12 as *const (),
    n_003_13 as *const (),
    n_003_14 as *const (),
]);
pub(super) static OBJECT_003: Object = Object {
    table: TABLE_003.0.as_ptr(),
};

// STEAMREMOTEPLAY_INTERFACE_VERSION004 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_004_2 => "remoteplay.BSessionRemotePlayTogether";
    n_004_4 => "remoteplay.GetSessionGuestID";
    n_004_5 => "remoteplay.GetSmallSessionAvatar";
    n_004_6 => "remoteplay.GetMediumSessionAvatar";
    n_004_7 => "remoteplay.GetLargeSessionAvatar";
    n_004_11 => "remoteplay.ShowRemotePlayTogetherUI";
    n_004_13 => "remoteplay.BEnableRemotePlayTogetherDirectInput";
    n_004_14 => "remoteplay.DisableRemotePlayTogetherDirectInput";
    n_004_15 => "remoteplay.GetInput";
    n_004_16 => "remoteplay.SetMouseVisibility";
    n_004_17 => "remoteplay.SetMousePosition";
    n_004_18 => "remoteplay.CreateMouseCursor";
    n_004_19 => "remoteplay.SetMouseCursor";
}
static TABLE_004: VTable<20> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    n_004_2 as *const (),
    TABLE.0[2],
    n_004_4 as *const (),
    n_004_5 as *const (),
    n_004_6 as *const (),
    n_004_7 as *const (),
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    n_004_11 as *const (),
    TABLE.0[6],
    n_004_13 as *const (),
    n_004_14 as *const (),
    n_004_15 as *const (),
    n_004_16 as *const (),
    n_004_17 as *const (),
    n_004_18 as *const (),
    n_004_19 as *const (),
]);
pub(super) static OBJECT_004: Object = Object {
    table: TABLE_004.0.as_ptr(),
};
