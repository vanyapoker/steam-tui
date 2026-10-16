//! Isolated official-client identity probe. Never loaded in our backend/game.
use super::*;
use std::ffi::{CString, c_void};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Status {
    pub steam_id: u64,
    pub logged_on: bool,
}

/// Invoked only in a short-lived subprocess, with an already running host.
pub(super) fn child(root: &Path) -> Result<()> {
    if steam_pids().is_empty() {
        return Err(failure("No Valve host is running"));
    }
    let library = fs::canonicalize(root.join("linux64/steamclient.so"))?;
    let path = CString::new(library.as_os_str().as_encoded_bytes())?;
    // SAFETY: the selected installed Valve library implements the public Linux
    // SteamClient020 / SteamUser021 ABI. This process never loads our shim.
    unsafe {
        let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            return Err(failure("Cannot load Valve's installed steamclient.so"));
        }
        let symbol = libc::dlsym(library, c"CreateInterface".as_ptr());
        if symbol.is_null() {
            return Err(failure("Valve client has no CreateInterface"));
        }
        let factory: unsafe extern "C" fn(*const libc::c_char, *mut i32) -> *mut c_void =
            std::mem::transmute(symbol);
        let client = factory(c"SteamClient020".as_ptr(), std::ptr::null_mut());
        if client.is_null() {
            return Err(failure("Valve SteamClient020 unavailable"));
        }
        let table = *client.cast::<*const *const c_void>();
        let create: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(*table.add(0));
        let release: unsafe extern "C" fn(*mut c_void, i32) -> bool =
            std::mem::transmute(*table.add(1));
        let connect: unsafe extern "C" fn(*mut c_void, i32) -> i32 =
            std::mem::transmute(*table.add(2));
        let release_user: unsafe extern "C" fn(*mut c_void, i32, i32) =
            std::mem::transmute(*table.add(4));
        let get_user: unsafe extern "C" fn(
            *mut c_void,
            i32,
            i32,
            *const libc::c_char,
        ) -> *mut c_void = std::mem::transmute(*table.add(5));
        let pipe = create(client);
        if pipe <= 0 {
            return Err(failure("Valve host IPC is not ready"));
        }
        let user = connect(client, pipe);
        let identity = get_user(client, user, pipe, c"SteamUser021".as_ptr());
        let result = if user > 0 && !identity.is_null() {
            let table = *identity.cast::<*const *const c_void>();
            let logged_on: unsafe extern "C" fn(*mut c_void) -> bool =
                std::mem::transmute(*table.add(1));
            let steam_id: unsafe extern "C" fn(*mut c_void) -> u64 =
                std::mem::transmute(*table.add(2));
            Status {
                logged_on: logged_on(identity),
                steam_id: steam_id(identity),
            }
        } else {
            Status {
                logged_on: false,
                steam_id: 0,
            }
        };
        if user > 0 {
            release_user(client, pipe, user);
        }
        release(client, pipe);
        println!("{}", serde_json::to_string(&result)?);
        // Valve's library stays loaded until this isolated process exits.
    }
    Ok(())
}

pub(super) fn status(root: &Path) -> Result<Status> {
    let mut command = Command::new("/proc/self/exe");
    command.args(["--internal-host-probe"]).arg(root);
    clean_environment(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(exit) = child.try_wait()? {
            if !exit.success() {
                return Err(failure("Valve identity probe failed"));
            }
            let output = child.wait_with_output()?;
            if output.stdout.len() > 8192 {
                return Err(failure("Invalid Valve identity probe output"));
            }
            // Native library diagnostics may precede our single JSON line.
            return std::str::from_utf8(&output.stdout)?
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str(line).ok())
                .ok_or_else(|| failure("Valve identity probe returned no status"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failure("Valve identity probe timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
