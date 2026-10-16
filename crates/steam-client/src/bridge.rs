use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use steam_ipc::{Operation, Reply, Request, Response, VERSION};

/// The path is a capability passed explicitly by the experimental launcher.
/// Refresh tokens, passwords and guard data never enter the game process.
pub(super) fn request(app_id: u32, operation: Operation) -> io::Result<Response> {
    let path = std::env::var_os("STEAM_TUI_BACKEND_SOCKET")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No Steam backend socket"))?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    steam_ipc::write_frame(
        &mut stream,
        &Request {
            version: VERSION,
            app_id,
            operation,
        },
    )?;
    let reply: Reply = steam_ipc::read_frame(&mut stream)?;
    if reply.version != VERSION || reply.app_id != app_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Mismatched backend reply",
        ));
    }
    match reply.response {
        Response::Error { message } => Err(io::Error::other(message)),
        response => Ok(response),
    }
}

pub(super) fn identity(app_id: u32) -> (u64, bool) {
    // Short per-process cache bounds IPC work for games checking every frame.
    // Never retain a previous successful response after a failed refresh.
    type Cache = Option<(Instant, u32, std::ffi::OsString, (u64, bool))>;
    static CACHE: std::sync::Mutex<Cache> = std::sync::Mutex::new(None);
    let path = std::env::var_os("STEAM_TUI_BACKEND_SOCKET").unwrap_or_default();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, id, socket, identity)) = &*cache
        && *id == app_id
        && *socket == path
        && at.elapsed() < Duration::from_millis(250)
    {
        return *identity;
    }
    let identity = match request(app_id, Operation::Identity) {
        Ok(Response::Identity {
            steam_id,
            logged_on,
        }) => (steam_id, logged_on),
        _ => (0, false),
    };
    *cache = Some((Instant::now(), app_id, path, identity));
    identity
}

pub(super) fn owned(app_id: u32) -> bool {
    matches!(
        request(app_id, Operation::Ownership),
        Ok(Response::Ownership { owned: true })
    )
}
