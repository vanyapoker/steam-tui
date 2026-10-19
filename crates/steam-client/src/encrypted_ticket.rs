//! `RequestEncryptedAppTicket` / `GetEncryptedAppTicket`: the CM encrypts the
//! app ownership ticket (plus up to 128 bytes from the game) for the game's own
//! server. The request is asynchronous; the game waits for its call result.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use steam_ipc::{ENCRYPTED_TICKET_MAX, ENCRYPTED_TICKET_USER_DATA_MAX, Operation, Response};

/// EncryptedAppTicketResponse_t { EResult m_eResult }.
const RESPONSE: i32 = 154;
const E_FAIL: i32 = 2;
const E_BUSY: i32 = 10;
const E_INVALID_PARAM: i32 = 8;

static TICKET: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static PENDING: AtomicBool = AtomicBool::new(false);

fn finish(call: u64, result: i32) {
    callresults::complete_for(call, RESPONSE, result.to_le_bytes().to_vec());
}

pub(super) unsafe extern "C" fn request(_this: *mut c_void, data: *const u8, length: i32) -> u64 {
    let call = callresults::reserve();
    if !(0..=ENCRYPTED_TICKET_USER_DATA_MAX as i32).contains(&length)
        || (length > 0 && data.is_null())
    {
        trace("user.RequestEncryptedAppTicket = invalid user data");
        finish(call, E_INVALID_PARAM);
        return call;
    }
    let user_data = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: the caller supplied `length` readable bytes at `data`.
        unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec()
    };
    if PENDING.swap(true, Ordering::AcqRel) {
        trace("user.RequestEncryptedAppTicket = busy (a request is in flight)");
        finish(call, E_BUSY);
        return call;
    }
    trace(format_args!(
        "user.RequestEncryptedAppTicket ({length} bytes of user data)"
    ));
    // The CM answers within seconds; the game thread must not wait for it.
    let spawned = std::thread::Builder::new()
        .name("steam-tui-ticket".into())
        .spawn(move || {
            let result = match bridge::request(
                current_app_id(),
                Operation::EncryptedAppTicket { user_data },
            ) {
                Ok(Response::EncryptedAppTicket { result, ticket }) => {
                    if result == 1 && !ticket.is_empty() && ticket.len() <= ENCRYPTED_TICKET_MAX {
                        *TICKET.lock().unwrap_or_else(|e| e.into_inner()) = Some(ticket);
                        1
                    } else if result == 1 {
                        E_FAIL
                    } else {
                        result
                    }
                }
                _ => E_FAIL,
            };
            trace(format_args!(
                "user.RequestEncryptedAppTicket completed: EResult {result}"
            ));
            PENDING.store(false, Ordering::Release);
            finish(call, result);
        });
    if spawned.is_err() {
        PENDING.store(false, Ordering::Release);
        finish(call, E_FAIL);
    }
    call
}

pub(super) unsafe extern "C" fn get(
    _this: *mut c_void,
    buffer: *mut u8,
    capacity: i32,
    size: *mut u32,
) -> bool {
    if size.is_null() || (capacity > 0 && buffer.is_null()) || capacity < 0 {
        trace("user.GetEncryptedAppTicket = false (invalid parameter)");
        return false;
    }
    let ticket = TICKET.lock().unwrap_or_else(|e| e.into_inner());
    let Some(ticket) = ticket.as_ref() else {
        trace("user.GetEncryptedAppTicket = false (no ticket available)");
        // SAFETY: caller-supplied out-pointer.
        unsafe { size.write(0) };
        return false;
    };
    // SAFETY: caller-supplied out-pointer.
    unsafe { size.write(ticket.len() as u32) };
    if ticket.len() > capacity as usize {
        trace("user.GetEncryptedAppTicket = false (insufficient buffer)");
        return false;
    }
    // SAFETY: the caller supplied `capacity` writable bytes at `buffer`.
    unsafe { ptr::copy_nonoverlapping(ticket.as_ptr(), buffer, ticket.len()) };
    trace(format_args!(
        "user.GetEncryptedAppTicket = {} bytes (bytes omitted)",
        ticket.len()
    ));
    true
}
