//! Valve's transcoded media databases for Proton. Windows games decode their
//! videos and audio through Proton's media converter, which can only play
//! codecs Valve has pre-transcoded for the app. The per-app databases come
//! from the same bucket service as shader caches (`G7:CompatVideoTCMediaV1`,
//! `G7:CompatAudioTCMediaV1`) and are handed to Proton through
//! `STEAM_COMPAT_TRANSCODED_MEDIA_PATH`, as Valve's client does. The path
//! layout is `steamapps/shadercache/<app>/fozmediav1`.
use crate::backend::{Direction, Event, EventSender};
use crate::catalog::DepotSpec;
use crate::content::{ContentClient, DepotFile, DepotManifest};
use crate::install::filesystem as safe;
use crate::installed::{InstalledGame, Result, failure};
use crate::launch::LaunchPlan;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_shader_steamclient::CShader_GetBucketManifest_Request;
use tokio_util::sync::CancellationToken;

const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;
const STATE: &str = "steam-tui-media.json";
/// (bucket driver description, database file name Proton reads)
const KINDS: [(&str, &str); 2] = [
    ("G7:CompatVideoTCMediaV1", "transcoded_video.foz"),
    ("G7:CompatAudioTCMediaV1", "transcoded_audio.foz"),
];

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct Entry {
    bucket: u64,
    manifest: u64,
    size: u64,
    sha1: [u8; 20],
}
type State = BTreeMap<String, Entry>;

fn emit(events: Option<&EventSender>, app: u32, title: String) {
    if let Some(events) = events {
        let _ = events.send(Event {
            at: SystemTime::now(),
            app_id: app,
            direction: Direction::Local,
            channel: "Media",
            title,
            detail: "Valve's transcoded media database for Proton".into(),
        });
    }
}

fn directory(game: &InstalledGame) -> Result<PathBuf> {
    let path = game
        .library_root
        .join("steamapps/shadercache")
        .join(game.app_id.to_string())
        .join("fozmediav1");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)?;
    Ok(path)
}

fn load(directory: &Path) -> State {
    crate::installed::read_bounded(&directory.join(STATE), 65536)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
        return Err(failure("Media database must be a regular file"));
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
            return Ok(Some(digest.finalize().into()));
        }
        digest.update(&buffer[..n]);
    }
}

fn database<'a>(manifest: &'a DepotManifest, name: &str, advertised: u64) -> Result<&'a DepotFile> {
    if manifest.files.len() != 1 {
        return Err(failure("Unexpected files in media bucket"));
    }
    let file = &manifest.files[0];
    if file.link_target.is_some()
        || file.flags & 64 != 0
        || file.path != Path::new(name)
        || file.size == 0
        || file.size > MAX_FILE
        || file.size != advertised
        || file.sha1.is_none()
    {
        return Err(failure("Media bucket size/hash outside expected bounds"));
    }
    Ok(file)
}

/// Makes the app's transcoded media databases current and exposes them to
/// Proton. Optional: any failure leaves the launch without (or with the last
/// verified) databases and never blocks the game.
pub async fn prepare(
    connection: &Connection,
    game: &InstalledGame,
    plan: &mut LaunchPlan,
    events: Option<&EventSender>,
) {
    if std::env::var_os("STEAM_COMPAT_TRANSCODED_MEDIA_PATH").is_some()
        || plan
            .environment
            .contains_key("STEAM_COMPAT_TRANSCODED_MEDIA_PATH")
    {
        return;
    }
    let directory = match directory(game) {
        Ok(directory) => directory,
        Err(error) => {
            emit(
                events,
                game.app_id,
                format!("Media databases skipped: {error}"),
            );
            return;
        }
    };
    let result = tokio::time::timeout(
        Duration::from_secs(600),
        refresh(connection, game, &directory, events),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => emit(
            events,
            game.app_id,
            format!("Media databases skipped: {error}"),
        ),
        Err(_) => emit(events, game.app_id, "Media databases timed out".into()),
    }
    let state = load(&directory);
    let usable = KINDS.iter().any(|(_, name)| {
        state.get(*name).is_some_and(|entry| {
            fs::symlink_metadata(directory.join(name))
                .is_ok_and(|m| m.is_file() && m.len() == entry.size)
        })
    });
    if usable {
        plan.environment.insert(
            "STEAM_COMPAT_TRANSCODED_MEDIA_PATH".into(),
            directory.to_string_lossy().into_owned(),
        );
    }
}

async fn refresh(
    connection: &Connection,
    game: &InstalledGame,
    directory: &Path,
    events: Option<&EventSender>,
) -> Result<()> {
    let _lock = safe::Lock::try_acquire(&directory.join("cache.lock"))?
        .ok_or_else(|| failure("another launch is updating this app's media databases"))?;
    let mut state = load(directory);
    let mut failures = Vec::new();
    for (driver, name) in KINDS {
        let bucket = match tokio::time::timeout(
            Duration::from_secs(60),
            connection.service_method(CShader_GetBucketManifest_Request {
                appid: Some(game.app_id),
                gpu_desc: Some("SteamSwarm".into()),
                driver_desc: Some(driver.into()),
                ..Default::default()
            }),
        )
        .await
        {
            Ok(Ok(bucket)) => bucket,
            // The service answers NoMatch when the app has no such database.
            Ok(Err(steam_vent::NetworkError::ApiError(steam_vent::EResult::NoMatch))) => continue,
            Ok(Err(error)) => {
                failures.push(format!("{name}: {error}"));
                continue;
            }
            Err(_) => {
                failures.push(format!("{name}: metadata request timed out"));
                continue;
            }
        };
        if bucket.manifestid() == 0 {
            continue;
        }
        let target = directory.join(name);
        if state.get(name).is_some_and(|entry| {
            entry.manifest == bucket.manifestid()
                && entry.bucket == bucket.bucketid()
                && entry.size == bucket.depotsize()
                && fs::symlink_metadata(&target).is_ok_and(|m| m.is_file() && m.len() == entry.size)
        }) {
            continue;
        }
        match download(connection, game, directory, name, &bucket, events).await {
            Ok(entry) => {
                state.insert(name.into(), entry);
                safe::atomic_write(&directory.join(STATE), &serde_json::to_vec(&state)?)?;
            }
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failure(failures.join("; ")))
    }
}

async fn download(
    connection: &Connection,
    game: &InstalledGame,
    directory: &Path,
    name: &str,
    bucket: &steam_vent_proto_steam::steammessages_shader_steamclient::CShader_GetBucketManifest_Response,
    events: Option<&EventSender>,
) -> Result<Entry> {
    let cancel = CancellationToken::new();
    let spec = DepotSpec {
        depot_id: game.app_id,
        source_app_id: game.app_id,
        manifest_id: bucket.manifestid(),
    };
    let client = ContentClient::new(connection.clone()).await?;
    let manifest = client.manifest(&spec, &cancel).await?;
    let file = database(&manifest, name, bucket.depotsize())?;
    let staging = directory.join(format!("{name}.part"));
    let mut output = open(&staging, true)?;
    output.set_len(file.size)?;
    emit(
        events,
        game.app_id,
        format!("Downloading {name} ({} MiB)", file.size / (1024 * 1024)),
    );
    for chunk in &file.chunks {
        let mut bytes = vec![0; chunk.uncompressed_size as usize];
        output.seek(SeekFrom::Start(chunk.offset))?;
        let reused = output.read_exact(&mut bytes).is_ok()
            && crate::content::verify_chunk(chunk, &bytes).is_ok();
        if !reused {
            let verified = client.chunk(&spec, chunk, &cancel).await?;
            output.seek(SeekFrom::Start(chunk.offset))?;
            output.write_all(verified.as_bytes())?;
        }
    }
    output.sync_all()?;
    drop(output);
    let (path, size) = (staging.clone(), file.size);
    let digest = tokio::task::spawn_blocking(move || hash(&path, size))
        .await
        .map_err(|_| failure("Media verification worker failed"))??;
    if digest != file.sha1 {
        let _ = fs::remove_file(&staging);
        return Err(failure("Media database whole-file verification failed"));
    }
    fs::rename(&staging, directory.join(name))?;
    fs::File::open(directory)?.sync_all()?;
    Ok(Entry {
        bucket: bucket.bucketid(),
        manifest: bucket.manifestid(),
        size: file.size,
        sha1: digest.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64, flags: u32) -> DepotFile {
        DepotFile {
            path: PathBuf::from(path),
            size,
            sha1: Some([1; 20]),
            flags,
            chunks: Vec::new(),
            link_target: None,
        }
    }
    fn manifest(files: Vec<DepotFile>) -> DepotManifest {
        DepotManifest {
            depot_id: 1,
            manifest_id: 1,
            files,
        }
    }

    #[test]
    fn only_the_advertised_single_database_file_is_accepted() {
        let good = manifest(vec![file("transcoded_video.foz", 10, 0)]);
        assert!(database(&good, "transcoded_video.foz", 10).is_ok());
        assert!(database(&good, "transcoded_audio.foz", 10).is_err());
        assert!(database(&good, "transcoded_video.foz", 11).is_err());
        let two = manifest(vec![
            file("transcoded_video.foz", 10, 0),
            file("extra", 1, 0),
        ]);
        assert!(database(&two, "transcoded_video.foz", 10).is_err());
        let directory = manifest(vec![file("transcoded_video.foz", 10, 64)]);
        assert!(database(&directory, "transcoded_video.foz", 10).is_err());
        let huge = manifest(vec![file("transcoded_video.foz", MAX_FILE + 1, 0)]);
        assert!(database(&huge, "transcoded_video.foz", MAX_FILE + 1).is_err());
    }

    #[test]
    fn state_round_trips_and_garbage_is_ignored() {
        let dir = std::env::temp_dir().join(format!("steam-tui-media-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_empty());
        let mut state = State::new();
        state.insert(
            "transcoded_video.foz".into(),
            Entry {
                bucket: 1,
                manifest: 2,
                size: 3,
                sha1: [4; 20],
            },
        );
        safe::atomic_write(&dir.join(STATE), &serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(load(&dir), state);
        fs::write(dir.join(STATE), b"nope").unwrap();
        assert!(load(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
