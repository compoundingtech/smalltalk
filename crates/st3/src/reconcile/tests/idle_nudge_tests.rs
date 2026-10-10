//! A seat that holds a claimed step, has finished its turn and has nothing set to wake it gets one
//! nudge after 30 minutes. A message not yet delivered, or a wait that woke it recently or names
//! a time still to come, holds the nudge; a wait that has gone quiet does not. A watched pull
//! request that leaves the merge queue without merging, and stays out, is nudged at once. The
//! back-off allows one nudge per idle period and, except for an action, one every two hours.
use super::*;
use serde_json::json;

const MINUTE: u128 = 60_000;
const HOUR: u128 = 60 * MINUTE;
const THREAD: &str = "acme/garden#12";

struct Idle {
    seat: SeatQueueFixture,
    start: u128,
    now: std::cell::Cell<u128>,
    held: std::cell::RefCell<Vec<String>>,
}

impl Drop for Idle {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

/// A frozen clock at the real now: step rows are stamped by the real clock.
fn fixture() -> Idle {
    let start = now_ms();
    let store = Arc::new(Store::open_memory("node").unwrap());
    smallclaims::store::set_thread_clock(Some(start));
    store.set_write_clock_at(start).unwrap();
    let seat = SeatQueueFixture::with_store(store);
    Idle {
        seat,
        start,
        now: std::cell::Cell::new(0),
        held: std::cell::RefCell::new(Vec::new()),
    }
}

impl Idle {
    /// Move the clock to `offset` after the start, renewing the held steps every few minutes
    /// on the way, as the seat's driver does while the seat runs.
    fn at(&self, offset: u128) {
        while self.now.get() < offset {
            let next = offset.min(self.now.get() + 4 * MINUTE);
            self.set_clock(next);
            for (index, step) in self.held.borrow().iter().enumerate() {
                self.seat
                    .work(step, "renew", &format!("renew-{index}-{next}"))
                    .unwrap();
            }
        }
        self.set_clock(offset);
    }

    fn set_clock(&self, offset: u128) {
        self.now.set(offset);
        let at = self.start + offset;
        smallclaims::store::set_thread_clock(Some(at));
        self.seat.store.set_write_clock_at(at).unwrap();
    }

    /// Start a run of `mission` and claim its step for the seat, with its wake read.
    fn hold(&self, mission: &str, key: &str) -> String {
        let run = self.seat.start(mission, key);
        let step = SeatQueueFixture::step(&run, "work");
        self.seat
            .work(&step, "claim", &format!("{key}-claim"))
            .unwrap();
        self.held.borrow_mut().push(step.clone());
        self.read_mail();
        step
    }

    fn progress(&self, step: &str, summary: &str) {
        self.seat
            .store
            .work_action(
                step,
                "progress",
                &crate::model::WorkRequest {
                    actor: Some(SEAT.into()),
                    incarnation: Some("seat-one".into()),
                    summary: Some(summary.into()),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("progress-{summary}"),
                },
            )
            .unwrap();
    }

    /// The harness reports `state`, quiet unless it is working.
    fn harness(&self, state: &str) {
        let quiet = state != "working";
        self.seat
            .store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "harness.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String(state.into())),
                    ("driver".into(), Value::String("claude".into())),
                    ("incarnation_id".into(), Value::String("seat-one".into())),
                    ("quiescent".into(), Value::Bool(quiet)),
                    (
                        "blocking".into(),
                        if quiet {
                            json!([])
                        } else {
                            json!(["turn-in-flight"])
                        },
                    ),
                    (
                        "observed_since_ms".into(),
                        Value::from(u64::try_from(now_ms()).unwrap()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    /// Deliver, read and close every message to the seat, as a turn does.
    fn read_mail(&self) {
        for message in self.seat.store.messages(Some(SEAT), false).unwrap() {
            for (kind, status) in [
                ("message.delivered", "delivered"),
                ("message.read", "read"),
                ("message.closed", "closed"),
            ] {
                self.seat
                    .store
                    .append_claim(&ClaimInput {
                        subject: message.subject.clone(),
                        kind: kind.into(),
                        actor: Some(SEAT.into()),
                        fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("{}-{status}", message.subject)),
                    })
                    .unwrap();
            }
        }
    }

    /// Run the seat's wake evaluation.
    fn evaluate(&self) {
        self.seat
            .reconciler
            .reconcile_work_messages(SEAT, "seat-one", None)
            .unwrap();
    }

    fn nudges(&self) -> Vec<crate::model::MessageView> {
        self.seat
            .store
            .messages(Some(SEAT), true)
            .unwrap()
            .into_iter()
            .filter(|message| message.tags.iter().any(|tag| tag == "st3-idle-nudge"))
            .collect()
    }

    fn nudged(&self, step: &str) -> Vec<crate::model::ClaimRecord> {
        self.seat
            .store
            .claims_for(step, Some(crate::store::WORK_NUDGED_KIND))
            .unwrap()
    }

    /// Watch the thread as the seat, from now.
    fn watch(&self) -> String {
        let thread = crate::github_watch::ThreadRef::parse(THREAD).unwrap();
        self.seat.store.declare_watch(&thread, SEAT, None).unwrap();
        thread.watch(SEAT)
    }

    /// The repository observer sees the pull request with `extra` facts.
    fn observe(&self, extra: Value) {
        let thread = crate::github_watch::ThreadRef::parse(THREAD).unwrap();
        let observer = thread.observer();
        let mut pull = json!({
            "number": 12, "title": "Plant the seed beds",
            "url": "https://github.com/acme/garden/pull/12",
            "head": "a".repeat(40), "state": "open", "draft": false,
            "required_checks": {"state": "pending", "source": "rules", "checks": ["build"], "failed": []},
        });
        for (name, value) in extra.as_object().unwrap() {
            pull[name] = value.clone();
        }
        let subscriptions = self
            .seat
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "subscription")
            .filter_map(|item| {
                Some((
                    item.subject.clone(),
                    crate::graph::subscription_spec(&item.desired)?,
                ))
            })
            .filter(|(_, spec)| !spec.stopped)
            .collect::<Vec<_>>();
        let revision = self
            .seat
            .store
            .selected_desired_revision(&observer)
            .unwrap()
            .unwrap();
        self.seat
            .store
            .record_resource_observation(
                &observer,
                &revision,
                None,
                &thread.resource(),
                None,
                &json!({"repository_id": 7, "pull_requests": [pull]}),
                now_ms() + MINUTE,
                &subscriptions,
                None,
            )
            .unwrap();
    }
}

#[test]
fn an_idle_seat_holding_a_step_with_nothing_to_wake_it_is_nudged_once() {
    let idle = fixture();
    let step = idle.hold("queued", "plain");
    idle.progress(&step, "Seed list drafted; waiting on the soil report");
    idle.harness("idle");

    idle.at(29 * MINUTE);
    idle.evaluate();
    assert!(
        idle.nudges().is_empty(),
        "29 minutes is not yet idle-holding"
    );

    idle.at(31 * MINUTE);
    idle.evaluate();
    let nudges = idle.nudges();
    assert_eq!(nudges.len(), 1);
    let content = &nudges[0].content;
    assert!(content.contains(&format!("Step: {step}")), "{content}");
    assert!(
        content.contains("Give the durable seat one step in each run."),
        "{content}"
    );
    assert!(
        content.contains("Last progress (31m ago): Seed list drafted; waiting on the soil report"),
        "{content}"
    );
    assert!(content.contains("idle for 31m"), "{content}");
    assert!(content.contains("set `st gh watch`"), "{content}");
    assert!(
        content.contains(&format!("st work release {step} --as {SEAT}")),
        "{content}"
    );
    let nudged = idle.nudged(&step);
    assert_eq!(nudged.len(), 1);
    assert_eq!(
        nudged[0].body["fields"]["message"],
        nudges[0].subject.as_str()
    );
    assert_eq!(nudged[0].body["fields"]["reason"], "quiet");

    // The same idle period never nudges again, however long it lasts.
    idle.at(5 * HOUR);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1);
}

#[test]
fn the_next_nudge_needs_a_new_turn_and_two_hours() {
    let idle = fixture();
    let step = idle.hold("queued", "backoff");
    idle.harness("idle");
    idle.at(31 * MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1);

    // The nudge wakes the seat; it takes a turn and goes idle again.
    idle.at(32 * MINUTE);
    idle.read_mail();
    idle.harness("working");
    idle.at(33 * MINUTE);
    idle.harness("idle");
    idle.at(70 * MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1, "within two hours of the last nudge");

    idle.at(31 * MINUTE + 2 * HOUR + 1);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 2);
    assert_eq!(idle.nudged(&step).len(), 2);
}

#[test]
fn an_undelivered_message_holds_the_nudge() {
    let idle = fixture();
    idle.hold("queued", "mail");
    idle.harness("idle");
    idle.seat
        .store
        .append_claim(&ClaimInput {
            subject: "message/soil-report".into(),
            kind: "message.sent".into(),
            actor: Some("agent/node.helper".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("agent/node.helper".into())),
                ("to".into(), Value::String(SEAT.into())),
                (
                    "content".into(),
                    Value::String("The soil report is ready.".into()),
                ),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String("Soil report".into())),
                ("in_reply_to".into(), Value::Null),
                ("tags".into(), json!([])),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("soil-report".into()),
        })
        .unwrap();
    idle.at(40 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty(), "the message will wake it");

    idle.read_mail();
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1);
}

#[test]
fn a_watch_holds_the_nudge_until_it_has_not_woken_the_seat_for_two_hours() {
    let idle = fixture();
    let step = idle.hold("queued", "watched");
    idle.watch();
    idle.observe(json!({}));
    idle.harness("idle");

    idle.at(31 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty(), "the watch began 31 minutes ago");

    // The required checks pass: the watch wakes the seat, which reads it and goes idle.
    idle.at(50 * MINUTE);
    idle.observe(json!({"required_checks": {"state": "pass", "source": "rules", "checks": ["build"], "failed": []}}));
    idle.read_mail();
    idle.harness("working");
    idle.at(51 * MINUTE);
    idle.harness("idle");

    // A movement the watch does not deliver, such as entering the merge queue, does not restart
    // its clock.
    idle.at(2 * HOUR);
    idle.observe(json!({
        "required_checks": {"state": "pass", "source": "rules", "checks": ["build"], "failed": []},
        "merge_queue": {"position": 3, "state": "queued"},
    }));
    idle.evaluate();
    assert!(
        idle.nudges().is_empty(),
        "the watch woke the seat 70 minutes ago"
    );

    idle.at(50 * MINUTE + 2 * HOUR + 1);
    idle.evaluate();
    let nudges = idle.nudges();
    assert_eq!(nudges.len(), 1);
    assert!(
        nudges[0]
            .content
            .contains("gh watch acme/garden#12: last moved 2h ago (it woke you)"),
        "{}",
        nudges[0].content
    );
    let waits = &idle.nudged(&step)[0].body["fields"]["waits"];
    assert_eq!(waits[0]["source"], "gh watch");
    assert_eq!(waits[0]["subject"], THREAD);
}

#[test]
fn a_wait_that_is_not_a_pull_request_holds_until_it_goes_quiet() {
    let idle = fixture();
    // The seat holds one step and has another that depends on a step another seat holds.
    let step = idle.hold("queued", "held");
    let gated = idle.seat.start("gated", "gated");
    let prepare = SeatQueueFixture::step(&gated, "prepare");
    idle.read_mail();
    idle.harness("idle");

    idle.at(31 * MINUTE);
    idle.evaluate();
    assert!(
        idle.nudges().is_empty(),
        "the depends-on wait began 31 minutes ago"
    );

    // Another seat's progress on the step it waits for is not delivered here, so it holds
    // nothing up.
    idle.at(2 * HOUR + MINUTE);
    idle.evaluate();
    let nudges = idle.nudges();
    assert_eq!(nudges.len(), 1);
    assert!(
        nudges[0]
            .content
            .contains(&format!("depends-on {prepare}: last moved")),
        "{}",
        nudges[0].content
    );
    assert_eq!(
        idle.nudged(&step)[0].body["fields"]["waits"][0]["source"],
        "depends-on"
    );
}

#[test]
fn a_pull_request_that_leaves_the_merge_queue_nudges_once_it_stays_out() {
    let idle = fixture();
    let step = idle.hold("queued", "queue");
    idle.watch();
    let queued = json!({"merge_queue": {"position": 4, "state": "queued"}});
    idle.observe(queued.clone());
    idle.harness("idle");

    // Out of the queue for two minutes while it rebuilds, then back: not an action.
    idle.at(5 * MINUTE);
    idle.observe(json!({"merge_queue": null}));
    idle.at(7 * MINUTE);
    idle.observe(queued.clone());
    idle.at(13 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty());

    // Out, and it stays out.
    idle.at(14 * MINUTE);
    idle.observe(json!({"merge_queue": null}));
    idle.at(18 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty(), "not settled yet");
    idle.at(19 * MINUTE + 1);
    idle.evaluate();
    let nudges = idle.nudges();
    assert_eq!(
        nudges.len(),
        1,
        "an action does not wait out the idle threshold"
    );
    assert!(
        nudges[0]
            .content
            .starts_with("acme/garden#12 left the merge queue without merging 5m ago"),
        "{}",
        nudges[0].content
    );
    assert!(
        idle.nudged(&step)[0].body["fields"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("left-merge-queue:acme/garden#12:")
    );
    idle.at(40 * MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1, "one nudge per idle period");
}

#[test]
fn a_merged_pull_request_is_not_an_action() {
    let idle = fixture();
    idle.hold("queued", "merged");
    idle.watch();
    idle.observe(json!({"merge_queue": {"position": 1, "state": "queued"}}));
    idle.harness("idle");
    idle.at(10 * MINUTE);
    idle.observe(json!({"merge_queue": null, "state": "closed", "merged": true}));
    idle.read_mail();
    idle.at(20 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty());
}

#[test]
fn a_busy_harness_or_nudges_turned_off_send_nothing() {
    let idle = fixture();
    idle.hold("queued", "busy");
    idle.harness("working");
    idle.at(3 * HOUR);
    idle.evaluate();
    assert!(idle.nudges().is_empty(), "a turn in flight will report");

    idle.harness("idle");
    idle.seat.reconciler.set_idle_nudge(IdleNudgeSettings {
        after_ms: None,
        ..IdleNudgeSettings::default()
    });
    idle.at(5 * HOUR);
    idle.evaluate();
    assert!(idle.nudges().is_empty());
}

#[test]
fn a_settled_idle_check_reads_a_bounded_number_of_rows() {
    let idle = fixture();
    idle.hold("queued", "cost");
    idle.watch();
    idle.observe(json!({}));
    idle.harness("idle");
    idle.at(40 * MINUTE);
    let work = idle.seat.store.work_for_reconcile(SEAT).unwrap();
    let harness = idle.seat.store.current_harness(SEAT).unwrap();
    let capture = smallclaims::sqlite::work::SqliteWorkScope::start();
    idle.seat
        .reconciler
        .nudge_idle_holder(SEAT, &work, harness.as_ref(), now_ms())
        .unwrap();
    let cost = capture.finish();
    eprintln!("idle check cost: {cost:?}");
    assert!(idle.nudges().is_empty(), "the watch holds");
    assert!(
        cost.statements <= 40 && cost.vm_steps <= 2500 && cost.fullscan_steps == 0,
        "the idle check must stay indexed and bounded: {cost:?}"
    );
}

#[test]
fn back_off_and_waits_decide_from_their_times() {
    use super::super::idle_nudge::{action_nudges, step_backoff};
    use super::super::waits::{Wait, WaitAction, WaitSource};
    // No nudge yet: due 30 minutes into the idle period.
    let fresh = step_backoff(1_000, 30 * 60_000, None);
    assert_eq!(fresh.quiet_due, Some(1_000 + 30 * 60_000));
    // Nudged in this idle period: nothing more until a new turn.
    assert_eq!(
        step_backoff(1_000, 30 * 60_000, Some(2_000)).quiet_due,
        None
    );
    // Nudged before this idle period: two hours after that nudge at the earliest.
    let later = step_backoff(10_000, 60_000, Some(5_000));
    assert_eq!(later.quiet_due, Some(5_000 + 2 * 3_600_000));
    // An action is not held by the two hours, only by this idle period and settling.
    let action = WaitAction {
        at_unix_ms: 20_000,
        due_unix_ms: 320_000,
        what: "left".into(),
        key: "left:1".into(),
    };
    assert!(!action_nudges(&later, &action, 319_999));
    assert!(action_nudges(&later, &action, 320_000));
    let after_action = step_backoff(10_000, 60_000, Some(25_000));
    assert!(
        !action_nudges(&after_action, &action, 400_000),
        "nudged since"
    );

    // An expected-by time wins over the quiet window.
    let retry = Wait {
        source: WaitSource::Retry,
        subject: "step-run/g/work".into(),
        last_moved_unix_ms: Some(0),
        last_move: None,
        expected_by_unix_ms: Some(10 * 3_600_000),
        action: None,
    };
    assert_eq!(
        retry.holds_until(2 * 3_600_000, 5 * 3_600_000),
        Some(10 * 3_600_000)
    );
    assert_eq!(retry.holds_until(2 * 3_600_000, 10 * 3_600_000), None);
    let quiet = Wait {
        expected_by_unix_ms: None,
        ..retry
    };
    assert_eq!(
        quiet.holds_until(2 * 3_600_000, 3_600_000),
        Some(2 * 3_600_000)
    );
    assert_eq!(quiet.holds_until(2 * 3_600_000, 2 * 3_600_000), None);
}

#[test]
fn a_person_ask_gate_or_scheduled_retry_holds_an_otherwise_idle_seat() {
    for status in ["waiting-person", "verifying", "ready"] {
        let idle = fixture();
        let held = idle.hold("queued", status);
        idle.harness("idle");
        idle.at(31 * MINUTE);
        let mut work = idle.seat.store.work_for_reconcile(SEAT).unwrap();
        let mut wait = work
            .iter()
            .find(|step| step.subject == held)
            .unwrap()
            .clone();
        wait.subject = format!("step-run/waits/{status}");
        wait.status = status.into();
        wait.claimant = None;
        wait.updated_at_unix_ms = idle.start;
        if status == "waiting-person" {
            wait.blocked_reason = Some("step-run/waits/answer".into());
        }
        if status == "ready" {
            wait.not_before_unix_ms = Some(idle.start + 3 * HOUR);
        }
        work.push(wait);
        let harness = idle.seat.store.current_harness(SEAT).unwrap();
        idle.seat
            .reconciler
            .nudge_idle_holder(SEAT, &work, harness.as_ref(), now_ms())
            .unwrap();
        assert!(idle.nudges().is_empty(), "{status} will wake the seat");
        // Once the supplied wait ends, the held work can be nudged.
        work.pop();
        idle.seat
            .reconciler
            .nudge_idle_holder(SEAT, &work, harness.as_ref(), now_ms())
            .unwrap();
        assert_eq!(idle.nudges().len(), 1, "{status}");
    }
}

#[test]
fn a_run_reporting_to_the_seat_holds_then_expires_without_delivered_progress() {
    let idle = fixture();
    idle.hold("queued", "reported");
    apply_source(
        &idle.seat.store,
        r#"version 2
mission "reporting" state="ready" report-to="agent/node.worker" {
  goal "Tell the gardener when the soil report is ready."
  step "work" { agentless }
}"#,
        "reporting-source",
    );
    idle.seat.start("reporting", "soil-run");
    idle.read_mail();
    idle.harness("idle");
    idle.at(31 * MINUTE);
    idle.evaluate();
    assert!(idle.nudges().is_empty(), "the reporting run is a live wait");
    idle.at(2 * HOUR + MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1);
    assert!(idle.nudges()[0].content.contains("report-to mission-run/"));
}

#[test]
fn idle_lookup_cost_does_not_grow_with_other_seats_watches() {
    let idle = fixture();
    idle.hold("queued", "fleet-cost");
    idle.watch();
    idle.observe(json!({}));
    idle.harness("idle");
    idle.at(40 * MINUTE);
    let work = idle.seat.store.work_for_reconcile(SEAT).unwrap();
    let harness = idle.seat.store.current_harness(SEAT).unwrap();
    let measure = || {
        let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
        idle.seat
            .reconciler
            .nudge_idle_holder(SEAT, &work, harness.as_ref(), now_ms())
            .unwrap();
        scope.finish()
    };
    let baseline = measure();
    let mut source = String::from("version 2\n");
    for index in 100..356 {
        let thread =
            crate::github_watch::ThreadRef::parse(&format!("acme/garden#{index}")).unwrap();
        source.push_str(
            &crate::github_watch::watch_source(&thread, "agent/node.helper", idle.start, None)
                .replace("version 2\n", ""),
        );
    }
    apply_source(&idle.seat.store, &source, "other-watches");
    let grown = measure();
    eprintln!("idle lookup: baseline={baseline:?}; 256 unrelated watches={grown:?}");
    assert_eq!(grown.fullscan_steps, 0);
    assert!(
        grown.vm_steps <= baseline.vm_steps + 50,
        "{baseline:?} -> {grown:?}"
    );
    assert!(idle.nudges().is_empty());
}

#[test]
fn readiness_without_a_turn_does_not_rearm_a_nudge() {
    let idle = fixture();
    idle.hold("queued", "ready-without-turn");
    idle.harness("idle");
    idle.at(31 * MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1);
    idle.read_mail();
    idle.at(3 * HOUR);
    idle.harness("ready");
    idle.at(4 * HOUR);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 1, "a ready report is not another turn");
    idle.harness("working");
    idle.at(4 * HOUR + MINUTE);
    idle.harness("idle");
    idle.at(4 * HOUR + 31 * MINUTE);
    idle.evaluate();
    assert_eq!(idle.nudges().len(), 2);
}

#[test]
fn idle_lookup_cost_is_flat_with_delivered_mail_and_pr_history() {
    let idle = fixture();
    let step = idle.hold("queued", "history-cost");
    let watch = idle.watch();
    for number in 13..17 {
        let thread =
            crate::github_watch::ThreadRef::parse(&format!("acme/garden#{number}")).unwrap();
        idle.seat.store.declare_watch(&thread, SEAT, None).unwrap();
    }
    idle.observe(json!({}));
    idle.at(MINUTE);
    idle.observe(
        json!({"required_checks":{"state":"pass","source":"rules","checks":["build"],"failed":[]}}),
    );
    idle.read_mail();
    idle.harness("idle");
    idle.at(40 * MINUTE);
    let work = idle.seat.store.work_for_reconcile(SEAT).unwrap();
    let harness = idle.seat.store.current_harness(SEAT).unwrap();
    let measure = || {
        let start = std::time::Instant::now();
        let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
        idle.seat
            .reconciler
            .nudge_idle_holder(SEAT, &work, harness.as_ref(), now_ms())
            .unwrap();
        // Also exercise the negative mailbox lookup which a holding quiet wait avoids.
        assert!(!idle.seat.store.has_undelivered_message(SEAT).unwrap());
        (scope.finish(), start.elapsed())
    };
    // Delivered but unread/unclosed messages are retained, so this cannot pass by archiving.
    idle.seat
        .store
        .seed_idle_history(SEAT, idle.start, 0..10_000, 0..2_000);
    let baseline = measure();
    idle.seat
        .store
        .seed_idle_history(SEAT, idle.start, 10_000..20_000, 2_000..4_000);
    let grown = measure();
    let plans = idle.seat.store.idle_lookup_plans(SEAT, &watch).unwrap();
    eprintln!(
        "idle retained-history: 10k delivered / 5 watches / 2k observations = {baseline:?}; 20k / 5 / 4k = {grown:?}; plans={plans:?}"
    );
    assert_eq!(baseline.0.fullscan_steps, 0);
    assert_eq!(grown.0.fullscan_steps, 0);
    assert!(
        grown.0.vm_steps <= baseline.0.vm_steps + 50,
        "{baseline:?} -> {grown:?}"
    );
    assert!(grown.0.statements <= baseline.0.statements + 1);
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("local_idle_messages_pending"))
    );
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("local_idle_messages_wake"))
    );
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("claims_subject_kind_accepted_index"))
    );
    assert!(plans.iter().all(|plan| !plan.contains("TEMP B-TREE")));
    // The aggregate scans at most two one-row subquery results; no retained table is scanned.
    assert!(
        plans
            .iter()
            .all(|plan| !plan.contains("SCAN ") || plan.contains("SCAN (subquery-"))
    );
    assert!(idle.seat.store.latest_nudge(&step).unwrap().is_none());
}
