//! Steam networking certificates and app-scoped P2P rendezvous over the CM.
//! No private signing keys or rendezvous payloads are written to logs.
use super::events::{Direction, EventSender, emit};
use futures_util::StreamExt;
use protobuf::Message;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use steam_ipc::{Operation, Response};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientNetworkingCertReply, CMsgClientNetworkingCertRequest, CMsgClientP2PConnectionInfo,
};
use tokio::{task::JoinHandle, time::timeout};
#[derive(Debug)]
struct CertReply(CMsgClientNetworkingCertReply);
impl steam_vent_proto_common::RpcMessageWithKind for CertReply {
    type KindEnum = steam_vent_proto_steam::enums_clientserver::EMsg;
    const KIND: Self::KindEnum = Self::KindEnum::k_EMsgClientNetworkingCertRequestResponse;
}
impl steam_vent_proto_common::RpcMessage for CertReply {
    fn parse(reader: &mut dyn std::io::Read) -> protobuf::Result<Self> {
        Ok(Self(CMsgClientNetworkingCertReply::parse_from_reader(
            reader,
        )?))
    }
    fn write(&self, writer: &mut dyn std::io::Write) -> protobuf::Result<()> {
        self.0.write_to_writer(writer)
    }
    fn encode_size(&self) -> usize {
        self.0.compute_size() as usize
    }
}
const MAX_SIGNAL: usize = 64 * 1024;
const MAX_QUEUED: usize = 128;
fn error(message: &str) -> Response {
    Response::Error {
        message: message.into(),
    }
}
fn valid_peer(peer: u64) -> bool {
    // Public-universe, individual SteamID with a nonzero account id.
    peer >> 56 == 1 && (peer >> 52) & 15 == 1 && peer as u32 != 0
}
fn valid_incoming(message: &CMsgClientP2PConnectionInfo, app_id: u32, local: u64) -> bool {
    // Steam omits the destination on forwarded messages. If supplied, it must match.
    message.app_id() == app_id
        && !message.steam_id_dest.is_some_and(|dest| dest != local)
        && valid_peer(message.steam_id_src())
        && !message.rendezvous().is_empty()
        && message.rendezvous().len() <= MAX_SIGNAL
}
pub struct Networking {
    queue: Arc<Mutex<VecDeque<(u64, Vec<u8>)>>>,
    receiver: JoinHandle<()>,
    config: Option<(Vec<String>, Vec<u64>, Option<steam_ipc::SdrConfig>)>,
    events: Option<EventSender>,
}
impl Networking {
    pub fn start(connection: &Connection, app_id: u32, events: Option<EventSender>) -> Self {
        let queue: Arc<Mutex<VecDeque<(u64, Vec<u8>)>>> = Arc::default();
        let sink = queue.clone();
        let incoming_events = events.clone();
        let local = connection.steam_id().steam64();
        let mut incoming = Box::pin(connection.on::<CMsgClientP2PConnectionInfo>());
        let receiver = tokio::spawn(async move {
            while let Some(message) = incoming.next().await {
                let Ok(mut message) = message else { continue };
                if !valid_incoming(&message, app_id, local) {
                    continue;
                }
                emit(
                    &incoming_events,
                    app_id,
                    Direction::FromSteam,
                    "P2P",
                    "ClientP2PConnectionInfo (5434)",
                    format!("{} bytes of rendezvous data", message.rendezvous().len()),
                );
                let mut queue = sink.lock().unwrap_or_else(|e| e.into_inner());
                if queue.len() < MAX_QUEUED {
                    queue.push_back((message.steam_id_src(), message.take_rendezvous()));
                }
            }
        });
        Self {
            queue,
            receiver,
            config: None,
            events,
        }
    }
    pub async fn respond(
        &mut self,
        connection: &Connection,
        app_id: u32,
        operation: Operation,
    ) -> Response {
        match operation {
            Operation::NetworkingCertificate { public_key } => {
                if public_key.len() != 32 || public_key.iter().all(|b| *b == 0) {
                    return error("Expected a 32-byte Ed25519 public key");
                }
                let request = CMsgClientNetworkingCertRequest {
                    key_data: Some(public_key),
                    app_id: Some(app_id),
                    ..Default::default()
                };
                match timeout(
                    Duration::from_secs(5),
                    connection.with_app_id(app_id).job::<_, CertReply>(request),
                )
                .await
                {
                    Ok(Ok(CertReply(reply)))
                        if !reply.cert().is_empty()
                            && reply.cert().len() < 16 * 1024
                            && reply.ca_key_id() != 0
                            && reply.ca_signature().len() == 64 =>
                    {
                        // Fields 4/5/6 have the exact CMsgSteamDatagramCertificateSigned wire layout.
                        match reply.write_to_bytes() {
                            Ok(signed) => {
                                emit(
                                    &self.events,
                                    app_id,
                                    Direction::FromSteam,
                                    "P2P",
                                    "ClientNetworkingCertReply (5622)",
                                    "signed certificate received; key material omitted",
                                );
                                Response::NetworkingCertificate { signed }
                            }
                            Err(_) => error("Could not encode networking certificate"),
                        }
                    }
                    _ => error("Steam did not issue a signed networking certificate"),
                }
            }
            Operation::NetworkingConfig => {
                if self.config.is_none() {
                    let result = timeout(Duration::from_secs(5), async {
                        let text = reqwest::get(format!("https://api.steampowered.com/ISteamApps/GetSDRConfig/v1/?appid={app_id}"))
                            .await?.error_for_status()?.text().await?;
                        Ok::<_, reqwest::Error>(text)
                    }).await;
                    let Ok(Ok(text)) = result else {
                        return error("Networking trust configuration unavailable");
                    };
                    let Ok(config) = serde_json::from_str::<serde_json::Value>(&text) else {
                        return error("Malformed networking configuration");
                    };
                    let Some(certs) = config["certs"].as_array() else {
                        return error("Networking configuration has no certificates");
                    };
                    let certs: Vec<String> = certs
                        .iter()
                        .filter_map(|v| v.as_str().filter(|s| s.len() < 16384).map(str::to_owned))
                        .take(256)
                        .collect();
                    let revoked = config["revoked_keys"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                        .take(4096)
                        .collect();
                    if certs.is_empty() {
                        return error("Networking trust store is empty");
                    }
                    let sdr = super::sdr::parse_config(&config);
                    self.config = Some((certs, revoked, sdr));
                }
                let (certs, revoked_keys, sdr) = self.config.as_ref().unwrap().clone();
                Response::NetworkingConfig {
                    certs,
                    revoked_keys,
                    sdr,
                }
            }
            Operation::P2PSend { peer, rendezvous } => {
                if !valid_peer(peer) || rendezvous.is_empty() || rendezvous.len() > MAX_SIGNAL {
                    return error("Invalid P2P signal");
                }
                let size = rendezvous.len();
                let message = CMsgClientP2PConnectionInfo {
                    steam_id_dest: Some(peer),
                    steam_id_src: Some(connection.steam_id().steam64()),
                    app_id: Some(app_id),
                    rendezvous: Some(rendezvous),
                    ..Default::default()
                };
                match timeout(
                    Duration::from_secs(3),
                    connection.with_app_id(app_id).send(message),
                )
                .await
                {
                    Ok(Ok(())) => {
                        emit(
                            &self.events,
                            app_id,
                            Direction::ToSteam,
                            "P2P",
                            "ClientP2PConnectionInfo (5434)",
                            format!("{size} bytes of rendezvous data"),
                        );
                        Response::P2PSent
                    }
                    _ => error("Could not send P2P signal"),
                }
            }
            Operation::P2PRecv => {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                let count = queue.len().min(8);
                Response::P2PSignals(queue.drain(..count).collect())
            }
            _ => error("Not a networking operation"),
        }
    }
}
impl Drop for Networking {
    fn drop(&mut self) {
        self.receiver.abort();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarded_signals_validate_app_source_destination_and_size() {
        let local = 76561198000000001;
        let mut message = CMsgClientP2PConnectionInfo {
            app_id: Some(480),
            steam_id_src: Some(76561198000000002),
            rendezvous: Some(vec![1]),
            ..Default::default()
        };
        assert!(valid_incoming(&message, 480, local));
        message.steam_id_dest = Some(local);
        assert!(valid_incoming(&message, 480, local));
        message.steam_id_dest = Some(local + 1);
        assert!(!valid_incoming(&message, 480, local));
        message.steam_id_dest = None;
        assert!(!valid_incoming(&message, 440, local));
        message.steam_id_src = Some(0);
        assert!(!valid_incoming(&message, 480, local));
        message.steam_id_src = Some(local + 1);
        message.rendezvous = Some(vec![]);
        assert!(!valid_incoming(&message, 480, local));
        message.rendezvous = Some(vec![0; MAX_SIGNAL + 1]);
        assert!(!valid_incoming(&message, 480, local));
    }
    #[test]
    fn reject_non_individual_peers() {
        assert!(valid_peer(76561198000000001));
        assert!(!valid_peer(0));
        assert!(!valid_peer(0x0110000000000000));
        assert!(!valid_peer(0x0120000000000001));
    }
}
