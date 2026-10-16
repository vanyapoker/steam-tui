//! Versioned local client/backend messages. Never carry login credentials.
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
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
