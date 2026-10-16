//! Live check of Workshop subscription writes on an item, restoring the account
//! to where it started. Usage: ugc-write-probe ACCOUNT APP ITEM
use std::error::Error;
use std::time::Duration;
use steam_core::account;
use steam_vent::ConnectionTrait;
use steam_vent_proto_steam::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetUserFiles_Request, CPublishedFile_Subscribe_Request,
    CPublishedFile_Unsubscribe_Request,
};
use tokio::time::timeout;

async fn listed(connection: &steam_vent::Connection, me: u64, app: u32, kind: &str) -> Vec<u64> {
    let request = CPublishedFile_GetUserFiles_Request {
        steamid: Some(me),
        appid: Some(app),
        page: Some(1),
        numperpage: Some(50),
        type_: Some(kind.to_owned()),
        ids_only: Some(true),
        ..Default::default()
    };
    match timeout(Duration::from_secs(10), connection.service_method(request)).await {
        Ok(Ok(reply)) => reply
            .publishedfiledetails
            .iter()
            .map(|d| d.publishedfileid())
            .collect(),
        other => {
            println!(
                "  list {kind} failed: {:?}",
                other.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))
            );
            Vec::new()
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut args = std::env::args().skip(1);
            let usage = "Usage: ugc-write-probe ACCOUNT APP ITEM";
            let account = args.next().ok_or(usage)?;
            let app: u32 = args.next().ok_or(usage)?.parse()?;
            let item: u64 = args.next().ok_or(usage)?.parse()?;
            let connection = account::resume(&account).await?;
            let connection = connection.with_app_id(app);
            let me = connection.steam_id().steam64();
            println!(
                "baseline: subs {:?} favs {:?}",
                listed(&connection, me, app, "mysubscriptions").await,
                listed(&connection, me, app, "myfavorites").await
            );
            for list_type in [1u32, 0, 2] {
                println!("subscribe with list_type {list_type}");
                let result = timeout(
                    Duration::from_secs(10),
                    connection.service_method(CPublishedFile_Subscribe_Request {
                        publishedfileid: Some(item),
                        list_type: Some(list_type),
                        appid: Some(app as i32),
                        notify_client: Some(false),
                        include_dependencies: Some(false),
                        ..Default::default()
                    }),
                )
                .await;
                println!(
                    "  -> {:?}",
                    result.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                let subs = listed(&connection, me, app, "mysubscriptions").await;
                let favs = listed(&connection, me, app, "myfavorites").await;
                println!(
                    "  subs has item: {} favs has item: {}",
                    subs.contains(&item),
                    favs.contains(&item)
                );
                let result = timeout(
                    Duration::from_secs(10),
                    connection.service_method(CPublishedFile_Unsubscribe_Request {
                        publishedfileid: Some(item),
                        list_type: Some(list_type),
                        appid: Some(app as i32),
                        notify_client: Some(false),
                        ..Default::default()
                    }),
                )
                .await;
                println!(
                    "  unsubscribe -> {:?}",
                    result.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                println!(
                    "  after: subs has item: {} favs has item: {}",
                    listed(&connection, me, app, "mysubscriptions")
                        .await
                        .contains(&item),
                    listed(&connection, me, app, "myfavorites")
                        .await
                        .contains(&item)
                );
            }
            println!(
                "final: subs {:?} favs {:?}",
                listed(&connection, me, app, "mysubscriptions").await,
                listed(&connection, me, app, "myfavorites").await
            );
            Ok(())
        })
        .await
}
