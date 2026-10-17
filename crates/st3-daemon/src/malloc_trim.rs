//! Periodically return free glibc arena pages without blocking the async runtime.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use opentelemetry::KeyValue;

const DEFAULT_THRESHOLD: usize = 256 * 1024 * 1024;
const DEFAULT_INTERVAL_SECS: u64 = 30;

fn should_trim(free_bytes: usize, threshold: usize) -> bool {
    threshold != 0 && free_bytes > threshold
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(value) => value.parse().with_context(|| format!("invalid {name}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("invalid {name}")),
    }
}

fn trim_interval(seconds: u64) -> anyhow::Result<(Duration, tokio::time::Instant)> {
    anyhow::ensure!(seconds > 0, "ST3_MALLOC_TRIM_INTERVAL_SECS must be positive");
    let period = Duration::from_secs(seconds);
    let first_tick = tokio::time::Instant::now()
        .checked_add(period)
        .context("ST3_MALLOC_TRIM_INTERVAL_SECS exceeds the timer range")?;
    Ok((period, first_tick))
}

/// Start once, after daemon telemetry initializes. Zero threshold disables the worker.
pub fn spawn() -> anyhow::Result<()> {
    let threshold = env_number("ST3_MALLOC_TRIM_THRESHOLD_BYTES", DEFAULT_THRESHOLD)?;
    if threshold == 0 {
        return Ok(());
    }
    let seconds = env_number("ST3_MALLOC_TRIM_INTERVAL_SECS", DEFAULT_INTERVAL_SECS)?;
    let (period, first_tick) = trim_interval(seconds)?;
    let meter = opentelemetry::global::meter("st3");
    let duration = meter
        .f64_histogram("st.allocator.trim.duration")
        .with_description("Time spent in malloc_trim")
        .with_unit("s")
        .build();
    let released = meter
        .u64_histogram("st.allocator.trim.released")
        .with_description("Observed decrease across malloc_trim, by source; not causal attribution")
        .with_unit("By")
        .build();
    tokio::spawn(crate::task_cpu::account("allocator.trim", async move {
        let mut interval = tokio::time::interval_at(first_tick, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // Sample on the same blocking thread as trim: mallinfo2 also takes arena locks.
            match tokio::task::spawn_blocking(move || crate::task_cpu::blocking("allocator.trim", || trim_once(threshold))).await {
                Ok(Some(sample)) => {
                    duration.record(sample.elapsed.as_secs_f64(), &[]);
                    released.record(
                        sample.free_before.saturating_sub(sample.free_after) as u64,
                        &[KeyValue::new("source", "fordblks")],
                    );
                    if let (Some(before), Some(after)) = (sample.rss_before, sample.rss_after) {
                        released.record(
                            before.saturating_sub(after),
                            &[KeyValue::new("source", "rss")],
                        );
                    }
                    tracing::debug!(
                        free_before = sample.free_before,
                        free_after = sample.free_after,
                        rss_before = sample.rss_before,
                        rss_after = sample.rss_after,
                        duration_ms = sample.elapsed.as_secs_f64() * 1000.0,
                        returned_pages = sample.returned_pages,
                        "glibc heap trim"
                    );
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "glibc heap trim worker failed"),
            }
        }
    }));
    Ok(())
}

struct Sample {
    free_before: usize,
    free_after: usize,
    rss_before: Option<u64>,
    rss_after: Option<u64>,
    elapsed: Duration,
    returned_pages: bool,
}

fn free_bytes() -> usize {
    // SAFETY: supported glibc >= 2.33; normally initialized thread, no signal
    // handler or asynchronous cancellation. Returns counters without pointers.
    unsafe { libc::mallinfo2().fordblks }
}

fn trim_once(threshold: usize) -> Option<Sample> {
    let free_before = free_bytes();
    if !should_trim(free_before, threshold) {
        return None;
    }
    let rss_before = crate::otel::linux_rss_bytes();
    let started = Instant::now();
    // SAFETY: glibc malloc_trim is thread-safe and preserves live allocations.
    // Called on a normally initialized blocking thread, not in a signal handler
    // or under asynchronous cancellation. Zero requests no retained top padding.
    let returned_pages = unsafe { libc::malloc_trim(0) != 0 };
    let elapsed = started.elapsed();
    let rss_after = crate::otel::linux_rss_bytes();
    Some(Sample {
        free_before,
        free_after: free_bytes(),
        rss_before,
        rss_after,
        elapsed,
        returned_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_decision() {
        assert!(!should_trim(usize::MAX, 0));
        assert!(!should_trim(255, 256));
        assert!(!should_trim(256, 256));
        assert!(should_trim(257, 256));
    }

    #[test]
    fn interval_rejects_zero_and_timer_overflow() {
        assert!(trim_interval(0).is_err());
        assert!(trim_interval(u64::MAX).is_err());
        assert_eq!(trim_interval(30).unwrap().0, Duration::from_secs(30));
    }

    #[test]
    fn retained_heap_returns_resident_pages() {
        const CHILD: &str = "ST3_MALLOC_TRIM_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate allocator state from parallel tests and limit arena noise.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "malloc_trim::tests::retained_heap_returns_resident_pages",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("MALLOC_ARENA_MAX", "1")
                .output()
                .unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let chunks: Vec<_> = (0..12_800)
            .map(|_| vec![0x5a_u8; 16 * 1024].into_boxed_slice())
            .collect();
        // Keep the last chunk live so freeing the preceding chunks cannot trim the top.
        let mut chunks = chunks;
        let live = chunks.pop().unwrap();
        drop(chunks);
        std::hint::black_box(&live);
        assert!(free_bytes() > 180 * 1024 * 1024);
        let sample = trim_once(128 * 1024 * 1024).unwrap();
        let before = sample.rss_before.unwrap();
        let after = sample.rss_after.unwrap();
        println!(
            "malloc_trim: RSS {before} -> {after} bytes; fordblks {} -> {} bytes; duration {:?}",
            sample.free_before, sample.free_after, sample.elapsed
        );
        assert!(before.saturating_sub(after) > 150 * 1024 * 1024);
        assert_eq!(live[0], 0x5a);
        assert_eq!(live[live.len() - 1], 0x5a);
    }
}
