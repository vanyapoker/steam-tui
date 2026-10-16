//! Workshop/UGC item details (ISteamUGC queries by published file id) through
//! the `PublishedFile.GetDetails` service method. Read-only.
use std::time::Duration;

use steam_ipc::{Response, UgcItem, UgcOp};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_publishedfile_steamclient::CPublishedFile_GetDetails_Request;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(8);
/// Games ask for up to 1000 items at once (Backpack Battles does).
const MAX_IDS: usize = 1000;
/// One service call per chunk; chunks run concurrently.
const CHUNK: usize = 100;
const MAX_METADATA: usize = 8192;
/// JSON frames are bounded (8 MiB) and bytes serialize as numbers, so the
/// metadata of one reply is budgeted; items past the budget come back without it.
const METADATA_BUDGET: usize = 1 << 20;

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
    let calls = ids.chunks(CHUNK).map(|chunk| {
        let request = CPublishedFile_GetDetails_Request {
            publishedfileids: chunk.to_vec(),
            includetags: Some(true),
            includemetadata: Some(true),
            includechildren: Some(false),
            includevotes: Some(true),
            short_description: Some(!long_description),
            appid: Some(app_id),
            ..Default::default()
        };
        connection.service_method(request)
    });
    let replies = timeout(DEADLINE, futures_util::future::join_all(calls)).await;
    let Ok(replies) = replies else {
        return Response::Error {
            message: "UGC details request timed out".into(),
        };
    };
    let mut details = Vec::new();
    for reply in replies {
        let Ok(reply) = reply else {
            return Response::Error {
                message: "UGC details request failed".into(),
            };
        };
        details.extend(reply.publishedfiledetails);
    }
    let mut budget = METADATA_BUDGET;
    Response::UgcDetails {
        result: 1,
        items: details
            .iter()
            .take(MAX_IDS)
            .map(|d| {
                let description = if long_description {
                    d.file_description()
                } else {
                    d.short_description()
                };
                let mut metadata = d.metadata().as_bytes().to_vec();
                metadata.truncate(MAX_METADATA.min(budget));
                budget -= metadata.len();
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
