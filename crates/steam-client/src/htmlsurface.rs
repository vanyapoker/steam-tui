//! STEAMHTMLSURFACE_INTERFACE_VERSION_005: no embedded browser (Init reports false).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("htmlsurface.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "~ISteamHTMLSurface";
    f1 => "Init";
    f2 => "Shutdown";
    f3 => "CreateBrowser";
    f4 => "RemoveBrowser";
    f5 => "LoadURL";
    f6 => "SetSize";
    f7 => "StopLoad";
    f8 => "Reload";
    f9 => "GoBack";
    f10 => "GoForward";
    f11 => "AddHeader";
    f12 => "ExecuteJavascript";
    f13 => "MouseUp";
    f14 => "MouseDown";
    f15 => "MouseDoubleClick";
    f16 => "MouseMove";
    f17 => "MouseWheel";
    f18 => "KeyDown";
    f19 => "KeyUp";
    f20 => "KeyChar";
    f21 => "SetHorizontalScroll";
    f22 => "SetVerticalScroll";
    f23 => "SetKeyFocus";
    f24 => "ViewSource";
    f25 => "CopyToClipboard";
    f26 => "PasteFromClipboard";
    f27 => "Find";
    f28 => "StopFind";
    f29 => "GetLinkAtPosition";
    f30 => "SetCookie";
    f31 => "SetPageScaleFactor";
    f32 => "SetBackgroundMode";
    f33 => "SetDPIScalingFactor";
    f34 => "OpenDeveloperTools";
    f35 => "AllowStartRequest";
    f36 => "JSDialogResponse";
    f37 => "FileLoadDialogResponse";
}
// Virtual destructor occupies two slots (complete + deleting) on the Itanium ABI.
static TABLE: VTable<39> = VTable([
    f0 as *const (),
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
    f34 as *const (),
    f35 as *const (),
    f36 as *const (),
    f37 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
