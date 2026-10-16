//! Explicit live write probe, restricted to Spacewar (480), one unique test file.
//! Never overwrites existing files. Cleanup requires the exact probe SHA1.
//! Usage: cloud-roundtrip ACCOUNT --write-spacewar-test
use sha1::{Digest, Sha1};
use std::{
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};
use steam_core::{
    account,
    cloud::{Batch, CloudApi, LiveApi, UploadFile, decode_download},
};
type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
fn failure(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}
const APP: u32 = 480;

async fn batch(api: &LiveApi, upload: &[String], delete: &[String], change: u64) -> Result<Batch> {
    let batch = api.begin_batch(APP, upload, delete, 0).await?;
    if batch.change_number.is_none() || batch.change_number != change.checked_add(1) {
        api.complete_batch(APP, batch.id, false).await?;
        return Err(failure(
            "Spacewar cloud changed concurrently; test batch cancelled",
        ));
    }
    Ok(batch)
}
async fn cleanup(api: &LiveApi, name: &str, sha: &str) -> Result<()> {
    let list = api.changelist(APP, 0).await?;
    let Some(file) = list.files.iter().find(|f| f.name == name && f.persisted) else {
        return Ok(());
    };
    if file.sha1.as_deref() != Some(sha) {
        return Err(failure(
            "test file changed externally; refusing to delete it",
        ));
    }
    let batch = batch(api, &[], &[name.into()], list.current).await?;
    let deleted = api.delete(APP, batch.id, name).await;
    let closed = api.complete_batch(APP, batch.id, deleted.is_ok()).await;
    closed?;
    deleted?;
    let list = api.changelist(APP, 0).await?;
    if list.files.iter().any(|f| f.name == name && f.persisted) {
        return Err(failure("test file remains in the cloud"));
    }
    Ok(())
}
async fn probe(api: &LiveApi) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let name = format!(
        "steam-tui-probe-{}-{}.dat",
        now.as_nanos(),
        std::process::id()
    );
    let body = format!(
        "steam-tui isolated cloud transport test {} {}\n",
        now.as_nanos(),
        std::process::id()
    )
    .into_bytes();
    let digest: [u8; 20] = Sha1::digest(&body).into();
    let sha = steam_ipc::hex_encode(&digest);
    let before = api.changelist(APP, 0).await?;
    if before.files.iter().any(|f| f.name == name) {
        return Err(failure("probe file already exists; refusing any write"));
    }
    println!("Spacewar-only probe: {name}; no existing cloud files will be changed");
    let result = async {
        let batch = batch(api, &[name.clone()], &[], before.current).await?;
        let upload = api
            .upload(&UploadFile {
                app_id: APP,
                batch_id: batch.id,
                name: &name,
                bytes: &body,
                sha1: digest,
                time: now.as_secs(),
                platforms: u32::MAX,
            })
            .await;
        let completed = api
            .complete_batch(APP, batch.id, matches!(upload, Ok(true)))
            .await;
        completed?;
        if !upload? {
            return Err(failure("test upload was not committed"));
        }
        let listing = api.changelist(APP, 0).await?;
        let file = listing
            .files
            .iter()
            .find(|f| f.name == name && f.persisted)
            .ok_or_else(|| failure("uploaded test file missing from listing"))?;
        if file.sha1.as_deref() != Some(sha.as_str()) || file.size != body.len() as u64 {
            return Err(failure("test listing hash/size mismatch"));
        }
        let downloaded = api.fetch(APP, &name).await?;
        if decode_download(&downloaded, &sha)? != body {
            return Err(failure("test roundtrip changed content"));
        }
        Ok(())
    }
    .await;
    let cleaned = cleanup(api, &name, &sha).await;
    if let Err(error) = &cleaned {
        eprintln!("Cleanup not confirmed for {name}: {error}");
    }
    result?;
    cleaned?;
    println!(
        "PASS: Spacewar upload, completed batch, fresh listing, hash-verified download and guarded deletion; no test file left"
    );
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new().run_until(async {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() != 2 || args[1] != "--write-spacewar-test" { return Err("Usage: cloud-roundtrip ACCOUNT --write-spacewar-test (explicitly creates and deletes one unique app 480 cloud file)".into()); }
        let connection = account::resume(&args[0]).await?;
        let api = LiveApi::new(connection).map_err(|e| e as Box<dyn Error>)?;
        probe(&api).await.map_err(|e| e as Box<dyn Error>)
    }).await
}
