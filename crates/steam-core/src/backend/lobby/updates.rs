//! Metadata setters admit local edits immediately; CM acknowledgement is a
//! background operation. Unreal invokes dozens of setters for each update, so
//! awaiting a server roundtrip in each one starves its session-destroy task.
use super::*;
use tokio::time::Instant;

type Key = (u64, u64); // lobby, member (zero for owner metadata)
const LIMIT: usize = 128;
const DEBOUNCE: Duration = Duration::from_millis(50);
#[derive(Clone)]
struct Update {
    request: CMsgClientMMSSetLobbyData,
    revision: u64,
    ready: Instant,
    failures: u32,
}
#[derive(Default)]
pub(super) struct Pending {
    entries: HashMap<Key, Update>,
    revision: u64,
}
impl Pending {
    pub fn has_capacity(&self, key: Key) -> bool {
        self.entries.contains_key(&key) || self.entries.len() < LIMIT
    }
    pub fn contains(&self, key: Key) -> bool {
        self.entries.contains_key(&key)
    }
    pub fn members(&self, lobby: u64) -> impl Iterator<Item = u64> + '_ {
        self.entries
            .keys()
            .filter_map(move |(id, member)| (*id == lobby && *member != 0).then_some(*member))
    }
    pub fn cancel(&mut self, lobby: u64) {
        self.entries.retain(|(id, _), _| *id != lobby);
    }
    pub fn enqueue(&mut self, request: CMsgClientMMSSetLobbyData) {
        let key = (request.steam_id_lobby(), request.steam_id_member());
        if self
            .entries
            .get(&key)
            .is_some_and(|old| old.request == request)
        {
            return;
        }
        self.revision += 1;
        // Preserve the first deadline so a steady stream of edits can't postpone
        // publication forever. An edit while a snapshot is in flight gets a new
        // revision and cannot be cleared by that older acknowledgement.
        let ready = self
            .entries
            .get(&key)
            .map_or(Instant::now() + DEBOUNCE, |old| old.ready);
        self.entries.insert(
            key,
            Update {
                request,
                revision: self.revision,
                ready,
                failures: 0,
            },
        );
    }
    fn next(&self) -> Option<Update> {
        self.entries
            .values()
            .min_by_key(|u| (u.ready, u.revision))
            .cloned()
    }
    fn finish(&mut self, update: &Update, success: bool) -> bool {
        let key = (
            update.request.steam_id_lobby(),
            update.request.steam_id_member(),
        );
        let Some(current) = self.entries.get_mut(&key) else {
            return false;
        };
        if current.revision != update.revision {
            return false;
        }
        if success {
            self.entries.remove(&key);
        } else {
            current.failures = current.failures.saturating_add(1);
            current.ready =
                Instant::now() + Duration::from_secs((1u64 << current.failures.min(5)).min(30));
        }
        true
    }
}

pub(super) fn valid_pair(key: &str, value: &str) -> bool {
    !key.is_empty()
        && key.len() < 256
        && value.len() < 8192
        && !key.contains('\0')
        && !value.contains('\0')
}
pub(super) fn valid_metadata(data: &[(String, String)]) -> bool {
    data.iter().all(|(k, v)| valid_pair(k, v))
        && data.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() <= 8192
}
pub(super) fn request(
    app_id: u32,
    lobby: u64,
    member: u64,
    l: &Lobby,
    data: Vec<(String, String)>,
) -> CMsgClientMMSSetLobbyData {
    CMsgClientMMSSetLobbyData {
        app_id: Some(app_id),
        steam_id_lobby: Some(lobby),
        steam_id_member: Some(member),
        max_members: Some(l.max_members),
        lobby_type: Some(l.kind),
        lobby_flags: Some(l.flags),
        metadata: Some(kv_encode(&data)),
        ..Default::default()
    }
}

pub(super) fn start(
    connection: Connection,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    app_id: u32,
    events: Option<EventSender>,
) -> tokio::task::JoinHandle<()> {
    start_sender(state, notify, app_id, events, move |request| {
        let connection = connection.clone();
        async move {
            connection
                .job::<_, CMsgClientMMSSetLobbyDataResponse>(request)
                .await
                .map_err(|e| e.to_string())
        }
    })
}

fn start_sender<F, Fut>(
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    app_id: u32,
    events: Option<EventSender>,
    mut send: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut(CMsgClientMMSSetLobbyData) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<CMsgClientMMSSetLobbyDataResponse, String>>
        + Send
        + 'static,
{
    tokio::spawn(async move {
        loop {
            // Register before checking the queue so an edit between the check
            // and await cannot lose its wakeup.
            let notified = notify.notified();
            let next = lock(&state).updates.next();
            let Some(update) = next else {
                notified.await;
                continue;
            };
            if update.ready > Instant::now() {
                tokio::select! { _ = tokio::time::sleep_until(update.ready) => {}, _ = notified => {} }
                continue;
            }
            let lobby = update.request.steam_id_lobby();
            let member = update.request.steam_id_member();
            emit(
                &events,
                app_id,
                Direction::ToSteam,
                "MMS",
                "MMSSetLobbyData (background)",
                format!(
                    "lobby {lobby}, revision {}, member {member}",
                    update.revision
                ),
            );
            // Only one snapshot is in flight. Never hold the local state lock
            // while waiting for CM, and never block the backend IPC dispatcher.
            let reply = tokio::time::timeout(TIMEOUT, send(update.request.clone())).await;
            let success = matches!(&reply, Ok(Ok(r)) if r.eresult() == 1 && r.app_id() == app_id && r.steam_id_lobby() == lobby);
            let detail = match reply {
                Ok(Ok(r)) => format!(
                    "lobby {lobby}, result {}, app {}, response lobby {}",
                    r.eresult(),
                    r.app_id(),
                    r.steam_id_lobby()
                ),
                Ok(Err(error)) => format!("lobby {lobby}, transport error: {error}"),
                Err(_) => format!("lobby {lobby}, acknowledgement timed out; retry queued"),
            };
            emit(
                &events,
                app_id,
                Direction::FromSteam,
                "MMS",
                "MMSSetLobbyDataResponse",
                detail,
            );
            let current = lock(&state).updates.finish(&update, success);
            if current {
                let mut payload = data_update(lobby, if member == 0 { lobby } else { member });
                payload[16] = u8::from(success);
                push(&state, LOBBY_DATA_UPDATE, payload);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make(lobby: u64, value: &str) -> CMsgClientMMSSetLobbyData {
        request(
            480,
            lobby,
            0,
            &Lobby::default(),
            vec![("name".into(), value.into())],
        )
    }
    #[test]
    fn coalesces_bursts_without_losing_new_edits_or_resurrecting_left_lobbies() {
        let mut pending = Pending::default();
        for i in 0..100 {
            pending.enqueue(make(1, &i.to_string()));
        }
        assert_eq!(pending.entries.len(), 1);
        let first = pending.next().unwrap();
        assert_eq!(
            kv_decode(first.request.metadata()),
            vec![("name".into(), "99".into())]
        );
        pending.enqueue(make(1, "new"));
        assert!(!pending.finish(&first, true));
        let new = pending.next().unwrap();
        assert!(pending.finish(&new, false));
        assert!(pending.next().unwrap().ready > Instant::now());
        assert!(pending.finish(&new, true));
        assert!(pending.next().is_none());
        pending.enqueue(make(1, "last"));
        let late = pending.next().unwrap();
        pending.cancel(1);
        assert!(!pending.finish(&late, true));
        assert!(pending.next().is_none());
    }
    #[tokio::test]
    async fn stalled_cm_reply_does_not_block_edits_or_leaving_and_late_reply_is_discarded() {
        let state: Arc<Mutex<State>> = Arc::default();
        let notify = Arc::new(Notify::new());
        lock(&state).updates.enqueue(make(7, "first"));
        let (sent, mut sends) = tokio::sync::mpsc::channel(2);
        let worker = start_sender(state.clone(), notify.clone(), 480, None, move |request| {
            let sent = sent.clone();
            async move {
                let (reply, wait) = tokio::sync::oneshot::channel();
                sent.send((request, reply)).await.unwrap();
                wait.await.map_err(|_| "test sender closed".into())
            }
        });
        let (_, reply) = tokio::time::timeout(Duration::from_secs(1), sends.recv())
            .await
            .unwrap()
            .unwrap();
        // The response is withheld. Local state, setters, and cancellation must
        // stay usable on this same runtime, without completing that response.
        {
            let mut state = lock(&state);
            for i in 0..100 {
                state.updates.enqueue(make(7, &i.to_string()));
            }
            assert_eq!(state.updates.entries.len(), 1);
            assert!(state.callbacks.is_empty()); // no fabricated success
            state.updates.cancel(7);
            state.left.insert(7);
        }
        notify.notify_one();
        reply
            .send(CMsgClientMMSSetLobbyDataResponse {
                app_id: Some(480),
                steam_id_lobby: Some(7),
                eresult: Some(1),
                ..Default::default()
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(lock(&state).callbacks.is_empty());
        assert!(!lock(&state).lobbies.contains_key(&7));
        assert!(lock(&state).updates.next().is_none());
        worker.abort();
    }

    #[test]
    fn bounded_queue_and_metadata_admission() {
        let mut pending = Pending::default();
        for id in 0..LIMIT as u64 {
            assert!(pending.has_capacity((id, 0)));
            pending.enqueue(make(id, "x"));
        }
        assert!(!pending.has_capacity((999, 0)));
        assert!(pending.has_capacity((1, 0)));
        assert!(!valid_pair("", "v"));
        assert!(!valid_pair("key", "x\0y"));
        assert!(!valid_metadata(&vec![
            ("x".repeat(200), "y".repeat(8000));
            2
        ]));
        assert!(valid_metadata(&vec![("map".into(), "jungle".into())]));
    }
}
