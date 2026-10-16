//! Read-only probe of the Workshop protocol: QueryFiles, GetUserFiles types and the
//! subscribed-item enumeration. Usage: ugc-probe ACCOUNT APP
use std::error::Error;
use std::time::Duration;
use steam_core::account;
use steam_vent::ConnectionTrait;
use steam_vent_proto_steam::steammessages_clientserver_ucm::{
    CMsgClientUCMEnumerateUserSubscribedFilesWithUpdates,
    CMsgClientUCMEnumerateUserSubscribedFilesWithUpdatesResponse,
};
use steam_vent_proto_steam::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetUserFiles_Request, CPublishedFile_QueryFiles_Request,
};
use tokio::time::timeout;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut args = std::env::args().skip(1);
            let usage = "Usage: ugc-probe ACCOUNT APP";
            let account = args.next().ok_or(usage)?;
            let app: u32 = args.next().ok_or(usage)?.parse()?;
            let connection = account::resume(&account).await?;
            let connection = connection.with_app_id(app);
            let me = connection.steam_id().steam64();

            let request = CPublishedFile_QueryFiles_Request {
                query_type: Some(3),
                page: Some(1),
                numperpage: Some(3),
                appid: Some(app),
                creator_appid: Some(app),
                return_tags: Some(true),
                return_metadata: Some(true),
                return_previews: Some(true),
                return_kv_tags: Some(true),
                return_vote_data: Some(true),
                return_children: Some(true),
                return_short_description: Some(true),
                return_details: Some(true),
                ..Default::default()
            };
            match timeout(Duration::from_secs(10), connection.service_method(request)).await {
                Ok(Ok(reply)) => {
                    println!("QueryFiles: total {} returned {} cursor {:?}", reply.total(), reply.publishedfiledetails.len(), reply.next_cursor());
                    if let Some(d) = reply.publishedfiledetails.last() {
                        let client = steam_core::content::ContentClient::new(connection.clone()).await.map_err(|e| e.to_string())?;
                        let spec = steam_core::catalog::DepotSpec { depot_id: app, manifest_id: d.hcontent_file(), source_app_id: app };
                        let cancel = tokio_util::sync::CancellationToken::new();
                        match client.manifest(&spec, &cancel).await {
                            Ok(m) => println!("  manifest ok: {} files, {} bytes; first {:?}", m.files.len(), m.files.iter().map(|f| f.size).sum::<u64>(), m.files.first().map(|f| (&f.path, f.size, f.flags, f.chunks.len()))),
                            Err(e) => println!("  manifest failed: {e}"),
                        }
                    }
                    for d in &reply.publishedfiledetails {
                        println!("  {} {:?} by {} type {} manifest {} size {} tags {} kv {} previews {} subs {} url {:?} file_url {:?}", d.publishedfileid(), d.title(), d.creator(), d.file_type(), d.hcontent_file(), d.file_size(), d.tags.len(), d.kvtags.len(), d.previews.len(), d.subscriptions(), d.url(), d.file_url());
                    }
                }
                other => println!("QueryFiles failed: {:?}", other.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))),
            }
            {
                use steam_vent_proto_steam::steammessages_publishedfile_steamclient::CPublishedFile_GetDetails_Request;
                let request = CPublishedFile_GetDetails_Request { publishedfileids: vec![105982362], appid: Some(app), ..Default::default() };
                match timeout(Duration::from_secs(10), connection.service_method(request)).await {
                    Ok(Ok(reply)) => for d in &reply.publishedfiledetails { println!("legacy item {} result {} title {:?} hcontent_file {} file_url {:?} url {:?} filename {:?} size {} consumer {} creator_app {} updated {}", d.publishedfileid(), d.result(), d.title(), d.hcontent_file(), d.file_url(), d.url(), d.filename(), d.file_size(), d.consumer_appid(), d.creator_appid(), d.time_updated()) },
                    other => println!("details failed: {:?}", other.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))),
                }
            }
            for kind in ["myvotes", "myvotedup", "myvotedown", "myvotedon", "myupvotes", "mydownvotes", "mywillvotelater", "myvoteslater", "myplayed", "myusedorplayed", "myfollowed", "myfollowing", "mycompleted", "mywatched", "myqueue", "mysubscribed", "myfriends", "mywishlist", "myrecent", "myrated"] {
                let request = CPublishedFile_GetUserFiles_Request {
                    steamid: Some(me),
                    appid: Some(app),
                    page: Some(1),
                    numperpage: Some(5),
                    type_: Some(kind.to_owned()),
                    ids_only: Some(true),
                    ..Default::default()
                };
                match timeout(Duration::from_secs(10), connection.service_method(request)).await {
                    Ok(Ok(reply)) => println!("GetUserFiles {kind}: total {} ids {:?}", reply.total(), reply.publishedfiledetails.iter().map(|d| d.publishedfileid()).collect::<Vec<_>>()),
                    other => println!("GetUserFiles {kind} failed: {:?}", other.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))),
                }
            }
            let request = CMsgClientUCMEnumerateUserSubscribedFilesWithUpdates {
                app_id: Some(app),
                start_index: Some(0),
                start_time: Some(0),
                desired_revision: Some(0),
                ..Default::default()
            };
            match timeout(Duration::from_secs(10), connection.job::<_, CMsgClientUCMEnumerateUserSubscribedFilesWithUpdatesResponse>(request)).await {
                Ok(Ok(reply)) => println!("EnumerateSubscribed: eresult {} total {} files {}", reply.eresult(), reply.total_results(), reply.subscribed_files.len()),
                other => println!("EnumerateSubscribed failed: {:?}", other.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))),
            }
            Ok(())
        })
        .await
}
