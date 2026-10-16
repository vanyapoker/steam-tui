//! Bounded local futures: Connection is not Send. Drain admitted work before
//! returning an error so blocking writes finish while the install locks live.
use super::{PauseGate, Result, check_cancel, failure, wait_while_paused};
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio_util::sync::CancellationToken;

pub(super) const CONCURRENCY: usize = 8;
// Encrypted response, decrypted compressed bytes and output. Decoder working
// memory is separately bounded by content's per-decoder 64 MiB window limit.
pub(super) const BUFFER_BUDGET: usize = 64 * 1024 * 1024;

pub(super) async fn run<J, O, F, Fut>(
    jobs: impl IntoIterator<Item = (J, usize)>,
    cancel: &CancellationToken,
    pause: &PauseGate,
    work: F,
    mut completed: impl FnMut(O),
) -> Result<()>
where
    F: Fn(J) -> Fut,
    Fut: Future<Output = Result<O>>,
{
    let mut jobs = jobs.into_iter().peekable();
    let mut active = FuturesUnordered::new();
    let mut reserved = 0usize;
    let mut error = None;
    loop {
        if error.is_none() {
            error = check_cancel(cancel).err();
        }
        while error.is_none() && !pause.is_paused() && active.len() < CONCURRENCY {
            let Some((_, charge)) = jobs.peek() else {
                break;
            };
            let charge = (*charge).max(1);
            if charge > BUFFER_BUDGET {
                error = Some(failure("Download chunk exceeds pipeline buffer budget"));
                break;
            }
            if reserved + charge > BUFFER_BUDGET {
                break;
            }
            let (job, _) = jobs.next().unwrap();
            reserved += charge;
            let future = work(job);
            active.push(async move { (charge, future.await) });
        }
        if active.is_empty() {
            if let Some(error) = error {
                return Err(error);
            }
            if jobs.peek().is_none() {
                return Ok(());
            }
            wait_while_paused(pause, cancel).await?;
            continue;
        }
        // Continue polling admitted transfers even while paused or draining on
        // error. Work checks cancellation; dropping futures could detach writes.
        let (charge, result) = active.next().await.unwrap();
        reserved -= charge;
        match result {
            Ok(value) => completed(value),
            Err(failure) => {
                error.get_or_insert(failure);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn transfers_overlap_and_each_job_finishes_once() {
        let gate = PauseGate::default();
        let token = CancellationToken::new();
        let barrier = Barrier::new(CONCURRENCY);
        let mut done = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                (0..32).map(|i| (i, 1)),
                &token,
                &gate,
                |i| {
                    let barrier = &barrier;
                    async move {
                        barrier.wait().await;
                        Ok(i)
                    }
                },
                |i| done.push(i),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        done.sort_unstable();
        assert_eq!(done, (0..32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn buffers_are_reserved_until_work_including_writes_finishes() {
        let active = Cell::new(0);
        let peak = Cell::new(0);
        let charge = BUFFER_BUDGET / 3;
        run(
            (0..20).map(|i| (i, charge)),
            &CancellationToken::new(),
            &PauseGate::default(),
            |i| {
                let (active, peak) = (&active, &peak);
                async move {
                    active.set(active.get() + 1);
                    peak.set(peak.get().max(active.get()));
                    tokio::task::yield_now().await;
                    active.set(active.get() - 1);
                    Ok(i)
                }
            },
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(peak.get(), 3);
        assert_eq!(active.get(), 0);
    }

    #[tokio::test]
    async fn pause_drains_admitted_work_and_stops_new_admission() {
        let gate = PauseGate::default();
        let token = CancellationToken::new();
        let admitted = Cell::new(0);
        let finished = Cell::new(0);
        let download = run(
            (0..16).map(|i| (i, 1)),
            &token,
            &gate,
            |i| {
                admitted.set(admitted.get() + 1);
                let gate = &gate;
                async move {
                    if i == 0 {
                        gate.set(true);
                    }
                    tokio::task::yield_now().await;
                    Ok(i)
                }
            },
            |_| finished.set(finished.get() + 1),
        );
        let resume = async {
            while finished.get() != CONCURRENCY {
                tokio::task::yield_now().await;
            }
            // Give the scheduler a chance to admit more if it ignores pause.
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert_eq!(admitted.get(), CONCURRENCY);
            gate.set(false);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            let (result, ()) = tokio::join!(download, resume);
            result.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(finished.get(), 16);
    }

    #[tokio::test]
    async fn failure_drains_started_writes_before_returning_and_admits_no_more() {
        let admitted = Cell::new(0);
        let written = Cell::new(0);
        let token = CancellationToken::new();
        let gate = PauseGate::default();
        let error = run(
            (0..32).map(|i| (i, 1)),
            &token,
            &gate,
            |i| {
                admitted.set(admitted.get() + 1);
                let written = &written;
                async move {
                    if i == 0 {
                        return Err(failure("injected fetch failure"));
                    }
                    // Simulates an admitted write that must be joined on error.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    written.set(written.get() + 1);
                    Ok(i)
                }
            },
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "injected fetch failure");
        assert_eq!(admitted.get(), CONCURRENCY);
        assert_eq!(written.get(), CONCURRENCY - 1);
    }

    #[tokio::test]
    async fn cancellation_drains_admitted_work_before_releasing_the_install() {
        let token = CancellationToken::new();
        let gate = PauseGate::default();
        let admitted = Cell::new(0);
        let finished = Cell::new(0);
        let error = run(
            (0..32).map(|i| (i, 1)),
            &token,
            &gate,
            |i| {
                admitted.set(admitted.get() + 1);
                let token = &token;
                async move {
                    if i == 0 {
                        token.cancel();
                    }
                    tokio::task::yield_now().await;
                    Ok(i)
                }
            },
            |_| finished.set(finished.get() + 1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(admitted.get(), CONCURRENCY);
        assert_eq!(finished.get(), CONCURRENCY);
    }
}
