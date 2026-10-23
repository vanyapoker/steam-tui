//! `LicensesUpdated_t`: posted whenever Steam sends a new license list, as
//! Valve's client does when purchases, refunds or free licenses change.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::StreamExt;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_clientserver::CMsgClientLicenseList;

use super::events::{Direction, EventSender, emit};

// k_iSteamUserCallbacks (100) + 25.
const LICENSES_UPDATED: i32 = 125;

pub struct Licenses {
    pending: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    worker: tokio::task::JoinHandle<()>,
}

impl Licenses {
    pub fn start(connection: &Connection, app_id: u32, events: Option<EventSender>) -> Self {
        let pending = Arc::new(AtomicBool::new(false));
        let flag = pending.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let changes = generation.clone();
        let mut lists = Box::pin(connection.on::<CMsgClientLicenseList>());
        let worker = tokio::spawn(async move {
            while let Some(list) = lists.next().await {
                let Ok(list) = list else { continue };
                emit(
                    &events,
                    app_id,
                    Direction::FromSteam,
                    "Licenses",
                    "LicenseList",
                    format!("{} licenses", list.licenses.len()),
                );
                // Several lists between polls are one change for the game.
                changes.fetch_add(1, Ordering::Relaxed);
                flag.store(true, Ordering::Relaxed);
            }
        });
        Self {
            pending,
            generation,
            worker,
        }
    }

    /// Changes with every license list Steam sends.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn poll(&self) -> Vec<(i32, Vec<u8>)> {
        if !self.pending.swap(false, Ordering::Relaxed) {
            return Vec::new();
        }
        // LicensesUpdated_t has no fields; C++ gives it one byte.
        vec![(LICENSES_UPDATED, vec![0])]
    }
}

impl Drop for Licenses {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
