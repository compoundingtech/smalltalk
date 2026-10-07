use super::*;

const AGENT: &str = "agent/garden/authority";

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
            "authority-fixture-only"
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
    let name = Box::leak(format!("authority-fixture/{}", uuid::Uuid::now_v7()).into_boxed_str());
    let installer = Installer::new(vec![Box::new(Capture {
        name,
        context: slot.clone(),
    })])
    .unwrap();
    installer.create_schema(&store.connection.write()).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    if installer.position(&tx, "authority-fixture-only").is_err() {
        installer
            .register_source(&tx, "authority-fixture-only", "fixture-only", 1)
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
                position: installer.position(&tx, "authority-fixture-only").unwrap(),
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

fn append(store: &Store, kind: &str, fields: Value) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: AGENT.into(),
            kind: kind.into(),
            actor: Some(AGENT.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
fn runtime(store: &Store, status: &str) -> ClaimRecord {
    append(
        store,
        "runtime.observed",
        json!({"status":status,"host":store.origin(),"runtime_id":"garden.authority","incarnation_id":store.origin()}),
    )
}
fn finish(tx: &Transaction<'_>, ns: &Namespace) {
    let mut after: Option<(String, String)> = None;
    for _ in 0..512 {
        let (next, clean) = flush_ancestry(
            tx,
            ns,
            after.as_ref().map(|(a, i)| (a.as_str(), i.as_str())),
            2,
        )
        .unwrap();
        if clean {
            return;
        }
        after = next;
    }
    panic!("authority fixture did not close its source graph");
}
fn install(store: &Store) -> Namespace {
    let ns = context(store);
    create_schema(&store.connection.write()).unwrap();
    let claims = store.claims_for(AGENT, None).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for claim in claims {
        let key = canonical::claim_key(&tx, &claim.id).unwrap();
        apply_claim(&tx, &ns, None, Some((&claim, &key))).unwrap();
    }
    finish(&tx, &ns);
    tx.commit().unwrap();
    ns
}
fn project(store: &Store, ns: &Namespace, old: Option<&ClaimRecord>, claim: &ClaimRecord) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let key = canonical::claim_key(&tx, &claim.id).unwrap();
    assert!(
        apply_claim(&tx, ns, old, Some((claim, &key)))
            .unwrap()
            .contains(AGENT)
    );
    assert!(read_authority(&tx, ns, AGENT, None).is_err());
    finish(&tx, ns);
    tx.commit().unwrap();
}
fn parity(store: &Store, ns: &Namespace, desired_host: Option<&str>) -> Authority {
    let c = store.readers.get();
    let actual = latest_actual_at(&c, AGENT, None).unwrap();
    let (claim, origin, conflict) =
        selected_actual_source_at(&c, AGENT, None, desired_host).unwrap();
    let expected = actual.as_ref().map(|actual| {
        Value::Object(
            FIELDS
                .iter()
                .filter_map(|field| {
                    actual
                        .get(*field)
                        .map(|value| ((*field).to_owned(), value.clone()))
                })
                .collect(),
        )
    });
    let got = read_authority(&c, ns, AGENT, desired_host).unwrap();
    assert_eq!(got.actual, expected);
    assert_eq!(got.actual_presence, actual.is_some());
    assert_eq!(got.actual_claim, claim);
    assert_eq!(got.actual_origin, origin);
    assert_eq!(got.runtime_origin_conflict, conflict);
    for (field, got) in [
        ("status", &got.status),
        ("reachability", &got.reachability),
        ("reason", &got.reason),
        ("runtime_id", &got.runtime_id),
        ("incarnation_id", &got.incarnation_id),
    ] {
        assert_eq!(
            *got,
            actual
                .as_ref()
                .and_then(|a| a[field].as_str())
                .map(str::to_owned)
        );
    }
    got
}

#[test]
fn actual_fields_reset_independently_of_runtime_source_selection() {
    let store = Store::open_memory("alder").unwrap();
    let first = append(
        &store,
        "runtime.observed",
        json!({"status":"running","host":"alder","runtime_id":"first","incarnation_id":"one","reachability":"unreachable","reason":"first"}),
    );
    let ns = install(&store);
    parity(&store, &ns, None);
    let generic = append(
        &store,
        "runtime.restart-window-reset",
        json!({"desired_token":"desired-one","incarnation_id":"one","reason":"stable interval"}),
    );
    project(&store, &ns, None, &generic);
    let got = parity(&store, &ns, None);
    assert_eq!(got.actual_claim, Some(first.id));
    assert_eq!(got.reason.as_deref(), Some("stable interval"));
    let second = append(
        &store,
        "runtime.observed",
        json!({"status":"starting","incarnation_id":"two"}),
    );
    project(&store, &ns, None, &second);
    let got = parity(&store, &ns, None);
    // runtime.observed is Append: omitted fields carry across this observation.
    assert_eq!(got.reason.as_deref(), Some("stable interval"));
    assert_eq!(got.reachability.as_deref(), Some("unreachable"));
    assert_eq!(got.runtime_id.as_deref(), Some("first"));
    let hold = append(
        &store,
        "delivery.hold",
        json!({"held":false,"until_unix_ms":0,"reason":"policy changed"}),
    );
    project(&store, &ns, None, &hold);
    assert_eq!(
        parity(&store, &ns, None).reason.as_deref(),
        Some("policy changed")
    );
    // An admitted legacy StateTransition missing a field still resets its schema fields.
    let mut legacy = hold.clone();
    legacy.body["fields"]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET body=?2 WHERE id=?1",
            params![hold.id, serde_json::to_string(&legacy.body).unwrap()],
        )
        .unwrap();
    project(&store, &ns, Some(&hold), &legacy);
    let got = parity(&store, &ns, None);
    assert!(got.reason.is_none());
    assert_eq!(got.status.as_deref(), Some("starting"));
}

#[test]
fn latest_origin_terminal_and_same_snapshot_desired_host_match_canonical_authority() {
    for terminal in ["stopped", "absent", "exited", "vanished"] {
        let old = Store::open_memory("alder").unwrap();
        let new = Store::open_memory("birch").unwrap();
        old.set_write_clock_at(1_800_000_000_000).unwrap();
        new.set_write_clock_at(1_800_000_000_001).unwrap();
        runtime(&old, "running");
        runtime(&old, terminal);
        runtime(&new, "running");
        new.import_replication("alder", &old.export_replication(0).unwrap())
            .unwrap();
        let ns = install(&new);
        assert!(!parity(&new, &ns, None).runtime_origin_conflict);
        assert!(parity(&new, &ns, Some("alder")).runtime_origin_conflict);
        old.set_write_clock_at(1_800_000_000_002).unwrap();
        runtime(&old, "running");
        new.import_replication("alder", &old.export_replication(0).unwrap())
            .unwrap();
        let ns = install(&new);
        assert!(parity(&new, &ns, None).runtime_origin_conflict);
    }
}

#[test]
fn intermediate_harness_claims_preserve_causal_runtime_ancestry() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    runtime(&right, "running");
    for _ in 0..3 {
        append(
            &right,
            "harness.observed",
            json!({"state":"working","incarnation_id":"birch"}),
        );
    }
    left.import_replication("birch", &right.export_replication(0).unwrap())
        .unwrap();
    for _ in 0..3 {
        append(
            &left,
            "harness.observed",
            json!({"state":"working","incarnation_id":"birch"}),
        );
    }
    let selected = runtime(&left, "running");
    append(
        &left,
        "harness.observed",
        json!({"state":"idle","incarnation_id":"alder"}),
    );
    let ns = install(&left);
    let got = parity(&left, &ns, None);
    assert_eq!(got.actual_claim, Some(selected.id));
    assert!(!got.runtime_origin_conflict);
    let other = Store::open_memory("cedar").unwrap();
    runtime(&other, "running");
    left.import_replication("cedar", &other.export_replication(0).unwrap())
        .unwrap();
    let ns = install(&left);
    assert!(parity(&left, &ns, None).runtime_origin_conflict);
}

#[test]
fn reversed_source_arrival_keeps_missing_parents_unavailable_until_closed() {
    let store = Store::open_memory("alder").unwrap();
    let parent = runtime(&store, "running");
    let child = runtime(&store, "starting");
    assert!(child.predecessors.contains(&parent.id));
    let ns = context(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let key = canonical::claim_key(&tx, &child.id).unwrap();
    apply_claim(&tx, &ns, None, Some((&child, &key))).unwrap();
    let (_, clean) = flush_ancestry(&tx, &ns, None, 128).unwrap();
    assert!(!clean);
    assert!(read_authority(&tx, &ns, AGENT, None).is_err());
    let key = canonical::claim_key(&tx, &parent.id).unwrap();
    apply_claim(&tx, &ns, None, Some((&parent, &key))).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    parity(&store, &ns, None);
}

#[test]
fn canonical_rank_correction_reorders_fields_source_and_ancestry() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    left.set_write_clock_at(1_800_000_000_000).unwrap();
    right.set_write_clock_at(1_800_000_000_000).unwrap();
    let a = runtime(&left, "running");
    let b = runtime(&right, "starting");
    left.import_replication("birch", &right.export_replication(0).unwrap())
        .unwrap();
    let ns = install(&left);
    let before = parity(&left, &ns, None);
    assert_eq!(before.actual_claim, Some(b.id));
    // Isolated canonical-source correction; the adapter supplies the newly resolved key.
    left.connection
        .write()
        .execute(
            "UPDATE batches SET origin='z-corrected' WHERE id=?1",
            [&a.batch_id],
        )
        .unwrap();
    project(&left, &ns, Some(&a), &a);
    let after = parity(&left, &ns, None);
    assert_eq!(after.actual_claim, Some(a.id));
    assert!(after.runtime_origin_conflict);
}

#[test]
fn rollback_restores_authority_fields_heads_and_pending_queue() {
    let store = Store::open_memory("alder").unwrap();
    let claim = runtime(&store, "running");
    let ns = install(&store);
    let before = parity(&store, &ns, None);
    let mut corrected = claim.clone();
    corrected.body["fields"]["status"] = json!("stopped");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let key = canonical::claim_key(&tx, &claim.id).unwrap();
    apply_claim(&tx, &ns, Some(&claim), Some((&corrected, &key))).unwrap();
    finish(&tx, &ns);
    assert_eq!(
        read_authority(&tx, &ns, AGENT, None)
            .unwrap()
            .status
            .as_deref(),
        Some("stopped")
    );
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(parity(&store, &ns, None), before);
}

#[test]
fn checkpoint_link_preserves_ancestry_without_becoming_actual_state() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    runtime(&right, "running");
    left.import_replication("birch", &right.export_replication(0).unwrap())
        .unwrap();
    let bridge = append(
        &left,
        "harness.observed",
        json!({"state":"working","incarnation_id":"birch"}),
    );
    runtime(&left, "running");
    let ns = install(&left);
    let before = parity(&left, &ns, None);
    assert!(!before.runtime_origin_conflict);
    let mut writer = left.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,accepted_at_unix_ms,checkpoint)
      VALUES(?1,'alder',1,'fixture',?2,?3,?2,?4,?5,'fixture')",params![bridge.id,AGENT,bridge.kind,serde_json::to_string(&bridge.predecessors).unwrap(),bridge.accepted_at_unix_ms as i64]).unwrap();
    tx.execute("DELETE FROM claims WHERE id=?1", [&bridge.id])
        .unwrap();
    apply_claim(&tx, &ns, Some(&bridge), None).unwrap();
    apply_tombstone(&tx, &ns, &bridge.id, AGENT, &bridge.predecessors).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(parity(&left, &ns, None), before);
}

#[test]
fn oversized_parent_source_fences_instead_of_returning_partial_authority() {
    let store = Store::open_memory("alder").unwrap();
    let mut claim = runtime(&store, "running");
    let ns = context(&store);
    claim.predecessors = (0..=EDGES).map(|n| format!("missing/{n}")).collect();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let key = canonical::claim_key(&tx, &claim.id).unwrap();
    apply_claim(&tx, &ns, None, Some((&claim, &key))).unwrap();
    assert!(
        read_authority(&tx, &ns, AGENT, None)
            .unwrap_err()
            .to_string()
            .contains("predecessor count")
    );
    tx.commit().unwrap();
}

#[test]
fn replacement_namespace_and_bounded_reclamation_preserve_previous_output() {
    let store = Store::open_memory("alder").unwrap();
    runtime(&store, "running");
    let old = install(&store);
    let before = parity(&store, &old, None);
    runtime(&store, "stopped");
    let next = install(&store);
    assert_ne!(old, next);
    assert_eq!(
        parity(&store, &next, None).status.as_deref(),
        Some("stopped")
    );
    assert_eq!(
        read_authority(&store.readers.get(), &old, AGENT, None).unwrap(),
        before
    );
    ensure_closed(&store.readers.get(), &old).unwrap();
    ensure_closed(&store.readers.get(), &next).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    fence(&tx, &next, AGENT, "fixture source gap").unwrap();
    assert!(read_authority(&tx, &next, AGENT, None).is_err());
    assert!(ensure_closed(&tx, &next).is_err());
    assert_eq!(read_authority(&tx, &old, AGENT, None).unwrap(), before);
    let mut done = false;
    for _ in 0..128 {
        if reclaim_namespace(&tx, &next, 2).unwrap() {
            done = true;
            break;
        }
    }
    assert!(done);
    assert_eq!(read_authority(&tx, &old, AGENT, None).unwrap(), before);
    tx.commit().unwrap();
}

#[test]
fn trimmed_runtime_ancestor_downgrades_to_the_remaining_live_origin_head() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    runtime(&right, "running");
    let trimmed = runtime(&right, "starting");
    left.import_replication("birch", &right.export_replication(0).unwrap())
        .unwrap();
    runtime(&left, "running");
    let ns = install(&left);
    assert!(!parity(&left, &ns, None).runtime_origin_conflict);
    let mut writer = left.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,accepted_at_unix_ms,checkpoint)
      VALUES(?1,'birch',1,'fixture',?2,?3,?2,?4,?5,'fixture')",params![trimmed.id,AGENT,trimmed.kind,serde_json::to_string(&trimmed.predecessors).unwrap(),trimmed.accepted_at_unix_ms as i64]).unwrap();
    tx.execute("DELETE FROM claims WHERE id=?1", [&trimmed.id])
        .unwrap();
    apply_claim(&tx, &ns, Some(&trimmed), None).unwrap();
    apply_tombstone(&tx, &ns, &trimmed.id, AGENT, &trimmed.predecessors).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert!(!parity(&left, &ns, None).runtime_origin_conflict);
}

#[test]
fn legacy_ascii_harness_kind_can_select_a_source_without_actual_fields() {
    let store = Store::open_memory("alder").unwrap();
    let claim = append(
        &store,
        "harness.observed",
        json!({"state":"working","incarnation_id":"one"}),
    );
    // Canonical source selection uses binary kind ranges, while the actual field fold
    // additionally excludes SQL LIKE 'harness.%' in ASCII case variants.
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET kind='HARNESS.legacy' WHERE id=?1",
            [&claim.id],
        )
        .unwrap();
    let ns = install(&store);
    let got = parity(&store, &ns, None);
    assert_eq!(got.actual_claim, Some(claim.id));
    assert!(!got.actual_presence);
    assert!(got.actual.is_none());
}

fn append_subject(store: &Store, subject: &str) -> ClaimRecord {
    store.append_claim(&ClaimInput {subject:subject.into(),kind:"runtime.observed".into(),actor:Some(AGENT.into()),
      fields:serde_json::from_value(json!({"status":"running","host":store.origin(),"incarnation_id":"captured-parent-fixture"})).unwrap(),
      evidence:vec![],expected_subject:None,idempotency_key:None}).unwrap()
}

#[test]
fn later_global_foreign_parent_cannot_complete_an_older_namespace_snapshot() {
    let source = Store::open_memory("birch").unwrap();
    let parent = append_subject(&source, "agent/garden/unrelated");
    let store = Store::open_memory("alder").unwrap();
    let child = runtime(&store, "running");
    // Isolated legacy-source fixture with an unresolved cross-subject predecessor. Its
    // parent is not received until AFTER this real Store read snapshot is released.
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET predecessors=?2 WHERE id=?1",
            params![
                child.id,
                serde_json::to_string(&vec![parent.id.clone()]).unwrap()
            ],
        )
        .unwrap();
    let (old_cut, captured_child, key) = store
        .read_snapshot(|cut| {
            let child = store.claim_by_id(&child.id)?.unwrap();
            let key = canonical::claim_key(&store.readers.get(), &child.id)?;
            assert!(store.claim_by_id(&parent.id)?.is_none());
            Ok((cut, child, key))
        })
        .unwrap();
    let old = context(&store);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply_claim(&tx, &old, None, Some((&captured_child, &key))).unwrap();
        assert!(!flush_ancestry(&tx, &old, None, 128).unwrap().1);
        assert!(read_authority(&tx, &old, AGENT, None).is_err());
        tx.commit().unwrap();
    }
    store
        .import_replication("birch", &source.export_replication(0).unwrap())
        .unwrap();
    let (new_cut, identity) = store
        .read_snapshot(|cut| Ok((cut, store.claim_by_id(&parent.id)?.unwrap().subject)))
        .unwrap();
    assert!(new_cut > old_cut);
    assert_ne!(identity, AGENT);
    {
        // This repair transaction can see the newer global foreign claim. It must still
        // reject the OLD namespace, whose captured input has not advanced with that cut.
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(claim_by_id_tx(&tx, &parent.id).unwrap().is_some());
        assert!(!flush_ancestry(&tx, &old, None, 128).unwrap().1);
        assert!(ensure_closed(&tx, &old).is_err());
        assert!(read_authority(&tx, &old, AGENT, None).is_err());
        tx.commit().unwrap();
    }
    let new = context(&store);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply_claim(&tx, &new, None, Some((&captured_child, &key))).unwrap();
        apply_parent_identity(&tx, &new, &parent.id, Some(&identity)).unwrap();
        finish(&tx, &new);
        assert!(ensure_closed(&tx, &new).is_ok());
        assert!(ensure_closed(&tx, &old).is_err());
        // A known-absent captured fact remains unresolved, even with a later global row.
        assert_eq!(
            apply_parent_identity(&tx, &old, &parent.id, None).unwrap(),
            BTreeSet::from([AGENT.into()])
        );
        assert!(!flush_ancestry(&tx, &old, None, 128).unwrap().1);
        tx.commit().unwrap();
    }
    parity(&store, &new, None);
    {
        // Only an explicit input capture at the advanced source cut can close the old
        // dependency. The fixture operator never certifies a production Ready root.
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert_eq!(
            apply_parent_identity(&tx, &old, &parent.id, Some(&identity)).unwrap(),
            BTreeSet::from([AGENT.into()])
        );
        finish(&tx, &old);
        tx.commit().unwrap();
    }
    parity(&store, &old, None);
}

#[test]
fn captured_identity_removal_and_correction_invalidate_existing_node_ancestry() {
    let store = Store::open_memory("alder").unwrap();
    let parent = runtime(&store, "running");
    runtime(&store, "starting");
    let ns = install(&store);
    let before = parity(&store, &ns, None);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(
        apply_parent_identity(&tx, &ns, &parent.id, Some(AGENT))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        apply_parent_identity(&tx, &ns, &parent.id, None).unwrap(),
        BTreeSet::from([AGENT.into()])
    );
    assert!(read_authority(&tx, &ns, AGENT, None).is_err());
    assert!(!flush_ancestry(&tx, &ns, None, 128).unwrap().1);
    // The captured identity overrides a stale node: a foreign identity skips this edge,
    // while changing it back to a same-subject unresolved node cannot retain completion.
    apply_parent_identity(&tx, &ns, &parent.id, Some("agent/garden/unrelated")).unwrap();
    finish(&tx, &ns);
    apply_parent_identity(&tx, &ns, &parent.id, Some(AGENT)).unwrap();
    finish(&tx, &ns);
    assert_eq!(read_authority(&tx, &ns, AGENT, None).unwrap(), before);
    // Identity corrections cannot borrow a node of a different captured subject.
    tx.execute(&sql(&ns,"UPDATE local_agent_authority_nodes SET agent='agent/garden/unrelated' WHERE namespace=@NS@ AND id=?1"),[&parent.id]).unwrap();
    apply_parent_identity(&tx, &ns, &parent.id, None).unwrap();
    apply_parent_identity(&tx, &ns, &parent.id, Some(AGENT)).unwrap();
    assert!(!flush_ancestry(&tx, &ns, None, 128).unwrap().1);
    assert!(read_authority(&tx, &ns, AGENT, None).is_err());
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(parity(&store, &ns, None), before);
}

#[test]
fn parent_identity_capture_is_namespace_isolated_and_transactional() {
    let store = Store::open_memory("alder").unwrap();
    let parent = runtime(&store, "running");
    runtime(&store, "starting");
    let old = install(&store);
    let next = install(&store);
    let before = parity(&store, &old, None);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply_parent_identity(&tx, &next, &parent.id, None).unwrap();
        assert!(!flush_ancestry(&tx, &next, None, 128).unwrap().1);
        assert_eq!(read_authority(&tx, &old, AGENT, None).unwrap(), before);
        assert!(read_authority(&tx, &next, AGENT, None).is_err());
        tx.rollback().unwrap();
    }
    assert_eq!(parity(&store, &next, None), before);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for _ in 0..256 {
        if reclaim_namespace(&tx, &next, 1).unwrap() {
            break;
        }
    }
    let remains: bool=tx.query_row(&sql(&next,"SELECT EXISTS(SELECT 1 FROM local_agent_authority_parent_identities WHERE namespace=@NS@)"),[],|r|r.get(0)).unwrap();
    assert!(!remains);
    assert_eq!(read_authority(&tx, &old, AGENT, None).unwrap(), before);
    tx.commit().unwrap();
}

#[test]
fn identity_reverse_fanout_fences_even_children_outside_the_bounded_page() {
    let store = Store::open_memory("alder").unwrap();
    let parent = append_subject(&store, "agent/garden/unrelated");
    let ns = context(&store);
    let mut claims = Vec::new();
    for n in 0..=EDGES {
        let mut child = append_subject(&store, &format!("agent/garden/fanout/{n:03}"));
        child.predecessors = vec![parent.id.clone()];
        claims.push(child);
    }
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    apply_parent_identity(&tx, &ns, &parent.id, Some(&parent.subject)).unwrap();
    for child in &claims {
        let key = canonical::claim_key(&tx, &child.id).unwrap();
        apply_claim(&tx, &ns, None, Some((child, &key))).unwrap();
    }
    finish(&tx, &ns);
    ensure_closed(&tx, &ns).unwrap();
    let affected = apply_parent_identity(&tx, &ns, &parent.id, None).unwrap();
    assert_eq!(affected.len(), EDGES);
    let unvisited = claims
        .iter()
        .find(|c| !affected.contains(&c.subject))
        .unwrap();
    let queued: bool=tx.query_row(&sql(&ns,"SELECT EXISTS(SELECT 1 FROM local_agent_authority_dirty WHERE namespace=@NS@ AND agent=?1)"),[&unvisited.subject],|r|r.get(0)).unwrap();
    assert!(!queued);
    assert!(ensure_closed(&tx, &ns).is_err());
    assert!(
        read_authority(&tx, &ns, &unvisited.subject, None)
            .unwrap_err()
            .to_string()
            .contains("child fanout")
    );
    tx.rollback().unwrap();
}
