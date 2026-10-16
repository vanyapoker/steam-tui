//! Steam's "subprocess reaper" for direct launches. Every launch gets a unique
//! token in its environment; once the game's main command exits, whatever still
//! carries the token (Proton's steam.exe, wineserver, xalia, the container's
//! helper processes) is terminated, so nothing outlives the game and the game
//! stops counting as running.
use std::fs;
use std::time::Duration;

pub(super) const TOKEN_VAR: &str = "STEAM_TUI_LAUNCH_TOKEN";

/// Container plumbing and Wine services; these never count as "the game".
const INFRASTRUCTURE: &[&str] = &[
    "srt-bwrap",
    "bwrap",
    "pv-adverb",
    "pressure-vessel",
    "pv-bwrap",
    "pressure-ves",
];

pub(super) fn new_token() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Process {
    pid: i32,
    comm: String,
}

/// Live processes of this user whose environment carries `token`.
fn tagged(token: &str) -> Vec<Process> {
    let needle = format!("{TOKEN_VAR}={token}");
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if !environ
            .split(|byte| *byte == 0)
            .any(|var| var == needle.as_bytes())
        {
            continue;
        }
        // Zombies are already dead, only waiting to be reaped by their parent.
        let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        let zombie = stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
        if zombie {
            continue;
        }
        let comm = fs::read_to_string(entry.path().join("comm"))
            .map(|c| c.trim().to_owned())
            .unwrap_or_default();
        found.push(Process { pid, comm });
    }
    found
}

pub(super) fn any_tagged(token: &str) -> bool {
    !tagged(token).is_empty()
}

fn is_infrastructure(process: &Process) -> bool {
    INFRASTRUCTURE
        .iter()
        .any(|name| process.comm.starts_with(name))
}

/// SIGTERM, then SIGKILL after `grace`, for everything carrying the token.
pub(super) fn terminate(token: &str, grace: Duration) {
    for process in tagged(token) {
        // SAFETY: plain signal to a process of ours that carries our token.
        unsafe { libc::kill(process.pid, libc::SIGTERM) };
    }
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if tagged(token).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for process in tagged(token) {
        unsafe { libc::kill(process.pid, libc::SIGKILL) };
    }
}

/// Watches a launch: the first non-infrastructure process is the game's main
/// command; when it is gone, the rest is terminated. Returns when nothing with
/// the token is left. `finished` reports that the spawned child already exited.
pub(super) fn watch(token: &str, finished: impl Fn() -> bool) {
    let mut main: Option<i32> = None;
    let mut seen_any = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let processes = tagged(token);
        if processes.is_empty() {
            if seen_any || finished() {
                return;
            }
            continue;
        }
        seen_any = true;
        match main {
            // The game's main command only exists once the container's init
            // (pv-adverb) is up; earlier processes are launcher plumbing.
            None if !processes.iter().any(|p| p.comm.starts_with("pv-adverb")) => {}
            None => {
                main = processes
                    .iter()
                    .filter(|p| !is_infrastructure(p))
                    .map(|p| p.pid)
                    .min();
            }
            Some(pid) if !processes.iter().any(|p| p.pid == pid) => {
                terminate(token, Duration::from_secs(10));
                return;
            }
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn tokens_are_unique_and_only_tagged_processes_are_terminated() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        let spawn = |token: &str| {
            Command::new("sleep")
                .arg("30")
                .env(TOKEN_VAR, token)
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut tagged_child = spawn(&a);
        let mut other = spawn(&b);
        assert!(tagged(&a).iter().any(|p| p.pid == tagged_child.id() as i32));
        terminate(&a, Duration::from_secs(2));
        assert!(tagged_child.wait().is_ok());
        assert!(
            other.try_wait().unwrap().is_none(),
            "other launch untouched"
        );
        terminate(&b, Duration::from_secs(2));
        let _ = other.wait();
        assert!(tagged(&a).is_empty() && tagged(&b).is_empty());
    }
}
