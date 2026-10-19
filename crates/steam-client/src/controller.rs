//! SteamController008: Init/Shutdown succeed, handles are 0 and strings are empty. The
//! only device ever reported is the virtual gamepad (see `pad`); action and motion data
//! are always inactive.
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal, $ret:expr;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("controller.", $label, " (no controllers)"));
            $ret
        })*
    };
}
stubs! {
    f0 => "Init", 1;
    f1 => "Shutdown", 1;
    f2 => "RunFrame", 0;
    f4 => "GetActionSetHandle", 0;
    f5 => "ActivateActionSet", 0;
    f6 => "GetCurrentActionSet", 0;
    f7 => "ActivateActionSetLayer", 0;
    f8 => "DeactivateActionSetLayer", 0;
    f9 => "DeactivateAllActionSetLayers", 0;
    f10 => "GetActiveActionSetLayers", 0;
    f11 => "GetDigitalActionHandle", 0;
    f12 => "GetDigitalActionData", 0;
    f13 => "GetDigitalActionOrigins", 0;
    f14 => "GetAnalogActionHandle", 0;
    f16 => "GetAnalogActionOrigins", 0;
    f17 => "GetGlyphForActionOrigin", c"".as_ptr() as usize;
    f18 => "GetStringForActionOrigin", c"".as_ptr() as usize;
    f19 => "StopAnalogActionMomentum", 0;
    f21 => "TriggerHapticPulse", 0;
    f22 => "TriggerRepeatedHapticPulse", 0;
    f23 => "TriggerVibration", 0;
    f24 => "SetLEDColor", 0;
    f25 => "ShowBindingPanel", 0;
    f29 => "GetStringForXboxOrigin", c"".as_ptr() as usize;
    f30 => "GetGlyphForXboxOrigin", c"".as_ptr() as usize;
    f31 => "GetActionOriginFromXboxOrigin", 0;
    f32 => "TranslateActionOrigin", 0;
    f33 => "GetControllerBindingRevision", 0;
}
// With a virtual gamepad the one pad is reported (slot 0); action data stays unavailable.
unsafe extern "C" fn f3(_this: *mut c_void, handles: *mut u64) -> i32 {
    trace("controller.GetConnectedControllers");
    unsafe { pad::connected(handles) }
}
unsafe extern "C" fn f26(_this: *mut c_void, handle: u64) -> i32 {
    trace("controller.GetInputTypeForHandle");
    pad::type_for(handle)
}
unsafe extern "C" fn f27(_this: *mut c_void, index: i32) -> u64 {
    trace("controller.GetControllerForGamepadIndex");
    pad::handle_for_index(index)
}
unsafe extern "C" fn f28(_this: *mut c_void, handle: u64) -> i32 {
    trace("controller.GetGamepadIndexForController");
    pad::index_for_handle(handle)
}
unsafe extern "C" fn f15(_this: *mut c_void, _handle: u64, _action: u64) -> pad::AnalogActionData {
    trace("controller.GetAnalogActionData (inactive)");
    pad::analog_action_data()
}
unsafe extern "C" fn f20(_this: *mut c_void, _handle: u64) -> pad::MotionData {
    trace("controller.GetMotionData (no motion data)");
    pad::motion_data()
}
pub(super) static TABLE: VTable<34> = VTable([
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
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
