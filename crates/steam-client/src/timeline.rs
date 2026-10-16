//! SteamTimeline V001 with recording disabled: no recorder is installed.
use super::*;

unsafe extern "C" fn description(_this: *mut c_void, _text: *const c_char, _offset: f32) {
    trace("timeline.SetTimelineStateDescription (recording disabled)");
}
unsafe extern "C" fn clear(_this: *mut c_void, _offset: f32) {
    trace("timeline.ClearTimelineStateDescription (recording disabled)");
}
unsafe extern "C" fn event(
    _this: *mut c_void,
    _icon: *const c_char,
    _title: *const c_char,
    _text: *const c_char,
    _priority: u32,
    _offset: f32,
    _duration: f32,
    _clip: i32,
) {
    trace("timeline.AddTimelineEvent (recording disabled)");
}
unsafe extern "C" fn mode(_this: *mut c_void, _mode: i32) {
    trace("timeline.SetTimelineGameMode (recording disabled)");
}
static TABLE: VTable<4> = VTable([
    description as *const (),
    clear as *const (),
    event as *const (),
    mode as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
