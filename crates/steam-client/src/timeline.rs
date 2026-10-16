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

// ---- generated: additional interface versions ----

// STEAMTIMELINE_INTERFACE_V004 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_004_0 => "timeline.SetTimelineTooltip";
    n_004_1 => "timeline.ClearTimelineTooltip";
    n_004_3 => "timeline.AddInstantaneousTimelineEvent";
    n_004_4 => "timeline.AddRangeTimelineEvent";
    n_004_5 => "timeline.StartRangeTimelineEvent";
    n_004_6 => "timeline.UpdateRangeTimelineEvent";
    n_004_7 => "timeline.EndRangeTimelineEvent";
    n_004_8 => "timeline.RemoveTimelineEvent";
    n_004_9 => "timeline.DoesEventRecordingExist";
    n_004_10 => "timeline.StartGamePhase";
    n_004_11 => "timeline.EndGamePhase";
    n_004_12 => "timeline.SetGamePhaseID";
    n_004_13 => "timeline.DoesGamePhaseRecordingExist";
    n_004_14 => "timeline.AddGamePhaseTag";
    n_004_15 => "timeline.SetGamePhaseAttribute";
    n_004_16 => "timeline.OpenOverlayToGamePhase";
    n_004_17 => "timeline.OpenOverlayToTimelineEvent";
}
static TABLE_004: VTable<18> = VTable([
    n_004_0 as *const (),
    n_004_1 as *const (),
    TABLE.0[3],
    n_004_3 as *const (),
    n_004_4 as *const (),
    n_004_5 as *const (),
    n_004_6 as *const (),
    n_004_7 as *const (),
    n_004_8 as *const (),
    n_004_9 as *const (),
    n_004_10 as *const (),
    n_004_11 as *const (),
    n_004_12 as *const (),
    n_004_13 as *const (),
    n_004_14 as *const (),
    n_004_15 as *const (),
    n_004_16 as *const (),
    n_004_17 as *const (),
]);
pub(super) static OBJECT_004: Object = Object {
    table: TABLE_004.0.as_ptr(),
};
