//! Native observation changes must invalidate a settled wake and retain its clock deadline.
use super::*;
use serde_json::json;

const START: u128 = 1_800_000_000_000;

struct Clock;
impl Clock {
    fn at(store: &Store, at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        store.set_write_clock_at(at).unwrap();
        Self
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

fn fixture() -> (Clock, SeatQueueFixture) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let clock = Clock::at(&store, START);
    let mut seat = SeatQueueFixture::with_store(store);
    seat.reconciler = seat.reconciler.skipping_unneeded(true);
    (clock, seat)
}

fn observe(seat: &SeatQueueFixture, sequence: u64, state: &str, incarnation: &str) {
    let (record, changed) = seat
        .store
        .append_harness_event(&crate::harness_events::Publication {
            runtime_incarnation: incarnation.into(),
            sequence,
            claim: ClaimInput {
                subject: SEAT.into(),
                kind: "harness.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("state".into(), json!(state)),
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("native-{sequence}")),
            },
        })
        .unwrap();
    assert!(changed);
    assert!(crate::store::local_observation_position(&record).is_some());
}

fn consume(seat: &SeatQueueFixture, wake: &crate::model::MessageView) {
    for lifecycle in ["delivered", "read", "closed"] {
        seat.store
            .append_claim(&ClaimInput {
                subject: wake.subject.clone(),
                kind: format!("message.{lifecycle}"),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([("status".into(), json!(lifecycle))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("native-consumed-{}-{lifecycle}", wake.subject)),
            })
            .unwrap();
    }
}

fn faults(seat: &SeatQueueFixture) -> Vec<crate::model::ClaimRecord> {
    seat.store
        .claims_for(SEAT, Some("harness.diagnostic"))
        .unwrap()
        .into_iter()
        .filter(|claim| claim.body["fields"]["code"] == "work-wake-exhausted")
        .collect()
}

#[test]
fn native_idle_rearms_the_consumed_wake_deadline_without_reordering_work() {
    let (_clock, seat) = fixture();
    let first = seat.start("queued", "native-first");
    let selected = SeatQueueFixture::step(&first, "work");
    let _clock = Clock::at(&seat.store, START + 1);
    let second = seat.start("queued", "native-second");
    let waiting = SeatQueueFixture::step(&second, "work");
    assert_eq!(seat.wake().as_deref(), Some(selected.as_str()));
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    let sent = seat
        .store
        .latest_claim(&wake.subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    consume(&seat, &wake);
    let _clock = Clock::at(&seat.store, START + 2);
    observe(&seat, 1, "working", "seat-one");
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(seat.reconciler.incremental.next_due("wake:"), None);
    assert!(faults(&seat).is_empty());
    let item = "wake:agent/node.worker@seat-one";
    assert!(
        !seat.reconciler.incremental.needs(item, now_ms()),
        "settled wake is clean"
    );
    let _clock = Clock::at(&seat.store, START + 3);
    observe(&seat, 2, "idle", "seat-one");
    seat.reconciler.reconcile_once().unwrap();
    let deadline = sent.accepted_at_unix_ms + WORK_WAKE_EXHAUST_GRACE_MS;
    assert_eq!(
        seat.reconciler.incremental.next_due("wake:"),
        Some(deadline),
        "the changed observation arms the item deadline before any periodic full pass"
    );
    assert!(faults(&seat).is_empty());
    let _clock = Clock::at(&seat.store, deadline);
    seat.reconciler.reconcile_once().unwrap();
    let first_faults = faults(&seat);
    assert_eq!(first_faults.len(), 1);
    assert_eq!(first_faults[0].body["fields"]["step_run"], selected);
    assert_eq!(first_faults[0].body["evidence"], json!([sent.id]));
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(faults(&seat).len(), 1);
    assert_eq!(
        seat.store.messages(Some(SEAT), true).unwrap().len(),
        1,
        "consumption does not authorize another wake or the queued successor"
    );
    assert_eq!(
        seat.store.step_run(&waiting).unwrap().unwrap().status,
        "ready"
    );
    assert_eq!(seat.wake().as_deref(), Some(selected.as_str()));
}

#[test]
fn a_claimed_step_is_not_reported_as_idle_unclaimed_work() {
    let (_clock, seat) = fixture();
    let run = seat.start("queued", "native-claimed");
    let selected = SeatQueueFixture::step(&run, "work");
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &wake);
    let _clock = Clock::at(&seat.store, START + 1);
    seat.work(&selected, "claim", "native-claim").unwrap();
    observe(&seat, 1, "idle", "seat-one");
    seat.reconciler.reconcile_once().unwrap();
    assert!(faults(&seat).is_empty());
    let held = seat.store.step_run(&selected).unwrap().unwrap();
    assert_eq!(
        seat.reconciler.incremental.next_due("wake:"),
        held.claim_expires_at_unix_ms,
        "the held work lease remains due; it is not an idle-unclaimed wake deadline"
    );
    assert_eq!(held.status, "claimed");
    assert_eq!(seat.store.messages(Some(SEAT), true).unwrap().len(), 1);
}

#[test]
fn a_new_incarnation_gets_its_own_wake_after_native_readiness() {
    let (_clock, seat) = fixture();
    let run = seat.start("queued", "native-restarted");
    let selected = SeatQueueFixture::step(&run, "work");
    let old = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &old);
    let _clock = Clock::at(&seat.store, START + 1);
    seat.reconciler.runtime.ptys.lock().unwrap()[0].incarnation_id = Some("seat-two".into());
    seat.reconciler.reconcile_once().unwrap();
    observe(&seat, 1, "ready", "seat-two");
    seat.reconciler.reconcile_once().unwrap();
    let messages = seat.store.messages(Some(SEAT), true).unwrap();
    assert_eq!(messages.len(), 2);
    let fresh = messages
        .iter()
        .find(|message| message.subject != old.subject)
        .unwrap();
    let (step, _, epoch, incarnation) = work_message_target(fresh).unwrap();
    assert_eq!(step, selected);
    assert_eq!(incarnation, harness_incarnation_key("seat-two"));
    assert_eq!(
        epoch,
        seat.store
            .step_run(&selected)
            .unwrap()
            .unwrap()
            .readiness_epoch
    );
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(seat.store.messages(Some(SEAT), true).unwrap().len(), 2);
    assert!(faults(&seat).is_empty());
}

#[test]
fn blocked_harness_does_not_receive_work_until_native_readiness() {
    let (_clock, seat) = fixture();
    observe(&seat, 1, "blocked", "seat-one");
    let run = seat.start("queued", "native-blocked");
    let selected = SeatQueueFixture::step(&run, "work");
    assert!(seat.store.messages(Some(SEAT), true).unwrap().is_empty());
    let _clock = Clock::at(&seat.store, START + 1);
    observe(&seat, 2, "ready", "seat-one");
    seat.reconciler.reconcile_once().unwrap();
    let messages = seat.store.messages(Some(SEAT), true).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(work_message_target(&messages[0]).unwrap().0, selected);
    assert!(faults(&seat).is_empty());
}
