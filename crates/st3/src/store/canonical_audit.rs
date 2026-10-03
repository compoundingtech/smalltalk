// Regression written before the canonical-projections implementation. Compare logical rows,
// including columns the old graph digest did not cover. Local arrival cursors are tested through
// the shared readers they accelerate rather than compared as if they were replicated identities.
const SHARED_TABLES: &[(&str, &[&str])] = &[
    ("operations", &[]),
    ("blobs", &[]),
    ("documents", &["created_index"]),
    ("desired", &[]),
    ("message_index", &["created_index"]),
    ("resource_observations", &[]),
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
        "replication_refusals",
        "capabilities",
        "local_work_lease_renewals",
        "local_mailbox_owners",
        "local_mailbox_bindings",
        "local_observations",
        "local_blobs",
        "local_blob_uploads",
        "local_subscription_mission_deferrals",
        "local_usage_spend",
        "local_usage_responses",
        "local_limit_stops",
        "local_seat_accounts",
        "local_latest_slots",
        "local_resource_projection_pending",
        "graph_generation",
        "projection_digest_state",
        "projection_digest_generation",
        "projection_digest_operation_rows",
        "projection_digest_repaired_claims",
        "replica_envelope_signatures",
        "claim_signatures",
        "claim_verdicts",
        "claim_verdict_links",
        "claim_verdict_queue",
        "claim_verdict_fresh",
        "expected_claim_signatures",
        "held_keys",
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
    assert_eq!(
        SHARED_TABLES,
        projection_digest::TABLES,
        "every shared table must be in production digests and shuffle coverage"
    );
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
fn native_mailbox_ownership_changes_no_shared_projection_digest() {
    let store = Store::open_memory("alder").unwrap();
    crate::mailbox::tests::ready(&store, "session-1");
    let before = graph_digest(&store.readers.get()).unwrap();
    let first = crate::mailbox::Fence::new("agent/eval.worker", "session-1", "delivery");
    store.bind_mailbox(&first).unwrap();
    store
        .bind_mailbox(&crate::mailbox::Fence::new(
            "agent/eval.worker",
            "session-1",
            "delivery",
        ))
        .unwrap();
    assert_eq!(before, graph_digest(&store.readers.get()).unwrap());
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
        "work_action_extending",
        "events_after_bounded",
        "events_tail_bounded",
        "projection_time_at",
        "events_after_filtered",
        "member_reconcile_fault",
        "member_reconcile_faults_for",
        "claims_for_subject_kind_at",
        "timeline_claim_rows_for_incarnation_at",
        "claims_for_kind_at",
        // Node-local terminal history pagination; reason selection remains canonical.
        "outcome_history",
        "agent_last_activity_at",
        "try_project_simple_replication_tx",
        "export_replication_for_heads",
        "seed_replica_envelopes_tx",
        // Bounded mailbox pages expose local cursors, then complete readers sort source keys.
        "messages_page",
        "work_wake_messages_for_reconcile",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![root.join("store.rs")];
    let mut directories = vec![root.join("store")];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !["canonical_audit.rs", "convergence.rs"]
                    .contains(&path.file_name().unwrap().to_str().unwrap())
            {
                files.push(path);
            }
        }
    }
    let mut violations = Vec::new();
    for file in files {
        let contents = std::fs::read_to_string(&file).unwrap();
        let source = contents
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap();
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
            // A marker explicitly orders the fold canonically. With window functions the
            // rest of the same SQL literal may contain a snapshot bound after this clause.
            if order.trim_start().starts_with("CANONICAL_ASC(")
                || order.trim_start().starts_with("CANONICAL_DESC(")
            {
                continue;
            }
            if !order.contains("store_index") && !order.contains("created_index") {
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
                    "{}:{} {scope}",
                    file.strip_prefix(&root).unwrap().display(),
                    source[..offset]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "shared arrival-order folds:\n{}",
        violations.join("\n")
    );
}

pub(super) fn shared_rows(store: &Store) -> BTreeMap<String, Vec<String>> {
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
            let mut columns = columns
                .iter()
                .filter(|name| !local_columns.contains(&name.as_str()))
                .map(|name| {
                    if (*table == "blobs" && name == "bytes")
                        || (*table == "documents" && name == "binding_key")
                    {
                        format!("hex({name})")
                    } else {
                        name.clone()
                    }
                })
                .collect::<Vec<_>>();
            columns.sort();
            let query = if *table == "operations" {
                format!(
                    "SELECT * FROM ({}) ORDER BY 1",
                    projection_digest::operation_rows()
                )
            } else {
                format!(
                    "SELECT json_array({}) AS logical_row FROM {table} ORDER BY logical_row",
                    columns.join(",")
                )
            };
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
    if expected.glasses("person/ada", i64::MAX as u64).unwrap()
        != actual.glasses("person/ada", i64::MAX as u64).unwrap()
    {
        mismatches.push(format!("{phase}: glass bodies/revision sources"));
    }
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
    let expected_digests = projection_digest::tables(&expected.readers.get()).unwrap();
    let actual_digests = projection_digest::tables(&actual.readers.get()).unwrap();
    assert_eq!(
        expected_digests,
        projection_digest::oracle(&expected.readers.get()).unwrap(),
        "{phase}: expected cache"
    );
    assert_eq!(
        actual_digests,
        projection_digest::oracle(&actual.readers.get()).unwrap(),
        "{phase}: actual cache"
    );
    for table in projection_digest::differing(&expected_digests, &actual_digests) {
        mismatches.push(format!("{phase}: production digest {table}"));
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
    let document_views = |store: &Store| {
        let mut all = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .list_documents_page(
                    Some("doc/audit"),
                    None,
                    true,
                    after
                        .as_ref()
                        .map(|(name, index): &(String, u64)| (name.as_str(), *index)),
                    1,
                )
                .unwrap();
            let Some(version) = page.first() else {
                break;
            };
            after = Some((version.name.clone(), version.created_index));
            let mut view = serde_json::to_value(version).unwrap();
            view.as_object_mut().unwrap().remove("created_index");
            all.push(view);
        }
        let latest = store.list_documents(Some("doc/audit"), false, 10).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].hash, hex::encode(Sha256::digest(b"second")));
        all
    };
    if document_views(expected) != document_views(actual) {
        mismatches.push(format!("{phase}: document flags and paged versions"));
    }
    let messages = |store: &Store| {
        let mut value = serde_json::to_value(
            store
                .operational_messages(Some("person/avery"), false)
                .unwrap(),
        )
        .unwrap();
        for message in value.as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("created_index");
        }
        value
    };
    if messages(expected) != messages(actual) {
        mismatches.push(format!("{phase}: selected person messages and reminders"));
    }
    let search_messages = |store: &Store| {
        let mut value = serde_json::to_value(
            store.conversation_search_messages("person/avery", store.index().unwrap()).unwrap(),
        ).unwrap();
        for message in value.as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("created_index");
        }
        value
    };
    if search_messages(expected) != search_messages(actual) {
        mismatches.push(format!("{phase}: canonically bounded search messages"));
    }
    let proposal_views = |store: &Store| {
        let connection = store.readers.get();
        let runs = connection
            .prepare("SELECT DISTINCT run_id FROM revision_proposals ORDER BY run_id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        runs.into_iter()
            .map(|run| {
                serde_json::to_value(store.revision_proposal_for_run(&run).unwrap()).unwrap()
            })
            .collect::<Vec<_>>()
    };
    if proposal_views(expected) != proposal_views(actual) {
        mismatches.push(format!("{phase}: selected equal-time revision proposals"));
    }
    // Time evaluation and recipient filtering are shared only at identical explicit contexts.
    for person in [
        None,
        Some("person/avery"),
        Some("person/robin"),
        Some("person/operator"),
    ] {
        for as_of in [2_000_000_000_000_u128, 2_000_000_600_000_u128] {
            let attention = |store: &Store| {
                serde_json::to_value(store.attention_snapshot(person, as_of).unwrap()).unwrap()
            };
            if attention(expected) != attention(actual) {
                mismatches.push(format!(
                    "{phase}: attention snapshot at {as_of} for {person:?}"
                ));
            }
        }
    }
    // Each node names the same owner for each fault, so exactly one host tells that agent.
    for as_of in [2_000_000_000_000_u128, 2_000_000_600_000_u128] {
        let faults =
            |store: &Store| serde_json::to_value(store.fault_snapshot(as_of).unwrap()).unwrap();
        if faults(expected) != faults(actual) {
            mismatches.push(format!("{phase}: fault snapshot at {as_of}"));
        }
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
    let lane = |store: &Store| {
        crate::lane::replay(
            &lanes::lane_events_tx(&store.readers.get(), "lane/audit").unwrap(),
            20,
        )
    };
    if lane(expected) != lane(actual) {
        mismatches.push(format!("{phase}: lane membership and marks"));
    }
}

fn write_audit_history(source: &Store) {
    publish_takeover(
        source,
        &format!(
            "{TAKEOVER_SOURCE}\nagent \"alder.worker\" {{ workspace \"/tmp/audit\"; command \"true\" }}"
        ),
        "audit-desired",
    );
    source.append_claim(&ClaimInput {
        subject:"glass/person/ada/019a0000-0000-7000-8000-000000000001".into(), kind:"glass.upserted".into(), actor:Some("person/ada".into()),
        fields: serde_json::from_value(json!({"body":{"name":"Audit workspace","layout":{"tabs":[{"pane":"opaque:anything"}]}}, "base_revision":null})).unwrap(), evidence:vec![], expected_subject:None, idempotency_key:None,
    }).unwrap();
    let declared_message = r#"version 2
message "audit-declared" {
  from "agent/alder.worker"
  to "person/avery"
  content "Read this declarative message."
}
"#;
    let intent = crate::graph::parse_test_intent(declared_message, "alder").unwrap();
    source
        .apply_internal(&intent, "audit-declared-message")
        .unwrap();
    let failed = failed_takeover_run(source, &["deploy-check"]);
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
    // Repeated immutable bindings must select the same canonical representative even when
    // a later copy arrives first. The latest distinct version must remain the second one.
    {
        let mut connection = source.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let (name, kind, body): (String, String, String) = transaction
            .query_row(
                "SELECT subject,kind,body FROM claims WHERE kind='mission.published' LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        append_claim_tx(
            &transaction,
            &source.origin,
            &name,
            &kind,
            None,
            &serde_json::from_str::<Value>(&body).unwrap(),
            &[],
            None,
        )
        .unwrap();
        let hash = hex::encode(Sha256::digest(b"first"));
        append_claim_tx(
            &transaction,
            &source.origin,
            "doc/audit",
            "doc.bound",
            None,
            &json!({"name":"doc/audit","hash":hash,"size":5}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
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
        (
            "lane/audit",
            "lane.joined",
            json!({"entry":"resource/audit/pull-request/1","reason":"Queue the first change."}),
        ),
        (
            "lane/audit",
            "lane.joined",
            json!({"entry":"resource/audit/pull-request/2","reason":"Queue the second change."}),
        ),
        (
            "lane/audit",
            "lane.moved",
            json!({"entry":"resource/audit/pull-request/1","placement":"bottom"}),
        ),
        (
            "lane/audit",
            "lane.marked",
            json!({"entry":"resource/audit/pull-request/2","state":"ready","detail":"Checks passed."}),
        ),
        (
            "mission-run/audit-secondary",
            "mission-run.created",
            json!({
                "status":"running", "mission":"mission/takeover", "revision":failed.revision,
                "current_generation":"run-generation/audit-secondary", "workspace":"/tmp/audit",
                "requester":"person/avery", "inputs":{}, "mode":"run", "after":failed.subject,
                "timeout_ms":1000, "deadline_at_unix_ms":now_ms()+1000
            }),
        ),
        (
            "revision-proposal/audit",
            "revision-proposal.created",
            json!({
                "run":failed.subject,"source_generation":failed.generation,"candidate_revision":failed.revision,
                "reason":"Check two independent reviewers.","status":"pending-approval","cutover":"restart-active",
                "compatible_steps":[],"reviewers":["person/avery","person/robin"],"preview_hash":"audit-preview"
            }),
        ),
        (
            "revision-proposal/audit-other",
            "revision-proposal.created",
            json!({
                "run":failed.subject,"source_generation":failed.generation,"candidate_revision":failed.revision,
                "reason":"Compare an equal-time proposal.","status":"pending-approval","cutover":"restart-active",
                "compatible_steps":[],"reviewers":["person/avery","person/robin"],"preview_hash":"audit-other-preview"
            }),
        ),
        (
            "revision-proposal/audit",
            "revision-proposal.approved",
            json!({
                "reviewer":"person/avery","all_approved":false,"preview_hash":"audit-preview"
            }),
        ),
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
            "message/audit-person-one",
            "message.sent",
            json!({"from":"agent/alder.worker","to":"person/avery","content":"First unread item.","status":"sent"}),
        ),
        (
            "message/audit-person-two",
            "message.sent",
            json!({"from":"agent/alder.worker","to":"person/avery","content":"Second unread item.","status":"sent"}),
        ),
        (
            "message/audit-reminder-one",
            "message.sent",
            json!({"from":"agent/alder.worker","to":"person/avery","content":"Old reminder.","status":"sent","tags":["reminder:audit","version:1"]}),
        ),
        (
            "message/audit-reminder-two",
            "message.sent",
            json!({"from":"agent/alder.worker","to":"person/avery","content":"Current reminder.","status":"sent","tags":["reminder:audit","version:2"]}),
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
        (
            "resource/audit/pull-request/1",
            "resource.observed",
            json!({
                "kind": "vcs.pull-request",
                "facts": {"number": 1, "state": "open", "opened_by": "agent/alder.worker", "opened_by_run": "mission-run/audit"}
            }),
        ),
        (
            "resource/audit/pull-request/1",
            "resource.observed",
            json!({
                "kind": "vcs.pull-request",
                "facts": {"number": 1, "state": "merged", "opened_by": "agent/alder.worker", "opened_by_run": "mission-run/audit"}
            }),
        ),
    ];
    for (subject, kind, fields) in events {
        let mut connection = source.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let mut body = json!({"fields": fields});
        if subject == "host/beacon" && body["fields"]["status"] == "up" {
            body["_operation"] =
                json!({"id":"op/audit-transport", "request_digest":"audit-transport-digest"});
        }
        append_claim_tx(
            &transaction,
            &source.origin,
            subject,
            kind,
            Some("agent/alder.worker"),
            &body,
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
    source
        .ask_person(&crate::model::PersonAskRequest {
            legacy_request: None,
            person: "person/avery".into(),
            title: "Choose the release date".into(),
            reason: "Reply with a date.".into(),
            actor: "agent/alder.worker".into(),
            step: None,
            new_run: Some("audit-person-ask".into()),
            incarnation: None,
            idempotency_key: "audit-person-ask".into(),
            request: None,
        })
        .unwrap();
    source
        .record_operational_failure(
            "audit-disk-episode",
            &AttentionRequest {
                reviewer: "person/avery".into(),
                title: "Disk space is low".into(),
                reason: "Free disk space.".into(),
                severity: "warning".into(),
                targets: vec!["daemon/alder".into()],
                actor: "daemon/runtime".into(),
                idempotency_key: "audit-disk-episode".into(),
            },
        )
        .unwrap();
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

#[test]
fn fresh_and_additively_migrated_column_orders_have_identical_full_digests() {
    let temp = tempfile::tempdir().unwrap();
    let fresh = Store::open_memory("alder").unwrap();
    write_audit_history(&fresh);
    let before = projection_digest::tables(&fresh.readers.get()).unwrap();
    let path = temp.path().join("upgraded.sqlite3");
    let original_columns: Vec<String> = fresh
        .readers
        .get()
        .prepare("PRAGMA table_info(planning_sessions)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    fresh
        .connection
        .lock()
        .unwrap()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TRIGGER projection_digest_planning_sessions_insert;
        DROP TRIGGER projection_digest_planning_sessions_update;
        DROP TRIGGER projection_digest_planning_sessions_delete;
        CREATE TABLE saved_planner_specs AS SELECT id,planner_spec_json FROM planning_sessions;
        ALTER TABLE planning_sessions DROP COLUMN planner_spec_json;
        PRAGMA user_version=12;",
        )
        .unwrap();
    drop(connection);
    let upgraded = Store::open(&path, "alder").unwrap();
    {
        let connection = upgraded.connection.lock().unwrap();
        connection.execute_batch("UPDATE planning_sessions SET planner_spec_json=(
            SELECT planner_spec_json FROM saved_planner_specs WHERE saved_planner_specs.id=planning_sessions.id);
            DROP TABLE saved_planner_specs;").unwrap();
    }
    let connection = upgraded.readers.get();
    let upgraded_columns: Vec<String> = connection
        .prepare("PRAGMA table_info(planning_sessions)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_ne!(
        original_columns, upgraded_columns,
        "exercise an actual physical column reorder"
    );
    assert_eq!(before, projection_digest::tables(&connection).unwrap());
    assert_eq!(before, projection_digest::oracle(&connection).unwrap());
    drop(connection);
    assert_eq!(shared_rows(&fresh), shared_rows(&upgraded));
    drop(upgraded);
    let reopened = Store::open(&path, "alder").unwrap();
    assert_eq!(shared_rows(&fresh), shared_rows(&reopened));
    assert_eq!(
        before,
        projection_digest::tables(&reopened.readers.get()).unwrap()
    );
}

#[test]
fn incremental_digests_cover_each_shared_column_and_roll_back_with_rows() {
    let store = Store::open_memory("alder").unwrap();
    write_audit_history(&store);
    let mut connection = store.connection.lock().unwrap();
    // Deliberate projection corruption checks coverage independently of reducer semantics.
    connection
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .unwrap();
    let baseline = projection_digest::tables(&connection).unwrap();
    assert_eq!(baseline, projection_digest::oracle(&connection).unwrap());
    for (table, excluded) in SHARED_TABLES {
        let columns = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, u64>(0))
                .unwrap()
                > 0,
            "shuffle history must exercise {table}"
        );
        for (column, kind) in columns {
            let before_generation = projection_digest::generation(&connection).unwrap();
            let transaction = connection.transaction().unwrap();
            let expression = if *table == "claims" && column == "body" {
                // JSON expression indexes require valid JSON even for deliberate corruption.
                "json_set(body, '$.fields.__canonical_audit', 'changed')".to_owned()
            } else if *table == "operations" && column == "state" {
                "CASE state WHEN 'active' THEN 'conflict' ELSE 'active' END".to_owned()
            } else if *table == "desired" && column == "member" {
                // The host index requires valid JSON; concatenate to keep ordinary TEXT,
                // as bound reducer values are, rather than SQLite's transient JSON subtype.
                "json_set(COALESCE(member,'{}'),'$.__digest_audit',1)||''".to_owned()
            } else if kind == "INTEGER" {
                format!("COALESCE({column},0)+1")
            } else if kind == "BLOB" {
                format!("CAST({column}||x'00' AS BLOB)")
            } else {
                format!("COALESCE({column},'')||'-changed'")
            };
            transaction.execute(&format!("UPDATE {table} SET {column}={expression} WHERE rowid=(SELECT rowid FROM {table} LIMIT 1)"), []).unwrap();
            let current = projection_digest::tables(&transaction).unwrap();
            assert_eq!(
                current,
                projection_digest::oracle(&transaction).unwrap(),
                "{table}.{column}"
            );
            if excluded.contains(&column.as_str()) {
                assert_eq!(current, baseline, "local overlay {table}.{column}");
                assert_eq!(
                    projection_digest::generation(&transaction).unwrap(),
                    before_generation
                );
            } else {
                assert_ne!(
                    current[*table], baseline[*table],
                    "shared column {table}.{column}"
                );
                assert_ne!(
                    projection_digest::root(&current),
                    projection_digest::root(&baseline)
                );
            }
            transaction.rollback().unwrap();
            assert_eq!(projection_digest::tables(&connection).unwrap(), baseline);
            assert_eq!(
                projection_digest::generation(&connection).unwrap(),
                before_generation
            );
        }
    }
    // REPLACE has a delete followed by an insert, including when the primary key is unchanged.
    connection
        .execute(
            "INSERT OR REPLACE INTO message_index VALUES('message/audit',123,0)",
            [],
        )
        .unwrap();
    assert_eq!(
        projection_digest::tables(&connection).unwrap(),
        projection_digest::oracle(&connection).unwrap()
    );
    let generation = projection_digest::generation(&connection).unwrap();
    connection
        .execute("UPDATE message_index SET closed=closed", [])
        .unwrap();
    assert_eq!(
        projection_digest::generation(&connection).unwrap(),
        generation
    );
    STATEMENTS_RUN.with(|count| count.set(0));
    let _ = projection_digest::root(&projection_digest::tables(&connection).unwrap());
    assert_eq!(
        STATEMENTS_RUN.with(std::cell::Cell::get),
        1,
        "digest reads one small cache, regardless of history size"
    );
}

#[test]
fn comparable_peers_name_differing_tables_and_legacy_peers_keep_their_digest() {
    let source = Store::open_memory("alder").unwrap();
    write_audit_history(&source);
    let target = Store::open_memory("birch").unwrap();
    receive_and_project(
        &target,
        "alder",
        &exchange_from(&source, &ReplicationInventory::default()),
    );
    let mut summary = source.export_replication_summary(TEST_FLEET).unwrap();
    let legacy = summary.graph_digest.clone();
    let full = projection_digest::root(&summary.projection_digests);
    assert_ne!(legacy, full);
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["alder".into()])
        .unwrap();
    assert_eq!(status.graph_digest, full);
    assert_eq!(status.projection_digests.len(), SHARED_TABLES.len() + 1);
    // A planning mismatch remains visible even though the six-table compatibility hash agrees.
    summary
        .projection_digests
        .insert("planning_sessions".into(), "different".into());
    target
        .receive_replication_exchange("alder", TEST_FLEET, &summary)
        .unwrap();
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["alder".into()])
        .unwrap();
    assert_eq!(status.peers[0].differing_tables, vec!["planning_sessions"]);
    // Different inventories cannot identify projection divergence: the peer still lacks data.
    summary.inventory.digest = "different inventory".into();
    target
        .receive_replication_exchange("alder", TEST_FLEET, &summary)
        .unwrap();
    assert!(
        target
            .replication_status(true, Some(TEST_FLEET), &["alder".into()])
            .unwrap()
            .peers[0]
            .differing_tables
            .is_empty()
    );
    // Old peers omit modern maps. Their unchanged compatibility hash remains comparable.
    let mut old =
        serde_json::to_value(source.export_replication_summary(TEST_FLEET).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("projection_digests");
    let old: ReplicationExchange = serde_json::from_value(old).unwrap();
    assert!(old.projection_digests.is_empty());
    assert_eq!(old.graph_digest, legacy);
    let receipt = target
        .receive_replication_exchange("alder", TEST_FLEET, &old)
        .unwrap();
    assert!(!receipt.heal);
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["alder".into()])
        .unwrap();
    assert!(status.peers[0].projection_digests.is_empty());
    assert!(status.peers[0].differing_tables.is_empty());
    let mut answer = source
        .heal_answer("birch", &ReplicationHealQuery::Ranges)
        .unwrap();
    if let ReplicationHealAnswer::Ranges {
        projection_digests, ..
    } = &mut answer
    {
        projection_digests.clear();
    }
    assert!(
        matches!(target.heal_next("alder",answer).unwrap(),ReplicationHealStep::Done{report} if report.healed)
    );
}

#[test]
fn pending_local_claims_do_not_report_divergence_at_equal_sealed_inventory() {
    let source = Store::open_memory("alder").unwrap();
    let target = Store::open_memory("birch").unwrap();
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    receive_and_project(&target, "alder", &exchange);
    target
        .append_claim(&ClaimInput {
            subject: "observer/audit-pending".into(),
            kind: "observer.state".into(),
            actor: None,
            fields: BTreeMap::from([("state".into(), json!("unreachable"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("audit-pending".into()),
        })
        .unwrap();
    let status = target
        .replication_status_sealed(true, Some(TEST_FLEET), &["alder".into()])
        .unwrap();
    assert_eq!(
        target
            .sealed_replication_snapshot()
            .unwrap()
            .inventory
            .digest,
        exchange.inventory.digest
    );
    assert_ne!(status.projection_digests, exchange.projection_digests);
    assert!(status.peers[0].differing_tables.is_empty());
}

#[test]
fn proposal_phase_dates_match_source_replay_and_replication() {
    for reviewed in [false, true] {
        let source = Store::open_memory("alder").unwrap();
        source.set_write_clock_at(now_ms() + 10_000).unwrap();
        let publish = |goal: &str, key: &str| {
            let reviewers = if reviewed {
                "revisions=\"human-only\" revision-reviewer=\"person/robin\""
            } else {
                ""
            };
            let kdl = format!(
                r#"
version 2
mission "audit-phase" state="ready" revision-cutover="when-idle" {reviewers} {{
  goal "Compare proposal phases."
  agent "owner" {{ workspace "."; command "true" }}
  step "work" {{ goal {goal:?} }}
}}
"#
            );
            publish_mission(&source, &kdl, key)
        };
        let first = publish("First goal.", "audit-phase-one");
        let run = source
            .create_mission_run(&MissionRunRequest {
                mission: first.id,
                revision: None,
                workspace: "/tmp/audit".into(),
                requester: Some("person/avery".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "audit-phase-run".into(),
            })
            .unwrap();
        let second = publish("Second goal.", "audit-phase-two");
        let target = Store::open_memory("birch").unwrap();
        let check = |phase: &str| {
            let before = shared_rows(&source);
            source.replay_replication_graph().unwrap();
            assert_eq!(before, shared_rows(&source), "{phase}: source/replay");
            target
                .import_replication("alder", &source.export_replication(0).unwrap())
                .unwrap();
            assert_eq!(before, shared_rows(&target), "{phase}: replica");
            assert_eq!(
                projection_digest::tables(&source.readers.get()).unwrap(),
                projection_digest::tables(&target.readers.get()).unwrap(),
                "{phase}: cached digests"
            );
        };
        let proposal = source
            .create_revision_proposal(
                &run.id,
                &second,
                "person/avery",
                "Compare admission with replay.",
                "audit-phase-create",
            )
            .unwrap();
        check("created");
        if reviewed {
            source
                .approve_revision_proposal(
                    &proposal.id,
                    "person/robin",
                    proposal.preview_hash.as_deref().unwrap(),
                    "audit-phase-approve",
                )
                .unwrap();
            check("approved/draining");
        }
        source
            .cancel_revision_proposal(
                &proposal.id,
                "person/avery",
                Some("Try another cutover."),
                "audit-phase-cancel",
            )
            .unwrap();
        check("cancelled");
        let replacement = source
            .create_revision_proposal(
                &run.id,
                &second,
                "person/avery",
                "Apply the idle replacement.",
                "audit-phase-replace",
            )
            .unwrap();
        if reviewed {
            source
                .approve_revision_proposal(
                    &replacement.id,
                    "person/robin",
                    replacement.preview_hash.as_deref().unwrap(),
                    "audit-phase-reapprove",
                )
                .unwrap();
        }
        check("replacement/draining");
        assert!(source.apply_drained_revision(&run.id).unwrap().is_some());
        check("applied");
    }
}
