//! ISteamUserStats over the CM connection: leaderboards (LBS), user stats and
//! achievements (`ClientGetUserStats` / `ClientStoreUserStats2`), the current
//! player count and global statistics. The game-side library keeps no state the
//! server does not confirm; every request here is one CM job with a deadline.
use std::time::Duration;

use protobuf::Message;
use steam_ipc::schema::Schema;
use steam_ipc::{BoardEntry, Response, StatsOp};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_common::protobuf;
use steam_vent_proto_common::{RpcMessage, RpcMessageWithKind};
use steam_vent_proto_steam::enums_clientserver::EMsg;
use steam_vent_proto_steam::steammessages_clientserver_2::{
    CMsgClientUGSGetGlobalStats, CMsgClientUGSGetGlobalStatsResponse,
    CMsgDPGetNumberOfCurrentPlayers, CMsgDPGetNumberOfCurrentPlayersResponse,
};
use steam_vent_proto_steam::steammessages_clientserver_lbs::{
    CMsgClientLBSFindOrCreateLB, CMsgClientLBSFindOrCreateLBResponse, CMsgClientLBSGetLBEntries,
    CMsgClientLBSGetLBEntriesResponse, CMsgClientLBSSetScore, CMsgClientLBSSetScoreResponse,
    CMsgClientLBSSetUGC, CMsgClientLBSSetUGCResponse,
};
use steam_vent_proto_steam::steammessages_clientserver_userstats::{
    CMsgClientGetUserStats, CMsgClientGetUserStatsResponse, CMsgClientStoreUserStats2,
    CMsgClientStoreUserStatsResponse, cmsg_client_store_user_stats2::Stats as StoreStat,
};
use tokio::time::timeout;

use super::events::{Direction, EventSender, emit};

const DEADLINE: Duration = Duration::from_secs(8);
/// Bounds a download so one reply always fits an IPC frame.
const MAX_ENTRIES: usize = 4096;
const MAX_DETAILS: usize = 256;
const MAX_NAME: usize = 256;
const MAX_STORE: usize = 4096;
const MAX_PERCENT_BYTES: usize = 2 * 1024 * 1024;
const E_OK: i32 = 1;

/// The generated bindings carry no message kind for the current-player count.
macro_rules! wire {
    ($name:ident($inner:ty), $kind:expr) => {
        #[derive(Debug)]
        struct $name($inner);
        impl RpcMessageWithKind for $name {
            type KindEnum = EMsg;
            const KIND: Self::KindEnum = $kind;
        }
        impl RpcMessage for $name {
            fn parse(reader: &mut dyn std::io::Read) -> protobuf::Result<Self> {
                Ok(Self(<$inner as Message>::parse_from_reader(reader)?))
            }
            fn write(&self, writer: &mut dyn std::io::Write) -> protobuf::Result<()> {
                self.0.write_to_writer(writer)
            }
            fn encode_size(&self) -> usize {
                self.0.compute_size() as usize
            }
        }
    };
}
wire!(
    PlayersRequest(CMsgDPGetNumberOfCurrentPlayers),
    EMsg::k_EMsgClientGetNumberOfCurrentPlayersDP
);
wire!(
    PlayersReply(CMsgDPGetNumberOfCurrentPlayersResponse),
    EMsg::k_EMsgClientGetNumberOfCurrentPlayersDPResponse
);

pub struct Stats {
    app_id: u32,
    events: Option<EventSender>,
    /// CRC the server last reported for this account's stats; echoed on stores.
    crc: u32,
    schema: Option<Schema>,
}

fn io_failure(what: &str) -> Response {
    Response::Error {
        message: format!("{what} failed or timed out"),
    }
}

impl Stats {
    pub fn new(app_id: u32, events: Option<EventSender>) -> Self {
        Self {
            app_id,
            events,
            crc: 0,
            schema: None,
        }
    }

    fn note(&self, title: &str, detail: String) {
        emit(
            &self.events,
            self.app_id,
            Direction::ToSteam,
            "CM",
            title,
            detail,
        );
    }

    pub async fn handle(&mut self, connection: &Connection, op: StatsOp) -> Response {
        // App-scoped CM messages are routed by the header's routing_appid.
        let connection = &connection.with_app_id(self.app_id);
        match op {
            StatsOp::FindLeaderboard {
                name,
                create,
                sort,
                display,
            } => self.find(connection, name, create, sort, display).await,
            StatsOp::LeaderboardEntries {
                board,
                request,
                start,
                end,
                users,
            } => {
                self.entries(connection, board, request, start, end, users)
                    .await
            }
            StatsOp::UploadScore {
                board,
                method,
                score,
                details,
            } => self.upload(connection, board, method, score, details).await,
            StatsOp::AttachUgc { board, ugc } => self.attach(connection, board, ugc).await,
            StatsOp::GetStats { user } => self.get_stats(connection, user).await,
            StatsOp::StoreStats { stats, reset } => self.store(connection, stats, reset).await,
            StatsOp::CurrentPlayers => self.players(connection).await,
            StatsOp::GlobalAchievementPercentages => self.percentages().await,
            StatsOp::GlobalStats { names, days } => self.global(connection, names, days).await,
        }
    }

    async fn find(
        &mut self,
        connection: &Connection,
        name: String,
        create: bool,
        sort: i32,
        display: i32,
    ) -> Response {
        if name.is_empty() || name.len() > MAX_NAME || name.contains('\0') {
            return Response::Error {
                message: "Invalid leaderboard name".into(),
            };
        }
        let request = CMsgClientLBSFindOrCreateLB {
            app_id: Some(self.app_id),
            // A plain lookup leaves the creation parameters unset.
            leaderboard_sort_method: create.then_some(sort),
            leaderboard_display_type: create.then_some(display),
            create_if_not_found: Some(create),
            leaderboard_name: Some(name.clone()),
            ..Default::default()
        };
        let reply = timeout(
            DEADLINE,
            connection.job::<_, CMsgClientLBSFindOrCreateLBResponse>(request),
        )
        .await;
        let Ok(Ok(reply)) = reply else {
            return io_failure("Leaderboard lookup");
        };
        let found = reply.eresult() == E_OK;
        self.note(
            "LBS FindOrCreateLeaderboard",
            format!(
                "{name}: {} (eresult {})",
                if found { "found" } else { "not found" },
                reply.eresult()
            ),
        );
        Response::Board {
            result: reply.eresult(),
            found,
            id: reply.leaderboard_id(),
            name: reply.leaderboard_name().to_owned(),
            entries: reply.leaderboard_entry_count(),
            sort: reply.leaderboard_sort_method(),
            display: reply.leaderboard_display_type(),
        }
    }

    async fn entries(
        &mut self,
        connection: &Connection,
        board: i32,
        request: i32,
        start: i32,
        end: i32,
        users: Vec<u64>,
    ) -> Response {
        if !(0..=3).contains(&request) || users.len() > MAX_ENTRIES {
            return Response::Error {
                message: "Invalid leaderboard download request".into(),
            };
        }
        let message = CMsgClientLBSGetLBEntries {
            app_id: Some(self.app_id as i32),
            leaderboard_id: Some(board),
            range_start: Some(start),
            range_end: Some(end),
            leaderboard_data_request: Some(request),
            steamids: users,
            ..Default::default()
        };
        let reply = timeout(
            DEADLINE,
            connection.job::<_, CMsgClientLBSGetLBEntriesResponse>(message),
        )
        .await;
        let Ok(Ok(reply)) = reply else {
            return io_failure("Leaderboard download");
        };
        let entries: Vec<BoardEntry> = reply
            .entries
            .iter()
            .take(MAX_ENTRIES)
            .map(|e| BoardEntry {
                steam_id: e.steam_id_user(),
                rank: e.global_rank(),
                score: e.score(),
                details: e.details().iter().take(MAX_DETAILS).copied().collect(),
                ugc: e.ugc_id(),
            })
            .collect();
        self.note(
            "LBS GetLeaderboardEntries",
            format!(
                "board {board}: {} of {} entries (eresult {})",
                entries.len(),
                reply.leaderboard_entry_count(),
                reply.eresult()
            ),
        );
        Response::BoardEntries {
            result: reply.eresult(),
            total: reply.leaderboard_entry_count(),
            entries,
        }
    }

    async fn upload(
        &mut self,
        connection: &Connection,
        board: i32,
        method: i32,
        score: i32,
        details: Vec<u8>,
    ) -> Response {
        if details.len() > MAX_DETAILS {
            return Response::Error {
                message: "Leaderboard score details are too large".into(),
            };
        }
        let message = CMsgClientLBSSetScore {
            app_id: Some(self.app_id),
            leaderboard_id: Some(board),
            score: Some(score),
            details: Some(details),
            upload_score_method: Some(method),
            ..Default::default()
        };
        let reply = timeout(
            DEADLINE,
            connection.job::<_, CMsgClientLBSSetScoreResponse>(message),
        )
        .await;
        let Ok(Ok(reply)) = reply else {
            return io_failure("Leaderboard score upload");
        };
        self.note(
            "LBS SetScore",
            format!(
                "board {board}: score {score}, eresult {}, changed {}",
                reply.eresult(),
                reply.score_changed()
            ),
        );
        Response::ScoreUploaded {
            success: reply.eresult() == E_OK,
            changed: reply.score_changed(),
            total: reply.leaderboard_entry_count(),
            rank_new: reply.global_rank_new(),
            rank_previous: reply.global_rank_previous(),
        }
    }

    async fn attach(&mut self, connection: &Connection, board: i32, ugc: u64) -> Response {
        let message = CMsgClientLBSSetUGC {
            app_id: Some(self.app_id),
            leaderboard_id: Some(board),
            ugc_id: Some(ugc),
            ..Default::default()
        };
        let reply = timeout(
            DEADLINE,
            connection.job::<_, CMsgClientLBSSetUGCResponse>(message),
        )
        .await;
        match reply {
            Ok(Ok(reply)) => Response::StatsResult {
                result: reply.eresult(),
            },
            _ => io_failure("Leaderboard UGC attach"),
        }
    }

    async fn fetch_stats(
        &mut self,
        connection: &Connection,
        user: u64,
    ) -> Result<CMsgClientGetUserStatsResponse, Response> {
        let own = connection.steam_id().steam64();
        let target = if user == 0 { own } else { user };
        let message = CMsgClientGetUserStats {
            game_id: Some(u64::from(self.app_id)),
            crc_stats: Some(0),
            // -1: never matches the server's schema version, so it always sends one.
            schema_local_version: Some(-1),
            steam_id_for_user: Some(target),
            ..Default::default()
        };
        match timeout(
            DEADLINE,
            connection.job::<_, CMsgClientGetUserStatsResponse>(message),
        )
        .await
        {
            Ok(Ok(reply)) => {
                if target == own {
                    self.crc = reply.crc_stats();
                    if !reply.schema().is_empty() {
                        self.schema = Schema::parse(reply.schema(), "english");
                    }
                }
                Ok(reply)
            }
            _ => Err(io_failure("User stats request")),
        }
    }

    async fn get_stats(&mut self, connection: &Connection, user: u64) -> Response {
        let reply = match self.fetch_stats(connection, user).await {
            Ok(reply) => reply,
            Err(error) => return error,
        };
        self.note(
            "ClientGetUserStats",
            format!(
                "{} stats, {} achievement blocks, schema {} bytes (eresult {})",
                reply.stats.len(),
                reply.achievement_blocks.len(),
                reply.schema().len(),
                reply.eresult()
            ),
        );
        Response::StatsData {
            result: reply.eresult(),
            crc: reply.crc_stats(),
            schema: steam_ipc::hex_encode(reply.schema()),
            stats: reply
                .stats
                .iter()
                .take(MAX_STORE)
                .map(|s| (s.stat_id(), s.stat_value()))
                .collect(),
            unlocks: reply
                .achievement_blocks
                .iter()
                .take(MAX_STORE)
                .map(|b| (b.achievement_id(), b.unlock_time.clone()))
                .collect(),
        }
    }

    async fn store(
        &mut self,
        connection: &Connection,
        stats: Vec<(u32, u32)>,
        reset: bool,
    ) -> Response {
        if stats.len() > MAX_STORE {
            return Response::Error {
                message: "Too many stats to store".into(),
            };
        }
        let own = connection.steam_id().steam64();
        let mut retried = false;
        loop {
            let message = CMsgClientStoreUserStats2 {
                game_id: Some(u64::from(self.app_id)),
                settor_steam_id: Some(own),
                settee_steam_id: Some(own),
                crc_stats: Some(self.crc),
                explicit_reset: Some(reset),
                stats: stats
                    .iter()
                    .map(|&(stat_id, stat_value)| StoreStat {
                        stat_id: Some(stat_id),
                        stat_value: Some(stat_value),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let reply = match timeout(
                DEADLINE,
                connection.job::<_, CMsgClientStoreUserStatsResponse>(message),
            )
            .await
            {
                Ok(Ok(reply)) => reply,
                _ => return io_failure("Storing user stats"),
            };
            if reply.stats_out_of_date() && !retried {
                // Our CRC is stale: adopt the server's baseline and send once more.
                retried = true;
                if let Err(error) = self.fetch_stats(connection, 0).await {
                    return error;
                }
                continue;
            }
            if reply.eresult() == E_OK {
                self.crc = reply.crc_stats();
            }
            self.note(
                "ClientStoreUserStats2",
                format!(
                    "{} stats stored, eresult {}, {} reverted",
                    stats.len(),
                    reply.eresult(),
                    reply.stats_failed_validation.len()
                ),
            );
            return Response::StatsStored {
                result: reply.eresult(),
                crc: reply.crc_stats(),
                failed: reply
                    .stats_failed_validation
                    .iter()
                    .map(|f| (f.stat_id(), f.reverted_stat_value()))
                    .collect(),
                out_of_date: reply.stats_out_of_date(),
            };
        }
    }

    async fn players(&mut self, connection: &Connection) -> Response {
        let message = PlayersRequest(CMsgDPGetNumberOfCurrentPlayers {
            appid: Some(self.app_id),
            ..Default::default()
        });
        match timeout(DEADLINE, connection.job::<_, PlayersReply>(message)).await {
            Ok(Ok(PlayersReply(reply))) => Response::Players {
                result: reply.eresult(),
                count: reply.player_count(),
            },
            _ => io_failure("Current player count"),
        }
    }

    /// Public Web API: there is no CM message for global achievement percentages.
    async fn percentages(&mut self) -> Response {
        let url = format!(
            "https://api.steampowered.com/ISteamUserStats/GetGlobalAchievementPercentagesForApp/v2/?gameid={}",
            self.app_id
        );
        let result = async {
            let http = reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(DEADLINE)
                .user_agent("steam-tui")
                .build()
                .ok()?;
            let response = http.get(url).send().await.ok()?;
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|n| n as usize > MAX_PERCENT_BYTES)
            {
                return None;
            }
            let bytes = response.bytes().await.ok()?;
            if bytes.len() > MAX_PERCENT_BYTES {
                return None;
            }
            parse_percentages(&bytes)
        }
        .await;
        match result {
            Some(entries) => Response::Percentages {
                result: E_OK,
                entries,
            },
            None => Response::Percentages {
                result: 2,
                entries: Vec::new(),
            },
        }
    }

    async fn global(&mut self, connection: &Connection, names: Vec<String>, days: u32) -> Response {
        if names.len() > MAX_STORE || days > 60 {
            return Response::Error {
                message: "Invalid global stats request".into(),
            };
        }
        if self.schema.is_none()
            && let Err(error) = self.fetch_stats(connection, 0).await
        {
            return error;
        }
        let ids: std::collections::BTreeMap<u32, String> = self
            .schema
            .as_ref()
            .map(|s| {
                s.names_by_id()
                    .into_iter()
                    .filter(|(_, name)| names.iter().any(|n| n == name))
                    .map(|(id, name)| (id, name.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let message = CMsgClientUGSGetGlobalStats {
            gameid: Some(u64::from(self.app_id)),
            history_days_requested: Some(days),
            ..Default::default()
        };
        let reply = match timeout(
            DEADLINE,
            connection.job::<_, CMsgClientUGSGetGlobalStatsResponse>(message),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            _ => return io_failure("Global stats request"),
        };
        let (totals, history) = global_series(&reply, &ids);
        self.note(
            "ClientUGSGetGlobalStats",
            format!(
                "{} days, {} stats (eresult {})",
                reply.days.len(),
                totals.len(),
                reply.eresult()
            ),
        );
        Response::GlobalStats {
            result: reply.eresult(),
            totals,
            history,
        }
    }
}

fn parse_percentages(bytes: &[u8]) -> Option<Vec<(String, f32)>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let list = value
        .get("achievementpercentages")?
        .get("achievements")?
        .as_array()?;
    Some(
        list.iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_owned();
                let percent = match item.get("percent")? {
                    serde_json::Value::Number(n) => n.as_f64()?,
                    serde_json::Value::String(s) => s.parse().ok()?,
                    _ => return None,
                };
                (name.len() <= MAX_NAME).then_some((name, percent as f32))
            })
            .take(MAX_STORE)
            .collect(),
    )
}

/// Totals are the day-0 aggregate when the server sends one, else the sum of the
/// returned days; history is per-day deltas, newest first.
fn global_series(
    reply: &CMsgClientUGSGetGlobalStatsResponse,
    names: &std::collections::BTreeMap<u32, String>,
) -> (Vec<(String, i64)>, Vec<(String, Vec<i64>)>) {
    let mut totals: std::collections::BTreeMap<&str, i64> = Default::default();
    let mut aggregate: std::collections::BTreeMap<&str, i64> = Default::default();
    let mut history: std::collections::BTreeMap<&str, Vec<(u32, i64)>> = Default::default();
    for day in &reply.days {
        for stat in &day.stats {
            let Some(name) = u32::try_from(stat.stat_id())
                .ok()
                .and_then(|id| names.get(&id))
            else {
                continue;
            };
            if day.day_id() == 0 {
                aggregate.insert(name, stat.data());
            } else {
                *totals.entry(name).or_default() += stat.data();
                history
                    .entry(name)
                    .or_default()
                    .push((day.day_id(), stat.data()));
            }
        }
    }
    let mut totals_out: Vec<(String, i64)> = totals
        .into_iter()
        .map(|(name, sum)| (name.to_owned(), aggregate.get(name).copied().unwrap_or(sum)))
        .collect();
    for (name, value) in aggregate {
        if !totals_out.iter().any(|(n, _)| n == name) {
            totals_out.push((name.to_owned(), value));
        }
    }
    let history_out = history
        .into_iter()
        .map(|(name, mut days)| {
            days.sort_by_key(|(day, _)| std::cmp::Reverse(*day));
            (name.to_owned(), days.into_iter().map(|(_, v)| v).collect())
        })
        .collect();
    (totals_out, history_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_api_percentages_accept_numbers_and_strings() {
        let json = br#"{"achievementpercentages":{"achievements":[
            {"name":"A","percent":12.5},{"name":"B","percent":"3.25"},{"name":"C"}]}}"#;
        let parsed = parse_percentages(json).unwrap();
        assert_eq!(parsed, vec![("A".to_owned(), 12.5), ("B".to_owned(), 3.25)]);
        assert!(parse_percentages(b"{}").is_none());
    }

    #[test]
    fn global_series_prefers_the_aggregate_and_orders_history_newest_first() {
        use steam_vent_proto_steam::steammessages_clientserver_2::cmsg_client_ugsget_global_stats_response::{Day, day::Stat};
        let day = |id: u32, value: i64| Day {
            day_id: Some(id),
            stats: vec![Stat {
                stat_id: Some(7),
                data: Some(value),
                ..Default::default()
            }],
            ..Default::default()
        };
        let reply = CMsgClientUGSGetGlobalStatsResponse {
            days: vec![day(0, 1000), day(10, 5), day(12, 7), day(11, 6)],
            ..Default::default()
        };
        let names = [(7u32, "kills".to_owned())].into();
        let (totals, history) = global_series(&reply, &names);
        assert_eq!(totals, vec![("kills".to_owned(), 1000)]);
        assert_eq!(history, vec![("kills".to_owned(), vec![7, 6, 5])]);
        let without = CMsgClientUGSGetGlobalStatsResponse {
            days: vec![day(10, 5), day(11, 6)],
            ..Default::default()
        };
        assert_eq!(
            global_series(&without, &names).0,
            vec![("kills".to_owned(), 11)]
        );
    }
}
