//! ISteamFriends for one app: the account's live social graph (shared with the
//! TUI's friends loop), personas of other users, this account's rich presence
//! and friends' rich presence. Changes become Steamworks callbacks the game
//! polls (`PollCallbacks`).
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use steam_ipc::{FriendInfo, FriendsOp, Response, valid_rich_presence};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver_2::{
    CMsgClientRichPresenceInfo, CMsgClientRichPresenceRequest, CMsgClientRichPresenceUpload,
};
use steam_vent_proto_steam::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;
use tokio::sync::Notify;

use super::events::{Direction, EventSender, emit};
use super::lobby::kv_decode;

// Steamworks callback ids (k_iSteamFriendsCallbacks = 300).
const PERSONA_STATE_CHANGE: i32 = 304;
const FRIEND_RICH_PRESENCE_UPDATE: i32 = 336;

// EPersonaChange flags.
const CHANGE_NAME: i32 = 0x0001;
const CHANGE_STATUS: i32 = 0x0002;
const CHANGE_COME_ONLINE: i32 = 0x0004;
const CHANGE_GONE_OFFLINE: i32 = 0x0008;
const CHANGE_GAME_PLAYED: i32 = 0x0010;
const CHANGE_GAME_SERVER: i32 = 0x0020;
const CHANGE_RELATIONSHIP: i32 = 0x0200;
const CHANGE_NAME_FIRST_SET: i32 = 0x0400;
const CHANGE_RICH_PRESENCE: i32 = 0x4000;

const MAX_QUEUED: usize = 4096;
const MAX_USERS: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Rich presence uploads are coalesced: at most one per interval, always
/// ending with the latest state.
const UPLOAD_INTERVAL: Duration = Duration::from_secs(1);
/// A friend's rich presence received this recently answers a new request
/// directly, without asking Steam again.
const FRESH: Duration = Duration::from_secs(1);

/// Binary KeyValues with a named root: `00 name 00 (01 key 00 value 00)* 08 08`.
fn kv_encode_root(root: &str, pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend(root.as_bytes());
    out.push(0);
    for (key, value) in pairs {
        out.push(1);
        out.extend(key.as_bytes());
        out.push(0);
        out.extend(value.as_bytes());
        out.push(0);
    }
    out.extend([8, 8]);
    out
}

/// PersonaStateChange_t { u64 steamid; i32 flags } (pack 4 => 12 bytes).
fn persona_change(user: u64, flags: i32) -> Vec<u8> {
    let mut v = user.to_le_bytes().to_vec();
    v.extend(flags.to_le_bytes());
    v
}

/// FriendRichPresenceUpdate_t { u64 friend; u32 app } (pack 4 => 12 bytes).
fn rich_presence_update(user: u64, app_id: u32) -> Vec<u8> {
    let mut v = user.to_le_bytes().to_vec();
    v.extend(app_id.to_le_bytes());
    v
}

/// EPersonaChange flags between two views of the same user.
fn changes(old: &FriendInfo, new: &FriendInfo) -> i32 {
    let mut flags = 0;
    if old.name != new.name {
        flags |= if old.name.is_empty() {
            CHANGE_NAME | CHANGE_NAME_FIRST_SET
        } else {
            CHANGE_NAME
        };
    }
    if old.state != new.state {
        flags |= CHANGE_STATUS;
        if old.state == 0 {
            flags |= CHANGE_COME_ONLINE;
        } else if new.state == 0 {
            flags |= CHANGE_GONE_OFFLINE;
        }
    }
    if old.game_id != new.game_id {
        flags |= CHANGE_GAME_PLAYED;
    }
    if (old.game_ip, old.game_port, old.query_port) != (new.game_ip, new.game_port, new.query_port)
    {
        flags |= CHANGE_GAME_SERVER;
    }
    if old.relationship != new.relationship {
        flags |= CHANGE_RELATIONSHIP;
    }
    if old.rich_presence != new.rich_presence {
        flags |= CHANGE_RICH_PRESENCE;
    }
    flags
}

fn app_of(game_id: u64) -> u32 {
    // Only GameID type 0 is a Steam app; its low 24 bits are the app id.
    if (game_id >> 24) & 0xFF == 0 {
        (game_id & 0x00FF_FFFF) as u32
    } else {
        0
    }
}

#[derive(Default)]
struct State {
    /// Last social snapshot the game saw, for change callbacks.
    seen: Option<(u64, Arc<Vec<FriendInfo>>)>,
    /// Personas of users who are not friends.
    users: HashMap<u64, FriendInfo>,
    users_requested: HashSet<u64>,
    /// Rich presence Steam answered to RequestFriendRichPresence.
    rich: HashMap<u64, (Instant, Vec<(String, String)>)>,
    rich_requested: HashSet<u64>,
    /// This account's rich presence and whether Steam has the latest.
    own: Vec<(String, String)>,
    uploaded: bool,
    callbacks: VecDeque<(i32, Vec<u8>)>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn push(state: &mut State, id: i32, data: Vec<u8>) {
    if state.callbacks.len() < MAX_QUEUED {
        state.callbacks.push_back((id, data));
    }
}

pub struct Friends {
    state: Arc<Mutex<State>>,
    app_id: u32,
    own: u64,
    /// Connection whose headers carry the app id.
    connection: Connection,
    events: Option<EventSender>,
    upload_notify: Arc<Notify>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl Friends {
    pub fn start(connection: &Connection, app_id: u32, events: Option<EventSender>) -> Self {
        let connection = connection.with_app_id(app_id);
        let state: Arc<Mutex<State>> = Arc::default();
        let upload_notify = Arc::new(Notify::new());
        let mut workers = Vec::new();

        let mut answers = Box::pin(connection.on::<CMsgClientRichPresenceInfo>());
        let listener_state = state.clone();
        let listener_events = events.clone();
        workers.push(tokio::spawn(async move {
            while let Some(message) = answers.next().await {
                let Ok(message) = message else { continue };
                for entry in message.rich_presence {
                    let user = entry.steamid_user();
                    let pairs = kv_decode(entry.rich_presence_kv());
                    emit(
                        &listener_events,
                        app_id,
                        Direction::FromSteam,
                        "Friends",
                        "RichPresenceInfo",
                        format!("user {user}, {} keys", pairs.len()),
                    );
                    let mut s = lock(&listener_state);
                    s.rich_requested.remove(&user);
                    if s.rich.len() >= MAX_USERS && !s.rich.contains_key(&user) {
                        s.rich.clear();
                    }
                    s.rich.insert(user, (Instant::now(), pairs));
                    push(
                        &mut s,
                        FRIEND_RICH_PRESENCE_UPDATE,
                        rich_presence_update(user, app_id),
                    );
                }
            }
        }));

        let uploader_state = state.clone();
        let uploader_notify = upload_notify.clone();
        let uploader_connection = connection.clone();
        let uploader_events = events.clone();
        workers.push(tokio::spawn(async move {
            loop {
                uploader_notify.notified().await;
                let pairs = {
                    let mut s = lock(&uploader_state);
                    if s.uploaded {
                        continue;
                    }
                    s.uploaded = true;
                    s.own.clone()
                };
                emit(
                    &uploader_events,
                    app_id,
                    Direction::ToSteam,
                    "Friends",
                    "RichPresenceUpload",
                    format!("{} keys", pairs.len()),
                );
                let sent = tokio::time::timeout(
                    TIMEOUT,
                    uploader_connection.send(CMsgClientRichPresenceUpload {
                        rich_presence_kv: Some(kv_encode_root("RP", &pairs)),
                        ..Default::default()
                    }),
                )
                .await
                .is_ok_and(|r| r.is_ok());
                if !sent {
                    // Keep the latest state pending; the next change or the
                    // retry below delivers it.
                    lock(&uploader_state).uploaded = false;
                    uploader_notify.notify_one();
                }
                tokio::time::sleep(UPLOAD_INTERVAL).await;
            }
        }));

        Self {
            state,
            app_id,
            own: connection.steam_id().steam64(),
            connection,
            events,
            upload_notify,
            workers,
        }
    }

    fn note(&self, direction: Direction, title: &str, detail: String) {
        emit(
            &self.events,
            self.app_id,
            direction,
            "Friends",
            title,
            detail,
        );
    }

    fn social(&self) -> Arc<Vec<FriendInfo>> {
        crate::friends::social(self.own)
            .map(|social| social.snapshot().1)
            .unwrap_or_default()
    }

    /// The social graph as the game sees it: friends' rich presence answered
    /// by RequestFriendRichPresence fills in what persona pushes left out.
    fn friends(&self) -> Vec<FriendInfo> {
        let social = self.social();
        let s = lock(&self.state);
        social
            .iter()
            .map(|friend| {
                let mut friend = friend.clone();
                if friend.rich_presence.is_empty()
                    && let Some((_, pairs)) = s.rich.get(&friend.steam_id)
                {
                    friend.rich_presence = pairs.clone();
                }
                friend
            })
            .collect()
    }

    pub async fn handle(&self, op: FriendsOp, lobby_name: Option<String>) -> Response {
        match op {
            FriendsOp::Snapshot => Response::Friends(self.friends()),
            FriendsOp::User { steam_id, request } => self.user(steam_id, request, lobby_name),
            FriendsOp::SetRichPresence { pairs } => {
                if !valid_rich_presence(&pairs) {
                    return Response::FriendsFlag(false);
                }
                {
                    let mut s = lock(&self.state);
                    if s.own == pairs {
                        return Response::FriendsFlag(true);
                    }
                    s.own = pairs;
                    s.uploaded = false;
                }
                self.upload_notify.notify_one();
                Response::FriendsFlag(true)
            }
            FriendsOp::RequestRichPresence { steam_id } => {
                if (steam_id >> 52) & 0xF != 1 || steam_id as u32 == 0 {
                    return Response::FriendsFlag(false);
                }
                {
                    let mut s = lock(&self.state);
                    let fresh = s
                        .rich
                        .get(&steam_id)
                        .is_some_and(|(at, _)| at.elapsed() < FRESH);
                    if fresh {
                        push(
                            &mut s,
                            FRIEND_RICH_PRESENCE_UPDATE,
                            rich_presence_update(steam_id, self.app_id),
                        );
                        return Response::FriendsFlag(true);
                    }
                    if !s.rich_requested.insert(steam_id) {
                        return Response::FriendsFlag(true);
                    }
                }
                self.note(
                    Direction::ToSteam,
                    "RichPresenceRequest",
                    format!("user {steam_id}"),
                );
                let sent = tokio::time::timeout(
                    TIMEOUT,
                    self.connection.send(CMsgClientRichPresenceRequest {
                        steamid_request: vec![steam_id],
                        ..Default::default()
                    }),
                )
                .await
                .is_ok_and(|r| r.is_ok());
                if !sent {
                    lock(&self.state).rich_requested.remove(&steam_id);
                }
                Response::FriendsFlag(sent)
            }
        }
    }

    /// A user from the social graph, a lobby we share, or the cache. With
    /// `request`, an unknown user is fetched in the background and announced
    /// with PersonaStateChange_t, like RequestUserInformation.
    fn user(&self, steam_id: u64, request: bool, lobby_name: Option<String>) -> Response {
        let known = self
            .social()
            .iter()
            .find(|f| f.steam_id == steam_id)
            .cloned()
            .or_else(|| lock(&self.state).users.get(&steam_id).cloned())
            .or_else(|| {
                lobby_name.map(|name| FriendInfo {
                    steam_id,
                    name,
                    ..Default::default()
                })
            })
            .or_else(|| {
                // Rich presence alone is enough to answer for that user.
                lock(&self.state)
                    .rich
                    .contains_key(&steam_id)
                    .then(|| FriendInfo {
                        steam_id,
                        ..Default::default()
                    })
            });
        if let Some(mut user) = known {
            // Rich presence answered by RequestFriendRichPresence fills in
            // what persona pushes left out.
            if user.rich_presence.is_empty()
                && let Some((_, pairs)) = lock(&self.state).rich.get(&steam_id)
            {
                user.rich_presence = pairs.clone();
            }
            return Response::User(Some(user));
        }
        if !request
            || (steam_id >> 52) & 0xF != 1
            || steam_id as u32 == 0
            || !lock(&self.state).users_requested.insert(steam_id)
        {
            return Response::User(None);
        }
        self.note(
            Direction::ToSteam,
            "GetPlayerLinkDetails",
            format!("user {steam_id}"),
        );
        let connection = self.connection.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let reply = tokio::time::timeout(
                TIMEOUT,
                connection.service_method(CPlayer_GetPlayerLinkDetails_Request {
                    steamids: vec![steam_id],
                    ..Default::default()
                }),
            )
            .await;
            let found = reply.ok().and_then(Result::ok).and_then(|reply| {
                reply.accounts.into_iter().find_map(|account| {
                    let public = account.public_data.as_ref()?;
                    (public.steamid() == steam_id).then(|| FriendInfo {
                        steam_id,
                        name: public
                            .persona_name
                            .clone()
                            .filter(|n| n.len() <= 4096 && !n.contains('\0'))
                            .unwrap_or_default(),
                        state: account
                            .private_data
                            .as_ref()
                            .and_then(|p| p.persona_state)
                            .unwrap_or(0),
                        ..Default::default()
                    })
                })
            });
            let mut s = lock(&state);
            s.users_requested.remove(&steam_id);
            if let Some(user) = found {
                if s.users.len() >= MAX_USERS {
                    s.users.clear();
                }
                s.users.insert(steam_id, user);
                push(
                    &mut s,
                    PERSONA_STATE_CHANGE,
                    persona_change(
                        steam_id,
                        CHANGE_NAME | CHANGE_NAME_FIRST_SET | CHANGE_STATUS,
                    ),
                );
            }
        });
        Response::User(None)
    }

    /// Queued callbacks, plus change callbacks for friends since the last poll.
    pub fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        let current = crate::friends::social(self.own).map(|social| social.snapshot());
        let mut s = lock(&self.state);
        if let Some((generation, friends)) = current
            && s.seen.as_ref().is_none_or(|(seen, _)| *seen != generation)
        {
            let previous = s.seen.replace((generation, friends.clone()));
            // The first snapshot is the baseline, not a burst of changes.
            if let Some((_, previous)) = previous {
                let before: HashMap<u64, &FriendInfo> =
                    previous.iter().map(|f| (f.steam_id, f)).collect();
                for friend in friends.iter() {
                    let flags = match before.get(&friend.steam_id) {
                        Some(old) => changes(old, friend),
                        None => CHANGE_RELATIONSHIP,
                    };
                    if flags != 0 {
                        push(
                            &mut s,
                            PERSONA_STATE_CHANGE,
                            persona_change(friend.steam_id, flags),
                        );
                    }
                    if flags & CHANGE_RICH_PRESENCE != 0 && app_of(friend.game_id) == self.app_id {
                        push(
                            &mut s,
                            FRIEND_RICH_PRESENCE_UPDATE,
                            rich_presence_update(friend.steam_id, self.app_id),
                        );
                    }
                }
                let now: HashSet<u64> = friends.iter().map(|f| f.steam_id).collect();
                for old in previous.iter().filter(|f| !now.contains(&f.steam_id)) {
                    push(
                        &mut s,
                        PERSONA_STATE_CHANGE,
                        persona_change(old.steam_id, CHANGE_RELATIONSHIP),
                    );
                }
            }
        }
        s.callbacks.drain(..).collect()
    }
}

impl Drop for Friends {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_presence_follows_valve_limits_and_encoding() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(valid_rich_presence(&[pair("steam_display", "#Menu")]));
        assert!(!valid_rich_presence(&[pair("", "x")]));
        assert!(!valid_rich_presence(&[pair(&"k".repeat(65), "x")]));
        assert!(!valid_rich_presence(&[pair("k", &"v".repeat(257))]));
        let many: Vec<_> = (0..31).map(|i| pair(&format!("k{i}"), "v")).collect();
        assert!(!valid_rich_presence(&many));

        let pairs = vec![pair("status", "Hub"), pair("steam_display", "#HubStatus")];
        let bytes = kv_encode_root("RP", &pairs);
        assert_eq!(&bytes[..4], b"\0RP\0");
        assert_eq!(&bytes[bytes.len() - 2..], [8, 8]);
        assert_eq!(kv_decode(&bytes), pairs);
    }

    #[test]
    fn persona_changes_map_to_steam_flags() {
        let old = FriendInfo {
            steam_id: 1,
            relationship: 3,
            name: "a".into(),
            state: 0,
            ..Default::default()
        };
        let mut new = old.clone();
        new.state = 1;
        new.game_id = 3247750;
        new.rich_presence = vec![("status".into(), "Hub".into())];
        assert_eq!(
            changes(&old, &new),
            CHANGE_STATUS | CHANGE_COME_ONLINE | CHANGE_GAME_PLAYED | CHANGE_RICH_PRESENCE
        );
        assert_eq!(changes(&new, &new), 0);
        let mut renamed = new.clone();
        renamed.name = "b".into();
        renamed.state = 0;
        assert_eq!(
            changes(&new, &renamed),
            CHANGE_NAME | CHANGE_STATUS | CHANGE_GONE_OFFLINE
        );
        assert_eq!(app_of(3247750), 3247750);
        assert_eq!(app_of(0x0200_0000_8000_0000), 0);
        assert_eq!(persona_change(7, 1).len(), 12);
        assert_eq!(rich_presence_update(7, 480).len(), 12);
    }
}
