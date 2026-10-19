//! SteamInput006: unavailable (Init fails) unless a virtual gamepad exists (see `pad`),
//! in which case that one device is reported. Action and motion data are always inactive.
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("input.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f2 => "SetInputActionManifestFilePath";
    f3 => "RunFrame";
    f4 => "BWaitForData";
    f5 => "BNewDataAvailable";
    f8 => "EnableActionEventCallbacks";
    f9 => "GetActionSetHandle";
    f10 => "ActivateActionSet";
    f11 => "GetCurrentActionSet";
    f12 => "ActivateActionSetLayer";
    f13 => "DeactivateActionSetLayer";
    f14 => "DeactivateAllActionSetLayers";
    f15 => "GetActiveActionSetLayers";
    f16 => "GetDigitalActionHandle";
    f17 => "GetDigitalActionData";
    f18 => "GetDigitalActionOrigins";
    f19 => "GetStringForDigitalActionName";
    f20 => "GetAnalogActionHandle";
    f22 => "GetAnalogActionOrigins";
    f23 => "GetGlyphPNGForActionOrigin";
    f24 => "GetGlyphSVGForActionOrigin";
    f25 => "GetGlyphForActionOrigin_Legacy";
    f26 => "GetStringForActionOrigin";
    f27 => "GetStringForAnalogActionName";
    f28 => "StopAnalogActionMomentum";
    f30 => "TriggerVibration";
    f31 => "TriggerVibrationExtended";
    f32 => "TriggerSimpleHapticEvent";
    f33 => "SetLEDColor";
    f34 => "Legacy_TriggerHapticPulse";
    f35 => "Legacy_TriggerRepeatedHapticPulse";
    f36 => "ShowBindingPanel";
    f40 => "GetStringForXboxOrigin";
    f41 => "GetGlyphForXboxOrigin";
    f42 => "GetActionOriginFromXboxOrigin";
    f43 => "TranslateActionOrigin";
    f44 => "GetDeviceBindingRevision";
    f45 => "GetRemotePlaySessionID";
    f46 => "GetSessionInputConfigurationSettings";
    f47 => "SetDualSenseTriggerEffect";
}
// With a virtual gamepad the one pad is reported (slot 0); without one Steam Input stays
// unavailable so games keep using their plain gamepad path. Action data is never served.
unsafe extern "C" fn f0(_this: *mut c_void, _explicit_run_frame: bool) -> bool {
    trace("input.Init");
    pad::present()
}
unsafe extern "C" fn f1(_this: *mut c_void) -> bool {
    trace("input.Shutdown");
    pad::present()
}
unsafe extern "C" fn f6(_this: *mut c_void, handles: *mut u64) -> i32 {
    trace("input.GetConnectedControllers");
    unsafe { pad::connected(handles) }
}
unsafe extern "C" fn f7(_this: *mut c_void) {
    trace("input.EnableDeviceCallbacks");
    pad::announce();
}
unsafe extern "C" fn f37(_this: *mut c_void, handle: u64) -> i32 {
    trace("input.GetInputTypeForHandle");
    pad::type_for(handle)
}
unsafe extern "C" fn f38(_this: *mut c_void, index: i32) -> u64 {
    trace("input.GetControllerForGamepadIndex");
    pad::handle_for_index(index)
}
unsafe extern "C" fn f39(_this: *mut c_void, handle: u64) -> i32 {
    trace("input.GetGamepadIndexForController");
    pad::index_for_handle(handle)
}
unsafe extern "C" fn f21(_this: *mut c_void, _handle: u64, _action: u64) -> pad::AnalogActionData {
    trace("input.GetAnalogActionData (inactive)");
    pad::analog_action_data()
}
unsafe extern "C" fn f29(_this: *mut c_void, _handle: u64) -> pad::MotionData {
    trace("input.GetMotionData (no motion data)");
    pad::motion_data()
}
pub(super) static TABLE: VTable<48> = VTable([
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
    f45 as *const (),
    f46 as *const (),
    f47 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// SteamInput005 (SDK 152 header layout): slots matched to the base table by method name; the rest are unavailable.
static TABLE_005: VTable<47> = VTable([
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
    TABLE.0[45],
    TABLE.0[46],
]);
pub(super) static OBJECT_005: Object = Object {
    table: TABLE_005.0.as_ptr(),
};
