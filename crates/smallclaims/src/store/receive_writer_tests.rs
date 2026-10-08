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
    // The prepared fold treated birch as legacy. Establish a signed anchor admission for
    // birch before its loan: the old unsigned page must now wait for that exact member key.
    let (key, _) = MemberKey::generate().unwrap();
    let key = Arc::new(key);
    target.set_member_key(Some(key.clone())).unwrap();
    target.pin_fleet_anchor(key.public()).unwrap();
    target
        .append_claim(&ClaimInput {
            subject: "host/birch".into(),
            kind: "fleet.member-admitted".into(),
            actor: None,
            fields: BTreeMap::from([
                ("fleet_id".into(), Value::String(FLEET.into())),
                ("member_key".into(), Value::String(key.public().into())),
                ("via".into(), Value::String("anchor".into())),
                ("mode".into(), Value::String("listening".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let membership = target.fleet_membership().unwrap();
    assert!(matches!(
        membership.window("birch", exchange.envelopes[0].sequence),
        crate::fleet::Window::Keyed(_)
    ));
    resume.send(()).unwrap();
    let outcome = admission.join().unwrap();
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
            "INSERT INTO checkpoint_envelopes VALUES(?1,?2,?3,?4,'checkpoint/test')",
            params![
                envelope.writer,
                envelope.sequence,
                envelope.hash,
                envelope.accepted_at_unix_ms as i64
            ],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM replica_envelopes WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
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
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
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
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
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
