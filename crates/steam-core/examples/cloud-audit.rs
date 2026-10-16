//! Read-only validation of live cloud listings and download hashes. Never uploads
//! or writes saves/state. Usage: cloud-audit ACCOUNT APP [APP ...]
use std::{error::Error, time::Duration};
use steam_core::{
    account,
    cloud::{CloudApi, LiveApi, decode_download},
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new().run_until(async {
        let mut args = std::env::args().skip(1);
        let account = args.next().ok_or("Usage: cloud-audit ACCOUNT APP [APP ...]")?;
        let apps = args.map(|arg| arg.parse::<u32>()).collect::<Result<Vec<_>, _>>()?;
        if apps.is_empty() { return Err("at least one app is required".into()); }
        let connection = account::resume(&account).await?;
        let api = LiveApi::new(connection).map_err(|e| e as Box<dyn Error>)?;
        for app_id in apps {
            let list = api.changelist(app_id, 0).await.map_err(|e| e as Box<dyn Error>)?;
            let mut verified = 0;
            let mut bytes = 0;
            for file in &list.files {
                if !file.persisted || file.size > 2 * 1024 * 1024 || bytes + file.size > 8 * 1024 * 1024 || verified >= 16 { continue; }
                let Some(sha) = &file.sha1 else { continue };
                let fetched = tokio::time::timeout(Duration::from_secs(60), api.fetch(app_id, &file.name)).await?.map_err(|e| e as Box<dyn Error>)?;
                if fetched.encrypted || fetched.explicit_delete { continue; }
                let decoded = decode_download(&fetched, sha).map_err(|e| e as Box<dyn Error>)?;
                if decoded.len() as u64 != file.size { return Err("listing size mismatch".into()); }
                bytes += decoded.len() as u64;
                verified += 1;
            }
            println!("app={app_id} listed={} verified={verified} bytes={bytes}; no save/state writes", list.files.len());
        }
        Ok(())
    }).await
}
