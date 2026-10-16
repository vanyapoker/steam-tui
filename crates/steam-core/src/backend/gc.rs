//! Transparent Game Coordinator relay: the game builds complete GC messages
//! (`ISteamGameCoordinator::SendMessage`), we wrap them in `ClientToGC` and queue
//! whatever `ClientFromGC` brings back for the game to poll. Nothing is
//! interpreted here, so every GC-backed game works the same way.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::events::{Direction, EventSender, emit, gc_detail, gc_title};
use futures_util::StreamExt;
use protobuf::Message;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_common::protobuf;
use steam_vent_proto_common::{RpcMessage, RpcMessageWithKind};
use steam_vent_proto_steam::enums_clientserver::EMsg;
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientGamesPlayed, cmsg_client_games_played::GamePlayed,
};
use steam_vent_proto_steam::steammessages_clientserver_2::CMsgGCClient;
use steam_vent_proto_steam::steammessages_clientserver_login::CMsgClientHello;

/// GC protocol message ids shared by all games (see steam-vent's GCMsgKind).
const K_EMSG_GC_CLIENT_WELCOME: u32 = 4004;
const K_EMSG_GC_CLIENT_HELLO: u32 = 4006;
const PROTOBUF_FLAG: u32 = 0x8000_0000;

/// Bounds memory if the game stops polling.
const MAX_QUEUED: usize = 4096;

#[derive(Debug)]
struct ToGc(CMsgGCClient);
#[derive(Debug)]
struct FromGc(CMsgGCClient);

macro_rules! gc_wire {
    ($name:ident, $kind:expr) => {
        impl RpcMessageWithKind for $name {
            type KindEnum = EMsg;
            const KIND: Self::KindEnum = $kind;
        }
        impl RpcMessage for $name {
            fn parse(reader: &mut dyn std::io::Read) -> protobuf::Result<Self> {
                Ok(Self(<CMsgGCClient as Message>::parse_from_reader(reader)?))
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
gc_wire!(ToGc, EMsg::k_EMsgClientToGC);
gc_wire!(FromGc, EMsg::k_EMsgClientFromGC);

pub struct Gc {
    queue: Arc<Mutex<VecDeque<(u32, Vec<u8>)>>>,
    events: Option<EventSender>,
}

impl Gc {
    /// Starts collecting this app's inbound GC messages and announces that the
    /// account is playing the app (cleared when this CM session ends), so the GC
    /// has a session before the game's first hello arrives.
    pub async fn start(
        connection: &Connection,
        app_id: u32,
        events: Option<EventSender>,
    ) -> Result<Self, &'static str> {
        let queue: Arc<Mutex<VecDeque<(u32, Vec<u8>)>>> = Arc::default();
        let sink = queue.clone();
        let welcomed = Arc::new(AtomicBool::new(false));
        let seen = welcomed.clone();
        let sink_events = events.clone();
        let mut stream = Box::pin(connection.on::<FromGc>());
        tokio::spawn(async move {
            while let Some(message) = stream.next().await {
                let Ok(FromGc(mut data)) = message else {
                    continue;
                };
                if data.appid != Some(app_id) {
                    continue;
                }
                if data.msgtype() & !PROTOBUF_FLAG == K_EMSG_GC_CLIENT_WELCOME {
                    seen.store(true, Ordering::SeqCst);
                }
                emit(
                    &sink_events,
                    app_id,
                    Direction::FromSteam,
                    "GC",
                    gc_title(data.msgtype()),
                    gc_detail(data.msgtype(), data.payload()),
                );
                let mut queue = sink.lock().unwrap_or_else(|e| e.into_inner());
                if queue.len() < MAX_QUEUED {
                    queue.push_back((data.msgtype(), data.take_payload()));
                }
            }
        });
        connection
            .send(CMsgClientGamesPlayed {
                games_played: vec![GamePlayed {
                    game_id: Some(u64::from(app_id)),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .await
            .map_err(|_| "Could not announce the played app to Steam")?;
        // A game's own hello can take ~30 s to appear, and the GC only answers
        // with a welcome once it has one. Nudge it with the generic hello until a
        // welcome arrives; the game receives that welcome from the queue.
        let hello_connection = connection.clone();
        let hello_events = events.clone();
        tokio::spawn(async move {
            for _ in 0..18 {
                if welcomed.load(Ordering::SeqCst) {
                    return;
                }
                let body = CMsgClientHello {
                    protocol_version: Some(1), // field 1 varint, same wire form as the GC hello
                    ..Default::default()
                };
                let mut payload = (K_EMSG_GC_CLIENT_HELLO | PROTOBUF_FLAG)
                    .to_le_bytes()
                    .to_vec();
                payload.extend(0u32.to_le_bytes()); // empty GC protobuf header
                if body.write_to_vec(&mut payload).is_err() {
                    return;
                }
                emit(
                    &hello_events,
                    app_id,
                    Direction::ToSteam,
                    "GC",
                    format!(
                        "{} · backend nudge",
                        gc_title(K_EMSG_GC_CLIENT_HELLO | PROTOBUF_FLAG)
                    ),
                    gc_detail(K_EMSG_GC_CLIENT_HELLO | PROTOBUF_FLAG, &payload),
                );
                let _ = hello_connection
                    .send(ToGc(CMsgGCClient {
                        appid: Some(app_id),
                        msgtype: Some(K_EMSG_GC_CLIENT_HELLO | PROTOBUF_FLAG),
                        payload: Some(payload),
                        ..Default::default()
                    }))
                    .await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        Ok(Self { queue, events })
    }

    pub async fn send(
        &self,
        connection: &Connection,
        app_id: u32,
        msg_type: u32,
        payload: Vec<u8>,
    ) -> Result<(), &'static str> {
        emit(
            &self.events,
            app_id,
            Direction::ToSteam,
            "GC",
            gc_title(msg_type),
            gc_detail(msg_type, &payload),
        );
        connection
            .send(ToGc(CMsgGCClient {
                appid: Some(app_id),
                msgtype: Some(msg_type),
                payload: Some(payload),
                ..Default::default()
            }))
            .await
            .map_err(|_| "Could not send the Game Coordinator message")
    }

    pub fn recv(&self) -> Option<(u32, Vec<u8>)> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}
