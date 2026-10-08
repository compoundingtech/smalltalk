//! Receipt durability and writer fairness under a catch-up page.
use super::runtime::Plain;
use super::*;
use crate::claim::ReplicaEnvelopeSignature;
use crate::fleet::MemberKey;
use crate::sqlite::WriterJob;
use rusqlite::functions::FunctionFlags;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";

fn node(name: &str) -> Arc<Store> {
    let store = Arc::new(Store::open_memory(name, Arc::new(Plain)).unwrap());
    store.bind_fleet(FLEET).unwrap();
    store
}

fn page(source: &Store, target: &Store, count: usize) -> ReplicationExchange {
    for index in 0..count {
        source
            .append_claim(&ClaimInput {
                subject: format!("note/catch-up/{index}"),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let mut inventory = target.replication_inventory().unwrap();
    inventory.accepts = Some(REPLICATION_PAGE_LIMIT);
    source
        .export_replication_exchange(FLEET, &inventory)
        .unwrap()
}

#[test]
fn export_stored_payload_budget_stops_before_fetching_the_next_envelope() {
    let source = node("birch");
    let target = node("cedar");
    page(&source, &target, 3);
    let identities = source.replication_inventory().unwrap().envelopes;
    assert_eq!(identities.len(), 3);
    let all = source.replica_envelopes(identities.clone()).unwrap();
    let first_stored = all[0].payload.bytes().unwrap().len();
    let second_stored = all[1].payload.bytes().unwrap().len();
    let budget = first_stored + second_stored - 1;

    let first_page = source
        .replica_envelopes_with_stored_budget(identities.clone(), Some(budget))
        .unwrap();
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].hash, identities[0].hash);
    let next_page = source
        .replica_envelopes_with_stored_budget(identities[1..].to_vec(), Some(budget))
        .unwrap();
    assert!(!next_page.is_empty());
    assert_eq!(next_page[0].hash, identities[1].hash);
    assert!(source
        .replica_envelopes_with_stored_budget(identities, Some(first_stored - 1))
        .is_err());
}

#[test]
fn export_stored_payload_budget_counts_legacy_text_past_nul_and_utf8_escapes() {
    let source = node("birch");
    let target = node("cedar");
    page(&source, &target, 1);
    let identities = source.replication_inventory().unwrap().envelopes;
    assert_eq!(identities.len(), 1);
    for text in [format!("A\0{}", "B".repeat(100)), "é\\\n\0C".repeat(30)] {
        source
            .connection
            .write()
            .execute(
                "UPDATE replica_envelopes SET payload=?1",
                rusqlite::params![text],
            )
            .unwrap();
        let connection = source.readers.get();
        let (characters, bytes): (i64, i64) = connection
            .query_row(
                "SELECT length(payload), octet_length(payload)
                 FROM replica_envelopes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        assert!(bytes > characters, "the metadata must count stored bytes");
        let actual = serde_json::to_vec(&source.replica_envelopes(identities.clone()).unwrap()[0].payload)
            .unwrap()
            .len();
        let stored = usize::try_from(bytes).unwrap();
        assert!(stored * 6 + 2 >= actual);
        assert!(source
            .replica_envelopes_with_stored_budget(identities.clone(), Some(stored - 1))
            .is_err());
        assert_eq!(
            source
                .replica_envelopes_with_stored_budget(identities.clone(), Some(stored))
                .unwrap()
                .len(),
            1,
            "a conservative JSON escape estimate must not reject a stored payload that fits"
        );
    }
}

#[test]
fn export_keeps_a_single_envelope_that_fits_the_full_signed_wire_cap() {
    let source = node("birch");
    let target = node("cedar");
    page(&source, &target, 1);
    // Exercise the export size path with a large stored BLOB. Admission validity is
    // tested separately; this diagnostic row tests that the wire budget does not
    // reject a response that the existing transport can carry.
    source
        .connection
        .write()
        .execute(
            "UPDATE replica_envelopes SET payload=?1",
            rusqlite::params![vec![7u8; 25 * 1024 * 1024]],
        )
        .unwrap();
    let exchange = source
        .export_replication_exchange(FLEET, &target.replication_inventory().unwrap())
        .unwrap();
    assert_eq!(exchange.envelopes.len(), 1);
    let bytes = serialized_bytes_bounded(&exchange, crate::sync::MAX_EXCHANGE_BYTES)
        .unwrap()
        .expect("the complete exchange fits the transport cap");
    assert!(bytes > 32 * 1024 * 1024);
}

#[test]
fn export_payload_page_sql_work_ignores_the_unfetched_tail() {
    let measure = |count| {
        let source = node("birch");
        let target = node("cedar");
        page(&source, &target, count);
        let identities = source.replication_inventory().unwrap().envelopes;
        let first = source.replica_envelopes(identities[..2].to_vec()).unwrap();
        let first_stored = first[0].payload.bytes().unwrap().len();
        let second_stored = first[1].payload.bytes().unwrap().len();
        let before = crate::sqlite::work::total();
        let selected = source
            .replica_envelopes_with_stored_budget(
                identities,
                Some(first_stored + second_stored - 1),
            )
            .unwrap();
        let work = crate::sqlite::work::total() - before;
        assert_eq!(selected.len(), 1);
        work
    };
    let small = measure(32);
    let large = measure(256);
    assert!(
        large.vm_steps <= small.vm_steps + 100,
        "export payload paging traversed the unfetched tail: small={small:?} large={large:?}"
    );
}

#[test]
fn export_body_limit_counts_complete_inventory_and_signature_proofs() {
    let source = node("birch");
    let target = node("cedar");
    let mut exchange = page(&source, &target, 3);
    assert_eq!(exchange.envelopes.len(), 3);
    exchange.signature_requests.push(ReplicaEnvelopeId {
        writer: "birch".into(),
        sequence: 3,
        hash: "f".repeat(512),
    });
    let original = exchange.clone();
    let mut without_payloads = exchange.clone();
    without_payloads.envelopes.clear();
    let base_bytes = serde_json::to_vec(&without_payloads).unwrap().len();
    let first_bytes = serde_json::to_vec(&exchange.envelopes[0]).unwrap().len();
    let limit = base_bytes + first_bytes;

    fit_replication_exchange_body(&mut exchange, limit).unwrap();
    assert_eq!(exchange.envelopes.len(), 1);
    assert_eq!(serde_json::to_vec(&exchange).unwrap().len(), limit);
    assert_eq!(exchange.inventory.digest, original.inventory.digest);
    assert_eq!(exchange.inventory.envelopes, original.inventory.envelopes);
    assert_eq!(exchange.inventory.buckets.len(), original.inventory.buckets.len());
    assert_eq!(exchange.signature_requests.len(), 1);

    let mut missing_first = original.clone();
    assert!(fit_replication_exchange_body(&mut missing_first, limit - 1).is_err());
    let mut oversized_proof = original;
    assert!(fit_replication_exchange_body(&mut oversized_proof, base_bytes - 1).is_err());
}

/// The injected SQL cost represents a populated runtime's per-claim admission work. The
/// queued write must commit before the remainder of the page, and its ACK has a 100ms CI
/// budget (including scheduler/commit overhead), separately from production's 50ms p99.
/// The 25ms case also proves the elapsed-work limit, rather than only a fixed envelope cap.
#[test]
fn catch_up_admission_bounds_the_work_a_queued_write_waits_for() {
    for (cost, count, prefix_limit) in [(3, 128, 8), (25, 12, 1)] {
        let source = node("birch");
        let target = node("cedar");
        let exchange = page(&source, &target, count);
        target
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap();
        let (entered, waiting) = mpsc::sync_channel(1);
        let (resume, resumed) = mpsc::sync_channel(1);
        let gate = Mutex::new(Some((entered, resumed)));
        {
            let connection = target.connection.write();
            connection
                .create_scalar_function(
                    "admission_cost",
                    0,
                    FunctionFlags::SQLITE_UTF8,
                    move |_| {
                        if let Some((entered, resumed)) = gate.lock().unwrap().take() {
                            entered.send(()).unwrap();
                            resumed.recv().unwrap();
                        }
                        std::thread::sleep(Duration::from_millis(cost));
                        Ok(0)
                    },
                )
                .unwrap();
            connection
                .execute_batch(
                    "CREATE TEMP TRIGGER admission_cost BEFORE INSERT ON claims
                 BEGIN SELECT admission_cost(); END;",
                )
                .unwrap();
        }
        let admitting = target.clone();
        let admission =
            std::thread::spawn(move || admitting.validate_replication_backlog().unwrap());
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();

        // Queue before releasing the first claim, without depending on thread scheduling.
        // This is the same durable batched writer path used by normal request writes.
        let (done, ack) = mpsc::sync_channel(1);
        let (observed, prefix) = mpsc::sync_channel(1);
        target.connection.send(WriterJob::Batched {
            run: Box::new(move |tx| {
                let admitted: usize = tx
                    .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
                    .unwrap();
                observed.send(admitted).unwrap();
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES('queued-write','committed')",
                    [],
                )
                .unwrap();
                true
            }),
            profile: None,
            wait: None,
            done,
        });
        let started = Instant::now();
        resume.send(()).unwrap();
        let result = ack.recv_timeout(Duration::from_secs(5));
        let latency = started.elapsed();
        // Join before asserting so a failed control cannot leave an owned background writer.
        let outcome = admission.join().unwrap();
        result.unwrap().unwrap();
        let prefix = prefix.recv().unwrap();
        assert!(
            prefix > 0 && prefix <= prefix_limit,
            "queued write followed {prefix} claims"
        );
        assert!(
            prefix < count,
            "the page kept the writer through its last claim"
        );
        assert!(
            latency < Duration::from_millis(100),
            "queued ACK took {latency:?}"
        );
        assert_eq!(outcome.valid, count);
        assert_eq!(
            target
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get::<_, usize>(0))
                .unwrap(),
            count
        );
        assert_eq!(
            target
                .receive_replication_exchange("birch", FLEET, &exchange)
                .unwrap()
                .received,
            0
        );
        assert_eq!(target.validate_replication_backlog().unwrap().valid, 0);
    }
}

#[test]
fn preverified_receipt_stays_atomic_and_signature_retry_stays_idempotent() {
    let source = node("birch");
    let target = node("cedar");
    let mut exchange = page(&source, &target, 3);
    let (key, _) = MemberKey::generate().unwrap();
    for envelope in &mut exchange.envelopes {
        envelope.member_key = Some(key.public().into());
        envelope.signature = Some(key.sign(&crate::fleet::envelope_signature_message(
            FLEET,
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
        )));
    }
    let wrong_fleet = &exchange.envelopes[1];
    exchange.envelopes[1].signature = Some(key.sign(&crate::fleet::envelope_signature_message(
        "another-fleet",
        &wrong_fleet.writer,
        wrong_fleet.sequence,
        &wrong_fleet.hash,
    )));
    let last = exchange.envelopes.last().unwrap().sequence;
    target
        .connection
        .write()
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER fail_receipt BEFORE INSERT ON replica_envelopes
         WHEN NEW.sequence={last} BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;",
        ))
        .unwrap();
    assert!(
        target
            .receive_replication_exchange("birch", FLEET, &exchange)
            .is_err()
    );
    for table in [
        "replica_envelopes",
        "replica_envelope_signatures",
        "replication_peers",
    ] {
        let count: usize = target
            .readers
            .get()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "failed receipt committed {table}");
    }
    target
        .connection
        .write()
        .execute_batch("DROP TRIGGER fail_receipt")
        .unwrap();
    let received = target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    assert_eq!(received.received, 3);
    assert_eq!(received.signatures, 2);
    let unsigned = &exchange.envelopes[1];
    exchange.signatures.push(ReplicaEnvelopeSignature {
        writer: unsigned.writer.clone(),
        sequence: unsigned.sequence,
        hash: unsigned.hash.clone(),
        member_key: key.public().into(),
        signature: key.sign(&crate::fleet::envelope_signature_message(
            FLEET,
            &unsigned.writer,
            unsigned.sequence,
            &unsigned.hash,
        )),
    });
    let retry = target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    assert_eq!(retry.received, 0);
    assert_eq!(retry.duplicate, 3);
    assert_eq!(retry.signatures, 1);
    let retry = target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    assert_eq!(retry.signatures, 0);
    assert_eq!(retry.inventory.digest, received.inventory.digest);
}

#[test]
fn modern_projection_comparison_needs_alignment_but_not_a_legacy_digest() {
    for (modern, legacy, aligned, compared) in [
        (true, false, true, true),
        (true, false, false, false),
        (false, false, true, false),
        (false, true, true, true),
    ] {
        let source = node("birch");
        let target = node("cedar");
        let mut summary = source.export_replication_summary(FLEET).unwrap();
        if modern {
            *summary
                .projection_digests
                .values_mut()
                .next()
                .expect("the Plain runtime registers projection tables") = "a".repeat(64);
        } else {
            summary.projection_digests.clear();
        }
        if !legacy {
            summary.graph_digest.clear();
        }
        if !aligned {
            summary.inventory.digest = "unaligned".into();
        }
        target
            .receive_replication_exchange("birch", FLEET, &summary)
            .unwrap();
        let progress = target.replication_sync.lock().unwrap();
        assert_eq!(
            progress["birch"].graph_compared_at_unix_ms.is_some(),
            compared
        );
    }
}

#[test]
fn partial_or_malformed_modern_projection_maps_do_not_complete_comparison() {
    for (malformed, legacy, compared) in [
        (false, false, false),
        (true, false, false),
        (false, true, true),
    ] {
        let source = node("birch");
        let target = node("cedar");
        let mut summary = source.export_replication_summary(FLEET).unwrap();
        if malformed {
            *summary.projection_digests.values_mut().next().unwrap() = String::new();
        } else {
            let key = summary.projection_digests.keys().next().unwrap().clone();
            summary.projection_digests.remove(&key);
        }
        if !legacy {
            summary.graph_digest.clear();
        }
        target
            .receive_replication_exchange("birch", FLEET, &summary)
            .unwrap();
        let progress = target.replication_sync.lock().unwrap();
        assert_eq!(
            progress["birch"].graph_compared_at_unix_ms.is_some(),
            compared,
            "malformed={malformed} legacy={legacy}"
        );
    }
}

#[test]
fn preverification_cannot_restore_a_checkpointed_payload_or_signature() {
    let source = node("birch");
    let target = node("cedar");
    let mut exchange = page(&source, &target, 1);
    let (key, _) = MemberKey::generate().unwrap();
    let envelope = &mut exchange.envelopes[0];
    let signature = key.sign(&crate::fleet::envelope_signature_message(
        FLEET,
        &envelope.writer,
        envelope.sequence,
        &envelope.hash,
    ));
    envelope.member_key = Some(key.public().into());
    envelope.signature = Some(signature.clone());
    exchange.signatures.push(ReplicaEnvelopeSignature {
        writer: envelope.writer.clone(),
        sequence: envelope.sequence,
        hash: envelope.hash.clone(),
        member_key: key.public().into(),
        signature,
    });
    // Synthetic local exclusion: this checks receipt, not checkpoint certificate adoption.
    target
        .connection
        .write()
        .execute(
            "INSERT INTO checkpoint_envelopes VALUES(?1,?2,?3,?4,'checkpoint/test')",
            params![
                envelope.writer,
                envelope.sequence,
                envelope.hash,
                envelope.accepted_at_unix_ms as i64
            ],
        )
        .unwrap();
    let receipt = target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    assert_eq!(
        (receipt.received, receipt.duplicate, receipt.signatures),
        (0, 1, 0)
    );
    for table in ["replica_envelopes", "replica_envelope_signatures"] {
        let count: usize = target
            .readers
            .get()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
