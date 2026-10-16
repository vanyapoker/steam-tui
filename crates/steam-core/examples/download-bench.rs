//! Bounded CDN/decode comparison without installing or changing game files.
//! Usage: cargo run --release -p steam-core --example download-bench -- ACCOUNT APP DEPOT MANIFEST
use futures_util::{StreamExt, TryStreamExt, stream};
use std::{collections::HashSet, error::Error, time::Instant};
use steam_core::{account, catalog::DepotSpec, content::ContentClient};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new().run_until(run()).await
}
async fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("Usage: download-bench ACCOUNT APP DEPOT MANIFEST".into());
    }
    let spec = DepotSpec {
        source_app_id: args[1].parse()?,
        depot_id: args[2].parse()?,
        manifest_id: args[3].parse()?,
    };
    let connection = account::resume(&args[0]).await?;
    let client = ContentClient::new(connection)
        .await
        .map_err(|e| e as Box<dyn Error>)?;
    let cancel = CancellationToken::new();
    let manifest = client
        .manifest(&spec, &cancel)
        .await
        .map_err(|e| e as Box<dyn Error>)?;
    let mut seen = HashSet::new();
    let mut sample = Vec::new();
    let mut bytes = 0u64;
    for chunk in manifest.files.iter().flat_map(|f| &f.chunks) {
        if chunk.compressed_size < 64 * 1024
            || chunk.uncompressed_size > 2 * 1024 * 1024
            || !seen.insert(chunk.id)
        {
            continue;
        }
        if bytes + u64::from(chunk.compressed_size) > 16 * 1024 * 1024 {
            continue;
        }
        bytes += u64::from(chunk.compressed_size);
        sample.push(chunk.clone());
        if sample.len() == 32 {
            break;
        }
    }
    if sample.len() < 8 {
        return Err("Not enough distinct chunks within the benchmark bounds".into());
    }
    println!(
        "{} distinct chunks, {:.2} MiB compressed per pass; ABBA ordering; no install writes",
        sample.len(),
        bytes as f64 / 1048576.0
    );
    // Four passes reduce simple order/cache bias. This measures CDN+validation,
    // not full-install time or disk throughput. Never log authenticated URLs.
    for concurrency in [1, 8, 8, 1] {
        let start = Instant::now();
        tokio::time::timeout(std::time::Duration::from_secs(90), async {
            stream::iter(sample.iter())
                .map(|chunk| client.chunk(&spec, chunk, &cancel))
                .buffer_unordered(concurrency)
                .try_for_each(|_| async { Ok(()) })
                .await
        })
        .await?
        .map_err(|e| e as Box<dyn Error>)?;
        let seconds = start.elapsed().as_secs_f64();
        println!(
            "concurrency={concurrency} seconds={seconds:.3} compressed_MiB_per_s={:.3}",
            bytes as f64 / 1048576.0 / seconds
        );
    }
    Ok(())
}
