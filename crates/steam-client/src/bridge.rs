use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use steam_ipc::{Operation, Reply, Request, Response, VERSION};

/// The path is a capability passed explicitly by the experimental launcher.
/// Refresh tokens, passwords and guard data never enter the game process.
pub(super) fn request(app_id: u32, operation: Operation) -> io::Result<Response> {
    let name = operation_name(&operation);
    let started = Instant::now();
    let result = request_inner(app_id, operation);
    record(name, started.elapsed());
    result
}

fn operation_name(operation: &Operation) -> String {
    let text = format!("{operation:?}");
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Backend round trips block the calling (often game) thread. Every ten seconds
/// the time spent is summarized in the game log when it is large enough to be
/// felt, so a stalling call can be named instead of guessed.
fn record(name: String, elapsed: Duration) {
    use std::collections::BTreeMap;
    type Window = (Instant, BTreeMap<String, (u32, Duration)>);
    static WINDOW: std::sync::Mutex<Option<Window>> = std::sync::Mutex::new(None);
    let mut guard = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    let (since, totals) = guard.get_or_insert_with(|| (Instant::now(), BTreeMap::new()));
    let entry = totals.entry(name).or_default();
    entry.0 += 1;
    entry.1 += elapsed;
    if since.elapsed() < Duration::from_secs(10) {
        return;
    }
    let total: Duration = totals.values().map(|(_, d)| *d).sum();
    if total >= Duration::from_millis(50) {
        let mut rows: Vec<_> = totals.iter().collect();
        rows.sort_by_key(|(_, (_, d))| std::cmp::Reverse(*d));
        let detail: Vec<String> = rows
            .iter()
            .take(4)
            .map(|(n, (count, d))| format!("{n} {count}x {}ms", d.as_millis()))
            .collect();
        crate::trace(format_args!(
            "backend calls blocked the caller {}ms in 10s: {}",
            total.as_millis(),
            detail.join(", ")
        ));
    }
    *guard = None;
}

fn request_inner(app_id: u32, operation: Operation) -> io::Result<Response> {
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

pub(super) fn subscribed_app(running_app: u32, target_app: u32) -> bool {
    if running_app == 0 || target_app == 0 {
        return false;
    }
    matches!(
        request(running_app, Operation::AppOwnership { app_id: target_app }),
        Ok(Response::Ownership { owned: true })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    #[test]
    fn dlc_ownership_uses_game_scope_and_fails_closed() {
        const CHILD: &str = "STEAM_TUI_TEST_DLC_OWNERSHIP";
        if std::env::var_os(CHILD).is_some() {
            // Exercise the actual SteamApps vtable entry in an isolated process;
            // no mutation of the test runner's environment or global app ID.
            let check: unsafe extern "C" fn(*mut std::ffi::c_void, u32) -> bool =
                unsafe { std::mem::transmute(crate::apps::TABLE.0[6]) };
            for expected in [true, false, false, false, false, false] {
                assert_eq!(unsafe { check(std::ptr::null_mut(), 481) }, expected);
            }
            assert!(!unsafe { check(std::ptr::null_mut(), 0) });
            assert!(!subscribed_app(0, 481));
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "steam-tui-dlc-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let socket = directory.join("backend.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bridge::tests::dlc_ownership_uses_game_scope_and_fails_closed",
            ])
            .env(CHILD, "1")
            .env("SteamAppId", "480")
            .env("STEAM_TUI_BACKEND_SOCKET", socket)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        // Reap even if a framing/scope assertion fails in the parent.
        struct Reap<'a>(&'a mut std::process::Child);
        impl Drop for Reap<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Reap(&mut child);
        for case in 0..6 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "child did not request ownership");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: Request = steam_ipc::read_frame(&mut stream).unwrap();
            assert_eq!(request.version, VERSION);
            assert_eq!(request.app_id, 480);
            assert!(matches!(
                request.operation,
                Operation::AppOwnership { app_id: 481 }
            ));
            let response = match case {
                1 => Response::Ownership { owned: false },
                2 => Response::Error {
                    message: "CM unavailable".into(),
                },
                5 => Response::Identity {
                    steam_id: 1,
                    logged_on: true,
                },
                _ => Response::Ownership { owned: true },
            };
            steam_ipc::write_frame(
                &mut stream,
                &Reply {
                    version: if case == 4 { VERSION + 1 } else { VERSION },
                    app_id: if case == 3 { 481 } else { 480 },
                    response,
                },
            )
            .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
