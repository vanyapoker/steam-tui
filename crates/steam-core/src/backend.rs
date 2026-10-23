//! Experimental Steamworks bridge, scoped to one app and one CM session.
//! Credentials remain in the parent process. Normal launches do not use this.
pub(crate) mod apps;
mod events;
mod friends;
mod gc;
pub mod images;
mod licenses;
mod lobby;
mod networking;
mod ownership;
pub mod sdr;
pub mod stats;
mod tickets;
pub mod ugc;
pub mod workshop;
pub use events::{Direction, Event, EventSender, gc_message_name};
use std::error::Error;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use steam_ipc::{MAX_FRAME, Operation, Reply, Request, Response, UgcOp, VERSION};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientGetAppOwnershipTicket, CMsgClientGetAppOwnershipTicketResponse,
    CMsgClientRequestEncryptedAppTicket, CMsgClientRequestEncryptedAppTicketResponse,
};
use steam_vent_proto_steam::steammessages_clientserver_login::CMsgClientLoggedOff;
use steam_vent_proto_steam::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};

/// Owns a mode-0700 directory and its socket. Never unlinks an existing socket.
pub struct Endpoint {
    directory: PathBuf,
    listener: UnixListener,
    /// The game's library `steamapps` directory, where Workshop items are installed.
    workshop_dir: Option<PathBuf>,
    apps: Option<std::sync::Arc<apps::Context>>,
}

impl Endpoint {
    pub fn create() -> io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "steam-tui-backend-{}-{sequence}",
                std::process::id()
            ));
            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
            let result = UnixListener::bind(directory.join("client.sock"));
            match result {
                Ok(listener) => {
                    let endpoint = Self {
                        directory,
                        listener,
                        workshop_dir: None,
                        apps: None,
                    };
                    fs::set_permissions(endpoint.path(), fs::Permissions::from_mode(0o600))?;
                    return Ok(endpoint);
                }
                Err(error) => {
                    let _ = fs::remove_dir(directory);
                    return Err(error);
                }
            }
        }
        Err(io::Error::other(
            "Could not allocate a private backend directory",
        ))
    }

    pub fn path(&self) -> PathBuf {
        self.directory.join("client.sock")
    }

    /// Installs Workshop items below `steamapps/workshop` of this library `steamapps` directory.
    pub fn with_workshop(mut self, steamapps: PathBuf) -> Self {
        self.workshop_dir = Some(steamapps);
        self
    }

    pub(crate) fn with_apps(mut self, context: apps::Context) -> Self {
        self.apps = Some(std::sync::Arc::new(context));
        self
    }

    /// Runs until cancelled/dropped. Caller owns session lifetime and shutdown.
    /// Serial requests bound resource use and share short-lived CM query caches.
    pub async fn run(&self, connection: &Connection, app_id: u32) -> Result<(), Box<dyn Error>> {
        self.run_with_events(connection, app_id, None).await
    }

    /// Like `run`, reporting what is sent and received to `events`.
    pub async fn run_with_events(
        &self,
        connection: &Connection,
        app_id: u32,
        events: Option<EventSender>,
    ) -> Result<(), Box<dyn Error>> {
        if app_id == 0 || connection.refresh_token().is_none() {
            return Err("Backend requires a nonzero app ID and an authenticated account".into());
        }
        // Liveness proves transport traffic, not account authentication. Treat
        // a logged-off push (including one buffered during login) as terminal.
        let mut logged_off = Box::pin(connection.on::<CMsgClientLoggedOff>());
        if !connection
            .take_unprocessed_kind::<CMsgClientLoggedOff>()
            .is_empty()
        {
            return Err("CM account session has logged off; restart the backend".into());
        }
        // Data-center pings for the game's matchmaking report, measured once.
        let pops: std::sync::Arc<std::sync::Mutex<Option<Vec<(String, i32)>>>> = Default::default();
        {
            let (pops, events) = (pops.clone(), events.clone());
            tokio::spawn(async move {
                match sdr::measure(app_id).await {
                    Ok(entries) => {
                        let reachable = entries.iter().filter(|(_, ms)| *ms >= 0).count();
                        let mut best: Vec<_> = entries.iter().filter(|(_, ms)| *ms >= 0).collect();
                        best.sort_by_key(|(_, ms)| *ms);
                        let summary: Vec<String> = best
                            .iter()
                            .take(5)
                            .map(|(code, ms)| format!("{code} {ms} ms"))
                            .collect();
                        events::emit(
                            &events,
                            app_id,
                            Direction::Local,
                            "SDR",
                            format!("data-center pings: {reachable}/{} reachable", entries.len()),
                            format!("fastest: {}", summary.join(", ")),
                        );
                        *pops.lock().unwrap_or_else(|e| e.into_inner()) = Some(entries);
                    }
                    Err(message) => events::emit(
                        &events,
                        app_id,
                        Direction::Local,
                        "SDR",
                        "data-center pings unavailable",
                        message,
                    ),
                }
            });
        }
        let session = Session {
            connection,
            app_id,
            alive: std::sync::Arc::default(),
            ownership: Mutex::new(ownership::Service::default()),
            apps: Mutex::new(apps::Service::new(
                connection,
                self.apps.clone(),
                events.clone(),
            )),
            networking: Mutex::new(networking::Networking::start(
                connection,
                app_id,
                events.clone(),
            )),
            tickets: Mutex::new(tickets::Service::start(connection, app_id, events.clone())?),
            gc: gc::Gc::start(connection, app_id, events.clone()).await?,
            pops: pops.clone(),
            lobbies: lobby::Lobbies::start(
                connection,
                app_id,
                persona_name(connection).await,
                events.clone(),
            ),
            stats: Mutex::new(stats::Stats::new(app_id, events.clone())),
            friends: friends::Friends::start(connection, app_id, events.clone()),
            licenses: licenses::Licenses::start(connection, app_id, events.clone()),
            workshop: workshop::Service::new(
                connection,
                app_id,
                self.workshop_dir.clone(),
                events.clone(),
            ),
            events: events.clone(),
        };
        // Requests are served concurrently: a slow CM-backed call (stats, ownership)
        // must never hold up the per-frame polls the game makes. Each service keeps
        // its own lock, and the number of in-flight requests is bounded.
        let session = &session;
        let mut in_flight = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                _ = logged_off.next() => return Err("CM account session ended; restart the backend".into()),
                Some(()) = in_flight.next(), if !in_flight.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    // SAFETY: geteuid has no preconditions.
                    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
                        continue;
                    }
                    if in_flight.len() >= MAX_IN_FLIGHT {
                        continue;
                    }
                    in_flight.push(serve(stream, session, app_id));
                }
            }
        }
    }
}

const MAX_IN_FLIGHT: usize = 64;

async fn serve(mut stream: UnixStream, session: &Session<'_>, app_id: u32) {
    let handle = timeout(Duration::from_secs(12), async {
        let request = read_request(&mut stream).await?;
        let response = session.respond(request).await;
        write_reply(
            &mut stream,
            &Reply {
                version: VERSION,
                app_id,
                response,
            },
        )
        .await
    });
    let _ = handle.await;
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(&self.directory);
    }
}

async fn read_request(stream: &mut UnixStream) -> io::Result<Request> {
    let size = stream.read_u32_le().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid IPC frame size",
        ));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

async fn write_reply(stream: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let bytes = steam_ipc::encode(reply)?;
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

/// Display name for lobby member lists; falls back to the SteamID.
async fn persona_name(connection: &Connection) -> String {
    let steam_id = connection.steam_id().steam64();
    let request = CPlayer_GetPlayerLinkDetails_Request {
        steamids: vec![steam_id],
        ..Default::default()
    };
    if let Ok(Ok(reply)) = timeout(Duration::from_secs(5), connection.service_method(request)).await
    {
        for account in reply.accounts {
            if let Some(public) = account.public_data.as_ref()
                && public.steamid() == steam_id
                && let Some(name) = public.persona_name.as_ref()
                && !name.is_empty()
                && name.len() <= 256
            {
                return name.clone();
            }
        }
    }
    steam_id.to_string()
}

/// Last CM liveness probe. The backend starts after a successful login, so
/// the session counts as alive until a probe says otherwise.
struct Liveness {
    alive: bool,
    checked: Option<Instant>,
    probing: bool,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            alive: true,
            checked: None,
            probing: false,
        }
    }
}

struct Session<'a> {
    connection: &'a Connection,
    app_id: u32,
    alive: std::sync::Arc<std::sync::Mutex<Liveness>>,
    ownership: Mutex<ownership::Service>,
    apps: Mutex<apps::Service>,
    networking: Mutex<networking::Networking>,
    tickets: Mutex<tickets::Service>,
    gc: gc::Gc,
    pops: std::sync::Arc<std::sync::Mutex<Option<Vec<(String, i32)>>>>,
    lobbies: lobby::Lobbies,
    stats: Mutex<stats::Stats>,
    friends: friends::Friends,
    licenses: licenses::Licenses,
    workshop: workshop::Service,
    events: Option<EventSender>,
}

fn request_error(request: &Request, app_id: u32) -> Option<Response> {
    let message = if request.version != VERSION {
        "Unsupported backend protocol version"
    } else if request.app_id != app_id {
        "Backend is scoped to a different app"
    } else {
        return None;
    };
    Some(Response::Error {
        message: message.into(),
    })
}

/// Accept only a matching, successful CM response with a nonempty ticket.
/// Do not expose the ticket over the identity/ownership IPC protocol.
fn owns_app(
    response: &CMsgClientGetAppOwnershipTicketResponse,
    app_id: u32,
) -> Result<bool, &'static str> {
    if response.app_id != Some(app_id) {
        return Err("Mismatched ownership response");
    }
    match response.eresult() {
        1 if !response.ticket().is_empty() => Ok(true),
        1 => Err("Steam returned an empty ownership ticket"),
        15 => Ok(false), // AccessDenied. NotLoggedOn (21) is an error, not denial.
        _ => Err("Steam could not confirm app ownership"),
    }
}

impl Session<'_> {
    fn note(&self, direction: Direction, channel: &'static str, title: &str, detail: &str) {
        events::emit(&self.events, self.app_id, direction, channel, title, detail);
    }

    /// Asks the CM for an encrypted app ticket of the running app (returned as the serialized `EncryptedAppTicket`). Only the CM's
    /// own answer is relayed: a refusal keeps its `EResult` and carries no ticket.
    async fn encrypted_app_ticket(&self, user_data: Vec<u8>) -> Response {
        const E_FAIL: i32 = 2;
        const E_TIMEOUT: i32 = 16;
        if user_data.len() > steam_ipc::ENCRYPTED_TICKET_USER_DATA_MAX {
            return Response::Error {
                message: "Encrypted ticket user data is too large".into(),
            };
        }
        let request = CMsgClientRequestEncryptedAppTicket {
            app_id: Some(self.app_id),
            userdata: Some(user_data),
            ..Default::default()
        };
        let reply = timeout(
            Duration::from_secs(10),
            self.connection
                .job::<_, CMsgClientRequestEncryptedAppTicketResponse>(request),
        )
        .await;
        let reply = match reply {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                return Response::EncryptedAppTicket {
                    result: E_FAIL,
                    ticket: Vec::new(),
                };
            }
            Err(_) => {
                return Response::EncryptedAppTicket {
                    result: E_TIMEOUT,
                    ticket: Vec::new(),
                };
            }
        };
        if reply.app_id() != self.app_id {
            return Response::Error {
                message: "CM answered the encrypted ticket request for another app".into(),
            };
        }
        let result = reply.eresult();
        // Valve's client keeps the whole EncryptedAppTicket message and serializes
        // it for the game; the game's server library parses that wire form.
        let ticket = reply
            .encrypted_app_ticket
            .as_ref()
            .and_then(|t| protobuf::Message::write_to_bytes(t).ok())
            .unwrap_or_default();
        if result == 1 && (ticket.is_empty() || ticket.len() > steam_ipc::ENCRYPTED_TICKET_MAX) {
            return Response::EncryptedAppTicket {
                result: E_FAIL,
                ticket: Vec::new(),
            };
        }
        Response::EncryptedAppTicket {
            result,
            ticket: if result == 1 { ticket } else { Vec::new() },
        }
    }

    /// Whether the CM session answers, as last probed. Requests never wait for a
    /// probe: a stale result starts one in the background (at most every two
    /// seconds, one at a time) and callers get the latest known state.
    async fn alive(&self) -> bool {
        let (alive, start) = {
            let mut state = self.alive.lock().unwrap_or_else(|e| e.into_inner());
            let stale = state
                .checked
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
            let start = stale && !state.probing;
            state.probing |= start;
            (state.alive, start)
        };
        if start {
            let state = self.alive.clone();
            let connection = self.connection.clone();
            tokio::spawn(async move {
                let alive = crate::liveness::probe(&connection, Duration::from_secs(3)).await;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.alive = alive;
                state.checked = Some(Instant::now());
                state.probing = false;
            });
        }
        if !alive {
            self.ownership.lock().await.clear();
        }
        alive
    }

    async fn respond(&self, request: Request) -> Response {
        if let Some(error) = request_error(&request, self.app_id) {
            return error;
        }
        // Local, queue-only operations answer without touching the CM session.
        let alive = if matches!(
            request.operation,
            Operation::PollCallbacks
                | Operation::GcRecv
                | Operation::PopPings
                | Operation::Country
                | Operation::Image(_)
                | Operation::CancelAuthTicket { .. }
                | Operation::PurchaseTime { .. }
                | Operation::Friends(steam_ipc::FriendsOp::Snapshot)
        ) {
            true
        } else {
            self.alive().await
        };
        match request.operation {
            Operation::Identity => Response::Identity {
                steam_id: self.connection.steam_id().steam64(),
                logged_on: alive,
            },
            Operation::Country => Response::Country {
                code: self.connection.ip_country_code().to_owned(),
            },
            Operation::NetworkingCertificate { .. }
            | Operation::NetworkingConfig
            | Operation::P2PSend { .. }
            | Operation::P2PRecv
            | Operation::Ownership
            | Operation::AppOwnership { .. }
            | Operation::Apps(_)
            | Operation::Persona
            | Operation::AuthSessionTicket { .. }
            | Operation::WebAuthTicket { .. }
            | Operation::EncryptedAppTicket { .. }
            | Operation::CancelAuthTicket { .. }
            | Operation::GcSend { .. }
            | Operation::Lobby(_)
            | Operation::Stats(_)
            | Operation::Ugc(_)
                if !alive =>
            {
                Response::Error {
                    message: "CM session is disconnected".into(),
                }
            }
            operation @ (Operation::NetworkingCertificate { .. }
            | Operation::NetworkingConfig
            | Operation::P2PSend { .. }
            | Operation::P2PRecv) => {
                self.networking
                    .lock()
                    .await
                    .respond(self.connection, self.app_id, operation)
                    .await
            }
            Operation::Ownership => {
                self.ownership
                    .lock()
                    .await
                    .answer(
                        self.connection,
                        self.app_id,
                        self.app_id,
                        &self.events,
                        self.licenses.generation(),
                    )
                    .await
            }
            Operation::AppOwnership { app_id } => {
                self.ownership
                    .lock()
                    .await
                    .answer(
                        self.connection,
                        self.app_id,
                        app_id,
                        &self.events,
                        self.licenses.generation(),
                    )
                    .await
            }
            Operation::Apps(op) => {
                if let steam_ipc::AppsOp::InstallDlc { app_id } = &op
                    && !matches!(
                        self.ownership
                            .lock()
                            .await
                            .query(self.connection, self.app_id, *app_id, &self.events)
                            .await,
                        Response::Ownership { owned: true }
                    )
                {
                    return Response::Error {
                        message: "DLC ownership could not be confirmed".into(),
                    };
                }
                self.apps.lock().await.handle(op).await
            }
            Operation::Persona => {
                let steam_id = self.connection.steam_id().steam64();
                let request = CPlayer_GetPlayerLinkDetails_Request {
                    steamids: vec![steam_id],
                    ..Default::default()
                };
                let reply = timeout(
                    Duration::from_secs(5),
                    self.connection.service_method(request),
                )
                .await;
                if let Ok(Ok(reply)) = reply {
                    for account in reply.accounts {
                        if let Some(public) = account.public_data.as_ref()
                            && public.steamid() == steam_id
                            && let Some(name) = public.persona_name.as_ref()
                            && name.len() <= 4096
                            && !name.contains('\0')
                        {
                            return Response::Persona {
                                steam_id,
                                name: name.clone(),
                                state: account.private_data.as_ref().and_then(|p| p.persona_state),
                            };
                        }
                    }
                }
                Response::Error {
                    message: "CM did not return this account's persona".into(),
                }
            }
            Operation::AuthSessionTicket { capacity } => {
                match self.tickets.lock().await.issue(capacity).await {
                    Ok((handle, ticket)) => {
                        self.note(
                            Direction::ToSteam,
                            "CM",
                            "ClientAuthList (auth session ticket)",
                            &format!(
                                "ticket handle {handle}, {} bytes issued and acknowledged by Steam (bytes hidden)",
                                ticket.len()
                            ),
                        );
                        Response::AuthTicket { handle, ticket }
                    }
                    Err(message) => Response::Error {
                        message: message.into(),
                    },
                }
            }
            Operation::EncryptedAppTicket { user_data } => {
                let response = self.encrypted_app_ticket(user_data).await;
                if let Response::EncryptedAppTicket { result, ticket } = &response {
                    self.note(
                        Direction::ToSteam,
                        "CM",
                        "ClientRequestEncryptedAppTicket",
                        &format!(
                            "result {result}, {} byte ticket (bytes hidden)",
                            ticket.len()
                        ),
                    );
                }
                response
            }
            Operation::GcSend { msg_type, payload } => {
                let Some(payload) = steam_ipc::hex_decode(&payload) else {
                    return Response::Error {
                        message: "Malformed Game Coordinator payload".into(),
                    };
                };
                match self
                    .gc
                    .send(self.connection, self.app_id, msg_type, payload)
                    .await
                {
                    Ok(()) => Response::GcSent,
                    Err(message) => Response::Error {
                        message: message.into(),
                    },
                }
            }
            Operation::Lobby(op) => self.lobbies.handle(self.connection, op).await,
            Operation::Stats(op) => self.stats.lock().await.handle(self.connection, op).await,
            Operation::Ugc(
                op @ (UgcOp::Snapshot
                | UgcOp::Download { .. }
                | UgcOp::Suspend { .. }
                | UgcOp::Subscribe { .. }),
            ) => self.workshop.handle(op).await,
            Operation::Ugc(op) => ugc::handle(self.connection, self.app_id, op).await,
            Operation::Image(op) => images::handle(op).await,
            Operation::Friends(op) => {
                let lobby_name = match op {
                    steam_ipc::FriendsOp::User { steam_id, .. } => {
                        self.lobbies.member_name(steam_id)
                    }
                    _ => None,
                };
                self.friends.handle(op, lobby_name).await
            }
            Operation::PurchaseTime { app_id } => Response::PurchaseTime(
                crate::library::earliest_purchase(self.connection, app_id).unwrap_or(0),
            ),
            Operation::WebAuthTicket { identity } => {
                match self.tickets.lock().await.web(identity) {
                    Ok(handle) => Response::AuthTicketPending { handle },
                    Err(message) => Response::Error {
                        message: message.into(),
                    },
                }
            }
            Operation::PollCallbacks => {
                let mut callbacks = self.lobbies.poll();
                // A busy service is simply polled again next round.
                if let Ok(tickets) = self.tickets.try_lock() {
                    callbacks.extend(tickets.poll());
                }
                callbacks.extend(self.workshop.poll());
                callbacks.extend(self.friends.poll());
                callbacks.extend(self.licenses.poll());
                if let Ok(apps) = self.apps.try_lock() {
                    callbacks.extend(apps.poll());
                }
                Response::Callbacks(
                    callbacks
                        .into_iter()
                        .map(|(id, bytes)| (id, steam_ipc::hex_encode(&bytes)))
                        .collect(),
                )
            }
            Operation::PopPings => {
                let pops = self.pops.lock().unwrap_or_else(|e| e.into_inner());
                match &*pops {
                    Some(entries) => Response::PopPings {
                        ready: true,
                        entries: entries.clone(),
                    },
                    None => Response::PopPings {
                        ready: false,
                        entries: Vec::new(),
                    },
                }
            }
            Operation::GcRecv => match self.gc.recv() {
                Some((msg_type, payload)) => Response::GcMessage {
                    msg_type,
                    payload: steam_ipc::hex_encode(&payload),
                },
                None => Response::GcNone,
            },
            Operation::CancelAuthTicket { handle } => {
                match self.tickets.lock().await.cancel(handle) {
                    Ok(()) => {
                        self.note(
                            Direction::ToSteam,
                            "CM",
                            "ClientAuthList (queue cancellation)",
                            &format!("ticket handle {handle} cancellation queued"),
                        );
                        Response::Cancelled
                    }
                    Err(message) => Response::Error {
                        message: message.into(),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_requires_matching_successful_nonempty_ticket() {
        let mut response = CMsgClientGetAppOwnershipTicketResponse {
            app_id: Some(440),
            eresult: Some(1),
            ticket: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(owns_app(&response, 440), Ok(true));
        assert!(owns_app(&response, 570).is_err());
        response.ticket = Some(vec![]);
        assert!(owns_app(&response, 440).is_err());
        response.eresult = Some(15);
        assert_eq!(owns_app(&response, 440), Ok(false));
        response.eresult = Some(2);
        assert!(owns_app(&response, 440).is_err());
        response.eresult = Some(21); // NotLoggedOn must not be cached as ownership denial.
        assert!(owns_app(&response, 440).is_err());
    }

    #[tokio::test]
    async fn endpoint_is_private_and_cleaned_up_and_frames_are_bounded() {
        let endpoint = Endpoint::create().unwrap();
        let path = endpoint.path();
        assert_eq!(
            fs::metadata(&endpoint.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut peer = UnixStream::connect(&path).await.unwrap();
        let (mut server, _) = endpoint.listener.accept().await.unwrap();
        peer.write_u32_le(u32::MAX).await.unwrap();
        assert!(read_request(&mut server).await.is_err());
        drop(endpoint);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn requests_cannot_cross_apps_or_protocol_versions() {
        let mut request = Request {
            version: VERSION,
            app_id: 440,
            operation: Operation::Identity,
        };
        assert!(request_error(&request, 440).is_none());
        assert!(request_error(&request, 570).is_some());
        // A DLC target is carried inside the operation, never in the outer
        // capability scope. It does not grant access to another app's backend.
        request.operation = Operation::AppOwnership { app_id: 441 };
        assert!(request_error(&request, 440).is_none());
        assert!(request_error(&request, 441).is_some());
        request.version += 1;
        assert!(request_error(&request, 440).is_some());
    }
}
