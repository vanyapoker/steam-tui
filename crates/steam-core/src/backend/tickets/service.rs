//! One bounded worker owns token consumption and CM auth-list publication.
//! Web requests return a handle immediately; only CM acknowledgement queues success.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc, oneshot};

const LIMIT: usize = 64;
type Issued = Result<(u32, Vec<u8>), &'static str>;
#[derive(Default)]
struct Entry {
    callback: Option<Vec<u8>>,
}
#[derive(Default)]
struct State {
    next: u32,
    live: BTreeMap<u32, Entry>,
}
impl State {
    fn reserve(&mut self) -> Result<u32, &'static str> {
        if self.live.len() >= LIMIT {
            return Err("Too many active auth tickets");
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Auth ticket handles exhausted")?;
        self.live.insert(self.next, Entry::default());
        Ok(self.next)
    }
    fn cancel(&mut self, handle: u32) -> bool {
        self.live.remove(&handle).is_some()
    }
    fn complete_web(&mut self, handle: u32, result: &Issued) {
        if let Some(entry) = self.live.get_mut(&handle) {
            entry.callback = steam_ipc::web_ticket_callback(
                handle,
                if result.is_ok() { 1 } else { 2 },
                result.as_ref().map_or(&[], |(_, ticket)| ticket.as_slice()),
            );
        }
    }
    fn poll(&mut self) -> Vec<(i32, Vec<u8>)> {
        self.live
            .values_mut()
            .filter_map(|e| e.callback.take().map(|b| (168, b)))
            .collect()
    }
}
struct Job {
    handle: u32,
    capacity: u32,
    identity: Option<Vec<u8>>,
    reply: Option<oneshot::Sender<Issued>>,
}
pub(crate) struct Service {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    sender: mpsc::Sender<Job>,
    worker: tokio::task::JoinHandle<()>,
}
impl Drop for Service {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
impl Service {
    pub fn start(
        connection: &Connection,
        app_id: u32,
        events: Option<super::super::EventSender>,
    ) -> Result<Self, &'static str> {
        let tickets = Tickets::new(connection)?;
        let state = Arc::new(Mutex::new(State::default()));
        let wake = Arc::new(Notify::new());
        let (sender, receiver) = mpsc::channel(LIMIT);
        let worker = tokio::spawn(run(
            connection.clone(),
            app_id,
            tickets,
            state.clone(),
            wake.clone(),
            receiver,
            events,
        ));
        Ok(Self {
            state,
            wake,
            sender,
            worker,
        })
    }
    fn enqueue(
        &self,
        capacity: u32,
        identity: Option<Vec<u8>>,
        reply: Option<oneshot::Sender<Issued>>,
    ) -> Result<u32, &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let handle = state.reserve()?;
        if self
            .sender
            .try_send(Job {
                handle,
                capacity,
                identity,
                reply,
            })
            .is_err()
        {
            state.cancel(handle);
            return Err("Auth ticket worker queue is full or closed");
        }
        Ok(handle)
    }
    pub async fn issue(&self, capacity: u32) -> Issued {
        if capacity == 0 || capacity > 16 * 1024 {
            return Err("Invalid auth ticket capacity");
        }
        let (tx, rx) = oneshot::channel();
        let handle = self.enqueue(capacity, None, Some(tx))?;
        // Cancel the reservation even if the IPC request future is dropped.
        struct Reservation<'a>(&'a Service, u32);
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                if self.1 != 0 {
                    let _ = self.0.cancel(self.1);
                }
            }
        }
        let mut reservation = Reservation(self, handle);
        let result = rx.await.unwrap_or(Err("Auth ticket worker stopped"));
        if result.is_ok() {
            reservation.1 = 0;
        }
        result
    }
    pub fn web(&self, identity: Vec<u8>) -> Result<u32, &'static str> {
        if identity.len() > steam_ipc::WEB_IDENTITY_MAX || identity.contains(&0) {
            return Err("Invalid web ticket service identity");
        }
        self.enqueue(steam_ipc::WEB_TICKET_MAX as u32, Some(identity), None)
    }
    pub fn cancel(&self, handle: u32) -> Result<(), &'static str> {
        if !self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel(handle)
        {
            return Err("Unknown auth ticket handle");
        }
        self.wake.notify_one();
        Ok(())
    }
    pub fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).poll()
    }
}
trait Driver {
    fn reconcile(
        &mut self,
        live: std::collections::BTreeSet<u32>,
    ) -> impl std::future::Future<Output = bool> + Send;
    fn issue(&mut self, job: &Job) -> impl std::future::Future<Output = Issued> + Send;
}
struct CmDriver {
    connection: Connection,
    app_id: u32,
    tickets: Tickets,
    dirty: bool,
    events: Option<super::super::EventSender>,
}
impl Driver for CmDriver {
    async fn reconcile(&mut self, live: std::collections::BTreeSet<u32>) -> bool {
        let count = self.tickets.active.len();
        self.tickets
            .active
            .retain(|handle, _| live.contains(handle));
        self.dirty |= count != self.tickets.active.len();
        if self.dirty {
            self.dirty = self
                .tickets
                .publish(&self.connection, self.app_id)
                .await
                .is_err();
        }
        self.dirty
    }
    async fn issue(&mut self, job: &Job) -> Issued {
        let result = self
            .tickets
            .issue(
                &self.connection,
                self.app_id,
                job.capacity,
                job.handle,
                job.identity.as_deref(),
            )
            .await;
        super::super::events::emit(
            &self.events,
            self.app_id,
            super::super::Direction::ToSteam,
            "CM",
            if job.identity.is_some() {
                "ClientAuthList (web API ticket)"
            } else {
                "ClientAuthList (session ticket)"
            },
            match &result {
                Ok((handle, ticket)) => format!(
                    "ticket handle {handle}, {} bytes acknowledged by Steam (bytes hidden)",
                    ticket.len()
                ),
                Err(error) => format!("ticket handle {} failed: {error}", job.handle),
            },
        );
        self.dirty |= result.is_err();
        result
    }
}
async fn run(
    connection: Connection,
    app_id: u32,
    tickets: Tickets,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    receiver: mpsc::Receiver<Job>,
    events: Option<super::super::EventSender>,
) {
    worker(
        CmDriver {
            connection,
            app_id,
            tickets,
            dirty: false,
            events,
        },
        state,
        wake,
        receiver,
    )
    .await;
}
async fn worker(
    mut driver: impl Driver,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    mut receiver: mpsc::Receiver<Job>,
) {
    loop {
        // No state lock is held across a CM await. A canceled in-flight ticket
        // never receives a callback and is removed in the following snapshot.
        let live = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .keys()
            .copied()
            .collect();
        let dirty = driver.reconcile(live).await;
        let job = tokio::select! {
            job = receiver.recv() => match job { Some(job) => job, None => break },
            _ = wake.notified() => continue,
            _ = tokio::time::sleep(Duration::from_secs(2)), if dirty => continue,
        };
        if !state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .contains_key(&job.handle)
        {
            continue;
        }
        let result = driver.issue(&job).await;
        if let Some(reply) = job.reply {
            if reply.send(result).is_err() {
                state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cancel(job.handle);
            }
        } else {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .complete_web(job.handle, &result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct HeldDriver {
        started: Option<oneshot::Sender<()>>,
        completion: Option<oneshot::Receiver<()>>,
        active: Arc<Mutex<std::collections::BTreeSet<u32>>>,
    }
    impl Driver for HeldDriver {
        async fn reconcile(&mut self, live: std::collections::BTreeSet<u32>) -> bool {
            self.active
                .lock()
                .unwrap()
                .retain(|handle| live.contains(handle));
            false
        }
        async fn issue(&mut self, job: &Job) -> Issued {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            if let Some(completion) = self.completion.take() {
                let _ = completion.await;
            }
            self.active.lock().unwrap().insert(job.handle);
            Ok((job.handle, vec![9; 32]))
        }
    }
    #[tokio::test]
    async fn withheld_ack_does_not_block_reservations_polling_or_cancel() {
        let state = Arc::new(Mutex::new(State::default()));
        let wake = Arc::new(Notify::new());
        let (sender, receiver) = mpsc::channel(LIMIT);
        let (started, started_rx) = oneshot::channel();
        let (complete, completion) = oneshot::channel();
        let active = Arc::new(Mutex::new(Default::default()));
        let worker = tokio::spawn(worker(
            HeldDriver {
                started: Some(started),
                completion: Some(completion),
                active: active.clone(),
            },
            state.clone(),
            wake.clone(),
            receiver,
        ));
        let service = Service {
            state,
            wake,
            sender,
            worker,
        };
        assert!(
            service
                .web(vec![1; steam_ipc::WEB_IDENTITY_MAX + 1])
                .is_err()
        );
        assert!(service.web(vec![0]).is_err());
        let canceled = service.web(b"service".to_vec()).unwrap();
        started_rx.await.unwrap();
        assert!(service.poll().is_empty());
        service.cancel(canceled).unwrap();
        let next = service.web(Vec::new()).unwrap();
        assert_ne!(canceled, next);
        assert!(service.poll().is_empty());
        complete.send(()).unwrap();
        let callbacks = timeout(Duration::from_secs(1), async {
            loop {
                let callbacks = service.poll();
                if !callbacks.is_empty() {
                    break callbacks;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(&callbacks[0].1[..4], &next.to_le_bytes());
        assert!(!active.lock().unwrap().contains(&canceled));
        service.cancel(next).unwrap();
        timeout(Duration::from_secs(1), async {
            while !active.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn bounded_handles_callbacks_and_late_cancellation() {
        let mut state = State::default();
        let first = state.reserve().unwrap();
        state.complete_web(first, &Ok((first, vec![7; 64])));
        assert!(state.cancel(first));
        assert!(state.poll().is_empty());
        state.complete_web(first, &Ok((first, vec![7; 64])));
        assert!(state.poll().is_empty());
        for _ in 0..LIMIT {
            state.reserve().unwrap();
        }
        assert!(state.reserve().is_err());
        let next = first + 1;
        state.complete_web(next, &Err("Timed out"));
        let callbacks = state.poll();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0].0, 168);
        assert_eq!(&callbacks[0].1[4..12], &[2, 0, 0, 0, 0, 0, 0, 0]);
        assert!(state.poll().is_empty());
        assert!(state.cancel(next));
        assert!(state.reserve().unwrap() > next);
        state.next = u32::MAX;
        assert!(state.reserve().is_err());
    }
}
