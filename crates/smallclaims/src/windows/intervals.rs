//! Externally observed closed UTC minutes, separate from process-Instant response timings.
//! Reuses Histogram's sparse bucket arithmetic in integer milliseconds (Series uses µs).
use super::{Histogram, bucket, upper};
use serde_json::{Value, json};

const MINUTE_MS: u64 = 60_000;
const SLOTS: usize = 60;
const MAX_SLOT_COUNT: u64 = 1_000_000_000;

/// Canonical inclusive bin bound, in integer ms: exact below 16, then 16 per doubling.
pub fn histogram_upper_ms(value_ms: u64) -> u64 {
    upper(bucket(value_ms))
}

pub fn histogram_lower_ms(bound_ms: u64) -> u64 {
    let b = bucket(bound_ms);
    if b == 0 { 0 } else { upper(b - 1) + 1 }
}

/// A validated minute. For share, count/over are foreground/disconnected milliseconds and
/// buckets is empty. For latency, count/over are observations and buckets has canonical bounds.
#[derive(Clone, Debug)]
pub struct MinuteSummary {
    pub start_ms: u64,
    pub count: u64,
    pub over: u64,
    pub max_ms: u64,
    pub buckets: Vec<(u64, u64)>,
}

#[derive(Clone, Default)]
pub struct IntervalSeries {
    slots: Vec<(u64, Histogram)>,
}

impl IntervalSeries {
    /// Merge bounded minute slots for a population total, preserving boundary intervals so
    /// snapshot can disclose exclusions. Never repartitions summaries into event times.
    pub fn absorb(&mut self, other: &Self) {
        if self.slots.is_empty() {
            self.slots.clone_from(&other.slots);
            return;
        }
        for ((epoch, hist), (their_epoch, theirs)) in self.slots.iter_mut().zip(&other.slots) {
            if theirs.count == 0 {
                continue;
            }
            if hist.count == 0 || *epoch < *their_epoch {
                *epoch = *their_epoch;
                hist.clone_from(theirs);
            } else if *epoch == *their_epoch {
                hist.merge(theirs);
            }
        }
    }

    /// Preflight aggregate overflow before an atomic multi-population admission.
    pub fn can_record(&self, sample: &MinuteSummary) -> bool {
        let epoch = sample.start_ms / MINUTE_MS;
        self.slots
            .get((epoch % SLOTS as u64) as usize)
            .is_none_or(|(old, hist)| {
                let count = if *old == epoch { hist.count } else { 0 };
                count
                    .checked_add(sample.count)
                    .is_some_and(|n| n <= MAX_SLOT_COUNT)
            })
    }

    /// Merge a whole minute; old minutes outside the hour are history only. Never splits an
    /// interval or pretends it consists of individual event timestamps.
    pub fn record(&mut self, now_ms: u64, sample: &MinuteSummary) {
        if sample.start_ms.saturating_add(MINUTE_MS) > now_ms
            || sample.start_ms < now_ms.saturating_sub(3_600_000)
        {
            return;
        }
        if self.slots.is_empty() {
            self.slots.resize_with(SLOTS, Default::default);
        }
        let epoch = sample.start_ms / MINUTE_MS;
        let (old, hist) = &mut self.slots[(epoch % SLOTS as u64) as usize];
        if hist.count > 0 && *old > epoch {
            return;
        }
        if *old != epoch {
            *old = epoch;
            *hist = Histogram::default();
        }
        // Admission bounds each slot's total, including bucket counts, to one billion.
        let summary = Histogram {
            count: sample.count,
            over: sample.over,
            max_us: sample.max_ms,
            buckets: sample
                .buckets
                .iter()
                .map(|&(bound, n)| (bucket(bound), n))
                .collect(),
        };
        hist.merge(&summary);
    }

    /// Bounded sparse view for consumers of these external intervals. At most 60 slots,
    /// each using the same finite bucket vocabulary as Histogram; expired slots excluded.
    pub fn minutes(&self, now_ms: u64) -> Vec<MinuteSummary> {
        self.slots
            .iter()
            .filter_map(|(epoch, hist)| {
                let start = epoch * MINUTE_MS;
                (hist.count > 0
                    && start >= now_ms.saturating_sub(3_600_000)
                    && start + MINUTE_MS <= now_ms)
                    .then(|| MinuteSummary {
                        start_ms: start,
                        count: hist.count,
                        over: hist.over,
                        max_ms: hist.max_us,
                        buckets: hist.buckets.iter().map(|&(b, n)| (upper(b), n)).collect(),
                    })
            })
            .collect()
    }

    pub fn snapshot(&self, now_ms: u64, share: bool) -> Value {
        let mut windows = serde_json::Map::new();
        for (name, width) in [("1m", 60_000), ("5m", 300_000), ("1h", 3_600_000)] {
            let mut total = Histogram::default();
            let mut covered = 0;
            let mut boundary = 0;
            for (epoch, hist) in &self.slots {
                let start = epoch * MINUTE_MS;
                let end = start + MINUTE_MS;
                if hist.count == 0 || end > now_ms || end <= now_ms.saturating_sub(width) {
                    continue;
                }
                if start < now_ms.saturating_sub(width) {
                    boundary += 1;
                    continue;
                }
                total.merge(hist);
                covered += MINUTE_MS;
            }
            let mut row = json!({"count":total.count, "over_target":total.over,
                "over_target_share": if total.count == 0 {0.0} else {total.over as f64/total.count as f64},
                "resolution_ms":MINUTE_MS, "reported_interval_ms":covered,
                "excluded_boundary_intervals":boundary, "complete_coverage":false,
                "population": if share {"client-observed-foreground-ms"} else {"client-observed-latencies"}});
            if share {
                row["foreground_ms"] = json!(total.count);
                row["live_ms"] = json!(total.count - total.over);
                row["live_share"] = if total.count == 0 {
                    Value::Null
                } else {
                    json!(1.0 - total.over as f64 / total.count as f64)
                };
            } else {
                row["p50_ms"] = json!(total.percentile(50));
                row["p99_ms"] = json!(total.percentile(99));
                row["max_ms"] = json!(total.max_us);
            }
            windows.insert(name.into(), row);
        }
        Value::Object(windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn intervals_age_at_original_time_and_exclude_partial_boundaries() {
        let mut series = IntervalSeries::default();
        let minute = MinuteSummary {
            start_ms: 3_600_000,
            count: 100,
            over: 2,
            max_ms: 100,
            buckets: vec![(histogram_upper_ms(100), 100)],
        };
        series.record(3_660_000, &minute);
        assert_eq!(series.snapshot(3_660_000, false)["1m"]["count"], 100);
        assert_eq!(series.snapshot(3_660_001, false)["1m"]["count"], 0);
        assert_eq!(
            series.snapshot(3_660_001, false)["1m"]["excluded_boundary_intervals"],
            1
        );
        assert_eq!(series.snapshot(3_660_001, false)["5m"]["count"], 100);
        assert_eq!(series.snapshot(7_260_001, false)["1h"]["count"], 0);
        series.record(7_260_001, &minute);
        assert_eq!(series.snapshot(7_260_001, false)["1h"]["count"], 0);
    }
    #[test]
    fn share_uses_only_included_foreground_and_disconnected_time() {
        let mut series = IntervalSeries::default();
        for (start, count, over) in [(3_540_000, 60_000, 30_000), (3_600_000, 10_000, 100)] {
            series.record(
                3_660_000,
                &MinuteSummary {
                    start_ms: start,
                    count,
                    over,
                    max_ms: 0,
                    buckets: vec![],
                },
            );
        }
        let w = series.snapshot(3_660_000, true);
        assert_eq!(w["1m"]["foreground_ms"], 10_000);
        assert_eq!(w["1m"]["live_ms"], 9_900);
        assert_eq!(w["1m"]["live_share"], 0.99);
        assert!(w["1m"].get("p99_ms").is_none());
        assert_eq!(w["5m"]["foreground_ms"], 70_000);
        assert!(series.snapshot(3_660_001, true)["1m"]["live_share"].is_null());
    }
    #[test]
    fn minute_population_capacity_and_sparse_merging_are_bounded() {
        let mut series = IntervalSeries::default();
        let minute = MinuteSummary {
            start_ms: 3_600_000,
            count: MAX_SLOT_COUNT,
            over: 0,
            max_ms: 100,
            buckets: vec![(histogram_upper_ms(100), MAX_SLOT_COUNT)],
        };
        series.record(3_660_000, &minute);
        assert!(!series.can_record(&minute));
        let mut total = IntervalSeries::default();
        total.absorb(&series);
        total.absorb(&series);
        assert_eq!(
            total.snapshot(3_660_000, false)["1m"]["count"],
            MAX_SLOT_COUNT * 2
        );
        assert_eq!(total.minutes(3_660_000)[0].buckets.len(), 1);
        assert_eq!(
            total.snapshot(3_660_001, false)["1m"]["excluded_boundary_intervals"],
            1
        );
    }
}
