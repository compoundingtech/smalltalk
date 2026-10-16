//! Candidate selection uses the existing clock/catch-up rules, not heartbeat expiry as authority.
use super::*;

fn spec(policy: &str, max: Option<u32>) -> crate::model::ScheduleSpec {
    crate::model::ScheduleSpec {
        stopped: false, host: "ivory".into(),
        standby: Some(crate::model::ScheduleStandby { host: "jade".into(), after_ms: 1_000 }),
        at_unix_ms: None, every_ms: Some(1_000), anchor_unix_ms: Some(0), calendar: None,
        catch_up: policy.into(), max_catch_up: max, work: None,
    }
}
fn current(spec: &crate::model::ScheduleSpec, occurrence: u64, last: Option<u64>, now: i64) -> bool {
    Reconciler::<NativeRuntime>::standby_candidate_current(spec, occurrence, last, now).unwrap()
}

#[test]
fn long_outage_latest_skip_bounded_all_and_already_reached_do_not_replay() {
    assert!(!current(&spec("latest", None), 2, Some(1), 100_000));
    assert!(current(&spec("latest", None), 100, Some(1), 100_000));
    assert!(!current(&spec("latest", None), 100, Some(100), 100_000));
    assert!(!current(&spec("skip", None), 2, Some(1), 100_000));
    // A skip candidate pre-armed for this tick is eligible at its tick, not a backlog tick.
    assert!(current(&spec("skip", None), 100, Some(1), 100_000));
    assert!(!current(&spec("all", Some(3)), 2, Some(1), 100_000));
    assert!(current(&spec("all", Some(3)), 98, Some(97), 100_000));
    assert!(!current(&spec("all", Some(3)), 98, Some(98), 100_000));
    assert!(!current(&spec("latest", None), 0, None, -1));
}

#[test]
fn one_shot_and_calendar_keep_existing_keys_and_do_not_reopen_reached_work() {
    let mut once = spec("latest", None);
    once.at_unix_ms = Some(1);
    once.every_ms = None;
    once.anchor_unix_ms = None;
    assert!(current(&once, 0, None, 100_000));
    assert!(!current(&once, 0, Some(0), 100_000));
    let mut daily = spec("latest", None);
    daily.calendar = Some(CalendarSchedule { at_minute: 12 * 60, weekday: None, timezone: "UTC".into() });
    daily.every_ms = None;
    daily.anchor_unix_ms = None;
    let at = chrono::DateTime::parse_from_rfc3339("2030-01-10T12:01:00Z").unwrap().timestamp_millis();
    assert!(!current(&daily, 20300102, Some(20300101), at));
    assert!(current(&daily, 20300110, Some(20300101), at));
    assert!(!current(&daily, 20300110, Some(20300110), at));
}
