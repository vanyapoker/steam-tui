//! SteamNetworkingSockets012 backed by Valve's open source UDP/ICE transport.
//! Signaling and certificates use our account backend; hosted SDR availability remains gated.
use super::*;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use steam_ipc::{Operation, Response};
unsafe extern "C" {
    fn steam_tui_transport_init(
        id: u64,
        app: u32,
        send: unsafe extern "C" fn(u64, *const u8, i32) -> bool,
        status: unsafe extern "C" fn(i32, *const u8, i32),
        error: *mut u8,
    ) -> *mut c_void;
    fn steam_tui_transport_messages() -> *mut c_void;
    fn steam_tui_transport_public_key(key: *mut u8) -> bool;
    fn steam_tui_transport_add_ca(cert: *const c_char, error: *mut u8) -> bool;
    fn steam_tui_transport_revoke(key: u64);
    fn steam_tui_transport_set_cert(data: *const u8, size: i32, error: *mut u8) -> bool;
    fn steam_tui_transport_recv(peer: u64, data: *const u8, size: i32);
    fn steam_tui_transport_callbacks();
    fn steam_tui_transport_config(
        value: i32,
        scope: i32,
        object: isize,
        kind: i32,
        data: *const c_void,
    ) -> bool;
    fn steam_tui_transport_get_config(
        value: i32,
        scope: i32,
        object: isize,
        kind: *mut i32,
        data: *mut c_void,
        size: *mut usize,
    ) -> i32;
    fn steam_tui_transport_timestamp() -> i64;
    fn steam_tui_transport_debug(level: i32, callback: *const c_void);
    fn steam_tui_transport_allocate(size: i32) -> *mut c_void;
    fn steam_tui_transport_stun(servers: *const c_char);
    fn steam_tui_sdr_configure(revision: u32);
    fn steam_tui_sdr_add_relay(pop: u32, ipv4: u32, port: u16) -> bool;
    fn steam_tui_sdr_ping(pop: u32) -> i32;
    fn steam_tui_sdr_ping_age() -> f32;
    fn steam_tui_sdr_routed_ping(pop: u32, via: *mut u32) -> i32;
}
static OBJECT: OnceLock<usize> = OnceLock::new();
static SIGNALS: Mutex<VecDeque<(u64, Vec<u8>)>> = Mutex::new(VecDeque::new());
static SDR_POPS: OnceLock<Vec<u32>> = OnceLock::new();
unsafe extern "C" fn signal(peer: u64, data: *const u8, size: i32) -> bool {
    if data.is_null() || !(1..=65536).contains(&size) {
        return false;
    }
    let mut queue = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() >= 128 {
        return false;
    }
    // SAFETY: GNS owns this immutable payload for the duration of SendSignal.
    queue.push_back((
        peer,
        unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec(),
    ));
    true
}
unsafe extern "C" fn status(callback: i32, data: *const u8, size: i32) {
    if data.is_null() || !matches!((callback, size), (1221, 704) | (1251, 136) | (1252, 696)) {
        return;
    }
    // SAFETY: native shim asserts the SDK callback layout at build time.
    auth::push_callback(
        callback,
        unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec(),
    );
}
fn diagnostic(error: &[u8; 1024]) -> String {
    String::from_utf8_lossy(&error[..error.iter().position(|b| *b == 0).unwrap_or(error.len())])
        .into_owned()
}
pub(super) fn object() -> *mut c_void {
    *OBJECT.get_or_init(initialize) as *mut c_void
}
pub(super) fn messages_object() -> *mut c_void {
    if object().is_null() {
        return ptr::null_mut();
    }
    unsafe { steam_tui_transport_messages() }
}
fn initialize() -> usize {
    let (id, _) = bridge::identity(current_app_id());
    let mut error = [0u8; 1024];
    // SAFETY: callback functions have process lifetime; error is a 1024-byte buffer.
    let object = unsafe {
        steam_tui_transport_init(id, current_app_id(), signal, status, error.as_mut_ptr())
    };
    if object.is_null() {
        trace(format_args!(
            "network transport initialization failed: {}",
            diagnostic(&error)
        ));
        return 0;
    }
    // Configure a STUN service explicitly; do not infer Valve's STUN defaults.
    let stun = std::env::var("STEAM_TUI_STUN_SERVERS")
        .unwrap_or_else(|_| "stun.l.google.com:19302".into());
    if let Ok(stun) = std::ffi::CString::new(stun) {
        unsafe { steam_tui_transport_stun(stun.as_ptr()) };
    }
    if id != 0 {
        install_certificate(&mut error);
    }
    trace("SteamNetworkingSockets012: native UDP/ICE/SDR transport initialized");
    object as usize
}
fn install_certificate(error: &mut [u8; 1024]) {
    let mut key = [0u8; 32];
    // SAFETY: GNS writes exactly 32 public-key bytes.
    if !unsafe { steam_tui_transport_public_key(key.as_mut_ptr()) } {
        trace("networking public key unavailable");
        return;
    }
    let Ok(Response::NetworkingConfig {
        certs,
        revoked_keys,
        sdr,
    }) = bridge::request(current_app_id(), Operation::NetworkingConfig)
    else {
        trace("networking trust configuration unavailable");
        return;
    };
    if let Some(sdr) = sdr {
        unsafe { steam_tui_sdr_configure(sdr.revision) };
        for pop in &sdr.pops {
            for relay in pop.relays.iter().take(2) {
                unsafe { steam_tui_sdr_add_relay(pop.id, relay.ipv4, relay.port_start) };
            }
        }
        let _ = SDR_POPS.set(sdr.pops.iter().map(|pop| pop.id).collect());
    }
    for cert in certs {
        if let Ok(cert) = std::ffi::CString::new(cert) {
            // SAFETY: the NUL-terminated certificate and output buffer remain valid.
            if !unsafe { steam_tui_transport_add_ca(cert.as_ptr(), error.as_mut_ptr()) } {
                trace(format_args!(
                    "networking CA rejected: {}",
                    diagnostic(error)
                ));
            }
        }
    }
    for key in revoked_keys {
        unsafe { steam_tui_transport_revoke(key) };
    }
    let response = bridge::request(
        current_app_id(),
        Operation::NetworkingCertificate {
            public_key: key.to_vec(),
        },
    );
    match response {
        Ok(Response::NetworkingCertificate { signed }) if signed.len() <= 16384 => {
            // SAFETY: the shim consumes these bytes synchronously and writes at most 1024 diagnostic bytes.
            if unsafe {
                steam_tui_transport_set_cert(
                    signed.as_ptr(),
                    signed.len() as i32,
                    error.as_mut_ptr(),
                )
            } {
                trace(
                    "networking certificate verified and installed (hosted SDR availability remains gated)",
                );
            } else {
                trace(format_args!(
                    "networking certificate rejected: {}",
                    diagnostic(error)
                ));
            }
        }
        Ok(Response::Error { message }) => {
            trace(format_args!(
                "networking certificate unavailable: {message}"
            ));
        }
        _ => trace("networking certificate unavailable"),
    }
}
/// Called before taking callback/pipe locks. Never perform IPC on the GNS service thread.
#[unsafe(no_mangle)]
pub(super) extern "C" fn steam_tui_networking_pump() {
    if OBJECT.get().copied().unwrap_or(0) == 0 {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_millis(10)) {
            return;
        }
        *last = Some(Instant::now());
    }
    unsafe { steam_tui_transport_callbacks() };
    if std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_none() {
        return;
    }
    let signals: Vec<_> = {
        let mut queue = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let count = queue.len().min(8);
        queue.drain(..count).collect()
    };
    for (peer, rendezvous) in signals {
        let size = rendezvous.len();
        if matches!(
            bridge::request(current_app_id(), Operation::P2PSend { peer, rendezvous }),
            Ok(Response::P2PSent)
        ) {
            trace(format_args!("P2P signal sent ({size} bytes)"));
        } else {
            trace("P2P signal send failed");
        }
    }
    if let Ok(Response::P2PSignals(signals)) = bridge::request(current_app_id(), Operation::P2PRecv)
    {
        for (peer, data) in signals {
            if data.len() <= 65536 {
                trace(format_args!("P2P signal received ({} bytes)", data.len()));
                unsafe { steam_tui_transport_recv(peer, data.as_ptr(), data.len() as i32) };
            }
        }
    }
}
pub(super) unsafe fn config(
    value: i32,
    scope: i32,
    object: isize,
    kind: i32,
    data: *const c_void,
) -> bool {
    if self::object().is_null() {
        return false;
    }
    unsafe { steam_tui_transport_config(value, scope, object, kind, data) }
}
pub(super) unsafe fn get_config(
    value: i32,
    scope: i32,
    object: isize,
    kind: *mut i32,
    data: *mut c_void,
    size: *mut usize,
) -> i32 {
    if self::object().is_null() {
        return -1;
    }
    unsafe { steam_tui_transport_get_config(value, scope, object, kind, data, size) }
}
pub(super) fn allocate(size: i32) -> *mut c_void {
    if object().is_null() || size < 0 {
        return ptr::null_mut();
    }
    unsafe { steam_tui_transport_allocate(size) }
}

pub(super) fn timestamp() -> i64 {
    unsafe { steam_tui_transport_timestamp() }
}
pub(super) unsafe fn debug(level: i32, callback: *const c_void) {
    unsafe { steam_tui_transport_debug(level, callback) }
}

pub(super) fn relay_pings() -> Option<Vec<(u32, i32)>> {
    if object().is_null() {
        return None;
    }
    SDR_POPS.get().map(|pops| {
        pops.iter()
            .map(|id| (*id, unsafe { steam_tui_sdr_ping(*id) }))
            .collect()
    })
}
pub(super) unsafe fn routed_ping(pop: u32, via: *mut u32) -> i32 {
    if object().is_null() {
        return -1;
    }
    unsafe { steam_tui_sdr_routed_ping(pop, via) }
}

pub(super) fn ping_age() -> f32 {
    if object().is_null() {
        return -1.0;
    }
    unsafe { steam_tui_sdr_ping_age() }
}
