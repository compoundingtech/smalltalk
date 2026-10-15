//! Hosted-only controls. These fixtures do not sample or mutate the host daemon.
use super::*;
use crate::store::fleet_admission_tests::{admit, key, node, sync};
use st3_schema::slo_breaches::ProcessDelta;
use std::time::{Duration, Instant};

const SOURCE_START: u64 = 1_600_000_000_000;

fn source() -> Store {
    let identity = key();
    smallclaims::store::set_thread_clock(Some(u128::from(SOURCE_START)));
    let store = node("alder", Some(&identity), Some(&identity));
    smallclaims::store::set_thread_clock(None);
    admit(&store, "alder", &identity, "anchor", None);
    store.project_replication_backlog().unwrap();
    store.fleet_membership().unwrap();
    store
}

fn declare_owner(store: &Store) {
    let source = "version 2\nagent \"fleet/smalltalk/speed\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }";
    let intent = crate::graph::parse_test_intent(source, "alder").unwrap();
    let planned = store.mission(&intent, IntentInput { kdl: source.into(), source_name: None }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "declare-owner").unwrap();
    store.append_claim(&ClaimInput { subject: DEFAULT_OWNER.into(), kind: "harness.observed".into(), actor: Some(DEFAULT_OWNER.into()),
        fields: serde_json::from_value(json!({"status":"ready","driver":"codex","incarnation_id":"prefix-control"})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None }).unwrap();
}

fn reading(store: &Store, start: u64, value: Option<f64>) -> Observation {
    let (epoch, base) = store.slo_source_epoch();
    let source_cut = format!("{epoch}/{start}");
    let predecessor_cut = (start>0).then(|| format!("{epoch}/{}",start-3_600_000));
    Observation {
        member: store.origin.clone(), epoch, epoch_started_ms: base, sequence: start/3_600_000+1,
        predecessor_cut,
        version: "person-read/v1".into(), start_ms: base + start, end_ms: base + start + 3_600_000,
        observed_at_ms: base + start + 3_600_000, window_ms: 3_600_000, covered_ms: 3_600_000,
        complete: true, population: "all eligible requests".into(), source_cut,
        checks: vec![Check { unit: "ms".into(), value, target: 100.0, minimum: false }],
        counters: BTreeMap::new(),
    }
}

fn miss(store: &Store, objective: &str) -> String {
    store.record_slo_miss(objective, None, reading(store, 0, Some(120.0)), None).unwrap().unwrap()
}

fn claims(store: &Store) -> Vec<ClaimRecord> {
    store.readers.get().prepare("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims ORDER BY store_index")
        .unwrap().query_map([], claim_from_row).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

// A partial-admission fixture must preserve the source claim IDs: the ordinary
// pinned-resource gate binds the immutable onset, not a reauthored lookalike.
// Signed admission itself is covered separately by the actual sync controls.
fn admit_fixture_claim(source: &Store, target: &Store, claim: &ClaimRecord) {
    let header = source.readers.get().query_row(
        "SELECT origin,replica_sequence,previous_hash,hash,accepted_at_unix_ms FROM batches WHERE id=?1",
        [&claim.batch_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,
            row.get::<_,Option<String>>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?))).unwrap();
    target.connection.batched(|tx| {
        tx.execute("INSERT OR IGNORE INTO batches(id,origin,replica_sequence,previous_hash,hash,accepted_at_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6)",
            params![claim.batch_id,header.0,header.1,header.2,header.3,header.4])?;
        smallclaims::store::insert_claim(tx,&claim.id,&claim.batch_id,&claim.subject,&claim.kind,
            &claim.origin,claim.actor.as_deref(),&claim.body,&claim.predecessors,claim.accepted_at_unix_ms)?;
        Ok::<_,anyhow::Error>(())
    }).unwrap().unwrap();
}

fn fact_state(store: &Store, episode: &str) -> String {
    store.latest_actual_value(&format!("resource/slo-breach/{episode}")).unwrap()
        .and_then(|value| value["facts"]["status"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn request(key: &str) -> WorkRequest {
    WorkRequest { actor: Some(DEFAULT_OWNER.into()), incarnation: Some("prefix-control".into()),
        summary: None, reason: None, evidence: vec![], wait_ms: None, idempotency_key: key.into() }
}

#[test]
fn unknown_partial_and_nonfinite_readings_cannot_open_a_breach() {
    let store = source();
    for (index, mut observation) in [reading(&store, 0, None), reading(&store, 0, Some(80.0))].into_iter().enumerate() {
        assert!(store.record_slo_miss(&format!("fixture/{index}"), None, observation.clone(), None).unwrap().is_none());
        observation.complete = false;
        observation.checks[0].value = Some(120.0);
        assert!(store.record_slo_miss(&format!("partial/{index}"), None, observation, None).unwrap().is_none());
    }
    assert!(store.record_slo_miss("person-read", None, reading(&store, 0, Some(f64::NAN)), None).is_err());
    assert!(store.prepare_slo_missions("/tmp").unwrap().is_empty());
}

#[test]
fn one_onset_survives_preparation_replay_and_credential_rotation() {
    let store = source();
    let episode = miss(&store, "person-read");
    let count = claims(&store).len();
    assert_eq!(miss(&store, "person-read"), episode);
    assert_eq!(claims(&store).len(), count, "repeated misses are not sample claims");
    assert_eq!(store.prepare_slo_missions("/tmp").unwrap().len(), 1);
    store.connection.batched(|tx| rebuild(tx)).unwrap().unwrap();
    assert_eq!(miss(&store, "person-read"), episode);
    let prepared = store.prepare_slo_missions("/tmp").unwrap().remove(0);
    store.set_member_key(Some(key())).unwrap();
    assert!(store.dispatch_slo_mission(prepared).is_err(), "stale preparation cannot cross key rotation");
    assert_eq!(head(&store.readers.get(), &store.bound_fleet().unwrap().unwrap(), "alder", "person-read").unwrap().unwrap().0,
        episode, "a key is not episode identity");
}

#[test]
fn activation_frontier_is_durable_bounded_and_rate_limited() {
    let store = source();
    for objective in 0..MAX_OBJECTIVES { miss(&store, &format!("objective/{objective}")); }
    assert!(store.record_slo_miss("objective/65", None, reading(&store, 0, Some(120.0)), None).is_err());
    let prepared = store.prepare_slo_missions("/tmp").unwrap();
    assert_eq!(prepared.len(), 2);
    for pending in prepared { store.dispatch_slo_mission(pending).unwrap(); }
    let pending = store.prepare_slo_missions("/tmp").unwrap().remove(0);
    assert!(store.dispatch_slo_mission(pending).is_err(), "third opening in the minute is retained pending");
    store.connection.batched(|tx| rebuild(tx)).unwrap().unwrap();
    assert_eq!(store.prepare_slo_missions("/tmp").unwrap().len(), 2);
    let count: usize = store.readers.get().query_row("SELECT COUNT(*) FROM slo_pending WHERE mission_run IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 62, "restart/replay cannot lose the later obligations");
}

#[test]
fn recovery_uses_the_objectives_actual_window_and_preserves_the_fix() {
    let store = source();
    declare_owner(&store);
    let episode = miss(&store, "person-read");
    let run = store.dispatch_slo_mission(store.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    let fix = store.work(None, true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
    store.set_step_state(&fix.subject, "ready", None).unwrap();
    store.work_action(&fix.subject, "claim", &request("active-fix")).unwrap();
    assert_eq!(fact_state(&store, &episode), "ready");
    let before = store.work(None, true).unwrap();
    let mut stale = reading(&store, 0, Some(80.0));
    assert!(store.recover_slo_breach(&episode, stale.clone(), None).is_err());
    stale = reading(&store, 3_600_000, Some(80.0));
    store.recover_slo_breach(&episode, stale, None).unwrap();
    assert_eq!(fact_state(&store, &episode), "recovered");
    assert!(execution_admitted(&store.readers.get(), &run).unwrap(), "an admitted fix remains admitted");
    let after = store.work(None, true).unwrap();
    assert_eq!(before.iter().map(|step| (&step.subject, &step.status, &step.claimant)).collect::<Vec<_>>(),
        after.iter().map(|step| (&step.subject, &step.status, &step.claimant)).collect::<Vec<_>>());
    store.work_action(&fix.subject, "renew", &request("renew-active-after-health")).unwrap();
    assert!(store.record_slo_miss("person-read", None, reading(&store, 0, Some(120.0)), None).is_err(), "a stale miss must not start a later episode");
}

#[test]
fn a_new_breach_requires_the_previous_recovery_and_one_mission() {
    let store = source();
    let first = miss(&store, "person-read");
    store.recover_slo_breach(&first, reading(&store, 3_600_000, Some(80.0)), None).unwrap();
    let second = store.record_slo_miss("person-read", None, reading(&store, 7_200_000, Some(120.0)), None).unwrap().unwrap();
    assert_eq!(store.prepare_slo_missions("/tmp").unwrap().len(), 2, "both episode obligations stay durable");
    let old = store.prepare_slo_missions("/tmp").unwrap().remove(0);
    assert_eq!(old.record.episode, first);
    store.dispatch_slo_mission(old).unwrap();
    assert_ne!(first, second);
    assert!(store.recover_slo_breach(&first, reading(&store, 10_800_000, Some(80.0)), None).is_ok(), "duplicate recovery is a no-op");
    assert_eq!(fact_state(&store, &second), "pending");
}

#[test]
fn owner_absence_is_named_once_without_fallback_or_sample_writes() {
    let store = source();
    let episode = miss(&store, "person-read");
    store.dispatch_slo_mission(store.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    assert_eq!(fact_state(&store, &episode), "owner_unavailable");
    let (_, blocked) = event(&store.readers.get(), &episode, Event::OwnerBlocked).unwrap().unwrap();
    assert_eq!(blocked.owner, DEFAULT_OWNER);
    assert!(store.unblock_slo_owner(&episode).is_err());
    let count = claims(&store).len();
    assert!(store.prepare_slo_missions("/tmp").unwrap().is_empty());
    assert_eq!(claims(&store).len(), count);
    assert!(store.work(None, true).unwrap().iter().filter(|step| step.step == "fix")
        .all(|step| step.assigned_to.as_deref() == Some(DEFAULT_OWNER)));
}

fn cpu_witness(reading: &Observation, cores: f64) -> HealthyWitness {
    let hour = 3_600_000_000_000u64;
    let start = (reading.start_ms-reading.epoch_started_ms)*1_000_000;
    HealthyWitness { source_cut: reading.source_cut.clone(), start_ms: reading.start_ms, end_ms: reading.end_ms, expected_workers: 1,
        monotonic_start_ns: start, monotonic_end_ns: start+hour, boundary_uncertainty_ns: 0,
        complete_cohort: true, positively_idle: true, processes: vec![
            ProcessDelta { role: "daemon".into(), pid: 10, birth: 1, cpu_start_ns: 0, cpu_end_ns: (cores * hour as f64) as u64 },
            ProcessDelta { role: "replication-worker".into(), pid: 11, birth: 2, cpu_start_ns: 0, cpu_end_ns: 0 },
        ] }
}

#[test]
fn idle_cpu_requires_full_eligible_cohort_and_consistent_counters() {
    let store = source();
    let mut observation = reading(&store, 0, Some(0.6));
    observation.checks[0].unit = "cores".into(); observation.checks[0].target = 0.5;
    observation.version = "combined-idle/v1".into();
    assert!(store.record_slo_miss("cpu/idle", None, observation.clone(), None).is_err());
    let mut missing = cpu_witness(&observation, 0.6); missing.processes.pop(); missing.complete_cohort = false;
    let mut duplicate = cpu_witness(&observation, 0.6); duplicate.processes[1] = duplicate.processes[0].clone();
    let mut birth_unknown = cpu_witness(&observation, 0.6); birth_unknown.processes[1].birth = 0;
    let mut short = cpu_witness(&observation, 0.6); short.monotonic_end_ns -= 1;
    for witness in [missing, duplicate, birth_unknown, short] {
        assert!(store.record_slo_miss("cpu/idle", None, observation.clone(), Some(witness)).is_err());
    }
    let mut busy = cpu_witness(&observation, 0.6); busy.positively_idle = false;
    assert!(store.record_slo_miss("cpu/idle", None, observation.clone(), Some(busy)).is_err());
    let mut reset = cpu_witness(&observation, 0.6); reset.processes[1].cpu_start_ns = 1;
    assert!(store.record_slo_miss("cpu/idle", None, observation.clone(), Some(reset)).is_err());
    let episode = store.record_slo_miss("cpu/idle", None, observation.clone(), Some(cpu_witness(&observation, 0.6))).unwrap().unwrap();
    observation.start_ms = SOURCE_START+3_600_000; observation.end_ms = SOURCE_START+7_200_000; observation.observed_at_ms = observation.end_ms;
    observation.sequence = 2; observation.predecessor_cut = Some(observation.source_cut.clone());
    observation.source_cut = "cut/healthy".into(); observation.checks[0].value = Some(0.4);
    let mut healthy = cpu_witness(&observation, 0.4);
    healthy.processes[0].cpu_start_ns = cpu_witness(&observation, 0.6).processes[0].cpu_end_ns;
    healthy.processes[0].cpu_end_ns += healthy.processes[0].cpu_start_ns;
    let mut partial = healthy.clone(); partial.boundary_uncertainty_ns = 1;
    assert!(store.recover_slo_breach(&episode, observation.clone(), Some(partial)).is_err());
    let reset = cpu_witness(&observation, 0.4);
    assert!(store.recover_slo_breach(&episode, observation.clone(), Some(reset)).is_err());
    store.recover_slo_breach(&episode, observation, Some(healthy)).unwrap();
}

#[test]
fn a_signed_replica_reduces_but_does_not_originate_work() {
    let source = source();
    let episode = miss(&source, "person-read");
    let identity = key();
    admit(&source, "birch", &identity, "invite", None);
    let replica = node("birch", Some(&identity), None);
    replica.pin_fleet_anchor(&source.member_public_key().unwrap()).unwrap();
    sync(&source, &replica);
    replica.fleet_membership().unwrap();
    assert_eq!(fact_state(&replica, &episode), "pending");
    assert!(replica.prepare_slo_missions("/tmp").unwrap().is_empty());
    assert!(replica.work(None, true).unwrap().is_empty());
    let source_count = claims(&source).len();
    sync(&replica, &source);
    assert_eq!(claims(&source).len(), source_count);
}

#[test]
fn replica_admission_rejects_relabelled_origin_or_non_runtime_actor() {
    let store = source();
    miss(&store, "person-read");
    let claim = claims(&store).into_iter().find(|claim| claim.kind == KIND).unwrap();
    assert!(matches!(classify_replicated_claim_with_registry(&claim, st3_schema::registry()).unwrap(), ReplicatedClaimAdmission::Valid));
    for forged in [true, false] {
        let mut bad = claim.clone();
        if forged { bad.origin = "birch".into(); } else { bad.actor = Some(DEFAULT_OWNER.into()); }
        assert!(classify_replicated_claim_with_registry(&bad, st3_schema::registry()).is_err());
    }
}

/// Replays the actual ordinary known claims, not a substitute mission state machine.
/// The registry fixture models a supported older build keeping new kinds unknown.
#[test]
fn recovered_before_dispatch_refuses_every_known_claim_prefix() {
    let source = source();
    declare_owner(&source);
    let baseline = claims(&source).into_iter().map(|claim| claim.id).collect::<BTreeSet<_>>();
    let episode = miss(&source, "person-read");
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    let run = source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    assert!(!execution_admitted(&source.readers.get(), &run).unwrap());
    // Cancellation is intentionally absent: this matrix proves the readiness fence,
    // rather than succeeding because an ordinary terminal run rejects claims.
    let canonical = claims(&source).into_iter().filter(|claim| !baseline.contains(&claim.id)
        && claim.kind != "mission-run.state").collect::<Vec<_>>();
    let mut old_registry = st3_schema::registry().clone();
    old_registry.claims.remove(KIND).unwrap();
    assert!(canonical.len() <= 8, "prefix control must stay bounded");
    let fix_subject = source.work(None, true).unwrap().into_iter().find(|step| step.step == "fix").unwrap().subject;
    // Every possible prefix is a subset of this finite group of known/new claims.
    // Test every subset in forward and reverse arrival order, including omission
    // of cancellation and/or recovery, and both build registries.
    for old in [false, true] {
        for reverse in [false, true] {
            for mask in 0..(1usize << canonical.len()) {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("prefix.sqlite3");
                let target = Arc::new(Store::open(&path, "alder").unwrap());
                declare_owner(&target);
                let mut chosen = canonical.iter().enumerate().filter(|(index, _)| mask & (1<<index) != 0)
                    .map(|(_, claim)| claim).collect::<Vec<_>>();
                if reverse { chosen.reverse(); }
                for claim in chosen {
                    if old && !matches!(classify_replicated_claim_with_registry(claim, &old_registry).unwrap(), ReplicatedClaimAdmission::Valid) { continue; }
                    admit_fixture_claim(&source,&target,claim);
                }
                // Missing ordinary prerequisites may defer projection; an error must
                // leave the previous projection non-executable as well. Import and
                // projection have separate durability boundaries in the actual store.
                let _projection = target.connection.batched(|tx| replay_graph_from_nothing_tx(tx)).unwrap();
                assert!(!crate::reconcile::tests::slo_breach_tests::gate_passes(target.clone(), &episode),
                    "old={old} reverse={reverse} mask={mask}: actual Field gate passed");
                let raw: Option<String> = target.readers.get().query_row("SELECT status FROM step_runs WHERE subject=?1",
                    [&fix_subject], |row| row.get(0)).optional().unwrap();
                if let Some(raw) = raw { assert_eq!(raw, "pending", "the ordinary prerequisite, before the new ready-only guard, holds the old path"); }
                let selected = target.work(Some(DEFAULT_OWNER), true).unwrap();
                assert!(selected.iter().all(|step| step.step != "fix" || step.status != "ready"));
                let error = target.work_action(&fix_subject, "claim", &request(&format!("prefix/{old}/{reverse}/{mask}"))).unwrap_err();
                assert!(matches!(error.code.as_str(), "missing-step-run" | "work-not-ready"),
                    "the normal claim must refuse for readiness/absence, not an unrelated missing fixture: {error:?}");
                let _checkpoint_projection = target.connection.batched(|tx| checkpoint_rules::replay_from_nothing(tx)).unwrap();
                assert!(!crate::reconcile::tests::slo_breach_tests::gate_passes(target.clone(), &episode));
                drop(target);
                let reopened = Arc::new(Store::open(&path, "alder").unwrap());
                assert!(!crate::reconcile::tests::slo_breach_tests::gate_passes(reopened.clone(), &episode));
                assert!(reopened.work_for_reconcile(DEFAULT_OWNER).unwrap().iter()
                    .all(|step| step.step != "fix" || step.status != "ready"));
                assert!(reopened.work_action(&fix_subject, "claim", &request(&format!("reopen/{old}/{reverse}/{mask}"))).is_err());
            }
        }
    }
}

// A test-scoped process counter, not a daemon collector or an hour-coverage claim.
fn process_cpu_ns() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: time is a writable, valid timespec for the duration of the call.
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) }, 0);
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

#[test]
fn hosted_cost_control_reports_full_writer_and_replica_holds() {
    let source = source();
    let identity = key();
    let replica = node("birch", Some(&identity), None);
    let begin = Instant::now();
    let cpu_before = process_cpu_ns();
    let ((episode, result), writer_hold) = smallclaims::windows::holds::longest_during(|| {
        let episode = miss(&source, "person-read");
        let prepared = source.prepare_slo_missions("/tmp").unwrap().remove(0);
        (episode, source.dispatch_slo_mission(prepared))
    });
    result.unwrap();
    let source_cpu_ns = process_cpu_ns().saturating_sub(cpu_before);
    let cpu_before = process_cpu_ns();
    let (admission, replica_hold) = smallclaims::windows::holds::longest_during(|| sync(&source, &replica));
    let replica_cpu_ns = process_cpu_ns().saturating_sub(cpu_before);
    assert_eq!(admission.invalid, 0);
    let exchange = source.export_replication_exchange(&source.bound_fleet().unwrap().unwrap(),
        &smallclaims::replication::ReplicationInventory::default()).unwrap();
    for envelope in &exchange.envelopes {
        assert!(serde_json::to_vec(envelope).unwrap().len() <= MAX_GROUP_BYTES);
    }
    eprintln!("SLO_COST objective=person-read episode={episode} full_dispatch_elapsed_ms={} writer_hold_ms={} replica_hold_ms={} source_process_cpu_ms={} replica_process_cpu_ms={} writer_hold_reference=st2/3080 qualification=HOSTED_CONTROL_ONLY isolation=nextest-process",
        begin.elapsed().as_secs_f64()*1000.0, writer_hold.as_secs_f64()*1000.0, replica_hold.as_secs_f64()*1000.0,
        source_cpu_ns as f64/1_000_000.0, replica_cpu_ns as f64/1_000_000.0);
    // Debug-runner ceiling only; meeting production budgets still requires Intake's
    // measured comparison with the current writer-hold baseline before activation.
    assert!(writer_hold <= Duration::from_millis(500));
    assert!(replica_hold <= Duration::from_millis(500));
}

#[test]
fn real_field_gate_accepts_only_a_positive_admitted_episode() {
    let source = Arc::new(source());
    declare_owner(&source);
    let episode = miss(&source, "person-read");
    assert!(!crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode));
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode));
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode),
        "recovery must not withdraw an admitted ordinary gate");
}

#[test]
fn a_supported_old_registry_keeps_signed_episode_claims_unknown_until_upgrade() {
    let source = source();
    let episode = miss(&source, "person-read");
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("old.sqlite3");
    let old = Store::open(&path, "birch").unwrap();
    old.bind_fleet(&source.bound_fleet().unwrap().unwrap()).unwrap();
    let mut registry = st3_schema::registry().clone();
    registry.claims.remove(KIND).unwrap();
    old.set_claim_registry(registry);
    let admission = sync(&source, &old);
    assert_eq!(admission.invalid, 0);
    assert_eq!(admission.unknown, 3);
    assert_eq!(fact_state(&old, &episode), "unknown");
    assert!(old.work(Some(DEFAULT_OWNER), true).unwrap().iter().all(|step| step.status != "ready"));
    drop(old);
    let upgraded = Store::open(&path, "birch").unwrap();
    assert_eq!(upgraded.validate_replication_backlog().unwrap().invalid, 0);
    upgraded.project_replication_backlog().unwrap();
    assert_eq!(fact_state(&upgraded, &episode), "recovered");
    assert!(upgraded.prepare_slo_missions("/tmp").is_err() || upgraded.prepare_slo_missions("/tmp").unwrap().is_empty());
    assert!(upgraded.work(Some(DEFAULT_OWNER), true).unwrap().iter().all(|step| step.status != "ready"));
}

#[test]
fn conflicting_events_converge_and_never_publish_ready() {
    let source = source();
    let episode = miss(&source, "person-read");
    let a = claims(&source).into_iter().find(|claim| claim.kind == KIND).unwrap();
    let mut b = a.clone();
    b.id = "a-conflicting-claim".into();
    b.body["fields"]["record"]["owner"] = json!("agent/another-owner");
    let mut digests = Vec::new();
    for order in [[&a, &b], [&b, &a]] {
        let target = Store::open_memory("projection").unwrap();
        target.connection.batched(|tx| {
            for claim in order { project(tx, claim)?; }
            Ok::<_,anyhow::Error>(())
        }).unwrap().unwrap();
        assert_eq!(fact_state(&target, &episode), "unknown");
        let tables = projection_digest::tables(&target.readers.get()).unwrap();
        digests.push([tables["slo_pending"].clone(), tables["slo_episode_heads"].clone(), tables["slo_episode_events"].clone()]);
    }
    assert_eq!(digests[0], digests[1]);
}

#[test]
fn relabelled_remote_readings_and_wrong_health_sequences_are_rejected() {
    let source = source();
    let mut remote = reading(&source, 0, Some(120.0)); remote.member = "birch".into();
    assert!(source.record_slo_miss("person-read", None, remote, None).is_err());
    let episode = miss(&source, "person-read");
    for mode in 0..3 {
        let mut healthy = reading(&source, 3_600_000, Some(80.0));
        match mode { 0 => healthy.member = "birch".into(), 1 => healthy.sequence = 1,
            _ => healthy.predecessor_cut = Some("unrelated-cut".into()) }
        assert!(source.recover_slo_breach(&episode, healthy, None).is_err());
    }
}

#[test]
fn database_recovery_needs_its_full_day_not_an_hour() {
    let source = source();
    let day = 86_400_000;
    let mut onset = reading(&source, 0, Some(120.0));
    onset.window_ms = day; onset.covered_ms = day; onset.end_ms = SOURCE_START+day; onset.observed_at_ms = SOURCE_START+day;
    let episode = source.record_slo_miss("database", None, onset, None).unwrap().unwrap();
    let mut healthy = reading(&source, day, Some(80.0)); healthy.predecessor_cut = Some(reading(&source, 0, Some(120.0)).source_cut);
    assert!(source.recover_slo_breach(&episode, healthy.clone(), None).is_err());
    healthy.window_ms = day; healthy.covered_ms = day; healthy.end_ms = SOURCE_START+2*day; healthy.observed_at_ms = SOURCE_START+2*day;
    source.recover_slo_breach(&episode, healthy, None).unwrap();
}

#[test]
fn oversized_publication_rolls_back_without_losing_pending_intake() {
    let source = source();
    let episode = miss(&source, "person-read");
    let before = claims(&source).len();
    let batch = Uuid::now_v7().simple().to_string();
    let result = source.connection.batched(|tx| {
        for index in 0..17 {
            append_claim_tx(tx, "alder", "daemon/alder", "daemon.diagnostic", Some("daemon/runtime"),
                &json!({"fields":{"severity":"warning","code":"group-bound","reason":index.to_string()}}), &[], Some(&batch))?;
        }
        check_group(tx, &batch)
    }).unwrap();
    assert!(result.is_err());
    assert_eq!(claims(&source).len(), before);
    assert_eq!(fact_state(&source, &episode), "pending");
}

#[test]
fn checkpoint_proof_trim_and_canonical_renumber_preserve_episode_fences() {
    let source = source();
    let episode = miss(&source, "person-read");
    let (_, pending) = event(&source.readers.get(), &episode, Event::Pending).unwrap().unwrap();
    for _ in 0..2 {
        source.connection.batched(|tx| append(tx, "alder", &pending, &Uuid::now_v7().simple().to_string()))
            .unwrap().unwrap();
    }
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    let rows = claims(&source).into_iter().filter(|claim| claim.kind == KIND).collect::<Vec<_>>();
    let latest = rows.iter().map(|claim| claim.accepted_at_unix_ms).max().unwrap();
    let young = latest + 86_400_000;
    let cut = latest + 8*86_400_000;
    let references = rows.iter().collect::<Vec<_>>();
    assert_eq!(checkpoint_rules::rule_keep(checkpoint_rules::Rule::SloEventFences, rows.len(), &references,
        false, &BTreeMap::new(), young).len(), rows.len());
    let scratch = tempfile::tempdir().unwrap();
    let (_, _, proof) = source.plan_checkpoint_through(cut, None, scratch.path()).unwrap();
    assert!(proof.passed, "canonical proof/renumber: {:?}", proof.mismatches);
    let sealed = source.checkpoint_sealed_set(cut).unwrap();
    let plan = checkpoint::plan_drops(&sealed);
    assert!(!plan.claims.is_empty(), "exercise an actual redundant receipt drop");
    let before = projection_digest::oracle(&source.readers.get()).unwrap();
    let mut connection = source.connection.write();
    let tx = connection.transaction().unwrap();
    checkpoint::record_checkpoint_tombstones_tx(&tx, &checkpoint::checkpoint_name(cut), &plan.envelopes, &plan.claims).unwrap();
    checkpoint::delete_dropped_rows_tx(&tx, &plan.envelopes, &plan.claims).unwrap();
    replay_graph_from_nothing_tx(&tx).unwrap();
    tx.commit().unwrap();
    drop(connection);
    let after = projection_digest::oracle(&source.readers.get()).unwrap();
    for table in ["slo_pending", "slo_episode_heads", "slo_episode_events"] {
        assert_eq!(before[table], after[table], "{table} changed after real trim/replay");
    }
    assert_eq!(fact_state(&source, &episode), "recovered");
}

#[test]
fn an_unknown_conflicting_objective_does_not_starve_other_pending_obligations() {
    let source = source();
    let first = miss(&source, "objective/a");
    let (_, mut conflicting) = event(&source.readers.get(), &first, Event::Pending).unwrap().unwrap();
    conflicting.owner = "agent/another-owner".into();
    source.connection.batched(|tx| append(tx, "alder", &conflicting, &Uuid::now_v7().simple().to_string())).unwrap().unwrap();
    let next = miss(&source, "objective/b");
    let prepared = source.prepare_slo_missions("/tmp").unwrap();
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].record.episode, next);
    assert_eq!(fact_state(&source, &first), "unknown");
}

#[test]
fn an_old_registry_repair_withdraws_closure_and_matches_replay_on_current_members() {
    let source = source();
    let episode = miss(&source, "person-read");
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    let (recovered, _) = event(&source.readers.get(), &episode, Event::Recovered).unwrap().unwrap();
    let replacement = claims(&source).into_iter().find(|claim| claim.kind == "fleet.member-admitted").unwrap().id;
    let identity = key();
    admit(&source, "birch", &identity, "invite", None);
    let old = node("birch", Some(&identity), None);
    old.pin_fleet_anchor(&source.member_public_key().unwrap()).unwrap();
    let mut registry = st3_schema::registry().clone(); registry.claims.remove(KIND).unwrap(); old.set_claim_registry(registry);
    sync(&source, &old);
    let reference: String = old.readers.get().query_row("SELECT record_ref FROM replica_records WHERE claim_id=?1",
        [&recovered], |row| row.get(0)).unwrap();
    old.repair_replica_record(&reference, &replacement, "explicit old-registry repair fixture", "person/avery", "repair-slo-recovery").unwrap();
    sync(&old, &source);
    source.apply_replication_repairs().unwrap();
    assert_ne!(fact_state(&source, &episode), "recovered", "a repaired closure is not live evidence");
    let before = projection_digest::oracle(&source.readers.get()).unwrap();
    source.connection.batched(|tx| rebuild(tx)).unwrap().unwrap();
    let after = projection_digest::oracle(&source.readers.get()).unwrap();
    for table in ["slo_pending", "slo_episode_heads", "slo_episode_events"] { assert_eq!(before[table], after[table], "{table}"); }
}

#[test]
fn initial_owner_block_is_positive_only_after_unblock_and_does_not_flap() {
    let source = Arc::new(source());
    let episode = miss(&source, "person-read");
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    assert!(!crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode));
    declare_owner(&source);
    source.unblock_slo_owner(&episode).unwrap();
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode));
    let intent = crate::graph::parse_internal_intent("version 2\nstop \"agent/fleet/smalltalk/speed\"", "alder").unwrap();
    let planned = source.mission(&intent, IntentInput { kdl: "stop owner".into(), source_name: None }).unwrap();
    source.apply(&intent, &planned.subject_tokens, "owner-went-away").unwrap();
    let before = claims(&source).len();
    source.unblock_slo_owner(&episode).unwrap();
    assert_eq!(claims(&source).len(), before, "owner flaps do not create receipt storms");
    assert_eq!(fact_state(&source, &episode), "owner_unavailable");
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode),
        "existing execution admission is preserved through ordinary owner liveness changes");
}

#[test]
fn concurrent_misses_and_prepared_dispatches_create_one_mission() {
    let source = Arc::new(source());
    let callers = (0..8).map(|_| {
        let source = source.clone();
        std::thread::spawn(move || miss(&source, "person-read"))
    }).collect::<Vec<_>>();
    let episodes = callers.into_iter().map(|thread| thread.join().unwrap()).collect::<BTreeSet<_>>();
    assert_eq!(episodes.len(), 1);
    let prepared = (0..2).map(|_| source.prepare_slo_missions("/tmp").unwrap().remove(0)).collect::<Vec<_>>();
    let dispatches = prepared.into_iter().map(|prepared| {
        let source = source.clone();
        std::thread::spawn(move || source.dispatch_slo_mission(prepared).unwrap())
    }).collect::<Vec<_>>();
    let missions = dispatches.into_iter().map(|thread| thread.join().unwrap()).collect::<BTreeSet<_>>();
    assert_eq!(missions.len(), 1);
    let records = claims(&source);
    assert_eq!(records.iter().filter(|claim| claim.kind == KIND && claim.body["fields"]["record"]["event"] == "pending").count(), 1);
    assert_eq!(records.iter().filter(|claim| claim.kind == "mission-run.created").count(), 1);
}

#[test]
fn a_later_miss_and_unknown_coverage_fence_out_late_healthy_cuts() {
    let source = source();
    let episode = miss(&source, "person-read"); // [0,1h]
    let later = reading(&source, 2*3_600_000, Some(120.0)); // [2h,3h]
    assert_eq!(source.record_slo_miss("person-read", None, later.clone(), None).unwrap().unwrap(), episode);
    let late = reading(&source, 3_600_000, Some(80.0)); // [1h,2h], names onset
    assert!(source.recover_slo_breach(&episode, late, None).is_err());
    assert_ne!(fact_state(&source, &episode), "recovered");
    let mut unknown = reading(&source, 3*3_600_000, None); unknown.complete = false;
    assert!(source.record_slo_miss("person-read", None, unknown, None).unwrap().is_none());
    let mut too_old = reading(&source, 3*3_600_000, Some(80.0)); too_old.predecessor_cut = Some(later.source_cut.clone());
    assert!(source.recover_slo_breach(&episode, too_old, None).is_err());
    let mut healthy = reading(&source, 4*3_600_000, Some(80.0)); healthy.predecessor_cut = Some(later.source_cut);
    source.recover_slo_breach(&episode, healthy, None).unwrap();
    assert_eq!(fact_state(&source, &episode), "recovered");
}

#[test]
fn cpu_cuts_must_match_the_declared_hour_and_required_worker_topology() {
    let source = source();
    let mut observation = reading(&source, 0, Some(0.6));
    observation.version = "combined-idle/v1".into(); observation.checks[0].unit = "cores".into(); observation.checks[0].target = 0.5;
    let mut two_hours = cpu_witness(&observation, 0.6); two_hours.monotonic_end_ns *= 2;
    assert!(source.record_slo_miss("cpu/idle", None, observation.clone(), Some(two_hours)).is_err());
    let mut wrong_cut = cpu_witness(&observation, 0.6); wrong_cut.source_cut = "other-cut".into();
    assert!(source.record_slo_miss("cpu/idle", None, observation.clone(), Some(wrong_cut)).is_err());
    let mut daemon_only = cpu_witness(&observation, 0.6); daemon_only.processes.pop();
    assert!(source.record_slo_miss("cpu/idle", None, observation.clone(), Some(daemon_only)).is_err());
    let mut unproved_zero = cpu_witness(&observation, 0.6); unproved_zero.expected_workers = 0; unproved_zero.processes.pop();
    assert!(source.record_slo_miss("cpu/idle", None, observation.clone(), Some(unproved_zero)).is_err());
    let mut omitted = cpu_witness(&observation, 0.6); omitted.expected_workers = 2;
    assert!(source.record_slo_miss("cpu/idle", None, observation.clone(), Some(omitted.clone())).is_err());
    let mut worker = omitted.processes[1].clone(); worker.pid = 12; worker.birth = 3; omitted.processes.push(worker);
    source.record_slo_miss("cpu/idle", None, observation, Some(omitted)).unwrap().unwrap();
}

#[test]
fn admitted_before_owner_return_recovers_then_runs_the_real_dependency_and_claim_flow() {
    let source = Arc::new(source());
    let episode = miss(&source, "person-read");
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    declare_owner(&source);
    source.unblock_slo_owner(&episode).unwrap();
    crate::reconcile::tests::slo_breach_tests::drive_ordinary_flow(source.clone(), "alder");
    let fix = source.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
    assert_eq!(fix.status, "ready", "healthy is not evidence of a substantive repair");
    source.work_action(&fix.subject, "claim", &request("owner-return-after-recovery")).unwrap();
}

#[test]
fn a_positive_dispatch_runs_after_recovery_and_a_later_episode_without_manual_step_state() {
    let source = Arc::new(source());
    declare_owner(&source);
    let episode = miss(&source, "person-read");
    source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    // The agentless dispatch step has not run yet.
    source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
    source.record_slo_miss("person-read", None, reading(&source, 2*3_600_000, Some(120.0)), None).unwrap();
    crate::reconcile::tests::slo_breach_tests::drive_ordinary_flow(source.clone(), "alder");
    let fix = source.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
    assert_eq!(fix.status, "ready");
    source.work_action(&fix.subject, "claim", &request("positive-after-later-episode")).unwrap();
    source.connection.batched(|tx| replay_graph_from_nothing_tx(tx)).unwrap().unwrap();
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(source.clone(), &episode));
    source.work_action(&fix.subject, "renew", &request("positive-history-survives-replay")).unwrap();
}

#[test]
fn an_ordinary_mission_step_named_fix_keeps_normal_claim_admission() {
    let source = Arc::new(source()); declare_owner(&source);
    let kdl = format!("version 2\nmission \"ordinary\" state=\"ready\" {{ goal \"ordinary control\"; step \"fix\" {{ assigned-to {DEFAULT_OWNER:?} }} }}");
    let intent = crate::graph::parse_test_intent(&kdl, "alder").unwrap();
    let planned = source.mission(&intent, IntentInput { kdl, source_name: None }).unwrap();
    source.apply(&intent, &planned.subject_tokens, "ordinary-fix").unwrap();
    source.create_mission_run(&MissionRunRequest { mission: "ordinary".into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/avery".into()), mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: "ordinary-run".into() }).unwrap();
    crate::reconcile::tests::slo_breach_tests::drive_ordinary_flow(source.clone(), "alder");
    let fix = source.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
    assert_eq!(fix.status, "ready");
    source.work_action(&fix.subject, "claim", &request("ordinary-positive-claim")).unwrap();
}

#[test]
fn empty_new_tables_change_root_bytes_but_mixed_registry_transport_waits_to_compare() {
    let source = source();
    let identity = key(); admit(&source, "birch", &identity, "invite", None);
    let old = node("birch", Some(&identity), None);
    old.pin_fleet_anchor(&source.member_public_key().unwrap()).unwrap();
    let mut registry = st3_schema::registry().clone(); registry.claims.remove(KIND).unwrap(); old.set_claim_registry(registry);
    sync(&source, &old); sync(&old, &source);
    let mut legacy = old.export_replication_summary(&source.bound_fleet().unwrap().unwrap()).unwrap();
    let canonical_before = claims(&source).into_iter().map(|claim| claim.id).collect::<BTreeSet<_>>();
    let digest_before = projection_digest::oracle(&source.readers.get()).unwrap();
    let full = legacy.projection_digests.clone();
    for table in ["slo_pending", "slo_episode_heads", "slo_episode_events"] { legacy.projection_digests.remove(table).unwrap(); }
    assert_ne!(projection_digest::root(&full), projection_digest::root(&legacy.projection_digests), "empty table names are hashed");
    legacy.graph_digest = projection_digest::root(&legacy.projection_digests);
    source.receive_replication_exchange("birch", &source.bound_fleet().unwrap().unwrap(), &legacy).unwrap();
    let status = source.replication_status_sealed(true, source.bound_fleet().unwrap().as_deref(), &["birch".into()]).unwrap();
    let peer = status.peers.into_iter().find(|peer| peer.peer == "birch").unwrap();
    assert!(peer.projection_comparison_waiting);
    assert!(peer.projection_comparison_waiting_reason.as_deref().unwrap().contains("builds differ"));
    assert!(peer.projection_graphs_equal.is_none(), "unknown comparison must not be reported as equal");
    assert!(peer.differing_tables.is_empty());
    assert!(peer.graph_differs_since_unix_ms.is_none());
    assert!(!peer.sync.unwrap().diverged);
    assert_eq!(canonical_before, claims(&source).into_iter().map(|claim| claim.id).collect());
    assert_eq!(digest_before, projection_digest::oracle(&source.readers.get()).unwrap(), "metadata comparison must not repair or replay projections");
}

#[test]
fn different_checkpoint_rules_cannot_form_one_complete_verification_certificate() {
    use smallclaims::store::checkpoint_agreement::{CheckpointClaim, certificates};
    let source = source();
    let cut = 86_400_000u128;
    let checkpoint = checkpoint::checkpoint_name(cut);
    // The source compatibility change has a different identity from the prior rule
    // engine. This control exercises the real certificate reducer for that mismatch.
    let mut legacy = Sha256::new(); legacy.update(b"st3-checkpoint-rules-v1\0");
    legacy.update(15u32.to_be_bytes());
    let prior_description = checkpoint_rules::rules_description().lines().filter(|line| !line.starts_with(KIND)).collect::<Vec<_>>().join("\n");
    legacy.update(prior_description.as_bytes());
    let legacy = hex::encode(legacy.finalize()); assert_ne!(legacy, rules_digest());
    let verification = |writer: &str, rules: &str| CheckpointClaim { id: format!("verified/{writer}"),
        kind: "checkpoint.verified".into(), subject: checkpoint.clone(), writer: writer.into(), actor: Some("daemon/runtime".into()),
        fields: serde_json::from_value(json!({"cut_unix_ms":cut,"participants":["alder","birch"],
            "sealed_digest":"same-seal","rules_digest":rules,"drop_digest":"same-drop","retained_digest":"same-retained",
            "graph_digest":"same-graph","reader_digest":"same-reader"})).unwrap() };
    let current = rules_digest();
    assert!(certificates(&[verification("alder", &current), verification("birch", &legacy)], &checkpoint).is_empty());
    assert_eq!(certificates(&[verification("alder", &current), verification("birch", &current)], &checkpoint).len(), 1);
    assert!(source.work(None, true).unwrap().is_empty(), "no checkpoint control starts work");
}

#[test]
fn source_restart_requires_fresh_coverage_and_retains_the_canonical_obligation() {
    let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("restart.sqlite3");
    let identity = key();
    smallclaims::store::set_thread_clock(Some(u128::from(SOURCE_START)));
    let source = Store::open(&path, "alder").unwrap();
    smallclaims::store::set_thread_clock(None);
    source.bind_fleet("7c9e6679-7425-40de-944b-e07fc1f90ae7").unwrap();
    source.pin_fleet_anchor(identity.public()).unwrap(); source.set_member_key(Some(identity.clone())).unwrap();
    admit(&source, "alder", &identity, "anchor", None); source.project_replication_backlog().unwrap(); source.fleet_membership().unwrap();
    let episode = miss(&source, "person-read");
    let onset_cut = reading(&source, 0, Some(120.0)).source_cut;
    let late = reading(&source, 3_600_000, Some(80.0));
    source.record_slo_miss("person-read", None, reading(&source, 2*3_600_000, Some(120.0)), None).unwrap();
    drop(source);
    smallclaims::store::set_thread_clock(Some(u128::from(SOURCE_START+3*3_600_000)));
    let reopened = Store::open(&path, "alder").unwrap();
    smallclaims::store::set_thread_clock(None);
    reopened.set_member_key(Some(identity)).unwrap(); reopened.fleet_membership().unwrap();
    assert!(reopened.recover_slo_breach(&episode, late, None).is_err());
    let mut fresh = reading(&reopened, 0, Some(80.0)); fresh.predecessor_cut = Some(onset_cut);
    reopened.recover_slo_breach(&episode, fresh, None).unwrap();
    assert_eq!(fact_state(&reopened, &episode), "recovered");
}

#[test]
fn the_same_live_owner_prefix_fixture_can_positively_claim_an_admitted_mission() {
    let original = source(); declare_owner(&original);
    let baseline = claims(&original).into_iter().map(|claim| claim.id).collect::<BTreeSet<_>>();
    let episode = miss(&original, "person-read");
    original.dispatch_slo_mission(original.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
    let onset = event(&original.readers.get(), &episode, Event::Pending).unwrap().unwrap().0;
    let publication = claims(&original).into_iter().find(|claim| claim.kind == "mission.published").unwrap();
    assert!(checkpoint_rules::cited_by(&publication).any(|id| id == onset),
        "the ordinary evidence reference must preserve the mission's immutable gate source");
    let group = claims(&original).into_iter().filter(|claim| !baseline.contains(&claim.id)).collect::<Vec<_>>();
    let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("positive-prefix.sqlite3");
    let target = Arc::new(Store::open(&path, "alder").unwrap()); declare_owner(&target);
    for claim in group {
        admit_fixture_claim(&original,&target,&claim);
    }
    target.connection.batched(|tx| replay_graph_from_nothing_tx(tx)).unwrap().unwrap();
    assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(target.clone(), &episode));
    crate::reconcile::tests::slo_breach_tests::drive_ordinary_flow(target.clone(), "alder");
    let fix = target.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
    assert_eq!(fix.status, "ready");
    target.work_action(&fix.subject, "claim", &request("identical-positive-prefix")).unwrap();
}

#[test]
fn worst_supported_opening_group_reports_writer_cpu_and_signed_bytes() {
    let source = source();
    let mut observation = reading(&source, 0, Some(0.6));
    observation.version = "v".repeat(90); observation.population = "p".repeat(90); observation.source_cut = "c".repeat(90);
    observation.checks[0].unit = "cores".into(); observation.checks[0].target = 0.5;
    for index in 0..8 { observation.counters.insert(format!("counter-{index}"), u64::MAX); }
    // Fill the admitted summary near its 1 KiB bound without inventing a sample.
    while observation.validate().is_err() {
        if observation.population.len()>1 { observation.population.pop(); }
        else if observation.version.len()>1 { observation.version.pop(); }
        else { panic!("maximum-summary fixture has an invalid non-size field"); }
    }
    while observation.version.len()<128 {
        let mut next = observation.clone(); next.version.push('v');
        if next.validate().is_err() { break; } observation = next;
    }
    observation.validate().unwrap();
    let mut witness = cpu_witness(&observation, 0.6); witness.expected_workers = 8;
    for index in 0..7 { let mut worker = witness.processes[1].clone(); worker.pid = 12+index; worker.birth = 3+u64::from(index); witness.processes.push(worker); }
    let before = process_cpu_ns();
    let (result, hold) = smallclaims::windows::holds::longest_during(|| {
        source.record_slo_miss("cpu/idle", None, observation, Some(witness)).unwrap().unwrap();
        source.dispatch_slo_mission(source.prepare_slo_missions(&format!("/{}", "w".repeat(1000))).unwrap().remove(0))
    });
    result.unwrap();
    let cpu = process_cpu_ns().saturating_sub(before);
    let exchange = source.export_replication_exchange(&source.bound_fleet().unwrap().unwrap(), &smallclaims::replication::ReplicationInventory::default()).unwrap();
    let wire = exchange.envelopes.iter().map(|envelope| serde_json::to_vec(envelope).unwrap().len()).max().unwrap();
    assert!(wire<=MAX_GROUP_BYTES);
    eprintln!("SLO_WORST_GROUP workers=8 source_summary_bound=1024 workspace_bytes=1001 writer_hold_ms={} source_process_cpu_ms={} signed_wire_max_bytes={wire} writer_hold_reference=st2/3080 qualification=HOSTED_CONTROL_ONLY",
        hold.as_secs_f64()*1000.0,cpu as f64/1_000_000.0);
    assert!(hold<=Duration::from_millis(500));
}

#[test]
fn a_two_hour_average_cannot_recover_a_one_hour_idle_reading() {
    let source = source();
    let mut onset = reading(&source, 0, Some(0.6));
    onset.version = "idle/v1".into(); onset.checks[0].unit = "cores".into(); onset.checks[0].target = 0.5;
    let original = cpu_witness(&onset, 0.6);
    let episode = source.record_slo_miss("cpu/idle", None, onset.clone(), Some(original.clone())).unwrap().unwrap();
    let mut healthy = reading(&source, 3_600_000, Some(0.4));
    healthy.version = onset.version; healthy.checks[0].unit = "cores".into(); healthy.checks[0].target = 0.5;
    let mut doubled = cpu_witness(&healthy, 0.4);
    doubled.monotonic_end_ns += 3_600_000_000_000;
    doubled.processes[0].cpu_start_ns = original.processes[0].cpu_end_ns;
    doubled.processes[0].cpu_end_ns = doubled.processes[0].cpu_start_ns + 2*cpu_witness(&healthy, 0.4).processes[0].cpu_end_ns;
    // Numerator / two hours is exactly the declared 0.4: numeric consistency
    // alone passes the old discriminator. Only same-window binding rejects it.
    assert!(source.recover_slo_breach(&episode, healthy.clone(), Some(doubled)).is_err());
    let mut proper = cpu_witness(&healthy, 0.4);
    proper.processes[0].cpu_start_ns = original.processes[0].cpu_end_ns;
    proper.processes[0].cpu_end_ns += proper.processes[0].cpu_start_ns;
    source.recover_slo_breach(&episode, healthy, Some(proper)).unwrap();
}

#[test]
fn admitted_claim_history_and_gate_survive_both_delivery_orders_reopen_and_owner_loss() {
    for stop_first in [false, true] {
        let source = Arc::new(source()); declare_owner(&source);
        let episode = miss(&source, "person-read");
        source.dispatch_slo_mission(source.prepare_slo_missions("/tmp").unwrap().remove(0)).unwrap();
        crate::reconcile::tests::slo_breach_tests::drive_ordinary_flow(source.clone(), "alder");
        let fix = source.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.step == "fix").unwrap();
        source.work_action(&fix.subject, "claim", &request("history-positive")).unwrap();
        let stop = || {
            let intent = crate::graph::parse_internal_intent("version 2\nstop \"agent/fleet/smalltalk/speed\"", "alder").unwrap();
            let planned = source.mission(&intent, IntentInput { kdl: "stop fixture owner".into(), source_name: None }).unwrap();
            source.apply(&intent, &planned.subject_tokens, "history-owner-stop").unwrap();
        };
        if stop_first { stop(); }
        source.recover_slo_breach(&episode, reading(&source, 3_600_000, Some(80.0)), None).unwrap();
        source.record_slo_miss("person-read", None, reading(&source, 2*3_600_000, Some(120.0)), None).unwrap();
        if !stop_first { stop(); }
        let expected_claims = claims(&source).into_iter().filter(|claim| claim.subject == fix.subject)
            .map(|claim| claim.id).collect::<BTreeSet<_>>();
        let fleet = source.bound_fleet().unwrap().unwrap();
        for reverse in [false, true] {
            let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("history.sqlite3");
            let target = Store::open(&path, "birch").unwrap(); target.bind_fleet(&fleet).unwrap();
            target.pin_fleet_anchor(&source.member_public_key().unwrap()).unwrap();
            let mut exchange = source.export_replication_exchange(&fleet, &target.replication_inventory().unwrap()).unwrap();
            if reverse { exchange.envelopes.reverse(); }
            target.receive_replication_exchange("alder", &fleet, &exchange).unwrap();
            target.validate_replication_backlog().unwrap(); target.project_replication_backlog().unwrap();
            target.connection.batched(|tx| checkpoint_rules::replay_from_nothing(tx)).unwrap().unwrap();
            drop(target);
            let reopened = Arc::new(Store::open(&path, "birch").unwrap());
            assert!(crate::reconcile::tests::slo_breach_tests::gate_passes(reopened.clone(), &episode));
            let actual = reopened.work(Some(DEFAULT_OWNER), true).unwrap().into_iter().find(|step| step.subject == fix.subject).unwrap();
            assert_eq!(actual.claimant.as_deref(), Some(DEFAULT_OWNER));
            assert_eq!(actual.status, "claimed");
            let actual_claims = claims(&reopened).into_iter().filter(|claim| claim.subject == fix.subject)
                .map(|claim| claim.id).collect::<BTreeSet<_>>();
            assert_eq!(expected_claims, actual_claims);
        }
    }
}
