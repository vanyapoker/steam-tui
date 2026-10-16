//! Optional Valve host: account-checked identity, cross-process leases, owned cleanup.
mod probe;
#[cfg(test)]
mod tests;
use super::*;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct HostAccount {
    pub name: String,
    pub steam_id: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Process {
    pid: i32,
    start: u64,
}
impl Process {
    fn read(pid: i32) -> Option<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let start = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()?;
        Some(Self { pid, start })
    }
    fn alive(&self) -> bool {
        Self::read(self.pid).as_ref() == Some(self)
    }
    fn tagged(&self, token: &str) -> bool {
        let needle = format!("STEAM_TUI_HOST_TOKEN={token}");
        self.alive()
            && fs::read(format!("/proc/{}/environ", self.pid))
                .is_ok_and(|v| v.split(|b| *b == 0).any(|v| v == needle.as_bytes()))
    }
    fn signal(&self, signal: i32) {
        // pidfd prevents reuse of a checked PID from signaling an unrelated process.
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_open, self.pid, 0) as i32;
            if fd < 0 {
                return;
            }
            if self.alive() {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    signal,
                    std::ptr::null::<c_void>(),
                    0,
                );
            }
            libc::close(fd);
        }
    }
}
use std::ffi::c_void;
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    root: PathBuf,
    account: HostAccountRecord,
    process: Process,
    token: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct HostAccountRecord {
    name: String,
    steam_id: u64,
}

pub(super) fn clean_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"STEAM_TUI_")
            || key.as_encoded_bytes().starts_with(b"LD_")
        {
            command.env_remove(key);
        }
    }
    for key in ["SteamAppId", "SteamGameId", "SteamOverlayGameId"] {
        command.env_remove(key);
    }
}
fn steam_pids() -> Vec<Process> {
    pids_named("steam").filter_map(Process::read).collect()
}
fn host_helpers(token: &str) -> Vec<Process> {
    pids_named("steamwebhelper")
        .filter_map(Process::read)
        .filter(|p| p.tagged(token))
        .collect()
}
fn validate(account: &HostAccount, status: &probe::Status) -> Result<()> {
    if !status.logged_on {
        return Err(failure("Valve host is not authenticated"));
    }
    if status.steam_id != account.steam_id {
        return Err(failure(format!(
            "Valve host account mismatch: selected {} ({}), host is {}. Existing host was left untouched",
            account.name, account.steam_id, status.steam_id
        )));
    }
    Ok(())
}
fn lock_path(root: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::geteuid() };
    // One namespace for this Unix user, independent of each TUI's HOME,
    // XDG_STATE_HOME or TMPDIR. Otherwise two sessions could release one host.
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let directory = if fs::symlink_metadata(&runtime).is_ok_and(|m| {
        m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid && m.mode() & 0o077 == 0
    }) {
        runtime.join("steam-tui-hosts")
    } else {
        PathBuf::from(format!("/tmp/steam-tui-hosts-{uid}"))
    };
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(failure(
            "Host lease directory must be private and owned by the current user",
        ));
    }
    Ok(directory.join(format!(
        "{:x}.lock",
        Sha1::digest(root.as_os_str().as_encoded_bytes())
    )))
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}
fn flock(file: &File, mode: i32) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn read_record(file: &mut File) -> Result<Record> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err(failure("Invalid host lease record"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn write_record(file: &mut File, record: &Record) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let bytes = serde_json::to_vec(record)?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}
pub(super) struct Lease {
    file: File,
    path: PathBuf,
}
impl Lease {
    pub fn watch(&self, game_pid: u32, game_token: &str) -> Result<()> {
        let game = Process::read(game_pid as i32)
            .ok_or_else(|| failure("Game exited before its host watcher started"))?;
        let mut command = Command::new("/proc/self/exe");
        command
            .arg("--internal-host-watch")
            .arg(&self.path)
            .arg(game.pid.to_string())
            .arg(game.start.to_string())
            .arg(game_token);
        clean_environment(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut pipe = child
            .stdout
            .take()
            .ok_or_else(|| failure("Host watcher has no ready pipe"))?;
        let mut ready = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let mut byte = [0];
        if unsafe { libc::poll(&mut ready, 1, 5000) } <= 0
            || pipe.read_exact(&mut byte).is_err()
            || byte != [1]
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failure("Host lifetime watcher did not become ready"));
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = flock(&self.file, libc::LOCK_UN);
        if flock(&self.file, libc::LOCK_EX | libc::LOCK_NB).is_err() {
            return;
        }
        if let Ok(record) = read_record(&mut self.file) {
            shutdown_owned(&record);
            let _ = self.file.set_len(0);
        }
        let _ = flock(&self.file, libc::LOCK_UN);
    }
}
fn shutdown_owned(record: &Record) {
    let Some(token) = &record.token else {
        return;
    };
    if !record.process.tagged(token) {
        return;
    }
    // Never send Steam's global shutdown command if an unrelated host appeared.
    if steam_pids().iter().all(|p| p.tagged(token)) {
        let mut command = Command::new("steam");
        clean_environment(&mut command);
        if let Ok(mut child) = command
            .arg("-shutdown")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    if record.process.tagged(token) {
        record.process.signal(libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while record.process.alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if record.process.tagged(token) {
        record.process.signal(libc::SIGKILL);
    }
    for process in host_helpers(token) {
        process.signal(libc::SIGKILL);
    }
}

pub(super) fn acquire(root: &Path, account: &HostAccount) -> Result<Lease> {
    if account.name.is_empty()
        || !account
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || account.steam_id >> 32 != 0x01100001
    {
        return Err(failure(
            "A selected account name and public individual SteamID are required for the Valve host",
        ));
    }
    let root = fs::canonicalize(root)?;
    let path = lock_path(&root)?;
    let mut file = open_lock(&path)?;
    match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
            flock(&file, libc::LOCK_SH)?;
            let record = read_record(&mut file)?;
            if record.root != root
                || record.account.steam_id != account.steam_id
                || !record.account.name.eq_ignore_ascii_case(&account.name)
                || !record.process.alive()
            {
                return Err(failure(
                    "Existing Valve host lease belongs to another account or is stale; it was left untouched",
                ));
            }
            validate(account, &probe::status(&root)?)?;
            return Ok(Lease { file, path });
        }
        Err(error) => return Err(error.into()),
    }
    let result = (|| -> Result<Record> {
        // A previous owned host may still be running after its holder crashed.
        let previous = read_record(&mut file).ok();
        let existing = steam_pids();
        if existing.len() > 1 {
            return Err(failure(
                "Multiple Steam hosts found; refusing ambiguous account selection",
            ));
        }
        if let Some(process) = existing.first() {
            let executable = fs::canonicalize(format!("/proc/{}/exe", process.pid))?;
            if !executable.starts_with(&root) {
                return Err(failure(
                    "Running Valve host belongs to another Steam installation; it was left untouched",
                ));
            }
            validate(account, &probe::status(&root)?)?;
            let token = previous
                .filter(|r| {
                    r.root == root
                        && r.account.steam_id == account.steam_id
                        && r.process == *process
                        && r.token.as_ref().is_some_and(|t| process.tagged(t))
                })
                .and_then(|r| r.token);
            return Ok(Record {
                root: root.clone(),
                account: HostAccountRecord {
                    name: account.name.clone(),
                    steam_id: account.steam_id,
                },
                process: process.clone(),
                token,
            });
        }
        let token = reaper::new_token();
        let mut command = Command::new("steam");
        clean_environment(&mut command);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path.with_extension("log"))?;
        command
            .args([
                "-silent",
                "-nochatui",
                "-nofriendsui",
                "-no-browser",
                "-login",
                &account.name,
            ])
            .env("STEAM_TUI_HOST_TOKEN", &token)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| failure(format!("Cannot start optional Valve host: {e}")))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        // Save ownership before waiting for authentication, so timeout/error can clean up.
        let wait = std::env::var("STEAM_TUI_HOST_LOGIN_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120)
            .clamp(10, 600);
        let deadline = Instant::now() + Duration::from_secs(wait);
        let mut owned: Option<Record> = None;
        while Instant::now() < deadline {
            if owned.is_none() {
                if let Some(process) = steam_pids().into_iter().find(|p| p.tagged(&token)) {
                    owned = Some(Record {
                        root: root.clone(),
                        account: HostAccountRecord {
                            name: account.name.clone(),
                            steam_id: account.steam_id,
                        },
                        process,
                        token: Some(token.clone()),
                    });
                }
            }
            if let Some(record) = &owned {
                if let Err(error) = write_record(&mut file, record) {
                    shutdown_owned(record);
                    return Err(error);
                }
                if !record.process.alive() {
                    return Err(failure("Valve host exited during sign-in"));
                }
                if let Ok(status) = probe::status(&root) {
                    if status.logged_on {
                        if let Err(error) = validate(account, &status) {
                            shutdown_owned(record);
                            return Err(error);
                        }
                        return Ok(owned.unwrap());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if let Some(record) = owned {
            shutdown_owned(&record);
        }
        Err(failure(format!(
            "Valve host sign-in for {} was not ready within {wait} seconds. Complete Valve's sign-in/Steam Guard prompt on the selected hidden display, then retry. Host log: {}",
            account.name,
            path.with_extension("log").display()
        )))
    })();
    let record = match result {
        Ok(record) => record,
        Err(error) => {
            let _ = flock(&file, libc::LOCK_UN);
            return Err(error);
        }
    };
    if let Err(error) = write_record(&mut file, &record) {
        shutdown_owned(&record);
        return Err(error);
    }
    if let Err(error) = flock(&file, libc::LOCK_SH) {
        shutdown_owned(&record);
        return Err(error.into());
    }
    Ok(Lease { file, path })
}

/// Internal subprocess entrypoints; neither creates an additional host-start path.
pub fn internal(args: &[String]) -> Option<Result<()>> {
    match args.first().map(String::as_str) {
        Some("--internal-host-probe") => Some(if args.len() == 2 {
            probe::child(Path::new(&args[1]))
        } else {
            Err(failure("Invalid host probe arguments"))
        }),
        Some("--internal-host-watch") => Some((|| {
            if args.len() != 5 {
                return Err(failure("Invalid host watcher arguments"));
            }
            let path = PathBuf::from(&args[1]);
            let file = open_lock(&path)?;
            flock(&file, libc::LOCK_SH)?;
            let lease = Lease { file, path };
            let game = Process {
                pid: args[2].parse()?,
                start: args[3].parse()?,
            };
            std::io::stdout().write_all(&[1])?;
            std::io::stdout().flush()?;
            let record = read_record(&mut lease.file.try_clone()?)?;
            loop {
                if !game.alive() && !reaper::any_tagged(&args[4]) {
                    break;
                }
                if let Some(token) = &record.token {
                    if record.process.tagged(token) {
                        for helper in host_helpers(token) {
                            helper.signal(libc::SIGKILL);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            drop(lease);
            Ok(())
        })()),
        _ => None,
    }
}
