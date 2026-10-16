//! Experimental Linux x86-64 Steamworks client bootstrap, not an online backend.
//! Only explicitly versioned interfaces are exposed. Missing methods terminate
//! with a diagnostic instead of returning fabricated auth/ownership results.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the prototype implements only the Linux x86-64 C++ ABI");

/// Slots that exist in a newer interface version but have no backing service:
/// they log and return zero/false/null, like the other unavailable-service stubs.
macro_rules! new_slots {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!($label, " (unavailable)"));
            0
        })*
    };
}

mod applist;
mod apps;
mod auth;
mod bridge;
mod callresults;
mod client;
mod compat;
mod controller;
mod crash;
mod friends;
mod gc;
mod htmlsurface;
mod images;
mod http;
mod input;
mod inventory;
mod matchmaking;
mod music;
mod musicremote;
mod networking;
mod parental;
mod parties;
mod remoteplay;
mod remotestorage;
mod screenshots;
mod sockets;
mod timeline;
mod ugc;
mod user;
mod userstats;
mod utils;
mod video;
mod workshop;

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::io::Write;
use std::ptr;
use std::sync::{Mutex, OnceLock};

// Linux's Itanium C++ ABI puts a vptr first in an object. These interfaces
// have no virtual destructor; slot zero is the first declared method.
#[repr(C)]
struct Object {
    table: *const *const (),
}
struct VTable<const N: usize>([*const (); N]);
// SAFETY: all pointers target immutable static tables or code. No object data
// is mutated through these pointers; session state is separately synchronized.
unsafe impl Sync for Object {}
unsafe impl<const N: usize> Sync for VTable<N> {}

#[derive(Default)]
struct Pipes {
    next: i32,
    users: BTreeMap<i32, bool>,
}
fn pipes() -> std::sync::MutexGuard<'static, Pipes> {
    static PIPES: Mutex<Pipes> = Mutex::new(Pipes {
        next: 0,
        users: BTreeMap::new(),
    });
    PIPES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Identical messages are logged a few times, then suppressed: per-frame calls
/// (RunFrame and friends) would otherwise bury everything else in the log.
const TRACE_REPEATS: u32 = 3;
fn trace(message: impl std::fmt::Display) {
    // Ignore a closed stderr rather than unwinding across the FFI boundary.
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    static SEEN: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());
    let secs = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f32();
    let message = message.to_string();
    let count = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        // Bounded: a message with ever-changing text must not grow this forever.
        if seen.len() > 4096 && !seen.contains_key(&message) {
            seen.clear();
        }
        let count = seen.entry(message.clone()).or_insert(0);
        *count += 1;
        *count
    };
    if count > TRACE_REPEATS {
        return;
    }
    let note = if count == TRACE_REPEATS {
        " (repeats suppressed)"
    } else {
        ""
    };
    let _ = writeln!(
        std::io::stderr().lock(),
        "[steam-tui-client +{secs:.1}s] {message}{note}"
    );
}
fn unsupported(method: &str) -> ! {
    trace(format_args!("UNIMPLEMENTED {method}"));
    // Diverging stubs never interpret arguments or return through an unknown
    // signature. This is intentionally limited to the Linux x86-64 ABI.
    std::process::abort();
}

unsafe fn version<'a>(name: *const c_char) -> Option<&'a CStr> {
    if name.is_null() {
        None
    } else {
        // SAFETY: factory/getter callers supply a live NUL-terminated string.
        Some(unsafe { CStr::from_ptr(name) })
    }
}
fn object(object: &'static Object) -> *mut c_void {
    ptr::from_ref(object).cast_mut().cast()
}

/// Steamworks interface factory. SteamClient023, SteamClient021, SteamClient020 and SteamClient017 (Proton) are supported.
///
/// # Safety
/// `name` must be null or a valid C string; `result` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn CreateInterface(name: *const c_char, result: *mut i32) -> *mut c_void {
    let name = unsafe { version(name) };
    crash::install();
    trace(format_args!("CreateInterface {name:?}"));
    let old = name.and_then(|n| compat::CLIENTS.iter().find(|(c, _)| *c == n));
    let found = old.is_some()
        || matches!(name, Some(n) if n == c"SteamClient023" || n == c"SteamClient021" || n == c"SteamClient020" || n == c"SteamClient017");
    if !result.is_null() {
        unsafe { result.write(if found { 0 } else { 1 }) };
    }
    if found {
        if let Some(&(_, table)) = old {
            object(table)
        } else if name == Some(c"SteamClient017") {
            object(&client::OBJECT017)
        } else if name == Some(c"SteamClient023") {
            object(&client::OBJECT023)
        } else if name == Some(c"SteamClient021") {
            object(&client::OBJECT021)
        } else {
            object(&client::OBJECT)
        }
    } else {
        ptr::null_mut()
    }
}

// SDK resolves these exports for callback dispatch and async-call results.
/// # Safety
/// `message` must point to writable CallbackMsg_t storage and `call` must be
/// null or writable. Returned payload lives until Steam_FreeLastCallback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Steam_BGetCallback(
    pipe: i32,
    message: *mut c_void,
    call: *mut i32,
) -> bool {
    unsafe { auth::next(pipe, message, call) }
}
#[unsafe(no_mangle)]
pub extern "C" fn Steam_FreeLastCallback(pipe: i32) {
    auth::free(pipe);
}
/// # Safety
/// `failed` must be null or point to a writable C++ bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Steam_GetAPICallResult(
    _pipe: i32,
    call: u64,
    callback: *mut c_void,
    size: i32,
    expected: i32,
    failed: *mut bool,
) -> bool {
    let found = callresults::take(call, expected, callback.cast(), size);
    if !failed.is_null() {
        // A result we do not hold (or cannot deliver) is reported as failed.
        unsafe { failed.write(!found) };
    }
    found
}
/// Proton's lsteamclient asks this before requesting an interface version.
///
/// # Safety
/// `name` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Steam_IsKnownInterface(name: *const c_char) -> bool {
    let known = unsafe { version(name) }.is_some_and(client::is_known);
    trace(format_args!(
        "Steam_IsKnownInterface({:?}) = {known}",
        unsafe { version(name) }
    ));
    known
}
/// Called when an interface version was requested but is not known.
///
/// # Safety
/// `name` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Steam_NotifyMissingInterface(_pipe: i32, name: *const c_char) {
    trace(format_args!("Steam_NotifyMissingInterface({:?})", unsafe {
        version(name)
    }));
}
#[unsafe(no_mangle)]
pub extern "C" fn Steam_ReleaseThreadLocalMemory(_thread_exit: bool) {}

unsafe extern "C" fn get_user(_this: *mut c_void) -> i32 {
    trace("user.GetHSteamUser");
    1
}
unsafe extern "C" fn logged_on(_this: *mut c_void) -> bool {
    let (_, logged_on) = bridge::identity(current_app_id());
    trace(format_args!("user.BLoggedOn = {logged_on}"));
    logged_on
}
// CSteamID is an eight-byte trivial value on this ABI, returned in RAX.
unsafe extern "C" fn steam_id(_this: *mut c_void) -> u64 {
    let (steam_id, _) = bridge::identity(current_app_id());
    trace("user.GetSteamID");
    steam_id
}
fn current_app_id() -> u32 {
    static APP: OnceLock<u32> = OnceLock::new();
    *APP.get_or_init(|| {
        std::env::var("SteamAppId")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}
unsafe extern "C" fn app_id(_this: *mut c_void) -> u32 {
    let id = current_app_id();
    trace(format_args!("utils.GetAppID = {id}"));
    id
}
