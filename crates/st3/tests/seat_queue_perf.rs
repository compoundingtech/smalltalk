//! Time the seat queue reads against a copy of the performance fixture store.
//!
//! Run with a fixture from `scripts/st3-seat-queue-perf/fixture`:
//!
//! ```sh
//! ST3_PERF_STORE=/path/to/copy/claims.sqlite3 \
//!   cargo test --release -p st3 --test integration seat_queue_perf:: -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn time(label: &str, rounds: u32, mut read: impl FnMut()) {
    read();
    let mut samples = Vec::with_capacity(rounds as usize);
    for _ in 0..rounds {
        let started = Instant::now();
        read();
        samples.push(started.elapsed());
    }
    samples.sort();
    let total: Duration = samples.iter().sum();
    let percentile = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction) as usize];
    println!(
        "{label}: mean {:.3} ms, p50 {:.3} ms, p95 {:.3} ms over {rounds} reads",
        total.as_secs_f64() * 1_000.0 / f64::from(rounds),
        percentile(0.5).as_secs_f64() * 1_000.0,
        percentile(0.95).as_secs_f64() * 1_000.0,
    );
}

#[test]
#[ignore = "needs ST3_PERF_STORE pointing at a copy of the performance fixture"]
fn seat_queue_reads_on_the_performance_fixture() {
    let path = PathBuf::from(std::env::var("ST3_PERF_STORE").expect("set ST3_PERF_STORE"));
    let store = st3::store::Store::open(&path, "perfnode").unwrap();
    let orders = store.seat_run_orders().unwrap();
    let seats = orders.keys().cloned().collect::<Vec<_>>();
    println!(
        "{} seats, {} queued runs",
        seats.len(),
        orders.values().map(Vec::len).sum::<usize>()
    );

    time("seat_run_orders (all seats)", 200, || {
        store.seat_run_orders().unwrap();
    });
    time("seat_run_order (one seat)", 200, || {
        store.seat_run_order(&seats[0]).unwrap();
    });
    time("seat_queue view (one seat)", 200, || {
        store.seat_queue(&seats[0]).unwrap();
    });
    time("agent_work_queues (roster)", 200, || {
        store.agent_work_queues().unwrap();
    });
    time("work_for_reconcile_all", 200, || {
        store.work_for_reconcile_all().unwrap();
    });
    time("work_for_reconcile (one seat)", 200, || {
        store.work_for_reconcile(&seats[0]).unwrap();
    });
}
