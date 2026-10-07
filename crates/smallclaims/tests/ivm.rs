use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};
use smallclaims::{
    ClaimRecord, Store,
    ivm::{Contribution, Definition, SourceCut, View, Views},
    store::{append_claim_record_tx, canonical, runtime::Plain},
};
use std::sync::Arc;

struct Limits(&'static str);
impl View for Limits {
    fn definition(&self) -> Definition {
        Definition {
            name: "limits",
            fingerprint: self.0,
            kinds: &["account.limits"],
            local_kinds: &[],
            max_contributions: 2,
        }
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        key: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: "limits".into(),
            value: claim.body["fields"].clone(),
            rank: canonical::sortable_key(key),
        }])
    }
}
fn views() -> Views {
    Views::new(vec![Box::new(Limits(
        "limits.v1;registry.v1;authority.v1;keys.v1",
    ))])
    .unwrap()
}
fn cut(index: u64) -> SourceCut {
    SourceCut {
        epoch: 1,
        admitted: index,
        projected: index,
        local_generation: 0,
    }
}
fn fixture() -> (Store, Views) {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views = views();
    let mut writer = store.connection.write();
    views.create_schema(&writer).unwrap();
    let tx = writer.transaction().unwrap();
    views.initialize_empty(&tx, cut(0)).unwrap();
    tx.commit().unwrap();
    drop(writer);
    (store, views)
}
fn append(store: &Store, views: &Views, value: Value) -> ClaimRecord {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let claim = append_claim_record_tx(
        &tx,
        &store.origin,
        "account/ada",
        "account.limits",
        None,
        &json!({"fields":value}),
        &[],
        None,
    )
    .unwrap();
    let key = canonical::claim_key(&tx, &claim.id).unwrap();
    views.change(&tx, None, Some((&claim, &key)), 1).unwrap();
    views.publish_cut(&tx, cut(claim.store_index)).unwrap();
    tx.commit().unwrap();
    claim
}
fn value(store: &Store, views: &Views) -> Option<Value> {
    store
        .read_snapshot(|index| {
            Ok(views
                .head(
                    &store.readers.get(),
                    "limits",
                    "account/ada",
                    "limits",
                    cut(index),
                )?
                .map(|h| h.value))
        })
        .unwrap()
}

#[test]
fn keyed_selector_captured_canonical_facts_retraction_duplicate_and_rollback() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let b = append(&store, &views, json!({"remaining":8}));
    assert_eq!(value(&store, &views), Some(json!({"remaining":8})));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let ka = canonical::claim_key(&tx, &a.id).unwrap();
    let kb = canonical::claim_key(&tx, &b.id).unwrap();
    let token = views.token(&tx, "limits", 1).unwrap();
    let duplicate = views.change(&tx, None, Some((&b, &kb)), 1).unwrap();
    assert!(duplicate.changed.is_empty());
    assert_eq!(views.token(&tx, "limits", 1).unwrap(), token);
    let lower = views.change(&tx, None, Some((&a, &ka)), 1).unwrap();
    assert!(lower.changed.is_empty());
    let change = views.change(&tx, Some(&b), None, 1).unwrap();
    assert_eq!(change.changed.len(), 1);
    assert_eq!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(b.store_index))
            .unwrap()
            .unwrap()
            .claim_id,
        a.id
    );
    // A retraction has no new claim and no new store index; its generation must change.
    assert_eq!(
        views.token(&tx, "limits", 1).unwrap().generation,
        token.generation + 1
    );
    views.change(&tx, Some(&a), None, 1).unwrap();
    // Inspect the selector separately from source freshness. The separate query-plan test
    // checks its contribution index; no source-schema mutation is needed for retraction.
    let remaining: u64 = tx.query_row(
        "SELECT COUNT(*) FROM ivm_heads WHERE view='limits' AND key='account/ada' AND register='limits'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(remaining, 0);
    // Roll back the removals.
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(value(&store, &views), Some(json!({"remaining":8})));

    // Rebuild oracle on copied invented claims, through real store transactions.
    for order in [[&a, &b], [&b, &a]] {
        let (target, target_views) = fixture();
        let mut writer = target.connection.write();
        let tx = writer.transaction().unwrap();
        for claim in order {
            let key = if claim.id == a.id { &ka } else { &kb };
            target_views
                .change(&tx, None, Some((claim, key)), 1)
                .unwrap();
        }
        target_views.publish_cut(&tx, cut(0)).unwrap();
        let head = target_views
            .head(&tx, "limits", "account/ada", "limits", cut(0))
            .unwrap()
            .unwrap();
        assert_eq!(head.value, json!({"remaining":8}));
        assert_eq!(head.claim_id, b.id);
        tx.commit().unwrap();
    }
}

#[test]
fn rollback_restores_claims_contributions_generations_and_source_cut() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let before = views.token(&store.readers.get(), "limits", 1).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let b = append_claim_record_tx(
        &tx,
        &store.origin,
        "account/ada",
        "account.limits",
        None,
        &json!({"fields":{"remaining":0}}),
        &[],
        None,
    )
    .unwrap();
    let key = canonical::claim_key(&tx, &b.id).unwrap();
    views.change(&tx, None, Some((&b, &key)), 1).unwrap();
    views.publish_cut(&tx, cut(b.store_index)).unwrap();
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(value(&store, &views), Some(json!({"remaining":3})));
    assert_eq!(
        views.token(&store.readers.get(), "limits", 1).unwrap(),
        before
    );
    let count: u64 = store
        .readers
        .get()
        .query_row("SELECT COUNT(*) FROM claims WHERE id=?1", [b.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        smallclaims::ivm::source_cut(&store.readers.get()).unwrap(),
        Some(cut(a.store_index))
    );
}

#[test]
fn changed_canonical_rank_reselects_without_new_arrivals_and_old_dependency_is_removed() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let b = append(&store, &views, json!({"remaining":8}));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let mut key = canonical::claim_key(&tx, &a.id).unwrap();
    key.0 = u128::MAX;
    views.change(&tx, Some(&a), Some((&a, &key)), 1).unwrap();
    assert_eq!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(b.store_index))
            .unwrap()
            .unwrap()
            .claim_id,
        a.id
    );
    let mut moved = a.clone();
    moved.subject = "account/bert".into();
    let changes = views
        .change(&tx, Some(&a), Some((&moved, &key)), 1)
        .unwrap();
    assert_eq!(changes.changed.len(), 2);
    assert_eq!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(b.store_index))
            .unwrap()
            .unwrap()
            .claim_id,
        b.id
    );
    assert_eq!(
        views
            .head(&tx, "limits", "account/bert", "limits", cut(b.store_index))
            .unwrap()
            .unwrap()
            .claim_id,
        a.id
    );
}

#[test]
fn unrelated_kind_dispatch_executes_no_view_sql_and_version_changes_are_unready() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let mut unrelated = a.clone();
    unrelated.kind = "example.unrelated".into();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    // Removing the view tables is a stronger negative control than query text inspection.
    tx.execute_batch("DROP TABLE ivm_contributions; DROP TABLE ivm_heads; DROP TABLE ivm_views; DROP TABLE ivm_keys").unwrap();
    let changes = views
        .change(
            &tx,
            None,
            Some((
                &unrelated,
                &(0, String::new(), 0, String::new(), 0, String::new()),
            )),
            1,
        )
        .unwrap();
    assert!(changes.affected.is_empty());
    tx.rollback().unwrap();
    let tx = writer.transaction().unwrap();
    let new = Views::new(vec![Box::new(Limits(
        "limits.v2;registry.v2;authority.v1;keys.v2",
    ))])
    .unwrap();
    new.initialize_empty(&tx, cut(a.store_index)).unwrap();
    assert!(
        new.head(&tx, "limits", "account/ada", "limits", cut(a.store_index))
            .is_err()
    );
    assert!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(a.store_index))
            .is_err()
    );
    tx.commit().unwrap();
}

#[test]
fn missing_projection_on_nonempty_source_never_reports_ready() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views = views();
    let mut writer = store.connection.write();
    views.create_schema(&writer).unwrap();
    let tx = writer.transaction().unwrap();
    views.initialize_empty(&tx, cut(100)).unwrap();
    assert!(views.token(&tx, "limits", 1).is_err());
    tx.commit().unwrap();
}

#[test]
fn bounded_index_lookup_exposes_no_fullscan_or_sort_in_candidate_selection() {
    let (store, views) = fixture();
    let claim = append(&store, &views, json!({"remaining":3}));
    let reader = store.readers.get();
    let mut statement=reader.prepare("EXPLAIN QUERY PLAN SELECT value,claim_id,rank FROM ivm_contributions WHERE view=?1 AND key=?2 AND register=?3 ORDER BY rank DESC,claim_id DESC LIMIT 1").unwrap();
    let plan = statement
        .query_map(params!["limits", "account/ada", "limits"], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("ivm_contributions_rank"), "{plan}");
    assert!(
        !plan.contains("SCAN ") && !plan.contains("TEMP B-TREE"),
        "{plan}"
    );
    assert_eq!(
        views
            .head(
                &reader,
                "limits",
                "account/ada",
                "limits",
                cut(claim.store_index)
            )
            .unwrap()
            .unwrap()
            .value,
        json!({"remaining":3})
    );
}

#[test]
fn registered_runtime_automatically_projects_real_local_and_shuffled_remote_store_writes() {
    use smallclaims::{ClaimInput, ivm::runtime::ViewRuntime, replication::ReplicationInventory};
    fn node(name: &str) -> (Store, Arc<ViewRuntime>) {
        let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
        let store = Store::open_memory(name, runtime.clone()).unwrap();
        (store, runtime)
    }
    fn input(value: u64) -> ClaimInput {
        ClaimInput {
            subject: "account/ada".into(),
            kind: "account.limits".into(),
            actor: None,
            fields: serde_json::from_value(json!({"remaining":value})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        }
    }
    let (a, ar) = node("alder");
    let first = a.append_claim(&input(3)).unwrap();
    let second = a.append_claim(&input(8)).unwrap();
    assert_eq!(value(&a, &ar.views), Some(json!({"remaining":8})));
    let exchange = a
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    let (b, br) = node("birch");
    let (c, cr) = node("cedar");
    for envelopes in [
        exchange.envelopes.clone(),
        exchange.envelopes.iter().rev().cloned().collect(),
    ] {
        let (target, runtime) = if value(&b, &br.views).is_none() {
            (&b, &br)
        } else {
            (&c, &cr)
        };
        for envelope in envelopes {
            let mut delivery = exchange.clone();
            delivery.envelopes = vec![envelope];
            target
                .receive_replication_exchange("alder", "sample-fleet", &delivery)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.project_replication_backlog().unwrap();
            // Duplicate delivery cannot change a semantic generation.
            let token = runtime
                .views
                .token(&target.readers.get(), "limits", 1)
                .unwrap();
            target
                .receive_replication_exchange("alder", "sample-fleet", &delivery)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.project_replication_backlog().unwrap();
            assert_eq!(
                runtime
                    .views
                    .token(&target.readers.get(), "limits", 1)
                    .unwrap(),
                token
            );
        }
        let index = smallclaims::store::current_index(&target.readers.get()).unwrap();
        let head = runtime
            .views
            .head(
                &target.readers.get(),
                "limits",
                "account/ada",
                "limits",
                cut(index),
            )
            .unwrap()
            .unwrap();
        assert_eq!(head.claim_id, second.id);
        assert_ne!(head.claim_id, first.id);
        assert_eq!(head.value, json!({"remaining":8}));
    }
}

#[test]
fn same_index_legacy_renumbering_fences_readiness_without_replay() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE claims SET store_index=store_index+100 WHERE id=?1",
        [&a.id],
    )
    .unwrap();
    assert!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(a.store_index))
            .is_err()
    );
    tx.rollback().unwrap();
    let tx = writer.transaction().unwrap();
    tx.execute("DELETE FROM claims WHERE id=?1", [&a.id])
        .unwrap();
    assert!(views.token(&tx, "limits", 1).is_err());
    tx.rollback().unwrap();
}

struct CurrentLimits;
impl View for CurrentLimits {
    fn definition(&self) -> Definition {
        Definition {
            name: "current-limits",
            fingerprint: "numeric.v1;verified-owner-adapter.v1;independent-windows.v1",
            kinds: &[],
            local_kinds: &["account-limits.snapshot"],
            max_contributions: 2,
        }
    }
    fn create_schema(&self, connection: &rusqlite::Connection) -> Result<()> {
        connection.execute_batch("CREATE TABLE latest_numeric(account TEXT PRIMARY KEY,payload TEXT NOT NULL); CREATE TABLE limits_card(account TEXT PRIMARY KEY,payload TEXT NOT NULL);")?;
        Ok(())
    }
    fn maintain_local_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        key: &str,
        _change: &smallclaims::ivm::LocalChange,
    ) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let source = tx
            .query_row(
                "SELECT payload FROM latest_numeric WHERE account=?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let actual = tx
            .query_row(
                "SELECT payload FROM limits_card WHERE account=?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if source == actual {
            return Ok(false);
        }
        match source {
            Some(source) => {
                tx.execute("INSERT INTO limits_card VALUES(?1,?2) ON CONFLICT(account) DO UPDATE SET payload=excluded.payload",params![key,source])?;
            }
            None => {
                tx.execute("DELETE FROM limits_card WHERE account=?1", [key])?;
            }
        }
        Ok(true)
    }
}

#[test]
fn latest_numeric_adapter_creates_no_claims_and_keeps_samples_windows_and_owner_identity() {
    use smallclaims::ivm::{LocalChange, Readiness};
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views = Views::new(vec![Box::new(CurrentLimits)]).unwrap();
    let mut writer = store.connection.write();
    views.create_schema(&writer).unwrap();
    let tx = writer.transaction().unwrap();
    views.initialize_empty(&tx, cut(0)).unwrap();
    let payload=json!({"measurement_id":"sample-ada-1","owner_proof":"invented-verified-owner","actual_sample_at":120,
        "windows":[{"id":"short","remaining":0,"resets_at":180,"exhausted":true},{"id":"weekly","remaining":900,"resets_at":604920,"exhausted":false}]}).to_string();
    tx.execute(
        "INSERT INTO latest_numeric VALUES(?1,?2)",
        params!["account/ada", payload],
    )
    .unwrap();
    let change = LocalChange {
        kind: "account-limits.snapshot".into(),
        old_keys: Default::default(),
        new_keys: ["account/ada".to_owned()].into(),
        evaluation_time_unix_ms: 125,
    };
    let local = SourceCut {
        local_generation: 1,
        ..cut(0)
    };
    assert_eq!(
        views
            .local_change(&tx, &change, local)
            .unwrap()
            .changed
            .len(),
        1
    );
    let token = views.token(&tx, "current-limits", 1).unwrap();
    assert!(
        views
            .local_change(
                &tx,
                &change,
                SourceCut {
                    local_generation: 2,
                    ..cut(0)
                }
            )
            .unwrap()
            .changed
            .is_empty()
    );
    assert_eq!(views.token(&tx, "current-limits", 1).unwrap(), token);
    let actual: String = tx
        .query_row(
            "SELECT payload FROM limits_card WHERE account='account/ada'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&actual).unwrap(),
        serde_json::from_str::<Value>(&payload).unwrap()
    );
    let claims: u64 = tx
        .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        claims, 0,
        "authenticated latest numeric samples must not become durable telemetry claims"
    );
    assert!(matches!(
        views.readiness(&tx, "current-limits", 1).unwrap(),
        Readiness::Ready(_)
    ));
    tx.commit().unwrap();
    let tx = writer.transaction().unwrap();
    tx.execute("DELETE FROM latest_numeric WHERE account='account/ada'", [])
        .unwrap();
    let removal = LocalChange {
        kind: "account-limits.snapshot".into(),
        old_keys: ["account/ada".to_owned()].into(),
        new_keys: Default::default(),
        evaluation_time_unix_ms: 150,
    };
    views
        .local_change(
            &tx,
            &removal,
            SourceCut {
                local_generation: 3,
                ..cut(0)
            },
        )
        .unwrap();
    let page = views
        .changed_keys(&tx, "current-limits", 1, 0, 10, None)
        .unwrap();
    assert_eq!(page.keys.len(), 1);
    assert_eq!(
        page.keys[0].generation, 2,
        "removal is retained as a key invalidation"
    );
    tx.rollback().unwrap();
    let state = views
        .projection_state(&writer, "current-limits", 1)
        .unwrap();
    assert_eq!(state.applied_local_generation, Some(2));
    assert_eq!(views.token(&writer, "current-limits", 1).unwrap(), token);
}

#[test]
fn unready_projection_preserves_valid_source_admission_without_advancing_view_frontier() {
    use smallclaims::{
        ClaimInput,
        ivm::{Readiness, runtime::ViewRuntime},
    };
    let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let input = |remaining| ClaimInput {
        subject: "account/ada".into(),
        kind: "account.limits".into(),
        actor: None,
        fields: serde_json::from_value(json!({"remaining":remaining})).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    };
    let first = store.append_claim(&input(3)).unwrap();
    let writer = store.connection.write();
    writer
        .execute("UPDATE ivm_views SET ready=0 WHERE name='limits'", [])
        .unwrap();
    drop(writer);
    let second = store.append_claim(&input(8)).unwrap();
    let state = runtime
        .views
        .projection_state(&store.readers.get(), "limits", 1)
        .unwrap();
    assert_eq!(state.readiness, Readiness::Fenced);
    assert_eq!(state.applied_claim_index, Some(first.store_index));
    assert_eq!(state.deferred_claim_index, Some(second.store_index));
    assert_eq!(store.claims_for("account/ada", None).unwrap().len(), 2);
    assert!(
        runtime
            .views
            .head(
                &store.readers.get(),
                "limits",
                "account/ada",
                "limits",
                cut(second.store_index)
            )
            .is_err()
    );
}

#[test]
fn coalesced_changed_key_pages_retain_removals_and_report_interpage_semantic_gaps() {
    let (store, views) = fixture();
    let a = append(&store, &views, json!({"remaining":3}));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let b = append_claim_record_tx(
        &tx,
        &store.origin,
        "account/bert",
        "account.limits",
        None,
        &json!({"fields":{"remaining":7}}),
        &[],
        None,
    )
    .unwrap();
    let key = canonical::claim_key(&tx, &b.id).unwrap();
    views.change(&tx, None, Some((&b, &key)), 1).unwrap();
    views.publish_cut(&tx, cut(b.store_index)).unwrap();
    let first = views.changed_keys(&tx, "limits", 1, 0, 1, None).unwrap();
    assert_eq!(first.keys.len(), 1);
    assert!(first.next_after.is_some());
    let second = views
        .changed_keys(
            &tx,
            "limits",
            1,
            first.next_after.unwrap(),
            1,
            Some(&first.token),
        )
        .unwrap();
    assert_eq!(second.keys[0].key, "account/bert");
    assert!(second.next_after.is_none());
    views.change(&tx, Some(&a), None, 1).unwrap();
    assert!(
        views
            .changed_keys(
                &tx,
                "limits",
                1,
                first.next_after.unwrap(),
                1,
                Some(&first.token)
            )
            .is_err()
    );
    let current = views.changed_keys(&tx, "limits", 1, 0, 10, None).unwrap();
    assert_eq!(current.keys.len(), 2);
    assert_eq!(
        current
            .keys
            .iter()
            .find(|k| k.key == "account/ada")
            .unwrap()
            .generation,
        2
    );
    assert!(
        views
            .head(&tx, "limits", "account/ada", "limits", cut(b.store_index))
            .unwrap()
            .is_none()
    );
    tx.rollback().unwrap();
}

struct Faulty;
impl View for Faulty {
    fn definition(&self) -> Definition {
        Limits("faulty.v1").definition()
    }
    fn contributions(
        &self,
        _claim: &ClaimRecord,
        _key: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        anyhow::bail!("invented operator failure")
    }
}
#[test]
fn operator_failure_rolls_back_projection_keeps_source_and_records_unready_evidence() {
    use smallclaims::{
        ClaimInput,
        ivm::{Readiness, runtime::ViewRuntime},
    };
    let runtime = Arc::new(ViewRuntime::new(Views::new(vec![Box::new(Faulty)]).unwrap()).unwrap());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let claim = store
        .append_claim(&ClaimInput {
            subject: "account/ada".into(),
            kind: "account.limits".into(),
            actor: None,
            fields: serde_json::from_value(json!({"remaining":3})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let reader = store.readers.get();
    let state = runtime
        .views
        .projection_state(&reader, "limits", 1)
        .unwrap();
    assert_eq!(state.readiness, Readiness::Fenced);
    assert_eq!(state.applied_claim_index, Some(0));
    assert_eq!(state.deferred_claim_index, Some(claim.store_index));
    let ranks: u64 = reader
        .query_row("SELECT COUNT(*) FROM ivm_claim_ranks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ranks, 0, "failed mapper rank writes must roll back");
    let error: String = reader
        .query_row(
            "SELECT error FROM ivm_view_errors WHERE view='limits'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(error.contains("invented operator failure"));
    assert_eq!(store.claims_for("account/ada", None).unwrap().len(), 1);
}

#[test]
fn registered_runtime_reopens_persisted_view_without_history_rebuild() {
    use smallclaims::{ClaimInput, ivm::runtime::ViewRuntime};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invented.sqlite");
    let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let store = Store::open(&path, "alder", runtime.clone()).unwrap();
    let claim = store
        .append_claim(&ClaimInput {
            subject: "account/ada".into(),
            kind: "account.limits".into(),
            actor: None,
            fields: serde_json::from_value(json!({"remaining":3})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let token = runtime
        .views
        .token(&store.readers.get(), "limits", 1)
        .unwrap();
    drop(store);
    drop(runtime);
    let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let reopened = Store::open(&path, "alder", runtime.clone()).unwrap();
    assert_eq!(
        runtime
            .views
            .token(&reopened.readers.get(), "limits", 1)
            .unwrap(),
        token
    );
    assert_eq!(
        runtime
            .views
            .head(
                &reopened.readers.get(),
                "limits",
                "account/ada",
                "limits",
                cut(claim.store_index)
            )
            .unwrap()
            .unwrap()
            .claim_id,
        claim.id
    );
}

#[test]
fn register_evidence_detects_same_index_corruption_without_read_side_healing() {
    let (store, views) = fixture();
    let claim = append(&store, &views, json!({"remaining":3}));
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("DELETE FROM ivm_heads WHERE view='limits'", [])
        .unwrap();
    let evidence = views
        .register_evidence(&tx, "limits", "account/ada", "limits", 1)
        .unwrap();
    assert_eq!(evidence.matches, Some(false));
    assert!(evidence.expected.is_some() && evidence.actual.is_none());
    assert!(
        views
            .head(
                &tx,
                "limits",
                "account/ada",
                "limits",
                cut(claim.store_index)
            )
            .is_err()
    );
    let actual: u64 = tx
        .query_row("SELECT COUNT(*) FROM ivm_heads", [], |r| r.get(0))
        .unwrap();
    assert_eq!(actual, 0, "evidence read must not repair actual rows");
    tx.execute("UPDATE ivm_views SET ready=0 WHERE name='limits'", [])
        .unwrap();
    assert_eq!(
        views
            .register_evidence(&tx, "limits", "account/ada", "limits", 1)
            .unwrap()
            .matches,
        None
    );
    tx.rollback().unwrap();
}

#[test]
fn availability_is_readable_while_fenced_and_status_rolls_back_without_semantic_changes() {
    use smallclaims::ivm::Readiness;
    let (store, views) = fixture();
    append(&store, &views, json!({"remaining":3}));
    let before = views
        .availability(&store.readers.get(), "limits", 1)
        .unwrap();
    let semantic = views.token(&store.readers.get(), "limits", 1).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("UPDATE ivm_views SET ready=0 WHERE name='limits'", [])
        .unwrap();
    let fenced = views.availability(&tx, "limits", 1).unwrap();
    assert_eq!(fenced.readiness, Readiness::Fenced);
    assert!(fenced.token.view_sequence > before.token.view_sequence);
    assert_eq!(fenced.token.source_sequence, before.token.source_sequence);
    assert!(views.changed_keys(&tx, "limits", 1, 0, 10, None).is_err());
    let generation: u64 = tx
        .query_row(
            "SELECT generation FROM ivm_views WHERE name='limits'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(generation, semantic.generation);
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(
        views
            .availability(&store.readers.get(), "limits", 1)
            .unwrap(),
        before
    );
}

#[test]
fn changed_error_evidence_advances_a_fenced_cursor_but_identical_evidence_does_not() {
    use smallclaims::ivm::Readiness;
    let (store, views) = fixture();
    append(&store, &views, json!({"remaining":3}));
    let semantic = views.token(&store.readers.get(), "limits", 1).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("UPDATE ivm_views SET ready=0 WHERE name='limits'", [])
        .unwrap();
    let fenced = views.availability(&tx, "limits", 1).unwrap();
    assert_eq!(fenced.readiness, Readiness::Fenced);
    let replace = |error: &str| {
        tx.execute(
        "INSERT INTO ivm_view_errors VALUES('limits',?1) ON CONFLICT(view) DO UPDATE SET error=excluded.error WHERE error<>excluded.error", [error],
    ).unwrap()
    };
    assert_eq!(replace("first bounded failure"), 1);
    let first = views.availability(&tx, "limits", 1).unwrap();
    assert!(first.token.view_sequence > fenced.token.view_sequence);
    assert_eq!(first.error.as_deref(), Some("first bounded failure"));
    assert_eq!(replace("second bounded failure"), 1);
    let second = views.availability(&tx, "limits", 1).unwrap();
    assert_eq!(second.readiness, Readiness::Fenced);
    assert_eq!(second.error.as_deref(), Some("second bounded failure"));
    assert!(second.token.view_sequence > first.token.view_sequence);
    assert_eq!(second.token.source_sequence, fenced.token.source_sequence);
    assert_eq!(replace("second bounded failure"), 0);
    // Also cover an unguarded identical SQL update: the status trigger's WHEN must suppress it.
    tx.execute(
        "UPDATE ivm_view_errors SET error=error WHERE view='limits'",
        [],
    )
    .unwrap();
    assert_eq!(views.availability(&tx, "limits", 1).unwrap(), second);
    tx.execute("DELETE FROM ivm_view_errors WHERE view='limits'", [])
        .unwrap();
    let cleared = views.availability(&tx, "limits", 1).unwrap();
    assert_eq!(cleared.readiness, Readiness::Fenced);
    assert_eq!(cleared.error, None);
    assert!(cleared.token.view_sequence > second.token.view_sequence);
    let generation: u64 = tx
        .query_row(
            "SELECT generation FROM ivm_views WHERE name='limits'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(generation, semantic.generation);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(
        views
            .availability(&store.readers.get(), "limits", 1)
            .unwrap(),
        cleared
    );
}

struct CustomOnly;
impl View for CustomOnly {
    fn definition(&self) -> Definition {
        Limits("custom-only.v1;references.v1").definition()
    }
    fn create_schema(&self, connection: &rusqlite::Connection) -> Result<()> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS custom_output(subject TEXT PRIMARY KEY,payload TEXT NOT NULL)")?;
        Ok(())
    }
    fn affected_keys(
        &self,
        _tx: &rusqlite::Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<std::collections::BTreeSet<String>> {
        Ok(old
            .into_iter()
            .chain(new)
            .map(|claim| claim.subject.clone())
            .collect())
    }
    fn canonical_dependencies(
        &self,
        claim: &ClaimRecord,
    ) -> Result<std::collections::BTreeSet<String>> {
        Ok(claim
            .body
            .pointer("/fields/reference")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect())
    }
    fn maintain_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        key: &str,
        _old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        let changed = if let Some(claim) = new.filter(|claim| claim.subject == key) {
            let payload =
                json!({"id":claim.id,"position":canonical::claim_key(tx,&claim.id)?.4}).to_string();
            tx.execute("INSERT INTO custom_output VALUES(?1,?2) ON CONFLICT(subject) DO UPDATE SET payload=excluded.payload WHERE payload<>excluded.payload", params![key,payload])? > 0
        } else {
            tx.execute("DELETE FROM custom_output WHERE subject=?1", [key])? > 0
        };
        Ok(Some(changed))
    }
}
fn insert_test_record(tx: &rusqlite::Transaction<'_>, claim: &ClaimRecord, position: u64) {
    tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms) VALUES(?1,'fixture',1,?2,?3,X'','valid',?2,'0')", params![format!("test-record/{}",claim.id),claim.id,position]).unwrap();
}

#[test]
fn custom_only_operator_registers_provenance_and_rank_mutation_fences_durably() {
    use smallclaims::ivm::{Readiness, runtime::ViewRuntime};
    let runtime =
        Arc::new(ViewRuntime::new(Views::new(vec![Box::new(CustomOnly)]).unwrap()).unwrap());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let claim = store
        .append_claim(&smallclaims::ClaimInput {
            subject: "account/ada".into(),
            kind: "account.limits".into(),
            actor: None,
            fields: serde_json::from_value(json!({"remaining":3})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let before = runtime
        .views
        .availability(&store.readers.get(), "limits", 1)
        .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let contributions: u64 = tx
        .query_row("SELECT COUNT(*) FROM ivm_contributions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(contributions, 0);
    let dependencies: u64 = tx
        .query_row(
            "SELECT COUNT(*) FROM ivm_claim_views WHERE view='limits' AND claim_id=?1",
            [&claim.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dependencies, 1);
    insert_test_record(&tx, &claim, 0);
    assert!(matches!(
        runtime.views.readiness(&tx, "limits", 1).unwrap(),
        Readiness::Ready(_)
    ));
    tx.execute(
        "UPDATE replica_records SET position=7 WHERE claim_id=?1",
        [&claim.id],
    )
    .unwrap();
    let fenced = runtime.views.availability(&tx, "limits", 1).unwrap();
    assert_eq!(fenced.readiness, Readiness::Fenced);
    assert!(fenced.token.view_sequence > before.token.view_sequence);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(
        runtime
            .views
            .availability(&store.readers.get(), "limits", 1)
            .unwrap(),
        fenced
    );
    let outputs: u64 = store
        .readers
        .get()
        .query_row("SELECT COUNT(*) FROM custom_output", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        outputs, 1,
        "fencing must not heal, erase output or reject its durable source"
    );
}

#[test]
fn unrelated_remote_source_pending_and_ready_are_durable_without_key_changes() {
    use smallclaims::{
        ivm::{Readiness, runtime::ViewRuntime},
        replication::ReplicationInventory,
    };
    let source_runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let source = Store::open_memory("alder", source_runtime).unwrap();
    source
        .append_claim(&smallclaims::ClaimInput {
            subject: "example/unrelated".into(),
            kind: "example.unrelated".into(),
            actor: None,
            fields: Default::default(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sqlite");
    let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let target = Store::open(&path, "birch", runtime.clone()).unwrap();
    let semantic = runtime
        .views
        .token(&target.readers.get(), "limits", 1)
        .unwrap();
    let initial = runtime
        .views
        .availability(&target.readers.get(), "limits", 1)
        .unwrap();
    target
        .receive_replication_exchange("alder", "sample-fleet", &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    let pending = runtime
        .views
        .availability(&target.readers.get(), "limits", 1)
        .unwrap();
    assert_eq!(pending.readiness, Readiness::SourcePending);
    assert!(pending.token.source_sequence > initial.token.source_sequence);
    assert_eq!(pending.token.view_sequence, initial.token.view_sequence);
    drop(target);
    drop(runtime);
    let runtime = Arc::new(ViewRuntime::new(views()).unwrap());
    let reopened = Store::open(&path, "birch", runtime.clone()).unwrap();
    assert_eq!(
        runtime
            .views
            .availability(&reopened.readers.get(), "limits", 1)
            .unwrap(),
        pending
    );
    reopened.project_replication_backlog().unwrap();
    let ready = runtime
        .views
        .availability(&reopened.readers.get(), "limits", 1)
        .unwrap();
    assert_eq!(ready.readiness, Readiness::Ready(semantic));
    assert!(ready.token.source_sequence > pending.token.source_sequence);
    assert_eq!(ready.token.view_sequence, pending.token.view_sequence);
    assert!(
        runtime
            .views
            .changed_keys(&reopened.readers.get(), "limits", 1, 0, 10, None)
            .unwrap()
            .keys
            .is_empty()
    );
    reopened.project_replication_backlog().unwrap();
    assert_eq!(
        runtime
            .views
            .availability(&reopened.readers.get(), "limits", 1)
            .unwrap(),
        ready
    );
}

#[test]
fn retracting_one_input_preserves_another_inputs_referenced_claim_dependency() {
    use smallclaims::ivm::{Readiness, runtime::ViewRuntime};
    let runtime =
        Arc::new(ViewRuntime::new(Views::new(vec![Box::new(CustomOnly)]).unwrap()).unwrap());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let input = |subject: &str, fields| smallclaims::ClaimInput {
        subject: subject.into(),
        kind: "account.limits".into(),
        actor: None,
        fields,
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    };
    let a = store
        .append_claim(&input("account/ada", Default::default()))
        .unwrap();
    let b = store
        .append_claim(&input(
            "account/bert",
            serde_json::from_value(json!({"reference":a.id})).unwrap(),
        ))
        .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    runtime.views.change(&tx, Some(&a), None, 1).unwrap();
    let owners: Vec<String> = tx
        .prepare("SELECT input_claim_id FROM ivm_claim_views WHERE claim_id=?1")
        .unwrap()
        .query_map([&a.id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(owners, vec![b.id]);
    insert_test_record(&tx, &a, 9);
    assert_eq!(
        runtime
            .views
            .availability(&tx, "limits", 1)
            .unwrap()
            .readiness,
        Readiness::Fenced
    );
    tx.rollback().unwrap();
}
