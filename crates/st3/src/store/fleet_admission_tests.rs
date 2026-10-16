use super::*;
use crate::fleet::{MemberKey, MemberState, envelope_signature_message, verify_signature};

const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

pub(super) fn key() -> Arc<MemberKey> {
    Arc::new(MemberKey::generate().unwrap().0)
}

pub(super) fn node(name: &str, key: Option<&Arc<MemberKey>>, anchor: Option<&Arc<MemberKey>>) -> Store {
    let store = Store::open_memory(name).unwrap();
    store.bind_fleet(FLEET).unwrap();
    if let Some(anchor) = anchor {
        store.pin_fleet_anchor(anchor.public()).unwrap();
    }
    if let Some(key) = key {
        store.set_member_key(Some(key.clone())).unwrap();
    }
    store
}

fn append(store: &Store, kind: &str, subject: &str, fields: Value) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn note(store: &Store, text: &str) -> ClaimRecord {
    append(
        store,
        "daemon.diagnostic",
        &format!("daemon/{}", store.origin),
        json!({"severity": "warning", "code": "fleet-test", "reason": text}),
    )
}

pub(super) fn admit(by: &Store, name: &str, member: &MemberKey, via: &str, floor: Option<u64>) {
    let mut fields = json!({
        "fleet_id": FLEET,
        "member_key": member.public(),
        "via": via,
        "mode": "listening",
    });
    if via != "anchor" {
        fields["sponsor"] = json!(format!("host/{}", by.origin));
    }
    if let Some(floor) = floor {
        fields["writer_floor"] = json!(floor);
    }
    append(by, "fleet.member-admitted", &format!("host/{name}"), fields);
}

fn remove(by: &Store, name: &str, member: Option<&MemberKey>, high_water: u64) {
    let mut fields = json!({"high_water": high_water, "reason": "test"});
    if let Some(member) = member {
        fields["member_key"] = json!(member.public());
    }
    append(by, "fleet.member-removed", &format!("host/{name}"), fields);
}

/// One exchange from `from` to `to`, then admission and projection on `to`.
pub(super) fn sync(from: &Store, to: &Store) -> ReplicationAdmission {
    let exchange = from
        .export_replication_exchange_answering(
            FLEET,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, FLEET, &exchange)
        .unwrap();
    let admission = to.validate_replication_backlog().unwrap();
    to.project_replication_backlog().unwrap();
    admission
}

fn admitted(store: &Store, claim: &ClaimRecord) -> bool {
    store.claim_by_id(&claim.id).unwrap().is_some()
}

/// A member one build behind keeps a newer member's unknown claims, and only an unknown claim
/// that could bear on runtime authority makes a subject indeterminate. Seen live: a newer
/// member's `harness.limits` made every seat on it unreachable for terminal attach from an
/// older member, though its runtime observations were valid.
#[test]
fn an_unknown_non_authority_claim_leaves_runtime_routing_alone() {
    let anchor_key = key();
    let older_key = key();
    let current = node("current", Some(&anchor_key), Some(&anchor_key));
    admit(&current, "current", &anchor_key, "anchor", None);
    admit(&current, "older", &older_key, "invite", None);
    let older = node("older", Some(&older_key), Some(&anchor_key));
    let mut registry = st3_schema::registry().clone();
    for kind in ["harness.limits", "runtime.restart-window-reset"] {
        registry.claims.remove(kind).unwrap();
    }
    older.set_claim_registry(registry);
    sync(&current, &older);

    let subject = "agent/example/worker";
    append(
        &current,
        "runtime.observed",
        subject,
        json!({"status": "running", "runtime_id": "agent.example.worker",
               "incarnation_id": "worker-one"}),
    );
    append(
        &current,
        "harness.limits",
        subject,
        json!({"driver": "claude", "incarnation_id": "worker-one",
               "five_hour_percent": 12.5, "measured_at_unix_ms": 1_790_000_000_000_u64}),
    );
    let admission = sync(&current, &older);
    assert_eq!(admission.unknown, 1, "the newer kind is kept, not dropped");
    let status = older.status(Some(subject)).unwrap();
    let worker = &status.subjects[0];
    assert_eq!(worker.reachability, "reachable", "{:?}", worker.reason);
    assert_eq!(worker.actual_origin.as_deref(), Some("current"));
    assert_eq!(worker.reason, None);
    let authority = older.runtime_authority(subject).unwrap().unwrap();
    assert_eq!(authority.reachability, "reachable");
    assert_eq!(authority.actual_origin.as_deref(), Some("current"));
    assert_eq!(authority.actual, worker.actual);

    // An unknown kind in the runtime family could change the answer: stay conservative.
    append(
        &current,
        "runtime.restart-window-reset",
        subject,
        json!({"incarnation_id": "worker-one", "reason": "test"}),
    );
    sync(&current, &older);
    let status = older.status(Some(subject)).unwrap();
    assert_eq!(status.subjects[0].reachability, "indeterminate");
    assert_eq!(older.runtime_authority(subject).unwrap().unwrap().reachability, "indeterminate");
    assert!(
        status.subjects[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("runtime.restart-window-reset"))
    );
}

#[test]
fn runtime_authority_preserves_append_fields_nulls_and_other_actual_kinds() {
    let subject = "agent/runtime-fence-fields";
    for missing in ["status", "runtime_id", "incarnation_id", "terminal", "reachability"] {
        let store = Store::open_memory("owner").unwrap();
        let original = json!({"status":"running","runtime_id":"r","incarnation_id":"i",
                              "terminal":true,"reachability":"reachable"});
        append(&store, "runtime.observed", subject, original.clone());
        append(&store, "runtime.observed", subject, json!({"reason":"later partial report"}));
        let narrow = store.runtime_authority(subject).unwrap().unwrap();
        let status = store.status(Some(subject)).unwrap();
        for field in ["status", "runtime_id", "incarnation_id", "terminal", "reachability"] {
            assert_eq!(narrow.actual.as_ref().unwrap().get(field), status.subjects[0].actual.as_ref().unwrap().get(field));
            assert_eq!(narrow.actual.as_ref().unwrap().get(field), original.get(field));
        }
        append(&store, "runtime.observed", subject, json!({missing: null}));
        let narrow = store.runtime_authority(subject).unwrap().unwrap();
        let status = store.status(Some(subject)).unwrap();
        assert_eq!(narrow.actual.as_ref().unwrap().get(missing), Some(&Value::Null));
        assert_eq!(narrow.reachability, status.subjects[0].reachability);
        assert_eq!(narrow.actual.as_ref().unwrap().get(missing), status.subjects[0].actual.as_ref().unwrap().get(missing));
        append(&store, "runtime.reconcile-decision", subject,
               json!({"decision":"hold","reachability":"unreachable"}));
        let narrow = store.runtime_authority(subject).unwrap().unwrap();
        let status = store.status(Some(subject)).unwrap();
        assert_eq!(narrow.reachability, "unreachable");
        assert_eq!(narrow.actual, status.subjects[0].actual);
    }
}

#[test]
fn subject_kind_lookup_seeks_distinct_kinds_at_pinned_cuts() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(
        "CREATE TABLE claims(subject TEXT,kind TEXT,store_index INTEGER);
             CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);",
    ).unwrap();
    let mut prior = None;
    for count in [32, 512, 7_900] {
        connection.execute("DELETE FROM claims", []).unwrap();
        connection.execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
                 INSERT INTO claims SELECT 'agent/worker','harness.diagnostic',i FROM n;
                 INSERT INTO claims VALUES ('agent/worker','runtime.future',{});", count + 1,
        )).unwrap();
        let mut statement = connection.prepare(SUBJECT_KINDS_AT_QUERY).unwrap();
        let kinds = statement.query_map(params!["agent/worker", count], |row| row.get::<_, String>(0))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(kinds, ["harness.diagnostic"]);
        let cost = statement.get_status(rusqlite::StatementStatus::VmStep);
        if let Some(prior) = prior { assert!(cost <= prior + 20, "{prior} -> {cost}"); }
        prior = Some(cost);
        let kinds = statement.query_map(params!["agent/worker", count + 1], |row| row.get::<_, String>(0))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(first_unknown_kind(kinds), Some("runtime.future".into()));
        let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {SUBJECT_KINDS_AT_QUERY}"))
            .unwrap().query_map(params!["agent/worker", 1], |row| row.get::<_, String>(3))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap().join("; ");
        assert!(plan.contains("kind>?") && plan.contains("store_index<?"), "{plan}");
    }
}

#[test]
fn conflict_lookup_seeks_operations_without_scanning_subject_history() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE claims(store_index INTEGER PRIMARY KEY, subject TEXT, kind TEXT, body TEXT);
                 CREATE INDEX claims_subject_index ON claims(subject,store_index);
                 CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);
                 CREATE INDEX claims_operation_index ON claims(json_extract(body,'$._operation.id'))
                   WHERE json_extract(body,'$._operation.id') IS NOT NULL;
                 CREATE TABLE operations(id TEXT PRIMARY KEY,state TEXT);
                 CREATE INDEX operations_conflict_index ON operations(id) WHERE state='conflict';",
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for n in 0..18 {
        transaction
            .execute(
                "INSERT INTO operations VALUES (?1,'conflict')",
                [format!("op/{n:02}")],
            )
            .unwrap();
    }
    for n in 1..=10_000 {
        transaction
            .execute(
                "INSERT INTO claims VALUES (?1,'agent/worker','runtime.observed','{}')",
                [n],
            )
            .unwrap();
    }
    transaction
        .execute_batch(
            "INSERT INTO claims VALUES
                   (10001,'agent/other','runtime.observed','{\"_operation\":{\"id\":\"op/00\"}}'),
                   (10002,'agent/worker','runtime.observed','{\"_operation\":{\"id\":\"op/17\"}}'),
                   (10003,'agent/worker','runtime.observed','{\"_operation\":{\"id\":\"op/01\"}}');",
        )
        .unwrap();
    transaction.commit().unwrap();
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {CONFLICT_CLAIM_QUERY}"))
        .unwrap()
        .query_map(params!["agent/worker", 10001], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(plan.contains("operations_conflict_index"), "{plan}");
    assert!(
        plan.contains("SEARCH claims USING INDEX claims_operation_index"),
        "{plan}"
    );
    connection
        .prepare_cached(CONFLICT_CLAIM_QUERY)
        .unwrap()
        .reset_status(rusqlite::StatementStatus::VmStep);
    assert_eq!(
        has_unknown_claim_at(&connection, "agent/worker", Some(10001)).unwrap(),
        None
    );
    let cost = connection
        .prepare_cached(CONFLICT_CLAIM_QUERY)
        .unwrap()
        .get_status(rusqlite::StatementStatus::VmStep);
    assert!(
        cost < 1_000,
        "unrelated history must not dominate the conflict check: {cost}"
    );
    assert_eq!(
        has_unknown_claim_at(&connection, "agent/worker", Some(10002)).unwrap(),
        Some("idempotency-conflict:op/17".into())
    );
    assert_eq!(
        has_unknown_claim_at(&connection, "agent/worker", Some(10003)).unwrap(),
        Some("idempotency-conflict:op/01".into())
    );
    connection
        .execute("UPDATE operations SET state='active'", [])
        .unwrap();
    assert_eq!(
        has_unknown_claim_at(&connection, "agent/worker", Some(10003)).unwrap(),
        None
    );
}

#[test]
fn replica_records_bear_authority_only_in_the_authority_families() {
    for (state, kind, bears) in [
        ("unknown", Some("harness.limits"), false),
        ("unknown", Some("message.tagged"), false),
        ("unknown", Some("runtime.lease"), true),
        ("unknown", Some("intent.paused"), true),
        ("unknown", Some("transport.future-observed"), true),
        ("unknown", None, true),
        ("invalid", Some("harness.limits"), true),
    ] {
        assert_eq!(
            replica_record_bears_authority(state, kind),
            bears,
            "{state} {kind:?}"
        );
    }
}

#[test]
fn older_registry_projects_missions_while_provenance_waits_for_upgrade() {
    let anchor_key = key();
    let older_key = key();
    let current = node("current", Some(&anchor_key), Some(&anchor_key));
    admit(&current, "current", &anchor_key, "anchor", None);
    admit(&current, "older", &older_key, "invite", None);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("older.sqlite3");
    let older = Store::open(&path, "older").unwrap();
    older.bind_fleet(FLEET).unwrap();
    older.pin_fleet_anchor(anchor_key.public()).unwrap();
    older.set_member_key(Some(older_key.clone())).unwrap();
    let mut registry = st3_schema::registry().clone();
    registry.claims.remove("mission.provenance").unwrap();
    older.set_claim_registry(registry);
    sync(&current, &older);
    let source = crate::provenance::tests::source("Build.", Some("Approved source."));
    let intent = crate::parse_intent(&source, "current").unwrap();
    current
        .apply_internal(&intent, "provenance-publication")
        .unwrap();
    let sidecar = current
        .latest_claim(
            &format!("mission/orchard@{}", intent.missions["orchard"].revision),
            Some("mission.provenance"),
        )
        .unwrap()
        .unwrap();
    assert!(checkpoint_rules::slot_of(&sidecar).is_none());
    let admission = sync(&current, &older);
    assert_eq!(admission.invalid, 0);
    assert_eq!(admission.unknown, 1);
    let mission = older.mission_spec("orchard", None).unwrap().unwrap();
    assert_eq!(mission, intent.missions["orchard"]);
    assert!(
        older
            .mission_provenance("orchard", &mission.revision)
            .unwrap()
            .is_none()
    );
    let run = older
        .create_mission_run(&MissionRunRequest {
            mission: "orchard".into(),
            revision: None,
            workspace: directory.path().display().to_string(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "old-build-run".into(),
        })
        .unwrap();
    assert_eq!(run.revision, mission.revision);
    assert!(!run.steps.is_empty());
    let before = older.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(before.invalid_records, 0);
    assert_eq!(before.unhealthy_projections, 0);
    drop(older);
    let upgraded = Store::open(&path, "older").unwrap();
    upgraded.set_member_key(Some(older_key)).unwrap();
    upgraded.validate_replication_backlog().unwrap();
    upgraded.project_replication_backlog().unwrap();
    assert_eq!(
        upgraded
            .mission_provenance("orchard", &mission.revision)
            .unwrap(),
        Some(intent.mission_provenance["orchard"].clone())
    );
    assert_eq!(
        upgraded
            .mission_run(&run.subject)
            .unwrap()
            .unwrap()
            .revision,
        run.revision
    );
}

#[test]
fn mixed_builds_keep_signed_unknown_claims_and_project_them_after_upgrade() {
    let anchor_key = key();
    let older_key = key();
    let current = node("current", Some(&anchor_key), Some(&anchor_key));
    admit(&current, "current", &anchor_key, "anchor", None);
    admit(&current, "older", &older_key, "invite", None);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("older.sqlite3");
    let older = Store::open(&path, "older").unwrap();
    older.bind_fleet(FLEET).unwrap();
    older.pin_fleet_anchor(anchor_key.public()).unwrap();
    older.set_member_key(Some(older_key.clone())).unwrap();
    let mut registry = st3_schema::registry().clone();
    registry.claims.remove("doc.bound").unwrap();
    older.set_claim_registry(registry);
    sync(&current, &older);
    older.begin_first_sync("current").unwrap();

    let first = current
        .put_document("doc/release", b"first", &None, "first")
        .unwrap();
    current
        .put_document(
            "doc/release",
            b"second",
            &Some(first.binding_claim_id),
            "second",
        )
        .unwrap();
    let later = note(&current, "A known claim after the unknown claims");
    let mut exchange = current
        .export_replication_exchange(FLEET, &older.replication_inventory().unwrap())
        .unwrap();
    assert!(
        exchange
            .envelopes
            .iter()
            .all(|envelope| envelope.signature.is_some())
    );
    // Receipt order must not choose the winning document, including during upgrade replay.
    exchange.envelopes.reverse();
    older
        .receive_replication_exchange("current", FLEET, &exchange)
        .unwrap();
    let summary = current.export_replication_summary(FLEET).unwrap();
    older
        .receive_replication_exchange_asking("current", FLEET, &summary, true)
        .unwrap();
    assert_eq!(older.first_sync().unwrap().unwrap().state, "syncing");
    let admission = older.validate_replication_backlog().unwrap();
    assert_eq!(admission.unknown, 2);
    assert_eq!(admission.invalid, 0);
    assert!(older.project_replication_backlog().unwrap());
    assert!(admitted(&older, &later));
    assert!(
        older
            .list_documents(Some("doc/release"), false, 10)
            .unwrap()
            .is_empty()
    );
    let local = note(&older, "The older member still writes and syncs");
    sync(&older, &current);
    assert!(admitted(&current, &local));

    // Equal wire logs from different registries are not projection divergence.
    for (from, to) in [(&current, &older), (&older, &current)] {
        let exchange = from.export_replication_summary(FLEET).unwrap();
        to
            .receive_replication_exchange_asking(&from.origin, FLEET, &exchange, true)
            .unwrap();
        let status = to
            .replication_status(true, Some(FLEET), std::slice::from_ref(&from.origin))
            .unwrap();
        assert!(status.peers[0].differing_tables.is_empty());
        assert!(status.peers[0].projection_comparison_waiting);
        assert!(!status.peers[0].sync.as_ref().unwrap().diverged);
        assert_eq!(status.unhealthy_projections, 0);
    }
    let before = older.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(before.waiting_claims, 2);
    assert_eq!(before.invalid_records, 0);
    let first_sync = before.first_sync.as_ref().unwrap();
    assert_eq!(first_sync.state, "verified");
    assert_eq!(
        first_sync.authority_digest.as_deref(),
        Some(before.authority_digest.as_str())
    );
    assert!(first_sync.graph_digest.is_none());
    let source = current.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(before.authority_digest, source.authority_digest);
    assert_ne!(before.graph_digest, source.graph_digest);
    // Unknown records are retained as authenticated, validated envelopes, not degradation.
    assert_eq!(
        older
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM replica_envelopes WHERE receipt_state<>'validated'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        0
    );
    drop(older);

    // The daemon startup path retries with the new registry, then canonically replays.
    let upgraded = Store::open(&path, "older").unwrap();
    upgraded.set_member_key(Some(older_key)).unwrap();
    let admission = upgraded.validate_replication_backlog().unwrap();
    assert_eq!(admission.unknown, 0);
    assert!(admission.changed);
    upgraded.apply_replication_repairs().unwrap();
    assert!(upgraded.project_replication_backlog().unwrap());
    let after = upgraded.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(after.waiting_claims, 0);
    assert_eq!(after.authority_digest, before.authority_digest);
    assert_eq!(after.graph_digest, source.graph_digest);
    assert_eq!(after.projection_digests, source.projection_digests);
    assert_eq!(
        upgraded
            .list_documents(Some("doc/release"), false, 10)
            .unwrap()[0]
            .hash,
        current
            .list_documents(Some("doc/release"), false, 10)
            .unwrap()[0]
            .hash
    );
    let exchange = current.export_replication_summary(FLEET).unwrap();
    upgraded
        .receive_replication_exchange_asking("current", FLEET, &exchange, true)
        .unwrap();
    // The first sync settled on the equal log; matching builds do not reopen it to compare
    // projections.
    let first_sync = upgraded.first_sync().unwrap().unwrap();
    assert_eq!(first_sync.state, "verified");
    assert_eq!(
        first_sync.authority_digest.as_deref(),
        Some(before.authority_digest.as_str())
    );
    assert!(first_sync.graph_digest.is_none());
    assert!(first_sync.message.is_none());
    assert!(
        upgraded.replication_peer_sync(&["current".into()])["current"]
            .graph_compared_at_unix_ms
            .is_some()
    );
}

#[test]
fn submitted_incarnation_waits_on_older_members_and_legacy_submissions_replay() {
    let anchor_key = key();
    let older_key = key();
    let current = node("current", Some(&anchor_key), Some(&anchor_key));
    admit(&current, "current", &anchor_key, "anchor", None);
    admit(&current, "older", &older_key, "invite", None);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("older.sqlite3");
    let older = Store::open(&path, "older").unwrap();
    older.bind_fleet(FLEET).unwrap();
    older.pin_fleet_anchor(anchor_key.public()).unwrap();
    older.set_member_key(Some(older_key.clone())).unwrap();
    let mut registry = st3_schema::registry().clone();
    registry
        .claims
        .get_mut("work.submitted")
        .unwrap()
        .fields
        .remove("submitted_incarnation")
        .unwrap();
    older.set_claim_registry(registry.clone());
    sync(&current, &older);
    let intent = crate::graph::parse_test_intent(
        r#"version 2
agent "garden/worker" { workspace "/tmp"; command "true" }
mission "garden" state="ready" {
  goal "Record and verify garden work submissions."
  step "new" { assigned-to "agent/garden/worker" }
  step "legacy" { assigned-to "agent/garden/worker" }
}
"#,
        "current",
    )
    .unwrap();
    current.apply_internal(&intent, "garden").unwrap();
    let run = current
        .create_mission_run(&MissionRunRequest {
            mission: "garden".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/ada".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "garden-run".into(),
        })
        .unwrap();
    let actor = "agent/garden/worker";
    let incarnation = "garden:one";
    let step = &run
        .steps
        .iter()
        .find(|step| step.step == "new")
        .unwrap()
        .subject;
    let legacy_step = &run
        .steps
        .iter()
        .find(|step| step.step == "legacy")
        .unwrap()
        .subject;
    let claim_step = |subject: &str| {
        current.set_step_state(subject, "ready", None).unwrap();
        current
            .work_action(
                subject,
                "claim",
                &WorkRequest {
                    wait_ms: None,
                    actor: Some(actor.into()),
                    incarnation: Some(incarnation.into()),
                    summary: None,
                    reason: None,
                    evidence: vec![],
                    idempotency_key: format!("claim-{subject}"),
                },
            )
            .unwrap();
    };
    claim_step(step);
    current
        .work_action(
            step,
            "complete",
            &WorkRequest {
                wait_ms: None,
                actor: Some(actor.into()),
                incarnation: Some(incarnation.into()),
                summary: None,
                reason: None,
                evidence: vec![],
                idempotency_key: "garden-submit".into(),
            },
        )
        .unwrap();
    let submitted = current
        .latest_claim(step, Some("work.submitted"))
        .unwrap()
        .unwrap();
    claim_step(legacy_step);
    let legacy = current
        .append_claim(&ClaimInput {
            subject: legacy_step.clone(),
            kind: "work.submitted".into(),
            actor: Some(actor.into()),
            fields: BTreeMap::from([
                ("attempt".into(), json!(1)),
                ("status".into(), json!("verifying")),
                ("worker_reported".into(), json!(true)),
                ("claimant".into(), Value::Null),
                ("claim_incarnation".into(), Value::Null),
                ("claim_expires_at_unix_ms".into(), Value::Null),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("legacy-submit".into()),
        })
        .unwrap();
    assert!(!crate::model::work_submission_matches(
        &legacy,
        legacy_step,
        1,
        actor,
        Some(incarnation),
        &[]
    ));
    assert!(!crate::model::work_submission_matches(
        &legacy,
        legacy_step,
        1,
        actor,
        None,
        &[]
    ));
    current.replay_replication_graph().unwrap();
    let legacy_view = current.step_run(legacy_step).unwrap().unwrap();
    assert_eq!(legacy_view.status, "verifying");
    assert!(legacy_view.submitted_incarnation.is_none());
    let fields = schema_fields_for_body(&submitted.kind, &submitted.body).unwrap();
    assert_eq!(
        registry
            .validate_claim(step, "work.submitted", &fields)
            .unwrap_err()
            .code,
        "unknown-claim-field"
    );
    assert!(matches!(
        classify_replicated_claim_with_registry(&submitted, &registry).unwrap(),
        ReplicatedClaimAdmission::UnknownField
    ));
    let later = note(&current, "A known claim after the submission");
    let admission = sync(&current, &older);
    assert_eq!(admission.unknown, 1);
    assert_eq!(admission.invalid, 0);
    assert!(!admitted(&older, &submitted));
    assert!(admitted(&older, &legacy));
    assert!(admitted(&older, &later));
    assert!(
        older
            .step_run(step)
            .unwrap()
            .unwrap()
            .submitted_incarnation
            .is_none()
    );
    assert_eq!(
        older.step_run(legacy_step).unwrap().unwrap().status,
        "verifying"
    );
    let local = note(&older, "The older member can still write known claims");
    sync(&older, &current);
    assert!(admitted(&current, &local));
    let before = older.replication_status(true, Some(FLEET), &[]).unwrap();
    let source = current.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(before.waiting_claims, 1);
    assert_eq!(before.invalid_records, 0);
    assert_eq!(before.unhealthy_projections, 0);
    assert_eq!(before.authority_digest, source.authority_digest);
    let waiting = older.replica_records(true).unwrap();
    assert_eq!(
        waiting[0].error_code.as_deref(),
        Some("unknown-claim-field")
    );
    drop(older);
    let upgraded = Store::open(&path, "older").unwrap();
    upgraded.set_member_key(Some(older_key)).unwrap();
    upgraded.validate_replication_backlog().unwrap();
    upgraded.apply_replication_repairs().unwrap();
    upgraded.project_replication_backlog().unwrap();
    let after = upgraded.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(after.waiting_claims, 0);
    assert_eq!(after.authority_digest, before.authority_digest);
    assert_eq!(after.graph_digest, source.graph_digest);
    assert_eq!(after.projection_digests, source.projection_digests);
    let learned = upgraded.claim_by_id(&submitted.id).unwrap().unwrap();
    assert_eq!(learned.body, submitted.body);
    let view = upgraded.step_run(step).unwrap().unwrap();
    assert_eq!(view.submitted_incarnation.as_deref(), Some(incarnation));
    assert!(view.claimant.is_none());
    assert!(view.claim_incarnation.is_none());
    assert!(
        upgraded
            .step_run(legacy_step)
            .unwrap()
            .unwrap()
            .submitted_incarnation
            .is_none()
    );
}

#[test]
fn a_step_wait_waits_on_older_members_and_converges_after_they_upgrade() {
    let anchor_key = key();
    let older_key = key();
    let current = node("current", Some(&anchor_key), Some(&anchor_key));
    admit(&current, "current", &anchor_key, "anchor", None);
    admit(&current, "older", &older_key, "invite", None);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("older.sqlite3");
    let older = Store::open(&path, "older").unwrap();
    older.bind_fleet(FLEET).unwrap();
    older.pin_fleet_anchor(anchor_key.public()).unwrap();
    older.set_member_key(Some(older_key.clone())).unwrap();
    // A build from before the wait: work.progress has neither new field.
    let mut registry = st3_schema::registry().clone();
    for field in ["clock", "wait_until_unix_ms"] {
        registry
            .claims
            .get_mut("work.progress")
            .unwrap()
            .fields
            .remove(field)
            .unwrap();
    }
    older.set_claim_registry(registry.clone());
    sync(&current, &older);
    let intent = crate::graph::parse_test_intent(
        r#"version 2
agent "garden/worker" { workspace "/tmp"; command "true" }
mission "garden" state="ready" {
  goal "Wait for a build slot without spending the execution clock."
  step "review" timeout="30m" { assigned-to "agent/garden/worker" }
}
"#,
        "current",
    )
    .unwrap();
    current.apply_internal(&intent, "garden").unwrap();
    let run = current
        .create_mission_run(&MissionRunRequest {
            mission: "garden".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/ada".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "garden-run".into(),
        })
        .unwrap();
    let step = run.steps[0].subject.clone();
    let request = |key: &str, wait_ms: Option<u64>, reason: Option<&str>| WorkRequest {
        wait_ms,
        actor: Some("agent/garden/worker".into()),
        incarnation: Some("garden:one".into()),
        summary: None,
        reason: reason.map(Into::into),
        evidence: vec![],
        idempotency_key: key.into(),
    };
    current.set_step_state(&step, "ready", None).unwrap();
    current
        .work_action(&step, "claim", &request("claim", None, None))
        .unwrap();
    current
        .work_action(
            &step,
            "wait",
            &request("wait", Some(60 * 60 * 1000), Some("build slot")),
        )
        .unwrap();
    let waiting = current
        .latest_claim(&step, Some("work.progress"))
        .unwrap()
        .unwrap();
    assert_eq!(waiting.body["fields"]["clock"], "paused");
    let fields = schema_fields_for_body(&waiting.kind, &waiting.body).unwrap();
    assert_eq!(
        registry
            .validate_claim(&step, "work.progress", &fields)
            .unwrap_err()
            .code,
        "unknown-claim-field"
    );
    assert!(matches!(
        classify_replicated_claim_with_registry(&waiting, &registry).unwrap(),
        ReplicatedClaimAdmission::UnknownField
    ));
    assert!(
        current
            .step_run(&step)
            .unwrap()
            .unwrap()
            .execution_paused_until_unix_ms
            .is_some()
    );

    // The older member keeps every claim it knows, holds the wait as an unknown record, and
    // never applies it, so it does not stop the clock until it is upgraded.
    let later = note(&current, "A known claim after the wait");
    let admission = sync(&current, &older);
    assert_eq!(admission.unknown, 1);
    assert_eq!(admission.invalid, 0);
    assert!(!admitted(&older, &waiting));
    assert!(admitted(&older, &later));
    let held = older.step_run(&step).unwrap().unwrap();
    assert_eq!(held.execution_paused_until_unix_ms, None);
    assert_eq!(held.execution_waited_ms, 0);
    let local = note(&older, "The older member can still write known claims");
    sync(&older, &current);
    assert!(admitted(&current, &local));
    let before = older.replication_status(true, Some(FLEET), &[]).unwrap();
    let source = current.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(before.waiting_claims, 1);
    assert_eq!(before.invalid_records, 0);
    assert_eq!(before.unhealthy_projections, 0);
    assert_eq!(before.authority_digest, source.authority_digest);
    assert_eq!(
        older.replica_records(true).unwrap()[0].error_code.as_deref(),
        Some("unknown-claim-field")
    );
    drop(older);

    // Upgrading retries the held claim with the new registry and converges on the same graph.
    let upgraded = Store::open(&path, "older").unwrap();
    upgraded.set_member_key(Some(older_key)).unwrap();
    upgraded.validate_replication_backlog().unwrap();
    upgraded.apply_replication_repairs().unwrap();
    upgraded.project_replication_backlog().unwrap();
    let after = upgraded.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(after.waiting_claims, 0);
    assert_eq!(after.authority_digest, before.authority_digest);
    assert_eq!(after.graph_digest, source.graph_digest);
    assert_eq!(after.projection_digests, source.projection_digests);
    let learned = upgraded.claim_by_id(&waiting.id).unwrap().unwrap();
    assert_eq!(learned.body, waiting.body);
    let view = upgraded.step_run(&step).unwrap().unwrap();
    assert!(view.execution_paused_until_unix_ms.is_some());
    assert_eq!(view.blocked_reason.as_deref(), Some("build slot"));
}

#[test]
fn a_member_leaves_once_and_a_second_leave_writes_nothing() {
    let anchor_key = key();
    let laptop_key = key();
    let anchor = node("anchor", Some(&anchor_key), Some(&anchor_key));
    anchor
        .admit_fleet_anchor(FLEET, anchor_key.public(), "listening")
        .unwrap();
    admit(&anchor, "laptop", &laptop_key, "invite", None);
    let laptop = node("laptop", Some(&laptop_key), Some(&anchor_key));
    sync(&anchor, &laptop);

    let first = laptop.leave_fleet("person/test").unwrap();
    let head = highest_sequence(&laptop, "laptop");
    assert_eq!(first.body["fields"]["high_water"], json!(head));
    let again = laptop.leave_fleet("person/test").unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(highest_sequence(&laptop, "laptop"), head);

    sync(&laptop, &anchor);
    let membership = anchor.fleet_membership().unwrap();
    let MemberState::Ended(ended) = membership.state("laptop") else {
        panic!("the anchor did not see the leave");
    };
    assert_eq!(ended.ended.as_deref(), Some("left"));
    assert_eq!(ended.end, Some(head));
}

fn highest_sequence(store: &Store, writer: &str) -> u64 {
    let connection = store.readers.get();
    connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM replica_envelopes WHERE writer=?1",
            [writer],
            |row| row.get(0),
        )
        .unwrap()
}

/// A publication's freshness is timed from this node's own commits: a claim another node
/// wrote, with that node's clock, is not one.
#[test]
fn the_first_local_commit_after_a_cut_skips_claims_another_node_wrote() {
    let member = key();
    let (_, a, stores) = fleet(&[("b", &member)]);
    let b = &stores[0];
    let before = b.index().unwrap();
    note(&a, "written on a");
    sync(&a, b);
    let replicated = b.index().unwrap();
    assert!(replicated > before, "a's claim reached b");
    assert_eq!(b.first_local_commit_after(before, replicated).unwrap(), None);
    let local = note(b, "written on b");
    let through = b.index().unwrap();
    assert_eq!(
        b.first_local_commit_after(before, through).unwrap(),
        Some(local.accepted_at_unix_ms)
    );
    assert_eq!(b.first_local_commit_after(through, through).unwrap(), None);
}

/// The anchor `a` founds the fleet and admits each named member.
fn fleet(members: &[(&str, &Arc<MemberKey>)]) -> (Arc<MemberKey>, Store, Vec<Store>) {
    let anchor = key();
    let a = node("a", Some(&anchor), Some(&anchor));
    admit(&a, "a", &anchor, "anchor", None);
    let mut stores = Vec::new();
    for (name, member) in members {
        admit(&a, name, member, "invite", None);
        stores.push(node(name, Some(member), Some(&anchor)));
    }
    for store in &stores {
        sync(&a, store);
    }
    (anchor, a, stores)
}

#[test]
fn keying_signs_every_own_envelope_and_later_batches_are_signed_as_written() {
    let a = Store::open_memory("a").unwrap();
    a.bind_fleet(FLEET).unwrap();
    note(&a, "before the key");
    let member = key();
    assert!(a.set_member_key(Some(member.clone())).unwrap() >= 1);
    note(&a, "after the key");
    let exchange = a
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    assert!(exchange.envelopes.len() >= 2);
    for envelope in &exchange.envelopes {
        assert_eq!(envelope.member_key.as_deref(), Some(member.public()));
        assert!(verify_signature(
            member.public(),
            &envelope_signature_message(FLEET, "a", envelope.sequence, &envelope.hash),
            envelope.signature.as_deref().unwrap(),
        ));
    }
}

#[test]
fn worker_signs_batches_already_sealed_by_an_unkeyed_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("claims.sqlite3");
    let worker = Store::open(&path, "alder").unwrap();
    worker.bind_fleet(FLEET).unwrap();
    let member = key();
    worker.set_member_key(Some(member.clone())).unwrap();
    let claim = note(&worker, "written while another process opens");
    // Startup seeds envelopes without a member key; the keyed worker has not seen this
    // batch yet. This is the main/replication-worker startup interleaving.
    let daemon = Store::open(&path, "alder").unwrap();
    let unsigned = daemon.replication_inventory().unwrap();
    assert!(!unsigned.envelopes.is_empty());
    let exchange = worker
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    let envelope = exchange.envelopes.iter().find(|e| e.sequence == 1).unwrap();
    assert!(verify_signature(
        member.public(),
        &envelope_signature_message(FLEET, "alder", envelope.sequence, &envelope.hash),
        envelope
            .signature
            .as_deref()
            .expect("already sealed batches must be signed")
    ));
    let receiver = node("birch", None, None);
    sync(&worker, &receiver);
    assert!(admitted(&receiver, &claim));
}

#[test]
fn staged_blobs_join_shared_authority_only_with_a_committed_claim() {
    let source = node("alder", None, None);
    let target = node("birch", None, None);
    let baseline = source.replication_status(true, Some(FLEET), &[]).unwrap();
    let hash = source.put_blob(b"staged upload").unwrap();
    assert_eq!(source.get_blob(&hash).unwrap().unwrap(), b"staged upload");
    assert_eq!(
        source
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .graph_digest,
        baseline.graph_digest
    );
    {
        let mut connection = source.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        promote_claim_blobs(&transaction, &json!({"blob_hash":hash})).unwrap();
        assert_ne!(
            projection_digest::tables(&transaction).unwrap()["blobs"],
            baseline.projection_digests["blobs"]
        );
        transaction.rollback().unwrap();
    }
    assert_eq!(
        source
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .graph_digest,
        baseline.graph_digest
    );
    append(
        &source,
        "file.observed",
        "file/alder:/example/result",
        json!({"status":"observed", "path":"/example/result", "blob_hash":hash}),
    );
    sync(&source, &target);
    assert_eq!(target.get_blob(&hash).unwrap().unwrap(), b"staged upload");
    assert_eq!(
        source
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .projection_digests,
        target
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .projection_digests
    );
}

#[test]
fn legacy_unreferenced_blobs_are_staged_without_losing_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("claims.sqlite3");
    let store = Store::open(&path, "alder").unwrap();
    let bytes = b"unreferenced legacy upload";
    let hash = hex::encode(Sha256::digest(bytes));
    let shared = store
        .put_document("doc/example", b"shared document", &None, "shared")
        .unwrap();
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO blobs VALUES(?1,?2,?3)",
                params![hash, bytes.as_slice(), bytes.len()],
            )
            .unwrap();
        connection
            .execute("DELETE FROM meta WHERE key='staged_blob_scope'", [])
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path, "alder").unwrap();
    assert_eq!(store.get_blob(&hash).unwrap().unwrap(), bytes);
    assert_eq!(
        store.get_blob(&shared.hash).unwrap().unwrap(),
        b"shared document"
    );
    let connection = store.readers.get();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        projection_digest::tables(&connection).unwrap(),
        projection_digest::oracle(&connection).unwrap()
    );
}

#[test]
fn arrangement_checkpoint_inputs_have_distinct_rules() {
    use smallclaims::store::checkpoint_agreement::{SealTerms, seal_difference};
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("claims.sqlite3"), "alder").unwrap();
    let connection = store.readers.get();
    for table in ["arrangements", "arrangement_registers"] {
        let count: u64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
    let current = projection_digest::oracle(&connection).unwrap();
    let legacy_tables = PROJECTION_DIGEST_TABLES
        .iter()
        .copied()
        .filter(|(table, _)| !matches!(*table, "arrangements" | "arrangement_registers"))
        .collect::<Vec<_>>();
    let legacy = smallclaims::store::projection_digest::oracle(&connection, &legacy_tables)
        .unwrap();
    assert_ne!(
        projection_digest::root(&current),
        projection_digest::root(&legacy)
    );
    // Actual v10 identity immediately before #1267; its description stayed unchanged.
    let legacy_rules = "d9a0e51ead09502fc373102832d96d530052735e53a75334b64906b149606fad";
    let legacy_terms = SealTerms {
        cut_unix_ms: 1,
        participants: BTreeSet::from(["alder".into(), "birch".into()]),
        sealed_digest: "same sealed claims".into(),
        rules_digest: legacy_rules.into(),
    };
    let current_terms = SealTerms {
        rules_digest: rules_digest(),
        ..legacy_terms.clone()
    };
    assert_eq!(seal_difference(&current_terms, &legacy_terms), "rules");
}

#[test]
fn a_store_without_an_anchor_admits_every_writer_as_before() {
    let b_key = key();
    let b = node("b", Some(&b_key), None);
    let legacy = node("legacy", None, None);
    let receiver = node("receiver", None, None);
    let signed = note(&b, "signed");
    let unsigned = note(&legacy, "unsigned");
    sync(&b, &receiver);
    sync(&legacy, &receiver);
    assert!(admitted(&receiver, &signed));
    assert!(admitted(&receiver, &unsigned));
    assert_eq!(receiver.fleet_membership().unwrap(), Default::default());
}

#[test]
fn fleet_membership_is_read_again_only_after_a_fleet_claim_or_signature_changes() {
    let b_key = key();
    let (_, a, stores) = fleet(&[("b", &b_key)]);
    let b = &stores[0];
    let ask = |store: &Store| {
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        let membership = store.fleet_membership().unwrap();
        (membership, STATEMENTS_RUN.with(std::cell::Cell::get) - before)
    };
    let (first, _) = ask(&a);
    let (again, spent) = ask(&a);
    assert_eq!(again, first);
    assert_eq!(spent, 1, "an unchanged fleet costs the generation read alone");
    // Ordinary claims, replicated or local, leave the generation where it was.
    note(b, "plain");
    sync(b, &a);
    note(&a, "local");
    let (after, spent) = ask(&a);
    assert_eq!(after, first);
    assert_eq!(spent, 1);
}

#[test]
fn a_keyed_writers_envelope_needs_its_signature_and_waits_for_it_as_unsigned() {
    let b_key = key();
    let (_, a, stores) = fleet(&[("b", &b_key)]);
    let b = &stores[0];
    let first = note(b, "signed");
    sync(b, &a);
    assert!(admitted(&a, &first));

    // An old build relays b's next envelope without its signature.
    let later = note(b, "relayed without a signature");
    let mut exchange = b
        .export_replication_exchange(FLEET, &a.replication_inventory().unwrap())
        .unwrap();
    for envelope in &mut exchange.envelopes {
        envelope.member_key = None;
        envelope.signature = None;
    }
    exchange.peer = "old-build".into();
    a.receive_replication_exchange("old-build", FLEET, &exchange)
        .unwrap();
    let admission = a.validate_replication_backlog().unwrap();
    assert_eq!(admission.held, 1);
    assert!(!admitted(&a, &later));
    assert_eq!(a.replication_signature_requests().unwrap().len(), 1);
    let status = a.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(status.unsigned_envelopes, 1);

    // The next exchange with a new build answers the request.
    sync(b, &a);
    assert!(admitted(&a, &later));
    assert!(a.replication_signature_requests().unwrap().is_empty());
    let status = a.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(status.unsigned_envelopes, 0);
}

#[test]
fn an_envelope_signed_by_another_members_key_or_unsigned_is_never_admitted() {
    let b_key = key();
    let (_, a, _) = fleet(&[("b", &b_key)]);
    // A machine that is not b writes under b's name, with its own key and without one.
    let forged_key = key();
    let forger = node("b", Some(&forged_key), None);
    let forged = note(&forger, "forged with another key");
    sync(&forger, &a);
    assert!(!admitted(&a, &forged));
    let unsigned_forger = node("b", None, None);
    let unsigned = note(&unsigned_forger, "forged without a key");
    sync(&unsigned_forger, &a);
    assert!(!admitted(&a, &unsigned));
    let status = a.replication_status(true, Some(FLEET), &[]).unwrap();
    assert!(status.unsigned_envelopes >= 2);
}

#[test]
fn a_wrong_key_signature_cannot_strand_an_envelope_its_writer_later_signs() {
    let b_key = key();
    let (_, a, stores) = fleet(&[("b", &b_key)]);
    let b = &stores[0];
    let genuine = note(b, "genuine");
    // A relay holding its own key re-signs b's exact envelope and delivers it first.
    let relay_key = key();
    let mut exchange = b
        .export_replication_exchange(FLEET, &a.replication_inventory().unwrap())
        .unwrap();
    for envelope in &mut exchange.envelopes {
        envelope.member_key = Some(relay_key.public().into());
        envelope.signature = Some(relay_key.sign(&envelope_signature_message(
            FLEET,
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
        )));
    }
    exchange.peer = "relay".into();
    a.receive_replication_exchange("relay", FLEET, &exchange)
        .unwrap();
    a.validate_replication_backlog().unwrap();
    assert!(!admitted(&a, &genuine));
    assert!(
        !a.replication_signature_requests().unwrap().is_empty(),
        "the envelope must keep asking for its writer's signature"
    );
    // The writer's signature, answered on request, admits it.
    sync(b, &a);
    assert!(admitted(&a, &genuine));
    let status = a.replication_status(true, Some(FLEET), &[]).unwrap();
    assert_eq!(status.unsigned_envelopes, 0);
    assert_eq!(status.fenced_envelopes, 0);
}

#[test]
fn a_removed_incarnation_is_fenced_everywhere_that_has_the_removal_even_through_a_relay() {
    let (b_key, c_key, r_key) = (key(), key(), key());
    let (_, a, stores) = fleet(&[("b", &b_key), ("c", &c_key), ("r", &r_key)]);
    let (b, c, r) = (&stores[0], &stores[1], &stores[2]);
    let before = note(r, "before the removal");
    sync(r, &a);
    assert!(admitted(&a, &before));

    // a removes r; c has not heard yet.
    remove(&a, "r", Some(&r_key), highest_sequence(&a, "r"));
    sync(&a, b);
    let after = note(r, "after the removal");
    // r admits a ghost member and tries to remove b.
    let ghost_key = key();
    admit(r, "ghost", &ghost_key, "invite", None);
    remove(r, "b", Some(&b_key), 1);
    // r writes as a with its own key.
    let posing = node("a", Some(&r_key), None);
    let posed = note(&posing, "r posing as a");

    sync(r, c);
    sync(&posing, c);
    assert!(admitted(c, &after), "c did not know of the removal yet");
    assert!(!admitted(c, &posed), "c knows a's key");

    // c relays everything to a and b, which have the removal.
    for informed in [&a, b] {
        sync(c, informed);
        assert!(!admitted(informed, &after));
        assert!(!admitted(informed, &posed));
        let membership = informed.fleet_membership().unwrap();
        assert_eq!(membership.state("ghost"), MemberState::NotMember);
        assert!(matches!(membership.state("b"), MemberState::Current(_)));
        assert!(matches!(membership.state("r"), MemberState::Ended(_)));
    }

    // c learns of the removal: it admits nothing more from r, and doctor reports the rest.
    sync(&a, c);
    let membership = c.fleet_membership().unwrap();
    assert_eq!(membership.state("ghost"), MemberState::NotMember);
    assert!(matches!(membership.state("b"), MemberState::Current(_)));
    let later = note(r, "later still");
    sync(r, c);
    assert!(!admitted(c, &later));
    let residue = c.fleet_admission_residue().unwrap();
    assert!(
        residue
            .iter()
            .any(|item| item.writer == "r" && item.reason == "admitted-beyond-high-water"),
        "{residue:?}"
    );
    assert!(a.fleet_admission_residue().unwrap().is_empty());
}

#[test]
fn a_legacy_writer_is_admitted_unsigned_until_a_keyless_removal_ends_its_window() {
    let (_, a, _) = fleet(&[]);
    let legacy = node("legacy", None, None);
    let first = note(&legacy, "legacy history");
    sync(&legacy, &a);
    assert!(admitted(&a, &first));
    let high_water = highest_sequence(&a, "legacy");
    remove(&a, "legacy", None, high_water);
    let after = note(&legacy, "after the removal");
    sync(&legacy, &a);
    assert!(!admitted(&a, &after));
    assert_eq!(
        a.fleet_membership().unwrap().state("legacy"),
        MemberState::LegacyRemoved(high_water)
    );
}

#[test]
fn a_rejoined_incarnation_starts_above_its_floor_and_the_old_key_stays_fenced() {
    let old_key = key();
    let (_, a, stores) = fleet(&[("b", &old_key)]);
    let old_b = &stores[0];
    note(old_b, "old history");
    sync(old_b, &a);
    let high_water = highest_sequence(&a, "b");
    remove(&a, "b", Some(&old_key), high_water);
    let late = note(old_b, "written after the removal");

    // The wiped machine joins again as b with a new key and a floor at the old head.
    let new_key = key();
    admit(&a, "b", &new_key, "invite", Some(high_water));
    let new_b = node("b", Some(&new_key), Some(&a_anchor(&a)));
    new_b.set_writer_floor(high_water).unwrap();
    sync(&a, &new_b);
    let fresh = note(&new_b, "new incarnation");
    sync(&new_b, &a);
    sync(old_b, &a);
    assert!(admitted(&a, &fresh));
    assert!(!admitted(&a, &late));
    assert!(matches!(
        a.fleet_membership().unwrap().state("b"),
        MemberState::Current(incarnation) if incarnation.member_key == new_key.public()
    ));
}

fn a_anchor(a: &Store) -> Arc<MemberKey> {
    a.member_key.read().unwrap().clone().unwrap()
}

#[test]
fn transport_link_expiry_uses_original_success_and_latest_status() {
    use smallclaims::store::{TRANSPORT_LINK_CLOCK_SKEW_MS, TRANSPORT_LINK_MAX_AGE_MS};
    let observer = node("cedar", None, None);
    let receiver = node("birch", None, None);
    let success = 1_000_000_u128;
    observer
        .record_transport_observation("elm", "up", None, Some(success))
        .unwrap();
    sync(&observer, &receiver);
    let expected = vec![("cedar".into(), "elm".into())];
    assert_eq!(
        receiver
            .transport_links_at(
                success + TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS - 1
            )
            .unwrap(),
        expected
    );
    assert!(
        receiver
            .transport_links_at(
                success + TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        receiver.transport_links().unwrap().is_empty(),
        "receiving an old success does not renew it"
    );

    // Only a fresh live success renews a stable up status, then it is deduplicated.
    observer
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    let count = observer
        .claims_for("host/elm", Some("transport.observed"))
        .unwrap()
        .len();
    assert_eq!(count, 2);
    observer
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    assert_eq!(
        observer
            .claims_for("host/elm", Some("transport.observed"))
            .unwrap()
            .len(),
        count
    );
    sync(&observer, &receiver);
    assert_eq!(receiver.transport_links().unwrap(), expected);

    observer
        .record_transport_observation("elm", "unknown", None, None)
        .unwrap();
    sync(&observer, &receiver);
    assert!(
        receiver.transport_links().unwrap().is_empty(),
        "latest unknown never revives an earlier up"
    );
}

#[test]
fn legacy_transport_links_expire_and_another_observer_cannot_suppress_refresh() {
    use smallclaims::store::{TRANSPORT_LINK_CLOCK_SKEW_MS, TRANSPORT_LINK_MAX_AGE_MS};
    let observer = node("cedar", None, None);
    let receiver = node("birch", None, None);
    let claim = append(
        &observer,
        "transport.observed",
        "host/elm",
        json!({"status":"up"}),
    );
    let accepted = claim.accepted_at_unix_ms;
    sync(&observer, &receiver);
    assert_eq!(
        receiver
            .transport_links_at(
                accepted + TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS - 1
            )
            .unwrap(),
        vec![("cedar".into(), "elm".into())]
    );
    assert!(
        receiver
            .transport_links_at(
                accepted + TRANSPORT_LINK_MAX_AGE_MS + TRANSPORT_LINK_CLOCK_SKEW_MS
            )
            .unwrap()
            .is_empty()
    );
    receiver
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    assert_eq!(
        receiver.transport_links().unwrap(),
        vec![
            ("birch".into(), "elm".into()),
            ("cedar".into(), "elm".into())
        ]
    );
}

#[test]
fn stable_transport_refresh_boundary_and_quiet_link_use_an_injected_clock() {
    use smallclaims::store::{TRANSPORT_LINK_REFRESH_MS, set_thread_clock};
    struct ResetClock;
    impl Drop for ResetClock {
        fn drop(&mut self) {
            set_thread_clock(None);
        }
    }
    let observer = node("cedar", None, None);
    let receiver = node("birch", None, None);
    let start = 2_000_000_000_000_u128;
    let _reset = ResetClock;
    let at = |time| {
        set_thread_clock(Some(time));
        observer.set_write_clock_at(time).unwrap();
    };
    at(start);
    observer
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    sync(&observer, &receiver);
    // Quiet listening links remain available beyond the old 90-second cutoff, without
    // generating refresh claims at each 30-60-second exchange.
    for elapsed in [
        30_000,
        60_000,
        90_000,
        120_000,
        TRANSPORT_LINK_REFRESH_MS - 1,
    ] {
        at(start + elapsed);
        observer
            .record_transport_observation("elm", "up", None, None)
            .unwrap();
        assert_eq!(
            observer
                .claims_for("host/elm", Some("transport.observed"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            receiver.transport_links().unwrap(),
            vec![("cedar".into(), "elm".into())]
        );
    }
    at(start + TRANSPORT_LINK_REFRESH_MS);
    observer
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    assert_eq!(
        observer
            .claims_for("host/elm", Some("transport.observed"))
            .unwrap()
            .len(),
        2
    );
    let refreshed = observer
        .latest_claim("host/elm", Some("transport.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(
        refreshed.body["fields"]["last_success_at"],
        json!(start + TRANSPORT_LINK_REFRESH_MS)
    );
    observer
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    assert_eq!(
        observer
            .claims_for("host/elm", Some("transport.observed"))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn replicated_transport_links_tolerate_bounded_skew_and_reject_far_future_evidence() {
    use smallclaims::store::{
        TRANSPORT_LINK_CLOCK_SKEW_MS, TRANSPORT_LINK_MAX_AGE_MS, set_thread_clock,
    };
    struct ResetClock;
    impl Drop for ResetClock {
        fn drop(&mut self) {
            set_thread_clock(None);
        }
    }
    let _reset = ResetClock;
    let read_at = 2_000_000_000_000_u128;
    set_thread_clock(Some(read_at));
    for success in [
        read_at - TRANSPORT_LINK_CLOCK_SKEW_MS,
        read_at + TRANSPORT_LINK_CLOCK_SKEW_MS,
    ] {
        let observer = node("cedar", None, None);
        let receiver = node("birch", None, None);
        observer.set_write_clock_at(read_at).unwrap();
        observer
            .record_transport_observation("elm", "up", None, Some(success))
            .unwrap();
        sync(&observer, &receiver);
        assert_eq!(
            receiver.transport_links_at(read_at).unwrap(),
            vec![("cedar".into(), "elm".into())]
        );
        if success > read_at {
            assert!(
                receiver.transport_links_at(read_at - 1).unwrap().is_empty(),
                "one millisecond past the skew allowance is rejected"
            );
        }
        assert!(
            receiver
                .transport_links_at(
                    read_at + TRANSPORT_LINK_MAX_AGE_MS + 2 * TRANSPORT_LINK_CLOCK_SKEW_MS
                )
                .unwrap()
                .is_empty()
        );
    }
    let future = node("cedar", None, None);
    future.set_write_clock_at(read_at).unwrap();
    future
        .record_transport_observation(
            "elm",
            "up",
            None,
            Some(read_at + TRANSPORT_LINK_CLOCK_SKEW_MS + 60_000),
        )
        .unwrap();
    assert!(
        future.transport_links_at(read_at).unwrap().is_empty(),
        "a far-future timestamp cannot keep a link up indefinitely"
    );
    set_thread_clock(Some(read_at + 1));
    future.set_write_clock_at(read_at + 1).unwrap();
    future
        .record_transport_observation("elm", "up", None, None)
        .unwrap();
    assert_eq!(
        future.transport_links_at(read_at).unwrap(),
        vec![("cedar".into(), "elm".into())],
        "a real exchange replaces unusable future evidence"
    );
}

#[test]
fn a_dial_out_member_is_never_observed_and_endpoints_are_announced_once() {
    let (d_key, s_key) = (key(), key());
    let anchor = key();
    let a = node("a", Some(&anchor), Some(&anchor));
    admit(&a, "a", &anchor, "anchor", None);
    admit(&a, "server", &s_key, "invite", None);
    append(
        &a,
        "fleet.member-admitted",
        "host/laptop",
        json!({
            "fleet_id": FLEET, "member_key": d_key.public(), "via": "invite",
            "mode": "dial-out", "sponsor": "host/a",
        }),
    );
    assert!(!a.observes_transport_to("laptop").unwrap());
    assert!(a.observes_transport_to("server").unwrap());
    assert!(a.observes_transport_to("legacy-peer").unwrap());

    let laptop = node("laptop", Some(&d_key), Some(&anchor));
    sync(&a, &laptop);
    assert!(
        laptop
            .publish_fleet_endpoints("dial-out", &[], "test")
            .unwrap()
    );
    assert!(
        !laptop
            .publish_fleet_endpoints("dial-out", &[], "test")
            .unwrap()
    );
    assert!(!laptop.observes_transport_to("a").unwrap());
    let endpoint = [json!({"transport": "loopback", "address": "127.0.0.1:1"})];
    assert!(
        laptop
            .publish_fleet_endpoints("listening", &endpoint, "test")
            .unwrap()
    );
    assert!(laptop.observes_transport_to("a").unwrap());
    sync(&laptop, &a);
    let membership = a.fleet_membership().unwrap();
    let MemberState::Current(incarnation) = membership.state("laptop") else {
        panic!("the laptop is current");
    };
    assert_eq!(incarnation.mode, "listening");
    assert_eq!(incarnation.endpoints, endpoint);
}

#[test]
fn fleet_claims_are_refused_on_the_public_claim_api() {
    let a = Store::open_memory("a").unwrap();
    let error = a
        .append_client_claim(&ClaimInput {
            subject: "host/ghost".into(),
            kind: "fleet.member-admitted".into(),
            actor: Some("person/ada".into()),
            fields: serde_json::from_value(json!({
                "fleet_id": FLEET,
                "member_key": "key",
                "via": "invite",
                "mode": "listening",
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap_err();
    assert_eq!(error.code, "claim-write-forbidden");
}

#[test]
fn a_store_pins_one_anchor() {
    let a = Store::open_memory("a").unwrap();
    a.pin_fleet_anchor("first").unwrap();
    a.pin_fleet_anchor("first").unwrap();
    assert!(a.pin_fleet_anchor("second").is_err());
    assert_eq!(a.fleet_anchor().unwrap().as_deref(), Some("first"));
}
