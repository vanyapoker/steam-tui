//! STEAMPARENTALSETTINGS_INTERFACE_VERSION001: parental lock reports disabled (false).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("parental.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "BIsParentalLockEnabled";
    f1 => "BIsParentalLockLocked";
    f2 => "BIsAppBlocked";
    f3 => "BIsAppInBlockList";
    f4 => "BIsFeatureBlocked";
    f5 => "BIsFeatureInBlockList";
}
static TABLE: VTable<6> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
