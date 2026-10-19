//! Independent downloaded Vulkan pipeline caches. SteamSwarm/G7 is the original
//! GPU-independent FOZ v6 bucket (016f2000/016f28d0). 016fb970..016fba31 passes
//! appid as BOTH app and depot ID to ContentUpdateContext; bucket IDs remain u64
//! metadata, never cast to depot IDs. Confirmed against live authenticated CM
//! and a verified two-file depot manifest, without starting Valve's client.
mod device;
mod replay;
#[cfg(test)]
mod tests;
use crate::catalog::DepotSpec;
use crate::content::{ContentClient, DepotFile, DepotManifest};
use crate::install::filesystem as safe;
use crate::install::{InstallPhase, InstallProgress, PauseGate};
use crate::installed::{InstalledGame, Result, failure};
use crate::updates::Queue;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_shader_steamclient::CShader_GetBucketManifest_Request;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
const ARCHIVE: &str = "fozpipelinesv6/steam_pipeline_cache.foz";
const MAX_ARCHIVE: u64 = 8 * 1024 * 1024 * 1024;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub bucket: u64,
    pub manifest: u64,
    pub size: u64,
    pub checked_at: u64,
    pub sha1: Option<[u8; 20]>,
    pub processed: Option<String>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn root(queue: &Queue, app: u32) -> Result<PathBuf> {
    if app == 0 {
        return Err(failure("Invalid shader app"));
    }
    let root = queue.state_directory().join("shaders");
    safe::private_directory(&root)?;
    let root = root.join(app.to_string());
    safe::private_directory(&root)?;
    Ok(root)
}
fn load(root: &Path) -> Result<State> {
    let path = safe::checked(root, Path::new("state.json"), true)?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
        Ok(m) if !m.is_file() || m.len() > 8192 => Err(failure("Invalid shader state")),
        Ok(_) => Ok(serde_json::from_slice(&crate::installed::read_bounded(
            &path, 8192,
        )?)?),
    }
}
fn save(root: &Path, state: &State) -> Result<()> {
    safe::atomic_write(&root.join("state.json"), &serde_json::to_vec(state)?)
}
pub fn state(queue: &Queue, app: u32) -> Result<State> {
    load(&root(queue, app)?)
}
pub fn due(queue: &Queue, app: u32) -> Result<bool> {
    let s = state(queue, app)?;
    Ok(now().saturating_sub(s.checked_at) >= 3600 || s.checked_at > now())
}
fn open(path: &Path, create: bool) -> Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure("Shader cache must be a regular file"));
    }
    Ok(file)
}
fn hash(path: &Path, size: u64) -> Result<Option<[u8; 20]>> {
    let mut file = open(path, false)?;
    if file.metadata()?.len() != size {
        return Ok(None);
    }
    let mut digest = Sha1::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(Some(digest.finalize().into()))
}
fn archive(manifest: &DepotManifest, advertised: u64) -> Result<&DepotFile> {
    if manifest.files.iter().any(|f| {
        f.link_target.is_some()
            || !(f.path == Path::new("fozpipelinesv6") && f.flags & 64 != 0
                || f.path == Path::new(ARCHIVE) && f.flags & 64 == 0)
    }) {
        return Err(failure("Unexpected file in Vulkan shader bucket"));
    }
    let file = manifest
        .files
        .iter()
        .find(|f| f.path == Path::new(ARCHIVE))
        .ok_or_else(|| failure("Shader bucket lacks FOZ archive"))?;
    if file.size == 0 || file.size > MAX_ARCHIVE || file.size != advertised || file.sha1.is_none() {
        return Err(failure("Shader bucket size/hash outside expected bounds"));
    }
    Ok(file)
}
fn report(
    tx: &mpsc::Sender<InstallProgress>,
    app: u32,
    done: u64,
    total: u64,
    wire: u64,
    message: &str,
) {
    let _ = tx.try_send(InstallProgress {
        app_id: app,
        phase: InstallPhase::Downloading,
        completed_bytes: done,
        total_bytes: total,
        downloaded_bytes: wire,
        total_download_bytes: total,
        verify_bytes_done: 0,
        verify_bytes_total: 0,
        message: message.into(),
    });
}
/// Download-only job: fresh CM authority, bounded chunks, verified resume and
/// atomic state commit. A game update is never inferred from shader freshness.
pub async fn download(
    queue: &Queue,
    connection: Connection,
    game: &InstalledGame,
    tx: mpsc::Sender<InstallProgress>,
    cancel: CancellationToken,
    pause: PauseGate,
) -> Result<()> {
    let root = root(queue, game.app_id)?;
    let lock_path = root.join("cache.lock");
    let _lease = safe::Lock::try_acquire(&lock_path)?
        .ok_or_else(|| failure("Shader cache is busy; retry this job"))?;
    if crate::launch::is_running(game.app_id) {
        return Err(failure("Cannot update a running game's shader cache"));
    }
    let bucket = tokio::select! {
        _ = cancel.cancelled() => return Err(failure("Shader download cancelled")),
        value = tokio::time::timeout(Duration::from_secs(60), connection.service_method(CShader_GetBucketManifest_Request { appid: Some(game.app_id), gpu_desc: Some("SteamSwarm".into()), driver_desc: Some("G7:VulkanPipelinesV6".into()), ..Default::default() })) => value.map_err(|_| failure("Shader metadata request timed out"))?.map_err(|_| failure("Shader service denied bucket metadata"))?,
    };
    let mut previous = load(&root)?;
    if bucket.manifestid() == 0 {
        // No bucket is a successful check, not a processed-cache claim.
        previous = State {
            checked_at: now(),
            ..Default::default()
        };
        save(&root, &previous)?;
        prune_locked(&root, &previous, false)?;
        return Ok(());
    }
    let target = root.join(format!("{}.foz", bucket.manifestid()));
    if previous.manifest == bucket.manifestid()
        && previous.bucket == bucket.bucketid()
        && previous.size == bucket.depotsize()
        && previous.sha1.is_some()
        && target.is_file()
    {
        let p = target.clone();
        let size = previous.size;
        if tokio::task::spawn_blocking(move || hash(&p, size))
            .await
            .map_err(|_| failure("Shader verification worker failed"))??
            == previous.sha1
        {
            previous.checked_at = now();
            save(&root, &previous)?;
            return Ok(());
        }
    }
    let spec = DepotSpec {
        depot_id: game.app_id,
        source_app_id: game.app_id,
        manifest_id: bucket.manifestid(),
    };
    let client = ContentClient::new(connection).await?;
    let manifest = client.manifest(&spec, &cancel).await?;
    let file = archive(&manifest, bucket.depotsize())?;
    let staging = root.join(format!("{}.part", bucket.manifestid()));
    // A changed bucket can retire an interrupted older generation. Never
    // remove its committed archive before the replacement has verified.
    for entry in fs::read_dir(&root)?.take(8192) {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path() != staging
            && entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".part"))
                .and_then(|s| s.parse::<u64>().ok())
                .is_some()
        {
            fs::remove_file(entry.path())?;
        }
    }
    let parent = root.parent().unwrap().to_owned();
    let target_staging = staging.clone();
    let size = file.size;
    let budget = queue.settings()?.shader_archive_budget;
    tokio::task::spawn_blocking(move || reserve(&parent, &target_staging, size, budget))
        .await
        .map_err(|_| failure("Shader budget worker failed"))??;
    let mut output = open(&staging, true)?;
    // Limit both disk growth and resume reads to the CM-authorized target.
    use std::os::unix::fs::MetadataExt;
    let allocated = output.metadata()?.blocks().saturating_mul(512);
    let free = crate::install::destination(&root)?
        .free_bytes
        .ok_or_else(|| failure("Cannot inspect shader cache free space"))?;
    if free
        < file
            .size
            .saturating_sub(allocated)
            .saturating_add(1024 * 1024)
    {
        return Err(failure("Insufficient free space for shader cache staging"));
    }
    output.set_len(file.size)?;
    let mut done = 0;
    let mut wire = 0;
    for chunk in &file.chunks {
        crate::install::wait_while_paused(&pause, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(failure("Shader download cancelled"));
        }
        let mut bytes = vec![0; chunk.uncompressed_size as usize];
        output.seek(SeekFrom::Start(chunk.offset))?;
        let reused = output.read_exact(&mut bytes).is_ok()
            && crate::content::verify_chunk(chunk, &bytes).is_ok();
        if !reused {
            let verified = client.chunk(&spec, chunk, &cancel).await?;
            output.seek(SeekFrom::Start(chunk.offset))?;
            output.write_all(verified.as_bytes())?;
            wire += u64::from(chunk.compressed_size);
        }
        done += u64::from(chunk.uncompressed_size);
        report(
            &tx,
            game.app_id,
            done,
            file.size,
            wire,
            "Downloading Vulkan shader pre-cache",
        );
    }
    output.sync_all()?;
    drop(output);
    let p = staging.clone();
    let size = file.size;
    if tokio::task::spawn_blocking(move || hash(&p, size))
        .await
        .map_err(|_| failure("Shader verification worker failed"))??
        != file.sha1
    {
        return Err(failure("Shader archive whole-file verification failed"));
    }
    if cancel.is_cancelled() {
        return Err(failure("Shader download cancelled before commit"));
    }
    fs::rename(staging, &target)?;
    fs::File::open(&root)?.sync_all()?;
    let state = State {
        bucket: bucket.bucketid(),
        manifest: bucket.manifestid(),
        size: file.size,
        checked_at: now(),
        sha1: file.sha1,
        processed: None,
    };
    save(&root, &state)?;
    prune_locked(&root, &state, false)?;
    Ok(())
}
fn prune_locked(root: &Path, state: &State, replay: bool) -> Result<()> {
    // Only our numeric generations and exact replay directories are eligible.
    for entry in fs::read_dir(root)?.take(8192) {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if entry.file_type()?.is_symlink() {
            continue;
        }
        if let Some(id) = name
            .strip_suffix(".foz")
            .or_else(|| name.strip_suffix(".part"))
            .and_then(|v| v.parse::<u64>().ok())
        {
            if id != state.manifest && entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        } else if replay
            && let Some(context) = name.strip_prefix("replay-")
            && context.len() == 40
            && context.bytes().all(|b| b.is_ascii_hexdigit())
            && state.processed.as_deref() != Some(context)
            && entry.file_type()?.is_dir()
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
pub fn prune(queue: &Queue, app: u32) -> Result<()> {
    let _game = crate::cloud::launch_lease(app)?;
    let root = root(queue, app)?;
    let _lock = safe::Lock::acquire(&root.join("cache.lock"))?;
    prune_locked(&root, &load(&root)?, true)
}
pub use replay::{prepare, skip};

fn reserve(parent: &Path, staging: &Path, expected: u64, budget: u64) -> Result<()> {
    let mut total = expected;
    for (index, app) in fs::read_dir(parent)?.enumerate() {
        if index >= 4096 {
            return Err(failure("Shader cache app count exceeds bound"));
        }
        let app = app?;
        if !app.file_type()?.is_dir()
            || app
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_none()
        {
            continue;
        }
        for (index, file) in fs::read_dir(app.path())?.enumerate() {
            if index >= 8192 {
                return Err(failure("Shader generation count exceeds bound"));
            }
            let file = file?;
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if file.path() == staging || !file.file_type()?.is_file() {
                continue;
            }
            if name
                .strip_suffix(".foz")
                .or_else(|| name.strip_suffix(".part"))
                .and_then(|s| s.parse::<u64>().ok())
                .is_some()
            {
                total = total
                    .checked_add(file.metadata()?.len())
                    .ok_or_else(|| failure("Shader cache size overflow"))?;
            }
        }
    }
    if total > budget {
        return Err(failure(
            "Shader archive budget reached; clear unused caches with shaders clear APPID, or raise shaders budget MIB",
        ));
    }
    Ok(())
}
/// Explicit reclamation, serialized against both launches and downloads. Only
/// recognized generated archives, replay contexts and our state are removed.
pub fn clear(queue: &Queue, app: u32) -> Result<()> {
    let _game = crate::cloud::launch_lease(app)?;
    let root = root(queue, app)?;
    let _lock = safe::Lock::acquire(&root.join("cache.lock"))?;
    save(&root, &State::default())?;
    prune_locked(&root, &State::default(), true)
}

/// Called by an active queue runner after scanning every configured library.
/// In-use apps are retained; no daemon or Valve installation is involved.
pub(crate) fn prune_uninstalled(
    queue: &Queue,
    installed: &std::collections::BTreeSet<u32>,
) -> Result<()> {
    let parent = queue.state_directory().join("shaders");
    if !parent.exists() {
        return Ok(());
    }
    safe::private_directory(&parent)?;
    for entry in fs::read_dir(&parent)?.take(4096) {
        let entry = entry?;
        let Some(app) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id != 0)
        else {
            continue;
        };
        if installed.contains(&app) || !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(_game) = crate::cloud::launch_lease(app) else {
            continue;
        };
        let root = root(queue, app)?;
        let Some(_lock) = safe::Lock::try_acquire(&root.join("cache.lock"))? else {
            continue;
        };
        save(&root, &State::default())?;
        prune_locked(&root, &State::default(), true)?;
    }
    Ok(())
}
