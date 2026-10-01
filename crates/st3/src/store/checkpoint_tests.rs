//! Tests of `smallclaims::store::checkpoint` through smalltalk's store.
use super::*;
use super::checkpoint::*;
use proptest::prelude::*;

const CUT: u128 = 20 * DAY_MS;

/// Builds a sealed set by hand: each claim in its own envelope unless grouped, in canonical
/// order. Every writer also gets a newest envelope that no rule drops, so the newest-envelope
/// guard stays out of the way unless a test wants it.
#[derive(Default)]
struct Sealed {
    claims: Vec<(u128, String, u64, usize, SealedClaim)>,
    envelopes: Vec<SealedEnvelope>,
    sequences: BTreeMap<String, u64>,
    next: u64,
}

struct Draft<'a> {
    kind: &'a str,
    subject: &'a str,
    actor: Option<&'a str>,
    body: Value,
}

fn draft<'a>(kind: &'a str, subject: &'a str, fields: Value) -> Draft<'a> {
    Draft {
        kind,
        subject,
        actor: None,
        body: json!({ "fields": fields }),
    }
}

impl Sealed {
    fn add(&mut self, origin: &str, at: u128, draft: Draft<'_>) -> String {
        self.envelope(origin, at, vec![draft]).remove(0)
    }

    fn envelope(&mut self, origin: &str, at: u128, drafts: Vec<Draft<'_>>) -> Vec<String> {
        let sequence = self.sequences.entry(origin.to_owned()).or_default();
        *sequence += 1;
        let key = EnvelopeKey {
            writer: origin.into(),
            sequence: *sequence,
            envelope_hash: format!("hash-{origin}-{sequence}"),
        };
        self.envelopes.push(SealedEnvelope {
            key: key.clone(),
            accepted_at_unix_ms: at,
            records: drafts.len(),
        });
        let mut ids = Vec::new();
        for (position, draft) in drafts.into_iter().enumerate() {
            self.next += 1;
            let id = format!("claim-{}", self.next);
            let operation = operation_parts(&draft.body)
                .map(|(id, digest)| (id.to_owned(), digest.to_owned()));
            self.claims.push((
                at,
                origin.to_owned(),
                key.sequence,
                position,
                SealedClaim {
                    claim: ClaimRecord {
                        id: id.clone(),
                        store_index: self.next,
                        batch_id: format!("batch/{origin}/{}", key.sequence),
                        subject: draft.subject.into(),
                        kind: draft.kind.into(),
                        origin: origin.into(),
                        actor: draft.actor.map(str::to_owned),
                        operation_id: operation.as_ref().map(|(id, _)| id.clone()),
                        request_digest: operation.map(|(_, digest)| digest),
                        body: draft.body,
                        predecessors: Vec::new(),
                        accepted_at_unix_ms: at,
                    },
                    envelope: key.clone(),
                    valid: true,
                    protected: false,
                },
            ));
            ids.push(id);
        }
        ids
    }

    fn claim_mut(&mut self, id: &str) -> &mut SealedClaim {
        self.claims
            .iter_mut()
            .map(|(.., claim)| claim)
            .find(|claim| claim.claim.id == id)
            .unwrap()
    }

    fn build(mut self) -> SealedSet {
        for writer in self.sequences.keys().cloned().collect::<Vec<_>>() {
            self.add(
                &writer,
                CUT - 1,
                draft("daemon.started", "daemon/filler", json!({})),
            );
        }
        self.claims.sort_by(|left, right| {
            (left.0, &left.1, left.2, left.3).cmp(&(right.0, &right.1, right.2, right.3))
        });
        self.envelopes
            .sort_by(|left, right| left.key.cmp(&right.key));
        SealedSet {
            cut_unix_ms: CUT,
            envelopes: self.envelopes,
            claims: self.claims.into_iter().map(|(.., claim)| claim).collect(),
            seal_rowid: 0,
            envelope_tombstones: Vec::new(),
            claim_tombstones: Vec::new(),
        }
    }
}

fn dropped(plan: &DropPlan) -> BTreeSet<String> {
    plan.claims.iter().map(|claim| claim.id.clone()).collect()
}

fn ids<const N: usize>(ids: [&String; N]) -> BTreeSet<String> {
    ids.into_iter().cloned().collect()
}

const T: u128 = CUT - 10 * DAY_MS;

#[test]
fn latest_rules_keep_the_newest_claim_of_each_slot() {
    let mut sealed = Sealed::default();
    let observer = [1, 2, 3].map(|offset| {
        sealed.add(
            "alder",
            T + offset,
            draft(
                "observer.observed",
                "observer/pulls",
                json!({"status": "ok", "count": offset}),
            ),
        )
    });
    let code_a = [10, 11].map(|offset| {
        sealed.add(
            "alder",
            T + offset,
            draft(
                "daemon.diagnostic",
                "daemon/alder",
                json!({"code": "slow-request", "message": "slow"}),
            ),
        )
    });
    let code_b = sealed.add(
        "alder",
        T + 12,
        draft(
            "daemon.diagnostic",
            "daemon/alder",
            json!({"code": "other", "message": "slow"}),
        ),
    );
    let link = |sealed: &mut Sealed, origin: &str, at| {
        sealed.add(
            origin,
            at,
            draft("transport.observed", "host/cedar", json!({"status": "up"})),
        )
    };
    let from_alder = [
        link(&mut sealed, "alder", T + 20),
        link(&mut sealed, "alder", T + 22),
    ];
    let from_birch = link(&mut sealed, "birch", T + 21);
    let plan = plan_drops(&sealed.build());
    assert_eq!(
        dropped(&plan),
        ids([&observer[0], &observer[1], &code_a[0], &from_alder[0]])
    );
    let _ = (code_b, from_birch);
    assert_eq!(
        plan.by_kind["observer.observed"],
        DropCount {
            sealed: 3,
            dropped: 2
        }
    );
}

#[test]
fn local_kinds_go_only_once_five_days_older_than_the_cut() {
    let mut sealed = Sealed::default();
    let entry = |sealed: &mut Sealed, at| {
        sealed.add(
            "alder",
            at,
            draft(
                "harness.timeline",
                "agent/alder.worker",
                json!({"incarnation_id": "inc-1", "entry": at.to_string()}),
            ),
        )
    };
    let old = [
        entry(&mut sealed, CUT - 6 * DAY_MS),
        entry(&mut sealed, CUT - 6 * DAY_MS + 1),
    ];
    let young = [
        entry(&mut sealed, CUT - 4 * DAY_MS),
        entry(&mut sealed, CUT - 3 * DAY_MS),
    ];
    let action = |sealed: &mut Sealed, actor: Option<&'static str>, at| {
        let mut draft = draft(
            "runtime.action.succeeded",
            "agent/alder.worker",
            json!({"action": "stop", "incarnation_id": "inc-1", "operation_status": "succeeded"}),
        );
        draft.actor = actor;
        sealed.add("alder", at, draft)
    };
    let system = [
        action(&mut sealed, None, CUT - 9 * DAY_MS),
        action(&mut sealed, None, CUT - 8 * DAY_MS),
    ];
    let person = [
        action(&mut sealed, Some("person/avery"), CUT - 9 * DAY_MS + 5),
        action(&mut sealed, Some("person/avery"), CUT - 8 * DAY_MS + 5),
    ];
    let plan = plan_drops(&sealed.build());
    assert_eq!(dropped(&plan), ids([&old[0], &old[1], &system[0]]));
    let _ = (young, person);
}

fn harness(state: &str, reason: Option<&str>) -> Value {
    json!({
        "state": state,
        "incarnation_id": "inc-1",
        "driver": "claude",
        "transport": "claude-channel",
        "reason": reason,
        "blocked_on": null,
        "ask": null,
        "input_buffer": null,
        "exit": null,
    })
}

#[test]
fn harness_rule_keeps_every_position_a_reader_reads() {
    let mut sealed = Sealed::default();
    let agent = "agent/alder.worker";
    let states = [
        ("starting", None),
        ("ready", Some("providerAuth")),
        ("ready", None),
        ("working", None),
        ("idle", None),
        ("working", None),
        ("working", None),
        ("working", None),
    ];
    let claims = states
        .iter()
        .enumerate()
        .map(|(offset, (state, reason))| {
            sealed.add(
                "alder",
                T + offset as u128,
                draft("harness.observed", agent, harness(state, *reason)),
            )
        })
        .collect::<Vec<_>>();
    // A legacy observation without an incarnation stays whatever follows it.
    let legacy = sealed.add(
        "alder",
        T - 1,
        draft("harness.observed", agent, json!({"state": "idle"})),
    );
    let plan = plan_drops(&sealed.build());
    // Kept: the first, the first ready, the first ready without a login prompt, the last
    // idle, every working after it, and the newest.
    assert_eq!(dropped(&plan), ids([&claims[3]]));
    assert!(!dropped(&plan).contains(&legacy));
}

/// `agent_working_since` over one incarnation's states in canonical order: the first
/// `working` after the last other state.
fn working_since(states: &[(Option<String>, u128)]) -> Option<u128> {
    let after = states
        .iter()
        .rposition(|(state, _)| state.as_deref().is_some_and(|state| state != "working"))
        .map_or(0, |position| position + 1);
    states[after..]
        .iter()
        .find(|(state, _)| state.as_deref() == Some("working"))
        .map(|(_, at)| *at)
}

fn harness_state() -> impl Strategy<Value = Option<&'static str>> {
    prop_oneof![
        4 => Just(Some("working")),
        2 => Just(Some("idle")),
        1 => Just(Some("ready")),
        1 => Just(Some("blocked")),
        1 => Just(None),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A late observation from a writer outside the sealed set can land anywhere in an
    /// incarnation's history. The answers read from the kept observations stay the same as
    /// those read from all of them.
    #[test]
    fn late_harness_observations_never_change_what_readers_answer(
        states in proptest::collection::vec(harness_state(), 1..30),
        late in harness_state(),
        late_at in 0u128..40,
    ) {
        let mut sealed = Sealed::default();
        for (offset, state) in states.iter().enumerate() {
            let mut body = json!({"incarnation_id": "inc-1"});
            if let Some(state) = state {
                body["state"] = json!(state);
            }
            sealed.add("alder", T + offset as u128, draft("harness.observed", AGENT, body));
        }
        let set = sealed.build();
        let gone = dropped(&plan_drops(&set));
        let observed = |claim: &ClaimRecord| {
            (field_str(claim, "state").map(str::to_owned), claim.accepted_at_unix_ms)
        };
        let harness = set
            .claims
            .iter()
            .filter(|claim| claim.claim.kind == "harness.observed")
            .collect::<Vec<_>>();
        let full = harness.iter().map(|claim| observed(&claim.claim)).collect::<Vec<_>>();
        let kept = harness
            .iter()
            .filter(|claim| !gone.contains(&claim.claim.id))
            .map(|claim| observed(&claim.claim))
            .collect::<Vec<_>>();
        let ever_ready = |states: &[(Option<String>, u128)]| {
            states.iter().any(|(state, _)| {
                matches!(state.as_deref(), Some("ready" | "working" | "idle"))
            })
        };
        prop_assert_eq!(working_since(&full), working_since(&kept));
        prop_assert_eq!(ever_ready(&full), ever_ready(&kept));
        let late = (late.map(str::to_owned), T + late_at);
        let with_late = |states: &[(Option<String>, u128)]| {
            let mut states = states.to_vec();
            let position = states.partition_point(|state| state.1 <= late.1);
            states.insert(position, late.clone());
            states
        };
        prop_assert_eq!(working_since(&with_late(&full)), working_since(&with_late(&kept)));
        prop_assert_eq!(ever_ready(&with_late(&full)), ever_ready(&with_late(&kept)));
    }
}

#[test]
fn loop_rule_keeps_the_ends_of_each_round_and_the_first_items() {
    let mut sealed = Sealed::default();
    let subject = "loop-run/alder";
    let states = [
        json!({"loop": "loop/a", "status": "running", "round": 1}),
        json!({"loop": "loop/a", "status": "running", "round": 1, "items": ["x"]}),
        json!({"loop": "loop/a", "status": "running", "round": 1}),
        json!({"loop": "loop/a", "status": "running", "round": 1}),
        json!({"loop": "loop/a", "status": "running", "round": 2}),
        json!({"loop": "loop/a", "status": "running", "round": 2}),
        json!({"loop": "loop/a", "status": "running", "round": 2}),
        json!({"loop": "loop/a", "status": "done", "round": 2}),
    ];
    let claims = states
        .into_iter()
        .enumerate()
        .map(|(offset, fields)| {
            sealed.add(
                "alder",
                T + offset as u128,
                draft("loop.state", subject, fields),
            )
        })
        .collect::<Vec<_>>();
    let plan = plan_drops(&sealed.build());
    assert_eq!(dropped(&plan), ids([&claims[2], &claims[5]]));
}

#[test]
fn deferrals_stay_while_their_request_is_open() {
    let mut sealed = Sealed::default();
    let subject = "subscription/pulls";
    let deferral = |sealed: &mut Sealed, request: &str, attempt: u64, at| {
        sealed.add(
            "alder",
            at,
            draft(
                "subscription.mission-deferred",
                subject,
                json!({"request": request, "attempt": attempt, "not_before_unix_ms": at + 60_000}),
            ),
        )
    };
    let closed = [
        deferral(&mut sealed, "claim-r1", 1, T + 1),
        deferral(&mut sealed, "claim-r1", 2, T + 2),
        deferral(&mut sealed, "claim-r1", 3, T + 3),
    ];
    sealed.add(
        "alder",
        T + 4,
        draft(
            "subscription.mission-started",
            subject,
            json!({"request": "claim-r1", "mission_run": "mission-run/x"}),
        ),
    );
    let open = [
        deferral(&mut sealed, "claim-r2", 1, T + 5),
        deferral(&mut sealed, "claim-r2", 2, T + 6),
    ];
    let plan = plan_drops(&sealed.build());
    assert_eq!(dropped(&plan), ids([&closed[0], &closed[1]]));
    let _ = open;
}

fn work(kind: &str, attempt: u64, expires: Option<u128>) -> Value {
    let mut fields =
        json!({"attempt": attempt, "status": "working", "claimant": "agent/alder.worker"});
    if let Some(expires) = expires {
        fields["claim_expires_at_unix_ms"] = json!(expires as u64);
    }
    let _ = kind;
    fields
}

fn step_events(sealed: &mut Sealed, events: &[(&str, u128, Option<u128>)]) -> Vec<String> {
    events
        .iter()
        .map(|(kind, at, expires)| {
            let fields = if kind.starts_with("step-run.") {
                json!({"status": if *expires == Some(0) { "completed" } else { "working" }})
            } else {
                work(kind, 1, *expires)
            };
            sealed.add("alder", T + at, draft(kind, "step-run/s/build", fields))
        })
        .collect()
}

/// No renewal goes. The timing fold closes an interval when the next event arrives after the
/// expiry that stands, so a late lease event from a writer outside the sealed set, landing
/// before a renewal, would make that renewal decide the answer. This is the case proptest
/// found for a rule that dropped renewals the next renewal made redundant.
#[test]
fn renewals_stay_because_a_late_lease_can_need_any_of_them() {
    let mut sealed = Sealed::default();
    step_events(
        &mut sealed,
        &[
            ("work.claimed", 0, Some(T + 30)),
            ("work.renewed", 4, Some(T + 30)),
            ("work.renewed", 18, Some(T + 19)),
        ],
    );
    let set = sealed.build();
    assert!(dropped(&plan_drops(&set)).is_empty());
    let events = set
        .claims
        .iter()
        .map(|claim| timing_event(&claim.claim))
        .collect::<Vec<_>>();
    let late = (
        "work.progress".to_owned(),
        json!({"fields": work("work.progress", 1, Some(T + 4))}),
        T,
    );
    let with_late = |events: &[TimingEvent]| {
        let mut events = events.to_vec();
        events.insert(1, late.clone());
        events
    };
    let without_first_renewal = [events[0].clone(), events[2].clone()];
    assert_eq!(
        fold_step_timing(&with_late(&events), 1, u128::MAX, false),
        (None, 19)
    );
    assert_eq!(
        fold_step_timing(&with_late(&without_first_renewal), 1, u128::MAX, false),
        (None, 4)
    );
}

#[test]
fn the_design_reviews_renewal_schedule_keeps_every_renewal() {
    // Claimed at 0 until 10; renewed at 5 until 20 and at 15 until 30. Without the renewal
    // at 5, the lease would lapse at 10 and the interval would close before 15.
    let mut sealed = Sealed::default();
    let claims = step_events(
        &mut sealed,
        &[
            ("work.claimed", 0, Some(T + 10)),
            ("work.renewed", 5, Some(T + 20)),
            ("work.renewed", 15, Some(T + 30)),
        ],
    );
    let set = sealed.build();
    let plan = plan_drops(&set);
    assert!(dropped(&plan).is_empty(), "{:?}", dropped(&plan));
    let events = set
        .claims
        .iter()
        .map(|claim| timing_event(&claim.claim))
        .collect::<Vec<_>>();
    assert_eq!(fold_step_timing(&events, 1, T + 25, true), (Some(T), 25));
    let _ = claims;
}

#[test]
fn a_renewal_that_shortens_the_lease_stays() {
    let mut sealed = Sealed::default();
    let claims = step_events(
        &mut sealed,
        &[
            ("work.claimed", 0, Some(T + 100)),
            ("work.renewed", 10, Some(T + 50)),
            ("work.renewed", 20, Some(T + 200)),
        ],
    );
    assert!(dropped(&plan_drops(&sealed.build())).is_empty());
    let _ = claims;
}

#[test]
fn guards_keep_claims_a_rule_would_drop() {
    let observed = |at: u128| {
        draft(
            "observer.observed",
            "observer/pulls",
            json!({"status": "ok", "at": at.to_string()}),
        )
    };
    let mut sealed = Sealed::default();
    let mut by_person = observed(1);
    by_person.actor = Some("person/avery");
    let person = sealed.add("alder", T + 1, by_person);
    let invalid = sealed.add("alder", T + 2, observed(2));
    let protected = sealed.add("alder", T + 3, observed(3));
    let cited = sealed.add("alder", T + 4, observed(4));
    let mut with_operation = observed(5);
    with_operation.body["_operation"] =
        json!({"id": "operation/shared", "request_digest": "d"});
    let shared = sealed.add("alder", T + 5, with_operation);
    let mut citing = draft(
        "attention.requested",
        "attention/look",
        json!({"reason": "look"}),
    );
    citing.body["evidence"] = json!([cited]);
    citing.body["_operation"] = json!({"id": "operation/shared", "request_digest": "d"});
    sealed.add("alder", T + 6, citing);
    let plain = sealed.add("alder", T + 7, observed(7));
    let newest = sealed.add("alder", T + 8, observed(8));
    sealed.claim_mut(&invalid).valid = false;
    sealed.claim_mut(&protected).protected = true;
    let plan = plan_drops(&sealed.build());
    assert_eq!(dropped(&plan), ids([&plain]));
    let _ = (person, shared, newest);
}

#[test]
fn each_writers_newest_envelope_before_the_cut_stays() {
    let mut sealed = Sealed::default();
    let first = sealed.add(
        "alder",
        T,
        draft("observer.observed", "observer/a", json!({"status": "ok"})),
    );
    let second = sealed.add(
        "alder",
        T + 1,
        draft("observer.observed", "observer/a", json!({"status": "ok"})),
    );
    let mut set = sealed.build();
    // Take the filler away, so the second observation is in alder's newest envelope.
    set.claims
        .retain(|claim| claim.claim.kind != "daemon.started");
    set.envelopes.pop();
    let plan = plan_drops(&set);
    assert_eq!(dropped(&plan), ids([&first]));
    let _ = second;
}

#[test]
fn an_envelope_goes_only_when_every_claim_in_it_goes() {
    let mut sealed = Sealed::default();
    let both = sealed.envelope(
        "alder",
        T,
        vec![
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
            draft("observer.observed", "observer/b", json!({"status": "ok"})),
        ],
    );
    let half = sealed.envelope(
        "alder",
        T + 1,
        vec![
            draft("observer.observed", "observer/c", json!({"status": "ok"})),
            draft("mission.produced", "mission/x", json!({})),
        ],
    );
    for subject in ["observer/a", "observer/b", "observer/c"] {
        sealed.add(
            "alder",
            T + 2,
            draft("observer.observed", subject, json!({"status": "ok"})),
        );
    }
    let plan = plan_drops(&sealed.build());
    assert_eq!(dropped(&plan), ids([&both[0], &both[1]]));
    assert_eq!(plan.envelopes.len(), 1);
    assert_eq!(plan.envelopes[0].sequence, 1);
    let _ = half;
}

#[test]
fn a_claim_whose_witness_lacks_one_of_its_fields_stays() {
    let mut sealed = Sealed::default();
    let wider = sealed.add(
        "alder",
        T,
        draft(
            "observer.observed",
            "observer/a",
            json!({"status": "ok", "extra": 1}),
        ),
    );
    let narrower = sealed.add(
        "alder",
        T + 1,
        draft("observer.observed", "observer/a", json!({"status": "ok"})),
    );
    let plan = plan_drops(&sealed.build());
    assert!(dropped(&plan).is_empty());
    let _ = (wider, narrower);
}

#[test]
fn later_claims_can_witness_a_claim_field_by_field() {
    let mut sealed = Sealed::default();
    let observed = |fields| draft("observer.observed", "observer/a", fields);
    let both = sealed.add("alder", T, observed(json!({"status": "ok", "cursor": "1"})));
    let status = sealed.add("alder", T + 1, observed(json!({"status": "ok"})));
    let cursor = sealed.add("alder", T + 2, observed(json!({"cursor": "2"})));
    let plan = plan_drops(&sealed.build());
    // The newest carrier of each field stays; the first claim's fields are both set again.
    assert_eq!(dropped(&plan), ids([&both]));
    let _ = (status, cursor);
}

#[test]
fn a_claim_held_in_two_envelopes_stays() {
    let mut sealed = Sealed::default();
    let older = sealed.add(
        "alder",
        T,
        draft("observer.observed", "observer/a", json!({"status": "ok"})),
    );
    let newer = sealed.add(
        "alder",
        T + 1,
        draft("observer.observed", "observer/a", json!({"status": "ok"})),
    );
    let mut set = sealed.build();
    // A second envelope of the same writer and sequence, under its legacy hash, holds the
    // newest claim again.
    let copy = set
        .claims
        .iter()
        .find(|claim| claim.claim.id == newer)
        .unwrap()
        .clone();
    let key = EnvelopeKey {
        envelope_hash: "legacy".into(),
        ..copy.envelope.clone()
    };
    set.envelopes.push(SealedEnvelope {
        key: key.clone(),
        accepted_at_unix_ms: T + 1,
        records: 1,
    });
    let position = set
        .claims
        .iter()
        .position(|claim| claim.claim.id == newer)
        .unwrap();
    set.claims.insert(
        position + 1,
        SealedClaim {
            envelope: key,
            ..copy
        },
    );
    set.envelopes
        .sort_by(|left, right| left.key.cmp(&right.key));
    let plan = plan_drops(&set);
    // Without the guard the first copy of the newest claim would be dropped, witnessed by
    // its own second copy.
    assert_eq!(dropped(&plan), ids([&older]));
    assert_eq!(
        plan.by_kind["observer.observed"],
        DropCount {
            sealed: 2,
            dropped: 1
        }
    );
}

#[test]
fn the_drop_digest_covers_every_tombstone_field() {
    let envelope = EnvelopeTombstone {
        writer: "alder".into(),
        sequence: 4,
        envelope_hash: "hash".into(),
        accepted_at_unix_ms: 10,
    };
    let claim = ClaimTombstone {
        id: "claim-1".into(),
        writer: "alder".into(),
        sequence: 4,
        envelope_hash: "hash".into(),
        subject: "observer/a".into(),
        kind: "observer.observed".into(),
        actor: None,
        predecessors: vec!["claim-0".into(), "claim-00".into()],
        operation_id: Some("operation/a".into()),
        request_digest: Some("digest".into()),
        accepted_at_unix_ms: 10,
    };
    let base = drop_digest(
        std::slice::from_ref(&envelope),
        std::slice::from_ref(&claim),
    );
    let mut changed_claims: Vec<ClaimTombstone> = Vec::new();
    let mut push = |change: fn(&mut ClaimTombstone)| {
        let mut claim = claim.clone();
        change(&mut claim);
        changed_claims.push(claim);
    };
    push(|claim| claim.writer = "birch".into());
    push(|claim| claim.sequence = 5);
    push(|claim| claim.envelope_hash = "other".into());
    push(|claim| claim.subject = "observer/b".into());
    push(|claim| claim.kind = "daemon.diagnostic".into());
    push(|claim| claim.actor = Some("person/avery".into()));
    push(|claim| claim.predecessors.pop().map(drop).unwrap_or_default());
    push(|claim| claim.predecessors.push("claim-000".into()));
    push(|claim| claim.predecessors.reverse());
    push(|claim| claim.operation_id = None);
    push(|claim| claim.operation_id = Some("operation/b".into()));
    push(|claim| claim.request_digest = Some("other".into()));
    push(|claim| claim.request_digest = None);
    push(|claim| claim.accepted_at_unix_ms = 11);
    for changed in changed_claims {
        assert_ne!(
            drop_digest(
                std::slice::from_ref(&envelope),
                std::slice::from_ref(&changed)
            ),
            base,
            "{changed:?}"
        );
    }
    let mut changed = envelope.clone();
    changed.accepted_at_unix_ms = 11;
    assert_ne!(drop_digest(&[changed], std::slice::from_ref(&claim)), base);
    assert_ne!(drop_digest(&[], std::slice::from_ref(&claim)), base);
    assert_ne!(drop_digest(std::slice::from_ref(&envelope), &[]), base);
    // Order does not matter; content does.
    let other = ClaimTombstone {
        id: "claim-2".into(),
        ..claim.clone()
    };
    assert_eq!(
        drop_digest(
            std::slice::from_ref(&envelope),
            &[claim.clone(), other.clone()]
        ),
        drop_digest(&[envelope], &[other, claim])
    );
}

#[test]
fn checkpoint_names_are_utc_days() {
    let cut = checkpoint_cut("2026-09-27").unwrap();
    assert_eq!(checkpoint_name(cut), "checkpoint/2026-09-27");
    assert_eq!(checkpoint_cut("checkpoint/2026-09-27").unwrap(), cut);
    assert_eq!(newest_due_cut(cut + 2 * DAY_MS), cut);
    assert_eq!(newest_due_cut(cut + 3 * DAY_MS - 1), cut);
    assert_eq!(newest_due_cut(cut + 3 * DAY_MS), cut + DAY_MS);
    assert!(checkpoint_cut("yesterday").is_err());
}

#[derive(Clone, Debug)]
enum Step {
    Renew(u128, u128),
    Progress(u128, u128),
    Active(u128),
    Submit(u128),
    Claim(u128, u128),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Renew(gap, lease)),
        2 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Progress(gap, lease)),
        1 => (1u128..40).prop_map(Step::Active),
        1 => (1u128..40).prop_map(Step::Submit),
        1 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Claim(gap, lease)),
    ]
}

fn timing_history(steps: &[Step]) -> Vec<(String, Value, u128)> {
    let mut at = T;
    let mut events = vec![(
        "work.claimed".to_owned(),
        json!({"fields": work("", 1, Some(T + 30))}),
        T,
    )];
    for step in steps {
        let (kind, gap, lease) = match step {
            Step::Renew(gap, lease) => ("work.renewed", *gap, Some(*lease)),
            Step::Progress(gap, lease) => ("work.progress", *gap, Some(*lease)),
            Step::Active(gap) => ("step-run.state", *gap, None),
            Step::Submit(gap) => ("work.submitted", *gap, None),
            Step::Claim(gap, lease) => ("work.claimed", *gap, Some(*lease)),
        };
        at += gap;
        let fields = if kind == "step-run.state" {
            json!({"status": "working"})
        } else {
            work(kind, 1, lease.map(|lease| at + lease))
        };
        events.push((kind.to_owned(), json!({ "fields": fields }), at));
    }
    events
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Dropped renewals never change a step's timing, now or after any late event of the
    /// same attempt is inserted anywhere in the history.
    #[test]
    fn dropped_renewals_never_change_step_timing(
        steps in proptest::collection::vec(step_strategy(), 1..30),
        late in step_strategy(),
        late_at in 0u128..1_200,
    ) {
        let history = timing_history(&steps);
        let mut sealed = Sealed::default();
        for (kind, body, at) in &history {
            sealed.add("alder", *at, Draft { kind, subject: "step-run/s/build", actor: None, body: body.clone() });
        }
        let set = sealed.build();
        let plan = plan_drops(&set);
        let gone = dropped(&plan);
        let kept = set.claims.iter().filter(|claim| !gone.contains(&claim.claim.id)).map(|claim| timing_event(&claim.claim)).collect::<Vec<_>>();
        let full = set.claims.iter().map(|claim| timing_event(&claim.claim)).collect::<Vec<_>>();
        for snapshot in [T + 50, T + 400, CUT, u128::MAX] {
            for active in [true, false] {
                prop_assert_eq!(fold_step_timing(&full, 1, snapshot, active), fold_step_timing(&kept, 1, snapshot, active));
            }
        }
        // A late event from a writer that did not take part.
        let late = timing_history(&[late]).pop().unwrap();
        let late = (late.0, late.1, T + late_at);
        let with_late = |events: &[(String, Value, u128)]| {
            let mut events = events.to_vec();
            let position = events.partition_point(|event| event.2 <= late.2);
            events.insert(position, late.clone());
            events
        };
        for snapshot in [T + 400, u128::MAX] {
            for active in [true, false] {
                prop_assert_eq!(
                    fold_step_timing(&with_late(&full), 1, snapshot, active),
                    fold_step_timing(&with_late(&kept), 1, snapshot, active)
                );
            }
        }
    }
}

fn input(
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: Value,
    key: &str,
) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: actor.map(str::to_owned),
        fields: fields
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    }
}

const AGENT: &str = "agent/alder.worker";

/// Claims of several rules' kinds, with some that a checkpoint drops.
fn write_history(store: &Store) {
    store
        .append_claim(&input(
            AGENT,
            "runtime.observed",
            None,
            json!({"status": "running", "incarnation_id": "inc-1"}),
            "runtime",
        ))
        .unwrap();
    for (n, state) in ["idle", "working", "idle", "working", "idle", "working"]
        .iter()
        .enumerate()
    {
        store
            .append_claim_outcome(&input(
                AGENT,
                "harness.observed",
                Some(AGENT),
                json!({"state": state, "incarnation_id": "inc-1", "observed_at_ms": n}),
                &format!("harness-{n}"),
            ))
            .unwrap();
    }
    for n in 0..4 {
        store
            .append_claim(&input(
                "daemon/alder",
                "daemon.diagnostic",
                None,
                json!({"severity": "warning", "code": "slow-request", "reason": format!("slow {n}")}),
                &format!("diagnostic-{n}"),
            ))
            .unwrap();
    }
}

fn receive(target: &Store, relay: &str, exchange: &ReplicationExchange) {
    target.bind_fleet("fleet/test").unwrap();
    target
        .receive_replication_exchange(relay, "fleet/test", exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    target.project_replication_backlog().unwrap();
}

fn only(
    exchange: &ReplicationExchange,
    envelopes: Vec<ReplicaEnvelope>,
) -> ReplicationExchange {
    ReplicationExchange {
        inventory: ReplicationInventory {
            digest: String::new(),
            envelopes: envelopes
                .iter()
                .map(|envelope| ReplicaEnvelopeId {
                    writer: envelope.writer.clone(),
                    sequence: envelope.sequence,
                    hash: envelope.hash.clone(),
                })
                .collect(),
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        },
        envelopes,
        ..exchange.clone()
    }
}

#[test]
fn the_proof_passes_for_the_plan_and_fails_for_a_drop_a_reader_needs() {
    let store = Store::open_memory("alder").unwrap();
    write_history(&store);
    let scratch = tempfile::tempdir().unwrap();
    let cut = now_ms() + 1_000;
    let (plan, proof) = store.plan_checkpoint(cut, scratch.path()).unwrap();
    assert!(proof.passed, "{proof:?}");
    assert_eq!(proof.graph_digest_before, proof.graph_digest);
    assert_eq!(proof.reader_digest_before, proof.reader_digest);
    assert!(
        plan.claims
            .iter()
            .any(|claim| claim.kind == "harness.observed")
            && plan
                .claims
                .iter()
                .any(|claim| claim.kind == "daemon.diagnostic"),
        "{plan:?}"
    );
    assert_eq!(proof.graph_digest, proof.graph_digest_before);
    assert_eq!(proof.reader_digest, proof.reader_digest_before);

    let before = store.claims_for(AGENT, None).unwrap().len();
    let sealed = store.checkpoint_sealed_set(cut).unwrap();
    let newest = sealed
        .claims
        .iter()
        .rev()
        .find(|claim| claim.claim.kind == "harness.observed")
        .unwrap();
    let mut wrong = plan.clone();
    wrong.claims.push(claim_tombstone(newest));
    let copy = scratch.path().join("wrong.sqlite3");
    store.copy_store_to(&copy).unwrap();
    let proof = prove_on_copy(&copy, &sealed, &wrong).unwrap();
    assert!(!proof.passed);
    assert!(
        proof
            .mismatches
            .iter()
            .any(|mismatch| mismatch.starts_with(&format!("{AGENT} "))),
        "{:?}",
        proof.mismatches
    );
    // The live store is never changed by a proof.
    assert_eq!(store.claims_for(AGENT, None).unwrap().len(), before);
}

#[test]
fn nodes_that_hold_the_same_claims_plan_the_same_drop() {
    let source = Store::open_memory("alder").unwrap();
    write_history(&source);
    source.bind_fleet("fleet/test").unwrap();
    let exchange = source
        .export_replication_exchange("fleet/test", &ReplicationInventory::default())
        .unwrap();
    assert!(exchange.envelopes.len() > 5);
    let in_order = Store::open_memory("birch").unwrap();
    receive(&in_order, "alder", &exchange);
    let reversed = Store::open_memory("cedar").unwrap();
    for envelope in exchange.envelopes.iter().rev() {
        receive(&reversed, "alder", &only(&exchange, vec![envelope.clone()]));
    }
    let cut = now_ms() + 1_000;
    let plans = [&source, &in_order, &reversed]
        .map(|store| plan_drops(&store.checkpoint_sealed_set(cut).unwrap()));
    assert!(!plans[0].claims.is_empty());
    for plan in &plans[1..] {
        assert_eq!(plan.sealed_digest, plans[0].sealed_digest);
        assert_eq!(plan.drop_digest, plans[0].drop_digest);
        assert_eq!(plan.retained_digest, plans[0].retained_digest);
        assert_eq!(plan.claims, plans[0].claims);
    }
    // The proofs agree too, although the nodes numbered their claims differently.
    let scratch = tempfile::tempdir().unwrap();
    let proofs = [&source, &in_order, &reversed]
        .map(|store| store.plan_checkpoint(cut, scratch.path()).unwrap().1);
    for proof in &proofs {
        assert!(proof.passed, "{proof:?}");
        assert_eq!(proof.reader_digest_before, proof.reader_digest);
        assert_eq!(proof.graph_digest, proofs[0].graph_digest);
        assert_eq!(proof.reader_digest, proofs[0].reader_digest);
    }
}

/// After a trim, the dropped envelopes are tombstones. A later checkpoint still seals their
/// identities, and its drop digest still covers them, so a node that trimmed and one that
/// has not yet agree on both.
#[test]
fn a_trimmed_node_seals_and_digests_like_an_untrimmed_one() {
    let trimmed = Store::open_memory("alder").unwrap();
    write_history(&trimmed);
    trimmed.bind_fleet("fleet/test").unwrap();
    let untrimmed = Store::open_memory("birch").unwrap();
    receive(
        &untrimmed,
        "alder",
        &trimmed
            .export_replication_exchange("fleet/test", &ReplicationInventory::default())
            .unwrap(),
    );
    let first = now_ms() + 1_000;
    let plan = plan_drops(&trimmed.checkpoint_sealed_set(first).unwrap());
    assert!(!plan.claims.is_empty());
    {
        let mut connection = trimmed.connection.write();
        let transaction = connection.transaction().unwrap();
        let checkpoint = checkpoint_name(first);
        record_checkpoint_tombstones_tx(
            &transaction,
            &checkpoint,
            &plan.envelopes,
            &plan.claims,
        )
        .unwrap();
        delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims).unwrap();
        transaction.commit().unwrap();
    }
    let second = first + DAY_MS;
    let [after_trim, without_trim] =
        [&trimmed, &untrimmed].map(|store| store.checkpoint_sealed_set(second).unwrap());
    assert_eq!(sealed_digest(&after_trim), sealed_digest(&without_trim));
    // A seal reads only the identities, and they give the same digest as the whole set.
    for (store, set) in [(&trimmed, &after_trim), (&untrimmed, &without_trim)] {
        assert_eq!(
            store.checkpoint_sealed_identities(second, None).unwrap(),
            SealedIdentities::of(set)
        );
    }
    assert_eq!(after_trim.envelope_tombstones.len(), plan.envelopes.len());
    assert!(after_trim.claims.len() < without_trim.claims.len());
    let [after_trim, without_trim] = [after_trim, without_trim].map(|set| plan_drops(&set));
    assert!(after_trim.claims.is_empty(), "{:?}", after_trim.claims);
    assert_eq!(after_trim.drop_digest, plan.drop_digest);
    assert_eq!(without_trim.drop_digest, plan.drop_digest);
}

/// Nearly every claim cites the claim before it on its subject. A walk from a runtime's
/// newest observation back to another writer's older one must pass through a dropped claim
/// by its tombstone, or the status would show a runtime conflict that is not there.
#[test]
fn ancestry_walks_through_a_dropped_claim() {
    let birch = Store::open_memory("birch").unwrap();
    let older = birch
        .append_claim(&input(
            AGENT,
            "runtime.observed",
            None,
            json!({"status": "running", "incarnation_id": "inc-1"}),
            "birch-runtime",
        ))
        .unwrap();
    birch.bind_fleet("fleet/test").unwrap();
    let alder = Store::open_memory("alder").unwrap();
    receive(
        &alder,
        "birch",
        &birch
            .export_replication_exchange("fleet/test", &ReplicationInventory::default())
            .unwrap(),
    );
    let middle = alder
        .append_claim(&input(
            AGENT,
            "harness.observed",
            Some(AGENT),
            json!({"state": "working", "incarnation_id": "inc-1"}),
            "alder-harness",
        ))
        .unwrap();
    let newest = alder
        .append_claim(&input(
            AGENT,
            "runtime.observed",
            None,
            json!({"status": "running", "incarnation_id": "inc-1"}),
            "alder-runtime",
        ))
        .unwrap();
    assert_eq!(middle.predecessors, std::slice::from_ref(&older.id));
    assert_eq!(newest.predecessors, std::slice::from_ref(&middle.id));
    let source = |store: &Store| {
        selected_actual_source_at(&store.readers.get(), AGENT, None, None).unwrap()
    };
    assert_eq!(
        source(&alder),
        (Some(newest.id.clone()), Some("alder".into()), false)
    );

    let tombstone = ClaimTombstone {
        id: middle.id.clone(),
        writer: "alder".into(),
        sequence: 0,
        envelope_hash: String::new(),
        subject: middle.subject.clone(),
        kind: middle.kind.clone(),
        actor: middle.actor.clone(),
        predecessors: middle.predecessors.clone(),
        operation_id: middle.operation_id.clone(),
        request_digest: middle.request_digest.clone(),
        accepted_at_unix_ms: middle.accepted_at_unix_ms,
    };
    {
        let mut connection = alder.connection.write();
        let transaction = connection.transaction().unwrap();
        record_checkpoint_tombstones_tx(
            &transaction,
            "checkpoint/test",
            &[],
            std::slice::from_ref(&tombstone),
        )
        .unwrap();
        // Recording twice changes nothing.
        record_checkpoint_tombstones_tx(
            &transaction,
            "checkpoint/test",
            &[],
            std::slice::from_ref(&tombstone),
        )
        .unwrap();
        delete_dropped_rows_tx(&transaction, &[], std::slice::from_ref(&tombstone)).unwrap();
        assert!(claim_descends_from(&transaction, &newest.id, &older.id).unwrap());
        transaction.commit().unwrap();
    }
    assert!(alder.claim_by_id(&middle.id).unwrap().is_none());
    assert_eq!(
        source(&alder),
        (Some(newest.id.clone()), Some("alder".into()), false)
    );
}

/// Plans a checkpoint over a copy of a real store and prints the dry run:
/// `ST3_CHECKPOINT_STORE=/var/tmp/copy.sqlite3 ST3_CHECKPOINT_ORIGIN=example-linux
/// ST3_CHECKPOINT_DAY=2026-09-27 cargo test -p st3 --lib plan_a_copy_of_a_real_store --
/// --ignored --nocapture`. The copy is changed; the store it came from is not read.
#[test]
#[ignore = "reads the store copy named by ST3_CHECKPOINT_STORE"]
fn plan_a_copy_of_a_real_store() {
    let path = PathBuf::from(std::env::var("ST3_CHECKPOINT_STORE").unwrap());
    let origin =
        std::env::var("ST3_CHECKPOINT_ORIGIN").unwrap_or_else(|_| "example-linux".into());
    let day = std::env::var("ST3_CHECKPOINT_DAY").unwrap();
    let started = std::time::Instant::now();
    let store = Store::open(&path, origin).unwrap();
    eprintln!("opened in {:?}", started.elapsed());
    let scratch = path.parent().unwrap().join("proof");
    let plan = if std::env::var("ST3_CHECKPOINT_SKIP_PROOF").is_ok() {
        None
    } else {
        Some(
            store
                .checkpoint_plan_view(checkpoint_cut(&day).unwrap(), &scratch)
                .unwrap(),
        )
    };
    eprintln!("planned and proved in {:?}", started.elapsed());
    println!("{}", serde_json::to_string_pretty(&plan).unwrap());
    if let Ok(subject) = std::env::var("ST3_CHECKPOINT_SUBJECT") {
        let sealed = store
            .checkpoint_sealed_set(checkpoint_cut(&day).unwrap())
            .unwrap();
        let plan = plan_drops(&sealed);
        let dropped = dropped(&plan);
        for claim in sealed
            .claims
            .iter()
            .filter(|claim| claim.claim.subject == subject)
        {
            println!(
                "{} {} {} valid={} protected={} envelope={}/{} dropped={}",
                claim.claim.store_index,
                claim.claim.kind,
                claim.claim.id,
                claim.valid,
                claim.protected,
                claim.envelope.writer,
                claim.envelope.sequence,
                dropped.contains(&claim.claim.id)
            );
        }
    }
}
