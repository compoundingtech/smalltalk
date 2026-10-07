use super::*;

const AGENT: &str = "agent/amber.sample";

// Obtain an opaque context from a real Installer callback. This fixture operator cannot
// publish, and its test-only source registration supplies no production source coverage.
fn context(store: &Store) -> Namespace {
    use smallclaims::ivm::install::{Installer, Limits, Mutation, Operator, ScanPage};
    struct Capture {
        name: &'static str,
        context: Arc<std::sync::Mutex<Option<Namespace>>>,
    }
    impl Operator for Capture {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fingerprint(&self) -> &'static str {
            FINGERPRINT
        }
        fn source(&self) -> &'static str {
            "lifecycle-fixture-only"
        }
        fn create_schema(&self, c: &Connection) -> Result<()> {
            create_schema(c)
        }
        fn apply(&self, _: &Transaction<'_>, ns: &Namespace, _: &[Mutation]) -> Result<bool> {
            *self.context.lock().unwrap() = Some(ns.clone());
            Ok(false)
        }
        fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
            anyhow::bail!("namespace fixture cannot certify a public card")
        }
        fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
            Ok(true)
        }
    }
    let slot = Arc::new(std::sync::Mutex::new(None));
    let name = Box::leak(format!("lifecycle-fixture/{}", uuid::Uuid::now_v7()).into_boxed_str());
    let installer = Installer::new(vec![Box::new(Capture {
        name,
        context: slot.clone(),
    })])
    .unwrap();
    installer.create_schema(&store.connection.write()).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    if installer.position(&tx, "lifecycle-fixture-only").is_err() {
        installer
            .register_source(&tx, "lifecycle-fixture-only", "fixture-only", 1)
            .unwrap();
    }
    let job = installer
        .start(
            &tx,
            name,
            Limits {
                page_rows: 2,
                page_bytes: 1024,
                pending_rows: 8,
                pending_bytes: 4096,
                total_rows: 16,
                callback_ms: 1000,
                lifetime_ms: 10000,
            },
            0,
        )
        .unwrap();
    installer
        .scan(
            &tx,
            &ScanPage {
                job: job.clone(),
                expected_cursor: vec![],
                next_cursor: vec![1],
                position: installer.position(&tx, "lifecycle-fixture-only").unwrap(),
                rows: vec![Mutation {
                    key: "fixture".into(),
                    old: None,
                    new: Some(Value::Null),
                }],
                finished: false,
            },
            0,
        )
        .unwrap();
    installer.cancel(&tx, &job).unwrap();
    tx.commit().unwrap();
    slot.lock().unwrap().clone().unwrap()
}

fn declare(store: &Store, host: &str, workspace: &str) {
    let mut intent=crate::parse_intent("version 2\nagent \"sample\" { host \"amber\"; workspace \"/sample\"; harness \"omp\" { model \"canary\" } }", "amber").unwrap();
    let member = intent
        .subjects
        .get_mut(AGENT)
        .unwrap()
        .member
        .as_mut()
        .unwrap();
    member.host = host.into();
    member.workspace = workspace.into();
    store.apply_internal(&intent, "fixture-declare").unwrap();
}
fn fixture() -> Store {
    let store = Store::open_memory("amber").unwrap();
    store.set_write_clock_at(1_800_000_000_000).unwrap();
    declare(&store, "amber", "/sample");
    store
}
fn append(
    store: &Store,
    kind: &str,
    fields: Value,
    evidence: Vec<String>,
    key: Option<String>,
) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: AGENT.into(),
            kind: kind.into(),
            actor: Some("person/test".into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence,
            expected_subject: None,
            idempotency_key: key,
        })
        .unwrap()
}
fn selected(tx: &Transaction<'_>, ns: &Namespace) {
    let row = current_desired_row(tx, AGENT).unwrap().map(|r| Desired {
        token: r.claim_id,
        kind: r.kind,
    });
    apply_desired(tx, ns, AGENT, row.as_ref()).unwrap();
}
fn operation(tx: &Transaction<'_>, ns: &Namespace, id: &str) {
    let record=tx.query_row("SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind,
      claims.origin, claims.actor, claims.body, claims.predecessors, claims.accepted_at_unix_ms
      FROM operations JOIN claims ON claims.id=operations.canonical_claim_id WHERE operations.id=?1 AND operations.state='active'",[id],claim_from_row).optional().unwrap();
    apply_operation(tx, ns, id, record.as_ref()).unwrap();
}
// Fixture-only canonical extraction under this transaction; these helpers do not certify
// a production source or supply a replacement for delta's physical-table source hooks.
fn finish(tx: &Transaction<'_>, ns: &Namespace) {
    for _ in 0..2048 {
        let page = flush_page(tx, ns, "", 128).unwrap();
        if page.complete {
            return;
        }
        assert!(
            !page.missing.is_empty(),
            "unexpected permanent lifecycle fence"
        );
        for need in page.missing {
            match need {
                Need::Claim(id) => {
                    if let Some(c) = claim_by_id_tx(tx, &id).unwrap() {
                        let rank = canonical::claim_key(tx, &id).unwrap();
                        apply_claim(tx, ns, None, Some((&c, &rank))).unwrap();
                    } else {
                        apply_absence(tx, ns, &id).unwrap();
                    }
                }
                Need::Desired(agent) => {
                    let row = current_desired_row(tx, &agent).unwrap().map(|r| Desired {
                        token: r.claim_id,
                        kind: r.kind,
                    });
                    apply_desired(tx, ns, &agent, row.as_ref()).unwrap();
                }
                Need::Operation(id) => operation(tx, ns, &id),
            }
        }
    }
    panic!("lifecycle fixture did not close");
}
fn install(store: &Store) -> Namespace {
    let ns = context(store);
    let claims = store.claims_for(AGENT, None).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    selected(&tx, &ns);
    for c in claims {
        let rank = canonical::claim_key(&tx, &c.id).unwrap();
        apply_claim(&tx, &ns, None, Some((&c, &rank))).unwrap();
    }
    finish(&tx, &ns);
    tx.commit().unwrap();
    ns
}
fn refresh(store: &Store, ns: &Namespace) {
    let claims = store.claims_for(AGENT, None).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    selected(&tx, ns);
    for c in claims {
        let rank = canonical::claim_key(&tx, &c.id).unwrap();
        apply_claim(&tx, ns, None, Some((&c, &rank))).unwrap();
    }
    finish(&tx, ns);
    tx.commit().unwrap();
}
fn project(store: &Store, ns: &Namespace, c: &ClaimRecord, key: Option<&str>) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let rank = canonical::claim_key(&tx, &c.id).unwrap();
    apply_claim(&tx, ns, None, Some((c, &rank))).unwrap();
    if let Some(key) = key {
        operation(&tx, ns, &operation_id_for_key(key));
    }
    assert!(read_lifecycle(&tx, ns, AGENT).is_err());
    finish(&tx, ns);
    tx.commit().unwrap();
}
fn parity(store: &Store, ns: &Namespace) -> Lifecycle {
    let token = store.selected_desired_token(AGENT).unwrap();
    let expected = token.as_ref().and_then(|t| {
        crate::placement::handoff(store, AGENT, t, store.index().unwrap())
            .unwrap()
            .map(|v| serde_json::to_value(v).unwrap())
    });
    let actual = read_lifecycle(&store.readers.get(), ns, AGENT).unwrap();
    assert_eq!(actual.desired_token, token);
    assert_eq!(actual.handoff, expected);
    assert_eq!(
        actual.suspension,
        crate::suspension::current(store, AGENT).unwrap()
    );
    actual
}
fn suspend(store: &Store) -> ClaimRecord {
    append(
        store,
        "runtime.action.requested",
        json!({"action":"suspend","incarnation_id":"source-one"}),
        vec![store.selected_desired_token(AGENT).unwrap().unwrap()],
        None,
    )
}
fn receipt(store: &Store, request: &ClaimRecord, key: &str, fields: Value) -> ClaimRecord {
    append(
        store,
        "runtime.action.succeeded",
        fields,
        vec![request.id.clone()],
        Some(key.into()),
    )
}

#[test]
fn suspension_receipts_and_failure_precedence_match_canonical_current() {
    let store = fixture();
    let ns = install(&store);
    assert!(parity(&store, &ns).suspension.is_none());
    let req = suspend(&store);
    project(&store, &ns, &req, None);
    assert_eq!(parity(&store, &ns).suspension.unwrap().phase, "quiescing");
    let key = crate::suspension::suspend_snapshot_key(&req.id);
    let c = receipt(
        &store,
        &req,
        &key,
        json!({"action":"suspend","harness":"omp","native_session_id":"native","source_host":"amber"}),
    );
    project(&store, &ns, &c, Some(&key));
    let got = parity(&store, &ns).suspension.unwrap();
    assert_eq!(got.phase, "snapshotting");
    assert_eq!(got.native_session_id.as_deref(), Some("native"));
    let key = crate::suspension::suspend_completed_key(&req.id);
    let c = receipt(&store, &req, &key, json!({"action":"suspend"}));
    project(&store, &ns, &c, Some(&key));
    assert_eq!(parity(&store, &ns).suspension.unwrap().phase, "suspended");
    let key = crate::suspension::suspend_failed_key(&req.id);
    let c = append(
        &store,
        "runtime.action.failed",
        json!({"action":"suspend","code":"busy","reason":"turn in flight","blocking":["turn"]}),
        vec![req.id],
        Some(key.clone()),
    );
    project(&store, &ns, &c, Some(&key));
    let got = parity(&store, &ns).suspension.unwrap();
    assert_eq!(got.phase, "failed");
    assert_eq!(got.blocking, vec!["turn"]);
}

#[test]
fn absent_operation_fact_stays_pending_until_captured_and_late_receipt_wakes() {
    let store = fixture();
    let ns = install(&store);
    let req = suspend(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let rank = canonical::claim_key(&tx, &req.id).unwrap();
    apply_claim(&tx, &ns, None, Some((&req, &rank))).unwrap();
    let page = flush_page(&tx, &ns, "", 1).unwrap();
    let failed = operation_id_for_key(&crate::suspension::suspend_failed_key(&req.id));
    assert_eq!(page.missing, BTreeSet::from([Need::Operation(failed)]));
    assert!(!page.complete);
    assert!(ensure_closed(&tx, &ns).is_err());
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    parity(&store, &ns);
    let key = crate::suspension::suspend_completed_key(&req.id);
    let c = receipt(&store, &req, &key, json!({"action":"suspend"}));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    // Dispatch only the operation capture, with no same-agent claim dirty shortcut.
    let ids = apply_operation(&tx, &ns, &operation_id_for_key(&key), Some(&c)).unwrap();
    assert_eq!(ids, BTreeSet::from([AGENT.into()]));
    assert!(read_lifecycle(&tx, &ns, AGENT).is_err());
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(parity(&store, &ns).suspension.unwrap().phase, "suspended");
}

#[test]
fn presentation_rename_preserves_launch_and_workspace_change_ends_suspension() {
    let store = fixture();
    suspend(&store);
    let ns = install(&store);
    parity(&store, &ns);
    store
        .rename_agent(AGENT, Some("Renamed sample"), "fixture-rename")
        .unwrap();
    refresh(&store, &ns);
    assert_eq!(parity(&store, &ns).suspension.unwrap().phase, "quiescing");
    declare(&store, "amber", "/different");
    refresh(&store, &ns);
    assert!(parity(&store, &ns).suspension.is_none());
}

#[test]
fn resumed_host_evidence_survives_migration_and_all_resume_phases() {
    let store = fixture();
    let token = store.selected_desired_token(AGENT).unwrap().unwrap();
    let req = suspend(&store);
    receipt(
        &store,
        &req,
        &crate::suspension::suspend_snapshot_key(&req.id),
        json!({"action":"suspend","harness":"omp","native_session_id":"native","source_host":"amber"}),
    );
    receipt(
        &store,
        &req,
        &crate::suspension::suspend_completed_key(&req.id),
        json!({"action":"suspend"}),
    );
    let resume = append(
        &store,
        "runtime.action.requested",
        json!({"action":"resume","host":"jade","source_host":"amber"}),
        vec![token.clone(), req.id.clone()],
        None,
    );
    let ns = install(&store);
    assert_eq!(
        parity(&store, &ns).suspension.unwrap().phase,
        "fencing-source"
    );
    let key = format!("agent-resume-transfer:{}", resume.id);
    let c = receipt(&store, &resume, &key, json!({"action":"resume"}));
    project(&store, &ns, &c, Some(&key));
    assert_eq!(
        parity(&store, &ns).suspension.unwrap().phase,
        "transferring"
    );
    store
        .place_resumed_seat(AGENT, &token, &resume.id, "jade", "person/test")
        .unwrap();
    refresh(&store, &ns);
    assert_eq!(parity(&store, &ns).suspension.unwrap().phase, "restoring");
    store
        .rename_agent(AGENT, Some("Moved sample"), "fixture-rename")
        .unwrap();
    refresh(&store, &ns);
    parity(&store, &ns);
    for (key, phase, fields) in [
        (
            format!("agent-resume-restored:{}", resume.id),
            "restoring",
            json!({"action":"resume"}),
        ),
        (
            crate::suspension::resume_started_key(&resume.id),
            "verifying",
            json!({"action":"resume"}),
        ),
        (
            crate::suspension::resume_completed_key(&resume.id),
            "resumed",
            json!({"action":"resume","incarnation_id":"destination-one"}),
        ),
    ] {
        let c = receipt(&store, &resume, &key, fields);
        project(&store, &ns, &c, Some(&key));
        assert_eq!(parity(&store, &ns).suspension.unwrap().phase, phase);
    }
    let key = crate::suspension::resume_failed_key(&resume.id);
    let c = append(
        &store,
        "runtime.action.failed",
        json!({"action":"resume","reason":"cannot bind","code":"native-resume-unavailable"}),
        vec![resume.id],
        Some(key.clone()),
    );
    project(&store, &ns, &c, Some(&key));
    let got = parity(&store, &ns).suspension.unwrap();
    assert_eq!(got.phase, "suspended");
    assert_eq!(got.native_session_id.as_deref(), Some("native"));
}

#[test]
fn placement_requires_same_origin_stopped_acknowledgement_and_tracks_destination() {
    let store = fixture();
    let oldtoken = store.selected_desired_token(AGENT).unwrap().unwrap();
    let old = append(
        &store,
        "runtime.observed",
        json!({"status":"stopped","host":"amber","incarnation_id":"old"}),
        vec![oldtoken],
        None,
    );
    declare(&store, "jade", "/sample");
    let token = store.selected_desired_token(AGENT).unwrap().unwrap();
    let ns = install(&store);
    assert_eq!(
        parity(&store, &ns).handoff.unwrap()["phase"],
        "stopping-source"
    );
    append(
        &store,
        "harness.observed",
        json!({"state":"idle","incarnation_id":"old"}),
        vec![token.clone()],
        None,
    );
    let stopped = append(
        &store,
        "runtime.observed",
        json!({"status":"stopped","host":"amber","incarnation_id":"old"}),
        vec![],
        None,
    );
    project(&store, &ns, &stopped, None);
    let got = parity(&store, &ns).handoff.unwrap();
    assert_eq!(got["phase"], "waiting-for-destination");
    assert!(got["pending_sources"].as_array().unwrap().is_empty());
    let dest = Store::open_memory("jade").unwrap();
    dest.set_write_clock_at(1_800_000_000_002).unwrap();
    dest.import_replication("amber", &store.export_replication(0).unwrap())
        .unwrap();
    let c = append(
        &dest,
        "runtime.observed",
        json!({"status":"starting","host":"jade","incarnation_id":"new"}),
        vec![token],
        None,
    );
    store
        .import_replication("jade", &dest.export_replication(0).unwrap())
        .unwrap();
    project(&store, &ns, &c, None);
    assert_eq!(parity(&store, &ns).handoff.unwrap()["phase"], "starting");
    let c = append(
        &dest,
        "runtime.observed",
        json!({"status":"running","host":"jade","incarnation_id":"new"}),
        vec![],
        None,
    );
    store
        .import_replication("jade", &dest.export_replication(0).unwrap())
        .unwrap();
    project(&store, &ns, &c, None);
    assert_eq!(parity(&store, &ns).handoff.unwrap()["phase"], "running");
    assert_ne!(old.id, stopped.id);
}

#[test]
fn explicit_source_offline_override_is_indexed_and_removed_with_old_claim() {
    let store = fixture();
    declare(&store, "jade", "/sample");
    let ns = install(&store);
    let token = store.selected_desired_token(AGENT).unwrap().unwrap();
    let input = crate::placement::source_offline_input(
        &store,
        crate::placement::SourceOfflineRequest {
            subject: AGENT.into(),
            actor: "person/test".into(),
            desired_token: token,
            sources: vec!["amber".into()],
            idempotency_key: "fixture-offline".into(),
        },
    )
    .unwrap();
    let c = store.append_claim(&input).unwrap();
    project(&store, &ns, &c, None);
    let got = parity(&store, &ns).handoff.unwrap();
    assert_eq!(got["overridden_sources"], json!(["amber"]));
    assert_eq!(got["phase"], "waiting-for-destination");
    // Isolated fixture corrects the source row: retract the old override contribution.
    let mut corrected = c.clone();
    corrected.body["fields"]["desired_token"] = json!("claim/different-placement");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE claims SET body=?2 WHERE id=?1",
        params![c.id, serde_json::to_string(&corrected.body).unwrap()],
    )
    .unwrap();
    let key = canonical::claim_key(&tx, &c.id).unwrap();
    apply_claim(&tx, &ns, Some(&c), Some((&corrected, &key))).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(
        parity(&store, &ns).handoff.unwrap()["phase"],
        "stopping-source"
    );
}

#[test]
fn concurrent_rename_lineage_keeps_raw_predecessor_order() {
    let left = fixture();
    let right = Store::open_memory("jade").unwrap();
    right.set_write_clock_at(1_800_000_000_001).unwrap();
    right
        .import_replication("amber", &left.export_replication(0).unwrap())
        .unwrap();
    left.rename_agent(AGENT, Some("Left name"), "left-name")
        .unwrap();
    let request = suspend(&left);
    right
        .rename_agent(AGENT, Some("Right name"), "right-name")
        .unwrap();
    left.import_replication("jade", &right.export_replication(0).unwrap())
        .unwrap();
    left.rename_agent(AGENT, Some("Merged name"), "merged-name")
        .unwrap();
    let token = left.selected_desired_token(AGENT).unwrap().unwrap();
    let mut merged = left.claim_by_id(&token).unwrap().unwrap();
    assert_eq!(merged.predecessors.len(), 2);
    // Isolated legacy fixture changes the recorded predecessor order to prove this is
    // ordered lineage, rather than sorting by canonical rank or accepting both branches.
    let left_token = evidence(&request, 0).unwrap();
    let first = merged
        .predecessors
        .iter()
        .position(|id| id == left_token)
        .unwrap();
    merged.predecessors.swap(0, first);
    left.connection
        .write()
        .execute(
            "UPDATE claims SET predecessors=?2 WHERE id=?1",
            params![token, serde_json::to_string(&merged.predecessors).unwrap()],
        )
        .unwrap();
    let ns = install(&left);
    assert!(parity(&left, &ns).suspension.is_some());
    let old = merged.clone();
    merged.predecessors.reverse();
    let mut writer = left.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE claims SET predecessors=?2 WHERE id=?1",
        params![token, serde_json::to_string(&merged.predecessors).unwrap()],
    )
    .unwrap();
    let rank = canonical::claim_key(&tx, &token).unwrap();
    apply_claim(&tx, &ns, Some(&old), Some((&merged, &rank))).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert!(parity(&left, &ns).suspension.is_none());
}

#[test]
fn operation_conflict_is_known_none_and_canonical_reselection_wakes_suspension() {
    let left = fixture();
    let req = suspend(&left);
    let right = Store::open_memory("jade").unwrap();
    right.set_write_clock_at(1_800_000_000_001).unwrap();
    right
        .import_replication("amber", &left.export_replication(0).unwrap())
        .unwrap();
    let key = crate::suspension::suspend_failed_key(&req.id);
    append(
        &left,
        "runtime.action.failed",
        json!({"action":"suspend","code":"busy","reason":"left"}),
        vec![req.id.clone()],
        Some(key.clone()),
    );
    append(
        &right,
        "runtime.action.failed",
        json!({"action":"suspend","code":"busy","reason":"right"}),
        vec![req.id],
        Some(key.clone()),
    );
    left.import_replication("jade", &right.export_replication(0).unwrap())
        .unwrap();
    assert!(left.operation_claim(&key).unwrap().is_none());
    let ns = install(&left);
    assert_eq!(parity(&left, &ns).suspension.unwrap().phase, "quiescing");
    // Isolated fixture changes only the global active selection. Even a receipt on another
    // subject is what the existing reducer reads from the operation's active canonical ID.
    let foreign = left
        .append_claim(&ClaimInput {
            subject: "agent/foreign-receipt".into(),
            kind: "runtime.action.failed".into(),
            actor: Some("person/test".into()),
            fields: serde_json::from_value(
                json!({"action":"suspend","code":"foreign","reason":"captured operation"}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut writer = left.connection.write();
    let tx = writer.transaction().unwrap();
    let id = operation_id_for_key(&key);
    tx.execute(
        "UPDATE operations SET state='active',canonical_claim_id=?2 WHERE id=?1",
        params![id, foreign.id],
    )
    .unwrap();
    let affected = apply_operation(&tx, &ns, &id, Some(&foreign)).unwrap();
    assert_eq!(affected, BTreeSet::from([AGENT.into()]));
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    let got = parity(&left, &ns).suspension.unwrap();
    assert_eq!(got.phase, "failed");
    assert_eq!(got.code.as_deref(), Some("foreign"));
}

#[test]
fn late_causal_input_remains_pending_but_known_checkpoint_absence_is_live_only() {
    let store = fixture();
    declare(&store, "jade", "/sample");
    let token = store.selected_desired_token(AGENT).unwrap().unwrap();
    let bridge = append(
        &store,
        "harness.observed",
        json!({"state":"idle","incarnation_id":"old"}),
        vec![token],
        None,
    );
    let stopped = append(
        &store,
        "runtime.observed",
        json!({"status":"stopped","incarnation_id":"old"}),
        vec![],
        None,
    );
    let ns = install(&store);
    assert_eq!(
        parity(&store, &ns).handoff.unwrap()["phase"],
        "waiting-for-destination"
    );
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        &sql(
            &ns,
            "DELETE FROM local_agent_lifecycle_claims WHERE namespace=@NS@ AND id=?1",
        ),
        [&bridge.id],
    )
    .unwrap();
    queue(&tx, &ns, AGENT).unwrap();
    let page = flush_page(&tx, &ns, "", 1).unwrap();
    assert!(page.missing.contains(&Need::Claim(bridge.id.clone())));
    assert!(read_lifecycle(&tx, &ns, AGENT).is_err());
    // Delete the live bridge while retaining only its canonical tombstone knowledge.
    tx.execute("DELETE FROM claims WHERE id=?1", [&bridge.id])
        .unwrap();
    apply_absence(&tx, &ns, &bridge.id).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    let got = parity(&store, &ns).handoff.unwrap();
    assert_eq!(got["phase"], "stopping-source");
    assert_eq!(got["pending_sources"], json!(["amber"]));
    assert_ne!(bridge.id, stopped.id);
}

#[test]
fn namespace_isolation_atomic_rollback_and_bounded_reclamation() {
    let store = fixture();
    let ns = install(&store);
    let other = install(&store);
    let before = parity(&store, &ns);
    let before_saved = before.clone();
    let req = suspend(&store);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let rank = canonical::claim_key(&tx, &req.id).unwrap();
        apply_claim(&tx, &ns, None, Some((&req, &rank))).unwrap();
        finish(&tx, &ns);
        assert!(
            read_lifecycle(&tx, &ns, AGENT)
                .unwrap()
                .suspension
                .is_some()
        );
        assert_eq!(read_lifecycle(&tx, &other, AGENT).unwrap(), before);
        tx.rollback().unwrap();
    }
    assert_eq!(
        read_lifecycle(&store.readers.get(), &ns, AGENT).unwrap(),
        before
    );
    project(&store, &ns, &req, None);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let count = |tx: &Transaction<'_>| -> i64 {
        tx.query_row(&sql(&ns,"SELECT (SELECT COUNT(*) FROM local_agent_lifecycle_claims WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_overrides WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_desired WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_operations WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_dependencies WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_rows WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_dirty WHERE namespace=@NS@)+(SELECT COUNT(*) FROM local_agent_lifecycle_fences WHERE namespace=@NS@)"),[],|r|r.get(0)).unwrap()
    };
    let before = count(&tx);
    assert!(!reclaim_namespace(&tx, &ns, 1).unwrap());
    assert_eq!(count(&tx), before - 1);
    for _ in 0..256 {
        if reclaim_namespace(&tx, &ns, 2).unwrap() {
            break;
        }
    }
    assert_eq!(count(&tx), 0);
    assert_eq!(read_lifecycle(&tx, &other, AGENT).unwrap(), before_saved);
    tx.commit().unwrap();
}

#[test]
fn over_bound_causal_input_fences_only_its_namespace_and_never_returns_partial_row() {
    let store = fixture();
    let ns = install(&store);
    let other = install(&store);
    let token = store.selected_desired_token(AGENT).unwrap().unwrap();
    let mut c = store.claim_by_id(&token).unwrap().unwrap();
    let old = c.clone();
    c.predecessors = (0..257).map(|i| format!("claim/missing-{i}")).collect();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let rank = canonical::claim_key(&tx, &c.id).unwrap();
    apply_claim(&tx, &ns, Some(&old), Some((&c, &rank))).unwrap();
    let page = flush_page(&tx, &ns, "", 128).unwrap();
    assert!(!page.complete);
    assert!(page.missing.is_empty());
    assert!(ensure_closed(&tx, &ns).is_err());
    assert!(read_lifecycle(&tx, &ns, AGENT).is_err());
    assert!(read_lifecycle(&tx, &other, AGENT).is_ok());
    assert!(flush_page(&tx, &ns, "", 129).is_err());
    assert!(reclaim_namespace(&tx, &ns, 129).is_err());
    tx.rollback().unwrap();
}

#[test]
fn late_receiver_arrival_and_canonical_rank_correction_reselect_request() {
    let left = fixture();
    let right = Store::open_memory("jade").unwrap();
    right.set_write_clock_at(1_800_000_000_000).unwrap();
    right
        .import_replication("amber", &left.export_replication(0).unwrap())
        .unwrap();
    let a = suspend(&left);
    let b = suspend(&right);
    left.import_replication("jade", &right.export_replication(0).unwrap())
        .unwrap();
    let ns = install(&left);
    let winner = parity(&left, &ns).suspension.unwrap().operation_id;
    // Preserve receiver-local delivery position while altering the canonical batch ordering.
    let loser = if winner == a.id { &b } else { &a };
    let rank_origin = "zzz-corrected";
    assert_eq!(a.accepted_at_unix_ms, b.accepted_at_unix_ms);
    let mut writer = left.connection.write();
    let tx = writer.transaction().unwrap();
    let index_before: u64 = tx
        .query_row(
            "SELECT store_index FROM claims WHERE id=?1",
            [&loser.id],
            |r| r.get(0),
        )
        .unwrap();
    tx.execute(
        "UPDATE batches SET origin=?2 WHERE id=?1",
        params![loser.batch_id, rank_origin],
    )
    .unwrap();
    let key = canonical::claim_key(&tx, &loser.id).unwrap();
    apply_claim(&tx, &ns, Some(loser), Some((loser, &key))).unwrap();
    finish(&tx, &ns);
    let index_after: u64 = tx
        .query_row(
            "SELECT store_index FROM claims WHERE id=?1",
            [&loser.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index_before, index_after);
    tx.commit().unwrap();
    drop(writer);
    let selected = parity(&left, &ns).suspension.unwrap().operation_id;
    // Every timestamp is held equal: origin correction must drive this selection.
    assert_ne!(selected, winner);
    assert_eq!(selected, loser.id);
}

#[test]
fn repair_cursor_advances_past_missing_input_without_certifying_completion() {
    let store = fixture();
    let ns = install(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    queue(&tx, &ns, "agent/000-deferred").unwrap();
    apply_desired(&tx, &ns, "agent/zzz-supported", None).unwrap();
    let first = flush_page(&tx, &ns, "", 1).unwrap();
    assert_eq!(first.after.as_deref(), Some("agent/000-deferred"));
    assert_eq!(
        first.missing,
        BTreeSet::from([Need::Desired("agent/000-deferred".into())])
    );
    assert!(first.changed.is_empty());
    assert!(!first.complete);
    let second = flush_page(&tx, &ns, first.after.as_deref().unwrap(), 1).unwrap();
    assert_eq!(second.after.as_deref(), Some("agent/zzz-supported"));
    assert_eq!(
        second.changed,
        BTreeSet::from(["agent/zzz-supported".into()])
    );
    assert!(!second.complete);
    assert!(read_lifecycle(&tx, &ns, "agent/000-deferred").is_err());
    assert!(read_lifecycle(&tx, &ns, "agent/zzz-supported").is_ok());
    apply_desired(&tx, &ns, "agent/000-deferred", None).unwrap();
    assert!(flush_page(&tx, &ns, "", 1).unwrap().complete);
    tx.commit().unwrap();
}
