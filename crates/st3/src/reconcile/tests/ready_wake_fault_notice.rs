//! Exhausted ready work reaches its fault owner once, only while that episode remains live.
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
fn a_consumed_idle_queue_head_reaches_a_fault_owner_after_repeated_full_passes() {
    let (_clock, seat) = fixture();
    apply_source(
        &seat.store,
        r#"version 2
agent "notice-owner" { workspace "/tmp"; command "true"; handles-faults }
"#,
        "idle-notice-owner",
    );
    let owner = "agent/node.notice-owner";
    let member = seat
        .store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|subject| subject.subject == owner)
        .unwrap()
        .member
        .unwrap();
    seat.reconciler
        .runtime
        .ptys
        .lock()
        .unwrap()
        .push(RuntimeObservation {
            runtime_id: member.runtime_id,
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("notice-one".into()),
        });
    seat.reconciler.reconcile_once().unwrap();
    seat.store
        .append_claim(&ClaimInput {
            subject: owner.into(),
            kind: "harness.observed".into(),
            actor: Some(owner.into()),
            fields: BTreeMap::from([
                ("state".into(), json!("ready")),
                ("driver".into(), json!("codex")),
                ("incarnation_id".into(), json!("notice-one")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("notice-owner-ready".into()),
        })
        .unwrap();
    assert_eq!(
        seat.store.fleet_fault_agent().unwrap().as_deref(),
        Some(owner)
    );
    let first = seat.start("queued", "diagnosis-head");
    let selected = SeatQueueFixture::step(&first, "work");
    let _clock = Clock::at(&seat.store, START + 1);
    let second = seat.start("queued", "diagnosis-follower");
    let follower = SeatQueueFixture::step(&second, "work");
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &wake);
    observe(&seat, 1, "idle", "seat-one");
    // Every call advances beyond the existing full-pass interval; no notifier or timer gap.
    for minute in 1..=60 {
        smallclaims::store::set_thread_clock(Some(START + minute * 60_000));
        seat.store
            .set_write_clock_at(START + minute * 60_000)
            .unwrap();
        seat.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(
        faults(&seat).len(),
        1,
        "the five-minute diagnostic was persisted"
    );
    assert_eq!(
        seat.store.step_run(&selected).unwrap().unwrap().status,
        "ready"
    );
    assert_eq!(
        seat.store.step_run(&follower).unwrap().unwrap().status,
        "ready"
    );
    assert_eq!(seat.wake().as_deref(), Some(selected.as_str()));
    assert_eq!(
        seat.store.messages(Some(SEAT), true).unwrap().len(),
        1,
        "the consumed head blocks the follower without changing retry/queue policy"
    );
    assert_eq!(
        now_ms(),
        START + 60 * 60_000,
        "the fault read retains the one-hour observation clock"
    );
    let notices = seat.store.fault_snapshot(now_ms()).unwrap();
    let actionable = notices.iter().find(|fault| {
        fault.item.targets.iter().any(|target| target == &selected)
            && fault.owner.as_deref() == Some(owner)
    });
    let actionable = actionable.expect("exhausted work reaches the configured fault owner");
    let diagnostic = &faults(&seat)[0];
    assert_eq!(actionable.item.step.as_deref(), Some(selected.as_str()));
    assert_eq!(
        actionable.item.mission_run.as_deref(),
        Some(first.subject.as_str())
    );
    assert!(actionable.item.targets.contains(&diagnostic.id));
    assert_eq!(wake_failures(&seat).len(), 1);
    let mail = seat.store.messages(Some(owner), true).unwrap();
    assert_eq!(mail.len(), 1, "one actual fault message after sixty passes");
    assert!(mail[0].content.contains(&selected));
    assert!(
        mail[0]
            .content
            .contains(&format!("/v1/claims/by-id/{}", diagnostic.id))
    );
    assert!(
        !mail[0].content.contains("--after-index"),
        "owner links use immutable identity, not a member-local index"
    );
    assert!(mail[0].content.contains("st work show"));
    assert!(
        seat.store
            .attention_snapshot(Some("person/requester"), now_ms())
            .unwrap()
            .iter()
            .all(|item| !item.targets.contains(&diagnostic.id)),
        "no unrelated person route"
    );
}

#[test]
fn explicit_release_gets_a_fresh_ready_epoch_and_does_not_reuse_closed_acknowledgement() {
    let (_clock, seat) = fixture();
    let run = seat.start("queued", "diagnosis-released-head");
    let selected = SeatQueueFixture::step(&run, "work");
    let old_epoch = seat
        .store
        .step_run(&selected)
        .unwrap()
        .unwrap()
        .readiness_epoch;
    let old = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &old);
    let _clock = Clock::at(&seat.store, START + 1);
    seat.work(&selected, "claim", "diagnosis-before-release")
        .unwrap();
    seat.reconciler.reconcile_once().unwrap();
    let _clock = Clock::at(&seat.store, START + 2);
    seat.work(&selected, "release", "diagnosis-explicit-release")
        .unwrap();
    let fresh = seat.store.step_run(&selected).unwrap().unwrap();
    assert_eq!(fresh.status, "ready");
    assert_eq!(fresh.readiness_epoch, old_epoch + 1);
    seat.reconciler.reconcile_once().unwrap();
    let messages = seat.store.messages(Some(SEAT), true).unwrap();
    assert_eq!(messages.len(), 2);
    let new = messages
        .iter()
        .find(|message| message.subject != old.subject)
        .unwrap();
    let (step, _, epoch, _) = work_message_target(new).unwrap();
    assert_eq!(step, selected);
    assert_eq!(epoch, fresh.readiness_epoch);
    assert_eq!(new.status, "sent");
}

fn wake_failures(seat: &SeatQueueFixture) -> Vec<crate::model::ClaimRecord> {
    seat.store
        .claims_for(SEAT, Some("operational.failure"))
        .unwrap()
        .into_iter()
        .filter(|claim| claim.body["fields"]["condition"] == "work-wake-exhausted")
        .collect()
}

fn notices(seat: &SeatQueueFixture) -> Vec<crate::model::FaultView> {
    seat.store
        .fault_snapshot(now_ms())
        .unwrap()
        .into_iter()
        .filter(|fault| {
            fault
                .item
                .targets
                .iter()
                .any(|target| target.starts_with("step-run/"))
                && fault.item.title == "Ready work remains unclaimed after its wake"
        })
        .collect()
}

fn exhausted() -> (Clock, SeatQueueFixture, String, String) {
    let (clock, seat) = fixture();
    let run = seat.start("queued", "notice-live-run");
    let step = SeatQueueFixture::step(&run, "work");
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &wake);
    observe(&seat, 1, "idle", "seat-one");
    smallclaims::store::set_thread_clock(Some(START + WORK_WAKE_EXHAUST_GRACE_MS));
    seat.store
        .set_write_clock_at(START + WORK_WAKE_EXHAUST_GRACE_MS)
        .unwrap();
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(notices(&seat).len(), 1);
    smallclaims::store::set_thread_clock(Some(START + WORK_WAKE_EXHAUST_GRACE_MS));
    (clock, seat, run.subject, step)
}

#[test]
fn claim_completion_and_release_remove_the_old_episode() {
    for action in ["claim", "complete", "release"] {
        let (_clock, seat, _run, step) = exhausted();
        seat.work(&step, "claim", "notice-claim").unwrap();
        assert!(notices(&seat).is_empty(), "claim ends idle-unclaimed fault");
        if action != "claim" {
            seat.work(&step, action, "notice-after-claim").unwrap();
            assert!(notices(&seat).is_empty(), "{action} removes old episode");
        }
        assert_eq!(
            wake_failures(&seat).len(),
            1,
            "history is retained without recovery flood"
        );
    }
}

#[test]
fn working_and_restart_remove_old_notice_and_new_incarnation_has_its_own_episode() {
    let (_clock, seat, _run, step) = exhausted();
    observe(&seat, 2, "working", "seat-one");
    assert!(notices(&seat).is_empty());
    seat.reconciler.runtime.ptys.lock().unwrap()[0].incarnation_id = Some("seat-two".into());
    seat.reconciler.reconcile_once().unwrap();
    assert!(notices(&seat).is_empty());
    observe(&seat, 1, "ready", "seat-two");
    seat.reconciler.reconcile_once().unwrap();
    let wake = seat
        .store
        .messages(Some(SEAT), false)
        .unwrap()
        .into_iter()
        .find(|message| work_message_target(message).is_some())
        .unwrap();
    assert_eq!(work_message_target(&wake).unwrap().0, step);
    consume(&seat, &wake);
    let _later = Clock::at(&seat.store, START + 2 * WORK_WAKE_EXHAUST_GRACE_MS + 1);
    observe(&seat, 2, "idle", "seat-two");
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(wake_failures(&seat).len(), 2);
    assert_eq!(notices(&seat).len(), 1, "only new incarnation is current");
}

#[test]
fn release_epoch_gets_one_new_episode_without_reusing_old_notice() {
    let (_clock, seat, _run, step) = exhausted();
    seat.work(&step, "claim", "notice-release-claim").unwrap();
    seat.work(&step, "release", "notice-release").unwrap();
    assert!(notices(&seat).is_empty());
    seat.reconciler.reconcile_once().unwrap();
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    consume(&seat, &wake);
    let sent = seat
        .store
        .latest_claim(&wake.subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    let _later = Clock::at(
        &seat.store,
        sent.accepted_at_unix_ms + WORK_WAKE_EXHAUST_GRACE_MS,
    );
    observe(&seat, 2, "idle", "seat-one");
    seat.reconciler.reconcile_once().unwrap();
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(wake_failures(&seat).len(), 2);
    assert_eq!(notices(&seat).len(), 1);
}

#[test]
fn cancellation_stop_and_assignment_away_remove_the_notice() {
    for change in ["cancel", "stop", "assigned-away"] {
        let (_clock, seat, run, step) = exhausted();
        match change {
            "cancel" => {
                seat.store
                    .request_mission_run_cancellation(&run, "notice control")
                    .unwrap();
            }
            "stop" => {
                apply_source(
                    &seat.store,
                    "version 2\nstop \"agent/node.worker\"",
                    "notice-stop",
                );
            }
            "assigned-away" => {
                apply_source(
                    &seat.store,
                    r#"version 2
agent "other" { workspace "/tmp"; command "true" }"#,
                    "notice-other",
                );
                seat.work(&step, "claim", "notice-handoff-claim").unwrap();
                seat.store
                    .handoff_work(
                        &step,
                        &crate::model::WorkHandoffRequest {
                            actor: SEAT.into(),
                            incarnation: Some("seat-one".into()),
                            to: "agent/node.other".into(),
                            note: "Own the step".into(),
                            evidence: vec![],
                            idempotency_key: "notice-handoff".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    seat.store
                        .step_run(&step)
                        .unwrap()
                        .unwrap()
                        .assigned_to
                        .as_deref(),
                    Some("agent/node.other")
                );
            }
            _ => unreachable!(),
        }
        assert!(notices(&seat).is_empty(), "{change} ends the fault");
        assert_eq!(wake_failures(&seat).len(), 1);
    }
}

#[test]
fn stale_attempt_epoch_incarnation_and_missing_diagnostics_do_not_create_owner_items() {
    for mismatch in ["attempt", "readiness_epoch", "incarnation_id", "missing"] {
        let (_clock, seat, _run, step) = exhausted();
        let original = faults(&seat).remove(0);
        let mut fields = original.body["fields"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        if mismatch != "missing" {
            fields.insert(
                mismatch.into(),
                if mismatch == "incarnation_id" {
                    json!("stale-seat")
                } else {
                    json!(999)
                },
            );
        }
        let diagnostic = seat
            .store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "harness.diagnostic".into(),
                actor: None,
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("stale-notice-{mismatch}")),
            })
            .unwrap();
        seat.store
            .record_runtime_failure(
                &format!("stale-notice-{mismatch}"),
                &AttentionRequest {
                    reviewer: SEAT.into(),
                    title: "Ready work remains unclaimed after its wake".into(),
                    reason: "stale evidence control".into(),
                    severity: "error".into(),
                    targets: vec![
                        SEAT.into(),
                        step,
                        if mismatch == "missing" {
                            "missing-claim".into()
                        } else {
                            diagnostic.id
                        },
                    ],
                    actor: RECONCILER_ACTOR.into(),
                    idempotency_key: format!("stale-failure-{mismatch}"),
                },
                "work-wake-exhausted",
            )
            .unwrap();
        assert_eq!(
            notices(&seat).len(),
            1,
            "{mismatch} leaves only the original current episode"
        );
    }
}

#[test]
fn noticing_keeps_the_existing_five_minute_budget_and_survives_reconciler_restart() {
    let (_clock, mut seat) = fixture();
    let run = seat.start("queued", "notice-budget");
    let step = SeatQueueFixture::step(&run, "work");
    let wake = seat.store.messages(Some(SEAT), false).unwrap().remove(0);
    let sent = seat
        .store
        .latest_claim(&wake.subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    consume(&seat, &wake);
    observe(&seat, 1, "idle", "seat-one");
    let deadline = sent.accepted_at_unix_ms + WORK_WAKE_EXHAUST_GRACE_MS;
    let _before = Clock::at(&seat.store, deadline - 1);
    seat.reconciler.reconcile_once().unwrap();
    assert!(faults(&seat).is_empty());
    assert!(wake_failures(&seat).is_empty());
    let _due = Clock::at(&seat.store, deadline);
    seat.reconciler.reconcile_once().unwrap();
    assert_eq!(notices(&seat).len(), 1);
    let failure = wake_failures(&seat).remove(0);
    seat.reconciler = Reconciler::new(
        seat.store.clone(),
        seat.reconciler.runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    for _ in 0..3 {
        seat.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(wake_failures(&seat).len(), 1);
    assert_eq!(wake_failures(&seat)[0].id, failure.id);
    assert_eq!(faults(&seat).len(), 1);
    assert_eq!(seat.wake().as_deref(), Some(step.as_str()));
    assert_eq!(seat.store.messages(Some(SEAT), true).unwrap().len(), 1);
}
