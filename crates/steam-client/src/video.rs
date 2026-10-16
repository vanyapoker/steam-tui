//! STEAMVIDEO_INTERFACE_V007: no video/broadcast support.
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("video.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "GetVideoURL";
    f1 => "IsBroadcasting";
    f2 => "GetOPFSettings";
    f3 => "GetOPFStringForApp";
}
static TABLE: VTable<4> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
