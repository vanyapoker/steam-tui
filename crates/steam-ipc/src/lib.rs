//! Versioned local client/backend messages. Never carry login credentials.
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
pub mod schema;
pub mod storage;

pub const VERSION: u32 = 1;
/// Large enough for a Game Coordinator message (e.g. a full item cache) in hex.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub app_id: u32,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operation {
    Identity,
    /// Public Ed25519 key only. The private key stays in the game process.
    NetworkingCertificate {
        public_key: Vec<u8>,
    },
    NetworkingConfig,
    P2PSend {
        peer: u64,
        rendezvous: Vec<u8>,
    },
    P2PRecv,
    Ownership,
    /// Check another app (including DLC) using this backend's authenticated CM
    /// session. Request.app_id remains the running game's scope; tickets never
    /// leave the backend.
    AppOwnership {
        app_id: u32,
    },
    Persona,
    Country,
    AuthSessionTicket {
        capacity: u32,
    },
    WebAuthTicket {
        identity: Vec<u8>,
    },
    CancelAuthTicket {
        handle: u32,
    },
    /// Forward one Game Coordinator message (`payload` is hex) to the app's GC.
    GcSend {
        msg_type: u32,
        payload: String,
    },
    /// Pop the next queued Game Coordinator message, if any.
    GcRecv,
    /// Measured round-trip times to the Steam Datagram Relay data centers.
    PopPings,
    /// Lobby (matchmaking server) operations.
    Lobby(LobbyOp),
    /// Take every queued asynchronous Steamworks callback (lobby updates, chat, ...).
    PollCallbacks,
    /// Leaderboards, stats, achievements and global statistics (ISteamUserStats).
    Stats(StatsOp),
    /// Workshop/UGC item queries (ISteamUGC).
    Ugc(UgcOp),
    /// Fetch and decode one image from Valve's CDN (achievement icons, avatars).
    Image(ImageOp),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageOp {
    /// `url` must be an https URL on an allow-listed Valve image host.
    Fetch { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UgcOp {
    /// Details of published files by id.
    Details {
        ids: Vec<u64>,
        long_description: bool,
    },
    /// A page of published files (CreateQueryAll/UserUGCRequest + Send).
    Query(Box<UgcQuery>),
    /// Every file this account is subscribed to for the app (all pages, no descriptions).
    Subscribed { app: u32 },
    /// Subscribe to or unsubscribe from a published file.
    Subscribe { app: u32, id: u64, subscribe: bool },
    /// Vote up or down.
    Vote { id: u64, up: bool },
    /// The account's own vote on a file.
    GetVote { id: u64 },
    /// Add to or remove from favorites.
    Favorite { app: u32, id: u64, add: bool },
    /// Start or stop playtime tracking; empty `ids` stops tracking for all items.
    Playtime {
        app: u32,
        ids: Vec<u64>,
        start: bool,
    },
    /// Add or remove a parent -> child item dependency.
    Dependency { parent: u64, child: u64, add: bool },
    /// Add or remove an app dependency of an item.
    AppDependency { id: u64, app: u32, add: bool },
    /// The apps an item depends on.
    GetAppDependencies { id: u64 },
    /// Delete an item the account published.
    Delete { app: u32, id: u64 },
    /// Subscribed and tracked items with their install state.
    Snapshot,
    /// Queue a download/update of an item (subscribed or not).
    Download { id: u64, high_priority: bool },
    /// Pause or resume item downloads.
    Suspend { on: bool },
}

/// One Workshop item as the install service sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkshopItem {
    pub id: u64,
    pub subscribed: bool,
    pub installed: bool,
    /// Pre-SteamPipe item stored as a single file.
    pub legacy: bool,
    pub needs_update: bool,
    pub downloading: bool,
    pub pending: bool,
    /// Size on disk of the installed version.
    pub size: u64,
    /// Publication time of the installed version (unix seconds).
    pub time_updated: u32,
    /// Absolute folder of the installed item.
    pub path: String,
    /// Bytes downloaded and expected, while downloading.
    pub done: u64,
    pub total: u64,
    /// EResult of the last failed download (0 = none).
    pub failed: i32,
}

/// One ISteamUGC query: what the game set between Create...Request and Send.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UgcQuery {
    /// `Some(EUserUGCList)` for CreateQueryUserUGCRequest, `None` for all-UGC queries.
    pub user_list: Option<u32>,
    /// Account id for user queries (0 = this account).
    pub account: u32,
    pub sort_order: u32,
    /// EUGCQuery for all-UGC queries.
    pub query_type: u32,
    /// EUGCMatchingUGCType (-1 = all).
    pub matching: i32,
    pub creator_app: u32,
    pub consumer_app: u32,
    pub page: u32,
    pub cursor: String,
    pub required_tags: Vec<String>,
    pub excluded_tags: Vec<String>,
    /// Each group matches when any of its tags does.
    pub tag_groups: Vec<Vec<String>>,
    pub match_any_tag: bool,
    pub kv_tags: Vec<(String, String)>,
    pub search: String,
    pub trend_days: u32,
    pub language: String,
    pub cache_age: u32,
    pub cloud_filename: String,
    pub total_only: bool,
    pub ids_only: bool,
    pub long_description: bool,
    pub metadata: bool,
    pub key_value_tags: bool,
    pub children: bool,
    pub previews: bool,
    pub playtime_days: u32,
    pub created: (u32, u32),
    pub updated: (u32, u32),
}

/// An additional preview of an item (image or video).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UgcPreview {
    /// Image URL, or the video id for video previews.
    pub url: String,
    pub original: String,
    /// EItemPreviewType.
    pub kind: u32,
}

/// The fields of `SteamUGCDetails_t` the backend can fill, plus what the query getters read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UgcItem {
    pub id: u64,
    /// EResult for this item.
    pub result: i32,
    pub file_type: i32,
    pub creator_app: u32,
    pub consumer_app: u32,
    pub title: String,
    pub description: String,
    pub owner: u64,
    pub created: u32,
    pub updated: u32,
    pub visibility: i32,
    pub banned: bool,
    pub tags: String,
    pub file_handle: u64,
    pub preview_handle: u64,
    pub file_name: String,
    pub file_size: i32,
    pub preview_size: i32,
    pub url: String,
    /// Direct download URL of legacy (pre-SteamPipe) items.
    pub file_url: String,
    pub votes_up: u32,
    pub votes_down: u32,
    pub score: f32,
    pub children: u32,
    pub metadata: Vec<u8>,
    /// (tag, display name)
    pub tag_list: Vec<(String, String)>,
    pub preview_url: String,
    pub kv_tags: Vec<(String, String)>,
    pub previews: Vec<UgcPreview>,
    pub child_ids: Vec<u64>,
    /// (EItemStatistic, value)
    pub stats: Vec<(u32, u64)>,
    /// When the account subscribed (unix seconds), 0 if unknown.
    pub subscribed_at: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StatsOp {
    /// Find a leaderboard by name, creating it when `create` is set.
    FindLeaderboard {
        name: String,
        create: bool,
        sort: i32,
        display: i32,
    },
    /// ELeaderboardDataRequest 0 global, 1 around the user, 2 friends, 3 listed users.
    LeaderboardEntries {
        board: i32,
        request: i32,
        start: i32,
        end: i32,
        users: Vec<u64>,
    },
    UploadScore {
        board: i32,
        method: i32,
        score: i32,
        details: Vec<u8>,
    },
    AttachUgc {
        board: i32,
        ugc: u64,
    },
    /// Stats, achievement unlock times and the schema of `user` (0 = this account).
    GetStats {
        user: u64,
    },
    /// Changed raw stat values (`stat id`, `value`); `reset` clears every stat first.
    StoreStats {
        stats: Vec<(u32, u32)>,
        reset: bool,
    },
    CurrentPlayers,
    GlobalAchievementPercentages,
    /// Aggregated global stats by name, with `days` of history (0 = none).
    GlobalStats {
        names: Vec<String>,
        days: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardEntry {
    pub steam_id: u64,
    pub rank: i32,
    pub score: i32,
    pub details: Vec<u8>,
    pub ugc: u64,
}

/// One lobby search criterion (maps to ISteamMatchmaking filters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyFilter {
    pub key: String,
    pub value: String,
    pub comparison: i32,
    /// 0 string, 1 numerical, 2 slots available, 3 near value, 4 distance.
    pub kind: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LobbyOp {
    Create {
        kind: i32,
        max_members: i32,
    },
    Join {
        lobby: u64,
    },
    Leave {
        lobby: u64,
    },
    SetData {
        lobby: u64,
        key: String,
        value: String,
    },
    DeleteData {
        lobby: u64,
        key: String,
    },
    SetMemberData {
        lobby: u64,
        key: String,
        value: String,
    },
    SetType {
        lobby: u64,
        kind: i32,
    },
    SetJoinable {
        lobby: u64,
        joinable: bool,
    },
    SetMemberLimit {
        lobby: u64,
        max_members: i32,
    },
    SetOwner {
        lobby: u64,
        new_owner: u64,
    },
    SetGameServer {
        lobby: u64,
        ip: u32,
        port: u16,
        server: u64,
    },
    /// `data` is hex.
    Chat {
        lobby: u64,
        data: String,
    },
    ChatEntry {
        lobby: u64,
        id: u32,
    },
    Invite {
        lobby: u64,
        user: u64,
    },
    /// Snapshot of what the backend knows about a lobby.
    Get {
        lobby: u64,
    },
    RequestList {
        filters: Vec<LobbyFilter>,
        max_results: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyMember {
    pub steam_id: u64,
    pub name: String,
    pub data: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyInfo {
    pub lobby: u64,
    pub owner: u64,
    pub kind: i32,
    pub flags: i32,
    pub max_members: i32,
    pub data: Vec<(String, String)>,
    pub members: Vec<LobbyMember>,
    /// (game server steam id, ip, port)
    pub game_server: Option<(u64, u32, u16)>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub app_id: u32,
    pub response: Response,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdrRelay {
    pub ipv4: u32,
    pub port_start: u16,
    pub port_end: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdrPop {
    pub id: u32,
    pub relays: Vec<SdrRelay>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdrConfig {
    pub revision: u32,
    pub relay_public_key: String,
    pub pops: Vec<SdrPop>,
    pub typical_pings: Vec<(u32, u32, u32)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Response {
    NetworkingCertificate {
        signed: Vec<u8>,
    },
    NetworkingConfig {
        certs: Vec<String>,
        revoked_keys: Vec<u64>,
        #[serde(default)]
        sdr: Option<SdrConfig>,
    },
    P2PSent,
    P2PSignals(Vec<(u64, Vec<u8>)>),
    Identity {
        steam_id: u64,
        logged_on: bool,
    },
    Ownership {
        owned: bool,
    },
    Persona {
        steam_id: u64,
        name: String,
        state: Option<i32>,
    },
    Error {
        message: String,
    },
    AuthTicket {
        handle: u32,
        ticket: Vec<u8>,
    },
    AuthTicketPending {
        handle: u32,
    },
    Cancelled,
    Country {
        code: String,
    },
    PopPings {
        /// False until the first measurement finished.
        ready: bool,
        /// (POP code such as "ams", round-trip ms or -1 if unreachable)
        entries: Vec<(String, i32)>,
    },
    /// Result of a lobby create/join: EResult (or EChatRoomEnterResponse for join) and lobby id,
    /// plus the callback payloads (id, hex) the operation produced.
    LobbyResult {
        result: i32,
        lobby: u64,
        callbacks: Vec<(i32, String)>,
    },
    LobbyOk,
    LobbyFlag(bool),
    LobbyInfo(Option<LobbyInfo>),
    LobbyIds(Vec<u64>),
    LobbyChat {
        sender: u64,
        data: String,
    },
    Callbacks(Vec<(i32, String)>),
    Board {
        /// EResult of the lookup.
        result: i32,
        found: bool,
        id: i32,
        name: String,
        entries: i32,
        sort: i32,
        display: i32,
    },
    BoardEntries {
        /// EResult of the download.
        result: i32,
        total: i32,
        entries: Vec<BoardEntry>,
    },
    ScoreUploaded {
        success: bool,
        changed: bool,
        total: i32,
        rank_new: i32,
        rank_previous: i32,
    },
    /// EResult of an operation without further data (leaderboard UGC attach).
    StatsResult {
        result: i32,
    },
    StatsData {
        result: i32,
        crc: u32,
        /// Binary KeyValues stats schema, hex; empty when the server sent none.
        schema: String,
        stats: Vec<(u32, u32)>,
        /// (stat id, unlock time per bit)
        unlocks: Vec<(u32, Vec<u32>)>,
    },
    StatsStored {
        result: i32,
        crc: u32,
        /// (stat id, value the server reverted it to)
        failed: Vec<(u32, u32)>,
        out_of_date: bool,
    },
    Players {
        result: i32,
        count: i32,
    },
    Percentages {
        result: i32,
        /// (achievement name, percent of players who unlocked it)
        entries: Vec<(String, f32)>,
    },
    GlobalStats {
        result: i32,
        /// (stat name, total)
        totals: Vec<(String, i64)>,
        /// (stat name, per-day deltas, newest first)
        history: Vec<(String, Vec<i64>)>,
    },
    UgcDetails {
        result: i32,
        items: Vec<UgcItem>,
    },
    UgcQuery {
        result: i32,
        total: u32,
        cursor: String,
        items: Vec<UgcItem>,
    },
    /// The EResult of a Workshop write (subscribe, vote, favorite, ...).
    UgcResult {
        result: i32,
    },
    UgcSubscribed {
        result: i32,
        items: Vec<UgcItem>,
    },
    UgcVote {
        result: i32,
        voted_up: bool,
        voted_down: bool,
        skipped: bool,
    },
    UgcSnapshot {
        /// False when the service could not list subscriptions (offline).
        ready: bool,
        items: Vec<WorkshopItem>,
    },
    UgcAppDependencies {
        result: i32,
        apps: Vec<u32>,
    },
    /// Decoded image: `rgba` is hex of `width * height * 4` bytes.
    Image {
        width: u32,
        height: u32,
        rgba: String,
    },
    GcSent,
    GcNone,
    GcMessage {
        msg_type: u32,
        payload: String,
    },
}

pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

pub fn encode(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame too large",
        ));
    }
    Ok(bytes)
}

pub fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let bytes = encode(value)?;
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)
}

pub fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid IPC frame size",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_auth_callback_layout_and_failure_payloads_are_bounded_and_zeroed() {
        let callback = web_ticket_callback(7, 1, &[1, 2, 3]).unwrap();
        assert_eq!(callback.len(), 2572);
        assert_eq!(
            &callback[..15],
            &[7, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 1, 2, 3]
        );
        assert!(callback[15..].iter().all(|b| *b == 0));
        assert_eq!(web_ticket_callback(7, 2, &[]).unwrap()[8..], vec![0; 2564]);
        assert!(web_ticket_callback(0, 1, &[1]).is_none());
        assert!(web_ticket_callback(1, 1, &[]).is_none());
        assert!(web_ticket_callback(1, 2, &[1]).is_none());
        assert!(web_ticket_callback(1, 1, &[1; WEB_TICKET_MAX]).is_some());
        assert!(web_ticket_callback(1, 1, &[1; WEB_TICKET_MAX + 1]).is_none());
        let request = Request {
            version: VERSION,
            app_id: 480,
            operation: Operation::WebAuthTicket {
                identity: b"service".to_vec(),
            },
        };
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &request).unwrap();
        let decoded: Request = read_frame(&mut encoded.as_slice()).unwrap();
        assert!(
            matches!(decoded.operation, Operation::WebAuthTicket { identity } if identity == b"service")
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_and_reject_bad_frames_before_allocating() {
        let request = Request {
            version: VERSION,
            app_id: 440,
            operation: Operation::Identity,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded: Request = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.app_id, 440);
        for size in [0, MAX_FRAME as u32 + 1, u32::MAX] {
            assert!(read_frame::<Request>(&mut size.to_le_bytes().as_slice()).is_err());
        }
        bytes.pop();
        assert!(read_frame::<Request>(&mut bytes.as_slice()).is_err());
    }
}

/// GetTicketForWebApiResponse_t: SDK callback 168, four-byte fields and fixed buffer.
pub const WEB_TICKET_MAX: usize = 2560;
/// Valve's web-ticket entry converts service strings of at most 31 bytes.
pub const WEB_IDENTITY_MAX: usize = 31;
pub fn web_ticket_callback(handle: u32, result: i32, ticket: &[u8]) -> Option<Vec<u8>> {
    if handle == 0 || ticket.len() > WEB_TICKET_MAX || (result == 1) == ticket.is_empty() {
        return None;
    }
    let mut bytes = vec![0; 12 + WEB_TICKET_MAX];
    bytes[..4].copy_from_slice(&handle.to_le_bytes());
    bytes[4..8].copy_from_slice(&result.to_le_bytes());
    bytes[8..12].copy_from_slice(&(ticket.len() as i32).to_le_bytes());
    bytes[12..12 + ticket.len()].copy_from_slice(ticket);
    Some(bytes)
}
