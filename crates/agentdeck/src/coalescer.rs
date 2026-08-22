//! Fixed-window reconciliation coalescing.
//!
//! Herdr events and the independent safety poll are both invalidation hints.
//! This module deliberately knows nothing about either producer or about deck
//! state: it only serializes authoritative reconciliation work.

use std::{future::Future, time::Duration};

use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

pub const RECONCILIATION_WINDOW: Duration = Duration::from_millis(30);

/// Run fixed, leading-edge-anchored reconciliation windows until cancellation
/// or until every sender is dropped.
///
/// The first signal fixes a deadline 30 ms later. Later arrivals never move
/// that deadline. Work is awaited inline, so reconciliations cannot overlap.
/// Signals received while work is in flight collapse into one immediate dirty
/// follow-up.
pub async fn run_reconciliation_coalescer<F, Fut>(
    mut invalidations: mpsc::Receiver<()>,
    cancellation: CancellationToken,
    mut reconcile: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let first = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            signal = invalidations.recv() => signal,
        };
        if first.is_none() {
            return;
        }

        let deadline = Instant::now() + RECONCILIATION_WINDOW;
        let mut channel_closed = false;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                _ = sleep_until(deadline) => break,
                signal = invalidations.recv(), if !channel_closed => {
                    if signal.is_none() {
                        channel_closed = true;
                    }
                }
            }
        }

        // Anything queued when the fixed deadline becomes ready belongs to
        // this window. Drain only the length snapshot: a concurrent producer
        // cannot turn this into an unbounded synchronous loop.
        if drain_snapshot(&mut invalidations, &cancellation).cancelled {
            return;
        }
        channel_closed |= invalidations.is_closed();

        loop {
            if cancellation.is_cancelled() {
                return;
            }
            let mut dirty = false;
            let work = reconcile();
            tokio::pin!(work);
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    () = &mut work => break,
                    signal = invalidations.recv(), if !channel_closed => {
                        match signal {
                            Some(()) => dirty = true,
                            None => channel_closed = true,
                        }
                    }
                }
            }

            // Close the completion race: anything already queued belongs to
            // this in-flight generation and becomes the same single follow-up.
            // The snapshot bound prevents a producer from starving completed
            // work or cancellation by continually refilling the channel.
            let drained = drain_snapshot(&mut invalidations, &cancellation);
            if drained.cancelled {
                return;
            }
            dirty |= drained.received;
            if invalidations.is_closed() {
                channel_closed = true;
            }

            if !dirty {
                break;
            }
        }

        if channel_closed {
            return;
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DrainResult {
    received: bool,
    cancelled: bool,
}

fn drain_snapshot(
    invalidations: &mut mpsc::Receiver<()>,
    cancellation: &CancellationToken,
) -> DrainResult {
    let queued = invalidations.len();
    let mut result = DrainResult::default();
    for _ in 0..queued {
        if cancellation.is_cancelled() {
            result.cancelled = true;
            break;
        }
        match invalidations.try_recv() {
            Ok(()) => result.received = true,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::{RECONCILIATION_WINDOW, run_reconciliation_coalescer};

    fn send_ok(result: Result<(), mpsc::error::SendError<()>>) {
        result.unwrap_or_else(|error| panic!("send failed: {error}"));
    }

    #[tokio::test(start_paused = true)]
    async fn window_is_anchored_to_first_arrival_without_trailing_drift() {
        let (tx, rx) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let task_calls = Arc::clone(&calls);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                task_calls.fetch_add(1, Ordering::SeqCst);
                async {}
            })
            .await;
        });

        send_ok(tx.send(()).await);
        tokio::task::yield_now().await;
        tokio::time::advance(RECONCILIATION_WINDOW - Duration::from_millis(1)).await;
        send_ok(tx.send(()).await);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cancellation.cancel();
        task.await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_arrivals_make_exactly_one_non_overlapping_follow_up() {
        let (tx, rx) = mpsc::channel(16);
        let cancellation = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());

        let task_calls = Arc::clone(&calls);
        let task_active = Arc::clone(&active);
        let task_max = Arc::clone(&max_active);
        let task_release = Arc::clone(&release);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                let call = task_calls.fetch_add(1, Ordering::SeqCst) + 1;
                let now_active = task_active.fetch_add(1, Ordering::SeqCst) + 1;
                task_max.fetch_max(now_active, Ordering::SeqCst);
                let active = Arc::clone(&task_active);
                let release = Arc::clone(&task_release);
                async move {
                    if call == 1 {
                        release.notified().await;
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            })
            .await;
        });

        send_ok(tx.send(()).await);
        tokio::task::yield_now().await;
        tokio::time::advance(RECONCILIATION_WINDOW).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        for _ in 0..6 {
            send_ok(tx.send(()).await);
        }
        tokio::task::yield_now().await;
        release.notify_one();
        tokio::task::yield_now().await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        cancellation.cancel();
        task.await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_event_race_cannot_miss_the_final_generation() {
        let (tx, rx) = mpsc::channel(2);
        let cancellation = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let task_calls = Arc::clone(&calls);
        let task_release = Arc::clone(&release);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                let call = task_calls.fetch_add(1, Ordering::SeqCst) + 1;
                let release = Arc::clone(&task_release);
                async move {
                    if call == 1 {
                        release.notified().await;
                    }
                }
            })
            .await;
        });

        send_ok(tx.send(()).await);
        tokio::task::yield_now().await;
        tokio::time::advance(RECONCILIATION_WINDOW).await;
        tokio::task::yield_now().await;
        send_ok(tx.send(()).await);
        release.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        cancellation.cancel();
        task.await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test]
    async fn cancellation_drops_an_in_flight_reconciliation_cleanly() {
        let (tx, rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || async {
                std::future::pending::<()>().await;
            })
            .await;
        });
        send_ok(tx.send(()).await);
        tokio::time::sleep(RECONCILIATION_WINDOW + Duration::from_millis(5)).await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap_or_else(|_| panic!("coalescer did not cancel"))
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sustained_producer_cannot_starve_the_fixed_deadline() {
        let (tx, rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let fired = Arc::new(Notify::new());
        let task_fired = Arc::clone(&fired);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                let fired = Arc::clone(&task_fired);
                async move { fired.notify_one() }
            })
            .await;
        });
        let producer = tokio::spawn(async move { while tx.send(()).await.is_ok() {} });

        tokio::time::timeout(Duration::from_millis(500), fired.notified())
            .await
            .unwrap_or_else(|_| panic!("ready 30 ms deadline was starved"));
        cancellation.cancel();
        producer.abort();
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap_or_else(|_| panic!("coalescer did not stop"))
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sustained_producer_cannot_starve_completed_work() {
        let (tx, rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let started = Arc::new(Notify::new());
        let followed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let task_started = Arc::clone(&started);
        let task_followed = Arc::clone(&followed);
        let task_release = Arc::clone(&release);
        let task_calls = Arc::clone(&calls);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                let call = task_calls.fetch_add(1, Ordering::SeqCst) + 1;
                let started = Arc::clone(&task_started);
                let followed = Arc::clone(&task_followed);
                let release = Arc::clone(&task_release);
                async move {
                    if call == 1 {
                        started.notify_one();
                        release.notified().await;
                    } else if call == 2 {
                        followed.notify_one();
                    }
                }
            })
            .await;
        });
        let producer = tokio::spawn(async move { while tx.send(()).await.is_ok() {} });

        tokio::time::timeout(Duration::from_millis(500), started.notified())
            .await
            .unwrap_or_else(|_| panic!("first reconciliation did not start"));
        release.notify_one();
        tokio::time::timeout(Duration::from_millis(500), followed.notified())
            .await
            .unwrap_or_else(|_| panic!("completed reconciliation was starved"));
        assert!(calls.load(Ordering::SeqCst) >= 2);

        cancellation.cancel();
        producer.abort();
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap_or_else(|_| panic!("coalescer did not stop"))
            .unwrap_or_else(|error| panic!("task failed: {error}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sustained_producer_cannot_starve_cancellation() {
        let (tx, rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let fired = Arc::new(Notify::new());
        let task_fired = Arc::clone(&fired);
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_reconciliation_coalescer(rx, task_cancel, || {
                let fired = Arc::clone(&task_fired);
                async move { fired.notify_one() }
            })
            .await;
        });
        let producer = tokio::spawn(async move { while tx.send(()).await.is_ok() {} });

        tokio::time::timeout(Duration::from_millis(500), fired.notified())
            .await
            .unwrap_or_else(|_| panic!("reconciliation did not run"));
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap_or_else(|_| panic!("cancellation was starved"))
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        producer.abort();
    }
}
