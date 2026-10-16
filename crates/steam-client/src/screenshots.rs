//! STEAMSCREENSHOTS_INTERFACE_VERSION003 without a screenshot library: writes
//! return the invalid handle (0) and tagging reports failure.
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("screenshots.", $label, " (no screenshot library)"));
            0
        })*
    };
}
stubs! {
    f0 => "WriteScreenshot";
    f1 => "AddScreenshotToLibrary";
    f2 => "TriggerScreenshot";
    f3 => "HookScreenshots";
    f4 => "SetLocation";
    f5 => "TagUser";
    f6 => "TagPublishedFile";
    f7 => "IsScreenshotsHooked";
    f8 => "AddVRScreenshotToLibrary";
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
