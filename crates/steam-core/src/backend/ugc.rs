//! Workshop/UGC item details (ISteamUGC queries by published file id) through
//! the `PublishedFile.GetDetails` service method. Read-only.
use std::time::Duration;

use steam_ipc::{Response, UgcItem, UgcOp};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_publishedfile_steamclient::CPublishedFile_GetDetails_Request;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(8);
const MAX_IDS: usize = 256;
const MAX_METADATA: usize = 8192;

pub async fn handle(connection: &Connection, app_id: u32, op: UgcOp) -> Response {
    let UgcOp::Details {
        ids,
        long_description,
    } = op;
    if ids.is_empty() || ids.len() > MAX_IDS {
        return Response::Error {
            message: "Invalid UGC details request".into(),
        };
    }
    let connection = &connection.with_app_id(app_id);
    let request = CPublishedFile_GetDetails_Request {
        publishedfileids: ids,
        includetags: Some(true),
        includemetadata: Some(true),
        includechildren: Some(false),
        includevotes: Some(true),
        short_description: Some(!long_description),
        appid: Some(app_id),
        ..Default::default()
    };
    let reply = timeout(DEADLINE, connection.service_method(request)).await;
    let Ok(Ok(reply)) = reply else {
        return Response::Error {
            message: "UGC details request failed or timed out".into(),
        };
    };
    Response::UgcDetails {
        result: 1,
        items: reply
            .publishedfiledetails
            .iter()
            .take(MAX_IDS)
            .map(|d| {
                let description = if long_description {
                    d.file_description()
                } else {
                    d.short_description()
                };
                let mut metadata = d.metadata().as_bytes().to_vec();
                metadata.truncate(MAX_METADATA);
                UgcItem {
                    id: d.publishedfileid(),
                    result: d.result() as i32,
                    file_type: d.file_type() as i32,
                    creator_app: d.creator_appid(),
                    consumer_app: d.consumer_appid(),
                    title: d.title().to_owned(),
                    description: description.to_owned(),
                    owner: d.creator(),
                    created: d.time_created(),
                    updated: d.time_updated(),
                    visibility: d.visibility() as i32,
                    banned: d.banned(),
                    tags: d.tags.iter().map(|t| t.tag()).collect::<Vec<_>>().join(","),
                    file_handle: d.hcontent_file(),
                    preview_handle: d.hcontent_preview(),
                    file_name: d.filename().to_owned(),
                    file_size: d.file_size() as i32,
                    preview_size: d.preview_file_size() as i32,
                    url: d.url().to_owned(),
                    votes_up: d.vote_data.votes_up(),
                    votes_down: d.vote_data.votes_down(),
                    score: d.vote_data.score(),
                    children: d.num_children(),
                    metadata,
                }
            })
            .collect(),
    }
}
