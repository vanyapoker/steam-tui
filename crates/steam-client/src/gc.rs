//! SteamGameCoordinator001 (private interface; slot order SendMessage,
//! IsMessageAvailable, RetrieveMessage, EGCResults values). Messages are relayed
//! unchanged through the account backend; without a backend every send fails
//! with NotLoggedOn and nothing is ever available.
use super::*;
use std::sync::Mutex;
use steam_ipc::{Operation, Response};

const OK: u32 = 0;
const NO_MESSAGE: u32 = 1;
const BUFFER_TOO_SMALL: u32 = 2;
const NOT_LOGGED_ON: u32 = 3;
const INVALID_MESSAGE: u32 = 4;
/// Matches the backend IPC bound (hex doubles the size).
const MAX_PAYLOAD: u32 = 3 * 1024 * 1024;

/// A message fetched from the backend by IsMessageAvailable but not yet retrieved.
static PENDING: Mutex<Option<(u32, Vec<u8>)>> = Mutex::new(None);

/// True once the pending message was announced with GCMessageAvailable_t.
static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called from the callback pump: pulls the next backend message (at most every
/// 50 ms) and returns its size once, so a GCMessageAvailable_t (id 1701) can be
/// queued. Games poll IsMessageAvailable only in response to that callback.
pub(super) fn announce() -> Option<u32> {
    use std::sync::atomic::Ordering;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(50)) {
            return None;
        }
        *last = Some(std::time::Instant::now());
    }
    if std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_none() {
        return None;
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    fetch(&mut pending);
    let size = pending.as_ref()?.1.len() as u32;
    (!ANNOUNCED.load(Ordering::SeqCst)).then_some(size)
}

/// The callback for the pending message is queued; do not announce it again.
pub(super) fn mark_announced() {
    ANNOUNCED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn fetch(pending: &mut Option<(u32, Vec<u8>)>) {
    if pending.is_some() {
        return;
    }
    if let Ok(Response::GcMessage { msg_type, payload }) =
        bridge::request(current_app_id(), Operation::GcRecv)
        && let Some(bytes) = steam_ipc::hex_decode(&payload)
    {
        trace(format_args!(
            "gc.recv type={:#x} ({}) len={}",
            msg_type,
            msg_type & 0x7fff_ffff,
            bytes.len()
        ));
        *pending = Some((msg_type, bytes));
    }
}

unsafe extern "C" fn send(_this: *mut c_void, msg_type: u32, data: *const u8, size: u32) -> u32 {
    if size > MAX_PAYLOAD || (data.is_null() && size != 0) {
        return INVALID_MESSAGE;
    }
    // SAFETY: the caller supplies `size` readable bytes at `data`.
    let payload = if size == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, size as usize) }
    };
    trace(format_args!(
        "gc.send type={:#x} ({}) len={size}",
        msg_type,
        msg_type & 0x7fff_ffff
    ));
    match bridge::request(
        current_app_id(),
        Operation::GcSend {
            msg_type,
            payload: steam_ipc::hex_encode(payload),
        },
    ) {
        Ok(Response::GcSent) => OK,
        _ => NOT_LOGGED_ON,
    }
}
unsafe extern "C" fn available(_this: *mut c_void, size: *mut u32) -> bool {
    static POLLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if POLLS
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(500)
    {
        trace("gc.IsMessageAvailable polled (logged every 500th call)");
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    fetch(&mut pending);
    match &*pending {
        Some((_, bytes)) => {
            if !size.is_null() {
                // SAFETY: non-null out-pointer supplied by the caller.
                unsafe { size.write(bytes.len() as u32) };
            }
            true
        }
        None => false,
    }
}
unsafe extern "C" fn retrieve(
    _this: *mut c_void,
    msg_type: *mut u32,
    dest: *mut u8,
    capacity: u32,
    size: *mut u32,
) -> u32 {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    fetch(&mut pending);
    let Some((kind, bytes)) = pending.as_ref() else {
        return NO_MESSAGE;
    };
    // SAFETY: out-pointers are caller-supplied; null is tolerated.
    unsafe {
        if !size.is_null() {
            size.write(bytes.len() as u32);
        }
        if bytes.len() as u32 > capacity || dest.is_null() && !bytes.is_empty() {
            return BUFFER_TOO_SMALL;
        }
        if !msg_type.is_null() {
            msg_type.write(*kind);
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
    }
    *pending = None;
    ANNOUNCED.store(false, std::sync::atomic::Ordering::SeqCst);
    OK
}
static TABLE: VTable<3> = VTable([
    send as *const (),
    available as *const (),
    retrieve as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
