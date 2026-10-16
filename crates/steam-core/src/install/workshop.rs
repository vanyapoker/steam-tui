//! Workshop (UGC) item installs: one SteamPipe depot manifest per item, written
//! to `steamapps/workshop/content/<app>/<item>/`. Like game installs, data is
//! verified before it is visible: chunks are written into a private staging
//! directory, every file is hashed, and only then is the item directory
//! replaced by an atomic rename. Nothing downloaded is executed.
//! Installed state lives in Valve's `steamapps/workshop/appworkshop_<app>.acf`.
use super::filesystem::{self as fsafe};
use super::transaction::encode_vdf;
use super::{PauseGate, Result, check_cancel, download, failure};
use crate::catalog::DepotSpec;
use crate::content::{ContentClient, DepotChunk, DepotManifest};
use crate::installed::{Node, read_vdf};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Bounds on one item so a hostile or broken manifest cannot exhaust the disk or fds.
const MAX_FILES: usize = 100_000;
const MAX_BYTES: u64 = 64 << 30;
const DIRECTORY: u32 = 64;
const SYMLINK: u32 = 512;
const EXECUTABLE: u32 = 32;

/// An item recorded as installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledItem {
    pub id: u64,
    pub manifest: u64,
    pub size: u64,
    /// Time the installed version was published (unix seconds).
    pub time_updated: u32,
    /// Latest version the service reported (0 = unknown); newer than `manifest` means an update is due.
    pub latest_manifest: u64,
    /// The account unsubscribed; the files are deleted the next time the app starts.
    pub remove_pending: bool,
}

/// A library's workshop directory for one app.
pub struct Store {
    steamapps: PathBuf,
    app_id: u32,
}

/// Serializes read-modify-write of the app's acf within this process.
static ACF: Mutex<()> = Mutex::new(());

impl Store {
    /// `steamapps` is the library's steamapps directory.
    pub fn new(steamapps: &Path, app_id: u32) -> Self {
        Self {
            steamapps: steamapps.to_owned(),
            app_id,
        }
    }

    pub fn content_dir(&self) -> PathBuf {
        self.steamapps
            .join("workshop/content")
            .join(self.app_id.to_string())
    }

    pub fn item_dir(&self, id: u64) -> PathBuf {
        self.content_dir().join(id.to_string())
    }

    fn acf(&self) -> PathBuf {
        self.steamapps
            .join("workshop")
            .join(format!("appworkshop_{}.acf", self.app_id))
    }

    fn read(&self) -> Result<Node> {
        match read_vdf(&self.acf()) {
            Ok(doc) => Ok(doc),
            Err(_) if !fsafe::exists(&self.acf())? => Ok(Node::default()),
            Err(error) => Err(error),
        }
    }

    /// Items the acf records as installed whose directory still exists.
    pub fn installed(&self) -> Result<BTreeMap<u64, InstalledItem>> {
        let _guard = ACF.lock().unwrap_or_else(|e| e.into_inner());
        let doc = self.read()?;
        let mut items = BTreeMap::new();
        let Some(app) = doc.child("appworkshop") else {
            return Ok(items);
        };
        let Some(installed) = app.child("workshopitemsinstalled") else {
            return Ok(items);
        };
        for (key, entry) in &installed.children {
            let (Ok(id), Some(manifest)) = (
                key.parse::<u64>(),
                entry.text("manifest").and_then(|m| m.parse::<u64>().ok()),
            ) else {
                continue;
            };
            if !self.item_dir(id).is_dir() {
                continue;
            }
            let details = app
                .child("workshopitemdetails")
                .and_then(|d| d.child(key));
            items.insert(
                id,
                InstalledItem {
                    id,
                    manifest,
                    size: entry.text("size").and_then(|v| v.parse().ok()).unwrap_or(0),
                    time_updated: entry
                        .text("timeupdated")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    latest_manifest: details
                        .and_then(|d| d.text("latest_manifest"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    remove_pending: details
                        .and_then(|d| d.text("removepending"))
                        .is_some_and(|v| v == "1"),
                },
            );
        }
        Ok(items)
    }

    fn update(&self, change: impl FnOnce(&mut Node)) -> Result<()> {
        let _guard = ACF.lock().unwrap_or_else(|e| e.into_inner());
        let mut doc = self.read()?;
        let app = doc.children.entry("appworkshop".into()).or_default();
        put(app, "appid", self.app_id.to_string());
        change(app);
        let installed = app
            .children
            .get("workshopitemsinstalled")
            .map(|n| {
                n.children
                    .values()
                    .filter_map(|e| e.text("size")?.parse::<u64>().ok())
                    .sum::<u64>()
            })
            .unwrap_or(0);
        put(app, "sizeondisk", installed.to_string());
        put(app, "needsupdate", "0".into());
        put(app, "needsdownload", "0".into());
        put(app, "timelastupdated", now().to_string());
        app.children
            .entry("timelastappran".into())
            .or_insert_with(|| scalar("0".into()));
        let path = self.acf();
        let parent = path.parent().ok_or_else(|| failure("acf has no parent"))?;
        fs::create_dir_all(parent)?;
        fsafe::atomic_write(&path, &encode_vdf(&doc)?)
    }

    /// Records a finished install (and the latest known version) for `id`.
    pub fn record(&self, item: &InstalledItem, subscriber: u32) -> Result<()> {
        self.update(|app| {
            let id = item.id.to_string();
            let entry = app
                .children
                .entry("workshopitemsinstalled".into())
                .or_default()
                .children
                .entry(id.clone())
                .or_default();
            put(entry, "size", item.size.to_string());
            put(entry, "timeupdated", item.time_updated.to_string());
            put(entry, "manifest", item.manifest.to_string());
            let details = app
                .children
                .entry("workshopitemdetails".into())
                .or_default()
                .children
                .entry(id)
                .or_default();
            put(details, "manifest", item.manifest.to_string());
            put(details, "timeupdated", item.time_updated.to_string());
            put(details, "timetouched", now().to_string());
            put(details, "subscribedby", subscriber.to_string());
            details.children.remove("removepending");
            put(details, "latest_timeupdated", item.time_updated.to_string());
            put(
                details,
                "latest_manifest",
                item.latest_manifest.max(item.manifest).to_string(),
            );
        })
    }

    /// Notes the latest published version of an installed item (an update is due when it differs).
    pub fn note_latest(&self, id: u64, manifest: u64, time_updated: u32) -> Result<()> {
        if !self.installed()?.contains_key(&id) {
            return Ok(());
        }
        self.update(|app| {
            let details = app
                .children
                .entry("workshopitemdetails".into())
                .or_default()
                .children
                .entry(id.to_string())
                .or_default();
            put(details, "latest_manifest", manifest.to_string());
            put(details, "latest_timeupdated", time_updated.to_string());
        })
    }

    /// Marks an installed item for deletion at the next start (it is unsubscribed).
    pub fn mark_remove(&self, id: u64) -> Result<()> {
        self.update(|app| {
            if let Some(details) = app
                .children
                .get_mut("workshopitemdetails")
                .and_then(|d| d.children.get_mut(&id.to_string()))
            {
                put(details, "removepending", "1".into());
            }
        })
    }

    /// Removes the item's files and its records.
    pub fn remove(&self, id: u64) -> Result<()> {
        let dir = self.item_dir(id);
        if fsafe::exists(&dir)? {
            let meta = fs::symlink_metadata(&dir)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(failure("Workshop item path is not a directory"));
            }
            fs::remove_dir_all(&dir)?;
        }
        self.update(|app| {
            let key = id.to_string();
            for table in ["workshopitemsinstalled", "workshopitemdetails"] {
                if let Some(node) = app.children.get_mut(table) {
                    node.children.remove(&key);
                }
            }
        })
    }

    fn staging(&self, id: u64) -> PathBuf {
        self.content_dir().join(format!(".partial-{id}"))
    }

    fn previous(&self, id: u64) -> PathBuf {
        self.content_dir().join(format!(".old-{id}"))
    }
}

fn scalar(value: String) -> Node {
    Node {
        value: Some(value),
        children: BTreeMap::new(),
    }
}

fn put(node: &mut Node, key: &str, value: String) {
    node.children.insert(key.into(), scalar(value));
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a manifest asks to write, validated.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub dirs: Vec<PathBuf>,
    /// (index into the manifest's files, relative path, size, executable)
    pub files: Vec<(usize, PathBuf, u64, bool)>,
    pub bytes: u64,
}

/// Validates a workshop manifest: relative paths only, no collisions between
/// files and directories, bounded counts and sizes, chunks inside their file.
pub(crate) fn plan(manifest: &DepotManifest) -> Result<Plan> {
    if manifest.files.len() > MAX_FILES {
        return Err(failure("Workshop item has too many files"));
    }
    let mut plan = Plan::default();
    let mut file_paths = HashSet::new();
    let mut dir_paths = BTreeSet::new();
    for (index, file) in manifest.files.iter().enumerate() {
        fsafe::relative(&file.path)?;
        if file.flags & SYMLINK != 0 {
            continue;
        }
        let mut ancestors = file.path.ancestors().skip(1);
        let is_dir = file.flags & DIRECTORY != 0;
        for ancestor in ancestors.by_ref() {
            if !ancestor.as_os_str().is_empty() {
                dir_paths.insert(ancestor.to_owned());
            }
        }
        if is_dir {
            dir_paths.insert(file.path.clone());
            continue;
        }
        if !file_paths.insert(file.path.clone()) {
            return Err(failure(format!(
                "Workshop item lists {} twice",
                file.path.display()
            )));
        }
        for chunk in &file.chunks {
            if chunk.offset + u64::from(chunk.uncompressed_size) > file.size {
                return Err(failure("Workshop item chunk lies outside its file"));
            }
        }
        plan.bytes = plan
            .bytes
            .checked_add(file.size)
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or_else(|| failure("Workshop item is too large"))?;
        plan.files.push((
            index,
            file.path.clone(),
            file.size,
            file.flags & EXECUTABLE != 0,
        ));
    }
    if let Some(clash) = file_paths.iter().find(|p| dir_paths.contains(*p)) {
        return Err(failure(format!(
            "Workshop item uses {} as both a file and a directory",
            clash.display()
        )));
    }
    plan.dirs = dir_paths.into_iter().collect();
    Ok(plan)
}

/// Replaces `dest` with the verified `staging` directory; on failure `dest` is restored.
fn swap(staging: &Path, dest: &Path, previous: &Path) -> Result<()> {
    if fsafe::exists(previous)? {
        fs::remove_dir_all(previous)?;
    }
    let had_old = fsafe::exists(dest)?;
    if had_old {
        fs::rename(dest, previous)?;
    }
    if let Err(error) = fs::rename(staging, dest) {
        if had_old {
            let _ = fs::rename(previous, dest);
        }
        return Err(error.into());
    }
    if had_old {
        let _ = fs::remove_dir_all(previous);
    }
    if let Some(parent) = dest.parent() {
        fsafe::sync_dir(parent)?;
    }
    Ok(())
}

fn clear(path: &Path) -> Result<()> {
    if fsafe::exists(path)? {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Downloads and installs one item; `progress(done, total)` reports verified bytes.
/// `latest_manifest` is the version being installed (the service's current one).
#[allow(clippy::too_many_arguments)]
pub async fn install(
    client: &ContentClient,
    store: &Store,
    id: u64,
    spec: &DepotSpec,
    time_updated: u32,
    subscriber: u32,
    cancel: &CancellationToken,
    pause: &PauseGate,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<InstalledItem> {
    check_cancel(cancel)?;
    let manifest = client.manifest(spec, cancel).await?;
    let plan = plan(&manifest)?;
    let content = store.content_dir();
    fs::create_dir_all(&content)?;
    fsafe::require_space(plan.bytes, fsafe::free_space(&content)?)?;
    let staging = store.staging(id);
    clear(&staging)?;
    fs::DirBuilder::new().mode(0o755).create(&staging)?;
    let result = stage(
        client, spec, &manifest, &plan, &staging, cancel, pause, progress,
    )
    .await;
    if let Err(error) = result {
        let _ = clear(&staging);
        return Err(error);
    }
    check_cancel(cancel).inspect_err(|_| {
        let _ = clear(&staging);
    })?;
    swap(&staging, &store.item_dir(id), &store.previous(id)).inspect_err(|_| {
        let _ = clear(&staging);
    })?;
    let item = InstalledItem {
        id,
        manifest: spec.manifest_id,
        size: plan.bytes,
        time_updated,
        latest_manifest: spec.manifest_id,
        remove_pending: false,
    };
    store.record(&item, subscriber)?;
    Ok(item)
}

/// The file name Valve's client gives a legacy (pre-SteamPipe) item inside its folder.
pub fn legacy_file_name(id: u64) -> String {
    format!("{id}_legacy.bin")
}

/// A legacy item's download URL: https on Valve's UGC CDN, no port or credentials.
fn legacy_url(url: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(url).map_err(|_| failure("Invalid Workshop download URL"))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !(host == "steamusercontent.com" || host.ends_with(".steamusercontent.com"))
    {
        return Err(failure("Workshop download is not on Valve's UGC CDN"));
    }
    Ok(url)
}

/// SHA-1 named by the last path segment of a legacy URL, when it is one.
fn legacy_hash(url: &reqwest::Url) -> Option<[u8; 20]> {
    let segment = url.path_segments()?.rfind(|s| !s.is_empty())?;
    if segment.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(segment.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Downloads a legacy item (one file) into `<item>/<id>_legacy.bin`.
#[allow(clippy::too_many_arguments)]
pub async fn install_legacy(
    store: &Store,
    id: u64,
    url: &str,
    size_hint: u64,
    time_updated: u32,
    subscriber: u32,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<InstalledItem> {
    use sha1::{Digest, Sha1};
    use std::io::Write;
    check_cancel(cancel)?;
    let url = legacy_url(url)?;
    let expected = legacy_hash(&url);
    let limit = if size_hint > 0 {
        size_hint.saturating_add(1 << 20).min(MAX_BYTES)
    } else {
        4 << 30
    };
    let content = store.content_dir();
    fs::create_dir_all(&content)?;
    fsafe::require_space(limit.min(size_hint.max(1 << 20)), fsafe::free_space(&content)?)?;
    let staging = store.staging(id);
    clear(&staging)?;
    fs::DirBuilder::new().mode(0o755).create(&staging)?;
    let target = staging.join(legacy_file_name(id));
    let result = async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(60))
            .user_agent("steam-tui/workshop")
            .build()
            .map_err(|_| failure("could not initialize HTTPS client"))?;
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(failure("Workshop download cancelled")),
            response = client.get(url.clone()).send() => response
                .map_err(|_| failure("Workshop download request failed"))?,
        };
        if !response.status().is_success() {
            return Err(failure(format!(
                "Workshop download answered {}",
                response.status()
            )));
        }
        let total = response.content_length().unwrap_or(size_hint);
        if total > limit {
            return Err(failure("Workshop download is larger than expected"));
        }
        let mut file = fsafe::regular(&target, true, true)?;
        let mut hash = Sha1::new();
        let mut done = 0u64;
        progress(0, total);
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(failure("Workshop download cancelled")),
                chunk = response.chunk() => chunk
                    .map_err(|_| failure("Workshop download interrupted"))?,
            };
            let Some(chunk) = chunk else { break };
            done += chunk.len() as u64;
            if done > limit {
                return Err(failure("Workshop download is larger than expected"));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .map_err(|e| fsafe::disk_error(&target, e))?;
            progress(done, total.max(done));
        }
        file.sync_all().map_err(|e| fsafe::disk_error(&target, e))?;
        if let Some(expected) = expected {
            let actual: [u8; 20] = hash.finalize().into();
            if actual != expected {
                return Err(failure("Workshop download failed verification"));
            }
        }
        Ok(done)
    }
    .await;
    let size = match result {
        Ok(size) => size,
        Err(error) => {
            let _ = clear(&staging);
            return Err(error);
        }
    };
    swap(&staging, &store.item_dir(id), &store.previous(id)).inspect_err(|_| {
        let _ = clear(&staging);
    })?;
    let item = InstalledItem {
        id,
        manifest: 0,
        size,
        time_updated,
        latest_manifest: 0,
        remove_pending: false,
    };
    store.record(&item, subscriber)?;
    Ok(item)
}

#[allow(clippy::too_many_arguments)]
async fn stage(
    client: &ContentClient,
    spec: &DepotSpec,
    manifest: &DepotManifest,
    plan: &Plan,
    staging: &Path,
    cancel: &CancellationToken,
    pause: &PauseGate,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    for dir in &plan.dirs {
        fs::create_dir_all(fsafe::checked(staging, dir, true)?)?;
    }
    for (_, path, size, executable) in &plan.files {
        let target = fsafe::checked(staging, path, false)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fsafe::regular(&target, true, true)?;
        file.set_len(*size)
            .map_err(|e| fsafe::disk_error(&target, e))?;
        if *executable {
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        }
    }
    let total = plan.bytes;
    let mut done = 0u64;
    progress(0, total);
    let jobs: Vec<((PathBuf, DepotChunk), usize)> = plan
        .files
        .iter()
        .flat_map(|(index, path, _, _)| {
            manifest.files[*index].chunks.iter().map(move |chunk| {
                (
                    (path.clone(), chunk.clone()),
                    chunk.compressed_size as usize + chunk.uncompressed_size as usize,
                )
            })
        })
        .collect();
    download::run(
        jobs,
        cancel,
        pause,
        |(path, chunk): (PathBuf, DepotChunk)| async move {
            let verified = client.chunk(spec, &chunk, cancel).await?;
            let bytes = verified.bytes_for(&chunk)?;
            let target = fsafe::checked(staging, &path, false)?;
            let file = fsafe::regular(&target, true, false)?;
            file.write_all_at(bytes, chunk.offset)
                .map_err(|e| fsafe::disk_error(&target, e))?;
            Ok(bytes.len() as u64)
        },
        |bytes| {
            done += bytes;
            progress(done, total);
        },
    )
    .await?;
    check_cancel(cancel)?;
    let verify: Vec<(PathBuf, u64, [u8; 20])> = plan
        .files
        .iter()
        .filter_map(|(index, path, size, _)| {
            manifest.files[*index]
                .sha1
                .map(|sha| (staging.join(path), *size, sha))
        })
        .collect();
    let token = cancel.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (path, size, sha) in verify {
            check_cancel(&token)?;
            if fsafe::hash_file(&path, size)? != Some(sha) {
                return Err(failure(format!(
                    "Workshop file {} failed verification",
                    path.display()
                )));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| failure(format!("Workshop verification worker failed: {e}")))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::DepotFile;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "steam-tui-workshop-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(path: &str, size: u64, flags: u32) -> DepotFile {
        DepotFile {
            path: path.into(),
            size,
            sha1: None,
            flags,
            chunks: Vec::new(),
            link_target: None,
        }
    }

    fn manifest(files: Vec<DepotFile>) -> DepotManifest {
        DepotManifest {
            depot_id: 4000,
            manifest_id: 1,
            files,
        }
    }

    #[test]
    fn plans_files_directories_and_skips_links() {
        let plan = plan(&manifest(vec![
            entry("a/b.txt", 10, 0),
            entry("run.sh", 5, EXECUTABLE),
            entry("empty", 0, DIRECTORY),
            entry("link", 0, SYMLINK),
        ]))
        .unwrap();
        assert_eq!(plan.bytes, 15);
        assert_eq!(plan.files.len(), 2);
        assert!(plan.files[1].3);
        assert_eq!(plan.dirs, vec![PathBuf::from("a"), PathBuf::from("empty")]);
    }

    #[test]
    fn rejects_unsafe_duplicate_and_colliding_paths() {
        for files in [
            vec![entry("../escape", 1, 0)],
            vec![entry("/abs", 1, 0)],
            vec![entry("a/../b", 1, 0)],
            vec![entry("", 1, 0)],
            vec![entry("x", 1, 0), entry("x", 1, 0)],
            vec![entry("x", 1, 0), entry("x/y", 1, 0)],
        ] {
            assert!(plan(&manifest(files)).is_err());
        }
    }

    #[test]
    fn bounds_counts_sizes_and_chunk_ranges() {
        let many = (0..MAX_FILES + 1)
            .map(|i| entry(&format!("f{i}"), 0, 0))
            .collect();
        assert!(plan(&manifest(many)).is_err());
        assert!(plan(&manifest(vec![entry("big", MAX_BYTES + 1, 0)])).is_err());
        let mut outside = entry("c", 4, 0);
        outside.chunks.push(DepotChunk {
            id: [0; 20],
            offset: 2,
            uncompressed_size: 8,
            compressed_size: 8,
            adler: 0,
        });
        assert!(plan(&manifest(vec![outside])).is_err());
    }

    #[test]
    fn legacy_urls_are_limited_to_the_ugc_cdn_and_carry_a_hash() {
        let hash = "990738E517E8A62D71A4FE2EB855AF4D0FE50B62";
        let ok = legacy_url(&format!("https://cdn.steamusercontent.com/ugc/540/{hash}/")).unwrap();
        assert_eq!(legacy_hash(&ok).unwrap()[0], 0x99);
        for bad in [
            "http://cdn.steamusercontent.com/ugc/1/2/",
            "https://evil.example/ugc/1/2/",
            "https://cdn.steamusercontent.com.evil.example/x",
            "https://cdn.steamusercontent.com:444/x",
            "https://u@cdn.steamusercontent.com/x",
        ] {
            assert!(legacy_url(bad).is_err(), "{bad}");
        }
        assert!(legacy_hash(&legacy_url("https://cdn.steamusercontent.com/ugc/1/zz/").unwrap()).is_none());
        assert_eq!(legacy_file_name(12), "12_legacy.bin");
    }

    #[test]
    fn records_installed_items_in_the_workshop_acf() {
        let root = Temp::new();
        let store = Store::new(&root.0, 4000);
        assert!(store.installed().unwrap().is_empty());
        fs::create_dir_all(store.item_dir(77)).unwrap();
        let item = InstalledItem {
            id: 77,
            manifest: 5,
            size: 123,
            time_updated: 9,
            latest_manifest: 5,
            remove_pending: false,
        };
        store.record(&item, 42).unwrap();
        assert_eq!(store.installed().unwrap().get(&77), Some(&item));
        store.note_latest(77, 6, 10).unwrap();
        assert_eq!(store.installed().unwrap()[&77].latest_manifest, 6);
        store.mark_remove(77).unwrap();
        assert!(store.installed().unwrap()[&77].remove_pending);
        store.record(&item, 42).unwrap();
        assert!(!store.installed().unwrap()[&77].remove_pending);
        let text = fs::read_to_string(root.0.join("workshop/appworkshop_4000.acf")).unwrap();
        assert!(text.contains("\"WorkshopItemsInstalled\"") || text.contains("workshopitemsinstalled"));
        assert!(text.contains("\"subscribedby\"") && text.contains("42"));
        // An item whose directory vanished is not reported as installed.
        fs::remove_dir_all(store.item_dir(77)).unwrap();
        assert!(store.installed().unwrap().is_empty());
        fs::create_dir_all(store.item_dir(77)).unwrap();
        store.record(&item, 42).unwrap();
        store.remove(77).unwrap();
        assert!(store.installed().unwrap().is_empty());
        assert!(!store.item_dir(77).exists());
    }

    #[test]
    fn swap_replaces_atomically_and_restores_on_failure() {
        let root = Temp::new();
        let store = Store::new(&root.0, 1);
        let content = store.content_dir();
        fs::create_dir_all(&content).unwrap();
        let dest = store.item_dir(5);
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("old"), "old").unwrap();
        let staging = store.staging(5);
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new"), "new").unwrap();
        swap(&staging, &dest, &store.previous(5)).unwrap();
        assert!(dest.join("new").exists() && !dest.join("old").exists());
        assert!(!store.previous(5).exists() && !staging.exists());
        // A missing staging directory restores the existing item.
        assert!(swap(&store.staging(5), &dest, &store.previous(5)).is_err());
        assert!(dest.join("new").exists());
    }
}
