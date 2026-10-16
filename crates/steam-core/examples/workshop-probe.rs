//! Live check of the Workshop install service: downloads items into a scratch
//! `steamapps` directory (nothing on the account changes).
//! Usage: workshop-probe ACCOUNT APP STEAMAPPS_DIR [ITEM_ID ...]
use std::error::Error;
use std::time::{Duration, Instant};
use steam_core::{account, backend::workshop::Service};
use steam_ipc::{Response, UgcOp};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut args = std::env::args().skip(1);
            let usage = "Usage: workshop-probe ACCOUNT APP STEAMAPPS_DIR [ITEM_ID ...]";
            let account = args.next().ok_or(usage)?;
            let app: u32 = args.next().ok_or(usage)?.parse()?;
            let dir = std::path::PathBuf::from(args.next().ok_or(usage)?);
            let ids: Vec<u64> = args.map(|a| a.parse()).collect::<Result<_, _>>()?;
            let connection = account::resume(&account).await?;
            let service = Service::new(&connection, app, Some(dir.clone()), None);
            for id in &ids {
                let reply = service.handle(UgcOp::Download { id: *id, high_priority: false }).await;
                println!("Download {id}: {}", describe(&reply));
            }
            let started = Instant::now();
            loop {
                let Response::UgcSnapshot { ready, items } = service.handle(UgcOp::Snapshot).await else {
                    return Err("unexpected reply".into());
                };
                let busy = items.iter().any(|i| i.downloading || i.pending);
                for i in &items {
                    println!("  {:>12} sub={} installed={} legacy={} update={} downloading={} pending={} size={} {}/{} failed={} path={}", i.id, i.subscribed, i.installed, i.legacy, i.needs_update, i.downloading, i.pending, i.size, i.done, i.total, i.failed, i.path);
                }
                println!("ready={ready} busy={busy} after {:?}", started.elapsed());
                if !busy || started.elapsed() > Duration::from_secs(240) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            for (id, bytes) in service.poll() {
                println!("callback {id}: {} bytes", bytes.len());
            }
            Ok(())
        })
        .await
}

fn describe(reply: &Response) -> String {
    match reply {
        Response::UgcResult { result } => format!("eresult {result}"),
        Response::Error { message } => format!("error: {message}"),
        _ => "other".into(),
    }
}
