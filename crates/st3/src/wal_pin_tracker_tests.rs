use super::*;

fn sample(frames: i32, backfilled: i32) -> smallclaims::sqlite::WalCheckpointReport {
    smallclaims::sqlite::WalCheckpointReport {
        frames,
        backfilled,
        passive_ms: 1,
        writer_wait_ms: 0,
        truncate_ms: None,
        recycled: false,
    }
}

#[test]
fn a_backfill_that_stops_advancing_counts_up_and_progress_resets_it() {
    let mut pin = WalPinTracker::default();
    assert_eq!(pin.observe(Some(&sample(100, 50))), 0, "the first sample has no baseline");
    assert_eq!(pin.observe(Some(&sample(200, 50))), 1);
    assert_eq!(pin.observe(Some(&sample(300, 50))), 2);
    assert_eq!(pin.observe(None), 2, "an errored attempt neither counts nor resets");
    assert_eq!(pin.observe(Some(&sample(400, 50))), 3);
    assert_eq!(pin.observe(Some(&sample(500, 60))), 0, "backfill moved");
    assert_eq!(pin.observe(Some(&sample(500, 500))), 0, "fully copied is not a pin");
    assert_eq!(pin.observe(Some(&sample(500, 500))), 0);
}
