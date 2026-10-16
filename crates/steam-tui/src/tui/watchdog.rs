//! Deterministic scheduling for Steam liveness probes. steam-vent cannot tell
//! that a socket died, so the event loop probes when something looks wrong:
//! periodically while idle, and when an install or catalog scan goes quiet.
//! Time is always passed in; nothing here touches the network.

use std::time::{Duration, Instant};

/// Idle probe interval.
pub(super) const IDLE_INTERVAL: Duration = Duration::from_secs(30);
/// Delay of the confirming probe after a first idle failure.
pub(super) const CONFIRM_DELAY: Duration = Duration::from_secs(2);
/// Idle probe failures in a row that declare the connection lost.
pub(super) const IDLE_FAILURES: u32 = 2;
/// Silence from a running download or catalog scan before probing.
pub(super) const STALL_AFTER: Duration = Duration::from_secs(60);
/// A pre-flight probe is skipped when one succeeded this recently.
pub(super) const FRESH_FOR: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProbeKind {
    Idle,
    InstallStall,
    ScanStall,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    Alive,
    /// First idle failure: probe again after `CONFIRM_DELAY` before deciding.
    Retry,
    Dead,
}

/// What the app is doing right now.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Activity {
    /// A connection exists and no reconnect attempt is in flight.
    pub connected: bool,
    pub install_active: bool,
    /// An install plan/destination request is being prepared.
    pub preparing: bool,
    pub paused: bool,
    pub downloading: bool,
    /// A catalog scan is running; `scan_generation` identifies it.
    pub loading: bool,
    pub scan_generation: u64,
}

#[derive(Debug)]
pub(super) struct Watchdog {
    online: bool,
    next_idle: Instant,
    idle_failures: u32,
    probing: bool,
    last_alive: Option<Instant>,
    /// Last sign of life from the running install.
    install_quiet_since: Option<Instant>,
    scan: Option<(u64, Instant)>,
}

impl Watchdog {
    pub fn new(online: bool, now: Instant) -> Self {
        Self {
            online,
            next_idle: now + IDLE_INTERVAL,
            idle_failures: 0,
            probing: false,
            last_alive: None,
            install_quiet_since: None,
            scan: None,
        }
    }

    /// A progress sample (or the install starting) arrived.
    pub fn install_activity(&mut self, now: Instant) {
        self.install_quiet_since = Some(now);
    }

    /// The old connection is gone: forget everything learned about it.
    pub fn reset(&mut self, now: Instant) {
        *self = Self::new(self.online, now);
    }

    /// True when a probe succeeded recently enough to skip a pre-flight probe.
    pub fn recently_alive(&self, now: Instant) -> bool {
        self.last_alive
            .is_some_and(|alive| now.saturating_duration_since(alive) < FRESH_FOR)
    }

    /// The probe to run now, if any. At most one probe is outstanding; the
    /// caller reports its result through `finished`.
    pub fn due(&mut self, now: Instant, state: Activity) -> Option<ProbeKind> {
        // Pausing legitimately idles the connection; the quiet timer restarts.
        if state.paused || !state.install_active {
            self.install_quiet_since = state.install_active.then_some(now);
        } else {
            // Covers an install that outlived a `reset`.
            self.install_quiet_since.get_or_insert(now);
        }
        match (state.loading, self.scan) {
            (false, _) => self.scan = None,
            (true, Some((generation, _))) if generation == state.scan_generation => {}
            (true, _) => self.scan = Some((state.scan_generation, now)),
        }
        if !self.online || self.probing || !state.connected {
            return None;
        }
        let install_stall = (state.install_active && !state.paused && state.downloading)
            .then_some(self.install_quiet_since)
            .flatten()
            .filter(|since| now.saturating_duration_since(*since) >= STALL_AFTER)
            .map(|_| ProbeKind::InstallStall);
        let scan_stall = self
            .scan
            .filter(|(_, since)| now.saturating_duration_since(*since) >= STALL_AFTER)
            .map(|_| ProbeKind::ScanStall);
        // A running scan is covered by its own stall probe.
        let idle = !state.install_active && !state.preparing && !state.loading;
        let idle_probe = (idle && now >= self.next_idle).then_some(ProbeKind::Idle);
        let kind = install_stall.or(scan_stall).or(idle_probe)?;
        self.probing = true;
        Some(kind)
    }

    pub fn finished(&mut self, kind: ProbeKind, alive: bool, now: Instant) -> Outcome {
        self.probing = false;
        if alive {
            self.last_alive = Some(now);
            self.idle_failures = 0;
            self.next_idle = now + IDLE_INTERVAL;
            match kind {
                ProbeKind::Idle => {}
                ProbeKind::InstallStall => self.install_quiet_since = Some(now),
                ProbeKind::ScanStall => {
                    if let Some((_, since)) = &mut self.scan {
                        *since = now;
                    }
                }
            }
            return Outcome::Alive;
        }
        match kind {
            // Already silent for a minute, plus the probe timeout: no second chance.
            ProbeKind::InstallStall | ProbeKind::ScanStall => Outcome::Dead,
            ProbeKind::Idle => {
                self.idle_failures += 1;
                if self.idle_failures >= IDLE_FAILURES {
                    self.idle_failures = 0;
                    self.next_idle = now + IDLE_INTERVAL;
                    Outcome::Dead
                } else {
                    self.next_idle = now + CONFIRM_DELAY;
                    Outcome::Retry
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn idle() -> Activity {
        Activity {
            connected: true,
            ..Activity::default()
        }
    }

    fn downloading() -> Activity {
        Activity {
            connected: true,
            install_active: true,
            downloading: true,
            ..Activity::default()
        }
    }

    #[test]
    fn idle_probe_runs_every_thirty_seconds_and_one_at_a_time() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        assert_eq!(dog.due(at(base, 29), idle()), None);
        assert_eq!(dog.due(at(base, 30), idle()), Some(ProbeKind::Idle));
        assert_eq!(dog.due(at(base, 31), idle()), None, "one probe in flight");
        assert_eq!(
            dog.finished(ProbeKind::Idle, true, at(base, 31)),
            Outcome::Alive
        );
        assert_eq!(dog.due(at(base, 60), idle()), None);
        assert_eq!(dog.due(at(base, 61), idle()), Some(ProbeKind::Idle));
    }

    #[test]
    fn two_idle_failures_in_a_row_declare_the_connection_lost() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        assert_eq!(dog.due(at(base, 30), idle()), Some(ProbeKind::Idle));
        assert_eq!(
            dog.finished(ProbeKind::Idle, false, at(base, 38)),
            Outcome::Retry
        );
        assert_eq!(dog.due(at(base, 39), idle()), None);
        assert_eq!(dog.due(at(base, 40), idle()), Some(ProbeKind::Idle));
        assert_eq!(
            dog.finished(ProbeKind::Idle, false, at(base, 48)),
            Outcome::Dead
        );
        // A success between failures clears the count.
        assert_eq!(dog.due(at(base, 78), idle()), Some(ProbeKind::Idle));
        assert_eq!(
            dog.finished(ProbeKind::Idle, false, at(base, 79)),
            Outcome::Retry
        );
        assert_eq!(dog.due(at(base, 81), idle()), Some(ProbeKind::Idle));
        assert_eq!(
            dog.finished(ProbeKind::Idle, true, at(base, 82)),
            Outcome::Alive
        );
        assert_eq!(dog.due(at(base, 112), idle()), Some(ProbeKind::Idle));
        assert_eq!(
            dog.finished(ProbeKind::Idle, false, at(base, 113)),
            Outcome::Retry
        );
    }

    #[test]
    fn no_idle_probe_while_installing_preparing_offline_or_reconnecting() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        let busy = Activity {
            install_active: true,
            ..idle()
        };
        assert_eq!(dog.due(at(base, 500), busy), None);
        let preparing = Activity {
            preparing: true,
            ..idle()
        };
        assert_eq!(dog.due(at(base, 500), preparing), None);
        let reconnecting = Activity {
            connected: false,
            ..idle()
        };
        assert_eq!(dog.due(at(base, 500), reconnecting), None);
        assert_eq!(dog.due(at(base, 500), idle()), Some(ProbeKind::Idle));
        let mut offline = Watchdog::new(false, base);
        assert_eq!(offline.due(at(base, 500), idle()), None);
    }

    #[test]
    fn a_quiet_download_is_probed_after_sixty_seconds() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        dog.install_activity(base);
        assert_eq!(dog.due(at(base, 59), downloading()), None);
        // Progress keeps the timer fresh.
        dog.install_activity(at(base, 59));
        assert_eq!(dog.due(at(base, 118), downloading()), None);
        assert_eq!(
            dog.due(at(base, 119), downloading()),
            Some(ProbeKind::InstallStall)
        );
        assert_eq!(
            dog.finished(ProbeKind::InstallStall, false, at(base, 127)),
            Outcome::Dead
        );
    }

    #[test]
    fn a_live_probe_during_a_stall_waits_another_minute() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        dog.install_activity(base);
        assert_eq!(
            dog.due(at(base, 60), downloading()),
            Some(ProbeKind::InstallStall)
        );
        assert_eq!(
            dog.finished(ProbeKind::InstallStall, true, at(base, 61)),
            Outcome::Alive
        );
        assert_eq!(dog.due(at(base, 120), downloading()), None);
        assert_eq!(
            dog.due(at(base, 121), downloading()),
            Some(ProbeKind::InstallStall)
        );
    }

    #[test]
    fn pausing_never_triggers_a_probe_and_restarts_the_quiet_timer() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        dog.install_activity(base);
        let paused = Activity {
            paused: true,
            ..downloading()
        };
        for secs in [30, 90, 600] {
            assert_eq!(dog.due(at(base, secs), paused), None);
        }
        // Resumed at t=600: the clock restarts, it does not count the pause.
        assert_eq!(dog.due(at(base, 601), downloading()), None);
        assert_eq!(dog.due(at(base, 659), downloading()), None);
        assert_eq!(
            dog.due(at(base, 661), downloading()),
            Some(ProbeKind::InstallStall)
        );
    }

    #[test]
    fn only_the_downloading_phase_counts_as_a_stall() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        dog.install_activity(base);
        let verifying = Activity {
            downloading: false,
            ..downloading()
        };
        assert_eq!(dog.due(at(base, 300), verifying), None);
    }

    #[test]
    fn a_scan_without_a_result_is_probed_after_sixty_seconds_per_generation() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        let scanning = Activity {
            loading: true,
            scan_generation: 3,
            ..idle()
        };
        assert_eq!(dog.due(at(base, 10), scanning), None);
        assert_eq!(dog.due(at(base, 70), scanning), Some(ProbeKind::ScanStall));
        assert_eq!(
            dog.finished(ProbeKind::ScanStall, true, at(base, 71)),
            Outcome::Alive
        );
        assert_eq!(dog.due(at(base, 130), scanning), None);
        assert_eq!(dog.due(at(base, 131), scanning), Some(ProbeKind::ScanStall));
        assert_eq!(
            dog.finished(ProbeKind::ScanStall, false, at(base, 139)),
            Outcome::Dead
        );
        // A new scan (new generation) restarts the timer; a finished one clears it.
        let next = Activity {
            scan_generation: 4,
            ..scanning
        };
        assert_eq!(dog.due(at(base, 140), next), None);
        assert_eq!(dog.due(at(base, 199), next), None);
        assert_eq!(dog.due(at(base, 200), next), Some(ProbeKind::ScanStall));
    }

    #[test]
    fn recent_success_skips_preflight_and_reset_forgets_it() {
        let base = Instant::now();
        let mut dog = Watchdog::new(true, base);
        assert!(!dog.recently_alive(base));
        dog.due(at(base, 30), idle());
        dog.finished(ProbeKind::Idle, true, at(base, 31));
        assert!(dog.recently_alive(at(base, 40)));
        assert!(!dog.recently_alive(at(base, 41)));
        dog.reset(at(base, 45));
        assert!(!dog.recently_alive(at(base, 45)));
        assert_eq!(dog.due(at(base, 74), idle()), None);
        assert_eq!(dog.due(at(base, 75), idle()), Some(ProbeKind::Idle));
    }
}
