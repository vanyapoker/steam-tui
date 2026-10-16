//! STEAMMUSIC_INTERFACE_VERSION001: no music player (not enabled, not playing).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("music.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "BIsEnabled";
    f1 => "BIsPlaying";
    f2 => "GetPlaybackStatus";
    f3 => "Play";
    f4 => "Pause";
    f5 => "PlayPrevious";
    f6 => "PlayNext";
    f7 => "SetVolume";
}
unsafe extern "C" fn f8() -> f32 {
    trace("music.GetVolume (unavailable)");
    0.0
}
static TABLE: VTable<9> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
    f6 as *const (),
    f7 as *const (),
    f8 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
