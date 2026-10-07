use anyhow::{Context, Result};
use serde_json::json;
use smallclaims::{
    ClaimRecord, Runtime, Store,
    ivm::{
        self, Contribution, Definition, LocalChange, RepairPolicy, SourceCut, View, Views,
        events::{self, Boundary, Gap, Notice, Page, Publisher},
        runtime::ViewRuntime,
    },
    store::{append_claim_record_tx, canonical, runtime::Plain},
};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::broadcast::error::TryRecvError;

struct ValueView {
    database_error: Option<i32>,
    replacement: bool,
}
impl View for ValueView {
    fn definition(&self) -> Definition {
        Definition {
            name: "values",
            fingerprint: "values.v1",
            kinds: &["event.value"],
            local_kinds: &["event.local"],
            max_contributions: 1,
        }
    }
    fn repair_policy(&self) -> RepairPolicy {
        if self.replacement {
            RepairPolicy::ReplaceOriginal
        } else {
            RepairPolicy::RetainOriginal
        }
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        rank: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        if let Some(code) = self.database_error {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
            .context("contribution storage failure");
        }
        if claim.body["fields"]["value"] == json!(-99) {
            anyhow::bail!("operator cannot maintain this admitted input");
        }
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: "value".into(),
            value: claim.body["fields"]["value"].clone(),
            rank: canonical::sortable_key(rank),
        }])
    }
    fn maintain_local_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        _key: &str,
        _change: &LocalChange,
    ) -> Result<bool> {
        tx.execute("UPDATE local_values SET value=1", [])?;
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            None,
        ))
        .context("local storage failure")
    }
}
fn views() -> Arc<Views> {
    Arc::new(
        Views::new(vec![Box::new(ValueView {
            database_error: None,
            replacement: false,
        })])
        .unwrap(),
    )
}
fn cut(index: u64) -> SourceCut {
    SourceCut {
        epoch: 1,
        admitted: index,
        projected: index,
        local_generation: 0,
    }
}
fn initialize(store: &Store, views: &Views, capacity: usize) {
    let mut w = store.connection.write();
    views.create_schema(&w).unwrap();
    let tx = w.transaction().unwrap();
    views.initialize_empty(&tx, cut(0)).unwrap();
    events::install(&tx, capacity).unwrap();
    tx.commit().unwrap();
}
fn fixture(capacity: usize) -> (Store, Arc<Views>) {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views = views();
    initialize(&store, &views, capacity);
    (store, views)
}
fn append_tx(
    tx: &rusqlite::Transaction<'_>,
    store: &Store,
    views: &Views,
    subject: &str,
    value: i64,
) -> Result<ClaimRecord> {
    let claim = append_claim_record_tx(
        tx,
        &store.origin,
        subject,
        "event.value",
        None,
        &json!({"fields":{"value":value}}),
        &[],
        None,
    )?;
    let rank = canonical::claim_key(tx, &claim.id)?;
    views.change(tx, None, Some((&claim, &rank)), 1)?;
    views.publish_cut(tx, cut(claim.store_index))?;
    Ok(claim)
}
fn append(store: &Store, views: &Views, subject: &str, value: i64) -> ClaimRecord {
    store
        .connection
        .batched(|tx| append_tx(tx, store, views, subject, value))
        .unwrap()
        .unwrap()
}
fn capture(store: &Store, views: &Views) -> Boundary {
    store
        .read_snapshot(|_| events::capture(&store.readers.get(), views, "values"))
        .unwrap()
}

#[test]
fn events_are_invisible_until_commit_and_include_the_final_source_cut() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("claims.db"),
        "alder",
        Arc::new(Plain),
    )
    .unwrap();
    let views = views();
    initialize(&store, &views, 32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let baseline = capture(&store, &views);
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        append_tx(&tx, &store, &views, "key/a", 1).unwrap();
        append_tx(&tx, &store, &views, "key/b", 2).unwrap();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(capture(&store, &views).keys, baseline.keys);
        tx.commit().unwrap();
        assert_eq!(
            receiver.try_recv(),
            Err(TryRecvError::Empty),
            "lent callback runs on guard return"
        );
    }
    let Notice::Committed(notice) = receiver.try_recv().unwrap() else {
        panic!("expected committed notice")
    };
    assert_eq!(notice.source_cut.projected, 2);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    store
        .read_snapshot(|_| {
            let Page::Changes {
                items,
                boundary,
                more,
                ..
            } = events::keys(&store.readers.get(), &views, &baseline.keys, 10, None)?
            else {
                panic!("unexpected gap")
            };
            assert_eq!(items.len(), 2);
            assert!(!more);
            assert!(
                items
                    .iter()
                    .all(|item| item.source_cut == notice.source_cut)
            );
            assert_eq!(boundary.source_cut, notice.source_cut);
            Ok(())
        })
        .unwrap();
}

#[test]
fn rollback_noop_and_failed_outer_commit_publish_nothing() {
    let (store, views) = fixture(32);
    {
        let w = store.connection.write();
        w.execute_batch("CREATE TABLE event_parents(id INTEGER PRIMARY KEY); CREATE TABLE event_children(parent INTEGER REFERENCES event_parents(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
    }
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let baseline = capture(&store, &views);
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        append_tx(&tx, &store, &views, "key/a", 1).unwrap();
        tx.rollback().unwrap();
    }
    store
        .connection
        .batched(|_tx| Ok::<_, anyhow::Error>(()))
        .unwrap()
        .unwrap();
    assert!(
        store
            .connection
            .batched(|tx| {
                append_tx(tx, &store, &views, "key/a", 2)?;
                tx.execute("INSERT INTO event_children VALUES(99)", [])?;
                Ok::<_, anyhow::Error>(())
            })
            .is_err()
    );
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(capture(&store, &views), baseline);
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM ivm_event_keys", [], |r| r
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[test]
fn failed_batched_savepoint_does_not_export_its_keys() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let baseline = capture(&store, &views);
    let result = store.connection.batched(|tx| -> Result<()> {
        append_tx(tx, &store, &views, "key/a", 1)?;
        anyhow::bail!("request rejected after its staged view changes")
    });
    assert!(result.unwrap().is_err());
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(capture(&store, &views), baseline);
}

#[test]
fn reconnect_identity_survives_updates_while_page_snapshots_change() {
    let (store, views) = fixture(32);
    let before = capture(&store, &views);
    append(&store, &views, "key/a", 1);
    let after = capture(&store, &views);
    assert_eq!(before.identity, after.identity);
    assert_ne!(before.snapshot, after.snapshot);
    store
        .read_snapshot(|_| {
            let r = store.readers.get();
            assert!(matches!(
                events::keys(&r, &views, &before.keys, 1, Some(&before.snapshot))?,
                Page::SnapshotChanged { .. }
            ));
            assert!(matches!(
                events::keys(&r, &views, &before.keys, 1, None)?,
                Page::Changes { .. }
            ));
            Ok(())
        })
        .unwrap();
}

#[test]
fn removal_and_readd_both_have_committed_invalidation_positions() {
    let (store, views) = fixture(32);
    let initial = capture(&store, &views);
    let claim = append(&store, &views, "key/a", 1);
    store
        .connection
        .batched(|tx| {
            views.change(tx, Some(&claim), None, 1)?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    assert!(
        views
            .head(
                &store.readers.get(),
                "values",
                "key/a",
                "value",
                cut(claim.store_index)
            )
            .unwrap()
            .is_none()
    );
    store
        .connection
        .batched(|tx| {
            let rank = canonical::claim_key(tx, &claim.id)?;
            views.change(tx, None, Some((&claim, &rank)), 1)?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    store
        .read_snapshot(|_| {
            let Page::Changes { items, .. } =
                events::keys(&store.readers.get(), &views, &initial.keys, 10, None)?
            else {
                panic!("unexpected gap")
            };
            assert_eq!(items.len(), 3);
            assert!(items.iter().all(|item| item.key == "key/a"));
            assert!(
                items
                    .windows(2)
                    .all(|items| items[0].sequence < items[1].sequence)
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn retention_is_bounded_and_expired_cursors_return_an_explicit_gap() {
    let (store, views) = fixture(2);
    let initial = capture(&store, &views);
    for i in 0..6 {
        append(&store, &views, &format!("key/{i}"), i);
    }
    let boundary = capture(&store, &views);
    assert_eq!(boundary.key_floor, 4);
    let r = store.readers.get();
    for table in ["ivm_event_keys", "ivm_event_status"] {
        let rows: usize = r
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert!(rows <= 2);
    }
    assert!(matches!(
        events::keys(&r, &views, &initial.keys, 1, None).unwrap(),
        Page::Resync {
            reason: Gap::Expired { floor: 4 },
            ..
        }
    ));
    let mut cursor = initial.keys.clone();
    cursor.sequence = boundary.key_floor;
    let Page::Changes {
        items,
        next,
        more,
        boundary: first,
    } = events::keys(&r, &views, &cursor, 1, None).unwrap()
    else {
        panic!("unexpected gap")
    };
    assert_eq!(items.len(), 1);
    assert!(more);
    let Page::Changes { items, more, .. } =
        events::keys(&r, &views, &next, 1, Some(&first.snapshot)).unwrap()
    else {
        panic!("unexpected continuation gap")
    };
    assert_eq!(items.len(), 1);
    assert!(!more);
}

#[test]
fn availability_remains_deliverable_while_output_is_fenced() {
    let (store, views) = fixture(32);
    let initial = capture(&store, &views);
    store
        .connection
        .batched(|tx| {
            tx.execute("UPDATE ivm_views SET ready=0 WHERE name='values'", [])?;
            tx.execute("INSERT INTO ivm_view_errors VALUES('values','first')", [])?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    let fenced = capture(&store, &views);
    assert_eq!(fenced.availability.readiness, ivm::Readiness::Fenced);
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_view_errors SET error='second'", []))
        .unwrap()
        .unwrap();
    let Page::Changes {
        items, boundary, ..
    } = events::availability(&store.readers.get(), &views, &fenced.status, 10, None).unwrap()
    else {
        panic!("availability cannot require a ready token")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].current.error.as_deref(), Some("second"));
    assert_eq!(boundary.keys, initial.keys);
    assert_eq!(
        boundary.snapshot.semantic_generation,
        initial.snapshot.semantic_generation
    );
}

#[test]
fn cursors_are_database_bound_and_explicit_restore_rotation_fences_them() {
    let (store, views) = fixture(32);
    let old = capture(&store, &views);
    let (other, other_views) = fixture(32);
    assert!(matches!(
        events::keys(&other.readers.get(), &other_views, &old.keys, 1, None).unwrap(),
        Page::Resync {
            reason: Gap::ProviderReplaced,
            ..
        }
    ));
    store
        .connection
        .batched(events::rotate_identity)
        .unwrap()
        .unwrap();
    assert!(matches!(
        events::keys(&store.readers.get(), &views, &old.keys, 1, None).unwrap(),
        Page::Resync {
            reason: Gap::ProviderReplaced,
            ..
        }
    ));
    let current = capture(&store, &views);
    assert!(matches!(
        events::keys(&store.readers.get(), &views, &current.status, 1, None).unwrap(),
        Page::Resync {
            reason: Gap::WrongStream,
            ..
        }
    ));
    let rejected = store
        .connection
        .batched(|tx| views.publish_cut(tx, SourceCut { epoch: 2, ..cut(0) }))
        .unwrap();
    assert!(rejected.is_err());
    assert_eq!(capture(&store, &views), current);
    // Test only the cursor's response to lifecycle metadata invalidation. This is not
    // an accepted restore/epoch transition, which remains the source owner's job.
    store
        .connection
        .batched(|tx| {
            tx.execute("UPDATE ivm_source SET epoch=2 WHERE singleton=1", [])?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    assert!(matches!(
        events::keys(&store.readers.get(), &views, &current.keys, 1, None).unwrap(),
        Page::Resync {
            reason: Gap::ProviderReplaced,
            ..
        }
    ));
}

#[test]
fn durable_event_rows_and_identity_survive_close_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.db");
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let views = views();
    initialize(&store, &views, 32);
    let initial = capture(&store, &views);
    append(&store, &views, "key/a", 1);
    let final_cut = capture(&store, &views);
    drop(store);
    let reopened = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(capture(&reopened, &views), final_cut);
    assert!(matches!(
        events::keys(&reopened.readers.get(), &views, &initial.keys, 10, None).unwrap(),
        Page::Changes { .. }
    ));
}

#[test]
fn oversized_event_keys_fence_the_feed_without_rejecting_source_admission() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let key = "x".repeat(4097);
    let claim = append(&store, &views, &key, 1);
    assert!(matches!(
        receiver.try_recv().unwrap(),
        Notice::Unavailable(_)
    ));
    assert!(events::capture(&store.readers.get(), &views, "values").is_err());
    assert!(
        views
            .head(
                &store.readers.get(),
                "values",
                &key,
                "value",
                cut(claim.store_index)
            )
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM ivm_event_keys", [], |r| r
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[test]
fn subscribe_before_capture_keeps_a_commit_after_the_snapshot_pending() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    append(&store, &views, "key/a", 1);
    let baseline = capture(&store, &views);
    append(&store, &views, "key/b", 2);
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_ok());
    let Page::Changes { items, .. } =
        events::keys(&store.readers.get(), &views, &baseline.keys, 10, None).unwrap()
    else {
        panic!("missed commit")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "key/b");
    drop(publisher);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn lagged_wakes_are_not_silently_presented_as_complete_delivery() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 1).unwrap();
    let mut receiver = publisher.subscribe();
    let baseline = capture(&store, &views);
    for i in 0..3 {
        append(&store, &views, &format!("key/{i}"), i);
    }
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Lagged(_))));
    let Page::Changes { items, .. } =
        events::keys(&store.readers.get(), &views, &baseline.keys, 10, None).unwrap()
    else {
        panic!("durable catch-up was lost")
    };
    assert_eq!(items.len(), 3);
}

#[test]
fn database_errors_propagate_without_permanently_fencing_a_view() {
    for code in [
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_LOCKED,
    ] {
        let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
        let views = Views::new(vec![Box::new(ValueView {
            database_error: Some(code),
            replacement: false,
        })])
        .unwrap();
        initialize(&store, &views, 32);
        let before = capture(&store, &views);
        let result = store
            .connection
            .batched(|tx| append_tx(tx, &store, &views, "key/a", 1))
            .unwrap();
        let error = result.unwrap_err();
        assert!(error.chain().any(|error| error.is::<rusqlite::Error>()));
        assert_eq!(capture(&store, &views), before);
        assert_eq!(
            store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get::<_, usize>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn local_database_failure_rolls_back_source_and_output_without_a_fence() {
    let (store, views) = fixture(32);
    store
        .connection
        .write()
        .execute_batch(
            "CREATE TABLE local_values(value INTEGER); INSERT INTO local_values VALUES(0)",
        )
        .unwrap();
    let before = capture(&store, &views);
    let change = LocalChange {
        kind: "event.local".into(),
        old_keys: BTreeSet::new(),
        new_keys: BTreeSet::from(["key/a".into()]),
        evaluation_time_unix_ms: 0,
    };
    let result = store
        .connection
        .batched(|tx| {
            views.local_change(
                tx,
                &change,
                SourceCut {
                    local_generation: 1,
                    ..cut(0)
                },
            )
        })
        .unwrap();
    assert!(result.is_err());
    assert_eq!(capture(&store, &views), before);
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT value FROM local_values", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn reference_runtime_gates_unproved_replacement_repair_eligibility() {
    let views = Views::new(vec![Box::new(ValueView {
        database_error: None,
        replacement: true,
    })])
    .unwrap();
    assert!(
        ViewRuntime::new(views)
            .err()
            .unwrap()
            .to_string()
            .contains("replacement-repair eligibility")
    );
}

#[test]
fn oversized_projection_fetch_has_constant_vm_work_on_larger_pending_logs() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let runtime = Arc::new(ViewRuntime::new(Views::new(vec![]).unwrap()).unwrap());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let mut w = store.connection.write();
    let tx = w.transaction().unwrap();
    let mut last = None;
    let mut inserted = 0;
    let mut counts = Vec::new();
    for total in [smallclaims::store::PROJECTION_CHUNK_CLAIMS + 2, 1300] {
        for i in inserted..total {
            last = Some(
                append_claim_record_tx(
                    &tx,
                    "alder",
                    &format!("key/{i}"),
                    "event.unknown",
                    None,
                    &json!({"fields":{}}),
                    &[],
                    None,
                )
                .unwrap(),
            );
        }
        inserted = total;
        let steps = Arc::new(AtomicU64::new(0));
        let recorded = steps.clone();
        tx.progress_handler(
            1,
            Some(move || {
                recorded.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let result = runtime.project_incremental(&tx, "alder", last.as_ref().unwrap().store_index);
        tx.progress_handler(0, None::<fn() -> bool>);
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("projection chunk exceeds admission bound"),
            "{error}"
        );
        counts.push(steps.load(Ordering::Relaxed));
        eprintln!(
            "pending_chunk_fetch_vm pending={total} vm={}",
            counts.last().unwrap()
        );
    }
    assert!(counts[0] > 0);
    assert!(
        counts[1] <= counts[0] + 16,
        "bounded fetch grew with the pending log: {counts:?}"
    );
    tx.rollback().unwrap();
}

#[test]
fn unknown_kinds_advance_source_evidence_without_semantic_key_events() {
    let (store, views) = fixture(32);
    let before = capture(&store, &views);
    store
        .connection
        .batched(|tx| {
            let claim = append_claim_record_tx(
                tx,
                "alder",
                "key/unrelated",
                "event.unknown",
                None,
                &json!({"fields":{}}),
                &[],
                None,
            )?;
            let changed = views.change(
                tx,
                None,
                Some((&claim, &canonical::claim_key(tx, &claim.id)?)),
                1,
            )?;
            assert!(changed.changed.is_empty());
            views.publish_cut(tx, cut(claim.store_index))
        })
        .unwrap()
        .unwrap();
    let after = capture(&store, &views);
    assert_eq!(before.identity, after.identity);
    assert_eq!(
        before.snapshot.semantic_generation,
        after.snapshot.semantic_generation
    );
    assert_eq!(before.keys, after.keys);
    assert!(after.status.sequence > before.status.sequence);
    let Page::Changes { items, .. } =
        events::keys(&store.readers.get(), &views, &before.keys, 10, None).unwrap()
    else {
        panic!("unexpected semantic gap")
    };
    assert!(items.is_empty());
}

#[test]
fn operator_failure_preserves_claim_and_publishes_only_unavailable_evidence() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let before = capture(&store, &views);
    let claim = append(&store, &views, "key/a", -99);
    let Notice::Committed(notice) = receiver.try_recv().unwrap() else {
        panic!("committed admission was lost")
    };
    assert_eq!(notice.source_cut.projected, claim.store_index);
    let after = capture(&store, &views);
    assert_eq!(after.availability.readiness, ivm::Readiness::Fenced);
    assert_eq!(after.keys, before.keys);
    assert_eq!(
        views
            .projection_state(&store.readers.get(), "values", 1)
            .unwrap()
            .deferred_claim_index,
        Some(claim.store_index)
    );
    let Page::Changes { items, .. } =
        events::availability(&store.readers.get(), &views, &before.status, 10, None).unwrap()
    else {
        panic!("availability cannot require ready output")
    };
    assert!(
        items
            .iter()
            .all(|item| item.current.readiness == ivm::Readiness::Fenced)
    );
}

#[test]
fn unfinished_lent_transaction_never_publishes_a_committed_frontier() {
    let (store, views) = fixture(32);
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut receiver = publisher.subscribe();
    let before = capture(&store, &views);
    {
        let w = store.connection.write();
        w.execute_batch("BEGIN; UPDATE ivm_views SET ready=0 WHERE name='values'")
            .unwrap();
    }
    let notice = receiver.try_recv().unwrap();
    // Restore the misused raw writer before any assertion can unwind the test. This is
    // an integration misuse control, not an admitted claim or a supported source mutation.
    {
        let w = store.connection.write();
        if !w.is_autocommit() {
            w.execute_batch("ROLLBACK").unwrap();
        }
    }
    assert!(matches!(notice, Notice::Unavailable(_)));
    assert_eq!(capture(&store, &views), before);
}
