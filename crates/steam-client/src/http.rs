//! STEAMHTTP_INTERFACE_VERSION003 without an HTTP client: requests cannot be
//! created (invalid handle 0) and every other call reports failure (false).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("http.", $label, " (no HTTP support)"));
            0
        })*
    };
}
stubs! {
    f0 => "CreateHTTPRequest";
    f1 => "SetHTTPRequestContextValue";
    f2 => "SetHTTPRequestNetworkActivityTimeout";
    f3 => "SetHTTPRequestHeaderValue";
    f4 => "SetHTTPRequestGetOrPostParameter";
    f5 => "SendHTTPRequest";
    f6 => "SendHTTPRequestAndStreamResponse";
    f7 => "DeferHTTPRequest";
    f8 => "PrioritizeHTTPRequest";
    f9 => "GetHTTPResponseHeaderSize";
    f10 => "GetHTTPResponseHeaderValue";
    f11 => "GetHTTPResponseBodySize";
    f12 => "GetHTTPResponseBodyData";
    f13 => "GetHTTPStreamingResponseBodyData";
    f14 => "ReleaseHTTPRequest";
    f15 => "GetHTTPDownloadProgressPct";
    f16 => "SetHTTPRequestRawPostBody";
    f17 => "CreateCookieContainer";
    f18 => "ReleaseCookieContainer";
    f19 => "SetCookie";
    f20 => "SetHTTPRequestCookieContainer";
    f21 => "SetHTTPRequestUserAgentInfo";
    f22 => "SetHTTPRequestRequiresVerifiedCertificate";
    f23 => "SetHTTPRequestAbsoluteTimeoutMS";
    f24 => "GetHTTPRequestWasTimedOut";
}
pub(super) static TABLE: VTable<25> = VTable([
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
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
