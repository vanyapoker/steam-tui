//! Live Steam CM relationships and persona data. Call immediately after login:
//! steam-vent retains only 32 unhandled initial pushes. The social session is
//! registered Online so Steam supplies live presence; no desktop client is used.
mod state;

use futures_util::{StreamExt, stream::FuturesUnordered};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait, NetworkError};
use steam_vent_proto_steam::enums_clientserver::EMsg;
use steam_vent_proto_steam::steammessages_clientserver_friends::{
    CMsgClientChangeStatus, CMsgClientFriendsList, CMsgClientPersonaState,
    CMsgClientRequestFriendData,
};
use steam_vent_proto_steam::steammessages_clientserver_login::CMsgClientLoggedOff;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at, sleep_until, timeout};

use state::{PLAYER_NAME, State};

// ClientRequestFriendData replies echo requested flags but omit live status/game
// fields. Those fields arrive through the Online social-session subscription;
// requesting their groups would turn absent metadata fields into false Offline
// and cleared-game updates. Explicit refresh only requests names.
const REQUEST_FLAGS: u32 = PLAYER_NAME;
// The vendored per-kind broadcast has 16 slots. Request smaller batches and
// wait for their persona replies, not merely completion of the socket write.
const BATCH_SIZE: usize = 8;
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;
const MAX_APP_NAMES: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_TIMEOUT: Duration = Duration::from_secs(30);
const PERSONA_GRACE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(60);

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Presence {
    Offline,
    Online,
    Busy,
    Away,
    Snooze,
    LookingToTrade,
    LookingToPlay,
    Unknown,
}

impl Presence {
    pub fn is_offline(self) -> bool {
        self == Self::Offline
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "Offline",
            Self::Online => "Online",
            Self::Busy => "Busy",
            Self::Away => "Away",
            Self::Snooze => "Snooze",
            Self::LookingToTrade => "Looking to trade",
            Self::LookingToPlay => "Looking to play",
            Self::Unknown => "Unknown",
        }
    }

    fn from_protocol(value: u32) -> Self {
        match value {
            0 => Self::Offline,
            1 => Self::Online,
            2 => Self::Busy,
            3 => Self::Away,
            4 => Self::Snooze,
            5 => Self::LookingToTrade,
            6 => Self::LookingToPlay,
            // Invisible is not an observed Offline state. Normally Steam hides
            // it from other users, but do not misclassify an unexpected value.
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Friend {
    pub steam_id: u64,
    pub name: String,
    pub state: Presence,
    pub game: Option<String>,
}

/// Live relationships and personas, shared with per-launch game backends so
/// ISteamFriends answers from the same social session the TUI shows.
#[derive(Clone, Default)]
pub struct Social(Arc<Mutex<SocialData>>);

/// The running friends loop of each signed-in account.
static REGISTRY: Mutex<BTreeMap<u64, Social>> = Mutex::new(BTreeMap::new());

/// The live social graph of `steam_id`, if a friends loop runs for it.
pub fn social(steam_id: u64) -> Option<Social> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&steam_id)
        .cloned()
}

/// Removes this loop's registration when the loop ends, unless a newer loop
/// for the same account replaced it.
struct Registration {
    steam_id: u64,
    social: Social,
}

impl Registration {
    fn new(steam_id: u64) -> Self {
        let social = Social::default();
        REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(steam_id, social.clone());
        Self { steam_id, social }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        if registry
            .get(&self.steam_id)
            .is_some_and(|current| Arc::ptr_eq(&current.0, &self.social.0))
        {
            registry.remove(&self.steam_id);
        }
    }
}

#[derive(Default)]
struct SocialData {
    generation: u64,
    friends: Arc<Vec<steam_ipc::FriendInfo>>,
}

impl Social {
    /// Change counter and the current list; the list is shared, not copied.
    pub fn snapshot(&self) -> (u64, Arc<Vec<steam_ipc::FriendInfo>>) {
        let data = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (data.generation, data.friends.clone())
    }

    fn publish(&self, friends: Vec<steam_ipc::FriendInfo>) {
        let mut data = self.0.lock().unwrap_or_else(|e| e.into_inner());
        data.generation += 1;
        data.friends = Arc::new(friends);
    }
}

#[derive(Debug, Clone)]
pub enum FriendEvent {
    /// Complete membership snapshot, in ascending SteamID order. Metadata not
    /// returned by Steam remains an ID/name fallback and Unknown, never Offline.
    Snapshot(Vec<Friend>),
    /// The signed-in user, separate from friend membership and navigation.
    Profile(Friend),
    Status(String),
}

/// Maintain live relationships, names, presence and games until the consumer
/// drops. Requires a LocalSet because steam-vent's session futures are not Send.
/// Keep this worker alive: a second invocation on the same connection cannot
/// recover the already consumed initial relationship list.
pub async fn run(connection: Connection, updates: mpsc::Sender<FriendEvent>) -> Result<()> {
    let (_keep_refresh_open, refresh) = mpsc::channel(1);
    run_with_refresh(connection, updates, refresh).await
}

/// As `run`, with explicit persona refresh requests. Startup registers this
/// session Online; subsequent refresh preserves that state and membership.
pub async fn run_with_refresh(
    connection: Connection,
    updates: mpsc::Sender<FriendEvent>,
    refresh: mpsc::Receiver<()>,
) -> Result<()> {
    let result = run_inner(connection, &updates, refresh).await;
    if let Err(error) = &result {
        // Errors contain only our protocol diagnostics, no login/token payloads.
        let _ = updates.send(FriendEvent::Status(error.to_string())).await;
    }
    result
}

enum WorkResult {
    Personas(std::result::Result<(), &'static str>),
    App(u32, std::result::Result<Option<String>, &'static str>),
}
type Work = Pin<Box<dyn Future<Output = WorkResult>>>;

fn queue_status(statuses: &mut VecDeque<String>, message: impl Into<String>) {
    // Status bursts must not block network ingestion or grow without bound.
    if statuses.len() == 8 {
        statuses.pop_front();
    }
    statuses.push_back(message.into());
}

/// Keeps the social graph available to a game backend when no friends loop
/// runs for this account (a launch without the TUI). Ends when dropped.
pub async fn run_for_games(connection: Connection) -> Result<()> {
    let (updates, mut events) = mpsc::channel(16);
    let drain = async move { while events.recv().await.is_some() {} };
    tokio::select! {
        result = run(connection, updates) => result,
        () = drain => Ok(()),
    }
}

async fn run_inner(
    connection: Connection,
    updates: &mpsc::Sender<FriendEvent>,
    mut refresh: mpsc::Receiver<()>,
) -> Result<()> {
    // on() registers synchronously. Register BOTH streams before draining;
    // otherwise a persona/list push can fall into the gap and be lost.
    let lists = connection.on::<CMsgClientFriendsList>();
    let personas = connection.on::<CMsgClientPersonaState>();
    let logged_off = connection.on::<CMsgClientLoggedOff>();
    futures_util::pin_mut!(lists, personas, logged_off);
    let own_id = connection.steam_id().steam64();
    let registration = Registration::new(own_id);
    let social = &registration.social;
    let mut state = State::with_owner(own_id);
    let mut requested = BTreeSet::from([own_id]);
    for raw in connection
        .take_unprocessed_kind::<CMsgClientFriendsList>()
        .into_iter()
        .chain(connection.take_unprocessed_kind::<CMsgClientPersonaState>())
        .chain(connection.take_unprocessed_kind::<CMsgClientLoggedOff>())
    {
        if raw.kind == EMsg::k_EMsgClientFriendsList {
            if raw.data.len() > MAX_RAW_BYTES {
                return Err(failure("Steam initial friend list exceeds the 8 MiB limit"));
            }
            let message = raw
                .into_message::<CMsgClientFriendsList>()
                .map_err(|_| failure("Cannot decode Steam initial friend list"))?;
            requested.extend(state.relationships(message)?);
        } else if raw.kind == EMsg::k_EMsgClientPersonaState {
            if raw.data.len() > MAX_RAW_BYTES {
                return Err(failure(
                    "Steam initial persona message exceeds the 8 MiB limit",
                ));
            }
            let message = raw
                .into_message::<CMsgClientPersonaState>()
                .map_err(|_| failure("Cannot decode Steam initial persona message"))?;
            state.personas(message)?;
        } else if raw.kind == EMsg::k_EMsgClientLoggedOff {
            return Err(failure("Steam logged off before loading friends"));
        }
    }

    // A CM login alone leaves this social session Offline. Activate its live
    // persona subscription before requesting metadata, without marking this a
    // user-set account preference. Preserve the initial baseline so incoming
    // social-session changes can update it, including Offline -> Online.
    timeout(
        REQUEST_TIMEOUT,
        connection.send(CMsgClientChangeStatus {
            persona_state: Some(1),
            persona_set_by_user: Some(false),
            need_persona_response: Some(true),
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| failure("Steam social-session registration timed out"))?
    .map_err(|_| {
        failure("Steam social-session registration failed; live friend presence is unavailable")
    })?;

    let mut statuses = VecDeque::new();
    queue_status(
        &mut statuses,
        "Loading Steam friends; TUI social session Online",
    );
    let mut work: FuturesUnordered<Work> = FuturesUnordered::new();
    let mut persona_in_flight = false;
    let mut awaiting_personas = BTreeSet::new();
    let mut persona_deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut app_in_flight = 0;
    let mut app_attempts = BTreeSet::new();
    let mut pending_apps = state.missing_apps();
    let mut name_limit_reported = false;
    let mut dirty = state.complete;
    let mut published = false;
    let mut published_profile = None;
    let mut social_online = false;
    let mut refresh_open = true;
    let mut deadline = Instant::now()
        + if state.complete {
            PERSONA_GRACE
        } else {
            INITIAL_TIMEOUT
        };
    let mut poll = interval_at(Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut shared_generation = None;
    loop {
        if updates.is_closed() {
            return Ok(());
        }
        if shared_generation != Some(state.generation()) {
            shared_generation = Some(state.generation());
            social.publish(state.social());
        }
        if !persona_in_flight && awaiting_personas.is_empty() && !requested.is_empty() {
            let ids: Vec<_> = requested.iter().copied().take(BATCH_SIZE).collect();
            for id in &ids {
                requested.remove(id);
            }
            awaiting_personas.extend(ids.iter().copied());
            persona_deadline = Instant::now() + REQUEST_TIMEOUT;
            let connection = connection.clone();
            work.push(Box::pin(async move {
                let message = CMsgClientRequestFriendData {
                    persona_state_requested: Some(REQUEST_FLAGS),
                    friends: ids,
                    ..Default::default()
                };
                let result = match timeout(REQUEST_TIMEOUT, connection.send(message)).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err("Steam friend data request failed"),
                    Err(_) => Err("Steam friend data request timed out"),
                };
                WorkResult::Personas(result)
            }));
            persona_in_flight = true;
        }
        // Metadata futures run alongside ingestion. At most two app requests
        // are active; each uses the existing bounded token/multipart PICS path.
        while app_in_flight < 2 && !pending_apps.is_empty() {
            if app_attempts.len() >= MAX_APP_NAMES {
                if !name_limit_reported {
                    queue_status(
                        &mut statuses,
                        "Steam game-name lookup limit reached; displaying remaining app IDs",
                    );
                    name_limit_reported = true;
                }
                pending_apps.clear();
                break;
            }
            let app_id = pending_apps.pop_first().expect("nonempty app queue");
            if !app_attempts.insert(app_id) {
                continue;
            }
            app_in_flight += 1;
            let connection = connection.clone();
            work.push(Box::pin(async move {
                let result = match timeout(
                    REQUEST_TIMEOUT,
                    crate::library::inspect_app(&connection, app_id),
                )
                .await
                {
                    Ok(Ok(app)) => Ok(app.and_then(|app| app.name)),
                    Ok(Err(_)) => Err("Steam game-name metadata request failed"),
                    Err(_) => Err("Steam game-name metadata request timed out"),
                };
                WorkResult::App(app_id, result)
            }));
        }
        if !published && social_online && state.ready() {
            published = true;
            dirty = true;
            queue_status(&mut statuses, "Steam friends loaded; listening for changes");
        }
        let profile = state.profile();
        let profile_changed = profile.is_some() && profile != published_profile;
        tokio::select! {
            _ = updates.closed() => return Ok(()),
            permit = updates.reserve(), if !statuses.is_empty() || profile_changed || (published && dirty) => {
                let Ok(permit) = permit else { return Ok(()); };
                if let Some(status) = statuses.pop_front() {
                    permit.send(FriendEvent::Status(status));
                } else if profile_changed {
                    let profile = profile.expect("changed profile exists");
                    published_profile = Some(profile.clone());
                    permit.send(FriendEvent::Profile(profile));
                } else {
                    permit.send(FriendEvent::Snapshot(state.snapshot()));
                    dirty = false;
                }
            }
            message = lists.next() => {
                let message = message.ok_or_else(|| failure("Steam friend relationship stream ended"))?
                    .map_err(|_| failure("Steam friend relationship stream failed or fell behind; reconnect to recover membership"))?;
                let had_complete = state.complete;
                requested.extend(state.relationships(message)?);
                if social_online {
                    state.establish_social_baseline();
                }
                // Do not request removed relationships still queued from an
                // earlier full snapshot or refresh.
                let current: BTreeSet<_> = state.ids().into_iter().collect();
                requested.retain(|id| *id == own_id || current.contains(id));
                awaiting_personas.retain(|id| *id == own_id || current.contains(id));
                dirty = state.complete;
                pending_apps = state.missing_apps().difference(&app_attempts).copied().collect();
                if !had_complete && state.complete {
                    deadline = Instant::now() + PERSONA_GRACE;
                }
            }
            message = personas.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(NetworkError::Lagged(skipped))) => {
                        // Metadata requests cannot recover missed live changes.
                        // Mark stale values Unknown, then require a new social
                        // session rather than fabricating an Offline refresh.
                        state.invalidate_presence();
                        if state.complete {
                            let _ = updates.send(FriendEvent::Snapshot(state.snapshot())).await;
                        }
                        return Err(failure(format!(
                            "Steam persona subscriber missed {skipped} updates; reconnect to recover live presence"
                        )));
                    }
                    Some(Err(NetworkError::EOF)) | None => {
                        return Err(failure("Steam persona stream ended; friends are no longer live"));
                    }
                    Some(Err(_)) => {
                        return Err(failure("Steam persona message decoding or protocol failed"));
                    }
                };
                if message.friends.iter().any(|friend| friend.friendid == Some(own_id) && friend.persona_state == Some(1)) {
                    social_online = true;
                    state.establish_social_baseline();
                }
                for friend in &message.friends {
                    if let Some(id) = friend.friendid {
                        awaiting_personas.remove(&id);
                    }
                }
                state.personas(message)?;
                pending_apps = state.missing_apps().difference(&app_attempts).copied().collect();
                dirty = state.complete;
            }
            _ = logged_off.next() => return Err(failure("Steam session logged off; friends are no longer live")),
            result = work.next(), if !work.is_empty() => {
                match result.expect("nonempty work queue") {
                    WorkResult::Personas(result) => {
                        persona_in_flight = false;
                        if let Err(message) = result {
                            awaiting_personas.clear();
                            queue_status(&mut statuses, message);
                        }
                    }
                    WorkResult::App(app_id, result) => {
                        app_in_flight -= 1;
                        let name = match result {
                            Ok(name) => name,
                            Err(message) => {
                                queue_status(&mut statuses, format!("{message} (app {app_id})"));
                                None
                            }
                        };
                        if name.is_none() {
                            queue_status(&mut statuses, format!("Game name unavailable for app {app_id}; displaying its ID"));
                        }
                        state.names.insert(app_id, name);
                        dirty = state.complete;
                    }
                }
            }
            _ = sleep_until(persona_deadline), if !awaiting_personas.is_empty() => {
                awaiting_personas.clear();
                queue_status(&mut statuses, "Steam did not return all requested persona data within 15 seconds");
            }
            _ = sleep_until(deadline), if !published => {
                if !state.complete {
                    return Err(failure("Steam did not provide its initial full friend list within 30 seconds; reconnect promptly after login"));
                }
                if !social_online {
                    return Err(failure("Steam did not confirm the Online social session; live friend presence is unavailable"));
                }
                published = true;
                dirty = true;
                queue_status(&mut statuses, "Some Steam persona data is unavailable; unobserved presence is Unknown");
            }
            event = refresh.recv(), if refresh_open => {
                if event.is_some() {
                    requested.extend(state.ids());
                    requested.insert(own_id);
                    dirty = state.complete;
                    queue_status(&mut statuses, "Refreshing Steam friend names; presence updates remain live");
                } else {
                    refresh_open = false;
                }
            }
            _ = poll.tick() => {
                // Refresh metadata only; presence/game updates are pushed by
                // the registered social session, not ClientRequestFriendData.
                requested.extend(state.ids());
                requested.insert(own_id);
            }
        }
    }
}
