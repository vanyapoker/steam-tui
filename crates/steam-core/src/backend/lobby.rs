//! Steam lobbies through the matchmaking servers (MMS) on the CM connection.
//! The backend owns the lobby state and turns server pushes into Steamworks
//! callback payloads that the game polls (`PollCallbacks`).
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
mod updates;

use futures_util::StreamExt;
use steam_ipc::{LobbyFilter, LobbyInfo, LobbyMember, LobbyOp, Response};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver_mms::{
    CMsgClientMMSCreateLobby, CMsgClientMMSCreateLobbyResponse, CMsgClientMMSGetLobbyList,
    CMsgClientMMSGetLobbyListResponse, CMsgClientMMSInviteToLobby, CMsgClientMMSJoinLobby,
    CMsgClientMMSJoinLobbyResponse, CMsgClientMMSLeaveLobby, CMsgClientMMSLobbyChatMsg,
    CMsgClientMMSLobbyData, CMsgClientMMSLobbyGameServerSet, CMsgClientMMSSendLobbyChatMsg,
    CMsgClientMMSSetLobbyData, CMsgClientMMSSetLobbyDataResponse, CMsgClientMMSSetLobbyGameServer,
    CMsgClientMMSSetLobbyOwner, CMsgClientMMSSetLobbyOwnerResponse, CMsgClientMMSUserJoinedLobby,
    CMsgClientMMSUserLeftLobby, cmsg_client_mmsget_lobby_list::Filter,
};

use super::events::{Direction, EventSender, emit};

// Steamworks callback ids (k_iSteamMatchmakingCallbacks = 500).
const LOBBY_ENTER: i32 = 504;
const LOBBY_DATA_UPDATE: i32 = 505;
const LOBBY_CHAT_UPDATE: i32 = 506;
const LOBBY_CHAT_MSG: i32 = 507;
const LOBBY_GAME_CREATED: i32 = 509;
const LOBBY_KICKED: i32 = 512;

const CHAT_ENTERED: u32 = 0x0001;
const CHAT_LEFT: u32 = 0x0002;
const ENTER_SUCCESS: i32 = 1;
const MAX_CHAT: usize = 256;
const MAX_QUEUED: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(10);

// ---- binary KeyValues (lobby metadata) ------------------------------------

/// Root key with the given string pairs: `00 "" (01 key 00 value 00)* 08`.
pub(super) fn kv_encode(pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = vec![0, 0];
    for (key, value) in pairs {
        out.push(1);
        out.extend(key.as_bytes().iter().filter(|b| **b != 0));
        out.push(0);
        out.extend(value.as_bytes().iter().filter(|b| **b != 0));
        out.push(0);
    }
    out.push(8);
    out
}

/// Flat string/int32 children of the root key; anything else is skipped.
pub(super) fn kv_decode(bytes: &[u8]) -> Vec<(String, String)> {
    fn cstr(bytes: &[u8], at: &mut usize) -> Option<String> {
        let end = bytes.get(*at..)?.iter().position(|b| *b == 0)? + *at;
        let text = String::from_utf8_lossy(&bytes[*at..end]).into_owned();
        *at = end + 1;
        Some(text)
    }
    let mut out = Vec::new();
    let mut at = 0;
    if bytes.first() != Some(&0) {
        return out;
    }
    at += 1;
    if cstr(bytes, &mut at).is_none() {
        return out;
    }
    while let Some(&kind) = bytes.get(at) {
        at += 1;
        match kind {
            8 => break,
            1 => {
                let (Some(key), Some(value)) = (cstr(bytes, &mut at), cstr(bytes, &mut at)) else {
                    break;
                };
                out.push((key, value));
            }
            2 => {
                let Some(key) = cstr(bytes, &mut at) else {
                    break;
                };
                let Some(raw) = bytes.get(at..at + 4) else {
                    break;
                };
                at += 4;
                out.push((key, i32::from_le_bytes(raw.try_into().unwrap()).to_string()));
            }
            _ => break,
        }
    }
    out
}

// ---- state -----------------------------------------------------------------

#[derive(Clone, Default)]
struct Member {
    name: String,
    data: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct Lobby {
    owner: u64,
    kind: i32,
    flags: i32,
    max_members: i32,
    data: Vec<(String, String)>,
    members: Vec<(u64, Member)>,
    game_server: Option<(u64, u32, u16)>,
    chat: VecDeque<(u64, Vec<u8>)>,
    /// How many chat entries were dropped from the front (ids stay stable).
    chat_base: u32,
}

#[derive(Default)]
struct State {
    lobbies: HashMap<u64, Lobby>,
    callbacks: VecDeque<(i32, Vec<u8>)>,
    updates: updates::Pending,
    left: std::collections::HashSet<u64>,
}

pub struct Lobbies {
    state: Arc<Mutex<State>>,
    app_id: u32,
    own: u64,
    name: String,
    cell_id: u32,
    public_ip: std::net::IpAddr,
    /// Connection whose headers carry the app id.
    connection: Connection,
    events: Option<EventSender>,
    update_notify: Arc<Notify>,
    update_worker: tokio::task::JoinHandle<()>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn u64b(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

fn push(state: &Mutex<State>, id: i32, data: Vec<u8>) {
    let mut state = lock(state);
    if state.callbacks.len() < MAX_QUEUED {
        state.callbacks.push_back((id, data));
    }
}

/// LobbyDataUpdate_t { u64 lobby; u64 member; u8 success } (pack 4 => 20 bytes).
fn data_update(lobby: u64, member: u64) -> Vec<u8> {
    let mut v = u64b(lobby).to_vec();
    v.extend(u64b(member));
    v.extend([1, 0, 0, 0]);
    v
}

/// LobbyEnter_t { u64 lobby; u32 permissions; bool locked; u32 response } (20 bytes).
pub(super) fn lobby_enter(lobby: u64, locked: bool, response: u32) -> Vec<u8> {
    let mut v = u64b(lobby).to_vec();
    v.extend(0u32.to_le_bytes());
    v.extend([u8::from(locked), 0, 0, 0]);
    v.extend(response.to_le_bytes());
    v
}

fn chat_update(lobby: u64, user: u64, changer: u64, change: u32) -> Vec<u8> {
    let mut v = u64b(lobby).to_vec();
    v.extend(u64b(user));
    v.extend(u64b(changer));
    v.extend(change.to_le_bytes());
    v
}

impl Lobbies {
    /// Starts the listeners for lobby pushes of this app.
    pub fn start(
        connection: &Connection,
        app_id: u32,
        persona: String,
        events: Option<EventSender>,
    ) -> Self {
        // Matchmaking messages must be routed to the app (header routing_appid).
        let connection = &connection.with_app_id(app_id);
        let state: Arc<Mutex<State>> = Arc::default();
        let own = connection.steam_id().steam64();
        macro_rules! listen {
            ($ty:ty, $handler:expr) => {{
                let state = state.clone();
                let events = events.clone();
                let mut stream = Box::pin(connection.on::<$ty>());
                tokio::spawn(async move {
                    while let Some(message) = stream.next().await {
                        if let Ok(message) = message {
                            #[allow(clippy::redundant_closure_call)]
                            ($handler)(&state, &events, message);
                        }
                    }
                });
            }};
        }
        listen!(
            CMsgClientMMSLobbyData,
            move |state: &Arc<Mutex<State>>,
                  events: &Option<EventSender>,
                  m: CMsgClientMMSLobbyData| {
                if m.app_id != Some(app_id) {
                    return;
                }
                apply_lobby_data(state, events, app_id, &m);
            }
        );
        listen!(
            CMsgClientMMSUserJoinedLobby,
            move |state: &Arc<Mutex<State>>,
                  events: &Option<EventSender>,
                  m: CMsgClientMMSUserJoinedLobby| {
                if m.app_id != Some(app_id) {
                    return;
                }
                let (lobby, user) = (m.steam_id_lobby(), m.steam_id_user());
                {
                    let mut s = lock(state);
                    if let Some(l) = s.lobbies.get_mut(&lobby)
                        && !l.members.iter().any(|(id, _)| *id == user)
                    {
                        l.members.push((
                            user,
                            Member {
                                name: m.persona_name().to_owned(),
                                data: Vec::new(),
                            },
                        ));
                    }
                }
                emit(
                    events,
                    app_id,
                    Direction::FromSteam,
                    "MMS",
                    "UserJoinedLobby",
                    format!("lobby {lobby} user {user}"),
                );
                push(
                    state,
                    LOBBY_CHAT_UPDATE,
                    chat_update(lobby, user, user, CHAT_ENTERED),
                );
            }
        );
        listen!(
            CMsgClientMMSUserLeftLobby,
            move |state: &Arc<Mutex<State>>,
                  events: &Option<EventSender>,
                  m: CMsgClientMMSUserLeftLobby| {
                if m.app_id != Some(app_id) {
                    return;
                }
                let (lobby, user) = (m.steam_id_lobby(), m.steam_id_user());
                if let Some(l) = lock(state).lobbies.get_mut(&lobby) {
                    l.members.retain(|(id, _)| *id != user);
                }
                emit(
                    events,
                    app_id,
                    Direction::FromSteam,
                    "MMS",
                    "UserLeftLobby",
                    format!("lobby {lobby} user {user}"),
                );
                push(
                    state,
                    LOBBY_CHAT_UPDATE,
                    chat_update(lobby, user, user, CHAT_LEFT),
                );
            }
        );
        listen!(
            CMsgClientMMSLobbyChatMsg,
            move |state: &Arc<Mutex<State>>,
                  events: &Option<EventSender>,
                  mut m: CMsgClientMMSLobbyChatMsg| {
                if m.app_id != Some(app_id) {
                    return;
                }
                let (lobby, sender) = (m.steam_id_lobby(), m.steam_id_sender());
                let data = m.take_lobby_message();
                emit(
                    events,
                    app_id,
                    Direction::FromSteam,
                    "MMS",
                    "LobbyChatMsg",
                    format!("lobby {lobby} from {sender}, {} bytes", data.len()),
                );
                let id = {
                    let mut s = lock(state);
                    let Some(l) = s.lobbies.get_mut(&lobby) else {
                        return;
                    };
                    if l.chat.len() >= MAX_CHAT {
                        l.chat.pop_front();
                        l.chat_base += 1;
                    }
                    l.chat.push_back((sender, data));
                    l.chat_base + l.chat.len() as u32 - 1
                };
                // LobbyChatMsg_t { u64 lobby; u64 user; u8 type; u32 chatID } (24 bytes).
                let mut v = u64b(lobby).to_vec();
                v.extend(u64b(sender));
                v.extend([1, 0, 0, 0]); // k_EChatEntryTypeChatMsg
                v.extend(id.to_le_bytes());
                push(state, LOBBY_CHAT_MSG, v);
            }
        );
        listen!(
            CMsgClientMMSLobbyGameServerSet,
            move |state: &Arc<Mutex<State>>,
                  events: &Option<EventSender>,
                  m: CMsgClientMMSLobbyGameServerSet| {
                if m.app_id != Some(app_id) {
                    return;
                }
                let lobby = m.steam_id_lobby();
                let ip = m.deprecated_game_server_ip();
                let port = m.game_server_port() as u16;
                let server = m.game_server_steam_id();
                if let Some(l) = lock(state).lobbies.get_mut(&lobby) {
                    l.game_server = Some((server, ip, port));
                }
                emit(
                    events,
                    app_id,
                    Direction::FromSteam,
                    "MMS",
                    "LobbyGameServerSet",
                    format!("lobby {lobby} server {server}"),
                );
                // LobbyGameCreated_t { u64 lobby; u64 server; u32 ip; u16 port } (24 bytes).
                let mut v = u64b(lobby).to_vec();
                v.extend(u64b(server));
                v.extend(ip.to_le_bytes());
                v.extend((port).to_le_bytes());
                v.extend([0, 0]);
                push(state, LOBBY_GAME_CREATED, v);
            }
        );
        let update_notify = Arc::new(Notify::new());
        let update_worker = updates::start(
            connection.clone(),
            state.clone(),
            update_notify.clone(),
            app_id,
            events.clone(),
        );
        Self {
            state,
            app_id,
            own,
            name: persona,
            cell_id: connection.cell_id(),
            public_ip: connection.public_ip(),
            connection: connection.clone(),
            events,
            update_notify,
            update_worker,
        }
    }

    pub fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        lock(&self.state).callbacks.drain(..).collect()
    }

    fn note(&self, direction: Direction, title: &str, detail: String) {
        emit(&self.events, self.app_id, direction, "MMS", title, detail);
    }

    fn snapshot(&self, lobby: u64) -> Option<LobbyInfo> {
        let s = lock(&self.state);
        let l = s.lobbies.get(&lobby)?;
        Some(LobbyInfo {
            lobby,
            owner: l.owner,
            kind: l.kind,
            flags: l.flags,
            max_members: l.max_members,
            data: l.data.clone(),
            members: l
                .members
                .iter()
                .map(|(id, m)| LobbyMember {
                    steam_id: *id,
                    name: m.name.clone(),
                    data: m.data.clone(),
                })
                .collect(),
            game_server: l.game_server,
        })
    }

    pub async fn handle(&self, _connection: &Connection, op: LobbyOp) -> Response {
        let connection = &self.connection;
        let app_id = Some(self.app_id);
        match op {
            LobbyOp::Create { kind, max_members } => {
                let request = CMsgClientMMSCreateLobby {
                    app_id,
                    max_members: Some(max_members),
                    lobby_type: Some(kind),
                    lobby_flags: Some(0),
                    cell_id: Some(self.cell_id),
                    metadata: Some(kv_encode(&[])),
                    persona_name_owner: Some(self.name.clone()),
                    public_ip: Some(ip_message(self.public_ip)).into(),
                    ..Default::default()
                };
                self.note(
                    Direction::ToSteam,
                    "MMSCreateLobby",
                    format!("type {kind}, max {max_members}"),
                );
                let reply = tokio::time::timeout(
                    TIMEOUT,
                    connection.job::<_, CMsgClientMMSCreateLobbyResponse>(request),
                )
                .await;
                let Ok(Ok(reply)) = reply else {
                    return Response::LobbyResult {
                        result: 16, // k_EResultTimeout
                        lobby: 0,
                        callbacks: Vec::new(),
                    };
                };
                let result = reply.eresult();
                let lobby = reply.steam_id_lobby();
                self.note(
                    Direction::FromSteam,
                    "MMSCreateLobbyResponse",
                    format!("result {result}, lobby {lobby}"),
                );
                if result != 1 || lobby == 0 {
                    return Response::LobbyResult {
                        result,
                        lobby: 0,
                        callbacks: Vec::new(),
                    };
                }
                lock(&self.state).left.remove(&lobby);
                lock(&self.state).lobbies.insert(
                    lobby,
                    Lobby {
                        owner: self.own,
                        kind,
                        max_members,
                        members: vec![(
                            self.own,
                            Member {
                                name: self.name.clone(),
                                data: Vec::new(),
                            },
                        )],
                        ..Default::default()
                    },
                );
                Response::LobbyResult {
                    result,
                    lobby,
                    callbacks: vec![(
                        LOBBY_ENTER,
                        steam_ipc::hex_encode(&lobby_enter(lobby, false, ENTER_SUCCESS as u32)),
                    )],
                }
            }
            LobbyOp::Join { lobby } => {
                let request = CMsgClientMMSJoinLobby {
                    app_id,
                    steam_id_lobby: Some(lobby),
                    persona_name: Some(self.name.clone()),
                    cell_id: Some(self.cell_id),
                    ..Default::default()
                };
                self.note(Direction::ToSteam, "MMSJoinLobby", format!("lobby {lobby}"));
                let reply = tokio::time::timeout(
                    TIMEOUT,
                    connection.job::<_, CMsgClientMMSJoinLobbyResponse>(request),
                )
                .await;
                let Ok(Ok(reply)) = reply else {
                    return Response::LobbyResult {
                        result: 5, // k_EChatRoomEnterResponseError
                        lobby,
                        callbacks: Vec::new(),
                    };
                };
                let response = reply.chat_room_enter_response();
                self.note(
                    Direction::FromSteam,
                    "MMSJoinLobbyResponse",
                    format!(
                        "lobby {lobby}, enter response {response}, {} members",
                        reply.members.len()
                    ),
                );
                if response != ENTER_SUCCESS {
                    return Response::LobbyResult {
                        result: response,
                        lobby,
                        callbacks: vec![(
                            LOBBY_ENTER,
                            steam_ipc::hex_encode(&lobby_enter(lobby, false, response as u32)),
                        )],
                    };
                }
                let members = reply
                    .members
                    .iter()
                    .map(|m| {
                        (
                            m.steam_id(),
                            Member {
                                name: m.persona_name().to_owned(),
                                data: kv_decode(m.metadata()),
                            },
                        )
                    })
                    .collect();
                lock(&self.state).left.remove(&lobby);
                lock(&self.state).lobbies.insert(
                    lobby,
                    Lobby {
                        owner: reply.steam_id_owner(),
                        kind: reply.lobby_type(),
                        flags: reply.lobby_flags(),
                        max_members: reply.max_members(),
                        data: kv_decode(reply.metadata()),
                        members,
                        ..Default::default()
                    },
                );
                Response::LobbyResult {
                    result: response,
                    lobby,
                    callbacks: vec![(
                        LOBBY_ENTER,
                        steam_ipc::hex_encode(&lobby_enter(lobby, false, response as u32)),
                    )],
                }
            }
            LobbyOp::Leave { lobby } => {
                self.note(
                    Direction::ToSteam,
                    "MMSLeaveLobby",
                    format!("lobby {lobby}"),
                );
                {
                    let mut state = lock(&self.state);
                    state.lobbies.remove(&lobby);
                    state.updates.cancel(lobby);
                    // Ignore late metadata pushes after we left; a successful
                    // explicit create/join below removes this tombstone.
                    if state.left.len() >= 4096 {
                        state.left.clear();
                    }
                    state.left.insert(lobby);
                }
                self.update_notify.notify_one();
                let sent = tokio::time::timeout(
                    Duration::from_secs(2),
                    connection.send(CMsgClientMMSLeaveLobby {
                        app_id,
                        steam_id_lobby: Some(lobby),
                        ..Default::default()
                    }),
                )
                .await
                .is_ok_and(|r| r.is_ok());
                if sent {
                    Response::LobbyOk
                } else {
                    Response::LobbyFlag(false)
                }
            }
            LobbyOp::SetData { lobby, key, value } => {
                if !updates::valid_pair(&key, &value) {
                    return Response::LobbyFlag(false);
                }
                self.set_lobby_data(connection, lobby, |l| {
                    set_pair(&mut l.data, key, Some(value))
                })
                .await
            }
            LobbyOp::DeleteData { lobby, key } => {
                self.set_lobby_data(connection, lobby, |l| set_pair(&mut l.data, key, None))
                    .await
            }
            LobbyOp::SetType { lobby, kind } => {
                self.set_lobby_data(connection, lobby, |l| l.kind = kind)
                    .await
            }
            LobbyOp::SetJoinable { lobby, joinable } => {
                // lobby_flags bit 0 = locked (not joinable).
                self.set_lobby_data(connection, lobby, |l| {
                    l.flags = if joinable { l.flags & !1 } else { l.flags | 1 }
                })
                .await
            }
            LobbyOp::SetMemberLimit { lobby, max_members } => {
                self.set_lobby_data(connection, lobby, |l| l.max_members = max_members)
                    .await
            }
            LobbyOp::SetMemberData { lobby, key, value } => {
                if !updates::valid_pair(&key, &value) {
                    return Response::LobbyFlag(false);
                }
                let mut s = lock(&self.state);
                if !s.updates.has_capacity((lobby, self.own)) {
                    return Response::LobbyFlag(false);
                }
                let Some(l) = s.lobbies.get_mut(&lobby) else {
                    return Response::LobbyFlag(false);
                };
                let Some((_, member)) = l.members.iter_mut().find(|(id, _)| *id == self.own) else {
                    return Response::LobbyFlag(false);
                };
                let mut data = member.data.clone();
                set_pair(&mut data, key, Some(value));
                if !updates::valid_metadata(&data) {
                    return Response::LobbyFlag(false);
                }
                member.data = data.clone();
                let request = updates::request(self.app_id, lobby, self.own, l, data);
                s.updates.enqueue(request);
                drop(s);
                self.update_notify.notify_one();
                Response::LobbyFlag(true)
            }
            LobbyOp::SetOwner { lobby, new_owner } => {
                let request = CMsgClientMMSSetLobbyOwner {
                    app_id,
                    steam_id_lobby: Some(lobby),
                    steam_id_new_owner: Some(new_owner),
                    ..Default::default()
                };
                self.note(
                    Direction::ToSteam,
                    "MMSSetLobbyOwner",
                    format!("lobby {lobby} -> {new_owner}"),
                );
                let ok = tokio::time::timeout(
                    TIMEOUT,
                    connection.job::<_, CMsgClientMMSSetLobbyOwnerResponse>(request),
                )
                .await
                .is_ok_and(|r| r.is_ok_and(|r| r.eresult() == 1));
                if ok && let Some(l) = lock(&self.state).lobbies.get_mut(&lobby) {
                    l.owner = new_owner;
                }
                Response::LobbyFlag(ok)
            }
            LobbyOp::SetGameServer {
                lobby,
                ip,
                port,
                server,
            } => {
                self.note(
                    Direction::ToSteam,
                    "MMSSetLobbyGameServer",
                    format!("lobby {lobby} server {server}"),
                );
                if let Some(l) = lock(&self.state).lobbies.get_mut(&lobby) {
                    l.game_server = Some((server, ip, port));
                }
                let sent = connection
                    .send(CMsgClientMMSSetLobbyGameServer {
                        app_id,
                        steam_id_lobby: Some(lobby),
                        deprecated_game_server_ip: Some(ip),
                        game_server_port: Some(u32::from(port)),
                        game_server_steam_id: Some(server),
                        ..Default::default()
                    })
                    .await
                    .is_ok();
                Response::LobbyFlag(sent)
            }
            LobbyOp::Chat { lobby, data } => {
                let Some(bytes) = steam_ipc::hex_decode(&data) else {
                    return Response::LobbyFlag(false);
                };
                self.note(
                    Direction::ToSteam,
                    "MMSSendLobbyChatMsg",
                    format!("lobby {lobby}, {} bytes", bytes.len()),
                );
                let sent = connection
                    .send(CMsgClientMMSSendLobbyChatMsg {
                        app_id,
                        steam_id_lobby: Some(lobby),
                        steam_id_target: Some(0),
                        lobby_message: Some(bytes),
                        ..Default::default()
                    })
                    .await
                    .is_ok();
                Response::LobbyFlag(sent)
            }
            LobbyOp::ChatEntry { lobby, id } => {
                let s = lock(&self.state);
                let entry = s.lobbies.get(&lobby).and_then(|l| {
                    let index = id.checked_sub(l.chat_base)? as usize;
                    l.chat.get(index).cloned()
                });
                match entry {
                    Some((sender, data)) => Response::LobbyChat {
                        sender,
                        data: steam_ipc::hex_encode(&data),
                    },
                    None => Response::LobbyFlag(false),
                }
            }
            LobbyOp::Invite { lobby, user } => {
                self.note(
                    Direction::ToSteam,
                    "MMSInviteToLobby",
                    format!("lobby {lobby} user {user}"),
                );
                let sent = connection
                    .send(CMsgClientMMSInviteToLobby {
                        app_id,
                        steam_id_lobby: Some(lobby),
                        steam_id_user_invited: Some(user),
                        ..Default::default()
                    })
                    .await
                    .is_ok();
                Response::LobbyFlag(sent)
            }
            LobbyOp::Get { lobby } => Response::LobbyInfo(self.snapshot(lobby)),
            LobbyOp::RequestList {
                filters,
                max_results,
            } => {
                let request = CMsgClientMMSGetLobbyList {
                    app_id,
                    num_lobbies_requested: Some(max_results),
                    cell_id: Some(self.cell_id),
                    public_ip: Some(ip_message(self.public_ip)).into(),
                    filters: filters.iter().map(to_filter).collect(),
                    ..Default::default()
                };
                self.note(
                    Direction::ToSteam,
                    "MMSGetLobbyList",
                    format!("{} filters, up to {max_results}", filters.len()),
                );
                let reply = tokio::time::timeout(
                    TIMEOUT,
                    connection.job::<_, CMsgClientMMSGetLobbyListResponse>(request),
                )
                .await;
                let Ok(Ok(reply)) = reply else {
                    return Response::LobbyIds(Vec::new());
                };
                self.note(
                    Direction::FromSteam,
                    "MMSGetLobbyListResponse",
                    format!("{} lobbies", reply.lobbies.len()),
                );
                // Remember what the list told us so GetLobbyData works on results.
                let mut s = lock(&self.state);
                let mut ids = Vec::new();
                for found in &reply.lobbies {
                    let id = found.steam_id();
                    ids.push(id);
                    s.lobbies.entry(id).or_insert_with(|| Lobby {
                        kind: found.lobby_type(),
                        flags: found.lobby_flags(),
                        max_members: found.max_members(),
                        data: kv_decode(found.metadata()),
                        ..Default::default()
                    });
                }
                Response::LobbyIds(ids)
            }
        }
    }

    /// Applies `change` to the stored lobby and pushes the full state to Steam
    /// (the server replaces the metadata blob). Only the owner may do this.
    async fn set_lobby_data(
        &self,
        _connection: &Connection,
        lobby: u64,
        change: impl FnOnce(&mut Lobby),
    ) -> Response {
        let mut s = lock(&self.state);
        if !s.updates.has_capacity((lobby, 0)) {
            return Response::LobbyFlag(false);
        }
        let Some(l) = s.lobbies.get_mut(&lobby) else {
            return Response::LobbyFlag(false);
        };
        if l.owner != self.own {
            return Response::LobbyFlag(false);
        }
        let mut changed = l.clone();
        change(&mut changed);
        if !updates::valid_metadata(&changed.data) {
            return Response::LobbyFlag(false);
        }
        let request = updates::request(self.app_id, lobby, 0, &changed, changed.data.clone());
        *l = changed;
        s.updates.enqueue(request);
        drop(s);
        self.update_notify.notify_one();
        Response::LobbyFlag(true)
    }
}

impl Drop for Lobbies {
    fn drop(&mut self) {
        self.update_worker.abort();
    }
}

fn ip_message(ip: std::net::IpAddr) -> steam_vent_proto_steam::steammessages_base::CMsgIPAddress {
    let mut message = steam_vent_proto_steam::steammessages_base::CMsgIPAddress::new();
    match ip {
        std::net::IpAddr::V4(v4) => message.set_v4(u32::from(v4)),
        std::net::IpAddr::V6(v6) => message.set_v6(v6.octets().to_vec()),
    }
    message
}

fn set_pair(pairs: &mut Vec<(String, String)>, key: String, value: Option<String>) {
    pairs.retain(|(k, _)| *k != key);
    if let Some(value) = value {
        pairs.push((key, value));
    }
}

fn to_filter(filter: &LobbyFilter) -> Filter {
    Filter {
        key: Some(filter.key.clone()),
        value: Some(filter.value.clone()),
        comparision: Some(filter.comparison),
        filter_type: Some(filter.kind),
        ..Default::default()
    }
}

/// Merges a server push (`ClientMMSLobbyData`) into the stored state and queues
/// the callbacks the game expects.
fn apply_lobby_data(
    state: &Mutex<State>,
    events: &Option<EventSender>,
    app_id: u32,
    m: &CMsgClientMMSLobbyData,
) {
    let lobby = m.steam_id_lobby();
    let members: Vec<(u64, Member)> = m
        .members
        .iter()
        .map(|member| {
            (
                member.steam_id(),
                Member {
                    name: member.persona_name().to_owned(),
                    data: kv_decode(member.metadata()),
                },
            )
        })
        .collect();
    let mut changed_members = Vec::new();
    {
        let mut s = lock(state);
        if s.left.contains(&lobby) {
            return;
        }
        let owner_pending = s.updates.contains((lobby, 0));
        let pending_members: std::collections::HashSet<_> = s.updates.members(lobby).collect();
        let entry = s.lobbies.entry(lobby).or_default();
        for (id, member) in &members {
            let previous = entry.members.iter().find(|(pid, _)| pid == id);
            if previous.is_none_or(|(_, old)| old.data != member.data) {
                changed_members.push(*id);
            }
        }
        entry.owner = m.steam_id_owner();
        if !owner_pending {
            entry.kind = m.lobby_type();
            entry.flags = m.lobby_flags();
            entry.max_members = m.max_members();
            entry.data = kv_decode(m.metadata());
        }
        if !members.is_empty() {
            let mut members = members;
            for (id, member) in &mut members {
                if pending_members.contains(id) {
                    if let Some((_, old)) = entry.members.iter().find(|(old_id, _)| old_id == id) {
                        member.data = old.data.clone();
                    }
                }
            }
            entry.members = members;
        }
    }
    emit(
        events,
        app_id,
        Direction::FromSteam,
        "MMS",
        "LobbyData",
        format!("lobby {lobby}, {} members", m.members.len()),
    );
    push(state, LOBBY_DATA_UPDATE, data_update(lobby, lobby));
    for member in changed_members {
        push(state, LOBBY_DATA_UPDATE, data_update(lobby, member));
    }
}

/// `LobbyKicked_t` for a lobby that vanished underneath us (used on disconnects).
#[allow(dead_code)]
pub(super) fn kicked(lobby: u64) -> (i32, Vec<u8>) {
    let mut v = u64b(lobby).to_vec();
    v.extend(u64b(lobby));
    v.extend([1, 0, 0, 0]);
    (LOBBY_KICKED, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_round_trips_and_decodes_int_children() {
        let pairs = vec![
            ("name".to_string(), "Rodeo".to_string()),
            ("map".to_string(), "dust".to_string()),
        ];
        let bytes = kv_encode(&pairs);
        assert_eq!(bytes[..2], [0, 0]);
        assert_eq!(*bytes.last().unwrap(), 8);
        assert_eq!(kv_decode(&bytes), pairs);
        // int32 child written by other clients
        let mut other = vec![0, b'x', 0, 2, b'n', 0];
        other.extend(7i32.to_le_bytes());
        other.push(8);
        assert_eq!(kv_decode(&other), vec![("n".to_string(), "7".to_string())]);
        assert!(kv_decode(&[]).is_empty());
        assert!(kv_decode(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn callback_payload_sizes_match_the_pack4_layouts() {
        assert_eq!(lobby_enter(1, false, 1).len(), 20);
        assert_eq!(data_update(1, 2).len(), 20);
        assert_eq!(chat_update(1, 2, 3, 1).len(), 28);
        assert_eq!(kicked(9).1.len(), 20);
    }

    #[test]
    fn old_push_cannot_overwrite_pending_edits_or_recreate_a_left_lobby() {
        let state = Mutex::new(State::default());
        let staged = Lobby {
            owner: 10,
            kind: 2,
            max_members: 4,
            data: vec![("map".into(), "new".into())],
            ..Default::default()
        };
        {
            let mut state = lock(&state);
            state
                .updates
                .enqueue(updates::request(480, 7, 0, &staged, staged.data.clone()));
            state.lobbies.insert(7, staged);
        }
        let old = CMsgClientMMSLobbyData {
            app_id: Some(480),
            steam_id_lobby: Some(7),
            steam_id_owner: Some(10),
            metadata: Some(kv_encode(&[("map".into(), "old".into())])),
            ..Default::default()
        };
        apply_lobby_data(&state, &None, 480, &old);
        assert_eq!(lock(&state).lobbies[&7].data[0].1, "new");
        {
            let mut state = lock(&state);
            state.lobbies.remove(&7);
            state.updates.cancel(7);
            state.left.insert(7);
            state.callbacks.clear();
        }
        apply_lobby_data(&state, &None, 480, &old);
        assert!(lock(&state).lobbies.is_empty());
        assert!(lock(&state).callbacks.is_empty());
    }

    #[test]
    fn set_pair_replaces_and_deletes() {
        let mut pairs = Vec::new();
        set_pair(&mut pairs, "a".into(), Some("1".into()));
        set_pair(&mut pairs, "a".into(), Some("2".into()));
        assert_eq!(pairs, vec![("a".to_string(), "2".to_string())]);
        set_pair(&mut pairs, "a".into(), None);
        assert!(pairs.is_empty());
    }
}
