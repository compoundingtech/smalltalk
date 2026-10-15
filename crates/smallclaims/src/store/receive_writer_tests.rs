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
                    /* sqlite-large-value: allow-for-now lv-hc280ea0222d1 owner=smallclaims::store::receive_writer_tests TODO=prepare-body-and-predecessor-values */ "CREATE TEMP TRIGGER admission_cost BEFORE INSERT ON claims
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
            enqueued: std::time::Instant::now(),
            run: Box::new(move |tx| {
                let admitted: usize = tx
                    .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
                    .unwrap();
                observed.send(admitted).unwrap();
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-hc19c6b34c2e3 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO meta(key,value) VALUES('queued-write','committed')",
                    [],
                )
                .unwrap();
                true
            }),
            profile: None,
            wait: None,
            holder: crate::windows::holds::Holder::current(),
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
            /* sqlite-large-value: allow-for-now lv-h467c21549699 owner=smallclaims::store::receive_writer_tests TODO=chunk-or-reference-exact-wire-values */ "CREATE TEMP TRIGGER fail_receipt BEFORE INSERT ON replica_envelopes
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
            .query_row(/* sqlite-large-value: allow-for-now lv-h29de37689fd2 owner=smallclaims::store::receive_writer_tests TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "failed receipt committed {table}");
    }
    target
        .connection
        .write()
        .execute_batch(/* sqlite-large-value: allow-for-now lv-h114ae93c1ae3 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DROP TRIGGER fail_receipt")
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
        summary.projection_digests.clear();
        if modern {
            summary
                .projection_digests
                .insert("documents".into(), "different".into());
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
            /* sqlite-large-value: allow-for-now lv-h40a6a477e4f0 owner=smallclaims::store::receive_writer_tests TODO=stage-generation-and-keyed-reclamation */ "INSERT INTO checkpoint_envelopes VALUES(?1,?2,?3,?4,'checkpoint/test')",
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
            .query_row(/* sqlite-large-value: allow-for-now lv-h8b7177f7a629 owner=smallclaims::store::receive_writer_tests TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn admission_rechecks_authority_when_a_foreground_change_overtakes_its_preparation() {
    let source = node("birch");
    let target = node("cedar");
    let exchange = page(&source, &target, 1);
    target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    let (prepared, preparing) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let admitting = target.clone();
    let admission = std::thread::spawn(move || {
        BEFORE_ADMISSION_LOAN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                prepared.send(()).unwrap();
                resumed.recv_timeout(Duration::from_secs(10)).unwrap();
            }));
        });
        admitting.validate_replication_backlog().unwrap()
    });
    preparing.recv_timeout(Duration::from_secs(5)).unwrap();
    // The prepared fold treated birch as legacy. Establish cedar as the signed anchor,
    // then let it admit birch: the old unsigned page must now wait for that exact member key.
    let (key, _) = MemberKey::generate().unwrap();
    let key = Arc::new(key);
    target.set_member_key(Some(key.clone())).unwrap();
    target.pin_fleet_anchor(key.public()).unwrap();
    target
        .admit_fleet_anchor(FLEET, key.public(), "listening")
        .unwrap();
    target
        .append_claim(&ClaimInput {
            subject: "host/birch".into(),
            kind: "fleet.member-admitted".into(),
            actor: None,
            fields: BTreeMap::from([
                ("fleet_id".into(), Value::String(FLEET.into())),
                ("member_key".into(), Value::String(key.public().into())),
                ("via".into(), Value::String("invite".into())),
                ("mode".into(), Value::String("listening".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let membership = target.fleet_membership().unwrap();
    resume.send(()).unwrap();
    let outcome = admission.join().unwrap();
    assert!(matches!(
        membership.window("birch", exchange.envelopes[0].sequence),
        crate::fleet::Window::Keyed(_)
    ));
    assert_eq!((outcome.valid, outcome.held), (0, 1));
    assert_eq!(
        target
            .readers
            .get()
            .query_row(
                "SELECT reason FROM replica_envelope_holds WHERE writer='birch'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "unsigned"
    );
    // A fresh exact signature then admits the same durable payload, rather than losing it.
    let envelope = &exchange.envelopes[0];
    let mut retry = exchange.clone();
    retry.signatures.push(ReplicaEnvelopeSignature {
        writer: envelope.writer.clone(),
        sequence: envelope.sequence,
        hash: envelope.hash.clone(),
        member_key: key.public().into(),
        signature: key.sign(&crate::fleet::envelope_signature_message(
            FLEET,
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
        )),
    });
    assert_eq!(
        target
            .receive_replication_exchange("birch", FLEET, &retry)
            .unwrap()
            .received,
        0
    );
    assert_eq!(target.validate_replication_backlog().unwrap().valid, 1);
}

#[test]
fn admission_does_not_restore_source_dropped_before_its_background_loan() {
    let source = node("birch");
    let target = node("cedar");
    let exchange = page(&source, &target, 1);
    target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    let (prepared, preparing) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let admitting = target.clone();
    let admission = std::thread::spawn(move || {
        BEFORE_ADMISSION_LOAN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                prepared.send(()).unwrap();
                resumed.recv_timeout(Duration::from_secs(10)).unwrap();
            }));
        });
        admitting.validate_replication_backlog().unwrap()
    });
    preparing.recv_timeout(Duration::from_secs(5)).unwrap();
    let envelope = &exchange.envelopes[0];
    // Synthetic retained exclusion; certificate adoption itself has separate controls.
    {
        let mut writer = target.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute(
            /* sqlite-large-value: allow-for-now lv-h9fe75ab6969f owner=smallclaims::store::receive_writer_tests TODO=stage-generation-and-keyed-reclamation */ "INSERT INTO checkpoint_envelopes VALUES(?1,?2,?3,?4,'checkpoint/test')",
            params![
                envelope.writer,
                envelope.sequence,
                envelope.hash,
                envelope.accepted_at_unix_ms as i64
            ],
        )
        .unwrap();
        tx.execute(
            /* sqlite-large-value: allow-for-now lv-h970b515ca97b owner=smallclaims::store::receive_writer_tests TODO=chunk-or-reference-exact-wire-values */ "DELETE FROM replica_envelopes WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
            params![envelope.writer, envelope.sequence, envelope.hash],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    resume.send(()).unwrap();
    assert_eq!(admission.join().unwrap().valid, 0);
    for table in [
        "claims",
        "replica_records",
        "replica_envelope_holds",
        "replica_envelopes",
    ] {
        assert_eq!(
            target
                .readers
                .get()
                .query_row(/* sqlite-large-value: allow-for-now lv-ha64a86b53612 owner=smallclaims::store::receive_writer_tests TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0,
            "restored {table}"
        );
    }
}

#[test]
fn admission_reuses_a_generation_checked_fold_across_unchanged_pages() {
    let source = node("birch");
    let target = node("cedar");
    let exchange = page(&source, &target, 33);
    target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    ADMISSION_MEMBERSHIP_FOLDS.with(|count| count.set(0));
    ADMISSION_TRANSACTIONS.with(|count| count.set(0));
    assert_eq!(target.validate_replication_backlog().unwrap().valid, 33);
    ADMISSION_TRANSACTIONS.with(|count| assert!(count.get() >= 5));
    ADMISSION_MEMBERSHIP_FOLDS.with(|count| {
        assert_eq!(
            count.get(),
            1,
            "unchanged pages must not repeat the full membership fold"
        )
    });
}

#[test]
fn background_receipt_finalizer_failure_rolls_back_the_whole_page_and_retry() {
    let source = node("birch");
    let target = node("cedar");
    let exchange = page(&source, &target, 3);
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let failing = fail.clone();
    target
        .connection
        .install_transaction_finalizer(move |tx| {
            let received: bool =
                tx.query_row("SELECT EXISTS(SELECT 1 FROM replica_envelopes)", [], |r| {
                    r.get(0)
                })?;
            anyhow::ensure!(
                !received || !failing.load(Ordering::Acquire),
                "injected receipt finalizer failure"
            );
            Ok(())
        })
        .unwrap();
    assert!(
        target
            .receive_replication_exchange("birch", FLEET, &exchange)
            .is_err()
    );
    for table in ["replica_envelopes", "replication_peers"] {
        assert_eq!(
            target
                .readers
                .get()
                .query_row(/* sqlite-large-value: allow-for-now lv-h3f0a6e9339fd owner=smallclaims::store::receive_writer_tests TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        target.replication_timers.exchanges.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        target
            .replication_timers
            .envelopes_received
            .load(Ordering::Relaxed),
        0
    );
    fail.store(false, Ordering::Release);
    assert_eq!(
        target
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap()
            .received,
        3
    );
    assert_eq!(
        target
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap()
            .received,
        0
    );
}

#[test]
fn trimming_the_highest_peer_batches_cannot_skip_new_local_envelopes() {
    let source = node("birch");
    let target = node("cedar");
    let observer = node("elm");
    let exchange = page(&source, &target, 8);
    target
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    target.seal_local_batches().unwrap();
    let frontier = target.seeded_batch_rowid.load(Ordering::Acquire);
    {
        // The checkpoint's physical deletion removes its claims, envelopes and orphan batches.
        let mut connection = target.connection.write();
        let transaction = connection.transaction().unwrap();
        let envelopes = exchange
            .envelopes
            .iter()
            .map(|envelope| EnvelopeTombstone {
                writer: envelope.writer.clone(),
                sequence: envelope.sequence,
                envelope_hash: envelope.hash.clone(),
                accepted_at_unix_ms: envelope.accepted_at_unix_ms,
            })
            .collect::<Vec<_>>();
        let claims = transaction
            .prepare("SELECT id FROM claims")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|id| ClaimTombstone {
                id: id.unwrap(),
                operation_id: None,
                writer: String::new(),
                sequence: 0,
                envelope_hash: String::new(),
                subject: String::new(),
                kind: String::new(),
                actor: None,
                predecessors: Vec::new(),
                request_digest: None,
                accepted_at_unix_ms: 0,
            })
            .collect::<Vec<_>>();
        super::checkpoint::delete_dropped_rows_tx(&transaction, &envelopes, &claims).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(max_batch_rowid(&target.readers.get()).unwrap(), 0);
    let claim = target
        .append_claim(&ClaimInput {
            subject: "note/after-adoption".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(max_batch_rowid(&target.readers.get()).unwrap() > frontier);
    let exported = target
        .export_replication_exchange(FLEET, &observer.replication_inventory().unwrap())
        .unwrap();
    assert_eq!(exported.envelopes.len(), 1);
    observer
        .receive_replication_exchange("cedar", FLEET, &exported)
        .unwrap();
    observer.validate_replication_backlog().unwrap();
    assert!(observer.claim_by_id(&claim.id).unwrap().is_some());
}

#[test]
fn an_old_sealing_frontier_recovers_skipped_batches_once_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    let claim;
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        claim = store
            .append_claim(&ClaimInput {
                subject: "note/skipped-before-upgrade".into(),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // An old build reused rowid 1 after deleting rows through 8. It had already advanced
        // the durable sealing cursor, so the retained claim never acquired its source envelope.
        store
            .connection
            .write()
            .execute_batch(
                /* sqlite-large-value: allow-for-now lv-h4e49f4042edf owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM meta WHERE key='batch_rowid_high_water';
             INSERT OR REPLACE INTO meta(key,value) VALUES('seeded_batch_rowid','8');",
            )
            .unwrap();
    }
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();
    let observer = node("elm");
    let exported = store
        .export_replication_exchange(FLEET, &observer.replication_inventory().unwrap())
        .unwrap();
    assert_eq!(exported.envelopes.len(), 1);
    observer
        .receive_replication_exchange("cedar", FLEET, &exported)
        .unwrap();
    observer.validate_replication_backlog().unwrap();
    assert!(observer.claim_by_id(&claim.id).unwrap().is_some());
    let cursor = store.seeded_batch_rowid.load(Ordering::Acquire);
    drop(store);
    let reopened = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    assert_eq!(reopened.seeded_batch_rowid.load(Ordering::Acquire), cursor);
}

/// The daemon validates the backlog before it serves. A fresh process has no replication
/// snapshot, and building one, with the sealing recovery search, reads every envelope and batch:
/// fifteen seconds of restart on a two-million-envelope store. Admission needs neither.
#[test]
fn startup_admission_seals_new_batches_without_a_snapshot_or_recovery_search() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    let local = {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        let exchange = page(&node("birch"), &node("cedar-view"), 3);
        store
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "note/written-before-serving".into(),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    // The restarted daemon: pending envelopes and an unsealed local batch, nothing cached.
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();

    assert_eq!(store.validate_replication_backlog().unwrap().valid, 3);

    let sealed: i64 = store
        .readers
        .get()
        .query_row(
            "SELECT COUNT(*) FROM replica_envelopes
             WHERE batch_id=(SELECT batch_id FROM claims WHERE id=?1)",
            [&local.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sealed, 1, "admission sealed the local batch first");
    assert!(store.replication_snapshot.lock().unwrap().is_none());
    assert!(*store.sealing_recovery.try_lock().unwrap());

    // The first export after serving builds both.
    store.replication_snapshot().unwrap();
    assert!(store.replication_snapshot.lock().unwrap().is_some());
    assert!(!*store.sealing_recovery.try_lock().unwrap());
}

/// After a downgrade an older binary can leave a batch behind the sealing frontier with no
/// envelope. Startup admission seals only the batches after the frontier, so the first snapshot
/// after serving seals the skipped one, after a newer batch. The envelope takes its writer,
/// sequence and previous hash from the batch header written with the batch, so sealing later
/// changes none of them: the writer's chain stays whole and a peer admits both batches.
#[test]
fn a_batch_skipped_behind_the_frontier_is_sealed_by_the_first_snapshot_after_admission() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    let skipped = {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        let exchange = page(&node("birch"), &node("cedar-view"), 3);
        store
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap();
        let skipped = recovery_note(&store, "note/written-by-an-older-binary");
        // The older binary advanced the durable sealing cursor past a batch it never sealed.
        store
            .connection
            .write()
            .execute_batch(
                /* sqlite-large-value: allow-for-now lv-h80dca9a82fd6 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM meta WHERE key='batch_rowid_high_water';
             INSERT OR REPLACE INTO meta(key,value) VALUES('seeded_batch_rowid','8');",
            )
            .unwrap();
        skipped
    };
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();
    let newer = recovery_note(&store, "note/written-after-the-upgrade");
    let envelope = |claim: &crate::claim::ClaimRecord| -> Option<(String, i64)> {
        store
            .readers
            .get()
            .query_row(
                "SELECT writer, sequence FROM replica_envelopes
                 WHERE batch_id=(SELECT batch_id FROM claims WHERE id=?1)",
                [&claim.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap()
    };
    let peer_verdicts = || -> Vec<(String, String)> {
        let connection = store.readers.get();
        let mut statement = connection
            .prepare(
                "SELECT claims.id, claim_verdicts.verdict FROM claims
                 LEFT JOIN claim_verdicts ON claim_verdicts.claim_id=claims.id
                 WHERE claims.subject LIKE 'note/catch-up/%' ORDER BY claims.id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default()))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };

    assert_eq!(store.validate_replication_backlog().unwrap().valid, 3);
    assert!(envelope(&newer).is_some(), "admission sealed the batch after the frontier");
    assert!(envelope(&skipped).is_none(), "the skipped batch waits for the first snapshot");
    let admitted = peer_verdicts();
    assert_eq!(admitted.len(), 3);

    store.replication_snapshot().unwrap();
    let (writer, skipped_sequence) = envelope(&skipped).unwrap();
    let (newer_writer, newer_sequence) = envelope(&newer).unwrap();
    assert_eq!(writer, newer_writer);
    assert!(skipped_sequence < newer_sequence, "sequences follow the batches, not the sealing");
    let chain: Vec<(i64, String, Option<String>)> = {
        let connection = store.readers.get();
        let mut statement = connection
            .prepare(
                "SELECT replica_envelopes.sequence, batches.hash, replica_envelopes.previous_hash
                 FROM replica_envelopes JOIN batches ON batches.id=replica_envelopes.batch_id
                 WHERE replica_envelopes.writer=?1 ORDER BY replica_envelopes.sequence",
            )
            .unwrap();
        statement
            .query_map([&writer], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let first = chain[0].0;
    for (offset, (sequence, _, previous)) in chain.iter().enumerate() {
        assert_eq!(*sequence, first + offset as i64, "the writer's sequences have no gap");
        if offset > 0 {
            assert_eq!(previous.as_ref(), Some(&chain[offset - 1].1), "each links the batch before");
        }
    }
    assert_eq!(peer_verdicts(), admitted, "the snapshot's verdict pass judged peers the same");

    // A peer that has none of it receives one valid chain and admits both batches.
    let observer = node("elm");
    let exported = store
        .export_replication_exchange(FLEET, &observer.replication_inventory().unwrap())
        .unwrap();
    observer
        .receive_replication_exchange("cedar", FLEET, &exported)
        .unwrap();
    observer.validate_replication_backlog().unwrap();
    assert!(observer.claim_by_id(&skipped.id).unwrap().is_some());
    assert!(observer.claim_by_id(&newer.id).unwrap().is_some());
}

fn recovery_note(store: &Store, subject: &str) -> crate::claim::ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

#[test]
fn sealing_recovery_ignores_trimmed_sources_and_preserves_a_complete_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        let covered = recovery_note(&store, "note/covered");
        let header_only = recovery_note(&store, "note/header-only");
        store.seal_local_batches().unwrap();
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute(/* sqlite-large-value: allow-for-now lv-h244bd143de35 owner=smallclaims::store::receive_writer_tests TODO=stage-generation-and-keyed-reclamation */ "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
            actor,predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
            VALUES (?1,'cedar',1,'fixture-envelope',?2,'example.note',NULL,'[]',NULL,NULL,0,'checkpoint/fixture')",
            params![covered.id, covered.subject]).unwrap();
        tx.execute(/* sqlite-large-value: allow-for-now lv-ha2c5e9d13ede owner=smallclaims::store::receive_writer_tests TODO=chunk-or-reference-exact-wire-values */ "DELETE FROM replica_envelopes", []).unwrap();
        tx.execute(/* sqlite-large-value: allow-for-now lv-ha806231badab owner=smallclaims::store::receive_writer_tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE id=?1", [&header_only.id])
            .unwrap();
        tx.execute(/* sqlite-large-value: allow-for-now lv-he5d7cc0aa7aa owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM meta WHERE key='batch_rowid_high_water'", [])
            .unwrap();
        tx.commit().unwrap();
    }
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 2);
    store.recover_sealing_frontier().unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 2);
    store.seal_local_batches().unwrap();
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='batch_rowid_high_water'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    drop(store);
    // A pre-cursor store should likewise discover that no retained claim needs recovery.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(/* sqlite-large-value: allow-for-now lv-heb01acaa45bb owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM meta WHERE key='seeded_batch_rowid'", [])
        .unwrap();
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 2);
    store.recover_sealing_frontier().unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 2);
}

#[test]
fn credential_loading_leaves_many_skipped_batches_for_resumable_history_activation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    let key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        for i in 0..256 {
            recovery_note(&store, &format!("note/skipped-{i}"));
        }
        store
            .connection
            .write()
            .execute_batch(
                /* sqlite-large-value: allow-for-now lv-h4bea37063e9f owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM meta WHERE key='batch_rowid_high_water';
            INSERT OR REPLACE INTO meta(key,value) VALUES('seeded_batch_rowid','400');",
            )
            .unwrap();
    }
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.load_member_key(Some(key.clone())).unwrap();
        // The pre-bind credential path neither scans history nor signs source envelopes.
        assert!(*store.sealing_recovery.blocking_lock());
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 400);
        assert_eq!(
            store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        store.recover_sealing_frontier().unwrap();
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 0);
        assert!(store.seal_local_batch_chunk(256).unwrap());
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 64);
    }
    let resumed = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    assert_eq!(resumed.seeded_batch_rowid.load(Ordering::Acquire), 64);
    resumed.load_member_key(Some(key.clone())).unwrap();
    resumed.recover_sealing_frontier().unwrap();
    assert_eq!(resumed.seeded_batch_rowid.load(Ordering::Acquire), 64);
    resumed.complete_replication_activation().unwrap();
    assert_eq!(resumed.seeded_batch_rowid.load(Ordering::Acquire), 256);
    assert_eq!(
        resumed
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        256
    );
    assert_eq!(
        resumed
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM replica_envelope_signatures WHERE member_key=?1",
                [key.public()],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        256
    );
}

#[test]
fn upgrading_again_recovers_a_batch_skipped_by_a_downgraded_writer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cedar.sqlite3");
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        let first = recovery_note(&store, "note/before-downgrade");
        store.seal_local_batches().unwrap();
        let connection = store.connection.write();
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-ha77ce21369d5 owner=smallclaims::store::receive_writer_tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE id=?1", [&first.id])
            .unwrap();
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-h87fc9d88824b owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM batches WHERE id=?1", [&first.batch_id])
            .unwrap();
    }
    {
        let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
        store.recover_sealing_frontier().unwrap();
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 1);
        let later = recovery_note(&store, "note/during-downgrade");
        // Emulate an older writer's default SQLite allocation and unchanged high-water meta.
        let connection = store.connection.write();
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-hf43488f16b99 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE batches SET rowid=1 WHERE id=?1", [&later.batch_id])
            .unwrap();
        connection
            .execute(
                /* sqlite-large-value: allow-for-now lv-h6586257ba6b1 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE meta SET value='1' WHERE key='batch_rowid_high_water'",
                [],
            )
            .unwrap();
    }
    let store = Store::open(&path, "cedar", Arc::new(Plain)).unwrap();
    store.recover_sealing_frontier().unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 0);
    store.seal_local_batches().unwrap();
    assert_eq!(store.readers.get().query_row("SELECT COUNT(*) FROM replica_envelopes e JOIN batches b ON b.id=e.batch_id WHERE b.rowid=1", [], |r| r.get::<_,u64>(0)).unwrap(), 1);
}

#[test]
fn signing_an_unsigned_backlog_releases_the_writer_between_pages() {
    let store = node("cedar");
    for i in 0..256 {
        recovery_note(&store, &format!("note/unsigned-{i}"));
    }
    store.seal_local_batches().unwrap();
    let key = Arc::new(MemberKey::generate().unwrap().0);
    store.load_member_key(Some(key.clone())).unwrap();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let gate = Mutex::new(Some((entered, resumed)));
    {
        let connection = store.connection.write();
        connection
            .create_scalar_function("signature_gate", 0, FunctionFlags::SQLITE_UTF8, move |_| {
                if let Some((entered, resumed)) = gate.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    resumed.recv().unwrap();
                }
                Ok(0)
            })
            .unwrap();
        connection
            .execute_batch(
                /* sqlite-large-value: allow-for-now lv-hcc7d9c00ac1a owner=smallclaims::store::receive_writer_tests TODO=chunk-or-reference-exact-wire-values */ "CREATE TEMP TRIGGER signature_gate BEFORE INSERT ON replica_envelope_signatures
            BEGIN SELECT signature_gate(); END;",
            )
            .unwrap();
    }
    let signing_store = store.clone();
    let signing =
        std::thread::spawn(move || signing_store.complete_replication_activation().unwrap());
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let (done, ack) = mpsc::sync_channel(1);
    let (observed, prefix) = mpsc::sync_channel(1);
    store.connection.send(WriterJob::Batched {
        enqueued: Instant::now(),
        profile: None,
        wait: None,
        holder: crate::windows::holds::Holder::current(),
        done,
        run: Box::new(move |tx| {
            observed
                .send(
                    tx.query_row(
                        "SELECT COUNT(*) FROM replica_envelope_signatures",
                        [],
                        |r| r.get::<_, usize>(0),
                    )
                    .unwrap(),
                )
                .unwrap();
            tx.execute(
                /* sqlite-large-value: allow-for-now lv-h1795cee573e7 owner=smallclaims::store::receive_writer_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO meta(key,value) VALUES('signing-fairness','committed')",
                [],
            )
            .unwrap();
            true
        }),
    });
    resume.send(()).unwrap();
    let result = ack.recv_timeout(Duration::from_secs(5));
    let signed = signing.join().unwrap();
    result.unwrap().unwrap();
    assert_eq!(prefix.recv().unwrap(), 64);
    assert_eq!(signed, 256);
    assert_eq!(store.sign_retained_own_envelopes().unwrap(), 0);
}

#[test]
fn a_read_cancelled_while_waiting_for_the_recovery_search_releases_its_worker() {
    let store = node("cedar");
    let held = store.sealing_recovery.blocking_lock();
    let budget = crate::read_budget::ReadBudget::new("/recovery-wait", Duration::from_secs(30));
    let worker_budget = budget.clone();
    let reading = store.clone();
    let (started, ready) = mpsc::sync_channel(1);
    let (done, result) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        crate::read_budget::with(Some(worker_budget), || {
            started.send(()).unwrap();
            done.send(
                reading
                    .recover_sealing_frontier()
                    .map_err(|e| e.to_string()),
            )
            .unwrap();
        });
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    budget.cancel();
    let cancelled = result.recv_timeout(Duration::from_secs(1));
    drop(held);
    worker.join().unwrap();
    assert!(cancelled.unwrap().unwrap_err().contains("deadline"));
    assert!(*store.sealing_recovery.blocking_lock());
}
