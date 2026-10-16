//! Workshop/UGC (ISteamUGC) over the CM connection: item details, queries,
//! subscriptions, votes, favorites, playtime tracking and dependencies through
//! the `PublishedFile` service. Publishing items is not implemented.
use std::time::Duration;

use steam_ipc::{Response, UgcItem, UgcOp, UgcPreview, UgcQuery};
use steam_vent::{Connection, ConnectionTrait, NetworkError};
use steam_vent_proto_steam::steammessages_publishedfile_steamclient::{
    CPublishedFile_AddAppRelationship_Request, CPublishedFile_AddChild_Request,
    CPublishedFile_Delete_Request, CPublishedFile_GetAppRelationships_Request,
    CPublishedFile_GetDetails_Request, CPublishedFile_GetUserFiles_Request,
    CPublishedFile_GetUserVoteSummary_Request, CPublishedFile_QueryFiles_Request,
    CPublishedFile_RemoveAppRelationship_Request, CPublishedFile_RemoveChild_Request,
    CPublishedFile_StartPlaytimeTracking_Request,
    CPublishedFile_StopPlaytimeTrackingForAllAppItems_Request,
    CPublishedFile_StopPlaytimeTracking_Request, CPublishedFile_Subscribe_Request,
    CPublishedFile_Unsubscribe_Request, CPublishedFile_Vote_Request, PublishedFileDetails,
    cpublished_file_get_user_files_request, cpublished_file_query_files_request,
};
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
/// Steam's page size for queries (k_cNumUGCResultsPerPage).
const PER_PAGE: u32 = 50;
/// Subscription listings stop after this many pages of `PER_PAGE`.
const MAX_SUBSCRIBED_PAGES: u32 = 40;
const MAX_TAGS: usize = 64;
const MAX_PREVIEWS: usize = 32;
const MAX_CHILDREN: usize = 256;
const E_OK: i32 = 1;
const E_FAIL: i32 = 2;
const E_INVALID_PARAM: i32 = 8;
const E_TIMEOUT: i32 = 16;

/// The EResult of a failed call: the server's code, or a generic failure.
fn failure_code(error: &NetworkError) -> i32 {
    match error {
        NetworkError::ApiError(code) => *code as i32,
        NetworkError::Timeout => E_TIMEOUT,
        _ => E_FAIL,
    }
}

/// SDK language names to Steam's ELanguage values.
fn language(name: &str) -> Option<i32> {
    const NAMES: [&str; 29] = [
        "english",
        "german",
        "french",
        "italian",
        "koreana",
        "spanish",
        "schinese",
        "tchinese",
        "russian",
        "thai",
        "japanese",
        "portuguese",
        "polish",
        "danish",
        "dutch",
        "finnish",
        "norwegian",
        "swedish",
        "hungarian",
        "czech",
        "romanian",
        "turkish",
        "arabic",
        "brazilian",
        "bulgarian",
        "greek",
        "ukrainian",
        "latam",
        "vietnamese",
    ];
    NAMES
        .iter()
        .position(|n| n.eq_ignore_ascii_case(name))
        .map(|i| i as i32)
}

/// EUGCMatchingUGCType to the `filetype` of IPublishedFileService
/// (EPublishedFileInfoMatchingFileType); `None` matches everything.
fn matching_file_type(matching: i32) -> Option<u32> {
    Some(match matching {
        0 => 0,   // Items
        1 => 17,  // Items_Mtx
        2 => 18,  // Items_ReadyToUse
        3 => 1,   // Collections
        4 => 2,   // Artwork
        5 => 3,   // Videos
        6 => 4,   // Screenshots
        7 => 10,  // AllGuides
        8 => 11,  // WebGuides
        9 => 12,  // IntegratedGuides
        10 => 13, // UsableInGame
        11 => 15, // ControllerBindings
        12 => 20, // GameManagedItems
        _ => return None,
    })
}

/// EUserUGCList to the `type` string of PublishedFile.GetUserFiles. Only the lists
/// the service accepts are mapped (verified against the live service).
fn user_list_type(list: u32) -> Option<&'static str> {
    match list {
        0 => Some("myfiles"),
        5 => Some("myfavorites"),
        6 => Some("mysubscriptions"),
        _ => None,
    }
}

fn pairs(tags: &[impl AsRef<str>]) -> usize {
    tags.len().min(MAX_TAGS)
}

/// One item from the service's details, with description and metadata bounded.
fn convert(d: &PublishedFileDetails, long_description: bool, budget: &mut usize) -> UgcItem {
    let description = if long_description {
        d.file_description()
    } else {
        d.short_description()
    };
    let mut metadata = d.metadata().as_bytes().to_vec();
    metadata.truncate(MAX_METADATA.min(*budget));
    *budget -= metadata.len();
    let tag_list: Vec<(String, String)> = d
        .tags
        .iter()
        .take(MAX_TAGS)
        .map(|t| (t.tag().to_owned(), t.display_name().to_owned()))
        .collect();
    let previews = d
        .previews
        .iter()
        .take(MAX_PREVIEWS)
        .map(|p| {
            if p.youtubevideoid().is_empty() {
                UgcPreview {
                    url: p.url().to_owned(),
                    original: p.filename().to_owned(),
                    kind: p.preview_type(),
                }
            } else {
                UgcPreview {
                    url: p.youtubevideoid().to_owned(),
                    original: String::new(),
                    kind: 1,
                }
            }
        })
        .collect();
    let playtime = d.playtime_stats.as_ref();
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
        tags: tag_list
            .iter()
            .map(|(tag, _)| tag.as_str())
            .collect::<Vec<_>>()
            .join(","),
        file_handle: d.hcontent_file(),
        preview_handle: d.hcontent_preview(),
        file_name: d.filename().to_owned(),
        file_size: d.file_size() as i32,
        preview_size: d.preview_file_size() as i32,
        url: d.url().to_owned(),
        file_url: d.file_url().to_owned(),
        votes_up: d.vote_data.votes_up(),
        votes_down: d.vote_data.votes_down(),
        score: d.vote_data.score(),
        children: d.num_children(),
        metadata,
        tag_list,
        preview_url: if d.preview_url().is_empty() {
            d.image_url().to_owned()
        } else {
            d.preview_url().to_owned()
        },
        kv_tags: d
            .kvtags
            .iter()
            .take(MAX_TAGS)
            .map(|t| (t.key().to_owned(), t.value().to_owned()))
            .collect(),
        previews,
        child_ids: d
            .children
            .iter()
            .take(MAX_CHILDREN)
            .map(|c| c.publishedfileid())
            .collect(),
        stats: vec![
            (0, u64::from(d.subscriptions())),
            (1, u64::from(d.favorited())),
            (2, u64::from(d.followers())),
            (3, u64::from(d.lifetime_subscriptions())),
            (4, u64::from(d.lifetime_favorited())),
            (5, u64::from(d.lifetime_followers())),
            (6, u64::from(d.views())),
            (7, u64::from(d.num_reports())),
            (8, playtime.map_or(0, |p| p.playtime_seconds())),
            (9, playtime.map_or(0, |p| p.num_sessions())),
            (10, d.num_comments_public().max(0) as u64),
            (11, playtime.map_or(0, |p| p.playtime_seconds())),
            (12, playtime.map_or(0, |p| p.num_sessions())),
        ],
        subscribed_at: d.time_subscribed(),
    }
}

fn items(details: &[PublishedFileDetails], long_description: bool) -> Vec<UgcItem> {
    let mut budget = METADATA_BUDGET;
    details
        .iter()
        .take(MAX_IDS)
        .map(|d| convert(d, long_description, &mut budget))
        .collect()
}

pub async fn handle(connection: &Connection, app_id: u32, op: UgcOp) -> Response {
    let connection = &connection.with_app_id(app_id);
    match op {
        UgcOp::Details {
            ids,
            long_description,
        } => details(connection, app_id, ids, long_description).await,
        UgcOp::Query(query) => query_files(connection, app_id, *query).await,
        UgcOp::Subscribed { app } => subscribed(connection, if app == 0 { app_id } else { app }).await,
        UgcOp::Subscribe {
            app,
            id,
            subscribe,
        } => {
            let result = if subscribe {
                call(connection.service_method(CPublishedFile_Subscribe_Request {
                    publishedfileid: Some(id),
                    list_type: Some(1),
                    appid: Some(app as i32),
                    notify_client: Some(false),
                    include_dependencies: Some(false),
                    ..Default::default()
                }))
                .await
                .map(|_| ())
            } else {
                call(connection.service_method(CPublishedFile_Unsubscribe_Request {
                    publishedfileid: Some(id),
                    list_type: Some(1),
                    appid: Some(app as i32),
                    notify_client: Some(false),
                    ..Default::default()
                }))
                .await
                .map(|_| ())
            };
            result_of(result)
        }
        UgcOp::Vote { id, up } => result_of(
            call(connection.service_method(CPublishedFile_Vote_Request {
                publishedfileid: Some(id),
                vote_up: Some(up),
                ..Default::default()
            }))
            .await
            .map(|_| ()),
        ),
        UgcOp::GetVote { id } => {
            match call(
                connection.service_method(CPublishedFile_GetUserVoteSummary_Request {
                    publishedfileids: vec![id],
                    ..Default::default()
                }),
            )
            .await
            {
                Ok(reply) => {
                    let summary = reply
                        .summaries
                        .iter()
                        .find(|s| s.publishedfileid() == id || reply.summaries.len() == 1);
                    Response::UgcVote {
                        result: E_OK,
                        voted_up: summary.is_some_and(|s| s.vote_for()),
                        voted_down: summary.is_some_and(|s| s.vote_against()),
                        skipped: false,
                    }
                }
                Err(code) => Response::UgcVote {
                    result: code,
                    voted_up: false,
                    voted_down: false,
                    skipped: false,
                },
            }
        }
        UgcOp::Favorite { .. } => Response::UgcResult {
            // No evidence yet for the favorite request; fail rather than guess.
            result: E_FAIL,
        },
        UgcOp::Playtime { app, ids, start } => {
            let app = if app == 0 { app_id } else { app };
            if ids.len() > MAX_IDS {
                return Response::UgcResult {
                    result: E_INVALID_PARAM,
                };
            }
            let result = if ids.is_empty() {
                call(
                    connection.service_method(
                        CPublishedFile_StopPlaytimeTrackingForAllAppItems_Request {
                            appid: Some(app),
                            ..Default::default()
                        },
                    ),
                )
                .await
                .map(|_| ())
            } else if start {
                call(
                    connection.service_method(CPublishedFile_StartPlaytimeTracking_Request {
                        appid: Some(app),
                        publishedfileids: ids,
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ())
            } else {
                call(
                    connection.service_method(CPublishedFile_StopPlaytimeTracking_Request {
                        appid: Some(app),
                        publishedfileids: ids,
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ())
            };
            result_of(result)
        }
        UgcOp::Dependency { parent, child, add } => {
            let result = if add {
                call(connection.service_method(CPublishedFile_AddChild_Request {
                    publishedfileid: Some(parent),
                    child_publishedfileid: Some(child),
                    ..Default::default()
                }))
                .await
                .map(|_| ())
            } else {
                call(connection.service_method(CPublishedFile_RemoveChild_Request {
                    publishedfileid: Some(parent),
                    child_publishedfileid: Some(child),
                    ..Default::default()
                }))
                .await
                .map(|_| ())
            };
            result_of(result)
        }
        UgcOp::AppDependency { id, app, add } => {
            // Relationship 0 is the item -> app dependency.
            let result = if add {
                call(
                    connection.service_method(CPublishedFile_AddAppRelationship_Request {
                        publishedfileid: Some(id),
                        appid: Some(app),
                        relationship: Some(0),
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ())
            } else {
                call(
                    connection.service_method(CPublishedFile_RemoveAppRelationship_Request {
                        publishedfileid: Some(id),
                        appid: Some(app),
                        relationship: Some(0),
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ())
            };
            result_of(result)
        }
        UgcOp::GetAppDependencies { id } => {
            match call(
                connection.service_method(CPublishedFile_GetAppRelationships_Request {
                    publishedfileid: Some(id),
                    ..Default::default()
                }),
            )
            .await
            {
                Ok(reply) => Response::UgcAppDependencies {
                    result: E_OK,
                    apps: reply
                        .app_relationships
                        .iter()
                        .map(|r| r.appid())
                        .take(MAX_TAGS)
                        .collect(),
                },
                Err(code) => Response::UgcAppDependencies {
                    result: code,
                    apps: Vec::new(),
                },
            }
        }
        UgcOp::Snapshot | UgcOp::Download { .. } | UgcOp::Suspend { .. } => Response::Error {
            message: "Workshop install operations are served by the install service".into(),
        },
        UgcOp::Delete { app, id } => result_of(
            call(connection.service_method(CPublishedFile_Delete_Request {
                publishedfileid: Some(id),
                appid: Some(if app == 0 { app_id } else { app }),
                ..Default::default()
            }))
            .await
            .map(|_| ()),
        ),
    }
}

/// Runs one service call under the deadline; the error is an EResult.
async fn call<T>(
    future: impl std::future::Future<Output = Result<T, NetworkError>>,
) -> Result<T, i32> {
    match timeout(DEADLINE, future).await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => Err(failure_code(&error)),
        Err(_) => Err(E_TIMEOUT),
    }
}

fn result_of(result: Result<(), i32>) -> Response {
    Response::UgcResult {
        result: result.map_or_else(|code| code, |()| E_OK),
    }
}

async fn details(
    connection: &Connection,
    app_id: u32,
    ids: Vec<u64>,
    long_description: bool,
) -> Response {
    if ids.is_empty() || ids.len() > MAX_IDS {
        return Response::Error {
            message: "Invalid UGC details request".into(),
        };
    }
    let calls = ids.chunks(CHUNK).map(|chunk| {
        let request = CPublishedFile_GetDetails_Request {
            publishedfileids: chunk.to_vec(),
            includetags: Some(true),
            includemetadata: Some(true),
            includechildren: Some(true),
            includekvtags: Some(true),
            includeadditionalpreviews: Some(true),
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
    let mut found = Vec::new();
    for reply in replies {
        let Ok(reply) = reply else {
            return Response::Error {
                message: "UGC details request failed".into(),
            };
        };
        found.extend(reply.publishedfiledetails);
    }
    Response::UgcDetails {
        result: E_OK,
        items: items(&found, long_description),
    }
}

/// The SteamID64 of an individual account in the public universe.
fn steam_id_of(account: u32) -> u64 {
    76_561_197_960_265_728 + u64::from(account)
}

async fn query_files(connection: &Connection, app_id: u32, q: UgcQuery) -> Response {
    let consumer = if q.consumer_app == 0 { app_id } else { q.consumer_app };
    let creator = if q.creator_app == 0 { consumer } else { q.creator_app };
    let language = language(&q.language);
    let per_page = PER_PAGE;
    let page = q.page.max(1);
    let result = if let Some(list) = q.user_list {
        let Some(kind) = user_list_type(list) else {
            return Response::UgcQuery {
                result: E_FAIL,
                total: 0,
                cursor: String::new(),
                items: Vec::new(),
            };
        };
        let account = if q.account == 0 {
            connection.steam_id().steam64()
        } else {
            steam_id_of(q.account)
        };
        let request = CPublishedFile_GetUserFiles_Request {
            steamid: Some(account),
            appid: Some(consumer),
            creator_appid: Some(creator),
            page: Some(page),
            numperpage: Some(per_page),
            type_: Some(kind.to_owned()),
            sortmethod: Some(
                match q.sort_order {
                    0 => "lastupdated",
                    1 => "creationorder",
                    2 => "title",
                    3 => "lastupdated",
                    4 => "subscriptiondate",
                    _ => "lastupdated",
                }
                .to_owned(),
            ),
            requiredtags: q.required_tags.iter().take(pairs(&q.required_tags)).cloned().collect(),
            excludedtags: q.excluded_tags.iter().take(pairs(&q.excluded_tags)).cloned().collect(),
            required_kv_tags: q
                .kv_tags
                .iter()
                .take(MAX_TAGS)
                .map(|(key, value)| {
                    let mut tag = cpublished_file_get_user_files_request::KVTag::new();
                    tag.set_key(key.clone());
                    tag.set_value(value.clone());
                    tag
                })
                .collect(),
            filetype: matching_file_type(q.matching),
            match_cloud_filename: (!q.cloud_filename.is_empty()).then(|| q.cloud_filename.clone()),
            cache_max_age_seconds: (q.cache_age > 0).then_some(q.cache_age),
            language,
            totalonly: Some(q.total_only),
            ids_only: Some(q.ids_only),
            return_vote_data: Some(true),
            return_tags: Some(true),
            return_kv_tags: Some(q.key_value_tags),
            return_previews: Some(q.previews),
            return_children: Some(q.children),
            return_short_description: Some(!q.long_description),
            return_metadata: Some(q.metadata),
            return_playtime_stats: (q.playtime_days > 0).then_some(q.playtime_days),
            ..Default::default()
        };
        call(connection.service_method(request))
            .await
            .map(|r| (r.total(), String::new(), r.publishedfiledetails))
    } else {
        let request = CPublishedFile_QueryFiles_Request {
            query_type: Some(q.query_type),
            page: q.cursor.is_empty().then_some(page),
            cursor: (!q.cursor.is_empty()).then(|| q.cursor.clone()),
            numperpage: Some(per_page),
            creator_appid: Some(creator),
            appid: Some(consumer),
            requiredtags: q.required_tags.iter().take(pairs(&q.required_tags)).cloned().collect(),
            excludedtags: q.excluded_tags.iter().take(pairs(&q.excluded_tags)).cloned().collect(),
            match_all_tags: Some(!q.match_any_tag),
            search_text: (!q.search.is_empty()).then(|| q.search.clone()),
            filetype: matching_file_type(q.matching),
            days: (q.trend_days > 0).then_some(q.trend_days),
            cache_max_age_seconds: (q.cache_age > 0).then_some(q.cache_age),
            language,
            required_kv_tags: q
                .kv_tags
                .iter()
                .take(MAX_TAGS)
                .map(|(key, value)| {
                    let mut tag = cpublished_file_query_files_request::KVTag::new();
                    tag.set_key(key.clone());
                    tag.set_value(value.clone());
                    tag
                })
                .collect(),
            taggroups: q
                .tag_groups
                .iter()
                .take(MAX_TAGS)
                .map(|group| {
                    let mut tags = cpublished_file_query_files_request::TagGroup::new();
                    tags.tags = group.iter().take(MAX_TAGS).cloned().collect();
                    tags
                })
                .collect(),
            date_range_created: (q.created != (0, 0))
                .then(|| date_range(q.created))
                .into(),
            date_range_updated: (q.updated != (0, 0))
                .then(|| date_range(q.updated))
                .into(),
            totalonly: Some(q.total_only),
            ids_only: Some(q.ids_only),
            return_vote_data: Some(true),
            return_tags: Some(true),
            return_kv_tags: Some(q.key_value_tags),
            return_previews: Some(q.previews),
            return_children: Some(q.children),
            return_short_description: Some(!q.long_description),
            return_metadata: Some(q.metadata),
            return_playtime_stats: (q.playtime_days > 0).then_some(q.playtime_days),
            return_details: Some(!q.ids_only && !q.total_only),
            ..Default::default()
        };
        call(connection.service_method(request))
            .await
            .map(|r| (r.total(), r.next_cursor().to_owned(), r.publishedfiledetails))
    };
    match result {
        Ok((total, cursor, found)) => Response::UgcQuery {
            result: E_OK,
            total,
            cursor,
            items: items(&found, q.long_description),
        },
        Err(code) => Response::UgcQuery {
            result: code,
            total: 0,
            cursor: String::new(),
            items: Vec::new(),
        },
    }
}

fn date_range((start, end): (u32, u32)) -> cpublished_file_query_files_request::DateRange {
    let mut range = cpublished_file_query_files_request::DateRange::new();
    range.set_timestamp_start(start);
    range.set_timestamp_end(end);
    range
}

/// Every subscribed item of the app, one page at a time (no long descriptions).
pub(super) async fn subscribed_items(
    connection: &Connection,
    app: u32,
) -> Result<Vec<UgcItem>, i32> {
    let me = connection.steam_id().steam64();
    let mut found = Vec::new();
    for page in 1..=MAX_SUBSCRIBED_PAGES {
        let request = CPublishedFile_GetUserFiles_Request {
            steamid: Some(me),
            appid: Some(app),
            page: Some(page),
            numperpage: Some(PER_PAGE),
            type_: Some("mysubscriptions".to_owned()),
            sortmethod: Some("subscriptiondate".to_owned()),
            return_short_description: Some(true),
            ..Default::default()
        };
        let reply = call(connection.service_method(request)).await?;
        let count = reply.publishedfiledetails.len();
        let total = reply.total() as usize;
        found.extend(reply.publishedfiledetails);
        if count < PER_PAGE as usize || found.len() >= total {
            break;
        }
    }
    Ok(items(&found, false))
}

async fn subscribed(connection: &Connection, app: u32) -> Response {
    match subscribed_items(connection, app).await {
        Ok(items) => Response::UgcSubscribed {
            result: E_OK,
            items,
        },
        Err(result) => Response::UgcSubscribed {
            result,
            items: Vec::new(),
        },
    }
}

/// Details of `ids` (short descriptions), as the install service needs them.
pub(super) async fn item_details(
    connection: &Connection,
    app_id: u32,
    ids: Vec<u64>,
) -> Result<Vec<UgcItem>, i32> {
    match details(connection, app_id, ids, false).await {
        Response::UgcDetails { items, .. } => Ok(items),
        _ => Err(E_FAIL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_file_types_map() {
        assert_eq!(language("english"), Some(0));
        assert_eq!(language("SCHINESE"), Some(6));
        assert_eq!(language("latam"), Some(27));
        assert_eq!(language("klingon"), None);
        assert_eq!(matching_file_type(0), Some(0));
        assert_eq!(matching_file_type(3), Some(1));
        assert_eq!(matching_file_type(-1), None);
    }

    #[test]
    fn only_lists_the_service_accepts_are_mapped() {
        assert_eq!(user_list_type(6), Some("mysubscriptions"));
        assert_eq!(user_list_type(5), Some("myfavorites"));
        assert_eq!(user_list_type(0), Some("myfiles"));
        assert_eq!(user_list_type(2), None);
    }

    #[test]
    fn converting_bounds_metadata_and_collects_fields() {
        let mut d = PublishedFileDetails::new();
        d.set_publishedfileid(5);
        d.set_title("Item".into());
        d.set_metadata("m".repeat(10_000));
        d.set_subscriptions(7);
        let mut tag = steam_vent_proto_steam::steammessages_publishedfile_steamclient::published_file_details::Tag::new();
        tag.set_tag("Map".into());
        tag.set_display_name("Maps".into());
        d.tags.push(tag);
        let mut budget = 100;
        let item = convert(&d, false, &mut budget);
        assert_eq!(item.id, 5);
        assert_eq!(item.metadata.len(), 100);
        assert_eq!(budget, 0);
        assert_eq!(item.tags, "Map");
        assert_eq!(item.tag_list, vec![("Map".to_owned(), "Maps".to_owned())]);
        assert_eq!(item.stats[0], (0, 7));
        let second = convert(&d, false, &mut budget);
        assert!(second.metadata.is_empty());
    }
}
