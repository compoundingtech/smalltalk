use super::*;
fn report(wait: u128, truncate: u128) -> smallclaims::sqlite::WalCheckpointReport {
    smallclaims::sqlite::WalCheckpointReport {
        frames: 3,
        backfilled: 3,
        passive_ms: 1,
        writer_wait_ms: wait,
        truncate_ms: Some(truncate),
        recycled: true,
    }
}
#[test]
fn ordinary_sample_does_not_hide_first_anomaly_and_suppressed_peaks_survive() {
    let now = Instant::now();
    let mut ordinary = WalCheckpointLogBucket::default();
    let mut abnormal = WalCheckpointLogBucket::default();
    assert!(ordinary.record(now, Some(&report(0, 1)), 1).is_some());
    assert!(!abnormal_wal_checkpoint(Some(&report(99, 99))));
    assert!(abnormal_wal_checkpoint(Some(&report(100, 1))));
    assert!(
        abnormal
            .record(now + Duration::from_secs(5), Some(&report(100, 1)), 101)
            .is_some()
    );
    assert!(
        abnormal
            .record(now + Duration::from_secs(10), Some(&report(500, 200)), 701)
            .is_none()
    );
    assert!(
        abnormal
            .record(now + Duration::from_secs(15), None, 900)
            .is_none()
    );
    assert!(abnormal_wal_checkpoint(None));
    let line = abnormal
        .record(now + Duration::from_secs(65), Some(&report(100, 1)), 101)
        .unwrap();
    assert_eq!(line["suppressed_outcomes"], 2);
    assert_eq!(line["error_outcomes"], 1);
    assert_eq!(line["max_writer_wait_ms"], 500);
    assert_eq!(line["max_truncate_ms"], 200);
    assert_eq!(line["max_duration_ms"], 900);
    let reset = abnormal
        .record(now + Duration::from_secs(125), Some(&report(100, 1)), 101)
        .unwrap();
    assert_eq!(reset["suppressed_outcomes"], 0);
    assert_eq!(reset["error_outcomes"], 0);
    assert_eq!(reset["max_writer_wait_ms"], 100);
}
#[test]
fn passive_threshold_and_counts_are_bounded() {
    let now = Instant::now();
    let mut sample = report(0, 0);
    sample.passive_ms = 1000;
    assert!(abnormal_wal_checkpoint(Some(&sample)));
    let mut bucket = WalCheckpointLogBucket {
        last: Some(now),
        suppressed: u64::MAX,
        errors: u64::MAX,
        ..Default::default()
    };
    assert!(
        bucket
            .record(now + Duration::from_secs(1), None, 1)
            .is_none()
    );
    let line = bucket
        .record(now + Duration::from_secs(60), None, 1)
        .unwrap();
    assert_eq!(line["suppressed_outcomes"], u64::MAX);
    assert_eq!(line["error_outcomes"], u64::MAX);
}
