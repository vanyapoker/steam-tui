//! Hyprland placement for windows owned by one direct game launch. No global
//! window rules, Steam host windows, or class-name guesses are involved.
use super::*;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

const MAX_REPLY: u64 = 8 * 1024 * 1024;

fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if !runtime.is_absolute() || !safe_key(&signature) {
        return None;
    }
    let path = runtime.join("hypr").join(signature).join(".socket.sock");
    path.exists().then_some(path)
}

fn safe_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn start_time(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn request(path: &Path, command: &str) -> Result<String> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.write_all(command.as_bytes())?;
    let mut reply = String::new();
    socket.take(MAX_REPLY + 1).read_to_string(&mut reply)?;
    if reply.len() as u64 > MAX_REPLY {
        return Err(failure("Hyprland reply exceeded the size limit"));
    }
    Ok(reply)
}

#[derive(serde::Deserialize)]
struct Window {
    address: String,
    pid: i64,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
}

fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|hex| {
        !hex.is_empty() && hex.len() <= 16 && hex.bytes().all(|c| c.is_ascii_hexdigit())
    })
}

fn owns_window(window: &Window, token: &str, proc_root: &Path) -> bool {
    if !window.mapped
        || window.hidden
        || window.pid <= 0
        || window.pid > i32::MAX as i64
        || !valid_address(&window.address)
    {
        return false;
    }
    let process = proc_root.join(window.pid.to_string());
    if !fs::metadata(&process).is_ok_and(|m| m.uid() == unsafe { libc::geteuid() }) {
        return false;
    }
    let name = fs::read_to_string(process.join("comm"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Launchers/crash reporters can carry the launch environment but are not games.
    if [
        "steam",
        "steam.exe",
        "steamwebhelper",
        "epicwebhelper",
        "crashreport",
        "crashhandler",
        "werfault",
    ]
    .iter()
    .any(|helper| name.trim() == *helper || (helper.len() > 8 && name.starts_with(helper)))
    {
        return false;
    }
    let needle = format!("{}={token}", reaper::TOKEN_VAR);
    fs::read(process.join("environ")).is_ok_and(|env| {
        env.split(|c| *c == 0)
            .any(|entry| entry == needle.as_bytes())
    })
}

fn placement(token: &str, window: &Window, pids: &BTreeSet<i64>) -> String {
    let pids = pids
        .iter()
        .map(|pid| format!("[{pid}]=true"))
        .collect::<Vec<_>>()
        .join(",");
    // Selection and movement happen together on the compositor thread. Two
    // simultaneous launches cannot claim the same not-yet-created workspace.
    format!(
        r#"eval local key="{token}"; local selector="address:{address}";
local w=hl.get_window(selector); if not w or w.pid~={pid} or not w.mapped or w.hidden then return end;
_G.__steam_tui_workspaces=_G.__steam_tui_workspaces or {{}};
local reservations=_G.__steam_tui_workspaces; local id=reservations[key]; local own={{{pids}}};
if id then for _,other in ipairs(hl.get_workspace_windows(tostring(id)) or {{}}) do if not own[other.pid] then id=nil; break end end end;
if not id then local used={{}}; for _,ws in ipairs(hl.get_workspaces()) do used[ws.id]=true end;
for _,reserved in pairs(reservations) do used[reserved]=true end;
id=1; while used[id] do id=id+1 end; reservations[key]=id end;
hl.dispatch(hl.dsp.window.move({{window=selector,workspace=tostring(id),follow=true}}));
hl.dispatch(hl.dsp.window.fullscreen({{window=selector,mode="fullscreen",action="set"}}))"#,
        address = window.address,
        pid = window.pid
    )
}

/// A separate helper survives the TUI exiting, so slow launches and replacement
/// game windows still get placement. The helper exits with the game's main PID.
pub(super) fn watch(pid: u32, token: &str, log: &fs::File) -> Result<()> {
    if socket_path().is_none() {
        return Ok(());
    }
    let Some(start) = start_time(pid) else {
        return Ok(());
    };
    let mut command = Command::new("/proc/self/exe");
    command
        .args([
            "--internal-game-desktop",
            &pid.to_string(),
            &start.to_string(),
            token,
        ])
        .env_remove(reaper::TOKEN_VAR)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn internal(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("--internal-game-desktop") {
        return None;
    }
    Some((|| {
        if args.len() != 4 || !safe_key(&args[3]) {
            return Err(failure("Invalid game desktop helper arguments"));
        }
        let pid = args[1].parse::<u32>()?;
        let start = args[2].parse::<u64>()?;
        let token = &args[3];
        let Some(path) = socket_path() else {
            return Ok(());
        };
        let mut placed = BTreeSet::new();
        let outcome = (|| -> Result<()> {
            while start_time(pid) == Some(start) {
                let rows: Vec<serde_json::Value> =
                    serde_json::from_str(&request(&path, "j/clients")?)?;
                // An unmanaged surface with missing PID metadata must not stop
                // placement for the actual game windows.
                let clients: Vec<Window> = rows
                    .into_iter()
                    .filter_map(|row| serde_json::from_value(row).ok())
                    .collect();
                let owned: Vec<_> = clients
                    .iter()
                    .filter(|w| owns_window(w, token, Path::new("/proc")))
                    .collect();
                let pids = owned.iter().map(|w| w.pid).collect();
                // Forget destroyed addresses: an allocator may reuse one later.
                placed.retain(|address| clients.iter().any(|w| &w.address == address));
                for window in owned {
                    if placed.contains(&window.address) {
                        continue;
                    }
                    let reply = request(&path, &placement(token, window, &pids))?;
                    if reply.trim() != "ok" {
                        return Err(failure(format!("Hyprland game placement failed: {reply}")));
                    }
                    placed.insert(window.address.clone());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(())
        })();
        let _ = request(
            &path,
            &format!(
                "eval if _G.__steam_tui_workspaces then _G.__steam_tui_workspaces[\"{token}\"]=nil end"
            ),
        );
        outcome
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placement_only_accepts_exact_launch_tokens_and_game_processes() {
        let root = std::env::temp_dir().join(format!("steam-tui-desktop-{}", std::process::id()));
        fs::create_dir_all(root.join("42")).unwrap();
        let window = Window {
            address: "0xab12".into(),
            pid: 42,
            mapped: true,
            hidden: false,
        };
        fs::write(root.join("42/comm"), "GameThread\n").unwrap();
        for env in [
            b"OTHER_TOKEN=target\0".as_slice(),
            b"STEAM_TUI_LAUNCH_TOKEN=target-extra\0",
        ] {
            fs::write(root.join("42/environ"), env).unwrap();
            assert!(!owns_window(&window, "target", &root));
        }
        fs::write(root.join("42/environ"), b"STEAM_TUI_LAUNCH_TOKEN=target\0").unwrap();
        assert!(owns_window(&window, "target", &root));
        for helper in ["steam", "steam.exe", "EpicWebHelper.ex", "CrashReportClie"] {
            fs::write(root.join("42/comm"), helper).unwrap();
            assert!(!owns_window(&window, "target", &root));
        }
        fs::remove_dir_all(root).unwrap();
        assert!(!valid_address("0x1; dispatch exit"));
        assert!(!safe_key("../../other"));
    }
}
