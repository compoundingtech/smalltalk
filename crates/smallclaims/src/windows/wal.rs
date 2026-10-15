//! The newest WAL checkpoint the daemon's checkpoint worker observed, and how long checkpoints
//! have been deferred: since the last one that found the WAL empty or recycled it. The worker
//! logs these only now and then; this keeps the newest one readable at any time.

use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::sqlite::WalCheckpointReport;

/// One checkpoint attempt's outcome, as `WalCheckpointReport` gives it.
#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    frames: i32,
    backfilled: i32,
    passive_ms: u128,
    writer_wait_ms: u128,
    truncate_ms: Option<u128>,
    recycled: bool,
}

#[derive(Clone, Debug, Default)]
struct Wal {
    observed_unix_ms: Option<u128>,
    /// `None` when the newest attempt failed.
    report: Option<Checkpoint>,
    /// The WAL file's size when observed, apart from the frames it holds.
    bytes: Option<u64>,
    /// When checkpoints started being deferred or failing, unbroken since.
    deferred_since_unix_ms: Option<u128>,
}

static WAL: Mutex<Wal> = Mutex::new(Wal {
    observed_unix_ms: None,
    report: None,
    bytes: None,
    deferred_since_unix_ms: None,
});

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

impl Wal {
    fn observe(&mut self, at: u128, report: Option<&WalCheckpointReport>, bytes: Option<u64>) {
        let settled = report.is_some_and(|report| report.frames <= 0 || report.recycled);
        if settled {
            self.deferred_since_unix_ms = None;
        } else if self.deferred_since_unix_ms.is_none() {
            self.deferred_since_unix_ms = Some(at);
        }
        self.observed_unix_ms = Some(at);
        self.report = report.map(|report| Checkpoint {
            frames: report.frames,
            backfilled: report.backfilled,
            passive_ms: report.passive_ms,
            writer_wait_ms: report.writer_wait_ms,
            truncate_ms: report.truncate_ms,
            recycled: report.recycled,
        });
        self.bytes = bytes;
    }

    fn report(&self, now: u128) -> Value {
        let Some(observed) = self.observed_unix_ms else {
            return Value::Null;
        };
        let checkpoint = match self.report {
            Some(checkpoint) => json!({
                "outcome": if checkpoint.frames <= 0 {
                    "empty"
                } else if checkpoint.recycled {
                    "recycled"
                } else {
                    "deferred"
                },
                "frames": checkpoint.frames,
                "backfilled": checkpoint.backfilled,
                "not_backfilled": (checkpoint.frames - checkpoint.backfilled).max(0),
                "passive_ms": checkpoint.passive_ms,
                "writer_wait_ms": checkpoint.writer_wait_ms,
                "truncate_ms": checkpoint.truncate_ms,
            }),
            None => json!({"outcome": "error"}),
        };
        json!({
            "observed_unix_ms": observed,
            "age_ms": now.saturating_sub(observed),
            "checkpoint": checkpoint,
            "wal_bytes": self.bytes,
            "deferred_since_unix_ms": self.deferred_since_unix_ms,
            "deferred_ms": self.deferred_since_unix_ms.map(|since| now.saturating_sub(since)),
        })
    }
}

/// Note one checkpoint attempt: its report, or `None` when it failed, and the WAL file's size.
pub fn observe(report: Option<&WalCheckpointReport>, bytes: Option<u64>) {
    WAL.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .observe(unix_ms(), report, bytes);
}

/// The newest observation and how long checkpoints have been deferred; null before any.
pub fn report() -> Value {
    WAL.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .report(unix_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(frames: i32, backfilled: i32, recycled: bool) -> WalCheckpointReport {
        WalCheckpointReport {
            frames,
            backfilled,
            passive_ms: 3,
            writer_wait_ms: 0,
            truncate_ms: None,
            recycled,
        }
    }

    #[test]
    fn deferral_runs_from_the_first_unsettled_checkpoint_until_one_settles() {
        let mut wal = Wal::default();
        assert_eq!(wal.report(1_000), Value::Null);
        wal.observe(1_000, Some(&checkpoint(0, 0, false)), Some(0));
        assert_eq!(wal.report(1_000)["deferred_ms"], Value::Null);
        // A reader pins the WAL: frames grow and only some are backfilled.
        wal.observe(6_000, Some(&checkpoint(800, 300, false)), Some(4_000_000));
        wal.observe(11_000, None, None);
        wal.observe(
            16_000,
            Some(&checkpoint(1_200, 300, false)),
            Some(6_000_000),
        );
        let deferred = wal.report(20_000);
        assert_eq!(deferred["deferred_since_unix_ms"], 6_000);
        assert_eq!(deferred["deferred_ms"], 14_000);
        assert_eq!(deferred["age_ms"], 4_000);
        assert_eq!(deferred["checkpoint"]["outcome"], "deferred");
        assert_eq!(deferred["checkpoint"]["not_backfilled"], 900);
        assert_eq!(deferred["wal_bytes"], 6_000_000);
        // The reader ends and the WAL recycles.
        wal.observe(
            21_000,
            Some(&checkpoint(1_200, 1_200, true)),
            Some(6_000_000),
        );
        let settled = wal.report(21_000);
        assert_eq!(settled["checkpoint"]["outcome"], "recycled");
        assert_eq!(settled["deferred_ms"], Value::Null);
    }
}
