//! Steam API call results: asynchronous calls return a handle, the result is
//! announced with SteamAPICallCompleted_t (703) and fetched with
//! `Steam_GetAPICallResult` / `ISteamUtils::GetAPICallResult`.
use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

const API_CALL_COMPLETED: i32 = 703;

static CALLS: Mutex<Option<HashMap<u64, (i32, Vec<u8>)>>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(0x4000_0000_0000_0001);

/// Stores `payload` as the result of a new call and queues the completion
/// callback; returns the SteamAPICall_t the game is waiting on.
pub(super) fn complete(callback: i32, payload: Vec<u8>) -> u64 {
    let call = reserve();
    complete_for(call, callback, payload);
    call
}

pub(super) fn reserve() -> u64 {
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn complete_for(call: u64, callback: i32, payload: Vec<u8>) {
    let size = payload.len() as u32;
    {
        let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        let map = calls.get_or_insert_with(HashMap::new);
        if map.len() >= 256 {
            // Games that never fetch results must not grow this without bound.
            if let Some(oldest) = map.keys().min().copied() {
                map.remove(&oldest);
            }
        }
        map.insert(call, (callback, payload));
    }
    // SteamAPICallCompleted_t { u64 call; i32 callback; u32 size } (pack 4).
    let mut data = call.to_le_bytes().to_vec();
    data.extend(callback.to_le_bytes());
    data.extend(size.to_le_bytes());
    auth::push_callback(API_CALL_COMPLETED, data);
}

pub(super) fn is_completed(call: u64) -> bool {
    CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|m| m.contains_key(&call))
}

/// Copies and consumes a matching result. Unknown/pending calls and invalid
/// arguments return false and leave any existing result available for retry.
pub(super) fn take(call: u64, expected: i32, buffer: *mut u8, size: i32) -> bool {
    let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = calls.as_mut() else {
        return false;
    };
    let Some((callback, payload)) = map.get(&call) else {
        return false;
    };
    if size < 0 || *callback != expected || (size as usize) < payload.len() || buffer.is_null() {
        return false;
    }
    // SAFETY: the caller supplied `size` writable bytes at `buffer`.
    unsafe { ptr::copy_nonoverlapping(payload.as_ptr(), buffer, payload.len()) };
    map.remove(&call);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_invalid_and_consumed_results_return_false_without_consuming_valid_results() {
        let call = reserve();
        let mut output = [0u8; 4];
        assert!(!take(call, 1331, output.as_mut_ptr(), 4));
        complete_for(call, 1331, 1i32.to_le_bytes().to_vec());
        assert!(!take(call, 1331, output.as_mut_ptr(), -1));
        assert!(!take(call, 1332, output.as_mut_ptr(), 4));
        assert!(!take(call, 1331, output.as_mut_ptr(), 3));
        assert!(!take(call, 1331, ptr::null_mut(), 4));
        assert!(is_completed(call));
        assert!(take(call, 1331, output.as_mut_ptr(), 4));
        assert_eq!(output, 1i32.to_le_bytes());
        assert!(!take(call, 1331, output.as_mut_ptr(), 4));
    }
}
