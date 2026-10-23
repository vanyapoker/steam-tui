//! Account ownership queries share the running app's authenticated session.
//! Public metadata and installed files never confer ownership.
use super::*;
use std::collections::BTreeMap;

const MAX_APPS: usize = 128;
/// Ownership only changes with the license list, which clears the cache
/// (`query`); the age limit bounds anything that goes unnoticed.
const TTL: Duration = Duration::from_secs(600);

#[derive(Default)]
pub(super) struct Service {
    entries: BTreeMap<u32, (Instant, bool)>,
    /// License list generation the entries were answered under.
    licenses: u64,
}

impl Service {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    fn cached(&mut self, app_id: u32, now: Instant) -> Option<bool> {
        // Drop expired grants before a refresh; a failed refresh cannot leave
        // a successful old result available to a subsequent call.
        self.entries
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < TTL);
        self.entries.get(&app_id).map(|(_, owned)| *owned)
    }

    fn record(&mut self, app_id: u32, owned: bool, now: Instant) {
        if self.entries.len() >= MAX_APPS
            && !self.entries.contains_key(&app_id)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(id, _)| *id)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(app_id, (now, owned));
    }

    /// Ownership for the game's API calls (BIsSubscribedApp and friends): the
    /// session's license list from Steam, which Valve's client answers from
    /// too, proves ownership at once; an absent app is asked with a CM ticket.
    /// Answers hold until Steam sends a new license list (`licenses` changes).
    pub(super) async fn answer(
        &mut self,
        connection: &Connection,
        running_app: u32,
        target_app: u32,
        events: &Option<EventSender>,
        licenses: u64,
    ) -> Response {
        if licenses != self.licenses {
            self.clear();
            self.licenses = licenses;
        }
        if target_app != 0
            && self.cached(target_app, Instant::now()).is_none()
            && crate::library::cached_ownership(connection)
                .is_some_and(|(apps, _)| apps.contains(&target_app))
        {
            self.record(target_app, true, Instant::now());
        }
        self.query(connection, running_app, target_app, events)
            .await
    }

    /// Ownership from a CM ownership ticket (or this service's cache).
    pub(super) async fn query(
        &mut self,
        connection: &Connection,
        running_app: u32,
        target_app: u32,
        events: &Option<EventSender>,
    ) -> Response {
        if target_app == 0 {
            return Response::Error {
                message: "Ownership requires a nonzero app ID".into(),
            };
        }
        if let Some(owned) = self.cached(target_app, Instant::now()) {
            return Response::Ownership { owned };
        }
        let request = CMsgClientGetAppOwnershipTicket {
            app_id: Some(target_app),
            ..Default::default()
        };
        let response = timeout(
            Duration::from_secs(5),
            connection.job::<_, CMsgClientGetAppOwnershipTicketResponse>(request),
        )
        .await;
        let result = match response {
            Ok(Ok(response)) => owns_app(&response, target_app),
            _ => Err("App ownership request failed or timed out"),
        };
        match result {
            Ok(owned) => {
                self.record(target_app, owned, Instant::now());
                events::emit(
                    events,
                    running_app,
                    Direction::ToSteam,
                    "CM",
                    "ClientGetAppOwnershipTicket",
                    format!("app {target_app}: owned={owned} (ticket not forwarded)"),
                );
                Response::Ownership { owned }
            }
            Err(message) => Response::Error {
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_separate_for_each_app_and_expires_and_clears_on_disconnect() {
        let now = Instant::now();
        let mut service = Service::default();
        service.record(10, true, now);
        service.record(11, false, now);
        assert_eq!(service.cached(10, now), Some(true));
        assert_eq!(service.cached(11, now), Some(false));
        assert_eq!(service.cached(12, now), None);
        assert_eq!(service.cached(10, now + TTL), None);
        assert!(service.entries.is_empty());
        service.record(10, true, now + TTL);
        service.clear();
        assert_eq!(service.cached(10, now + TTL), None);
    }

    #[test]
    fn ownership_cache_bounds_distinct_targets_and_evicts_oldest() {
        let now = Instant::now();
        let mut service = Service::default();
        for id in 1..=MAX_APPS as u32 {
            service.record(id, true, now + Duration::from_millis(id.into()));
        }
        service.record(1000, false, now + Duration::from_secs(1));
        assert_eq!(service.entries.len(), MAX_APPS);
        assert!(!service.entries.contains_key(&1));
        assert_eq!(
            service.cached(1000, now + Duration::from_secs(1)),
            Some(false)
        );
        assert_eq!(service.cached(2, now + Duration::from_secs(1)), Some(true));
    }
}
