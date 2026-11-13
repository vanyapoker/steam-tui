//! Two-stage install pipeline: group N+1 is scanned (disk bound) while group N
//! downloads (network bound). Groups finish strictly in order. A running scan
//! is always awaited before `run` returns so blocking work ends while the
//! install locks are still held.
use super::{PauseGate, Result, check_cancel, failure, wait_while_paused};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(super) trait Stages {
    type Group;
    type Scanned: Send + 'static;
    /// Next unscanned group; `Ok(None)` when all files are listed.
    fn next(&mut self) -> Result<Option<Self::Group>>;
    /// Starts the blocking scan at once and returns its handle.
    fn scan(&self, group: Self::Group) -> JoinHandle<Result<Self::Scanned>>;
    /// Download and bookkeeping for one scanned group.
    async fn finish(&mut self, scanned: Self::Scanned) -> Result<()>;
}

async fn join<T>(handle: JoinHandle<Result<T>>) -> Result<T> {
    handle
        .await
        .map_err(|e| failure(format!("Installer filesystem worker failed: {e}")))?
}

pub(super) async fn run<S: Stages>(
    stages: &mut S,
    pause: &PauseGate,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut pending: Option<JoinHandle<Result<S::Scanned>>> = None;
    let result = async {
        while let Some(group) = stages.next()? {
            wait_while_paused(pause, cancel).await?;
            check_cancel(cancel)?;
            let started = stages.scan(group);
            if let Some(previous) = pending.replace(started) {
                stages.finish(join(previous).await?).await?;
            }
        }
        if let Some(previous) = pending.take() {
            stages.finish(join(previous).await?).await?;
        }
        Ok(())
    }
    .await;
    if let Some(handle) = pending.take() {
        let _ = handle.await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    struct Fake {
        groups: usize,
        next: usize,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
        started: Arc<Vec<AtomicBool>>,
        scans: Arc<AtomicUsize>,
        /// Group whose finish fails.
        fail_finish: Option<usize>,
        /// Group whose scan fails.
        fail_scan: Option<usize>,
        /// Group whose finish waits for the next group's scan to start.
        wait_overlap: Option<usize>,
        slow_done: Arc<AtomicBool>,
    }

    impl Fake {
        fn new(groups: usize) -> Self {
            Self {
                groups,
                next: 0,
                log: Default::default(),
                started: Arc::new((0..groups).map(|_| AtomicBool::new(false)).collect()),
                scans: Default::default(),
                fail_finish: None,
                fail_scan: None,
                wait_overlap: None,
                slow_done: Default::default(),
            }
        }
    }

    impl Stages for Fake {
        type Group = usize;
        type Scanned = usize;
        fn next(&mut self) -> Result<Option<usize>> {
            if self.next == self.groups {
                return Ok(None);
            }
            self.next += 1;
            Ok(Some(self.next - 1))
        }
        fn scan(&self, group: usize) -> JoinHandle<Result<usize>> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            self.started[group].store(true, Ordering::SeqCst);
            let fail = self.fail_scan == Some(group);
            let slow = self.fail_finish.is_some() && group == 1;
            let done = Arc::clone(&self.slow_done);
            tokio::task::spawn_blocking(move || {
                if slow {
                    std::thread::sleep(Duration::from_millis(100));
                    done.store(true, Ordering::SeqCst);
                }
                if fail {
                    Err(failure("scan failed"))
                } else {
                    Ok(group)
                }
            })
        }
        async fn finish(&mut self, scanned: usize) -> Result<()> {
            if self.wait_overlap == Some(scanned) {
                while !self.started[scanned + 1].load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            self.log.lock().push(format!("finish {scanned}"));
            if self.fail_finish == Some(scanned) {
                return Err(failure("finish failed"));
            }
            Ok(())
        }
    }

    async fn drive(fake: &mut Fake, cancel: &CancellationToken) -> Result<()> {
        let pause = PauseGate::default();
        tokio::time::timeout(Duration::from_secs(5), run(fake, &pause, cancel))
            .await
            .expect("pipeline hung")
    }

    #[tokio::test]
    async fn scans_next_group_while_previous_finishes() {
        let mut fake = Fake::new(3);
        fake.wait_overlap = Some(0);
        drive(&mut fake, &CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn groups_finish_in_order_and_each_once() {
        let mut fake = Fake::new(3);
        drive(&mut fake, &CancellationToken::new()).await.unwrap();
        assert_eq!(*fake.log.lock(), ["finish 0", "finish 1", "finish 2"]);
    }

    #[tokio::test]
    async fn finish_error_still_drains_the_running_scan() {
        let mut fake = Fake::new(3);
        fake.fail_finish = Some(0);
        let err = drive(&mut fake, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "finish failed");
        assert!(fake.slow_done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn scan_error_stops_without_calling_finish_for_later_groups() {
        let mut fake = Fake::new(4);
        fake.fail_scan = Some(1);
        let err = drive(&mut fake, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "scan failed");
        assert_eq!(*fake.log.lock(), ["finish 0"]);
    }

    #[tokio::test]
    async fn cancelled_before_scan_starts_no_scan() {
        let mut fake = Fake::new(2);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(drive(&mut fake, &cancel).await.is_err());
        assert_eq!(fake.scans.load(Ordering::SeqCst), 0);
    }
}
