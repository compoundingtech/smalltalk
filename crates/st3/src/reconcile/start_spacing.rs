//! Background pass admission: current rate and last actual start survive supervision restarts.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

pub(super) struct StartSpacing {
    state: Mutex<State>,
    changed: Notify,
}

/// Dropping a run-loop attempt while spawn_blocking is queued cancels its admission.
pub(super) struct QueuedAdmission(pub(super) Arc<AtomicBool>);
impl Default for QueuedAdmission {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}
impl Drop for QueuedAdmission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct State {
    last: Option<Instant>,
    per_minute: u32,
}

impl Default for StartSpacing {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                last: None,
                per_minute: 30,
            }),
            changed: Notify::new(),
        }
    }
}

impl State {
    fn next(&self) -> Option<Instant> {
        // Round up, so fractional nanoseconds cannot permit more than the configured cap.
        self.last.map(|last| {
            last + Duration::from_nanos(60_000_000_000_u64.div_ceil(u64::from(self.per_minute)))
        })
    }
}

impl StartSpacing {
    pub(super) fn set(&self, per_minute: u32) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=600).contains(&per_minute),
            "reconcile.max_passes_per_minute must be between 1 and 600"
        );
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .per_minute = per_minute;
        self.changed.notify_one();
        Ok(())
    }

    /// Called at blocking-work entry, before the pass clock or any database loan. A config
    /// reduction while spawn_blocking was queued must defer admission rather than begin early.
    #[cfg(test)]
    fn try_start(&self) -> bool {
        self.try_start_if(&AtomicBool::new(true))
    }

    pub(super) fn try_start_if(&self, active: &AtomicBool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !active.load(Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        if state.next().is_some_and(|next| next > now) {
            return false;
        }
        state.last = Some(now);
        true
    }

    pub(super) async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before reading state, so a concurrent rate change cannot be lost.
            changed.as_mut().enable();
            let next = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .next();
            let Some(next) = next.filter(|next| *next > Instant::now()) else {
                return;
            };
            tokio::select! {
                _ = tokio::time::sleep_until(next) => {},
                _ = &mut changed => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn startup_repeats_and_long_passes_use_actual_start_without_catch_up() {
        let gate = StartSpacing::default();
        assert!(gate.try_start()); // Startup is immediately eligible.
        assert!(!gate.try_start()); // Changed-repeat uses the same gate.
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(!gate.try_start());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(gate.try_start());
        tokio::time::advance(Duration::from_secs(20)).await; // A long pass earns no burst credit.
        assert!(gate.try_start());
        assert!(!gate.try_start());
    }

    #[tokio::test(start_paused = true)]
    async fn burst_and_deadline_during_gap_keep_latest_input_for_next_pass() {
        let gate = Arc::new(StartSpacing::default());
        let notify = Arc::new(Notify::new());
        let value = Arc::new(std::sync::atomic::AtomicU32::new(0));
        assert!(gate.try_start());
        let worker = {
            let (gate, notify, value) = (gate.clone(), notify.clone(), value.clone());
            tokio::spawn(async move {
                // Model the run-loop's selected deadline/notification followed by gated entry.
                notify.notified().await;
                gate.wait().await;
                assert!(gate.try_start());
                value.load(std::sync::atomic::Ordering::Relaxed)
            })
        };
        for n in 1..=100 {
            value.store(n, std::sync::atomic::Ordering::Relaxed);
            notify.notify_one(); // Pending notifications coalesce, without clearing current input.
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        notify.notify_one(); // Another deadline/notification during the gap.
        assert!(!worker.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(worker.await.unwrap(), 100);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_changes_preserve_history_and_recheck_delayed_dispatch() {
        let gate = Arc::new(StartSpacing::default());
        assert!(gate.try_start());
        tokio::time::advance(Duration::from_secs(1)).await;
        gate.set(60).unwrap();
        gate.wait().await; // Eligible under raised rate; blocking dispatch has not begun yet.
        gate.set(15).unwrap();
        assert!(!gate.try_start()); // Lower rate while queued still applies to actual admission.
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                gate.try_start()
            })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!waiter.is_finished());
        gate.set(30).unwrap(); // Raise wakes a pending wait and retains its original last start.
        assert!(waiter.await.unwrap());
        assert!(!gate.try_start());
        assert!(gate.set(0).is_err());
        assert!(gate.set(601).is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!gate.try_start()); // Invalid config retained effective30, rather than reset history.
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_supervisor_restart_do_not_reserve_or_reset_starts() {
        let gate = Arc::new(StartSpacing::default());
        let queued = QueuedAdmission::default();
        let active = queued.0.clone();
        drop(queued);
        assert!(!gate.try_start_if(&active));
        assert!(gate.state.lock().unwrap().last.is_none());
        assert!(gate.try_start());
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait().await })
        };
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(!gate.try_start()); // Same Reconciler/gate is kept on supervisor restart.
        tokio::time::advance(Duration::from_secs(2)).await;
        gate.wait().await;
        assert!(gate.try_start());
    }
}
