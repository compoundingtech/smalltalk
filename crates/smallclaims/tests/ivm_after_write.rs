use anyhow::Result;
use serde_json::json;
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    ivm::{
        self, Contribution, Definition, View, Views,
        after_write::{self as after, After, Gap, Target, WaitOptions, WaitOutcome, WriteOutcome},
        events::{self, Publisher},
        runtime::ViewRuntime,
    },
    replication::ReplicationInventory,
    store::canonical,
};
use std::sync::Arc;
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

struct Status;
impl View for Status {
    fn definition(&self) -> Definition {
        Definition {
            name: "status",
            fingerprint: "status.v1",
            kinds: &["agent.status"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        rank: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: "status".into(),
            value: claim.body["fields"]["status"].clone(),
            rank: canonical::sortable_key(rank),
        }])
    }
}
fn runtime() -> Arc<ViewRuntime> {
    Arc::new(ViewRuntime::new(Views::new(vec![Box::new(Status)]).unwrap()).unwrap())
}
fn initialize(store: &Store) {
    store
        .connection
        .batched(|tx| events::install(tx, 64))
        .unwrap()
        .unwrap();
}
fn fixture(origin: &str) -> (Store, Arc<ViewRuntime>) {
    let rt = runtime();
    let store = Store::open_memory(origin, rt.clone()).unwrap();
    initialize(&store);
    (store, rt)
}
fn input(subject: &str, status: &str) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "agent.status".into(),
        actor: None,
        fields: serde_json::from_value(json!({"status":status})).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}
fn written(store: &Store, subject: &str, status: &str) -> (ClaimRecord, after::WriteReceipt) {
    match after::write(store, &input(subject, status)).unwrap() {
        WriteOutcome::Committed { claim, receipt } => (claim, receipt),
        other => panic!("{other:?}"),
    }
}
fn inspect(store: &Store, rt: &ViewRuntime, target: Target<'_>) -> After {
    store
        .read_snapshot(|_| after::inspect(&store.readers.get(), &rt.views, "status", target))
        .unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn cancelled() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}
fn export(store: &Store) -> smallclaims::replication::ReplicationExchange {
    store
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap()
}

#[test]
fn same_machine_receipt_verifies_each_write_and_survives_later_writes() {
    let (store, rt) = fixture("alder");
    let (claim, receipt) = written(&store, "agent/a", "working");
    assert_eq!(receipt.claim_id, claim.id);
    assert!(
        matches!(inspect(&store,&rt,Target::Local(&receipt)),After::Ready {mapped_index,..} if mapped_index==claim.store_index)
    );
    let (_, next) = written(&store, "agent/a", "working");
    assert_ne!(next.claim_id, receipt.claim_id);
    assert_eq!(after::committed_receipt(&store, &claim).unwrap(), receipt);
    let json = serde_json::to_string(&receipt).unwrap();
    assert_eq!(
        serde_json::from_str::<after::WriteReceipt>(&json).unwrap(),
        receipt
    );
}

#[test]
fn future_or_rolled_back_local_claims_never_become_ready() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    let mut unknown = receipt.clone();
    unknown.claim_id = "claim/not-committed".into();
    unknown.store_index += 1;
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&unknown)),
        After::Resync {
            reason: Gap::UnknownLocalWrite,
            ..
        }
    ));
    let claim = {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        use smallclaims::Runtime;
        let claim = rt
            .append_claim_tx(
                &tx,
                &store.origin,
                "agent/rollback",
                "agent.status",
                None,
                &json!({"fields":{"status":"bad"}}),
                &[],
                None,
            )
            .unwrap();
        tx.rollback().unwrap();
        claim
    };
    assert!(after::committed_receipt(&store, &claim).is_err());
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Ready { .. }
    ));
}

#[test]
fn unavailable_feed_does_not_hide_a_committed_write() {
    let rt = runtime();
    let store = Store::open_memory("alder", rt).unwrap();
    match after::write(&store, &input("agent/a", "working")).unwrap() {
        WriteOutcome::CommittedWithoutReceipt { claim, .. } => {
            assert_eq!(
                store
                    .readers
                    .get()
                    .query_row("SELECT id FROM claims WHERE id=?1", [&claim.id], |r| r
                        .get::<_, String>(
                        0
                    ))
                    .unwrap(),
                claim.id
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn database_rotation_and_local_position_changes_are_gaps() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(events::rotate_identity)
        .unwrap()
        .unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Resync {
            reason: Gap::DatabaseReplaced,
            ..
        }
    ));
    let claim = store
        .readers
        .get()
        .query_row(
            "SELECT store_index FROM claims WHERE id=?1",
            [&receipt.claim_id],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    let current = after::WriteReceipt {
        database_id: events::capture(&store.readers.get(), &rt.views, "status")
            .unwrap()
            .identity
            .database_id,
        ..receipt
    };
    let wrong = after::WriteReceipt {
        store_index: claim + 1,
        ..current
    };
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&wrong)),
        After::Resync {
            reason: Gap::ClaimPositionChanged,
            ..
        }
    ));
}

#[test]
fn ready_max_applied_index_does_not_certify_a_pending_source() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    let (source, _) = fixture("birch");
    written(&source, "agent/unrelated", "idle");
    store
        .receive_replication_exchange("birch", "sample-fleet", &export(&source))
        .unwrap();
    store.validate_replication_backlog().unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Pending {
            mapped_index: Some(_),
            ..
        }
    ));
    store.project_replication_backlog().unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Ready { .. }
    ));
}

#[test]
fn same_index_fenced_view_is_not_admitted_by_an_old_receipt() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=0 WHERE name='status'", []))
        .unwrap()
        .unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Pending { .. }
    ));
}

#[test]
fn disk_reopen_preserves_receipts_without_replaying_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.db");
    let rt = runtime();
    let store = Store::open(&path, "alder", rt.clone()).unwrap();
    initialize(&store);
    let (_, receipt) = written(&store, "agent/a", "working");
    drop(store);
    let store = Store::open(&path, "alder", rt.clone()).unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Ready { .. }
    ));
}

#[tokio::test]
async fn replication_receipt_waits_for_admission_and_projection_in_the_receiving_log() {
    let (source, _) = fixture("alder");
    written(&source, "agent/a", "idle");
    let (_, receipt) = written(&source, "agent/a", "working");
    let exchange = export(&source);
    let (target, rt) = fixture("birch");
    written(&target, "agent/unrelated", "idle");
    let publisher = Publisher::attach(&target, 8).unwrap();
    let (_cancel, receiver) = cancelled();
    assert!(matches!(
        inspect(&target, &rt, Target::Replicated(&receipt)),
        After::Pending {
            mapped_index: None,
            ..
        }
    ));
    let wait = after::wait(
        &target,
        &rt.views,
        "status",
        Target::Replicated(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation: receiver,
        },
        |connection, boundary| {
            Ok(rt
                .views
                .head(
                    connection,
                    "status",
                    "agent/a",
                    "status",
                    boundary.source_cut,
                )?
                .unwrap()
                .value)
        },
    );
    let deliver = async {
        tokio::task::yield_now().await;
        // Reverse envelopes and send a duplicate through the real admission lifecycle.
        let mut reversed = exchange.clone();
        reversed.envelopes.reverse();
        target
            .receive_replication_exchange("alder", "sample-fleet", &reversed)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        assert!(matches!(
            inspect(&target, &rt, Target::Replicated(&receipt)),
            After::Pending {
                mapped_index: Some(_),
                ..
            }
        ));
        target.project_replication_backlog().unwrap();
        target
            .receive_replication_exchange("alder", "sample-fleet", &reversed)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
    };
    let (result, _) = tokio::join!(wait, deliver);
    assert!(
        matches!(result.unwrap(),WaitOutcome::Ready {value,mapped_index,..} if value==json!("working") && mapped_index!=receipt.store_index)
    );
}

#[tokio::test]
async fn subscribe_before_capture_covers_commit_before_and_after_snapshot() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    let publisher = Publisher::attach(&store, 8).unwrap();
    let receiver = publisher.subscribe();
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=0 WHERE name='status'", []))
        .unwrap()
        .unwrap();
    let (_cancel, cancellation) = cancelled();
    let wait = after::wait_subscribed(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        receiver,
        WaitOptions {
            deadline: deadline(),
            cancellation,
        },
        |connection, boundary| {
            Ok(rt
                .views
                .head(
                    connection,
                    "status",
                    "agent/a",
                    "status",
                    boundary.source_cut,
                )?
                .unwrap()
                .value)
        },
    );
    let resume = async {
        tokio::task::yield_now().await;
        store
            .connection
            .batched(|tx| tx.execute("UPDATE ivm_views SET ready=1 WHERE name='status'", []))
            .unwrap()
            .unwrap();
    };
    let (result, _) = tokio::join!(wait, resume);
    assert!(matches!(result.unwrap(),WaitOutcome::Ready {value,..} if value==json!("working")));
}

#[tokio::test]
async fn cancellation_deadline_and_shutdown_are_explicit_without_running_the_predicate() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=0 WHERE name='status'", []))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (cancel, receiver) = cancelled();
    cancel.send(true).unwrap();
    let result = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation: receiver,
        },
        |_, _| -> Result<()> { panic!("unavailable predicate") },
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::Cancelled));
    let (_cancel, receiver) = cancelled();
    let result = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: Instant::now(),
            cancellation: receiver,
        },
        |_, _| -> Result<()> { panic!("unavailable predicate") },
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::TimedOut));
    let notices = publisher.subscribe();
    drop(publisher);
    let (_cancel, receiver) = cancelled();
    let result = after::wait_subscribed(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        notices,
        WaitOptions {
            deadline: deadline(),
            cancellation: receiver,
        },
        |_, _| -> Result<()> { panic!("unavailable predicate") },
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::PublisherClosed));
}

#[tokio::test]
async fn rotation_while_waiting_requires_resync_instead_of_foreign_remapping() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=0 WHERE name='status'", []))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (_cancel, receiver) = cancelled();
    let wait = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation: receiver,
        },
        |_, _| -> Result<()> { panic!("unavailable predicate") },
    );
    let rotate = async {
        tokio::task::yield_now().await;
        store
            .connection
            .batched(events::rotate_identity)
            .unwrap()
            .unwrap();
    };
    let (result, _) = tokio::join!(wait, rotate);
    assert!(matches!(result.unwrap(), WaitOutcome::Resync { .. }));
}

#[test]
fn epoch_change_is_a_gap_and_malformed_tokens_are_rejected() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    let mut bad = receipt.clone();
    bad.store_index = 0;
    assert!(
        store
            .read_snapshot(|_| after::inspect(
                &store.readers.get(),
                &rt.views,
                "status",
                Target::Local(&bad)
            ))
            .is_err()
    );
    // Metadata invalidation control only: not a certified restore/epoch transition.
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_source SET epoch=2 WHERE singleton=1", []))
        .unwrap()
        .unwrap();
    assert!(matches!(
        inspect(&store, &rt, Target::Local(&receipt)),
        After::Resync {
            reason: Gap::EpochChanged,
            ..
        }
    ));
}

#[tokio::test]
async fn missing_view_does_not_call_output_and_times_out_without_rebuild() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(|tx| tx.execute("DELETE FROM ivm_views WHERE name='status'", []))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (_cancel, cancellation) = cancelled();
    let result = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: Instant::now() + Duration::from_millis(5),
            cancellation,
        },
        |_, _| -> Result<()> { panic!("missing view") },
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::TimedOut));
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM ivm_views", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn cancellation_after_subscribing_stops_a_pending_wait() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=0 WHERE name='status'", []))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (cancel, cancellation) = cancelled();
    let wait = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation,
        },
        |_, _| -> Result<()> { panic!("fenced view") },
    );
    let stop = async {
        tokio::task::yield_now().await;
        cancel.send(true).unwrap();
    };
    let (result, _) = tokio::join!(wait, stop);
    assert!(matches!(result.unwrap(), WaitOutcome::Cancelled));
}

#[tokio::test]
async fn ready_output_still_runs_the_authoritative_predicate_and_propagates_denial() {
    let (store, rt) = fixture("alder");
    let (_, receipt) = written(&store, "agent/a", "working");
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (_cancel, cancellation) = cancelled();
    let result = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation,
        },
        |connection, boundary| -> Result<()> {
            assert!(matches!(
                boundary.availability.readiness,
                ivm::Readiness::Ready(_)
            ));
            assert_eq!(
                connection.query_row(
                    "SELECT id FROM claims WHERE id=?1",
                    [&receipt.claim_id],
                    |r| r.get::<_, String>(0)
                )?,
                receipt.claim_id
            );
            anyhow::bail!("current authority denies this caller")
        },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("authority denies"));
}

#[tokio::test]
async fn unknown_kind_receipt_is_processed_without_a_semantic_view_change() {
    let (store, rt) = fixture("alder");
    let before = rt.views.token(&store.readers.get(), "status", 1).unwrap();
    let mut unknown = input("agent/a", "unknown");
    unknown.kind = "future.status".into();
    let receipt = match after::write(&store, &unknown).unwrap() {
        WriteOutcome::Committed { receipt, .. } => receipt,
        other => panic!("{other:?}"),
    };
    let publisher = Publisher::attach(&store, 8).unwrap();
    let (_cancel, cancellation) = cancelled();
    let result = after::wait(
        &store,
        &rt.views,
        "status",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: deadline(),
            cancellation,
        },
        |connection, boundary| {
            rt.views.head(
                connection,
                "status",
                "agent/a",
                "status",
                boundary.source_cut,
            )
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::Ready { value: None, .. }));
    assert_eq!(
        rt.views.token(&store.readers.get(), "status", 1).unwrap(),
        before
    );
}
