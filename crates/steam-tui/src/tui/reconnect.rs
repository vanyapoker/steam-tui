//! Pure decision logic for replacing a dead Steam connection. The event loop
//! performs the I/O; everything here is deterministic and network-free.

use std::time::Duration;

/// Consecutive failed attempts after which nothing retries automatically.
/// An explicit `r` starts a fresh budget.
pub(super) const MAX_AUTO_ATTEMPTS: u32 = 5;

/// Wait before the next attempt: 2 s, 5 s, 15 s, then 30 s.
pub(super) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 => 2,
        1 => 5,
        2 => 15,
        _ => 30,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Trigger {
    /// An install, install-plan or catalog job failed in a way that suggests a dead session.
    JobFailed,
    /// Both workers ended, or liveness probes failed.
    ConnectionLost,
    /// The user pressed `r`.
    Manual,
    /// First sign-in after the terminal is up.
    Startup,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Decision {
    /// Start an attempt after this delay.
    Start(Duration),
    /// An install is running; the request is kept until it ends.
    Deferred,
    /// The automatic attempt budget is spent; only `r` retries.
    Exhausted,
    /// Offline, or an attempt is already in flight.
    Ignored,
}

#[derive(Debug)]
pub(super) struct Reconnect {
    online: bool,
    failures: u32,
    in_flight: bool,
    deferred: Option<Trigger>,
}

impl Reconnect {
    /// `online` is true only with a remembered account outside `--offline`.
    pub fn new(online: bool) -> Self {
        Self {
            online,
            failures: 0,
            in_flight: false,
            deferred: None,
        }
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub fn request(&mut self, trigger: Trigger, install_active: bool) -> Decision {
        if !self.online || self.in_flight {
            return Decision::Ignored;
        }
        if install_active {
            self.deferred.get_or_insert(trigger);
            return Decision::Deferred;
        }
        self.deferred = None;
        if trigger == Trigger::Manual {
            self.failures = 0;
        } else if self.failures >= MAX_AUTO_ATTEMPTS {
            return Decision::Exhausted;
        }
        let delay = match trigger {
            Trigger::Manual | Trigger::Startup => Duration::ZERO,
            Trigger::JobFailed if self.failures == 0 => Duration::ZERO,
            _ => backoff(self.failures),
        };
        self.in_flight = true;
        Decision::Start(delay)
    }

    /// Re-issues a deferred request once no install is running.
    pub fn release(&mut self, install_active: bool) -> Option<(Trigger, Decision)> {
        if install_active {
            return None;
        }
        let trigger = self.deferred.take()?;
        Some((trigger, self.request(trigger, false)))
    }

    pub fn succeeded(&mut self) {
        self.in_flight = false;
        self.failures = 0;
    }

    /// Records a failed attempt; returns the delay of the automatic retry, if any.
    /// A permanent failure (credentials need `steam-tui login`) never retries.
    pub fn failed(&mut self, permanent: bool) -> Option<Duration> {
        self.in_flight = false;
        self.failures = if permanent {
            MAX_AUTO_ATTEMPTS
        } else {
            self.failures.saturating_add(1)
        };
        (self.failures < MAX_AUTO_ATTEMPTS).then(|| {
            self.in_flight = true;
            backoff(self.failures)
        })
    }
}

/// True when a failure message looks like a dead or unreachable Steam session
/// rather than a local problem (disk, permissions, validation).
pub(super) fn is_connection_error(message: &str) -> bool {
    let text = message.to_lowercase();
    [
        "timed out",
        "timeout",
        "unexpected end of stream",
        "request failed",
        "connection",
        "network",
        "disconnect",
        "stream ended",
        "stream failed",
        "logged off",
        "broken pipe",
        "reset by peer",
        "eof",
        "cdn",
    ]
    .iter()
    .any(|token| text.contains(token))
}

/// Reconnect failures that retrying cannot fix: the saved login must be replaced.
pub(super) fn is_permanent_failure(message: &str) -> bool {
    let text = message.to_lowercase();
    text.contains("steam-tui login") || text.contains("no account")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn backoff_sequence_caps_at_thirty_seconds() {
        let delays: Vec<_> = (0..6).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(delays, [2, 5, 15, 30, 30, 30]);
    }

    #[test]
    fn startup_sign_in_begins_immediately_and_retries_with_backoff() {
        let mut reconnect = Reconnect::new(true);
        assert_eq!(
            reconnect.request(Trigger::Startup, false),
            Decision::Start(Duration::ZERO)
        );
        assert_eq!(reconnect.failed(false), Some(backoff(1)));
        let mut offline = Reconnect::new(false);
        assert_eq!(offline.request(Trigger::Startup, false), Decision::Ignored);
    }

    #[test]
    fn offline_never_reconnects() {
        let mut state = Reconnect::new(false);
        for trigger in [Trigger::JobFailed, Trigger::ConnectionLost, Trigger::Manual] {
            assert_eq!(state.request(trigger, false), Decision::Ignored);
        }
    }

    #[test]
    fn job_failures_start_at_once_and_lost_workers_wait_for_backoff() {
        let mut state = Reconnect::new(true);
        assert_eq!(
            state.request(Trigger::JobFailed, false),
            Decision::Start(Duration::ZERO)
        );
        let mut state = Reconnect::new(true);
        assert_eq!(
            state.request(Trigger::ConnectionLost, false),
            Decision::Start(2 * SECOND)
        );
        let mut state = Reconnect::new(true);
        assert_eq!(
            state.request(Trigger::Manual, false),
            Decision::Start(Duration::ZERO)
        );
    }

    #[test]
    fn only_one_attempt_is_in_flight() {
        let mut state = Reconnect::new(true);
        assert!(matches!(
            state.request(Trigger::JobFailed, false),
            Decision::Start(_)
        ));
        for trigger in [Trigger::JobFailed, Trigger::ConnectionLost, Trigger::Manual] {
            assert_eq!(state.request(trigger, false), Decision::Ignored);
        }
        state.succeeded();
        assert!(matches!(
            state.request(Trigger::ConnectionLost, false),
            Decision::Start(_)
        ));
    }

    #[test]
    fn an_active_install_defers_and_the_request_is_released_after_it_ends() {
        let mut state = Reconnect::new(true);
        assert_eq!(
            state.request(Trigger::ConnectionLost, true),
            Decision::Deferred
        );
        assert_eq!(state.request(Trigger::Manual, true), Decision::Deferred);
        assert_eq!(state.release(true), None);
        assert_eq!(
            state.release(false),
            Some((Trigger::ConnectionLost, Decision::Start(2 * SECOND)))
        );
        assert_eq!(state.release(false), None);
    }

    #[test]
    fn failures_retry_with_backoff_until_the_cap_then_stop() {
        let mut state = Reconnect::new(true);
        assert!(matches!(
            state.request(Trigger::ConnectionLost, false),
            Decision::Start(_)
        ));
        let mut delays = Vec::new();
        while let Some(delay) = state.failed(false) {
            delays.push(delay.as_secs());
        }
        assert_eq!(delays, [5, 15, 30, 30]);
        // Budget spent: automatic triggers stop, `r` starts over.
        assert_eq!(
            state.request(Trigger::ConnectionLost, false),
            Decision::Exhausted
        );
        assert_eq!(
            state.request(Trigger::JobFailed, false),
            Decision::Exhausted
        );
        assert_eq!(
            state.request(Trigger::Manual, false),
            Decision::Start(Duration::ZERO)
        );
        assert_eq!(state.failed(false), Some(5 * SECOND));
    }

    #[test]
    fn success_restores_the_full_budget() {
        let mut state = Reconnect::new(true);
        state.request(Trigger::JobFailed, false);
        assert!(state.failed(false).is_some());
        state.succeeded();
        assert_eq!(
            state.request(Trigger::ConnectionLost, false),
            Decision::Start(2 * SECOND)
        );
    }

    #[test]
    fn permanent_failures_never_retry() {
        let mut state = Reconnect::new(true);
        state.request(Trigger::JobFailed, false);
        assert_eq!(state.failed(true), None);
        assert_eq!(
            state.request(Trigger::JobFailed, false),
            Decision::Exhausted
        );
    }

    #[test]
    fn classifies_connection_errors_and_permanent_failures() {
        for message in [
            "Steam CDN directory request failed: error sending request",
            "Steam content request timed out",
            "Response timed out",
            "Unexpected end of stream",
            "Steam friend relationship stream failed or fell behind",
            "Steam logged off before loading friends",
        ] {
            assert!(is_connection_error(message), "{message}");
        }
        for message in [
            "Installation cancelled; verified staging data is retained for resume",
            "Not enough free space in /games",
            "Permission denied",
            "No destination available",
        ] {
            assert!(!is_connection_error(message), "{message}");
        }
        assert!(is_permanent_failure(
            "Saved refresh token has expired; run steam-tui login again"
        ));
        assert!(is_permanent_failure(
            "No saved refresh token for this account; run steam-tui login first"
        ));
        assert!(!is_permanent_failure(
            "Steam server discovery failed; check connectivity"
        ));
    }
}
