//! Steam Input device presence for the virtual gamepad steam-tui keeps for the game
//! (`steam_core::gamepad`): one connected device in gamepad slot 0, no action data.
use super::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The one device handle this library hands out.
pub(super) const HANDLE: u64 = 1;
const DEVICE_CONNECTED: i32 = 2801;
const GAMEPAD_SLOT_CHANGE: i32 = 2804;

/// `ESteamInputType` of the pad behind the virtual one, `None` without a virtual pad
/// (or when presence reporting is switched off).
pub(super) fn input_type() -> Option<i32> {
    static TYPE: OnceLock<Option<i32>> = OnceLock::new();
    *TYPE.get_or_init(|| {
        let disabled = std::env::var_os("STEAM_TUI_NO_GAMEPAD_PRESENCE").is_some();
        type_for_kind(
            std::env::var("STEAM_TUI_VIRTUAL_GAMEPAD").ok().as_deref(),
            disabled,
        )
    })
}

fn type_for_kind(kind: Option<&str>, disabled: bool) -> Option<i32> {
    if disabled {
        return None;
    }
    match kind? {
        "xbox360" => Some(2),
        "xboxone" => Some(3),
        "ps4" => Some(5),
        "ps5" => Some(13),
        _ => None,
    }
}

pub(super) fn present() -> bool {
    input_type().is_some()
}

/// `GetConnectedControllers`: writes the handle and returns how many there are.
pub(super) unsafe fn connected(handles: *mut u64) -> i32 {
    if !present() || handles.is_null() {
        return 0;
    }
    // SAFETY: the caller's array holds at least STEAM_INPUT_MAX_COUNT (16) handles.
    unsafe { handles.write(HANDLE) };
    1
}

pub(super) fn type_for(handle: u64) -> i32 {
    match input_type() {
        Some(kind) if handle == HANDLE => kind,
        _ => 0,
    }
}

pub(super) fn handle_for_index(index: i32) -> u64 {
    if index == 0 && present() { HANDLE } else { 0 }
}

pub(super) fn index_for_handle(handle: u64) -> i32 {
    if handle == HANDLE && present() { 0 } else { -1 }
}

/// Queues the device-connected and gamepad-slot callbacks once, when the game
/// asks for device callbacks.
pub(super) fn announce() {
    static DONE: AtomicBool = AtomicBool::new(false);
    let Some(kind) = input_type() else {
        return;
    };
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    auth::push_callback(DEVICE_CONNECTED, HANDLE.to_le_bytes().to_vec());
    // SteamInputGamepadSlotChange_t (pack 4): AppId, handle, type, old slot, new slot.
    let mut change = current_app_id().to_le_bytes().to_vec();
    change.extend(HANDLE.to_le_bytes());
    change.extend(kind.to_le_bytes());
    change.extend((-1i32).to_le_bytes());
    change.extend(0i32.to_le_bytes());
    auth::push_callback(GAMEPAD_SLOT_CHANGE, change);
    trace("input: virtual gamepad announced (device connected, gamepad slot 0)");
}

/// `InputAnalogActionData_t` / `ControllerAnalogActionData_t`: no action is ever active.
#[repr(C, packed)]
pub(super) struct AnalogActionData {
    mode: i32,
    x: f32,
    y: f32,
    active: bool,
}

pub(super) fn analog_action_data() -> AnalogActionData {
    AnalogActionData {
        mode: 0,
        x: 0.0,
        y: 0.0,
        active: false,
    }
}

/// `InputMotionData_t` / `ControllerMotionData_t`: rotation quaternion, acceleration and
/// angular velocity, all zero (no motion sensor is forwarded).
#[repr(C)]
pub(super) struct MotionData {
    values: [f32; 10],
}

pub(super) fn motion_data() -> MotionData {
    MotionData { values: [0.0; 10] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_kinds_map_to_steam_input_types() {
        assert_eq!(type_for_kind(Some("xbox360"), false), Some(2));
        assert_eq!(type_for_kind(Some("xboxone"), false), Some(3));
        assert_eq!(type_for_kind(Some("ps4"), false), Some(5));
        assert_eq!(type_for_kind(Some("ps5"), false), Some(13));
        assert_eq!(type_for_kind(Some("switch"), false), None);
        assert_eq!(type_for_kind(None, false), None);
        assert_eq!(type_for_kind(Some("ps5"), true), None);
    }

    #[test]
    fn inactive_action_data_has_the_sdk_layout() {
        assert_eq!(std::mem::size_of::<AnalogActionData>(), 13);
        assert_eq!(std::mem::size_of::<MotionData>(), 40);
        let data = analog_action_data();
        assert!(!{ data.active });
    }
}
