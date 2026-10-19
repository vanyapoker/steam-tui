//! Shader service probe. Never starts Steam, a game or a replay helper.
//! --download-test writes an isolated temporary cache and verifies a second pass.
//! Optional --resume reads only the remembered login name; refresh tokens remain
//! in SecretService. Output contains bucket metadata, not authentication data.
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait, ServerList};
use steam_vent_proto_steam::steammessages_shader_steamclient::CShader_GetBucketManifest_Request;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let app: u32 = args
        .next()
        .ok_or("usage: shader-inspect APPID [--resume] [--chunk] [--download-test]")?
        .parse()?;
    if app == 0 {
        return Err("app must be positive".into());
    }
    let mut resume = false;
    let mut chunk = false;
    let mut download = false;
    for arg in args {
        match arg.as_str() {
            "--resume" if !resume => resume = true,
            "--chunk" if !chunk => chunk = true,
            "--download-test" if !download => download = true,
            _ => return Err("unknown or duplicate option".into()),
        }
    }
    let connection = if resume {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".config")
            });
        let name = std::fs::read_to_string(config.join("steam-tui/account"))?;
        steam_core::account::resume(name.trim())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
    } else {
        let servers =
            tokio::time::timeout(Duration::from_secs(20), ServerList::discover()).await??;
        tokio::time::timeout(Duration::from_secs(30), Connection::anonymous(&servers)).await??
    };
    // Observed in the original shader manager and its persistent bucket list.
    // GPU-independent Fossilize Vulkan pipeline archive, version 6. Other bucket
    // kinds (G7:CompatVideoTCMediaV1, G7:CompatAudioTCMediaV1) can be probed with
    // SHADER_DRIVER_DESC.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        connection.service_method(CShader_GetBucketManifest_Request {
            appid: Some(app),
            gpu_desc: Some("SteamSwarm".into()),
            driver_desc: Some(
                std::env::var("SHADER_DRIVER_DESC")
                    .unwrap_or_else(|_| "G7:VulkanPipelinesV6".into()),
            ),
            ..Default::default()
        }),
    )
    .await??;
    println!(
        "app={app} bucket={} manifest={} bytes={}",
        result.bucketid(),
        result.manifestid(),
        result.depotsize()
    );
    if result.manifestid() != 0 {
        let spec = steam_core::catalog::DepotSpec {
            depot_id: app,
            source_app_id: app,
            manifest_id: result.manifestid(),
        };
        let client = steam_core::content::ContentClient::new(connection.clone()).await?;
        let manifest = client
            .manifest(&spec, &tokio_util::sync::CancellationToken::new())
            .await?;
        println!("manifest files={}", manifest.files.len());
        if chunk && let Some(first) = manifest.files.iter().flat_map(|f| &f.chunks).next() {
            let verified = client
                .chunk(&spec, first, &tokio_util::sync::CancellationToken::new())
                .await?;
            println!(
                "First chunk verified: {} bytes, prefix={:02x?}",
                verified.as_bytes().len(),
                &verified.as_bytes()[..verified.as_bytes().len().min(12)]
            );
        }
        for file in manifest.files.iter().take(10) {
            println!(
                "{} bytes={} chunks={}",
                file.path
                    .to_string_lossy()
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>(),
                file.size,
                file.chunks.len()
            );
        }
    }
    if download {
        let game = steam_core::installed::scan(None, None)?
            .games
            .into_iter()
            .find(|g| g.app_id == app)
            .ok_or("test app must already be installed")?;
        let directory =
            std::env::temp_dir().join(format!("steam-tui-shader-probe-{}", std::process::id()));
        let queue = steam_core::updates::Queue::in_directory(&directory, "probe")?;
        for pass in 1..=2 {
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
            let cancel = tokio_util::sync::CancellationToken::new();
            let run = steam_core::shaders::download(
                &queue,
                connection.clone(),
                &game,
                tx,
                cancel.clone(),
                steam_core::install::PauseGate::default(),
            );
            tokio::pin!(run);
            let mut last = 0;
            loop {
                tokio::select! {
                    result = &mut run => { result?; break; },
                    Some(progress) = rx.recv() => if progress.completed_bytes >= last + 50 * 1024 * 1024 { last = progress.completed_bytes; println!("pass={pass} verified={}/{}", progress.completed_bytes, progress.total_bytes); },
                    _ = tokio::signal::ctrl_c(), if !cancel.is_cancelled() => cancel.cancel(),
                }
            }
            let state = steam_core::shaders::state(&queue, app)?;
            println!(
                "pass={pass} committed manifest={} bytes={} processed={}",
                state.manifest,
                state.size,
                state.processed.is_some()
            );
        }
        println!("Isolated test cache: {}", directory.display());
    }
    Ok(())
}
