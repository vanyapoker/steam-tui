//! Rust-only depot installation. Downloaded data is never executed. Files are
//! verified in a private, same-filesystem stage before a journaled commit.
//! Cancellation is honored until commit; callers must await `run`, not abort it.
//! A `PauseGate` holds the download between chunks and resumes without redoing
//! preflight; cancelling while paused still ends the job as Cancelled.
//! Staged chunk writes are not fsynced: resume re-hashes every chunk from disk,
//! and `finish_stage` verifies each file end to end in parallel and fsyncs it
//! before anything is committed.
//! Changed local managed files are retained under `.steam-tui/preserved`; files
//! absent from the new depot remain untouched, including mods and saves.
//!
//! Safety boundary: Linux filesystems with working rename, flock, fsync and
//! symlinks are required. Existing updates stay in their original library.
//! Steam must be closed; observable Steam/game processes and live Steam PID
//! markers cause refusal. Other same-user programs do not honor our locks, so
//! do not edit library paths while committing. Unreadable /proc environments
//! cannot establish that a process is a game. Symlinks never escape the game
//! tree, directory/file collisions require explicit user action, and obsolete
//! depot files are retained rather than guessing which files are saves/mods.
//! UserConfig defaults are verified but never replace existing configuration.
//! Depot install scripts are downloaded as data, never executed; prerequisites
//! or DRM requiring Steam remain explicit manual setup/launch limitations.
mod download;
mod filesystem;
#[cfg(test)]
mod tests;
mod transaction;

use crate::catalog::{DepotSpec, InstallPlan};
#[cfg(test)]
use crate::content;
use crate::content::{ContentClient, DepotFile, DepotManifest};
use crate::installed::InstalledGame;
use parking_lot::Mutex;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error as StdError;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use steam_vent::Connection;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub type Error = Box<dyn StdError + Send + Sync>;
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Destination {
    /// A library root, not its steamapps/common directory.
    pub path: PathBuf,
    pub free_bytes: Option<u64>,
    pub available: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub plan: InstallPlan,
    pub destination: PathBuf,
    pub steam_root: PathBuf,
    pub account: String,
    pub existing: Option<InstalledGame>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InstallPhase {
    Preparing,
    Downloading,
    Verifying,
    Committing,
    Finished,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct InstallProgress {
    pub app_id: u32,
    pub phase: InstallPhase,
    /// Verified uncompressed depot bytes, including verified reused bytes.
    pub completed_bytes: u64,
    pub total_bytes: u64,
    /// Compressed bytes requested from the CDN, excluding reused chunks.
    pub downloaded_bytes: u64,
    pub total_download_bytes: u64,
    /// Verifying phase: bytes of staged files hashed so far and in total.
    pub verify_bytes_done: u64,
    pub verify_bytes_total: u64,
    pub message: String,
}

/// Blocking filesystem work; call from spawn_blocking in an async UI.
pub fn destinations(steam_root: Option<&Path>) -> Result<Vec<Destination>> {
    let root = crate::installed::discover_root(steam_root)?;
    let mut warnings = Vec::new();
    let paths = crate::installed::library_roots(&root, &mut warnings);
    paths.iter().map(|path| destination(path)).collect()
}

/// Inspect the nearest existing parent for a new custom library, without
/// creating it. Available space is statvfs f_bavail (the caller's usable space).
pub fn destination(path: &Path) -> Result<Destination> {
    filesystem::destination(path)
}

/// Recover interrupted commits before displaying/launching this library's games.
/// This is blocking and refuses recovery while Steam or the affected game runs.
/// A run also performs recovery automatically before opening its stage.
pub fn recover(destination: &Path) -> Result<()> {
    transaction::recover(destination)
}

fn failure(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

async fn blocking<T, F>(action: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(action)
        .await
        .map_err(|e| failure(format!("Installer filesystem worker failed: {e}")))?
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(failure(
            "Installation cancelled; verified staging data is retained for resume",
        ))
    } else {
        Ok(())
    }
}

/// Shared pause switch. Clones control the same flag, so the UI keeps one while
/// the job holds another. Only the Downloading phase honors it, between chunks:
/// admitted chunks finish, and preflight and commit never block.
#[derive(Clone, Debug)]
pub struct PauseGate(Arc<watch::Sender<bool>>);

impl Default for PauseGate {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }
}

impl PauseGate {
    pub fn set(&self, paused: bool) {
        self.0.send_replace(paused);
    }

    pub fn is_paused(&self) -> bool {
        *self.0.borrow()
    }
}

/// Returns at once when not paused; otherwise waits for resume or cancellation
/// (cancellation takes the usual cancelled-error path in the caller).
async fn wait_while_paused(gate: &PauseGate, cancel: &CancellationToken) -> Result<()> {
    if !gate.is_paused() {
        return Ok(());
    }
    let mut paused = gate.0.subscribe();
    tokio::select! {
        _ = cancel.cancelled() => check_cancel(cancel),
        _ = paused.wait_for(|paused| !*paused) => Ok(()),
    }
}

fn report(sender: &mpsc::Sender<InstallProgress>, value: &InstallProgress) {
    // Never wait on a UI which may itself be awaiting installer shutdown.
    let _ = sender.try_send(value.clone());
}

#[derive(Clone)]
struct PlannedFile {
    depot: DepotSpec,
    file: DepotFile,
}

#[derive(Clone)]
struct ReusableChunk {
    path: PathBuf,
    offset: u64,
    size: u32,
}

/// Chunks checked per blocking resume-scan call; bounds the silence before the
/// first progress report on a large file.
const SCAN_WINDOW: usize = 32;
const REUSE_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const VERIFY_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Live counters shared between the verification worker threads and the async
/// side, which polls them for progress reports.
#[derive(Default)]
struct VerifyProgress {
    bytes: AtomicU64,
    files: AtomicU64,
    current: Mutex<String>,
}

impl VerifyProgress {
    fn begin(&self, path: &Path) {
        *self.current.lock() = path.display().to_string();
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn finish_file(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }

    /// Bytes hashed, files finished, and the most recently started file.
    fn snapshot(&self) -> (u64, u64, String) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.files.load(Ordering::Relaxed),
            self.current.lock().clone(),
        )
    }
}

struct Layout {
    files: Vec<PlannedFile>,
    old_files: BTreeMap<PathBuf, DepotFile>,
    reuse: HashMap<[u8; 20], ReusableChunk>,
    bytes: u64,
    download_bytes: u64,
    install_scripts: bool,
}

impl Layout {
    /// Regular files that `finish_stage` hashes, and their total size.
    fn verify_files(&self) -> u64 {
        self.files
            .iter()
            .filter(|planned| planned.file.flags & (64 | 512) == 0)
            .count() as u64
    }

    fn verify_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter(|planned| planned.file.flags & (64 | 512) == 0)
            .map(|planned| planned.file.size)
            .sum()
    }

    fn new(
        mut manifests: Vec<(DepotSpec, DepotManifest)>,
        mut old: Vec<DepotManifest>,
    ) -> Result<Self> {
        // Plan and installed-depot sources arrive in different orders. Use one
        // numeric overlay order for both so ownership classification is stable.
        manifests.sort_by_key(|(depot, _)| depot.depot_id);
        old.sort_by_key(|manifest| manifest.depot_id);
        let mut selected: BTreeMap<PathBuf, PlannedFile> = BTreeMap::new();
        // Later selected depots overlay earlier depots, as Steam does for DLC
        // and localized content. Ancestor/type conflicts are still rejected.
        for (depot, manifest) in manifests {
            if depot.depot_id != manifest.depot_id || depot.manifest_id != manifest.manifest_id {
                return Err(failure(
                    "Requested depot and returned manifest identities differ",
                ));
            }
            for file in manifest.files {
                filesystem::relative(&file.path)?;
                selected.insert(
                    file.path.clone(),
                    PlannedFile {
                        depot: depot.clone(),
                        file,
                    },
                );
            }
        }
        let mut old_files = BTreeMap::new();
        let mut reuse = HashMap::new();
        for manifest in old {
            for file in manifest.files {
                filesystem::relative(&file.path)?;
                if file.flags & (64 | 512) == 0 {
                    for chunk in &file.chunks {
                        reuse.entry(chunk.id).or_insert_with(|| ReusableChunk {
                            path: file.path.clone(),
                            offset: chunk.offset,
                            size: chunk.uncompressed_size,
                        });
                    }
                }
                old_files.insert(file.path.clone(), file);
            }
        }
        let mut bytes = 0u64;
        let mut download_bytes = 0u64;
        let mut install_scripts = false;
        for planned in selected.values() {
            let file = &planned.file;
            if file.flags & 4 != 0 {
                return Err(failure(format!(
                    "Encrypted depot file {} is unsupported",
                    file.path.display()
                )));
            }
            for ancestor in file
                .path
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                if let Some(parent) = selected.get(ancestor)
                    && parent.file.flags & 64 == 0
                {
                    return Err(failure(format!(
                        "Depot file has a non-directory ancestor: {}",
                        file.path.display()
                    )));
                }
            }
            if file.flags & 512 != 0 {
                Self::resolve_link(&selected, &file.path)?;
            } else if file.flags & 64 == 0 {
                let mut offset = 0u64;
                for chunk in &file.chunks {
                    if chunk.offset != offset || chunk.uncompressed_size == 0 {
                        return Err(failure(format!(
                            "Invalid chunk coverage in {}",
                            file.path.display()
                        )));
                    }
                    offset = offset
                        .checked_add(u64::from(chunk.uncompressed_size))
                        .ok_or_else(|| failure("Depot size overflow"))?;
                    download_bytes = download_bytes
                        .checked_add(u64::from(chunk.compressed_size))
                        .ok_or_else(|| failure("Depot size overflow"))?;
                }
                if offset != file.size || file.sha1.is_none() {
                    return Err(failure(format!(
                        "Incomplete content hash/coverage in {}",
                        file.path.display()
                    )));
                }
                bytes = bytes
                    .checked_add(file.size)
                    .ok_or_else(|| failure("Depot size overflow"))?;
            }
            install_scripts |= file.flags & 256 != 0;
        }
        Ok(Self {
            files: selected.into_values().collect(),
            old_files,
            reuse,
            bytes,
            download_bytes,
            install_scripts,
        })
    }

    fn resolve_link<'a>(
        files: &'a BTreeMap<PathBuf, PlannedFile>,
        link: &'a Path,
    ) -> Result<PathBuf> {
        let mut resolved = PathBuf::new();
        let mut pending: VecDeque<_> = link.components().collect();
        let mut expansions = 0usize;
        while let Some(component) = pending.pop_front() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !resolved.pop() {
                        return Err(failure("Depot symlink escapes install directory"));
                    }
                }
                Component::Normal(part) => {
                    resolved.push(part);
                    if let Some(entry) = files.get(&resolved) {
                        if entry.file.flags & 512 != 0 {
                            expansions += 1;
                            if expansions > 256 {
                                return Err(failure("Cyclic/excessively chained depot symlink"));
                            }
                            let target = entry
                                .file
                                .link_target
                                .as_ref()
                                .ok_or_else(|| failure("Symlink has no target"))?;
                            if target.is_absolute() || target.as_os_str().is_empty() {
                                return Err(failure("Unsafe depot symlink target"));
                            }
                            resolved.pop();
                            for part in target.components().rev() {
                                pending.push_front(part);
                            }
                        } else if !pending.is_empty() && entry.file.flags & 64 == 0 {
                            return Err(failure(
                                "Depot symlink target passes through a non-directory",
                            ));
                        }
                    } else if !pending.is_empty()
                        && !files
                            .keys()
                            .any(|path| path.starts_with(&resolved) && path != &resolved)
                    {
                        return Err(failure(
                            "Depot symlink target has an unavailable directory ancestor",
                        ));
                    }
                }
                _ => return Err(failure("Unsafe depot symlink target")),
            }
        }
        if resolved.as_os_str().is_empty()
            || files.contains_key(&resolved)
            || files
                .keys()
                .any(|p| p.starts_with(&resolved) && p != &resolved)
        {
            Ok(resolved)
        } else {
            Err(failure(format!(
                "Depot symlink target {} is not supplied by selected depots",
                resolved.display()
            )))
        }
    }
}

/// Install/update one app. Must run on a LocalSet when required by Connection.
/// No Steam process, helper executable, install script or prerequisite is run.
/// Once Committing is published cancellation is deferred until the durable
/// transaction has either completed or rolled back; do not abort this future.
/// `pause` holds the Downloading phase between chunks without ending the job.
pub async fn run(
    connection: Connection,
    request: InstallRequest,
    progress: mpsc::Sender<InstallProgress>,
    cancel: CancellationToken,
    pause: PauseGate,
) -> Result<InstalledGame> {
    let mut status = InstallProgress {
        app_id: request.plan.app_id,
        phase: InstallPhase::Preparing,
        completed_bytes: 0,
        total_bytes: request.plan.installed_bytes,
        downloaded_bytes: 0,
        total_download_bytes: request.plan.download_bytes,
        verify_bytes_done: 0,
        verify_bytes_total: 0,
        message: "Checking library, running processes and interrupted transactions".into(),
    };
    report(&progress, &status);
    let result = run_inner(connection, request, &progress, &cancel, &pause, &mut status).await;
    if result.is_err() && cancel.is_cancelled() && status.phase != InstallPhase::Committing {
        status.phase = InstallPhase::Cancelled;
        status.message =
            "Cancelled; prior installation is unchanged and verified staging data can resume"
                .into();
        report(&progress, &status);
    }
    result
}

async fn run_inner(
    connection: Connection,
    request: InstallRequest,
    progress: &mpsc::Sender<InstallProgress>,
    cancel: &CancellationToken,
    pause: &PauseGate,
    status: &mut InstallProgress,
) -> Result<InstalledGame> {
    check_cancel(cancel)?;
    let owner = connection.steam_id().steam64();
    let request_copy = request.clone();
    let session =
        Arc::new(blocking(move || transaction::Session::prepare(request_copy, owner)).await?);
    check_cancel(cancel)?;
    let client = ContentClient::new(connection)
        .await?
        .with_branch(&request.plan.branch);
    let mut manifests = Vec::new();
    for depot in &request.plan.depots {
        check_cancel(cancel)?;
        status.message = format!(
            "Reading depot {} manifest {}",
            depot.depot_id, depot.manifest_id
        );
        report(progress, status);
        manifests.push((depot.clone(), client.manifest(depot, cancel).await?));
    }
    let mut old = Vec::new();
    if let Some(existing) = &request.existing {
        for (&depot_id, &manifest_id) in &existing.installed_depots {
            check_cancel(cancel)?;
            if let Some((_, same)) = manifests
                .iter()
                .find(|(spec, _)| spec.depot_id == depot_id && spec.manifest_id == manifest_id)
            {
                old.push(same.clone());
            } else {
                let source_app_id = request
                    .plan
                    .depots
                    .iter()
                    .find(|d| d.depot_id == depot_id)
                    .map_or(request.plan.app_id, |d| d.source_app_id);
                old.push(
                    client
                        .manifest(
                            &DepotSpec {
                                depot_id,
                                manifest_id,
                                source_app_id,
                            },
                            cancel,
                        )
                        .await
                        .map_err(|e| {
                            failure(format!(
                                "Cannot safely classify existing depot {depot_id} files: {e}"
                            ))
                        })?,
                );
            }
        }
    }
    let layout = Arc::new(Layout::new(manifests, old)?);
    status.total_bytes = layout.bytes;
    status.total_download_bytes = layout.download_bytes;
    status.message = "Checking verified staging data, collisions and required disk space".into();
    report(progress, status);
    let s = Arc::clone(&session);
    let l = Arc::clone(&layout);
    let token = cancel.clone();
    blocking(move || s.preflight(&l, &token)).await?;
    check_cancel(cancel)?;
    status.phase = InstallPhase::Downloading;
    for (file_index, planned) in layout.files.iter().enumerate() {
        check_cancel(cancel)?;
        let file = &planned.file;
        if file.flags & (64 | 512) != 0 {
            continue;
        }
        let s = Arc::clone(&session);
        let l = Arc::clone(&layout);
        blocking(move || s.prepare_file(&l.files[file_index].file)).await?;
        let chunks = file.chunks.len();
        let mut last_report = std::time::Instant::now();
        let mut window_start = 0;
        while window_start < chunks {
            wait_while_paused(pause, cancel).await?;
            check_cancel(cancel)?;
            let window_end = (window_start + SCAN_WINDOW).min(chunks);
            // One blocking call decides, in parallel, which chunks of the window
            // are already valid; only the missing ones are downloaded.
            let (s, l, token) = (Arc::clone(&session), Arc::clone(&layout), cancel.clone());
            let reused = blocking(move || {
                s.reuse_scan(
                    &l.files[file_index].file,
                    window_start..window_end,
                    &l.reuse,
                    &token,
                )
            })
            .await?;
            let mut missing = Vec::new();
            for (offset, chunk) in file.chunks[window_start..window_end].iter().enumerate() {
                if reused[offset] {
                    status.total_download_bytes -= u64::from(chunk.compressed_size);
                    status.completed_bytes += u64::from(chunk.uncompressed_size);
                } else {
                    let charge =
                        2 * chunk.compressed_size as usize + chunk.uncompressed_size as usize;
                    missing.push((window_start + offset, charge));
                }
            }
            if last_report.elapsed() >= REUSE_REPORT_INTERVAL || missing.is_empty() {
                status.message = format!("Verified/reused {}", file.path.display());
                report(progress, status);
            }
            download::run(
                missing,
                cancel,
                pause,
                |chunk_index| {
                    let client = &client;
                    let token = cancel.clone();
                    let (s, l) = (Arc::clone(&session), Arc::clone(&layout));
                    async move {
                        let chunk = &file.chunks[chunk_index];
                        let bytes = client.chunk(&planned.depot, chunk, cancel).await?;
                        blocking(move || {
                            check_cancel(&token)?;
                            let file = &l.files[file_index].file;
                            s.write_downloaded_chunk(&file.path, &file.chunks[chunk_index], &bytes)
                        })
                        .await?;
                        Ok(chunk_index)
                    }
                },
                |chunk_index| {
                    let chunk = &file.chunks[chunk_index];
                    status.downloaded_bytes += u64::from(chunk.compressed_size);
                    status.completed_bytes += u64::from(chunk.uncompressed_size);
                    status.message = format!("Downloaded {}", file.path.display());
                    report(progress, status);
                },
            )
            .await?;
            last_report = std::time::Instant::now();
            window_start = window_end;
        }
    }
    status.phase = InstallPhase::Verifying;
    status.verify_bytes_done = 0;
    status.verify_bytes_total = layout.verify_bytes();
    let verify_files = layout.verify_files();
    status.message = format!("Verifying files 0/{verify_files}");
    report(progress, status);
    let verify = Arc::new(VerifyProgress::default());
    let (s, l, token, v) = (
        Arc::clone(&session),
        Arc::clone(&layout),
        cancel.clone(),
        Arc::clone(&verify),
    );
    // Whole-file hashing runs on worker threads; poll its counters so the UI
    // sees steady progress.
    let job = tokio::task::spawn_blocking(move || s.finish_stage(&l, &token, &v));
    tokio::pin!(job);
    let mut tick = tokio::time::interval(VERIFY_REPORT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            joined = &mut job => {
                joined.map_err(|e| failure(format!("Installer filesystem worker failed: {e}")))??;
                break;
            }
            _ = tick.tick() => {
                let (bytes, files, current) = verify.snapshot();
                status.verify_bytes_done = bytes.min(status.verify_bytes_total);
                status.message = format!("Verifying files {files}/{verify_files} · {current}");
                report(progress, status);
            }
        }
    }
    status.verify_bytes_done = status.verify_bytes_total;
    check_cancel(cancel)?;
    status.phase = InstallPhase::Committing;
    status.message = "Committing verified files; cancellation waits for commit/rollback".into();
    report(progress, status);
    let s = Arc::clone(&session);
    let l = Arc::clone(&layout);
    let (game, preserved) = blocking(move || s.commit(&l)).await?;
    status.phase = InstallPhase::Finished;
    status.completed_bytes = layout.bytes;
    status.message = "Installed and verified".into();
    if let Some(path) = preserved {
        status.message.push_str(&format!(
            "; modified originals preserved in {}",
            path.display()
        ));
    }
    if layout.install_scripts {
        status.message.push_str(
            "; depot install scripts/prerequisites were not executed (may require manual setup)",
        );
    }
    report(progress, status);
    Ok(game)
}

fn hash_bytes(bytes: &[u8]) -> [u8; 20] {
    Sha1::digest(bytes).into()
}
