//! Steam Cloud for direct launch, without the Steam client.
//!
//! Download sync runs before launch (`Mode::Down`); uploading is manual and runs
//! only with `Mode::Full` (download, then upload of local changes).
//!
//! Protocol (generated `steam-vent-proto-steam` types): `Cloud.GetAppFileChangelist`
//! lists files (names are `path_prefixes[path_prefix_index] + file_name`), then
//! `Cloud.ClientFileDownload` returns an HTTP location per file. Uploads use
//! `BeginAppUploadBatch`, `ClientBeginFileUpload` (+ HTTP blocks),
//! `ClientCommitFileUpload`, `ClientDeleteFile` and always
//! `CompleteAppUploadBatchBlocking`.
//!
//! Safety: nothing is deleted locally; a local file is overwritten only after a
//! backup copy exists; a file that changed locally and in the cloud is never
//! overwritten in either direction. All paths stay inside their root.
//!
//! Regression-tested with an injectable service; see docs/save-sync-review.md
//! for live verification limits.

mod conflict;
mod layout;
mod net;
mod state;
pub use conflict::{Choice, Conflict, SaveVersion, conflicts_game, resolve_game};
#[cfg(test)]
mod tests;

pub use layout::Os;
pub use net::LiveApi;

use crate::installed::{InstalledGame, Result, failure};
use layout::{LocalFile, Roots, Ufs, unix_seconds};
use sha1::Digest;
use state::{CloudEntry, FileRecord, State, sha1_hex, state_file};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use steam_vent::Connection;

/// Largest cloud file handled in either direction.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// Largest file uploaded (it is held in memory).
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;
const DOWN_TIMEOUT: Duration = Duration::from_secs(20);
const FULL_DOWN_TIMEOUT: Duration = Duration::from_secs(60);

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One file of the cloud listing, with its full `%Root%path` name.
#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub name: String,
    pub sha1: Option<String>,
    pub time: u64,
    pub size: u64,
    pub persisted: bool,
    pub deleted: bool,
    pub platforms: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Changelist {
    pub current: u64,
    /// Only changes since the requested number, not the full listing.
    pub delta: bool,
    pub files: Vec<RemoteFile>,
}

/// A downloaded file as served: possibly zip-compressed.
#[derive(Clone, Debug)]
pub struct Fetched {
    pub file_size: u64,
    pub raw_file_size: u64,
    pub sha1: Option<String>,
    pub time: u64,
    pub explicit_delete: bool,
    pub encrypted: bool,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Batch {
    pub id: u64,
    pub change_number: Option<u64>,
}

// Valve's recorded cloud log advances 14 -> batch 15 and 9 -> batch 10.
// Begin reserves the next change number; equality with the old listing is wrong.
fn batch_matches(batch: Batch, before: u64) -> bool {
    batch.change_number.is_some() && batch.change_number == before.checked_add(1)
}

pub struct UploadFile<'a> {
    pub app_id: u32,
    pub batch_id: u64,
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub sha1: [u8; 20],
    pub time: u64,
    pub platforms: u32,
}

/// The network side, injectable so the whole flow runs against temp directories.
#[allow(async_fn_in_trait)]
pub trait CloudApi {
    async fn changelist(&self, app_id: u32, since: u64) -> Result<Changelist>;
    async fn fetch(&self, app_id: u32, name: &str) -> Result<Fetched>;
    async fn begin_batch(
        &self,
        app_id: u32,
        upload: &[String],
        delete: &[String],
        build_id: u64,
    ) -> Result<Batch>;
    /// Transfers one file; `Ok(false)` means the server did not commit it.
    async fn upload(&self, file: &UploadFile<'_>) -> Result<bool>;
    async fn delete(&self, app_id: u32, batch_id: u64, name: &str) -> Result<()>;
    async fn complete_batch(&self, app_id: u32, batch_id: u64, success: bool) -> Result<()>;
    async fn recover_batch(&self, app_id: u32, batch_id: u64) -> Result<()> {
        self.complete_batch(app_id, batch_id, false).await
    }
}

/// An app with Steam Cloud configured, resolved for this machine.
pub struct CloudApp {
    pub app_id: u32,
    pub account: String,
    pub(crate) roots: Roots,
    pub(crate) ufs: Ufs,
    /// Holds `<account>-<app>.json` and `backup/<app>/…`.
    pub state_dir: PathBuf,
    pub build_id: u64,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn app_lock_at(directory: &Path, app_id: u32) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(format!("app-{app_id}.lock")))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(failure(
            "Steam Cloud sync is already running for this account and game",
        ));
    }
    Ok(lock)
}

pub(crate) fn launch_lease(app_id: u32) -> Result<fs::File> {
    let state = env_path("XDG_STATE_HOME")
        .or_else(|| env_path("HOME").map(|p| p.join(".local/state")))
        .ok_or_else(|| failure("HOME is not set"))?;
    app_lock_at(&state.join("steam-tui/cloud"), app_id)
}

impl CloudApp {
    /// Fail fast instead of letting two processes replace one another's baseline.
    fn lock(&self) -> Result<(fs::File, steam_ipc::storage::Lock)> {
        let lock = app_lock_at(&self.state_dir, self.app_id)?;
        let cache = steam_ipc::storage::Store::open(&self.roots.remote)?;
        Ok((lock, cache.lock()?))
    }

    /// `None` when the app has no `ufs` section (Steam Cloud not configured).
    pub fn for_game(game: &InstalledGame, account: &str, account_id: u32) -> Result<Option<Self>> {
        Self::for_game_with_backend(
            game,
            account,
            account_id,
            crate::launch::LaunchBackend::BuiltIn,
        )
    }

    /// Resolve save roots using the same immutable mode as the impending launch.
    pub fn for_game_with_backend(
        game: &InstalledGame,
        account: &str,
        account_id: u32,
        backend: crate::launch::LaunchBackend,
    ) -> Result<Option<Self>> {
        let document = crate::launch::app_document(game)?;
        let proton = crate::launch::uses_proton(
            game,
            &document,
            crate::launch::options::chosen(game.app_id),
        )
        .or_else(|error| match game.platform {
            Some(crate::catalog::InstallPlatform::Windows) => Ok(true),
            Some(crate::catalog::InstallPlatform::Linux) => Ok(false),
            None => Err(error),
        })?;
        let os = if proton { Os::Windows } else { Os::Linux };
        let Some(mut ufs) = Ufs::parse(&document, os) else {
            return Ok(None);
        };
        ufs.expand_account(account_id);
        let home = env_path("HOME").ok_or_else(|| failure("HOME is not set"))?;
        let state_dir = env_path("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("steam-tui/cloud");
        let roots = Roots {
            os,
            install_dir: game.install_dir.clone(),
            windows_user: game
                .library_root
                .join("steamapps/compatdata")
                .join(game.app_id.to_string())
                .join("pfx/drive_c/users/steamuser"),
            xdg_data: env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            xdg_config: env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            cloud_documents: cloud_documents(
                &home,
                account,
                document
                    .child("common")
                    .and_then(|n| n.text("name"))
                    .unwrap_or(&format!("App {}", game.app_id)),
            ),
            home: if !proton && backend == crate::launch::LaunchBackend::BuiltIn {
                crate::launch::native_home(game.app_id, account_id)?
            } else {
                home
            },
            remote: game
                .steam_root
                .join("userdata")
                .join(account_id.to_string())
                .join(game.app_id.to_string())
                .join("remote"),
        };
        Ok(Some(Self {
            app_id: game.app_id,
            account: account.to_owned(),
            roots,
            ufs,
            state_dir,
            build_id: game.build_id.unwrap_or(0),
        }))
    }

    fn state_path(&self) -> PathBuf {
        state_file(&self.state_dir, &self.account, self.app_id)
    }

    /// Backup directory for this run, created on first use.
    fn backup_dir<'a>(&self, run: &'a mut Option<PathBuf>) -> Result<&'a Path> {
        if run.is_none() {
            let stamp = format!(
                "{}-{}-{}",
                unix_seconds(SystemTime::now()),
                std::process::id(),
                RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let dir = self
                .state_dir
                .join("backup")
                .join(self.app_id.to_string())
                .join(stamp);
            fs::create_dir_all(&dir)?;
            *run = Some(dir);
        }
        Ok(run.as_deref().expect("set above"))
    }

    /// Saves `bytes` as the backup of `name`, under `<group>/<root>/<path>`.
    fn backup(
        &self,
        run: &mut Option<PathBuf>,
        group: &str,
        name: &str,
        bytes: &[u8],
        mtime: u64,
    ) -> Result<PathBuf> {
        let (token, rest) = layout::split_name(name).map_err(|e| failure(e.to_string()))?;
        let mut path = self.backup_dir(run)?.join(group);
        path.push(token.unwrap_or("remote"));
        for part in rest.split('/') {
            path.push(part);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        layout::write_atomic(&path, bytes, (mtime > 0).then_some(mtime))?;
        Ok(path)
    }
}

fn cloud_documents(home: &Path, account: &str, name: &str) -> Option<PathBuf> {
    let single = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains(['/', '\\', '\0'])
            && !s.chars().any(char::is_control)
    };
    (single(account) && single(name)).then(|| home.join(".SteamCloud").join(account).join(name))
}

/// What a sync did.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub cloud_files: usize,
    pub downloaded: Vec<String>,
    pub uploaded: Vec<String>,
    pub deleted_in_cloud: Vec<String>,
    pub deleted_locally: Vec<String>,
    pub unchanged: usize,
    /// Changed locally and in the cloud: left alone.
    pub conflicts: Vec<String>,
    /// Not handled: unsupported root, unsafe path, other platform.
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
    pub warnings: Vec<String>,
    pub backups: Vec<PathBuf>,
    /// Local saves that differ from the cloud and were not uploaded.
    pub pending_upload: usize,
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.downloaded.is_empty() {
            parts.push(format!(
                "downloaded {}",
                plural(self.downloaded.len(), "file")
            ));
        }
        if !self.uploaded.is_empty() {
            parts.push(format!("uploaded {}", plural(self.uploaded.len(), "file")));
        }
        if !self.deleted_in_cloud.is_empty() {
            parts.push(format!(
                "deleted {} in the cloud",
                plural(self.deleted_in_cloud.len(), "file")
            ));
        }
        if !self.deleted_locally.is_empty() {
            parts.push(format!(
                "removed {} locally (backed up)",
                plural(self.deleted_locally.len(), "file")
            ));
        }
        if !self.conflicts.is_empty() {
            parts.push(format!(
                "{} kept local (changed in both places)",
                plural(self.conflicts.len(), "conflict")
            ));
        }
        if !self.failed.is_empty() {
            parts.push(format!("{} failed", self.failed.len()));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("{} skipped", self.skipped.len()));
        }
        if !self.warnings.is_empty() {
            parts.push(format!(
                "{} warning(s): {}",
                self.warnings.len(),
                self.warnings[0]
            ));
        }
        if self.pending_upload > 0 {
            parts.push(format!(
                "{} not uploaded — press y",
                plural(self.pending_upload, "local save")
            ));
        }
        if parts.is_empty() {
            parts.push(
                if self.cloud_files == 0 {
                    "no cloud files"
                } else {
                    "up to date"
                }
                .into(),
            );
        }
        format!("Steam Cloud: {}", parts.join(" · "))
    }

    fn absorb(&mut self, other: SyncReport) {
        self.cloud_files = self.cloud_files.max(other.cloud_files);
        self.downloaded.extend(other.downloaded);
        self.uploaded.extend(other.uploaded);
        self.deleted_in_cloud.extend(other.deleted_in_cloud);
        self.deleted_locally.extend(other.deleted_locally);
        self.unchanged += other.unchanged;
        self.conflicts.extend(other.conflicts);
        self.conflicts.sort();
        self.conflicts.dedup();
        self.skipped.extend(other.skipped);
        self.failed.extend(other.failed);
        self.warnings.extend(other.warnings);
        self.backups.extend(other.backups);
        self.pending_upload = other.pending_upload;
    }
}

/// A local file's content fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Local {
    sha1: String,
    mtime: u64,
    size: u64,
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(failure("not a regular file"));
    }
    if meta.len() > cap {
        return Err(failure(format!("larger than the {cap}-byte cloud limit")));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(failure("grew past the cloud size limit while reading"));
    }
    Ok(bytes)
}

fn fingerprint(path: &Path) -> Result<(Local, Vec<u8>)> {
    let bytes = read_capped(path, MAX_FILE_BYTES)?;
    let mtime = fs::metadata(path)?.modified().map_or(0, unix_seconds);
    Ok((
        Local {
            sha1: sha1_hex(&bytes),
            mtime,
            size: bytes.len() as u64,
        },
        bytes,
    ))
}

#[derive(Debug, Eq, PartialEq)]
enum Down {
    Missing,
    Identical,
    Overwrite,
    LocalChange,
    Conflict,
}

/// Cloud file against the local one and what the last sync recorded.
fn decide_down(cloud: &CloudEntry, local: Option<&Local>, record: Option<&FileRecord>) -> Down {
    let Some(local) = local else {
        return Down::Missing;
    };
    if local.sha1 == cloud.sha1 {
        return Down::Identical;
    }
    match record {
        // Untouched since the last sync, so the cloud copy is the newer one.
        Some(record) if record.sha1 == local.sha1 => Down::Overwrite,
        Some(record) if record.sha1 == cloud.sha1 => Down::LocalChange,
        // Without a baseline, neither timestamp proves which save to discard.
        Some(_) | None => Down::Conflict,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Up {
    Unchanged,
    Upload,
    Conflict,
}

fn decide_up(local_sha: &str, record: Option<&FileRecord>, cloud: Option<&CloudEntry>) -> Up {
    if cloud.is_some_and(|cloud| cloud.sha1 == local_sha) {
        return Up::Unchanged;
    }
    match (record, cloud) {
        (Some(record), _) if record.sha1 == local_sha => Up::Unchanged,
        (Some(record), Some(cloud)) if cloud.sha1 != record.sha1 => Up::Conflict,
        (Some(_), _) => Up::Upload,
        (None, Some(_)) => Up::Conflict,
        (None, None) => Up::Upload,
    }
}

/// Whether adding `add_bytes` (replacing `replaces` bytes) and `add_files` new
/// files stays within the app's `ufs` quota.
fn within_quota(
    ufs: &Ufs,
    files: u64,
    bytes: u64,
    add_files: u64,
    add_bytes: u64,
    replaces: u64,
) -> bool {
    ufs.max_files.is_none_or(|max| files + add_files <= max)
        && ufs
            .quota
            .is_none_or(|quota| (bytes + add_bytes).saturating_sub(replaces) <= quota)
}

/// Turns a served download into the file's bytes: unzips when the served size
/// differs from the raw size, then requires the SHA1 the changelist promised.
pub fn decode_download(fetched: &Fetched, expected_sha1: &str) -> Result<Vec<u8>> {
    if fetched.encrypted || fetched.explicit_delete {
        return Err(failure("cloud download is encrypted or deleted"));
    }
    if fetched.raw_file_size > MAX_FILE_BYTES {
        return Err(failure("cloud file exceeds the size limit"));
    }
    if fetched.body.len() as u64 != fetched.file_size {
        return Err(failure("downloaded size differs from the announced size"));
    }
    let bytes = if fetched.file_size == fetched.raw_file_size {
        fetched.body.clone()
    } else {
        let mut archive = zip::ZipArchive::new(Cursor::new(&fetched.body))
            .map_err(|e| failure(format!("compressed cloud file is not a valid zip: {e}")))?;
        let entry = archive
            .by_index(0)
            .map_err(|e| failure(format!("compressed cloud file has no entry: {e}")))?;
        let mut bytes = Vec::new();
        entry
            .take(fetched.raw_file_size + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| failure(format!("cannot decompress cloud file: {e}")))?;
        bytes
    };
    if bytes.len() as u64 != fetched.raw_file_size {
        return Err(failure("decompressed size differs from the announced size"));
    }
    let actual = sha1_hex(&bytes);
    if actual != expected_sha1 || fetched.sha1.as_deref().is_some_and(|s| s != actual) {
        return Err(failure("SHA1 mismatch: refusing the downloaded file"));
    }
    Ok(bytes)
}

fn load_state(path: &Path) -> Result<State> {
    State::load(path).map_err(|error| {
        failure(format!(
            "cloud state unreadable ({error}); refusing sync until it is repaired"
        ))
    })
}

fn save_state(state: &State, path: &Path, report: &mut SyncReport) {
    if let Err(error) = state.save(path) {
        report
            .warnings
            .push(format!("cannot save cloud state: {error}"));
    }
}

/// Merges the server listing into the known one.
async fn refresh_listing<A: CloudApi>(api: &A, app: &CloudApp, state: &mut State) -> Result<()> {
    if let Some(batch) = state.pending_batch {
        // A previous process died or lost the acknowledgement. Explicitly close
        // the abandoned batch before reading authoritative server state.
        api.recover_batch(app.app_id, batch).await?;
        state.pending_batch = None;
        state.change_number = 0;
    }
    let list = api.changelist(app.app_id, state.change_number).await?;
    if !list.delta {
        let present: BTreeSet<_> = list.files.iter().map(|f| f.name.as_str()).collect();
        for name in state.files.keys() {
            if !present.contains(name.as_str()) {
                state.deleted_cloud.insert(name.clone());
            }
        }
    }
    if !list.delta || state.change_number == 0 {
        state.cloud.clear();
    }
    let bit = app.roots.os.platform_bit();
    for file in list.files {
        if file.deleted {
            state.deleted_cloud.insert(file.name.clone());
        } else {
            state.deleted_cloud.remove(&file.name);
        }
        let for_us = file.platforms.is_none_or(|mask| mask & bit != 0);
        match file.sha1 {
            Some(sha1) if file.persisted && for_us => {
                state.cloud.insert(
                    file.name,
                    CloudEntry {
                        sha1,
                        time: file.time,
                        size: file.size,
                    },
                );
            }
            _ => {
                state.cloud.remove(&file.name);
            }
        }
    }
    state.change_number = list.current;
    Ok(())
}

/// Steam Cloud state of an installed game with cloud configured, as shown in
/// the library table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudBadge {
    /// Cloud-capable but never synced on this machine (no state file).
    Never,
    /// Local saves match the last sync.
    Synced,
    /// Local saves changed since the last sync (a manual `C` would upload).
    Pending,
}

/// Pure badge derivation. Cheap path: a local file whose size and mtime equal
/// its sync record is unchanged without reading it; only files that differ by
/// stat are hashed (via `hash`, which may refuse with `None`).
fn derive_badge(
    state: Option<&State>,
    locals: &[LocalFile],
    hash: &mut dyn FnMut(&LocalFile) -> Option<String>,
) -> CloudBadge {
    let Some(state) = state else {
        return CloudBadge::Never;
    };
    for file in locals {
        let record = state.files.get(&file.name);
        if record.is_some_and(|r| {
            r.size == file.size
                && r.time == file.mtime
                && state
                    .cloud
                    .get(&file.name)
                    .is_some_and(|c| c.sha1 == r.sha1)
        }) {
            continue;
        }
        let unchanged = hash(file).is_some_and(|sha| {
            decide_up(&sha, record, state.cloud.get(&file.name)) == Up::Unchanged
        });
        if !unchanged {
            return CloudBadge::Pending;
        }
    }
    CloudBadge::Synced
}

/// Blocking: the badge for one installed game, `None` when it has no Steam Cloud
/// configuration. `account_id` is only known with a connection; without it the
/// unrooted `remote` directory is not looked at.
pub fn badge_for(
    game: &InstalledGame,
    account: &str,
    account_id: Option<u32>,
) -> Option<CloudBadge> {
    let app = CloudApp::for_game(game, account, account_id.unwrap_or(0))
        .ok()
        .flatten()?;
    let path = app.state_path();
    if !path.is_file() {
        return Some(CloudBadge::Never);
    }
    let state = State::load(&path).ok();
    let locals = layout::scan(&app.roots, &app.ufs, &mut Vec::new());
    Some(derive_badge(state.as_ref(), &locals, &mut |file| {
        read_capped(&file.path, MAX_FILE_BYTES)
            .ok()
            .map(|bytes| sha1_hex(&bytes))
    }))
}

fn count_pending(app: &CloudApp, state: &State, report: &mut SyncReport) -> usize {
    let metadata = steam_ipc::storage::Store::open(&app.roots.remote).and_then(|s| s.metadata());
    let Ok(metadata) = metadata else {
        return 0;
    };
    if !metadata.enabled {
        return 0;
    }
    layout::scan(&app.roots, &app.ufs, &mut report.warnings)
        .iter()
        .filter(|file| {
            !state.deleted_cloud.contains(&file.name)
                && (file.name.starts_with('%') || metadata.syncs(&file.name))
        })
        .filter(|file| {
            fingerprint(&file.path).is_ok_and(|(local, _)| {
                decide_up(
                    &local.sha1,
                    state.files.get(&file.name),
                    state.cloud.get(&file.name),
                ) == Up::Upload
            })
        })
        .count()
}

/// Carry only declared LinuxHome saves when changing launch modes. Original
/// copies remain, replacements require an unchanged baseline and a backup.
fn migrate_home(app: &CloudApp, state: &mut State, report: &mut SyncReport) -> Result<()> {
    if app.roots.os != Os::Linux {
        return Ok(());
    }
    let previous = state.home_root.clone().or_else(|| env_path("HOME"));
    let Some(previous) = previous.filter(|p| p != &app.roots.home) else {
        state.home_root = Some(app.roots.home.clone());
        return Ok(());
    };
    let mut old_roots = app.roots.clone();
    old_roots.home = previous.clone();
    let mut backup = None;
    let mut blocked = false;
    for file in layout::scan(&old_roots, &app.ufs, &mut report.warnings) {
        let old_target = old_roots
            .target(&app.ufs, &file.name)
            .map_err(|e| failure(e.to_string()))?;
        if !old_target.root_dir.starts_with(&previous) || !file.name.starts_with('%') {
            continue;
        }
        let target = app
            .roots
            .target(&app.ufs, &file.name)
            .map_err(|e| failure(e.to_string()))?;
        if old_target == target {
            continue;
        }
        let (source, bytes) = fingerprint(&file.path)?;
        let destination =
            layout::locate(&target, true)?.ok_or_else(|| failure("no migration destination"))?;
        if destination.exists() {
            let (local, old_bytes) = fingerprint(&destination)?;
            if local.sha1 == source.sha1 {
                continue;
            }
            let baseline = state.files.get(&file.name);
            if baseline.is_some_and(|r| r.sha1 == source.sha1) {
                continue;
            } // current copy is newer
            if baseline.is_none_or(|r| r.sha1 != local.sha1) {
                blocked = true;
                report.warnings.push(format!(
                    "{}: launch-mode saves differ; kept both at {} and {}",
                    file.name,
                    file.path.display(),
                    destination.display()
                ));
                continue;
            }
            report.backups.push(app.backup(
                &mut backup,
                "home-migration",
                &file.name,
                &old_bytes,
                local.mtime,
            )?);
        }
        layout::write_atomic(
            &destination,
            &bytes,
            (source.mtime > 0).then_some(source.mtime),
        )?;
    }
    if !blocked {
        state.home_root = Some(app.roots.home.clone());
    }
    Ok(())
}

/// Brings cloud files down. Local files are only overwritten after a backup,
/// and never when they changed since the last sync.
pub async fn sync_down<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    progress: &mut dyn FnMut(String),
) -> Result<SyncReport> {
    let _lock = app.lock()?;
    sync_down_locked(api, app, false, progress).await
}

async fn sync_down_locked<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    manual: bool,
    progress: &mut dyn FnMut(String),
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let path = app.state_path();
    let mut state = load_state(&path)?;
    let outcome = down(api, app, &mut state, &mut report, manual, progress).await;
    save_state(&state, &path, &mut report);
    outcome?;
    Ok(report)
}

async fn down<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    state: &mut State,
    report: &mut SyncReport,
    manual: bool,
    progress: &mut dyn FnMut(String),
) -> Result<()> {
    migrate_home(app, state, report)?;
    progress("Steam Cloud: checking files…".into());
    refresh_listing(api, app, state).await?;
    report.cloud_files = state.cloud.len();
    let metadata = steam_ipc::storage::Store::open(&app.roots.remote)?.metadata()?;
    if !metadata.enabled {
        report.warnings.push("cloud disabled by the game".into());
        return Ok(());
    }
    let mut deletion_backups = None;
    for name in state.deleted_cloud.clone() {
        let target = match app.roots.target(&app.ufs, &name) {
            Ok(t) => t,
            Err(error) => {
                report.skipped.push(format!("{name}: {error}"));
                continue;
            }
        };
        let Some(path) = layout::locate(&target, false)? else {
            state.files.remove(&name);
            state.deleted_cloud.remove(&name);
            continue;
        };
        let (local, bytes) = fingerprint(&path)?;
        if state.files.get(&name).is_none_or(|r| r.sha1 != local.sha1) {
            report.conflicts.push(name);
            continue;
        }
        report.backups.push(app.backup(
            &mut deletion_backups,
            "cloud-deleted",
            &name,
            &bytes,
            local.mtime,
        )?);
        fs::remove_file(&path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        state.files.remove(&name);
        state.deleted_cloud.remove(&name);
        report.deleted_locally.push(name);
    }
    struct Planned {
        name: String,
        entry: CloudEntry,
        target: layout::Target,
        decision: Down,
        local: Option<Local>,
    }
    let mut planned = Vec::new();
    for (name, entry) in state.cloud.clone() {
        if !name.starts_with('%') && !metadata.syncs(&name) {
            continue;
        }
        let target = match app.roots.target(&app.ufs, &name) {
            Ok(target) => target,
            Err(error) => {
                report.skipped.push(format!("{name}: {error}"));
                continue;
            }
        };
        let local = match layout::locate(&target, false) {
            Ok(Some(path)) => match fingerprint(&path) {
                Ok((local, _)) => Some(local),
                Err(error) => {
                    report.skipped.push(format!("{name}: local file {error}"));
                    continue;
                }
            },
            Ok(None) => None,
            Err(error) => {
                report.skipped.push(format!("{name}: {error}"));
                continue;
            }
        };
        let decision = if manual && local.is_none() && state.files.contains_key(&name) {
            // A plain-file deletion needs an explicit choice, since a missing
            // prefix or disconnected disk is indistinguishable from intention.
            Down::Conflict
        } else {
            decide_down(&entry, local.as_ref(), state.files.get(&name))
        };
        planned.push(Planned {
            name,
            entry,
            target,
            decision,
            local,
        });
    }
    let wanted = planned
        .iter()
        .filter(|p| matches!(p.decision, Down::Missing | Down::Overwrite))
        .count();
    if wanted > 0 {
        progress(format!(
            "Steam Cloud: downloading {}…",
            plural(wanted, "file")
        ));
    }
    let mut backups = None;
    for item in planned {
        let record = FileRecord {
            sha1: item.entry.sha1.clone(),
            time: item.entry.time,
            size: item.entry.size,
        };
        match item.decision {
            Down::Identical => {
                state.files.insert(item.name, record);
                report.unchanged += 1;
            }
            Down::Conflict => report.conflicts.push(item.name),
            Down::LocalChange => {}
            Down::Missing | Down::Overwrite => {
                match download_one(
                    api,
                    app,
                    &item.name,
                    &item.entry,
                    &item.target,
                    item.local.as_ref(),
                    &mut backups,
                )
                .await
                {
                    Ok(backup) => {
                        report.backups.extend(backup);
                        state.files.insert(item.name.clone(), record);
                        report.downloaded.push(item.name);
                    }
                    Err(error) => report.failed.push(format!("{}: {error}", item.name)),
                }
            }
        }
    }
    report.pending_upload = count_pending(app, state, report);
    Ok(())
}

async fn download_one<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    name: &str,
    entry: &CloudEntry,
    target: &layout::Target,
    expected_local: Option<&Local>,
    backups: &mut Option<PathBuf>,
) -> Result<Option<PathBuf>> {
    let fetched = api.fetch(app.app_id, name).await?;
    if fetched.explicit_delete {
        return Err(failure("deleted in the cloud; local file left alone"));
    }
    if fetched.encrypted {
        return Err(failure("encrypted cloud files are not supported"));
    }
    let bytes = decode_download(&fetched, &entry.sha1)?;
    let destination = layout::locate(target, true)?.ok_or_else(|| failure("no destination"))?;
    // fetch() yielded: a game or another program may have written a save since
    // planning. Recheck immediately before backing up and replacing it.
    let current = layout::locate(target, false)?
        .map(|path| fingerprint(&path))
        .transpose()?;
    if current.as_ref().map(|(local, _)| &local.sha1) != expected_local.map(|local| &local.sha1) {
        return Err(failure(
            "local save changed during download; left untouched",
        ));
    }
    let backup = current
        .map(|(local, old)| app.backup(backups, "overwritten", name, &old, local.mtime))
        .transpose()?;
    layout::write_atomic(&destination, &bytes, (entry.time > 0).then_some(entry.time))?;
    Ok(backup)
}

/// Uploads local changes. The batch is always completed, with a failure result
/// when anything went wrong. `allow_deletes` propagates local deletions of
/// previously synced files to the cloud.
pub async fn sync_up<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    allow_deletes: bool,
    progress: &mut dyn FnMut(String),
) -> Result<SyncReport> {
    let _lock = app.lock()?;
    sync_up_locked(api, app, allow_deletes, progress).await
}

async fn sync_up_locked<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    allow_deletes: bool,
    progress: &mut dyn FnMut(String),
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let path = app.state_path();
    let mut state = load_state(&path)?;
    let outcome = up(api, app, &mut state, &mut report, allow_deletes, progress).await;
    save_state(&state, &path, &mut report);
    outcome?;
    Ok(report)
}

async fn up<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    state: &mut State,
    report: &mut SyncReport,
    allow_deletes: bool,
    progress: &mut dyn FnMut(String),
) -> Result<()> {
    migrate_home(app, state, report)?;
    progress("Steam Cloud: checking local saves…".into());
    refresh_listing(api, app, state).await?;
    report.cloud_files = state.cloud.len();
    let locals = layout::scan(&app.roots, &app.ufs, &mut report.warnings);
    let metadata = steam_ipc::storage::Store::open(&app.roots.remote)?.metadata()?;
    if !metadata.enabled {
        report.warnings.push("cloud disabled by the game".into());
        return Ok(());
    }
    let local_names: BTreeSet<&str> = locals.iter().map(|f| f.name.as_str()).collect();
    let mut candidates: Vec<(&LocalFile, String)> = Vec::new();
    let mut conflicts = Vec::new();
    for file in &locals {
        if state.deleted_cloud.contains(&file.name) {
            conflicts.push(file.name.clone());
            continue;
        }
        if !file.name.starts_with('%') && !metadata.syncs(&file.name) {
            continue;
        }
        if file.size > MAX_UPLOAD_BYTES {
            report
                .skipped
                .push(format!("{}: larger than the upload limit", file.name));
            continue;
        }
        let (local, _) = match fingerprint(&file.path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                report.skipped.push(format!("{}: {error}", file.name));
                continue;
            }
        };
        let cloud = state.cloud.get(&file.name);
        match decide_up(&local.sha1, state.files.get(&file.name), cloud) {
            Up::Unchanged => {
                if let Some(cloud) = cloud.filter(|c| c.sha1 == local.sha1) {
                    if !file.name.starts_with('%') && metadata.dirty.contains(&file.name) {
                        let store = steam_ipc::storage::Store::open(&app.roots.remote)?;
                        let mut meta = store.metadata()?;
                        meta.dirty.remove(&file.name);
                        store.save_metadata(&meta)?;
                    }
                    state.files.insert(
                        file.name.clone(),
                        FileRecord {
                            sha1: cloud.sha1.clone(),
                            time: cloud.time,
                            size: cloud.size,
                        },
                    );
                }
                report.unchanged += 1;
            }
            Up::Upload => candidates.push((file, local.sha1)),
            Up::Conflict => conflicts.push(file.name.clone()),
        }
    }
    // Quota: counted against the known cloud listing, in name order.
    let mut files = state.cloud.len() as u64;
    let mut bytes: u64 = state.cloud.values().map(|e| e.size).sum();
    let mut uploads: Vec<(&LocalFile, String)> = Vec::new();
    for (file, sha) in candidates {
        let existing = state.cloud.get(&file.name).map(|e| e.size);
        let (add_files, replaces) = (u64::from(existing.is_none()), existing.unwrap_or(0));
        if within_quota(&app.ufs, files, bytes, add_files, file.size, replaces) {
            files += add_files;
            bytes = (bytes + file.size).saturating_sub(replaces);
            uploads.push((file, sha));
        } else {
            report
                .skipped
                .push(format!("{}: would exceed the Steam Cloud quota", file.name));
        }
    }
    let mut deletes = Vec::new();
    // Explicit game API deletes/forget are durable intents. Auto-Cloud local
    // deletions remain opt-in; a changed remote copy is always a conflict.
    for name in metadata.deleted.union(&metadata.forgotten) {
        if let Some(cloud) = state.cloud.get(name) {
            if state.files.get(name).is_some_and(|r| r.sha1 == cloud.sha1) {
                deletes.push(name.clone());
            } else {
                conflicts.push(name.clone());
            }
        }
    }
    if allow_deletes {
        for (name, record) in &state.files {
            if local_names.contains(name.as_str())
                || state.cloud.get(name).is_none_or(|c| c.sha1 != record.sha1)
            {
                continue;
            }
            let gone = app
                .roots
                .target(&app.ufs, name)
                .ok()
                .is_some_and(|t| layout::locate(&t, false).is_ok_and(|p| p.is_none()));
            if gone {
                if !deletes.contains(name) {
                    deletes.push(name.clone());
                }
            }
        }
    }
    // Both sides changed: keep the local file, keep a copy of the cloud one.
    let mut backups = None;
    let mut verified_deletes = Vec::new();
    for name in deletes {
        let saved = async {
            let entry = state
                .cloud
                .get(&name)
                .ok_or_else(|| failure("cloud deletion has no baseline"))?;
            let fetched = api.fetch(app.app_id, &name).await?;
            let bytes = decode_download(&fetched, &entry.sha1)?;
            app.backup(&mut backups, "deleted-in-cloud", &name, &bytes, entry.time)
        }
        .await;
        match saved {
            Ok(path) => {
                report.backups.push(path);
                verified_deletes.push(name);
            }
            Err(error) => report.failed.push(format!(
                "{name}: cannot back up before cloud deletion: {error}"
            )),
        }
    }
    let deletes = verified_deletes;
    for name in &conflicts {
        report.conflicts.push(name.clone());
        if !state.cloud.contains_key(name) {
            continue;
        }
        let saved = async {
            let entry = state
                .cloud
                .get(name)
                .ok_or_else(|| failure("no cloud entry"))?;
            let fetched = api.fetch(app.app_id, name).await?;
            let old = decode_download(&fetched, &entry.sha1)?;
            app.backup(&mut backups, "cloud-conflict", name, &old, entry.time)
        }
        .await;
        match saved {
            Ok(path) => report.backups.push(path),
            Err(error) => report
                .warnings
                .push(format!("{name}: cannot back up the cloud copy: {error}")),
        }
    }
    if uploads.is_empty() && deletes.is_empty() {
        return Ok(());
    }
    progress(format!(
        "Steam Cloud: uploading {}…",
        plural(uploads.len() + deletes.len(), "file")
    ));
    let names: Vec<String> = uploads.iter().map(|(f, _)| f.name.clone()).collect();
    let batch = api
        .begin_batch(app.app_id, &names, &deletes, app.build_id)
        .await?;
    if !batch_matches(batch, state.change_number) {
        api.complete_batch(app.app_id, batch.id, false).await?;
        state.change_number = 0;
        return Err(failure(
            "cloud changed while preparing upload; batch cancelled, sync again",
        ));
    }
    let before_batch = state.clone();
    state.pending_batch = Some(batch.id);
    if let Err(error) = state.save(&app.state_path()) {
        api.complete_batch(app.app_id, batch.id, false).await?;
        state.pending_batch = None;
        return Err(error);
    }
    let mut ok = true;
    for (file, sha) in &uploads {
        match upload_one(api, app, batch, file, sha).await {
            Ok(Some(record)) => {
                state.cloud.insert(
                    file.name.clone(),
                    CloudEntry {
                        sha1: record.sha1.clone(),
                        time: record.time,
                        size: record.size,
                    },
                );
                state.files.insert(file.name.clone(), record);
                report.uploaded.push(file.name.clone());
            }
            Ok(None) => {
                ok = false;
                report
                    .failed
                    .push(format!("{}: not committed by Steam", file.name));
            }
            Err(error) => {
                ok = false;
                report.failed.push(format!("{}: {error}", file.name));
            }
        }
    }
    for name in &deletes {
        match api.delete(app.app_id, batch.id, name).await {
            Ok(()) => {
                state.cloud.remove(name);
                state.files.remove(name);
                report.deleted_in_cloud.push(name.clone());
            }
            Err(error) => {
                ok = false;
                report
                    .failed
                    .push(format!("{name}: delete failed: {error}"));
            }
        }
    }
    // Never leave the batch open, whatever happened above.
    if let Err(error) = api.complete_batch(app.app_id, batch.id, ok).await {
        // Per-file commits alone do not establish a completed batch. Keep the
        // previous baseline and force a full server listing on the next retry.
        *state = before_batch;
        state.pending_batch = Some(batch.id);
        state.change_number = 0;
        for name in report
            .uploaded
            .drain(..)
            .chain(report.deleted_in_cloud.drain(..))
        {
            report
                .failed
                .push(format!("{name}: upload batch completion unconfirmed"));
        }
        report
            .warnings
            .push(format!("cannot complete the upload batch: {error}"));
    } else {
        state.pending_batch = None;
        let store = steam_ipc::storage::Store::open(&app.roots.remote)?;
        let mut meta = store.metadata()?;
        for name in &report.uploaded {
            meta.dirty.remove(name);
        }
        store.save_metadata(&meta)?;
    }
    Ok(())
}

async fn upload_one<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    batch: Batch,
    file: &LocalFile,
    sha1: &str,
) -> Result<Option<FileRecord>> {
    let bytes = read_capped(&file.path, MAX_UPLOAD_BYTES)?;
    if sha1_hex(&bytes) != sha1 {
        return Err(failure("changed while it was being read; try again"));
    }
    let digest: [u8; 20] = sha1::Sha1::digest(&bytes).into();
    let time = fs::metadata(&file.path)?.modified().map_or(0, unix_seconds);
    let committed = api
        .upload(&UploadFile {
            app_id: app.app_id,
            batch_id: batch.id,
            name: &file.name,
            bytes: &bytes,
            sha1: digest,
            time,
            platforms: if file.name.starts_with('%') {
                u32::MAX
            } else {
                steam_ipc::storage::Store::open(&app.roots.remote)?
                    .metadata()?
                    .platforms
                    .get(&file.name)
                    .copied()
                    .unwrap_or(u32::MAX)
            },
        })
        .await?;
    Ok(committed.then(|| FileRecord {
        sha1: sha1.to_owned(),
        time,
        size: bytes.len() as u64,
    }))
}

/// Download, then upload local changes.
pub async fn sync_full<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    progress: &mut dyn FnMut(String),
) -> Result<SyncReport> {
    let _lock = app.lock()?;
    let mut report = sync_down_locked(api, app, true, progress).await?;
    report.absorb(sync_up_locked(api, app, false, progress).await?);
    Ok(report)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Before launch: download only.
    Down,
    /// The manual key: download, then upload.
    Full,
}

/// Syncs one installed game through the live connection. `None` when the game
/// has no Steam Cloud configuration. The download phase is bounded (20 s before
/// launch, 60 s for the manual sync); the upload phase is bounded only by the
/// per-request timeouts, because cancelling it would leave its batch open.
pub async fn sync_game(
    connection: &Connection,
    game: &InstalledGame,
    account: &str,
    mode: Mode,
    progress: &mut dyn FnMut(String),
) -> Result<Option<SyncReport>> {
    sync_game_with_backend(
        connection,
        game,
        account,
        mode,
        crate::launch::LaunchBackend::BuiltIn,
        progress,
    )
    .await
}

pub async fn sync_game_with_backend(
    connection: &Connection,
    game: &InstalledGame,
    account: &str,
    mode: Mode,
    backend: crate::launch::LaunchBackend,
    progress: &mut dyn FnMut(String),
) -> Result<Option<SyncReport>> {
    let (owned, name, id) = (
        game.clone(),
        account.to_owned(),
        connection.steam_id().account_id(),
    );
    let app = tokio::task::spawn_blocking(move || {
        CloudApp::for_game_with_backend(&owned, &name, id, backend)
    })
    .await
    .map_err(|e| failure(format!("cloud worker failed: {e}")))??;
    let Some(app) = app else {
        return Ok(None);
    };
    let api = LiveApi::new(connection.clone())?;
    let _lock = app.lock()?;
    if crate::launch::is_running(app.app_id) {
        return Err(failure("close the game before syncing Steam Cloud"));
    }
    let limit = if mode == Mode::Down {
        DOWN_TIMEOUT
    } else {
        FULL_DOWN_TIMEOUT
    };
    let mut report = tokio::time::timeout(
        limit,
        sync_down_locked(&api, &app, mode == Mode::Full, progress),
    )
    .await
    .map_err(|_| {
        failure(format!(
            "Steam Cloud download timed out after {}s",
            limit.as_secs()
        ))
    })??;
    if mode == Mode::Full {
        report.absorb(sync_up_locked(&api, &app, false, progress).await?);
    }
    Ok(Some(report))
}
