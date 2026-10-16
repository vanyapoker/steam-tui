use super::*;
use std::collections::VecDeque;
use steam_ipc::{Operation, Response};

/// One queued callback: its Steamworks callback id and raw payload bytes.
struct Callback {
    id: i32,
    data: Vec<u8>,
}
fn auth_result(handle: u32, result: i32) -> Callback {
    // GetAuthSessionTicketResponse_t: u32 handle, i32 result.
    let mut data = handle.to_le_bytes().to_vec();
    data.extend(result.to_le_bytes());
    Callback { id: 163, data }
}
#[repr(C, packed(4))]
struct CallbackMessage {
    user: i32,
    callback: i32,
    data: *mut u8,
    size: i32,
}
#[derive(Default)]
struct Queue {
    pending: VecDeque<Callback>,
    current: Option<Callback>,
    announced: bool,
}
#[derive(Default)]
struct State {
    active: BTreeMap<u32, i32>,
    queues: BTreeMap<i32, Queue>,
}
fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: Mutex<State> = Mutex::new(State {
        active: BTreeMap::new(),
        queues: BTreeMap::new(),
    });
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Queues a callback for every attached pipe (the game dispatches it on its
/// next `SteamAPI_RunCallbacks`).
pub(super) fn push_callback(id: i32, data: Vec<u8>) {
    let attached: Vec<i32> = pipes()
        .users
        .iter()
        .filter(|(_, attached)| **attached)
        .map(|(pipe, _)| *pipe)
        .collect();
    let mut state = state();
    for pipe in attached {
        state
            .queues
            .entry(pipe)
            .or_default()
            .pending
            .push_back(Callback {
                id,
                data: data.clone(),
            });
    }
}

/// Asynchronous callbacks the account backend queued (lobby updates, chat, ...).
/// A background thread polls every 50 ms: the backend serves requests one at a
/// time, so a slow operation must never stall the game thread that pumps callbacks.
fn backend_callbacks() -> Vec<Callback> {
    use steam_ipc::{Operation, Response};
    static QUEUE: Mutex<Vec<Callback>> = Mutex::new(Vec::new());
    static START: std::sync::Once = std::sync::Once::new();
    if std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_none() {
        return Vec::new();
    }
    START.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("steam-tui-callbacks".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if let Ok(Response::Callbacks(callbacks)) =
                        bridge::request(current_app_id(), Operation::PollCallbacks)
                    {
                        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                        queue.extend(callbacks.into_iter().filter_map(|(id, hex)| {
                            Some(Callback {
                                id,
                                data: steam_ipc::hex_decode(&hex)?,
                            })
                        }));
                    }
                }
            });
        if spawned.is_err() {
            trace("callback poller thread could not start");
        }
    });
    std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

pub(super) unsafe extern "C" fn ticket(
    _this: *mut c_void,
    buffer: *mut c_void,
    capacity: i32,
    size: *mut u32,
) -> u32 {
    if size.is_null() {
        return 0;
    }
    unsafe {
        size.write(0);
    }
    if buffer.is_null() || capacity <= 0 {
        return 0;
    }
    // The bootstrap currently shares one user object. Refuse ambiguous multi-
    // pipe ticket requests until per-user objects carry an explicit pipe.
    let pipe = {
        let pipes = pipes();
        let attached: Vec<_> = pipes
            .users
            .iter()
            .filter(|(_, attached)| **attached)
            .map(|(id, _)| *id)
            .collect();
        if attached.len() != 1 {
            trace("Auth tickets require exactly one attached user pipe");
            return 0;
        }
        attached[0]
    };
    let response = bridge::request(
        current_app_id(),
        Operation::AuthSessionTicket {
            capacity: (capacity as u32).min(16 * 1024),
        },
    );
    let Ok(Response::AuthTicket { handle, ticket }) = response else {
        trace("GetAuthSessionTicket: backend did not issue an acknowledged ticket");
        return 0;
    };
    if handle == 0
        || ticket.is_empty()
        || ticket.len() > capacity as usize
        || ticket.len() > 16 * 1024
    {
        trace("GetAuthSessionTicket: invalid backend ticket response");
        return 0;
    }
    let mut state = state();
    if pipes().users.get(&pipe) != Some(&true)
        || state.active.len() >= 64
        || state.active.contains_key(&handle)
    {
        drop(state);
        let _ = bridge::request(current_app_id(), Operation::CancelAuthTicket { handle });
        return 0;
    }
    unsafe {
        ptr::copy_nonoverlapping(ticket.as_ptr(), buffer.cast(), ticket.len());
        size.write(ticket.len() as u32);
    }
    state.active.insert(handle, pipe);
    state
        .queues
        .entry(pipe)
        .or_default()
        .pending
        .push_back(auth_result(handle, 1));
    trace("GetAuthSessionTicket: CM-acknowledged ticket issued (bytes omitted)");
    handle
}

/// SteamUser023 web authentication is completed by callback 168, never 163.
pub(super) unsafe extern "C" fn web_ticket(_this: *mut c_void, identity: *const c_char) -> u32 {
    // Valve's web entry wraps service strings of at most 31 bytes + NUL.
    let mut service = Vec::new();
    if !identity.is_null() {
        for offset in 0..=steam_ipc::WEB_IDENTITY_MAX {
            let byte = unsafe { identity.add(offset).read() } as u8;
            if byte == 0 {
                break;
            }
            service.push(byte);
        }
        if service.len() > steam_ipc::WEB_IDENTITY_MAX {
            return 0;
        }
    }
    let pipe = {
        let pipes = pipes();
        let mut attached = pipes.users.iter().filter(|(_, yes)| **yes);
        let Some((&pipe, _)) = attached.next() else {
            return 0;
        };
        if attached.next().is_some() {
            return 0;
        }
        pipe
    };
    let mut state = state();
    if state.active.len() >= 64 {
        return 0;
    }
    let Ok(Response::AuthTicketPending { handle }) = bridge::request(
        current_app_id(),
        Operation::WebAuthTicket { identity: service },
    ) else {
        trace("GetAuthTicketForWebApi: backend could not accept request");
        return 0;
    };
    if handle == 0 || state.active.contains_key(&handle) {
        return 0;
    }
    if pipes().users.get(&pipe) != Some(&true) {
        drop(state);
        let _ = bridge::request(current_app_id(), Operation::CancelAuthTicket { handle });
        return 0;
    }
    state.active.insert(handle, pipe);
    trace("GetAuthTicketForWebApi: requested CM web ticket (identity and bytes omitted)");
    handle
}

pub(super) unsafe extern "C" fn cancel(_this: *mut c_void, handle: u32) {
    let mut state = state();
    if let Some(pipe) = state.active.remove(&handle) {
        if let Some(queue) = state.queues.get_mut(&pipe) {
            queue.pending.retain(|cb| {
                !matches!(cb.id, 163 | 168) || cb.data.get(..4) != Some(&handle.to_le_bytes()[..])
            });
        }
        drop(state);
        if !matches!(
            bridge::request(current_app_id(), Operation::CancelAuthTicket { handle }),
            Ok(Response::Cancelled)
        ) {
            trace("CancelAuthTicket: backend could not confirm cancellation");
        } else {
            trace("CancelAuthTicket: backend accepted cancellation");
        }
    }
}

pub(super) fn release_pipe(pipe: i32) {
    let handles: Vec<_> = state()
        .active
        .iter()
        .filter(|(_, owner)| **owner == pipe)
        .map(|(handle, _)| *handle)
        .collect();
    for handle in handles {
        unsafe { cancel(ptr::null_mut(), handle) };
    }
    state().queues.remove(&pipe);
}

pub(super) unsafe fn next(pipe: i32, message: *mut c_void, call: *mut i32) -> bool {
    if message.is_null() {
        return false;
    }
    // Before taking the state lock: this may do a backend round trip.
    sockets::steam_tui_networking_pump();
    let gc_size = crate::gc::announce();
    let backend = backend_callbacks();
    let mut state = state();
    if !pipes().users.contains_key(&pipe) {
        return false;
    }
    for cb in backend {
        let target = if cb.id == 168 {
            if cb.data.len() != 12 + steam_ipc::WEB_TICKET_MAX {
                continue;
            }
            let handle = u32::from_le_bytes(cb.data[..4].try_into().unwrap());
            let result = i32::from_le_bytes(cb.data[4..8].try_into().unwrap());
            let length = i32::from_le_bytes(cb.data[8..12].try_into().unwrap());
            if !(0..=steam_ipc::WEB_TICKET_MAX as i32).contains(&length)
                || (result == 1) == (length == 0)
            {
                continue;
            }
            // Route web completions to their owner even if a different pipe
            // happened to poll the backend first. Canceled handles are absent.
            let Some(owner) = state.active.get(&handle) else {
                continue;
            };
            *owner
        } else {
            pipe
        };
        state
            .queues
            .entry(target)
            .or_default()
            .pending
            .push_back(cb);
    }
    let queue = state.queues.entry(pipe).or_default();
    if queue.current.is_some() {
        return false;
    }
    if !queue.announced {
        // Games wait for SteamServersConnected_t (101, empty struct) before
        // talking to online services such as the Game Coordinator; only claim it
        // when the backend really reports a live CM session.
        if bridge::identity(current_app_id()).1 {
            queue.announced = true;
            queue.pending.push_front(Callback {
                id: 101,
                data: vec![0],
            });
            trace("callback SteamServersConnected_t queued (CM session is live)");
        }
    }
    if let Some(size) = gc_size {
        crate::gc::mark_announced();
        // GCMessageAvailable_t { uint32 m_nMessageSize }
        queue.pending.push_back(Callback {
            id: 1701,
            data: size.to_le_bytes().to_vec(),
        });
    }
    let Some(result) = queue.pending.pop_front() else {
        return false;
    };
    queue.current = Some(result);
    let result = queue.current.as_mut().expect("just inserted");
    let callback = CallbackMessage {
        user: 1,
        callback: result.id,
        data: result.data.as_mut_ptr(),
        size: result.data.len() as i32,
    };
    unsafe {
        message.cast::<CallbackMessage>().write_unaligned(callback);
    }
    if !call.is_null() {
        unsafe { call.write(0) };
    }
    true
}

pub(super) fn free(pipe: i32) {
    if let Some(queue) = state().queues.get_mut(&pipe) {
        queue.current = None;
    }
}
