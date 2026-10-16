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
