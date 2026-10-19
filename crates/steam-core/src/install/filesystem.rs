use super::{Destination, Result, failure};
use crate::content::{self, DepotChunk};
use sha1::{Digest, Sha1};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

static UNIQUE: AtomicU64 = AtomicU64::new(0);

pub(super) fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(failure(format!(
            "Unsafe depot/install path: {}",
            path.display()
        )));
    }
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(failure("NUL in filesystem path"));
    }
    Ok(())
}

pub(super) fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(failure("Library path must not contain parent traversal"));
    }
    let mut missing = Vec::new();
    let mut parent = path.as_path();
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    parent
                        .file_name()
                        .ok_or_else(|| failure("Library has no existing parent"))?
                        .to_os_string(),
                );
                parent = parent
                    .parent()
                    .ok_or_else(|| failure("Library has no existing parent"))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut resolved = parent.canonicalize()?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub(super) fn free_space(path: &Path) -> Result<u64> {
    Ok(space_info(path)?.0)
}

pub(super) fn space_info(path: &Path) -> Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs writes the complete structure on success; CString is NUL-terminated.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    let unit = stats.f_frsize.max(1);
    let available = stats
        .f_bavail
        .checked_mul(unit)
        .ok_or_else(|| failure("Available-space overflow"))?;
    Ok((available, unit))
}

pub(super) fn require_space(needed: u64, free: u64) -> Result<()> {
    if free < needed {
        Err(failure(format!(
            "Insufficient free space: need {needed} additional bytes for verified staging and metadata, only {free} available"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn destination(path: &Path) -> Result<Destination> {
    let path = absolute(path)?;
    let mut parent = path.as_path();
    while !parent.exists() {
        parent = parent
            .parent()
            .ok_or_else(|| failure("Library has no existing parent"))?;
    }
    if !parent.is_dir() {
        return Ok(Destination {
            path,
            free_bytes: None,
            available: false,
            message: Some("Library or nearest parent is not a directory".into()),
        });
    }
    let cpath = CString::new(parent.as_os_str().as_bytes())?;
    let writable = unsafe { libc::access(cpath.as_ptr(), libc::W_OK | libc::X_OK) } == 0;
    let (free_bytes, space_error) = match free_space(parent) {
        Ok(bytes) => (Some(bytes), None),
        Err(e) => (None, Some(format!("Cannot inspect available space: {e}"))),
    };
    let message = if !writable {
        Some("Library or nearest existing parent is not writable/searchable".into())
    } else {
        space_error
    };
    Ok(Destination {
        path,
        free_bytes,
        available: writable && message.is_none(),
        message,
    })
}

pub(super) fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Reject symlink traversal, even if the link happens to point inside the tree.
/// The library root itself is canonicalized once before this function is used.
pub(crate) fn checked(base: &Path, path: &Path, include_leaf: bool) -> Result<PathBuf> {
    relative(path)?;
    let mut current = base.to_path_buf();
    let count = path.components().count();
    for (index, component) in path.components().enumerate() {
        current.push(component.as_os_str());
        if !include_leaf && index + 1 == count {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(failure(format!(
                    "Refusing symlink traversal: {}",
                    current.display()
                )));
            }
            Ok(metadata) if index + 1 < count && !metadata.is_dir() => {
                return Err(failure(format!(
                    "Non-directory path ancestor: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(base.join(path))
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    if !exists(path)? {
        fs::DirBuilder::new().mode(0o700).create(path)?;
        sync_dir(
            path.parent()
                .ok_or_else(|| failure("Private directory has no parent"))?,
        )?;
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err(failure(format!(
            "Installer state directory must be owned by the current user and mode 0700: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn regular(path: &Path, write: bool, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| disk_error(path, e))?;
    if !file.metadata()?.is_file() {
        return Err(failure(format!("Not a regular file: {}", path.display())));
    }
    Ok(file)
}

pub(super) fn read_chunk(path: &Path, chunk: &DepotChunk, offset: u64) -> Result<Option<Vec<u8>>> {
    let mut file = match regular(path, false, false) {
        Ok(file) => file,
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>()
                && io.kind() == std::io::ErrorKind::NotFound
            {
                return Ok(None);
            }
            return Err(e);
        }
    };
    if file.metadata()?.len() < offset.saturating_add(u64::from(chunk.uncompressed_size)) {
        return Ok(None);
    }
    let mut bytes = vec![0; chunk.uncompressed_size as usize];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    if content::verify_chunk(chunk, &bytes).is_ok() {
        Ok(Some(bytes))
    } else {
        Ok(None)
    }
}

pub(super) fn hash_file(path: &Path, size: u64) -> Result<Option<[u8; 20]>> {
    let mut file = regular(path, false, false)?;
    if file.metadata()?.len() != size {
        return Ok(None);
    }
    let mut hash = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(Some(hash.finalize().into()))
}

/// Whole-file SHA1 for staged verification: a 1 MiB buffer sized to the file,
/// sequential read-ahead, cancellation checked each buffer and every buffer
/// reported through `on_bytes`. `None` means the length differs from `size`.
pub(super) fn hash_file_progress(
    path: &Path,
    size: u64,
    cancel: &CancellationToken,
    on_bytes: &dyn Fn(u64),
) -> Result<Option<[u8; 20]>> {
    let mut file = regular(path, false, false)?;
    if file.metadata()?.len() != size {
        return Ok(None);
    }
    if size > 0 {
        // Advisory only; failure just means no read-ahead hint.
        unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
        }
    }
    let mut hash = Sha1::new();
    let mut buffer = vec![0u8; size.clamp(1, 1024 * 1024) as usize];
    loop {
        super::check_cancel(cancel)?;
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
        on_bytes(length as u64);
    }
    Ok(Some(hash.finalize().into()))
}

pub(super) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| failure("Atomic write has no parent"))?;
    let name = format!(
        ".steam-tui-write-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.join(name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(bytes).map_err(|e| disk_error(path, e))?;
        file.sync_all().map_err(|e| disk_error(path, e))?;
        fs::rename(&temporary, path).map_err(|e| disk_error(path, e))?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn disk_error(path: &Path, error: std::io::Error) -> super::Error {
    if error.raw_os_error() == Some(libc::ENOSPC) || error.raw_os_error() == Some(libc::EDQUOT) {
        failure(format!(
            "Disk full or quota exhausted while writing {}: {error}; previous install is retained, free space and resume",
            path.display()
        ))
    } else {
        error.into()
    }
}

pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let file = regular(path, true, true)?;
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(failure(format!(
                "Another installer holds {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(super) fn refuse_running(steam_root: &Path, app_id: u32, install: &Path) -> Result<()> {
    check_processes(steam_root, app_id, install, false)
}

/// Used exclusively by a validated, same-build DLC transaction. Valve clients
/// and PID markers still prohibit concurrent mutation of their metadata.
pub(super) fn check_processes(
    steam_root: &Path,
    app_id: u32,
    install: &Path,
    dlc: bool,
) -> Result<()> {
    check_processes_in(steam_root, app_id, install, dlc, Path::new("/proc"))
}

pub(super) fn check_processes_in(
    steam_root: &Path,
    app_id: u32,
    install: &Path,
    dlc: bool,
    processes: &Path,
) -> Result<()> {
    let app = app_id.to_string();
    let app_env = format!("SteamAppId={app}");
    let game_env = format!("SteamGameId={app}");
    for entry in fs::read_dir(processes)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let proc = entry.path();
        if let Ok(name) = fs::read_to_string(proc.join("comm"))
            && matches!(name.trim(), "steam" | "steamwebhelper" | "steam.exe")
        {
            return Err(failure(
                "Close Steam completely before installing/updating a shared library",
            ));
        }
        if let Ok(exe) = fs::read_link(proc.join("exe")) {
            let name = exe.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "steam" | "steamwebhelper" | "steam.exe") {
                return Err(failure(
                    "Close Steam completely before installing/updating a shared library",
                ));
            }
            if !dlc && exe.starts_with(install) {
                return Err(failure(format!(
                    "App {app_id} is running; close it before installation"
                )));
            }
        }
        if let Ok(file) = File::open(proc.join("environ")) {
            let mut bytes = Vec::new();
            if file.take(1024 * 1024).read_to_end(&mut bytes).is_err() {
                continue;
            }
            if !dlc
                && bytes
                    .split(|b| *b == 0)
                    .any(|part| part == app_env.as_bytes() || part == game_env.as_bytes())
            {
                return Err(failure(format!(
                    "App {app_id} has an active SteamAppId/SteamGameId process; close it first"
                )));
            }
        }
    }
    for path in [
        steam_root.join("steam.pid"),
        steam_root.join("config/steam.pid"),
    ] {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
            && pid > 0
        {
            // A live PID hint is conservatively treated as active Steam. A
            // stale hint whose PID was reused must be cleared by its owner.
            let result = unsafe { libc::kill(pid, 0) };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                return Err(failure(
                    "Steam's PID marker references a live process; close Steam/clear its stale marker before installing",
                ));
            }
        }
    }
    Ok(())
}
