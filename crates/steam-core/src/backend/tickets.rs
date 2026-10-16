//! CM-issued tokens + ownership tickets, registered and acknowledged by CRC.
//! Based on the SteamKit2 protocol flow; never invent identities or signatures.
use futures_util::{FutureExt, Stream, StreamExt};
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait, NetworkError};
use steam_vent_proto_steam::steammessages_base::CMsgAuthTicket;
use steam_vent_proto_steam::steammessages_clientserver::{
    CMsgClientAuthList, CMsgClientAuthListAck, CMsgClientGameConnectTokens,
    CMsgClientGetAppOwnershipTicket, CMsgClientGetAppOwnershipTicketResponse,
};
use tokio::time::{Instant, timeout};

type Messages<T> = Pin<Box<dyn Stream<Item = Result<T, NetworkError>> + Send>>;
mod service;
pub(super) use service::Service;
pub(super) struct Tickets {
    pushes: Messages<CMsgClientGameConnectTokens>,
    acks: Messages<CMsgClientAuthListAck>,
    tokens: VecDeque<Vec<u8>>,
    active: BTreeMap<u32, CMsgAuthTicket>,
    started: Instant,
}

impl Tickets {
    pub fn new(connection: &Connection) -> Result<Self, &'static str> {
        let mut state = Self {
            pushes: Box::pin(connection.on::<CMsgClientGameConnectTokens>()),
            acks: Box::pin(connection.on::<CMsgClientAuthListAck>()),
            tokens: VecDeque::new(),
            active: BTreeMap::new(),
            started: Instant::now(),
        };
        for raw in connection.take_unprocessed_kind::<CMsgClientGameConnectTokens>() {
            if raw.data.len() > 64 * 1024 {
                return Err("CM token push is too large");
            }
            let push = raw
                .into_message::<CMsgClientGameConnectTokens>()
                .map_err(|_| "Malformed CM token push")?;
            state.accept_tokens(push)?;
        }
        Ok(state)
    }

    fn accept_tokens(&mut self, push: CMsgClientGameConnectTokens) -> Result<(), &'static str> {
        if push.tokens.len() > 256 || push.tokens.iter().any(|t| t.is_empty() || t.len() > 4096) {
            self.tokens.clear();
            return Err("Invalid game-connect token pool");
        }
        let keep = push.max_tokens_to_keep().min(256) as usize;
        self.tokens.extend(push.tokens);
        while self.tokens.len() > keep {
            self.tokens.pop_front();
        }
        Ok(())
    }

    async fn token(&mut self) -> Result<Vec<u8>, &'static str> {
        while let Some(push) = self.pushes.next().now_or_never() {
            let push = push
                .ok_or("CM token stream ended")?
                .map_err(|_| "CM token stream lost messages")?;
            self.accept_tokens(push)?;
        }
        if self.tokens.is_empty() {
            let push = timeout(Duration::from_secs(2), self.pushes.next())
                .await
                .map_err(|_| "No CM game-connect tokens available")?
                .ok_or("CM token stream ended")?
                .map_err(|_| "CM token stream lost messages")?;
            self.accept_tokens(push)?;
        }
        self.tokens
            .pop_front()
            .ok_or("No CM game-connect tokens available")
    }

    fn list(&self, app_id: u32) -> CMsgClientAuthList {
        CMsgClientAuthList {
            tokens_left: Some(self.tokens.len() as u32),
            app_ids: vec![app_id],
            tickets: self.active.values().cloned().collect(),
            ..Default::default()
        }
    }

    async fn publish(&mut self, connection: &Connection, app_id: u32) -> Result<(), &'static str> {
        // Discard acknowledgements of earlier snapshots before sending a new one.
        loop {
            match self.acks.next().now_or_never() {
                Some(Some(Ok(_))) => {}
                Some(_) => return Err("CM auth acknowledgement stream ended or lost messages"),
                None => break,
            }
        }
        connection
            .send(self.list(app_id))
            .await
            .map_err(|_| "Could not publish CM auth list")?;
        let expected: std::collections::BTreeSet<_> =
            self.active.values().map(|t| t.ticket_crc()).collect();
        timeout(Duration::from_secs(3), async {
            while let Some(ack) = self.acks.next().await {
                let ack = ack.map_err(|_| "CM auth acknowledgement stream lost messages")?;
                let accepted: std::collections::BTreeSet<_> = ack.ticket_crc.into_iter().collect();
                if accepted == expected {
                    return Ok(());
                }
            }
            Err("CM auth acknowledgement stream ended")
        })
        .await
        .map_err(|_| "Steam did not acknowledge the auth-list snapshot")?
    }

    pub async fn issue(
        &mut self,
        connection: &Connection,
        app_id: u32,
        capacity: u32,
        handle: u32,
        identity: Option<&[u8]>,
    ) -> Result<(u32, Vec<u8>), &'static str> {
        if capacity == 0
            || capacity > 16 * 1024
            || self.active.len() >= 64
            || handle == 0
            || self.active.contains_key(&handle)
        {
            return Err("Invalid ticket capacity or too many active tickets");
        }
        let reply = timeout(
            Duration::from_secs(3),
            connection.job::<_, CMsgClientGetAppOwnershipTicketResponse>(
                CMsgClientGetAppOwnershipTicket {
                    app_id: Some(app_id),
                    ..Default::default()
                },
            ),
        )
        .await
        .map_err(|_| "Ownership ticket request timed out")?
        .map_err(|_| "Ownership ticket request failed")?;
        if super::owns_app(&reply, app_id) != Ok(true) {
            return Err("Steam did not grant app ownership");
        }
        let token = self.token().await?;
        let auth = build_auth(
            &token,
            self.started.elapsed().as_millis() as u32,
            handle,
            if identity.is_some() { 5 } else { 2 },
        );
        let ticket = combine(&auth, reply.ticket());
        if ticket.len() > capacity as usize {
            return Err("Auth ticket buffer is too small");
        }
        let crc = crc32fast::hash(&auth);
        self.active.insert(
            handle,
            CMsgAuthTicket {
                gameid: Some(app_id as u64),
                ticket: Some(auth),
                ticket_crc: Some(crc),
                server_secret: server_identity(identity),
                ..Default::default()
            },
        );
        if let Err(error) = self.publish(connection, app_id).await {
            self.active.remove(&handle);
            let _ = connection.send(self.list(app_id)).await;
            return Err(error);
        }
        Ok((handle, ticket))
    }
}

fn server_identity(identity: Option<&[u8]>) -> Option<Vec<u8>> {
    identity.filter(|id| !id.is_empty()).map(|id| {
        let mut secret = b"str:".to_vec();
        secret.extend_from_slice(id);
        secret.push(0);
        secret
    })
}

fn build_auth(token: &[u8], timestamp: u32, sequence: u32, kind: u32) -> Vec<u8> {
    let mut ticket = Vec::new();
    ticket.extend_from_slice(&(token.len() as u32).to_le_bytes());
    ticket.extend_from_slice(token);
    // Session section: length, format, auth-session type, optional IPs, time, sequence.
    for value in [24u32, 1, kind, 0, 0, timestamp, sequence] {
        ticket.extend_from_slice(&value.to_le_bytes());
    }
    ticket
}

fn combine(auth: &[u8], ownership: &[u8]) -> Vec<u8> {
    let mut ticket = auth.to_vec();
    ticket.extend_from_slice(&(ownership.len() as u32).to_le_bytes());
    ticket.extend_from_slice(ownership);
    ticket
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_tickets_use_type_five_and_bind_service_without_altering_signed_ownership() {
        let token = [10, 20, 30];
        let session = build_auth(&token, 42, 7, 2);
        let web = build_auth(&token, 42, 8, 5);
        assert_eq!(&web[..7], &session[..7]);
        let words: Vec<_> = web[7..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        assert_eq!(words, [24, 1, 5, 0, 0, 42, 8]);
        assert_ne!(crc32fast::hash(&web), crc32fast::hash(&session));
        assert_eq!(
            server_identity(Some(b"service")),
            Some(b"str:service\0".to_vec())
        );
        assert_eq!(
            server_identity(Some(b"str:service")),
            Some(b"str:str:service\0".to_vec())
        );
        assert_eq!(server_identity(None), None);
        assert_eq!(server_identity(Some(b"")), None);
        let ownership = b"signed ownership bytes from CM";
        let full = combine(&web, ownership);
        assert_eq!(&full[web.len() + 4..], ownership);
    }

    #[test]
    fn session_ticket_layout_and_crc_match_protocol() {
        let auth = build_auth(&[10, 20, 30], 42, 7, 2);
        assert_eq!(&auth[..7], &[3, 0, 0, 0, 10, 20, 30]);
        let words: Vec<_> = auth[7..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        assert_eq!(words, [24, 1, 2, 0, 0, 42, 7]);
        let full = combine(&auth, &[5, 6]);
        assert_eq!(&full[auth.len()..], &[2, 0, 0, 0, 5, 6]);
        assert_eq!(crc32fast::hash(b"123456789"), 0xcbf43926);
        assert_ne!(crc32fast::hash(&auth), crc32fast::hash(&full));
    }

    #[test]
    fn token_pushes_trim_old_tokens_and_invalid_pushes_clear_the_pool() {
        let mut state = Tickets {
            pushes: Box::pin(futures_util::stream::pending()),
            acks: Box::pin(futures_util::stream::pending()),
            tokens: VecDeque::new(),
            active: BTreeMap::new(),
            started: Instant::now(),
        };
        state
            .accept_tokens(CMsgClientGameConnectTokens {
                tokens: vec![vec![1], vec![2], vec![3]],
                max_tokens_to_keep: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.tokens, VecDeque::from([vec![2], vec![3]]));
        state.active.insert(
            7,
            CMsgAuthTicket {
                gameid: Some(480),
                ticket_crc: Some(123),
                ..Default::default()
            },
        );
        let list = state.list(480);
        assert_eq!(list.app_ids, [480]);
        assert_eq!(list.tokens_left, Some(2));
        assert_eq!(list.tickets[0].ticket_crc(), 123);
        state.active.remove(&7);
        assert!(state.list(480).tickets.is_empty());
        assert_eq!(state.list(480).app_ids, [480]); // empty snapshot clears this app
        assert!(
            state
                .accept_tokens(CMsgClientGameConnectTokens {
                    tokens: vec![vec![]],
                    ..Default::default()
                })
                .is_err()
        );
        assert!(state.tokens.is_empty());
    }
}
