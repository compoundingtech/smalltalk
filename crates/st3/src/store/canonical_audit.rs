// Regression written before the canonical-projections implementation. Compare logical rows,
// including columns the old graph digest did not cover. Local arrival cursors are tested through
// the shared readers they accelerate rather than compared as if they were replicated identities.
const SHARED_TABLES: &[(&str, &[&str])] = &[
    ("operations", &[]),
    ("blobs", &[]),
    ("documents", &["created_index"]),
    ("desired", &[]),
    ("message_index", &["created_index"]),
    ("mission_revisions", &["created_index"]),
    ("mission_definitions", &[]),
    ("mission_runs", &[]),
    ("mission_run_deadlines", &[]),
    ("mission_run_after", &[]),
    ("run_generations", &[]),
    (
        "step_runs",
        &["lease_expires_at_unix_ms", "updated_at_unix_ms"],
    ),
    ("revision_proposals", &[]),
    ("planning_sessions", &[]),
    ("planning_candidates", &[]),
    ("planning_previews", &[]),
];

#[test]
fn every_persistent_table_has_a_projection_scope() {
    // Explicit local/storage exceptions from docs/st3/canonical-projections-audit.md. New
    // tables must be classified; shared tables automatically join the row/digest comparison.
    let exceptions = [
        "meta",
        "batches",
        "claims",
        "idempotency",
        "mission_run_requests",
        "events",
        "peer_cursors",
        "peer_replica_cursors",
        "replica_envelopes",
        "replica_records",
        "projection_health",
        "replication_peers",
        "capabilities",
        "local_work_lease_renewals",
        "local_observations",
        "local_subscription_mission_deferrals",
        "local_usage_totals",
        "local_usage_seen",
        "local_latest_slots",
        "graph_generation",
        "replica_envelope_signatures",
        "fleet_invite_tokens",
        "replica_envelope_holds",
        "checkpoint_envelopes",
        "checkpoint_claims",
        "checkpoints",
    ];
    let classified = SHARED_TABLES
        .iter()
        .map(|(name, _)| *name)
        .chain(exceptions)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let store = Store::open_memory("alder").unwrap();
    let connection = store.readers.get();
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    assert_eq!(
        tables, classified,
        "classify new tables and include shared logical rows in both ordering and digest tests"
    );
}

#[test]
fn shared_folds_never_order_by_local_arrival() {
    // Local purposes and immutable intra-batch export exceptions are individually documented
    // in the audit. A new raw shared ordering fails by default. The fix step will route every
    // remaining violation through the common canonical helper, including the multiline queue.
    let allowed = [
        "AGENT_STATUS_INDEX_QUERY",
        "claims_page_query",
        "agent_projection_index",
        "work_action",
        "events_after_bounded",
        "events_tail_bounded",
        "projection_time_at",
        "events_after_filtered",
        "member_reconcile_fault",
        "member_reconcile_faults_for",
        "claims_for_subject_kind_at",
        "timeline_claim_rows_for_incarnation_at",
        "claims_for_kind_at",
        "agent_last_activity_at",
        "try_project_simple_replication_tx",
        "export_replication_for_heads",
        "seed_replica_envelopes_tx",
    ];
    let source = include_str!("../store.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap();
    let mut violations = Vec::new();
    for (offset, _) in source.match_indices("ORDER BY") {
        let order = source[offset + "ORDER BY".len()..]
            .split('"')
            .next()
            .unwrap()
            .split("LIMIT")
            .next()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        if !order.contains("store_index") {
            continue;
        }
        let scope = source[..offset]
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                if line.starts_with("//") {
                    return None;
                }
                if let Some((_, name)) = line.split_once("fn ") {
                    Some(name.split(['(', '<']).next().unwrap())
                } else if let Some(name) = line.strip_prefix("const ") {
                    Some(name.split(':').next().unwrap())
                } else {
                    None
                }
            })
            .next_back()
            .unwrap();
        if !allowed.contains(&scope) {
            violations.push(format!(
                "store.rs:{} {scope}",
                source[..offset]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "shared arrival-order folds:\n{}",
        violations.join("\n")
    );
}

fn shared_rows(store: &Store) -> BTreeMap<String, Vec<String>> {
    let connection = store.readers.get();
    SHARED_TABLES
        .iter()
        .map(|(table, local_columns)| {
            let columns = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let columns = columns
                .iter()
                .filter(|name| !local_columns.contains(&name.as_str()))
                .map(|name| {
                    if *table == "blobs" && name == "bytes" {
                        "hex(bytes)".to_owned()
                    } else {
                        name.clone()
                    }
                })
                .collect::<Vec<_>>();
            let query = format!(
                "SELECT json_array({}) AS logical_row FROM {table} ORDER BY logical_row",
                columns.join(",")
            );
            let rows = connection
                .prepare(&query)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (table.to_string(), rows)
        })
        .collect()
}

fn table_digest(rows: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-projection-audit-v1\0");
    for row in rows {
        digest.update((row.len() as u64).to_be_bytes());
        digest.update(row.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn compare_shared(expected: &Store, actual: &Store, phase: &str, mismatches: &mut Vec<String>) {
    let expected_rows = shared_rows(expected);
    for (table, rows) in shared_rows(actual) {
        let wanted = &expected_rows[&table];
        if rows != *wanted {
            mismatches.push(format!(
                "{phase}: {table} rows ({} versus {})",
                wanted.len(),
                rows.len()
            ));
        }
        if table_digest(&rows) != table_digest(wanted) {
            mismatches.push(format!("{phase}: {table} digest"));
        }
    }
    if graph_digest_of(expected) != graph_digest_of(actual) {
        mismatches.push(format!("{phase}: graph_digest"));
    }
    // Document selection is shared even though the current index used to select it is local.
    let documents = BTreeSet::from(["doc/audit".to_owned()]);
    if expected.document_bindings(&documents).unwrap()
        != actual.document_bindings(&documents).unwrap()
    {
        mismatches.push(format!("{phase}: selected document bindings"));
    }
    let message_state = |store: &Store| {
        store
            .message("message/audit")
            .unwrap()
            .map(|message| message.status)
    };
    if message_state(expected) != message_state(actual) {
        mismatches.push(format!("{phase}: message lifecycle"));
    }
    let usage = |store: &Store| {
        serde_json::to_value(
            store
                .usage_summary_at("agent/alder.worker", None, None)
                .unwrap(),
        )
        .unwrap()
    };
    if usage(expected) != usage(actual) {
        mismatches.push(format!("{phase}: usage summary"));
    }
    let targets = vec!["observer/audit".into(), "loop-run/audit/repeat".into()];
    let target_states = |store: &Store| {
        serde_json::to_value(store.attention_target_states(&targets).unwrap()).unwrap()
    };
    if target_states(expected) != target_states(actual) {
        mismatches.push(format!("{phase}: attention source states"));
    }
    if expected.reconcile_fault("daemon/alder", "audit").unwrap()
        != actual.reconcile_fault("daemon/alder", "audit").unwrap()
    {
        mismatches.push(format!("{phase}: fault/recovery episode"));
    }
    if expected.usage_period_rows(0, 3000).unwrap() != actual.usage_period_rows(0, 3000).unwrap() {
        mismatches.push(format!("{phase}: period usage"));
    }
    if expected.transport_links().unwrap() != actual.transport_links().unwrap() {
        mismatches.push(format!("{phase}: replicated transport observations"));
    }
}

fn write_audit_history(source: &Store) {
    failed_takeover_run(source, &["deploy-check"]);
    source
        .put_document("doc/audit", b"first", &None, "audit-document-first")
        .unwrap();
    source
        .put_document(
            "doc/audit",
            b"second",
            &source.latest_document_token("doc/audit").unwrap(),
            "audit-document-second",
        )
        .unwrap();
    // Equal acceptance times require the writer/sequence/position parts of the total order.
    source
        .connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO temp.write_clock(offset_ms, at_ms) VALUES (0, ?1)",
            [now_ms() as i64],
        )
        .unwrap();
    // Raw claim append is the daemon's internal writer path. Replicas still perform ordinary
    // schema admission, envelope verification and projection; no projection rows are seeded.
    let events = [
        ("host/beacon", "transport.observed", json!({"status": "up"})),
        (
            "host/beacon",
            "transport.observed",
            json!({"status": "unknown"}),
        ),
        (
            "planning-session/audit",
            "planning-session.started",
            json!({
                "mission": "mission/audit", "request": "doc/audit-request@request-hash",
                "workspace": "/tmp/audit", "requester": "person/avery", "planner": "agent/alder.worker"
            }),
        ),
        (
            "planning-session/audit",
            "planning-session.candidate-submitted",
            json!({
                "variant": "default", "candidate_revision": 1,
                "markdown": "doc/audit-markdown@markdown-hash", "kdl": "doc/audit-kdl@kdl-hash",
                "mission_revision": "audit-revision"
            }),
        ),
        (
            "planning-session/audit",
            "planning-session.previewed",
            json!({
                "variant": "default", "candidate_revision": 1, "preview_hash": "audit-preview",
                "store_index": 17, "graph": "graph", "diff": "diff", "mission": {
                    "store_index": 17, "source_hash": "audit-source", "normalized": {},
                    "resolved_intent": {"kdl": "version 2\n"}, "changes": [],
                    "predicted_actions": [], "blockers": [], "warnings": [],
                    "subject_tokens": {}, "mission_revisions": {}
                }
            }),
        ),
        (
            "planning-session/audit",
            "planning-session.approved",
            json!({"mission_revision": "audit-revision"}),
        ),
        (
            "message/audit",
            "message.sent",
            json!({
                "from": "agent/alder.worker", "to": "agent/birch.worker", "content": "Audit the shared views.", "status": "sent"
            }),
        ),
        (
            "message/audit",
            "message.closed",
            json!({"status": "closed"}),
        ),
        (
            "observer/audit",
            "observer.state",
            json!({"state": "unreachable"}),
        ),
        (
            "observer/audit",
            "observer.state",
            json!({"state": "healthy"}),
        ),
        (
            "loop-run/audit/repeat",
            "loop.state",
            json!({"status": "failed", "round": 1}),
        ),
        (
            "loop-run/audit/repeat",
            "loop.state",
            json!({"status": "running", "round": 2}),
        ),
        (
            "daemon/alder",
            "reconcile.fault",
            json!({"scope": "audit", "status": "faulted", "reason": "Retry the probe."}),
        ),
        (
            "daemon/alder",
            "reconcile.fault",
            json!({"scope": "audit", "status": "recovered"}),
        ),
        (
            "agent/alder.worker",
            "runtime.observed",
            json!({"status": "running", "incarnation_id": "audit-incarnation"}),
        ),
        (
            "agent/alder.worker",
            "harness.usage",
            json!({
                "driver": "codex",
                "semantics": "response_rollup", "incarnation_id": "audit-incarnation",
                "model": "audit-model", "owner_run": "", "owner_step": "", "host": "alder",
                "total_tokens": 10, "input_tokens": 8, "output_tokens": 2, "observed_at_unix_ms": 1000
            }),
        ),
        (
            "agent/alder.worker",
            "harness.usage",
            json!({
                "driver": "codex",
                "semantics": "response_rollup", "incarnation_id": "audit-incarnation",
                "model": "audit-model", "owner_run": "", "owner_step": "", "host": "alder",
                "total_tokens": 30, "input_tokens": 20, "output_tokens": 10, "observed_at_unix_ms": 2000
            }),
        ),
    ];
    for (subject, kind, fields) in events {
        let mut connection = source.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &source.origin,
            subject,
            kind,
            Some("agent/alder.worker"),
            &json!({"fields": fields}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    for (state, observed_at) in [("idle", 1000), ("working", 2000), ("idle", 3000)] {
        source
            .append_claim_outcome(&harness_state("agent/alder.worker", state, observed_at))
            .unwrap();
    }
    source.replay_replication_graph().unwrap();
}

fn trim_for_audit(store: &Store, cut: u128) {
    let sealed = store.checkpoint_sealed_set(cut).unwrap();
    let plan = checkpoint::plan_drops(&sealed);
    assert!(
        !plan.claims.is_empty(),
        "the checkpoint must exercise a real drop"
    );
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    checkpoint::record_checkpoint_tombstones_tx(
        &transaction,
        &checkpoint_name(cut),
        &plan.envelopes,
        &plan.claims,
    )
    .unwrap();
    checkpoint::delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims).unwrap();
    replay_graph_from_nothing_tx(&transaction).unwrap();
    transaction.commit().unwrap();
}

#[test]
fn every_shared_projection_agrees_after_shuffle_restart_and_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let source = Store::open_memory("alder").unwrap();
    write_audit_history(&source);
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    let count = exchange.envelopes.len();
    let reference = Store::open_memory("birch").unwrap();
    receive_and_project(&reference, "alder", &exchange);
    // Reverse and odd/even schedules exercise both backwards histories and interleaving.
    let schedules = [
        ("reverse", (0..count).rev().collect::<Vec<_>>()),
        (
            "interleaved",
            (1..count).step_by(2).chain((0..count).step_by(2)).collect(),
        ),
    ];
    let mut mismatches = Vec::new();
    let cut = now_ms() + 1000;
    for (name, order) in schedules {
        let path = directory.path().join(format!("{name}.sqlite3"));
        let mut target = Store::open(&path, name).unwrap();
        for index in order {
            receive_and_project(
                &target,
                "alder",
                &exchange_of("alder", vec![exchange.envelopes[index].clone()]),
            );
        }
        // Check the schedule really changed local admission order, not only transport order.
        assert_ne!(
            reference
                .claims_for("planning-session/audit", None)
                .unwrap()[0]
                .store_index,
            target.claims_for("planning-session/audit", None).unwrap()[0].store_index
        );
        compare_shared(
            &reference,
            &target,
            &format!("{name}/arrival"),
            &mut mismatches,
        );
        drop(target);
        target = Store::open(&path, name).unwrap();
        target.replay_replication_graph().unwrap();
        compare_shared(
            &reference,
            &target,
            &format!("{name}/restart-replay"),
            &mut mismatches,
        );
        let scratch = tempfile::tempdir().unwrap();
        let (_, _, proof) = target
            .plan_checkpoint_through(cut, None, scratch.path())
            .unwrap();
        if !proof.passed {
            mismatches.push(format!("{name}/checkpoint proof: {:?}", proof.mismatches));
        }
        // Use the production tombstone/delete/replay path on isolated stores. Certificate and
        // fleet protocol behavior already has separate tests; this tests the projection boundary.
        let checkpoint_reference = Store::open_memory("cedar").unwrap();
        receive_and_project(&checkpoint_reference, "alder", &exchange);
        trim_for_audit(&checkpoint_reference, cut);
        trim_for_audit(&target, cut);
        compare_shared(
            &checkpoint_reference,
            &target,
            &format!("{name}/checkpoint"),
            &mut mismatches,
        );
    }
    assert!(
        mismatches.is_empty(),
        "shared projection divergence:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn equal_time_writers_choose_the_same_shared_source() {
    let at = now_ms();
    let writers = [
        Store::open_memory("alder").unwrap(),
        Store::open_memory("cedar").unwrap(),
    ];
    let mut envelopes = Vec::new();
    for (writer, state) in writers.iter().zip(["unreachable", "healthy"]) {
        writer.set_write_clock_at(at).unwrap();
        writer
            .append_claim(&ClaimInput {
                subject: "observer/audit".into(),
                kind: "observer.state".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), json!(state))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("audit-{state}")),
            })
            .unwrap();
        envelopes.extend(exchange_from(writer, &ReplicationInventory::default()).envelopes);
    }
    let ordered = Store::open_memory("birch").unwrap();
    let reversed = Store::open_memory("elm").unwrap();
    for target in [&ordered, &reversed] {
        let order = if target.origin == "birch" {
            envelopes.clone()
        } else {
            envelopes.iter().rev().cloned().collect()
        };
        for envelope in order {
            receive_and_project(target, "relay", &exchange_of("relay", vec![envelope]));
        }
        let claims = target.claims_for("observer/audit", None).unwrap();
        assert_eq!(
            claims
                .iter()
                .map(|claim| canonical::claim_key(&target.readers.get(), &claim.id).unwrap())
                .collect::<Vec<_>>(),
            {
                let mut keys = claims
                    .iter()
                    .map(|claim| canonical::claim_key(&target.readers.get(), &claim.id).unwrap())
                    .collect::<Vec<_>>();
                keys.sort();
                keys
            }
        );
        let states = target
            .attention_target_states(&["observer/audit".into()])
            .unwrap();
        assert_eq!(states[0].state, "healthy");
    }
    assert_eq!(
        serde_json::to_value(ordered.latest_actual_value("observer/audit").unwrap()).unwrap(),
        serde_json::to_value(reversed.latest_actual_value("observer/audit").unwrap()).unwrap()
    );
}
