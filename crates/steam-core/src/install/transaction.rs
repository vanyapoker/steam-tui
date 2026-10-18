use super::filesystem::{self as fsafe, Lock};
use super::{InstallRequest, Layout, Result, ReusableChunk, VerifyProgress, failure, hash_bytes};
use crate::catalog::InstallPlatform;
use crate::content::{DepotChunk, DepotFile};
use crate::installed::{InstalledGame, Node, read_vdf};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
#[cfg(test)]
mod tests;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    target: PathBuf,
    source: PathBuf,
    existed: bool,
    preserve: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    steam_root: PathBuf,
    /// Relative to steam_root; only the two established registry locations.
    path: PathBuf,
    existed: bool,
    before_hash: Option<[u8; 20]>,
    after_hash: [u8; 20],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    app_id: u32,
    steam_root: PathBuf,
    install_name: String,
    committed: bool,
    operations: Vec<Operation>,
    created_dirs: Vec<PathBuf>,
    registry: Option<Registry>,
    preserved_name: String,
}

pub(super) struct Session {
    request: InstallRequest,
    library: PathBuf,
    steamapps: PathBuf,
    stage: PathBuf,
    install: PathBuf,
    owner: u64,
    old_manifest_hash: Option<[u8; 20]>,
    _library_lock: Lock,
    _root_lock: Lock,
    _app_lock: Lock,
}

/// Upper bound on verification and resume-scan threads.
const MAX_WORKERS: usize = 12;

/// Runs `work(0..count)` on up to `MAX_WORKERS` scoped threads pulling indexes
/// from a shared counter. The first error wins and stops the others.
fn parallel<F>(count: usize, work: F) -> Result<()>
where
    F: Fn(usize) -> Result<()> + Sync,
{
    let workers = std::thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(1, MAX_WORKERS)
        .min(count);
    if workers <= 1 {
        return (0..count).try_for_each(work);
    }
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<super::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    if let Err(error) = work(index) {
                        stop.store(true, Ordering::Relaxed);
                        first_error.lock().get_or_insert(error);
                        break;
                    }
                }
            });
        }
    });
    first_error.into_inner().map_or(Ok(()), Err)
}

fn create_dirs(base: &Path, relative: &Path) -> Result<()> {
    fsafe::relative(relative)?;
    let mut current = base.to_path_buf();
    for part in relative.components() {
        current.push(part.as_os_str());
        if !fsafe::exists(&current)? {
            fs::create_dir(&current).map_err(|e| fsafe::disk_error(&current, e))?;
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
            fsafe::sync_dir(
                current
                    .parent()
                    .ok_or_else(|| failure("Directory has no parent"))?,
            )?;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(failure(format!(
                "Refusing non-directory/symlink ancestor {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn state_directory(library: &Path) -> Result<PathBuf> {
    create_dirs(library, Path::new("steamapps"))?;
    let state = library.join("steamapps/.steam-tui");
    fsafe::private_directory(&state)?;
    Ok(state)
}

pub(super) fn recover(destination: &Path) -> Result<()> {
    let library = fsafe::absolute(destination)?;
    if !library.exists() {
        return Ok(());
    }
    let state = fsafe::checked(&library, Path::new("steamapps/.steam-tui"), true)?;
    if !state.exists() {
        return Ok(());
    }
    fsafe::private_directory(&state)?;
    let _lock = Lock::acquire(&state.join("library.lock"))?;
    recover_locked(&library, &state)
}

fn recover_locked(library: &Path, state: &Path) -> Result<()> {
    for entry in fs::read_dir(state)? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let stage = entry.path();
        fsafe::private_directory(&stage)?;
        if !stage.join("journal.json").exists() {
            continue;
        }
        let journal = read_journal(&stage)?;
        validate_journal(&journal, id)?;
        let root = journal.steam_root.canonicalize()?;
        fsafe::checked(&root, Path::new("steamapps"), true)?;
        let _root_lock = Lock::acquire(&root.join("steamapps/.steam-tui-install.lock"))?;
        fsafe::refuse_running(
            &root,
            id,
            &library.join("steamapps/common").join(&journal.install_name),
        )?;
        if journal.committed {
            finalize(library, &stage, &journal)?;
        } else {
            rollback(library, &stage, &journal)?;
        }
    }
    Ok(())
}

impl Session {
    pub(super) fn prepare(mut request: InstallRequest, owner: u64) -> Result<Self> {
        if request.plan.app_id == 0 || request.plan.depots.is_empty() {
            return Err(failure("Install plan has no app/depot content"));
        }
        let name = Path::new(&request.plan.install_dir_name);
        fsafe::relative(name)?;
        if name.components().count() != 1 {
            return Err(failure("Install directory must be a single path component"));
        }
        let inspected = fsafe::destination(&request.destination)?;
        if !inspected.available {
            return Err(failure(
                inspected
                    .message
                    .unwrap_or_else(|| "Destination is unavailable".into()),
            ));
        }
        request.steam_root = request.steam_root.canonicalize()?;
        if !request.steam_root.join("steamapps").is_dir() {
            return Err(failure("Steam root has no steamapps directory"));
        }
        fsafe::checked(&request.steam_root, Path::new("steamapps"), true)?;
        request.destination = inspected.path;
        let library = request.destination.clone();
        if !library.exists() {
            fs::create_dir_all(&library)?;
        }
        let library = library.canonicalize()?;
        request.destination = library.clone();
        let state = state_directory(&library)?;
        let library_lock = Lock::acquire(&state.join("library.lock"))?;
        recover_locked(&library, &state)?;
        let root_lock =
            Lock::acquire(&request.steam_root.join("steamapps/.steam-tui-install.lock"))?;
        let app_lock = Lock::acquire(&state.join(format!("{}.lock", request.plan.app_id)))?;
        create_dirs(&library, Path::new("steamapps/common"))?;
        let steamapps = library.join("steamapps");
        let install = steamapps
            .join("common")
            .join(&request.plan.install_dir_name);
        fsafe::checked(
            &steamapps,
            &Path::new("common").join(&request.plan.install_dir_name),
            true,
        )?;
        fsafe::refuse_running(&request.steam_root, request.plan.app_id, &install)?;
        let manifest = steamapps.join(format!("appmanifest_{}.acf", request.plan.app_id));
        fsafe::checked(
            &steamapps,
            Path::new(&format!("appmanifest_{}.acf", request.plan.app_id)),
            true,
        )?;
        let old_manifest_hash = if let Some(existing) = &request.existing {
            if existing.app_id != request.plan.app_id
                || existing.library_root.canonicalize()? != library
                || existing.install_dir.canonicalize()? != install
            {
                return Err(failure(
                    "Update destination does not match the installed game; relocation is not supported",
                ));
            }
            let doc = read_vdf(&manifest)?;
            let app = doc
                .child("AppState")
                .ok_or_else(|| failure("Existing appmanifest has no AppState"))?;
            if app.text("appid").and_then(|v| v.parse::<u32>().ok()) != Some(request.plan.app_id)
                || app.text("installdir") != Some(request.plan.install_dir_name.as_str())
            {
                return Err(failure(
                    "Existing appmanifest identifies another installation",
                ));
            }
            let recorded_depots: BTreeMap<u32, u64> = app
                .child("InstalledDepots")
                .into_iter()
                .flat_map(|n| n.children.iter())
                .filter_map(|(id, node)| {
                    Some((id.parse().ok()?, node.text("manifest")?.parse().ok()?))
                })
                .collect();
            if recorded_depots != existing.installed_depots {
                return Err(failure(
                    "Installed depot metadata changed since catalog load; refresh before updating",
                ));
            }
            Some(hash_bytes(&bounded_read(&manifest, 16 * 1024 * 1024)?))
        } else {
            if fsafe::exists(&manifest)? || fsafe::exists(&install)? {
                return Err(failure(format!(
                    "Unrelated existing appmanifest or install directory collides with {}; select an empty destination",
                    install.display()
                )));
            }
            None
        };
        // Reject another app's claim even if the target appmanifest is absent.
        for entry in fs::read_dir(&steamapps)? {
            let entry = entry?;
            let filename = entry.file_name();
            let filename = filename.to_string_lossy();
            if filename.starts_with("appmanifest_")
                && filename.ends_with(".acf")
                && entry.path() != manifest
            {
                let doc = read_vdf(&entry.path())?;
                if doc.child("AppState").and_then(|a| a.text("installdir"))
                    == Some(request.plan.install_dir_name.as_str())
                {
                    return Err(failure(
                        "Another appmanifest already owns this install directory",
                    ));
                }
            }
        }
        let stage = state.join(request.plan.app_id.to_string());
        fsafe::private_directory(&stage)?;
        let plan_key = format!(
            "{}\n{}\n{}\n{:?}\n{}\n{}\n{:?}",
            request.plan.app_id,
            request.plan.build_id,
            request.plan.branch,
            request.plan.platform,
            request.plan.language,
            request.plan.install_dir_name,
            request.plan.depots
        );
        let key = stage.join("plan.key");
        if key.exists() && bounded_read(&key, 1024 * 1024)? != plan_key.as_bytes() {
            for name in ["content", "metadata", "backup"] {
                let path = stage.join(name);
                if fsafe::exists(&path)? {
                    let meta = fs::symlink_metadata(&path)?;
                    if !meta.is_dir() || meta.file_type().is_symlink() {
                        return Err(failure("Unsafe previous installer stage"));
                    }
                    fs::remove_dir_all(&path)?;
                }
            }
        }
        fsafe::atomic_write(&key, plan_key.as_bytes())?;
        for name in ["content", "metadata", "backup"] {
            fsafe::private_directory(&stage.join(name))?;
        }
        Ok(Self {
            request,
            library,
            steamapps,
            stage,
            install,
            owner,
            old_manifest_hash,
            _library_lock: library_lock,
            _root_lock: root_lock,
            _app_lock: app_lock,
        })
    }

    pub(super) fn preflight(&self, layout: &Layout, cancel: &CancellationToken) -> Result<()> {
        let mut needed = 0u64;
        let (_, block_size) = fsafe::space_info(&self.library)?;
        for planned in &layout.files {
            super::check_cancel(cancel)?;
            let file = &planned.file;
            let target = fsafe::checked(&self.install, &file.path, false)?;
            if fsafe::exists(&target)? {
                let meta = fs::symlink_metadata(&target)?;
                if file.flags & 64 != 0 {
                    if !meta.is_dir() || meta.file_type().is_symlink() {
                        return Err(failure(format!(
                            "Directory collision: {}",
                            target.display()
                        )));
                    }
                } else if meta.is_dir() {
                    return Err(failure(format!(
                        "File collides with existing directory (user files are never deleted): {}",
                        target.display()
                    )));
                } else if !layout.old_files.contains_key(&file.path) {
                    return Err(failure(format!(
                        "New depot file collides with unmanaged user content: {} (move it aside explicitly)",
                        target.display()
                    )));
                }
            }
            if file.flags & (64 | 512) == 0 {
                let staged = fsafe::checked(&self.stage.join("content"), &file.path, true)?;
                let allocated = match fs::symlink_metadata(&staged) {
                    Ok(metadata) if metadata.is_file() => metadata
                        .blocks()
                        .checked_mul(512)
                        .ok_or_else(|| failure("Allocated-space overflow"))?,
                    Ok(_) => return Err(failure("Stage content is not a regular file")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                    Err(e) => return Err(e.into()),
                };
                // Invalid cached bytes can be overwritten in their existing
                // blocks; validity is checked during reuse, never inferred
                // from allocation. Sparse/unallocated blocks still need room.
                let rounded = file
                    .size
                    .checked_add(block_size - 1)
                    .map(|v| v / block_size * block_size)
                    .ok_or_else(|| failure("Required-space overflow"))?;
                needed = needed
                    .checked_add(rounded.saturating_sub(allocated))
                    .ok_or_else(|| failure("Required-space overflow"))?;
            }
        }
        // Account for block rounding, directory entries, two journal versions,
        // manifest/registry backups and filesystem metadata. Existing content
        // stays allocated until commit, and is not counted as free space.
        let overhead = (layout.files.len() as u64)
            .checked_mul(block_size)
            .and_then(|v| v.checked_mul(2))
            .and_then(|v| v.checked_add(2 * 1024 * 1024))
            .ok_or_else(|| failure("Required-space overflow"))?;
        needed = needed
            .checked_add(overhead)
            .ok_or_else(|| failure("Required-space overflow"))?;
        let registry_path = if self
            .request
            .steam_root
            .join("steamapps/libraryfolders.vdf")
            .exists()
        {
            self.request.steam_root.join("steamapps/libraryfolders.vdf")
        } else {
            self.request.steam_root.join("config/libraryfolders.vdf")
        };
        let registry_size = match fs::symlink_metadata(&registry_path) {
            Ok(metadata) => metadata.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        needed = needed
            .checked_add(registry_size)
            .ok_or_else(|| failure("Required-space overflow"))?;
        let free = fsafe::free_space(&self.library)?;
        fsafe::require_space(needed, free)?;
        let root_free = fsafe::free_space(&self.request.steam_root)?;
        let registry_space = registry_size
            .checked_mul(2)
            .and_then(|v| v.checked_add(2 * 1024 * 1024))
            .ok_or_else(|| failure("Registry-space overflow"))?;
        fsafe::require_space(registry_space, root_free)?;
        Ok(())
    }

    pub(super) fn prepare_file(&self, file: &DepotFile) -> Result<()> {
        let base = self.stage.join("content");
        let path = fsafe::checked(&base, &file.path, true)?;
        if let Some(parent) = file.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_dirs(&base, parent)?;
        }
        if fsafe::exists(&path)? {
            if !fs::symlink_metadata(&path)?.is_file() {
                return Err(failure("Stage content is not a regular file"));
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let handle = fsafe::regular(&path, true, true)?;
        handle
            .set_len(file.size)
            .map_err(|e| fsafe::disk_error(&path, e))?;
        Ok(())
    }

    pub(super) fn reuse_chunk(
        &self,
        path: &Path,
        chunk: &DepotChunk,
        old: Option<&ReusableChunk>,
    ) -> Result<bool> {
        let staged = fsafe::checked(&self.stage.join("content"), path, true)?;
        if fsafe::read_chunk(&staged, chunk, chunk.offset)?.is_some() {
            return Ok(true);
        }
        if let Some(old) = old
            && old.size == chunk.uncompressed_size
        {
            let candidate = fsafe::checked(&self.install, &old.path, false)?;
            if fsafe::exists(&candidate)?
                && fs::symlink_metadata(&candidate)?.is_file()
                && let Some(bytes) = fsafe::read_chunk(&candidate, chunk, old.offset)?
            {
                self.write_chunk(path, chunk, &bytes)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Writes one verified chunk. There is deliberately no fsync here: a staged
    /// byte is never trusted without being re-hashed. Resume re-verifies each
    /// chunk from disk (`reuse_chunk`), so data lost or torn by a crash is
    /// detected and downloaded again, and `verify_file` hashes the complete file
    /// and fsyncs it before the file can be committed.
    pub(super) fn write_chunk(&self, path: &Path, chunk: &DepotChunk, bytes: &[u8]) -> Result<()> {
        crate::content::verify_chunk(chunk, bytes)?;
        self.write_chunk_data(path, chunk, bytes)
    }

    pub(super) fn write_downloaded_chunk(
        &self,
        path: &Path,
        chunk: &DepotChunk,
        verified: &crate::content::VerifiedChunk,
    ) -> Result<()> {
        self.write_chunk_data(path, chunk, verified.bytes_for(chunk)?)
    }

    fn write_chunk_data(&self, path: &Path, chunk: &DepotChunk, bytes: &[u8]) -> Result<()> {
        let staged = fsafe::checked(&self.stage.join("content"), path, true)?;
        let mut file = fsafe::regular(&staged, true, false)?;
        file.seek(SeekFrom::Start(chunk.offset))?;
        file.write_all(bytes)
            .map_err(|e| fsafe::disk_error(&staged, e))?;
        Ok(())
    }

    /// Which chunks of each `(file, chunk range)` window are already valid
    /// in the stage (or can be copied from the old install), checked in parallel
    /// across all windows in one call. Result `[w][i]` is chunk `range.start + i`
    /// of window `w`.
    pub(super) fn reuse_scan(
        &self,
        windows: &[(&DepotFile, std::ops::Range<usize>)],
        reuse: &std::collections::HashMap<[u8; 20], ReusableChunk>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<bool>>> {
        let flags: Vec<Vec<AtomicBool>> = windows
            .iter()
            .map(|(_, range)| range.clone().map(|_| AtomicBool::new(false)).collect())
            .collect();
        let tasks: Vec<(usize, usize)> = windows
            .iter()
            .enumerate()
            .flat_map(|(w, (_, range))| (0..range.len()).map(move |i| (w, i)))
            .collect();
        parallel(tasks.len(), |index| {
            super::check_cancel(cancel)?;
            let (w, i) = tasks[index];
            let (file, range) = &windows[w];
            let chunk = &file.chunks[range.start + i];
            let reused = self.reuse_chunk(&file.path, chunk, reuse.get(&chunk.id))?;
            flags[w][i].store(reused, Ordering::Relaxed);
            Ok(())
        })?;
        Ok(flags
            .into_iter()
            .map(|w| w.into_iter().map(AtomicBool::into_inner).collect())
            .collect())
    }

    /// Hashes the complete staged file against the manifest SHA1, then applies the
    /// final permissions and fsyncs it.
    pub(super) fn verify_file(
        &self,
        file: &DepotFile,
        cancel: &CancellationToken,
        progress: &VerifyProgress,
    ) -> Result<()> {
        progress.begin(&file.path);
        let path = fsafe::checked(&self.stage.join("content"), &file.path, true)?;
        let hash = fsafe::hash_file_progress(&path, file.size, cancel, &|bytes| {
            progress.add_bytes(bytes)
        })?;
        if file.size != 0 && hash != file.sha1 {
            return Err(failure(format!(
                "Complete file SHA1 mismatch: {}",
                file.path.display()
            )));
        }
        // Steam makes every file of a container runtime executable; its depots carry no flags.
        let tool = self
            .request
            .plan
            .install_dir_name
            .starts_with("SteamLinuxRuntime");
        let mode = if file.flags & 32 != 0 || tool {
            0o755
        } else {
            0o644
        };
        let mode = if file.flags & 8 != 0 {
            mode & !0o222
        } else {
            mode
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        fsafe::regular(&path, false, false)?.sync_all()?;
        progress.finish_file();
        Ok(())
    }

    /// Verifies every staged regular file in parallel, then creates symlinks.
    /// Each worker hashes whole files end to end against the manifest SHA1 (the
    /// authoritative check; nothing unverified is ever committed), applies the
    /// final permissions and fsyncs that file. The first error stops the others
    /// and cancellation is honored at every buffer.
    pub(super) fn finish_stage(
        &self,
        layout: &Layout,
        cancel: &CancellationToken,
        progress: &VerifyProgress,
    ) -> Result<()> {
        // Largest first keeps the workers balanced when a few files dominate.
        let mut regular: Vec<&DepotFile> = layout
            .files
            .iter()
            .map(|planned| &planned.file)
            .filter(|file| file.flags & (64 | 512) == 0)
            .collect();
        regular.sort_by_key(|file| std::cmp::Reverse(file.size));
        parallel(regular.len(), |index| {
            super::check_cancel(cancel)?;
            self.verify_file(regular[index], cancel, progress)
        })?;
        for planned in &layout.files {
            super::check_cancel(cancel)?;
            let file = &planned.file;
            if file.flags & 512 != 0 {
                let base = self.stage.join("content");
                let path = fsafe::checked(&base, &file.path, false)?;
                if let Some(parent) = file.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    create_dirs(&base, parent)?;
                }
                if fsafe::exists(&path)? {
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.file_type().is_symlink()
                        || fs::read_link(&path)?
                            != *file
                                .link_target
                                .as_ref()
                                .ok_or_else(|| failure("Symlink has no target"))?
                    {
                        return Err(failure(
                            "Stage symlink collision; remove the rejected stage before retrying",
                        ));
                    }
                } else {
                    symlink(
                        file.link_target
                            .as_ref()
                            .ok_or_else(|| failure("Symlink has no target"))?,
                        &path,
                    )?;
                }
                fsafe::sync_dir(
                    path.parent()
                        .ok_or_else(|| failure("Symlink has no parent"))?,
                )?;
            }
        }
        let manifest = self.build_manifest(layout)?;
        fsafe::atomic_write(
            &self.stage.join("metadata/appmanifest.acf"),
            &encode_vdf(&manifest)?,
        )?;
        fsafe::sync_dir(&self.stage.join("content"))?;
        Ok(())
    }

    fn build_manifest(&self, layout: &Layout) -> Result<Node> {
        let manifest = self
            .steamapps
            .join(format!("appmanifest_{}.acf", self.request.plan.app_id));
        let mut doc = if self.old_manifest_hash.is_some() {
            read_vdf(&manifest)?
        } else {
            Node::default()
        };
        let app = doc.children.entry("appstate".into()).or_default();
        for (key, value) in [
            ("appid", self.request.plan.app_id.to_string()),
            // Steam rejects a missing Universe (default 0) in the public client.
            ("universe", "1".into()),
            ("name", self.request.plan.name.clone()),
            ("installdir", self.request.plan.install_dir_name.clone()),
            ("stateflags", "4".into()),
            ("buildid", self.request.plan.build_id.to_string()),
            ("sizeondisk", layout.bytes.to_string()),
            ("lastupdated", timestamp().to_string()),
            ("lastowner", self.owner.to_string()),
            ("updateresult", "0".into()),
            ("bytestodownload", "0".into()),
            ("bytesdownloaded", "0".into()),
            ("bytestostage", "0".into()),
            ("bytesstaged", "0".into()),
            ("targetbuildid", "0".into()),
        ] {
            put(app, key, value);
        }
        let config = app.children.entry("userconfig".into()).or_default();
        put(config, "language", self.request.plan.language.clone());
        put(config, "betakey", self.request.plan.branch.clone());
        put(
            config,
            "os_override",
            match self.request.plan.platform {
                InstallPlatform::Linux => "linux",
                InstallPlatform::Windows => "windows",
            }
            .into(),
        );
        let mut installed = Node::default();
        for depot in &self.request.plan.depots {
            let entry = installed
                .children
                .entry(depot.depot_id.to_string())
                .or_default();
            put(entry, "manifest", depot.manifest_id.to_string());
            if depot.source_app_id != self.request.plan.app_id {
                put(entry, "dlcappid", depot.source_app_id.to_string());
            }
        }
        app.children.insert("installeddepots".into(), installed);
        let mut shared = Node::default();
        for (depot, source) in &self.request.plan.shared_depots {
            put(&mut shared, &depot.to_string(), source.to_string());
        }
        app.children.insert("shareddepots".into(), shared);
        // Only depot-flagged, fully verified files may become first-run scripts.
        let mut scripts = Node::default();
        put(&mut scripts, "version", "1".into());
        let files = scripts.children.entry("files".into()).or_default();
        let mut legacy = Node::default();
        for planned in layout.files.iter().filter(|p| p.file.flags & 256 != 0) {
            let mut script = Node::default();
            put(
                &mut script,
                "path",
                planned.file.path.to_string_lossy().into_owned(),
            );
            let hash = planned
                .file
                .sha1
                .ok_or_else(|| failure("Install script has no content hash"))?;
            put(
                &mut script,
                "sha1",
                hash.iter().map(|b| format!("{b:02x}")).collect(),
            );
            put(&mut script, "depot", planned.depot.depot_id.to_string());
            put(
                &mut script,
                "manifest",
                planned.depot.manifest_id.to_string(),
            );
            put(
                &mut legacy,
                &planned.depot.depot_id.to_string(),
                planned.file.path.to_string_lossy().into_owned(),
            );
            files
                .children
                .insert(files.children.len().to_string(), script);
        }
        app.children
            .insert("steamtuiinstallscripts".into(), scripts);
        app.children.insert("installscripts".into(), legacy);
        Ok(doc)
    }

    pub(super) fn commit(&self, layout: &Layout) -> Result<(InstalledGame, Option<PathBuf>)> {
        fsafe::refuse_running(
            &self.request.steam_root,
            self.request.plan.app_id,
            &self.install,
        )?;
        let manifest_path = self
            .steamapps
            .join(format!("appmanifest_{}.acf", self.request.plan.app_id));
        let current = if fsafe::exists(&manifest_path)? {
            Some(hash_bytes(&bounded_read(&manifest_path, 16 * 1024 * 1024)?))
        } else {
            None
        };
        if current != self.old_manifest_hash {
            return Err(failure(
                "Appmanifest changed during download; refusing to overwrite another installer/client",
            ));
        }
        let prefix = Path::new("common").join(&self.request.plan.install_dir_name);
        let mut journal = Journal {
            version: 1,
            app_id: self.request.plan.app_id,
            steam_root: self.request.steam_root.clone(),
            install_name: self.request.plan.install_dir_name.clone(),
            committed: false,
            operations: Vec::new(),
            created_dirs: Vec::new(),
            registry: None,
            preserved_name: format!(
                "{}-{}-{}",
                self.request.plan.build_id,
                timestamp(),
                std::process::id()
            ),
        };
        for planned in &layout.files {
            let file = &planned.file;
            let target = fsafe::checked(&self.steamapps, &prefix.join(&file.path), false)?;
            if file.flags & 64 != 0 {
                continue;
            }
            let existed = fsafe::exists(&target)?;
            if existed && !layout.old_files.contains_key(&file.path) {
                return Err(failure(format!(
                    "Unmanaged file appeared during download: {}; refusing to overwrite it",
                    target.display()
                )));
            }
            // Steam UserConfig/VersionedUserConfig files are intentionally not
            // replaced. Their vendor defaults were still downloaded/verified.
            let user_config = file.flags & 3 != 0
                || layout
                    .old_files
                    .get(&file.path)
                    .is_some_and(|old| old.flags & 3 != 0);
            if existed && user_config {
                // A retained user link must not let newly installed aliases
                // redirect writes outside the game tree.
                if fs::symlink_metadata(&target)?.file_type().is_symlink() {
                    let root = fs::canonicalize(self.steamapps.join(&prefix))?;
                    let resolved = fs::canonicalize(&target).map_err(|_| {
                        failure(format!(
                            "Retained configuration link {} is unresolved; remove or repair it before updating",
                            target.display()
                        ))
                    })?;
                    if !resolved.starts_with(root) {
                        return Err(failure(format!(
                            "Retained configuration link {} points outside the game tree; replace it before updating",
                            target.display()
                        )));
                    }
                }
                continue;
            }
            let preserve = if existed {
                let meta = fs::symlink_metadata(&target)?;
                if meta.file_type().is_symlink() {
                    layout
                        .old_files
                        .get(&file.path)
                        .and_then(|f| f.link_target.as_ref())
                        .is_none_or(|old| fs::read_link(&target).ok().as_ref() != Some(old))
                } else if meta.is_file() {
                    match layout.old_files.get(&file.path) {
                        Some(old) if old.flags & (64 | 512) == 0 => {
                            fsafe::hash_file(&target, old.size)? != old.sha1
                        }
                        _ => true,
                    }
                } else {
                    return Err(failure("Install target changed type during download"));
                }
            } else {
                false
            };
            journal.operations.push(Operation {
                target: prefix.join(&file.path),
                source: Path::new("content").join(&file.path),
                existed,
                preserve,
            });
        }
        journal.operations.push(Operation {
            target: PathBuf::from(format!("appmanifest_{}.acf", journal.app_id)),
            source: PathBuf::from("metadata/appmanifest.acf"),
            existed: self.old_manifest_hash.is_some(),
            preserve: false,
        });
        // Journal all new directories before creating any of them.
        let mut dirs = std::collections::BTreeSet::new();
        dirs.insert(prefix.clone());
        for planned in &layout.files {
            let target = prefix.join(&planned.file.path);
            let parent = if planned.file.flags & 64 != 0 {
                target.as_path()
            } else {
                target
                    .parent()
                    .ok_or_else(|| failure("Depot path has no parent"))?
            };
            for dir in parent.ancestors().filter(|p| !p.as_os_str().is_empty()) {
                if !fsafe::exists(&fsafe::checked(&self.steamapps, dir, true)?)? {
                    dirs.insert(dir.to_path_buf());
                }
            }
        }
        journal.created_dirs = dirs
            .into_iter()
            .filter(|p| !self.steamapps.join(p).exists())
            .collect();
        journal.created_dirs.sort_by_key(|p| p.components().count());
        save_journal(&self.stage, &journal)?;
        let result = (|| {
            for dir in &journal.created_dirs {
                create_dirs(&self.steamapps, dir)?;
            }
            for (index, operation) in journal.operations.iter().enumerate() {
                let target = fsafe::checked(&self.steamapps, &operation.target, false)?;
                let source = fsafe::checked(&self.stage, &operation.source, false)?;
                let backup = self.stage.join("backup").join(index.to_string());
                if fsafe::exists(&target)? != operation.existed {
                    return Err(failure("Install path changed during commit"));
                }
                if operation.existed {
                    fs::rename(&target, &backup).map_err(|e| fsafe::disk_error(&backup, e))?;
                    fsafe::sync_dir(
                        target
                            .parent()
                            .ok_or_else(|| failure("Target has no parent"))?,
                    )?;
                    fsafe::sync_dir(&self.stage.join("backup"))?;
                }
                fs::rename(&source, &target).map_err(|e| fsafe::disk_error(&target, e))?;
                fsafe::sync_dir(
                    source
                        .parent()
                        .ok_or_else(|| failure("Source has no parent"))?,
                )?;
                fsafe::sync_dir(
                    target
                        .parent()
                        .ok_or_else(|| failure("Target has no parent"))?,
                )?;
            }
            register_library(&self.request, &self.stage, &mut journal, layout.bytes)
        })();
        if let Err(error) = result {
            // All mutations so far are covered by the durable uncommitted
            // journal; recovery remains idempotent if rollback is interrupted.
            if let Err(rollback_error) = rollback(&self.library, &self.stage, &journal) {
                return Err(failure(format!(
                    "Commit failed: {error}; rollback needs recovery: {rollback_error}. Do not launch this app; retry install/recover."
                )));
            }
            return Err(error);
        }
        journal.committed = true;
        // A failed durability barrier retains the journal and backups. Do not
        // start rollback with an ambiguously persisted committed marker.
        save_journal(&self.stage, &journal)?;
        let preserved = finalize(&self.library, &self.stage, &journal)?;
        let existing = self.request.existing.as_ref();
        let game = InstalledGame {
            app_id: self.request.plan.app_id,
            name: self.request.plan.name.clone(),
            size_bytes: Some(layout.bytes),
            last_played: existing.and_then(|g| g.last_played),
            playtime_minutes: existing.and_then(|g| g.playtime_minutes),
            install_dir: self.install.clone(),
            library_root: self.library.clone(),
            steam_root: self.request.steam_root.clone(),
            launch_options: existing.and_then(|g| g.launch_options.clone()),
            build_id: Some(self.request.plan.build_id),
            branch: self.request.plan.branch.clone(),
            language: self.request.plan.language.clone(),
            platform: Some(self.request.plan.platform),
            installed_depots: self
                .request
                .plan
                .depots
                .iter()
                .map(|d| (d.depot_id, d.manifest_id))
                .collect(),
        };
        Ok((game, preserved))
    }
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = fsafe::regular(path, false, false)?;
    if file.metadata()?.len() > maximum {
        return Err(failure(format!(
            "Installer metadata exceeds limit: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(failure("Installer metadata grew past limit"));
    }
    Ok(bytes)
}

fn save_journal(stage: &Path, journal: &Journal) -> Result<()> {
    fsafe::atomic_write(&stage.join("journal.json"), &serde_json::to_vec(journal)?)
}
fn read_journal(stage: &Path) -> Result<Journal> {
    Ok(serde_json::from_slice(&bounded_read(
        &stage.join("journal.json"),
        32 * 1024 * 1024,
    )?)?)
}

fn validate_journal(journal: &Journal, app_id: u32) -> Result<()> {
    fsafe::relative(Path::new(&journal.install_name))?;
    if journal.version != 1
        || journal.app_id != app_id
        || Path::new(&journal.install_name).components().count() != 1
        || !journal.steam_root.is_absolute()
    {
        return Err(failure("Invalid recovery journal identity/version"));
    }
    fsafe::relative(Path::new(&journal.preserved_name))?;
    if Path::new(&journal.preserved_name).components().count() != 1 {
        return Err(failure("Invalid preserved-backup name"));
    }
    let prefix = Path::new("common").join(&journal.install_name);
    for operation in &journal.operations {
        fsafe::relative(&operation.target)?;
        fsafe::relative(&operation.source)?;
        if !(operation.target.starts_with(&prefix) && operation.source.starts_with("content"))
            && !(operation.target == format!("appmanifest_{app_id}.acf")
                && operation.source == Path::new("metadata/appmanifest.acf"))
        {
            return Err(failure("Recovery journal refers outside this app"));
        }
    }
    for dir in &journal.created_dirs {
        fsafe::relative(dir)?;
        if dir != Path::new("common") && !dir.starts_with(&prefix) {
            return Err(failure("Recovery directory outside this app"));
        }
    }
    if let Some(registry) = &journal.registry
        && (registry.steam_root != journal.steam_root
            || ![
                Path::new("steamapps/libraryfolders.vdf"),
                Path::new("config/libraryfolders.vdf"),
            ]
            .contains(&registry.path.as_path()))
    {
        return Err(failure("Recovery registry outside Steam metadata"));
    }
    Ok(())
}

fn rollback(library: &Path, stage: &Path, journal: &Journal) -> Result<()> {
    validate_journal(journal, journal.app_id)?;
    if let Some(registry) = &journal.registry {
        restore_registry(stage, registry)?;
    }
    let steamapps = library.join("steamapps");
    for (index, operation) in journal.operations.iter().enumerate().rev() {
        let target = fsafe::checked(&steamapps, &operation.target, false)?;
        let source = fsafe::checked(stage, &operation.source, false)?;
        let backup = stage.join("backup").join(index.to_string());
        let has_backup = fsafe::exists(&backup)?;
        if has_backup || !operation.existed {
            if fsafe::exists(&target)? && !fsafe::exists(&source)? {
                fs::rename(&target, &source)?;
                fsafe::sync_dir(
                    target
                        .parent()
                        .ok_or_else(|| failure("Target has no parent"))?,
                )?;
                fsafe::sync_dir(
                    source
                        .parent()
                        .ok_or_else(|| failure("Source has no parent"))?,
                )?;
            } else if has_backup && fsafe::exists(&target)? {
                return Err(failure(
                    "Both stage and commit target exist; refusing destructive recovery",
                ));
            }
        }
        if has_backup {
            fs::rename(&backup, &target)?;
            fsafe::sync_dir(&stage.join("backup"))?;
            fsafe::sync_dir(
                target
                    .parent()
                    .ok_or_else(|| failure("Target has no parent"))?,
            )?;
        } else if operation.existed && !fsafe::exists(&target)? {
            return Err(failure("Recovery original and backup are both missing"));
        }
    }
    for dir in journal.created_dirs.iter().rev() {
        let path = fsafe::checked(&steamapps, dir, true)?;
        if path.exists() {
            match fs::remove_dir(&path) {
                Ok(()) => fsafe::sync_dir(
                    path.parent()
                        .ok_or_else(|| failure("Directory has no parent"))?,
                )?,
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    fs::remove_file(stage.join("journal.json"))?;
    fsafe::sync_dir(stage)?;
    Ok(())
}

fn finalize(library: &Path, stage: &Path, journal: &Journal) -> Result<Option<PathBuf>> {
    let mut preserved = None;
    for (index, operation) in journal.operations.iter().enumerate() {
        let backup = stage.join("backup").join(index.to_string());
        if !fsafe::exists(&backup)? {
            continue;
        }
        if operation.preserve {
            let root = library.join("steamapps/.steam-tui/preserved");
            fsafe::private_directory(&root)?;
            let app = root.join(journal.app_id.to_string());
            fsafe::private_directory(&app)?;
            let collection = app.join(&journal.preserved_name);
            fsafe::private_directory(&collection)?;
            let destination = fsafe::checked(&collection, &operation.target, false)?;
            if let Some(parent) = operation
                .target
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                create_dirs(&collection, parent)?;
            }
            if fsafe::exists(&destination)? {
                return Err(failure(
                    "Preserved backup collision; committed installation is intact but recovery needs attention",
                ));
            }
            fs::rename(&backup, &destination)?;
            fsafe::sync_dir(
                destination
                    .parent()
                    .ok_or_else(|| failure("Backup has no parent"))?,
            )?;
            preserved = Some(collection);
        } else {
            fs::remove_file(&backup)?;
        }
        fsafe::sync_dir(&stage.join("backup"))?;
    }
    if stage.join("registry.before").exists() {
        fs::remove_file(stage.join("registry.before"))?;
    }
    fs::remove_file(stage.join("journal.json"))?;
    fsafe::sync_dir(stage)?;
    Ok(preserved)
}

fn put(node: &mut Node, key: &str, value: String) {
    node.children.insert(
        key.into(),
        Node {
            value: Some(value),
            children: BTreeMap::new(),
        },
    );
}

pub(super) fn encode_vdf(node: &Node) -> Result<Vec<u8>> {
    fn quote(value: &str, out: &mut String) -> Result<()> {
        if value.contains('\0') {
            return Err(failure("NUL in Steam metadata"));
        }
        out.push('"');
        for c in value.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push(c),
            }
        }
        out.push('"');
        Ok(())
    }
    fn append(node: &Node, depth: usize, out: &mut String) -> Result<()> {
        if depth > 64 {
            return Err(failure("Steam metadata nesting exceeds limit"));
        }
        for (key, value) in &node.children {
            for _ in 0..depth {
                out.push('\t');
            }
            quote(key, out)?;
            if let Some(text) = &value.value {
                if !value.children.is_empty() {
                    return Err(failure("Mixed scalar/table Steam metadata"));
                }
                out.push('\t');
                quote(text, out)?;
                out.push('\n');
            } else {
                out.push('\n');
                for _ in 0..depth {
                    out.push('\t');
                }
                out.push_str("{\n");
                append(value, depth + 1, out)?;
                for _ in 0..depth {
                    out.push('\t');
                }
                out.push_str("}\n");
            }
        }
        Ok(())
    }
    let mut out = String::new();
    append(node, 0, &mut out)?;
    if out.len() > 16 * 1024 * 1024 {
        return Err(failure("Steam metadata exceeds limit"));
    }
    Ok(out.into_bytes())
}

fn register_library(
    request: &InstallRequest,
    stage: &Path,
    journal: &mut Journal,
    installed_bytes: u64,
) -> Result<()> {
    let primary = request.steam_root.join("steamapps/libraryfolders.vdf");
    let relative = if primary.exists()
        || !request
            .steam_root
            .join("config/libraryfolders.vdf")
            .exists()
    {
        PathBuf::from("steamapps/libraryfolders.vdf")
    } else {
        PathBuf::from("config/libraryfolders.vdf")
    };
    let path = fsafe::checked(&request.steam_root, &relative, true)?;
    let existed = fsafe::exists(&path)?;
    let before = if existed {
        Some(bounded_read(&path, 16 * 1024 * 1024)?)
    } else {
        None
    };
    let mut doc = if existed {
        read_vdf(&path)?
    } else {
        Node::default()
    };
    let folders = doc.children.entry("libraryfolders".into()).or_default();
    if folders.value.is_some() {
        return Err(failure("libraryfolders is not a table"));
    }
    if folders
        .children
        .keys()
        .all(|key| key.parse::<u32>().is_err())
    {
        let mut primary = Node::default();
        put(
            &mut primary,
            "path",
            request
                .steam_root
                .to_str()
                .ok_or_else(|| failure("Steam root is not UTF-8"))?
                .to_owned(),
        );
        folders.children.insert("0".into(), primary);
    }
    let wanted = request.destination.canonicalize()?;
    let mut key = None;
    let mut greatest = 0u32;
    for (index, entry) in &folders.children {
        if let Ok(number) = index.parse::<u32>() {
            greatest = greatest.max(number);
            let candidate = entry.value.as_deref().or_else(|| entry.text("path"));
            if let Some(candidate) = candidate
                && Path::new(candidate).canonicalize().ok().as_ref() == Some(&wanted)
            {
                key = Some(index.clone());
            }
        }
    }
    let key = match key {
        Some(key) => key,
        None => greatest
            .checked_add(1)
            .ok_or_else(|| failure("Library index overflow"))?
            .to_string(),
    };
    let entry = folders.children.entry(key).or_default();
    if entry.value.take().is_some() {
        put(
            entry,
            "path",
            wanted
                .to_str()
                .ok_or_else(|| failure("Steam library path is not UTF-8"))?
                .to_owned(),
        );
    }
    if entry.text("path").is_none() {
        put(
            entry,
            "path",
            wanted
                .to_str()
                .ok_or_else(|| failure("Steam library path is not UTF-8"))?
                .to_owned(),
        );
    }
    let apps = entry.children.entry("apps".into()).or_default();
    if apps.value.is_some() {
        return Err(failure("Library apps metadata is not a table"));
    }
    put(
        apps,
        &request.plan.app_id.to_string(),
        installed_bytes.to_string(),
    );
    let after = encode_vdf(&doc)?;
    if let Some(before) = &before {
        fsafe::atomic_write(&stage.join("registry.before"), before)?;
    }
    journal.registry = Some(Registry {
        steam_root: request.steam_root.clone(),
        path: relative,
        existed,
        before_hash: before.as_ref().map(|v| hash_bytes(v)),
        after_hash: hash_bytes(&after),
    });
    save_journal(stage, journal)?;
    fsafe::atomic_write(&path, &after)
}

fn restore_registry(stage: &Path, registry: &Registry) -> Result<()> {
    let path = fsafe::checked(&registry.steam_root, &registry.path, true)?;
    let current = if fsafe::exists(&path)? {
        Some(hash_bytes(&bounded_read(&path, 16 * 1024 * 1024)?))
    } else {
        None
    };
    if current == registry.before_hash {
        return Ok(());
    }
    if current != Some(registry.after_hash) {
        return Err(failure(
            "libraryfolders changed externally during commit; recovery refuses to overwrite it",
        ));
    }
    if registry.existed {
        let before = bounded_read(&stage.join("registry.before"), 16 * 1024 * 1024)?;
        if Some(hash_bytes(&before)) != registry.before_hash {
            return Err(failure("Registry recovery backup hash mismatch"));
        }
        fsafe::atomic_write(&path, &before)
    } else {
        fs::remove_file(&path)?;
        fsafe::sync_dir(
            path.parent()
                .ok_or_else(|| failure("Registry has no parent"))?,
        )
    }
}
