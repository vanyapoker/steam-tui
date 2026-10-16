use crate::message::ServiceMethodNotification;
use crate::net::RawNetMessage;
use dashmap::DashMap;
use futures_util::Stream;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::pin::pin;
use std::sync::Arc;
use steam_vent_core::JobId;
use steam_vent_proto_common::MsgKind;
use steam_vent_proto_steam::enums_clientserver::EMsg;
use tokio::spawn;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::StreamExt;
use tracing::{debug, error};

#[derive(Clone)]
pub struct RingBuffer<T>(Arc<Mutex<VecDeque<T>>>);

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(VecDeque::with_capacity(capacity))))
    }

    pub fn push(&self, item: T) -> Option<T> {
        let mut deque = self.0.lock();
        if deque.len() == deque.capacity() {
            let popped = deque.pop_front();
            deque.push_back(item);
            debug_assert!(deque.len() == deque.capacity());
            popped
        } else {
            deque.push_back(item);
            None
        }
    }

    #[allow(dead_code)]
    pub fn pop(&self) -> Option<T> {
        self.0.lock().pop_front()
    }

    /// Move only selected messages out, preserving every other service's order.
    pub fn take_where(&self, mut selected: impl FnMut(&T) -> bool) -> Vec<T> {
        let mut deque = self.0.lock();
        let mut items = Vec::new();
        for _ in 0..deque.len() {
            let item = deque.pop_front().unwrap();
            if selected(&item) {
                items.push(item);
            } else {
                deque.push_back(item);
            }
        }
        items
    }
}

impl<T: Clone> RingBuffer<T> {
    pub fn take(&self) -> Vec<T> {
        let mut dequeu = self.0.lock();
        let items = dequeu.make_contiguous().to_vec();
        dequeu.clear();
        items
    }
}

/// A filter for incoming messages, allowing listing by message type, job id and notifications
#[derive(Clone)]
pub struct MessageFilter {
    job_id_filters: Arc<DashMap<JobId, oneshot::Sender<RawNetMessage>>>,
    job_id_multi_filters: Arc<DashMap<JobId, mpsc::Sender<RawNetMessage>>>,
    notification_filters: Arc<DashMap<&'static str, broadcast::Sender<ServiceMethodNotification>>>,
    kind_filters: Arc<DashMap<MsgKind, broadcast::Sender<RawNetMessage>>>,
    oneshot_kind_filters: Arc<DashMap<MsgKind, oneshot::Sender<RawNetMessage>>>,
    rest: RingBuffer<RawNetMessage>,
}

/// Owns a multipart subscription, including cleanup when its stream is cancelled.
pub struct MultiJobReceiver {
    receiver: mpsc::Receiver<RawNetMessage>,
    filters: Arc<DashMap<JobId, mpsc::Sender<RawNetMessage>>>,
    id: JobId,
}

impl MultiJobReceiver {
    pub async fn recv(&mut self) -> Option<RawNetMessage> {
        self.receiver.recv().await
    }
}

impl Drop for MultiJobReceiver {
    fn drop(&mut self) {
        self.filters.remove(&self.id);
    }
}

impl MessageFilter {
    pub fn new<Input: Stream<Item = crate::connection::Result<RawNetMessage>> + Send + 'static>(
        source: Input,
    ) -> Self {
        let filter = MessageFilter {
            job_id_filters: Default::default(),
            job_id_multi_filters: Default::default(),
            kind_filters: Default::default(),
            notification_filters: Default::default(),
            oneshot_kind_filters: Default::default(),
            rest: RingBuffer::new(32),
        };

        let filter_send = filter.clone();
        spawn(async move {
            let mut source = pin!(source);
            while let Some(res) = source.next().await {
                match res {
                    Ok(message) => {
                        debug!(job_id = ?message.header.target_job_id, kind = ?message.kind, "processing message");
                        if let Some((_, tx)) = filter_send
                            .job_id_filters
                            .remove(&message.header.target_job_id)
                        {
                            tx.send(message).ok();
                        } else if let Some(tx) = filter_send
                            .job_id_multi_filters
                            .get(&message.header.target_job_id)
                            .map(|entry| entry.value().clone())
                        {
                            tx.send(message).await.ok();
                        } else if let Some((_, tx)) =
                            filter_send.oneshot_kind_filters.remove(&message.kind)
                        {
                            tx.send(message).ok();
                        } else if message.kind == EMsg::k_EMsgServiceMethod {
                            if let Ok(notification) =
                                message.into_message::<ServiceMethodNotification>()
                            {
                                debug!(
                                    job_name = notification.job_name.as_str(),
                                    "processing notification"
                                );
                                if let Some(tx) = filter_send
                                    .notification_filters
                                    .get(notification.job_name.as_str())
                                {
                                    tx.send(notification).ok();
                                }
                            }
                        } else if let Some(tx) = filter_send.kind_filters.get(&message.kind) {
                            tx.send(message).ok();
                        } else if let Some(popped) = filter_send.rest.push(message) {
                            debug!(kind = ?popped.kind, "Unhandled message");
                        }
                    }
                    Err(err) => {
                        error!(error = ?err, "Error while reading message");
                    }
                }
            }
        });
        filter
    }

    pub fn on_job_id(&self, id: JobId) -> oneshot::Receiver<RawNetMessage> {
        let (tx, rx) = oneshot::channel();
        self.job_id_filters.insert(id, tx);
        rx
    }

    pub fn on_job_id_multi(&self, id: JobId) -> MultiJobReceiver {
        let (tx, receiver) = mpsc::channel(16);
        self.job_id_multi_filters.insert(id, tx);
        MultiJobReceiver {
            receiver,
            filters: self.job_id_multi_filters.clone(),
            id,
        }
    }

    pub fn on_notification(
        &self,
        job_name: &'static str,
    ) -> broadcast::Receiver<ServiceMethodNotification> {
        let tx = self
            .notification_filters
            .entry(job_name)
            .or_insert_with(|| broadcast::channel(16).0);
        tx.subscribe()
    }

    pub fn on_kind<K: Into<MsgKind>>(&self, kind: K) -> broadcast::Receiver<RawNetMessage> {
        let tx = self
            .kind_filters
            .entry(kind.into())
            .or_insert_with(|| broadcast::channel(16).0);
        tx.subscribe()
    }

    pub fn one_kind<K: Into<MsgKind>>(&self, kind: K) -> oneshot::Receiver<RawNetMessage> {
        let (tx, rx) = oneshot::channel();
        self.oneshot_kind_filters.insert(kind.into(), tx);
        rx
    }

    pub fn unprocessed(&self) -> Vec<RawNetMessage> {
        self.rest.take()
    }

    pub fn unprocessed_kind<K: Into<MsgKind>>(&self, kind: K) -> Vec<RawNetMessage> {
        let kind = kind.into();
        self.rest.take_where(|message| message.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selective_startup_drain_preserves_other_services_and_order() {
        let buffer = RingBuffer::new(6);
        for value in [1, 2, 3, 4, 5, 6] {
            buffer.push(value);
        }
        assert_eq!(buffer.take_where(|value| value % 2 == 0), vec![2, 4, 6]);
        assert_eq!(buffer.take_where(|value| *value == 5), vec![5]);
        assert_eq!(buffer.take(), vec![1, 3]);
    }

    #[tokio::test]
    async fn dropping_multipart_receiver_removes_subscription() {
        let filter = MessageFilter::new(futures_util::stream::pending());
        let id = JobId::new(42);
        let receiver = filter.on_job_id_multi(id);
        assert!(filter.job_id_multi_filters.contains_key(&id));
        drop(receiver);
        assert!(!filter.job_id_multi_filters.contains_key(&id));
    }
}
