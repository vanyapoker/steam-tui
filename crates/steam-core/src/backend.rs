//! Experimental Steamworks bridge, scoped to one app and one CM session.
//! Credentials remain in the parent process. Normal launches do not use this.
mod events;
mod gc;
mod lobby;
mod networking;
pub mod sdr;
mod tickets;
pub use events::{Direction, Event, EventSender, gc_message_name};
use std::error::Error;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use steam_ipc::{MAX_FRAME, Operation, Reply, Request, Response, VERSION};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientGetAppOwnershipTicket, CMsgClientGetAppOwnershipTicketResponse,
};
use steam_vent_proto_steam::steammessages_clientserver_login::CMsgClientLoggedOff;
use steam_vent_proto_steam::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{Instant, timeout};

/// Owns a mode-0700 directory and its socket. Never unlinks an existing socket.
pub struct Endpoint {
    directory: PathBuf,
    listener: UnixListener,
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
        let mut session = Session {
            connection,
            app_id,
            alive: None,
            ownership: None,
            networking: networking::Networking::start(connection, app_id, events.clone()),
            tickets: tickets::Service::start(connection, app_id, events.clone())?,
            gc: gc::Gc::start(connection, app_id, events.clone()).await?,
            pops: pops.clone(),
            lobbies: lobby::Lobbies::start(
                connection,
                app_id,
                persona_name(connection).await,
                events.clone(),
            ),
            events: events.clone(),
        };
        loop {
            let (mut stream, _) = tokio::select! {
                biased;
                _ = logged_off.next() => return Err("CM account session ended; restart the backend".into()),
                accepted = self.listener.accept() => accepted?,
            };
            // SAFETY: geteuid has no preconditions.
            if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
                continue;
            }
            // No unbounded task spawning, allocations, client reads or CM waits.
            let handle_request = timeout(Duration::from_secs(12), async {
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
            tokio::select! {
                biased;
                _ = logged_off.next() => return Err("CM account session ended; restart the backend".into()),
                _ = handle_request => {},
            }
        }
    }
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

struct Session<'a> {
    connection: &'a Connection,
    app_id: u32,
    alive: Option<(Instant, bool)>,
    ownership: Option<(Instant, bool)>,
    networking: networking::Networking,
    tickets: tickets::Service,
    gc: gc::Gc,
    pops: std::sync::Arc<std::sync::Mutex<Option<Vec<(String, i32)>>>>,
    lobbies: lobby::Lobbies,
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

    async fn respond(&mut self, request: Request) -> Response {
        if let Some(error) = request_error(&request, self.app_id) {
            return error;
        }
        let alive = if let Some((at, alive)) = self.alive
            && at.elapsed() < Duration::from_secs(2)
        {
            alive
        } else {
            let alive = crate::liveness::probe(self.connection, Duration::from_secs(3)).await;
            self.alive = Some((Instant::now(), alive));
            if !alive {
                self.ownership = None;
            }
            alive
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
            | Operation::Persona
            | Operation::AuthSessionTicket { .. }
            | Operation::WebAuthTicket { .. }
            | Operation::CancelAuthTicket { .. }
            | Operation::GcSend { .. }
            | Operation::Lobby(_)
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
                    .respond(self.connection, self.app_id, operation)
                    .await
            }
            Operation::Ownership => {
                if let Some((at, owned)) = self.ownership
                    && at.elapsed() < Duration::from_secs(2)
                {
                    return Response::Ownership { owned };
                }
                let request = CMsgClientGetAppOwnershipTicket {
                    app_id: Some(self.app_id),
                    ..Default::default()
                };
                let response = timeout(
                    Duration::from_secs(5),
                    self.connection
                        .job::<_, CMsgClientGetAppOwnershipTicketResponse>(request),
                )
                .await;
                let result = match response {
                    Ok(Ok(response)) => owns_app(&response, self.app_id),
                    _ => Err("App ownership request failed or timed out"),
                };
                match result {
                    Ok(owned) => {
                        self.ownership = Some((Instant::now(), owned));
                        self.note(
                            Direction::ToSteam,
                            "CM",
                            "ClientGetAppOwnershipTicket",
                            if owned {
                                "app is owned (ticket not forwarded)"
                            } else {
                                "access denied: app not owned"
                            },
                        );
                        Response::Ownership { owned }
                    }
                    Err(message) => Response::Error {
                        message: message.into(),
                    },
                }
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
            Operation::AuthSessionTicket { capacity } => match self.tickets.issue(capacity).await {
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
            },
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
            Operation::WebAuthTicket { identity } => match self.tickets.web(identity) {
                Ok(handle) => Response::AuthTicketPending { handle },
                Err(message) => Response::Error {
                    message: message.into(),
                },
            },
            Operation::PollCallbacks => {
                let mut callbacks = self.lobbies.poll();
                callbacks.extend(self.tickets.poll());
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
            Operation::CancelAuthTicket { handle } => match self.tickets.cancel(handle) {
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
            },
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
        request.version += 1;
        assert!(request_error(&request, 440).is_some());
    }
}
