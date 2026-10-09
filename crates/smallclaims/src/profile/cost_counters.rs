//! One cached completed profile bucket. Reads neither initialize profiling nor touch SQLite.
use super::{Minute, STATE, enabled, unix_ms};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

static MANAGED_COMMITS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn managed_commit_count() -> u64 {
    MANAGED_COMMITS.load(Ordering::Relaxed)
}

/// Successful managed outer commits, including no-ops; never claims, loans or ACKs.
pub(crate) fn managed_commit_succeeded() {
    MANAGED_COMMITS.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
pub(super) struct Boundary {
    at: Instant,
    unix_ms: u128,
    cpu_ns: Option<u64>,
    managed_commits: u64,
}

impl Boundary {
    pub(super) fn now() -> Self {
        Self {
            at: Instant::now(),
            unix_ms: unix_ms(),
            cpu_ns: process_cpu_ns(),
            managed_commits: MANAGED_COMMITS.load(Ordering::Relaxed),
        }
    }
}

fn process_cpu_ns() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid timespec is passed to the process CPU clock.
        if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) } != 0 {
            return None;
        }
        u64::try_from(time.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(time.tv_nsec).ok()?)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub(super) struct Window {
    start: Boundary,
    epoch: Value,
    completed: Option<Arc<Value>>,
}

impl Window {
    pub(super) fn new() -> Self {
        Self::starting(
            Boundary::now(),
            std::process::id(),
            option_env!("ST3_BUILD_REVISION"),
        )
    }

    fn starting(start: Boundary, pid: u32, source: Option<&str>) -> Self {
        Self {
            start,
            epoch: json!({
                "pid": pid, "profile_started_at_unix_ms": start.unix_ms,
                "source_revision": source,
                "source_revision_state": if source.is_some() { "build_metadata" } else { "unavailable" },
            }),
            completed: None,
        }
    }

    pub(super) fn complete(
        &mut self,
        minute: &Minute,
        end: Boundary,
        process_start_ticks: Option<u64>,
    ) {
        let seconds = end
            .at
            .saturating_duration_since(self.start.at)
            .as_secs_f64();
        let per_minute = |number: f64| (seconds > 0.0).then(|| number * 60.0 / seconds);
        let reconcile = minute.ops.get("task reconcile-pass");
        let passes = reconcile.map_or(0, |op| op.count);
        let sql = reconcile.map_or(0, |op| op.acc.sql.count);
        let count = |name: &str| {
            reconcile
                .and_then(|op| op.acc.spans.get(name))
                .map_or(0, |span| span.count)
        };
        // Use the original map, before totals_json truncates top spans for disk output.
        let startup = count("trigger/startup");
        let notification = count("trigger/notification");
        let deadline = count("trigger/deadline");
        let repeat = count("trigger/changed-repeat");
        let classified = startup
            .checked_add(notification)
            .and_then(|n| n.checked_add(deadline))
            .and_then(|n| n.checked_add(repeat));
        let remainder = classified.and_then(|n| passes.checked_sub(n));
        let commits = end.managed_commits.checked_sub(self.start.managed_commits);
        let cpu = self
            .start
            .cpu_ns
            .zip(end.cpu_ns)
            .and_then(|(start, end)| end.checked_sub(start));
        let ratio = |numerator: f64, denominator: u64| {
            (denominator > 0).then(|| numerator / denominator as f64)
        };
        self.completed = Some(Arc::new(json!({
            "epoch": self.epoch,
            "process_start_ticks": process_start_ticks,
            "start_unix_ms": self.start.unix_ms, "end_unix_ms": end.unix_ms,
            "elapsed_seconds": seconds,
            "boundary_sampling": "sequential monotonic clock, wall clock, process CPU and managed-commit counter; not an atomic process snapshot",
            "reconcile": {
                "unit": "completed profiled task reconcile-pass operations, including errors",
                "coverage": "work is charged at operation completion; unfinished/unprofiled/nested direct calls and work recorded after finish are not included; completed work may have begun before this interval",
                "passes": passes, "passes_per_minute": per_minute(passes as f64),
                "by_trigger": {"startup": startup, "notification": notification, "deadline": deadline, "changed_repeat": repeat, "direct_or_unknown": remainder},
                "trigger_state": if remainder.is_some() { "complete_with_remainder" } else { "inconsistent" },
                "trigger_source": "full completed task spans before top-span truncation",
                "sql_count": sql, "sql_per_minute": per_minute(sql as f64),
                "statements_per_pass": ratio(sql as f64, passes),
                "passes_per_managed_commit": commits.and_then(|n| ratio(passes as f64, n)),
            },
            "process_cpu": {
                "state": if cpu.is_some() { "available" } else { "unavailable" },
                "clock": "CLOCK_PROCESS_CPUTIME_ID",
                "start_cpu_ns": self.start.cpu_ns, "end_cpu_ns": end.cpu_ns,
                "cpu_ns": cpu, "seconds_per_minute": cpu.and_then(|n| per_minute(n as f64 / 1_000_000_000.0)),
                "seconds_per_1000_managed_commits": cpu.zip(commits).and_then(|(cpu,n)| ratio(cpu as f64 / 1_000_000.0,n)),
            },
            "managed_outer_commits": {
                "state": if commits.is_some() { "partial" } else { "unavailable" },
                "unit": "managed_outer_commits",
                "includes": "successful managed outer transaction commits, including no-op commits",
                "coverage": "writer batches and managed WriterTransaction commits only; raw Connection transactions, autocommit, SQL BEGIN/COMMIT and private abortable_transaction direct commits excluded; not claims/rows/ACKs",
                "start_count": self.start.managed_commits, "end_count": end.managed_commits,
                "count": commits, "per_minute": commits.and_then(|n| per_minute(n as f64)),
                "all_process_writes_state": "unavailable",
            },
        })));
        self.start = end;
    }
}

fn response(is_enabled: bool, completed: Option<Arc<Value>>, as_of: u128) -> Value {
    json!({
        "state": if !is_enabled { "disabled" } else if completed.is_none() { "no_completed_window" } else { "available" },
        "as_of_unix_ms": as_of,
        "window": if is_enabled { completed.as_deref() } else { None },
    })
}

/// Fixed-size in-memory read. No profiler activation, file scan, timer or graph fold.
pub fn snapshot() -> Value {
    let on = enabled();
    let completed = if on {
        STATE.get().and_then(|state| {
            state
                .cost_window
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .completed
                .clone()
        })
    } else {
        None
    };
    response(on, completed, unix_ms())
}

#[cfg(test)]
mod tests {
    use super::super::{SpanStat, Totals};
    use super::*;
    use std::time::Duration;

    fn boundary(at: Instant, unix_ms: u128, cpu_ns: Option<u64>, commits: u64) -> Boundary {
        Boundary {
            at,
            unix_ms,
            cpu_ns,
            managed_commits: commits,
        }
    }

    fn minute(passes: u64, sql: u64, triggers: [u64; 4]) -> Minute {
        let mut minute = Minute::default();
        let mut task = Totals {
            count: passes,
            ..Totals::default()
        };
        task.acc.sql.count = sql;
        // Deliberately unrelated task/thread CPU must not replace the process clock.
        task.acc.cpu_ns = 900_000_000_000;
        task.sampled_cpu_ns = 800_000_000_000;
        for (name, count) in ["startup", "notification", "deadline", "changed-repeat"]
            .into_iter()
            .zip(triggers)
        {
            task.acc.spans.insert(
                format!("trigger/{name}").into_boxed_str(),
                SpanStat {
                    count,
                    ..SpanStat::default()
                },
            );
        }
        // More spans than the exported top-five limit cannot hide trigger counts.
        for n in 0..12 {
            task.acc.spans.insert(
                format!("large/{n}").into_boxed_str(),
                SpanStat {
                    count: 100,
                    wall_ns: u64::MAX / 100,
                    ..SpanStat::default()
                },
            );
        }
        minute.ops.insert("task reconcile-pass".into(), task);
        let mut unrelated = Totals {
            count: 500,
            ..Totals::default()
        };
        unrelated.acc.sql.count = 999_999;
        minute.ops.insert("GET /unrelated".into(), unrelated);
        minute
    }

    #[test]
    fn completed_window_has_same_interval_raw_units_and_untruncated_triggers() {
        let at = Instant::now();
        let mut window = Window::starting(
            boundary(at, 1000, Some(10_000_000_000), 40),
            7,
            Some("pinned-source"),
        );
        window.complete(
            &minute(10, 500, [1, 2, 3, 3]),
            boundary(
                at + Duration::from_secs(30),
                31_000,
                Some(16_000_000_000),
                44,
            ),
            Some(123),
        );
        let result = response(true, window.completed.clone(), 35_000);
        let data = &result["window"];
        assert_eq!(result["as_of_unix_ms"], 35_000);
        assert_eq!(data["epoch"]["source_revision"], "pinned-source");
        assert_eq!(data["process_start_ticks"], 123);
        assert_eq!(data["elapsed_seconds"], 30.0);
        assert_eq!(data["reconcile"]["passes"], 10);
        assert_eq!(
            data["reconcile"]["by_trigger"],
            json!({"startup":1,"notification":2,"deadline":3,"changed_repeat":3,"direct_or_unknown":1})
        );
        assert_eq!(data["reconcile"]["sql_count"], 500);
        assert_eq!(data["reconcile"]["sql_per_minute"], 1000.0);
        assert_eq!(data["reconcile"]["statements_per_pass"], 50.0);
        assert_eq!(data["reconcile"]["passes_per_managed_commit"], 2.5);
        assert_eq!(data["managed_outer_commits"]["count"], 4);
        assert_eq!(data["managed_outer_commits"]["per_minute"], 8.0);
        assert_eq!(data["process_cpu"]["cpu_ns"], 6_000_000_000_u64);
        assert_eq!(data["process_cpu"]["seconds_per_minute"], 12.0);
        assert_eq!(
            data["process_cpu"]["seconds_per_1000_managed_commits"],
            1500.0
        );
        // Reading a retained snapshot never advances or recomputes its accounting interval.
        assert_eq!(
            response(true, window.completed.clone(), 40_000)["window"],
            *data
        );
        window.complete(
            &minute(1, 7, [0, 0, 0, 1]),
            boundary(
                at + Duration::from_secs(90),
                91_000,
                Some(18_000_000_000),
                45,
            ),
            Some(123),
        );
        let next = response(true, window.completed, 92_000);
        assert_eq!(next["window"]["start_unix_ms"], 31_000);
        assert_eq!(next["window"]["managed_outer_commits"]["count"], 1);
        assert_eq!(next["window"]["process_cpu"]["cpu_ns"], 2_000_000_000_u64);
    }

    #[test]
    fn disabled_missing_zero_and_unavailable_are_explicit() {
        let at = Instant::now();
        let mut window = Window::starting(boundary(at, 1, None, 10), 9, None);
        assert_eq!(response(true, None, 2)["state"], "no_completed_window");
        window.complete(&Minute::default(), boundary(at, 1, None, 10), None);
        let data = window.completed.clone().unwrap();
        assert_eq!(data["epoch"]["source_revision_state"], "unavailable");
        assert!(data["process_start_ticks"].is_null());
        assert_eq!(data["reconcile"]["passes"], 0);
        assert!(data["reconcile"]["statements_per_pass"].is_null());
        assert!(data["reconcile"]["passes_per_minute"].is_null());
        assert!(data["reconcile"]["passes_per_managed_commit"].is_null());
        assert_eq!(data["managed_outer_commits"]["count"], 0);
        assert_eq!(data["managed_outer_commits"]["state"], "partial");
        assert_eq!(data["process_cpu"]["state"], "unavailable");
        assert!(data["process_cpu"]["seconds_per_1000_managed_commits"].is_null());
        let disabled = response(false, Some(data), 3);
        assert_eq!(disabled["state"], "disabled");
        assert!(disabled["window"].is_null());
    }

    #[test]
    fn invalid_counter_order_and_trigger_overcount_do_not_fabricate_rates() {
        let at = Instant::now();
        let mut window = Window::starting(boundary(at, 100, Some(100), 20), 1, None);
        window.complete(
            &minute(1, 4, [1, 1, 0, 0]),
            boundary(at + Duration::from_secs(60), 50, Some(90), 19),
            None,
        );
        let data = window.completed.unwrap();
        assert_eq!(data["reconcile"]["trigger_state"], "inconsistent");
        assert!(data["reconcile"]["by_trigger"]["direct_or_unknown"].is_null());
        assert_eq!(data["elapsed_seconds"], 60.0); // Wall-clock reversal does not alter the divisor.
        assert!(data["managed_outer_commits"]["count"].is_null());
        assert!(data["process_cpu"]["cpu_ns"].is_null());
    }

    #[test]
    fn a_new_process_window_keeps_its_own_baseline_and_epoch() {
        let at = Instant::now();
        let mut old = Window::starting(boundary(at, 100, Some(900), 40), 1, Some("old"));
        old.complete(
            &minute(4, 10, [0, 4, 0, 0]),
            boundary(at + Duration::from_secs(1), 1100, Some(1000), 42),
            Some(10),
        );
        let mut fresh = Window::starting(boundary(at, 2000, Some(10), 0), 2, Some("new"));
        assert!(fresh.completed.is_none());
        fresh.complete(
            &Minute::default(),
            boundary(at + Duration::from_secs(1), 3000, Some(15), 1),
            Some(20),
        );
        let data = fresh.completed.unwrap();
        assert_eq!(data["epoch"]["pid"], 2);
        assert_eq!(data["epoch"]["source_revision"], "new");
        assert_eq!(data["managed_outer_commits"]["start_count"], 0);
        assert_eq!(data["process_cpu"]["cpu_ns"], 5);
        assert_eq!(data["reconcile"]["passes"], 0);
    }
}
