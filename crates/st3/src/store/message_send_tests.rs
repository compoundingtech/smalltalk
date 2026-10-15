//! Signed admission retains one committed message/signature and no temporary expectation.
use super::*;
use smallclaims::principal::{ClaimSignature, FIELDS_FORMAT, Judged, fields_signing_bytes};

fn request() -> ClaimInput {
    ClaimInput {
        subject: "message/local-signed-send".into(),
        kind: "message.sent".into(),
        actor: Some("person/eval".into()),
        fields: BTreeMap::from([
            ("from".into(), json!("person/eval")),
            ("to".into(), json!("agent/eval.recipient")),
            ("content".into(), json!("One durable local send")),
            ("status".into(), json!("sent")),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some("local-signed-send-request".into()),
    }
}

fn signed(input: &ClaimInput) -> ClaimSignature {
    let (key, _) = smallclaims::fleet::MemberKey::generate().unwrap();
    let mut signature = ClaimSignature::sign(
        &key,
        "",
        input.actor.as_deref().unwrap(),
        None,
        vec![],
        now_ms() as u64,
    );
    signature.format = Some(FIELDS_FORMAT.into());
    signature.signed_fields = vec!["content".into(), "from".into(), "to".into()];
    let fields = serde_json::to_value(&input.fields).unwrap();
    signature.signature = key.sign(&fields_signing_bytes(
        &input.subject,
        &input.kind,
        input.actor.as_deref(),
        &fields,
        &signature.signed_fields,
        &signature.signer,
        None,
        &signature.key,
        &signature.chain,
        &signature.nonce,
        signature.signed_at_unix_ms,
    ));
    signature
}

#[test]
fn signed_send_commits_once_and_retries_keep_the_original_signature() {
    let store = Store::open_memory("node").unwrap();
    let input = request();
    let signature = signed(&input);
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let observed = snapshots.clone();
    let _observer = store.connection.observe_commits(move |connection| {
        let counts: (u64,u64) = connection.query_row(
            "SELECT (SELECT COUNT(*) FROM claims WHERE subject='message/local-signed-send'),(SELECT COUNT(*) FROM expected_claim_signatures)",
            [], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        observed.lock().unwrap().push(counts);
    });
    let (record, appended) = store.append_signed_message(&input, &signature).unwrap();
    assert!(appended);
    assert_eq!(
        *snapshots.lock().unwrap(),
        [(1, 0)],
        "ACK follows one committed message and consumed signature"
    );
    let held = store.claim_signature(&record.id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&held).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
    assert!(held.verifies(&Judged {
        id: &record.id,
        subject: &record.subject,
        kind: &record.kind,
        actor: record.actor.as_deref(),
        content: String::new(),
        fields: &record.body["fields"]
    }));
    let repeat_signature = signed(&input);
    let (repeat, appended) = store
        .append_signed_message(&input, &repeat_signature)
        .unwrap();
    assert!(!appended);
    assert_eq!(repeat.id, record.id);
    assert_eq!(
        store.claim_signature(&record.id).unwrap().unwrap().nonce,
        signature.nonce
    );
    assert!(
        !store
            .signature_nonce_used(&repeat_signature.key, &repeat_signature.nonce)
            .unwrap()
    );
    assert!(
        snapshots
            .lock()
            .unwrap()
            .iter()
            .all(|counts| *counts == (1, 0))
    );
    let mut conflict = input;
    conflict
        .fields
        .insert("content".into(), json!("A different send"));
    assert_eq!(
        store
            .append_signed_message(&conflict, &signed(&conflict))
            .unwrap_err()
            .code,
        "idempotency-mismatch"
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM expected_claim_signatures",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn failed_signed_send_rolls_back_its_message_and_temporary_signature() {
    let store = Store::open_memory("node").unwrap();
    store.connection.write().execute_batch(
        /* sqlite-large-value: allow-for-now lv-h0cc0912e2583 owner=st3::store::message_send_tests TODO=prepare-body-and-predecessor-values */ "CREATE TRIGGER reject_fixture_send BEFORE INSERT ON claims
         WHEN NEW.subject='message/local-signed-send' BEGIN SELECT RAISE(ABORT,'injected send failure'); END;",
    ).unwrap();
    let input = request();
    let signature = signed(&input);
    assert!(store.append_signed_message(&input, &signature).is_err());
    assert!(store.message(&input.subject).unwrap().is_none());
    assert!(
        !store
            .signature_nonce_used(&signature.key, &signature.nonce)
            .unwrap()
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM expected_claim_signatures",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    store
        .connection
        .write()
        .execute_batch(/* sqlite-large-value: allow-for-now lv-hc99ba59305d8 owner=st3::store::message_send_tests TODO=bound-or-reference-values-and-reclamation */ "DROP TRIGGER reject_fixture_send")
        .unwrap();
    assert!(store.append_signed_message(&input, &signature).unwrap().1);
    assert_eq!(
        store.message(&input.subject).unwrap().unwrap().status,
        "sent"
    );
}

#[test]
fn digest_mismatched_message_operation_repair_persists_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invented-message.db");
    let store = Store::open(&path, "fixture").unwrap();
    store.project_replication_backlog().unwrap();
    let input = request();
    let signature = signed(&input);
    let (claim, _) = store.append_signed_message(&input, &signature).unwrap();
    let (operation, digest) = operation_parts(&claim.body).unwrap();
    let operation = operation.to_owned();
    let digest = digest.to_owned();
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h8cedba85b859 owner=st3::store::message_send_tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE operations SET request_digest='invented-stale-digest' WHERE id=?1",
            [&operation],
        )
        .unwrap();
    let mut lines = Vec::new();
    store
        .project_replication_backlog_with_log("fixture/startup", |line| lines.push(line.to_owned()))
        .unwrap();
    assert!(
        lines.is_empty(),
        "receipt-only repair must be incremental: {lines:?}"
    );
    assert_eq!(
        operation_tx(&store.readers.get(), &operation)
            .unwrap()
            .unwrap(),
        (digest.clone(), claim.id.clone(), "active".into())
    );
    assert_eq!(
        store.claim_by_id(&claim.id).unwrap().unwrap().body,
        claim.body
    );
    assert_eq!(
        serde_json::to_value(store.claim_signature(&claim.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
    drop(store);
    let reopened = Store::open(&path, "fixture").unwrap();
    let mut lines = Vec::new();
    reopened
        .project_replication_backlog_with_log("fixture/reopen", |line| lines.push(line.to_owned()))
        .unwrap();
    assert!(
        lines.is_empty(),
        "committed stale receipt repair must survive restart: {lines:?}"
    );
    assert_eq!(
        operation_tx(&reopened.readers.get(), &operation)
            .unwrap()
            .unwrap(),
        (digest, claim.id.clone(), "active".into())
    );
    assert_eq!(
        serde_json::to_value(reopened.claim_signature(&claim.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
}

#[test]
fn resolved_message_operation_conflict_does_not_replay_for_another_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invented-conflict.db");
    let store = Store::open(&path, "fixture").unwrap();
    store.project_replication_backlog().unwrap();
    let input = request();
    let signature = signed(&input);
    let (original, _) = store.append_signed_message(&input, &signature).unwrap();
    store.project_replication_backlog().unwrap();
    let (operation, _) = operation_parts(&original.body).unwrap();
    let operation = operation.to_owned();
    let mut body = original.body.clone();
    body["_operation"]["request_digest"] = json!("invented-conflicting-digest");
    let incoming = {
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        let claim = append_claim_record_tx(
            &transaction,
            "invented-peer",
            &original.subject,
            "message.sent",
            original.actor.as_deref(),
            &body,
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
        claim
    };
    let mut lines = Vec::new();
    store
        .project_replication_backlog_with_log("fixture/conflict", |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
    assert!(
        lines.is_empty(),
        "message conflict repair must be incremental: {lines:?}"
    );
    let resolved = operation_tx(&store.readers.get(), &operation)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.2, "conflict");
    assert_eq!(
        store.claim_by_id(&original.id).unwrap().unwrap().body,
        original.body
    );
    assert_eq!(store.claim_by_id(&incoming.id).unwrap().unwrap().body, body);
    assert_eq!(
        serde_json::to_value(store.claim_signature(&original.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
    drop(store);
    let reopened = Store::open(&path, "fixture").unwrap();
    reopened.project_replication_backlog().unwrap();
    assert_eq!(
        operation_tx(&reopened.readers.get(), &operation)
            .unwrap()
            .unwrap(),
        resolved
    );
    {
        let mut connection = reopened.connection.write();
        let transaction = connection.transaction().unwrap();
        append_claim_record_tx(
            &transaction,
            "invented-other-peer",
            &original.subject,
            "message.sent",
            original.actor.as_deref(),
            &body,
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    let mut lines = Vec::new();
    reopened
        .project_replication_backlog_with_log("fixture/new-receipt", |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("projection full replay")),
        "durably recorded conflict must not force another full-store replay: {lines:?}"
    );
    assert_eq!(
        reopened
            .claim_signature(&original.id)
            .unwrap()
            .unwrap()
            .nonce,
        signature.nonce
    );
}

fn operation_rows(connection: &Connection) -> Vec<(String, String, String, String)> {
    connection
        .prepare("SELECT id,request_digest,canonical_claim_id,state FROM operations ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn assert_operation_replay_parity(store: &Store) {
    let before = {
        let connection = store.readers.get();
        (
            operation_rows(&connection),
            graph_digest(&connection).unwrap(),
            projection_digest::tables(&connection).unwrap(),
            projection_digest::oracle(&connection).unwrap(),
        )
    };
    store.replay_replication_graph().unwrap();
    let connection = store.readers.get();
    assert_eq!(operation_rows(&connection), before.0);
    assert_eq!(graph_digest(&connection).unwrap(), before.1);
    assert_eq!(projection_digest::tables(&connection).unwrap(), before.2);
    assert_eq!(projection_digest::oracle(&connection).unwrap(), before.3);
}

#[test]
fn message_operation_reconciliation_matches_replay_for_orders_tombstones_and_repairs() {
    for reverse in [false, true] {
        for mode in ["stored", "checkpoint", "repaired"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("invented-parity.db");
            let store = Store::open(&path, "fixture").unwrap();
            store.project_replication_backlog().unwrap();
            let digests = if reverse {
                ["digest-b", "digest-a"]
            } else {
                ["digest-a", "digest-b"]
            };
            let mut first = None;
            for (number, digest) in digests.into_iter().enumerate() {
                let mut connection = store.connection.write();
                let tx = connection.transaction().unwrap();
                if number == 1 {
                    let claim: &ClaimRecord = first.as_ref().unwrap();
                    if mode == "checkpoint" {
                        tx.execute(/* sqlite-large-value: allow-for-now lv-h6a085eac427b owner=st3::store::message_send_tests TODO=stage-generation-and-keyed-reclamation */ "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
                            actor,predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
                            VALUES (?1,'invented-peer',1,'invented-envelope',?2,'message.sent',NULL,'[]',
                            'op/invented-parity',?3,0,'checkpoint/2026-10-01')",
                            params![claim.id, claim.subject, digests[0]]).unwrap();
                        tx.execute(/* sqlite-large-value: allow-for-now lv-h127ead89939b owner=st3::store::message_send_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM operations WHERE id='op/invented-parity'", [])
                            .unwrap();
                        tx.execute(/* sqlite-large-value: allow-for-now lv-haf5d7ac0f802 owner=st3::store::message_send_tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE id=?1", [&claim.id])
                            .unwrap();
                    } else if mode == "repaired" {
                        tx.execute(/* sqlite-large-value: allow-for-now lv-h9e8f3a6a081f owner=st3::store::message_send_tests TODO=chunk-or-reference-exact-wire-values */ "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
                            raw,state,claim_id,updated_at_unix_ms) VALUES ('record/invented-repair','invented-peer',
                            1,'invented-envelope',0,X'','repaired',?1,'1')", [&claim.id]).unwrap();
                    }
                }
                let claim = append_claim_record_tx(&tx, "invented-peer", "message/invented-parity",
                    "message.sent", Some("person/example"), &json!({"fields":{
                        "from":"person/example", "to":"agent/invented", "content":"Invented receipt", "status":"sent"
                    }, "_operation":{"id":"op/invented-parity", "request_digest":digest}}), &[], None).unwrap();
                tx.commit().unwrap();
                drop(connection);
                if number == 0 {
                    first = Some(claim);
                }
                FULL_REPLAYS.with(|count| count.set(0));
                store.project_replication_backlog().unwrap();
                assert_eq!(
                    FULL_REPLAYS.with(std::cell::Cell::get),
                    0,
                    "{reverse}/{mode}/{number}"
                );
                assert_operation_replay_parity(&store);
            }
            let expected_state = if mode == "repaired" {
                "active"
            } else {
                "conflict"
            };
            assert_eq!(
                operation_tx(&store.readers.get(), "op/invented-parity")
                    .unwrap()
                    .unwrap()
                    .2,
                expected_state
            );
            let before = operation_rows(&store.readers.get());
            drop(store);
            let reopened = Store::open(&path, "fixture").unwrap();
            FULL_REPLAYS.with(|count| count.set(0));
            reopened.project_replication_backlog().unwrap();
            assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
            assert_eq!(operation_rows(&reopened.readers.get()), before);
            // Another receipt exercises the keyed reducer with a retained tombstone,
            // including a minimum digest for which no stored claim remains.
            {
                let mut connection = reopened.connection.write();
                let tx = connection.transaction().unwrap();
                append_claim_record_tx(&tx, "invented-other-peer", "message/invented-parity",
                    "message.sent", Some("person/example"), &json!({"fields":{
                        "from":"person/example", "to":"agent/invented", "content":"Another receipt", "status":"sent"
                    }, "_operation":{"id":"op/invented-parity", "request_digest":digests[1]}}), &[], None).unwrap();
                tx.commit().unwrap();
            }
            reopened.project_replication_backlog().unwrap();
            assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
            assert_eq!(
                operation_tx(&reopened.readers.get(), "op/invented-parity")
                    .unwrap()
                    .unwrap()
                    .2,
                expected_state
            );
            assert_operation_replay_parity(&reopened);
        }
    }
}

#[test]
fn message_operation_conflict_receipt_cost_depends_on_its_own_history() {
    use smallclaims::sqlite::work::SqliteWorkScope;
    let mut costs = Vec::new();
    for count in [2, 10, 100] {
        let store = Store::open_memory("fixture").unwrap();
        store.project_replication_backlog().unwrap();
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            for number in 0..count {
                append_claim_record_tx(&tx, "invented-peer", "message/invented-cost", "message.sent",
                    Some("person/example"), &json!({"fields":{"from":"person/example",
                        "to":"agent/invented", "content":format!("Invented content {number}"), "status":"sent"},
                        "_operation":{"id":"op/invented-cost", "request_digest":format!("digest-{number:03}")}}),
                    &[], None).unwrap();
            }
            tx.commit().unwrap();
        }
        store.project_replication_backlog().unwrap();
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            append_claim_record_tx(
                &tx,
                "invented-peer",
                "message/invented-cost",
                "message.sent",
                Some("person/example"),
                &json!({"fields":{"from":"person/example",
                    "to":"agent/invented", "content":"Another receipt", "status":"sent"},
                    "_operation":{"id":"op/invented-cost", "request_digest":"digest-001"}}),
                &[],
                None,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        FULL_REPLAYS.with(|replays| replays.set(0));
        let work = SqliteWorkScope::start();
        store.project_replication_backlog().unwrap();
        let cost = work.finish();
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
        eprintln!(
            "message conflict receipt prior_claims={count} statements={} vm_steps={} fullscan_steps={}",
            cost.statements, cost.vm_steps, cost.fullscan_steps
        );
        assert!(cost.vm_steps > 0);
        costs.push(cost);
    }
    assert_eq!(costs[0].statements, costs[1].statements);
    assert_eq!(costs[1].statements, costs[2].statements);
    assert!(
        costs[2].vm_steps < costs[0].vm_steps * 50,
        "one indexed fold should grow linearly, not repeat a whole-store replay per receipt"
    );
}

#[test]
fn message_operation_reconciliation_rolls_back_with_its_frontier() {
    let store = Store::open_memory("fixture").unwrap();
    store.project_replication_backlog().unwrap();
    let (original, _) = store
        .append_signed_message(&request(), &signed(&request()))
        .unwrap();
    store.project_replication_backlog().unwrap();
    let (operation, _) = operation_parts(&original.body).unwrap();
    let before = operation_rows(&store.readers.get());
    let index = store.index().unwrap();
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    let frontier: u64 = tx
        .query_row(
            "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut body = original.body.clone();
    body["_operation"]["request_digest"] = json!("invented-rollback-digest");
    let incoming = append_claim_record_tx(
        &tx,
        "invented-peer",
        &original.subject,
        "message.sent",
        original.actor.as_deref(),
        &body,
        &[],
        None,
    )
    .unwrap();
    assert_eq!(
        try_project_simple_replication_tx(&tx, "fixture", incoming.store_index).unwrap(),
        IncrementalProjection::Projected
    );
    assert_eq!(operation_tx(&tx, operation).unwrap().unwrap().2, "conflict");
    assert_eq!(
        tx.query_row(
            "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [],
            |row| row.get::<_, u64>(0)
        )
        .unwrap(),
        frontier
    );
    tx.rollback().unwrap();
    drop(connection);
    assert_eq!(store.index().unwrap(), index);
    assert_eq!(operation_rows(&store.readers.get()), before);
    assert!(store.claim_by_id(&incoming.id).unwrap().is_none());
}

#[test]
fn message_operation_late_conflict_after_duplicate_receipt_matches_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invented-late-conflict.db");
    let mut store = Store::open(&path, "fixture").unwrap();
    store.project_replication_backlog().unwrap();
    let input = request();
    let signature = signed(&input);
    let (original, _) = store.append_signed_message(&input, &signature).unwrap();
    store.project_replication_backlog().unwrap();
    let (operation, _) = operation_parts(&original.body).unwrap();
    let mut body = original.body.clone();
    for conflicting in [false, true] {
        if conflicting {
            body["_operation"]["request_digest"] = json!("invented-late-conflict");
        }
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            append_claim_record_tx(
                &tx,
                "invented-peer",
                &original.subject,
                "message.sent",
                original.actor.as_deref(),
                &body,
                &[],
                None,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        FULL_REPLAYS.with(|count| count.set(0));
        store.project_replication_backlog().unwrap();
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
        assert_eq!(
            operation_tx(&store.readers.get(), operation)
                .unwrap()
                .unwrap()
                .2,
            if conflicting { "conflict" } else { "active" }
        );
        assert_operation_replay_parity(&store);
        assert_eq!(
            serde_json::to_value(store.claim_signature(&original.id).unwrap().unwrap()).unwrap(),
            serde_json::to_value(&signature).unwrap()
        );
        drop(store);
        store = Store::open(&path, "fixture").unwrap();
        store.project_replication_backlog().unwrap();
    }
    assert_eq!(
        store
            .append_signed_message(&input, &signed(&input))
            .unwrap_err()
            .code,
        "idempotency-conflict"
    );
    assert_eq!(
        store.claim_by_id(&original.id).unwrap().unwrap().body,
        original.body
    );
    assert_eq!(
        serde_json::to_value(store.claim_signature(&original.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
}
