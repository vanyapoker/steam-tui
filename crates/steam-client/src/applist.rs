//! STEAMAPPLIST_INTERFACE_VERSION001: no installed-app list is exposed (all calls return 0/false).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("applist.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "GetNumInstalledApps";
    f1 => "GetInstalledApps";
    f2 => "GetAppName";
    f3 => "GetAppInstallDir";
    f4 => "GetAppBuildId";
}
static TABLE: VTable<5> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
