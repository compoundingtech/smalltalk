//! Real populated Store + explicit namespace installation/event/read-certificate controls.
//! Full-card admission/authority/extractor parity remains the production adapter's obligation.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimInput, Store,
    ivm::{
        self, Definition, Readiness, SourceCut, View, Views,
        events::{self, Publisher},
        install::{Installer, Limits, Mutation, Namespace, Operator, Outcome, ScanPage},
        installed::{Changed, SyncOutcome},
    },
    store::runtime::Plain,
};
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;
const NAME: &str = "agent.cards";
const SOURCE: &str = "admitted-agent-card-source";
const FINGERPRINT: &str = "agent-card-fixture.v1;complete-replacement;person-status-work-usage";
struct Cards;
impl View for Cards {
    fn definition(&self) -> Definition {
        Definition {
            name: NAME,
            fingerprint: FINGERPRINT,
            kinds: &["custom.agent.fixture"],
            local_kinds: &["local.agent"],
            max_contributions: 4,
        }
    }
    fn installed_source(&self) -> Option<&'static str> {
        Some(SOURCE)
    }
    fn contributions(
        &self,
        _: &smallclaims::ClaimRecord,
        _: &smallclaims::store::canonical::ClaimKey,
    ) -> Result<Vec<ivm::Contribution>> {
        panic!("legacy callback must not run")
    }
}
impl Operator for Cards {
    fn name(&self) -> &'static str {
        NAME
    }
    fn source(&self) -> &'static str {
        SOURCE
    }
    fn fingerprint(&self) -> &'static str {
        FINGERPRINT
    }
    fn create_schema(&self, db: &Connection) -> Result<()> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS installed_agent_cards(namespace TEXT,key TEXT,payload TEXT,PRIMARY KEY(namespace,key));")?;
        Ok(())
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        let mut changed = false;
        for row in rows {
            ensure!(row.key != "unsupported", "incomplete authority evidence");
            let before: Option<String> = tx
                .query_row(
                    "SELECT payload FROM installed_agent_cards WHERE namespace=?1 AND key=?2",
                    params![ns.as_str(), row.key],
                    |r| r.get(0),
                )
                .optional()?;
            let after = row.new.as_ref().map(serde_json::to_string).transpose()?;
            if before == after {
                continue;
            }
            if let Some(after) = after {
                tx.execute("INSERT INTO installed_agent_cards VALUES(?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET payload=excluded.payload",params![ns.as_str(),row.key,after])?;
            } else {
                tx.execute(
                    "DELETE FROM installed_agent_cards WHERE namespace=?1 AND key=?2",
                    params![ns.as_str(), row.key],
                )?;
            }
            changed = true;
        }
        Ok(changed)
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        tx.execute("DELETE FROM installed_agent_cards WHERE namespace=?1 AND key IN(SELECT key FROM installed_agent_cards WHERE namespace=?1 ORDER BY key LIMIT ?2)",params![ns.as_str(),rows as u64])?;
        Ok(!tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM installed_agent_cards WHERE namespace=?1)",
            [ns.as_str()],
            |r| r.get::<_, bool>(0),
        )?)
    }
}
struct Fixture {
    store: Store,
    views: Views,
    installer: Installer,
    index: u64,
    claim: smallclaims::ClaimRecord,
}
fn card(person: &str, status: &str) -> Value {
    json!({"person":person,"status":status,"work":"step/amber","usage":{"weekly":42}})
}
fn fixture() -> Fixture {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let claim = store
        .append_claim(&ClaimInput {
            subject: "agent/amber".into(),
            kind: "custom.agent.fixture".into(),
            actor: None,
            fields: serde_json::from_value(card("alder", "idle")).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let views = Views::new(vec![Box::new(Cards)]).unwrap();
    let installer = Installer::new(vec![Box::new(Cards)]).unwrap();
    {
        let mut writer = store.connection.write();
        views.create_schema(&writer).unwrap();
        installer.create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        views
            .initialize_empty(
                &tx,
                SourceCut {
                    epoch: 9,
                    admitted: claim.store_index,
                    projected: 0,
                    local_generation: 0,
                },
            )
            .unwrap();
        // Installation source epoch/revision intentionally differ from graph epoch/store_index.
        installer
            .register_source(&tx, SOURCE, "full-agent-source-fixture.v1", 3)
            .unwrap();
        tx.execute_batch("CREATE TABLE raw_agent_cards(key TEXT PRIMARY KEY,payload TEXT)")
            .unwrap();
        tx.execute(
            "INSERT INTO raw_agent_cards VALUES('agent/amber',?1)",
            [card("alder", "idle").to_string()],
        )
        .unwrap();
        events::install(&tx, 64).unwrap();
        views.register_installed(&tx, &installer, NAME).unwrap();
        tx.commit().unwrap();
    }
    Fixture {
        store,
        views,
        installer,
        index: claim.store_index,
        claim,
    }
}
fn cut(f: &Fixture, local: u64) -> SourceCut {
    SourceCut {
        epoch: 9,
        admitted: f.index,
        projected: f.index,
        local_generation: local,
    }
}
fn limits() -> Limits {
    Limits {
        page_rows: 4,
        page_bytes: 4096,
        pending_rows: 32,
        pending_bytes: 65536,
        total_rows: 128,
        callback_ms: 1000,
        lifetime_ms: 10000,
    }
}
fn scanned(f: &Fixture) -> String {
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let job = f.installer.start(&tx, NAME, limits(), 0).unwrap();
    let payload: String = tx
        .query_row(
            "SELECT payload FROM raw_agent_cards WHERE key='agent/amber'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let position = f.installer.position(&tx, SOURCE).unwrap();
    assert_eq!(
        f.installer
            .scan(
                &tx,
                &ScanPage {
                    job: job.clone(),
                    expected_cursor: vec![],
                    next_cursor: vec![1],
                    position,
                    rows: vec![Mutation {
                        key: "agent/amber".into(),
                        old: None,
                        new: Some(serde_json::from_str(&payload).unwrap())
                    }],
                    finished: true
                },
                1
            )
            .unwrap(),
        Outcome::Progress
    );
    tx.commit().unwrap();
    job
}
fn publish(f: &Fixture, job: &str, local: u64) {
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let position = f.installer.position(&tx, SOURCE).unwrap();
    assert_eq!(
        f.views
            .catch_up_installed(&tx, &f.installer, job, &position, cut(f, local), 2)
            .unwrap(),
        Outcome::Published
    );
    tx.commit().unwrap();
}
fn boundary(f: &Fixture) -> events::Boundary {
    f.store
        .read_snapshot(|_| events::capture(&f.store.readers.get(), &f.views, NAME))
        .unwrap()
}
fn root_payload(f: &Fixture) -> Value {
    f.store.read_snapshot(|_| {
        let db=f.store.readers.get(); let root=f.views.installed_root(&db,&f.installer,NAME)?;
        let value:String=db.query_row("SELECT payload FROM installed_agent_cards WHERE namespace=?1 AND key='agent/amber'",[root.namespace.as_str()],|r|r.get(0))?;
        Ok(serde_json::from_str(&value)?)
    }).unwrap()
}
fn mutation(
    f: &Fixture,
    tx: &Transaction<'_>,
    key: &str,
    after: Option<Value>,
) -> smallclaims::ivm::install::SourcePosition {
    let before: Option<String> = tx
        .query_row(
            "SELECT payload FROM raw_agent_cards WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    if let Some(value) = &after {
        tx.execute("INSERT INTO raw_agent_cards VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET payload=excluded.payload",params![key,value.to_string()]).unwrap();
    } else {
        tx.execute("DELETE FROM raw_agent_cards WHERE key=?1", [key])
            .unwrap();
    }
    f.installer
        .record(
            tx,
            SOURCE,
            &Mutation {
                key: key.into(),
                old: before.map(|s| serde_json::from_str(&s).unwrap()),
                new: after,
            },
        )
        .unwrap()
}

#[test]
fn populated_store_publication_exposes_exact_namespace_and_committed_ready_event() {
    let f = fixture();
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::Fenced
    ));
    let job = scanned(&f);
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::Fenced
    ));
    let publisher = Publisher::attach(&f.store, 4).unwrap();
    let mut receiver = publisher.subscribe();
    publish(&f, &job, 7);
    assert!(receiver.try_recv().is_ok());
    let after = boundary(&f);
    assert!(matches!(after.availability.readiness, Readiness::Ready(_)));
    assert_eq!(after.source_cut, cut(&f, 7));
    assert_eq!(root_payload(&f), card("alder", "idle"));
    let db = f.store.readers.get();
    let root = f.views.installed_root(&db, &f.installer, NAME).unwrap();
    assert_eq!(root.namespace.as_str(), job);
    assert_eq!(root.epoch, 3);
    assert_eq!(root.revision, 0);
    assert_eq!(after.identity.epoch, 9);
    assert!(f.index > 0);
}

#[test]
fn rolled_back_publication_keeps_root_events_and_cut_unpublished() {
    let f = fixture();
    let job = scanned(&f);
    let before = boundary(&f);
    let publisher = Publisher::attach(&f.store, 4).unwrap();
    let mut receiver = publisher.subscribe();
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = f.installer.position(&tx, SOURCE).unwrap();
        assert_eq!(
            f.views
                .catch_up_installed(&tx, &f.installer, &job, &pos, cut(&f, 0), 2)
                .unwrap(),
            Outcome::Published
        );
        tx.rollback().unwrap();
    }
    assert_eq!(boundary(&f), before);
    assert!(f.installer.root(&f.store.readers.get(), NAME).is_err());
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    publish(&f, &job, 0);
}

#[test]
fn live_replacement_and_removal_publish_keys_with_same_namespace_and_source_cut() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let before = boundary(&f);
    let publisher = Publisher::attach(&f.store, 4).unwrap();
    let mut receiver = publisher.subscribe();
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "agent/amber", Some(card("birch", "working")));
        assert_eq!(
            f.views
                .sync_installed(
                    &tx,
                    &f.installer,
                    NAME,
                    &pos,
                    cut(&f, 1),
                    Changed::Keys(&["agent/amber".into()])
                )
                .unwrap(),
            SyncOutcome::Current
        );
        tx.commit().unwrap();
    }
    assert!(receiver.try_recv().is_ok());
    assert_eq!(root_payload(&f), card("birch", "working"));
    let after = boundary(&f);
    assert!(after.keys.sequence > before.keys.sequence);
    assert_eq!(after.source_cut, cut(&f, 1));
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "agent/amber", None);
        f.views
            .sync_installed(
                &tx,
                &f.installer,
                NAME,
                &pos,
                cut(&f, 2),
                Changed::Keys(&["agent/amber".into()]),
            )
            .unwrap();
        tx.commit().unwrap();
    }
    let db = f.store.readers.get();
    let root = f.views.installed_root(&db, &f.installer, NAME).unwrap();
    assert_eq!(
        db.query_row::<u64, _, _>(
            "SELECT COUNT(*) FROM installed_agent_cards WHERE namespace=?1",
            [root.namespace.as_str()],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert!(boundary(&f).keys.sequence > after.keys.sequence);
}

#[test]
fn identical_answer_preserves_generation_and_keys_and_unrelated_claim_cut_stays_fresh() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "agent/amber", Some(card("alder", "idle")));
        f.views
            .sync_installed(
                &tx,
                &f.installer,
                NAME,
                &pos,
                cut(&f, 1),
                Changed::Keys(&[]),
            )
            .unwrap();
        tx.commit().unwrap();
    }
    let after = boundary(&f);
    assert_eq!(
        after.snapshot.semantic_generation,
        before.snapshot.semantic_generation
    );
    assert_eq!(after.keys, before.keys);
    let unrelated = f
        .store
        .append_claim(&ClaimInput {
            subject: "other/unrelated".into(),
            kind: "custom.unknown.fixture".into(),
            actor: None,
            fields: Default::default(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::SourcePending
    ));
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        f.views
            .publish_cut(
                &tx,
                SourceCut {
                    admitted: unrelated.store_index,
                    projected: unrelated.store_index,
                    ..cut(&f, 1)
                },
            )
            .unwrap();
        tx.commit().unwrap();
    }
    let fresh = boundary(&f);
    assert!(matches!(fresh.availability.readiness, Readiness::Ready(_)));
    assert_eq!(fresh.keys, after.keys);
}

#[test]
fn root_mutated_without_sync_is_unready_even_if_legacy_ready_flag_stays_true() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        mutation(&f, &tx, "agent/amber", Some(card("alder", "working")));
        tx.commit().unwrap();
    }
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::SourcePending
    ));
    assert!(
        f.views
            .installed_root(&f.store.readers.get(), &f.installer, NAME)
            .is_err()
    );
}

#[test]
fn later_gap_cannot_be_cleared_by_live_sync_but_fresh_namespace_can_recover() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        f.views
            .fence(&tx, NAME, "uncaptured authority change")
            .unwrap();
        tx.commit().unwrap();
    }
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "agent/amber", Some(card("birch", "working")));
        assert_eq!(
            f.views
                .sync_installed(&tx, &f.installer, NAME, &pos, cut(&f, 1), Changed::Refresh)
                .unwrap(),
            SyncOutcome::Fenced
        );
        tx.commit().unwrap();
    }
    assert_eq!(boundary(&f), before);
    let replacement = scanned(&f);
    publish(&f, &replacement, 1);
    assert_ne!(replacement, job);
    assert_eq!(root_payload(&f), card("birch", "working"));
}

#[test]
fn operator_failure_preserves_source_admission_and_fences_registry_without_advancing_cut() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "unsupported", Some(card("alder", "idle")));
        assert_eq!(
            f.views
                .sync_installed(&tx, &f.installer, NAME, &pos, cut(&f, 1), Changed::Refresh)
                .unwrap(),
            SyncOutcome::Fenced
        );
        tx.commit().unwrap();
    }
    let after = boundary(&f);
    assert!(matches!(after.availability.readiness, Readiness::Fenced));
    assert_eq!(after.source_cut, before.source_cut);
    assert_eq!(
        f.store
            .readers
            .get()
            .query_row::<u64, _, _>(
                "SELECT COUNT(*) FROM raw_agent_cards WHERE key='unsupported'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn wrong_source_position_or_graph_frontier_cannot_publish() {
    let f = fixture();
    let job = scanned(&f);
    let before = boundary(&f);
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let pos = f.installer.position(&tx, SOURCE).unwrap();
    let mut stale = pos.clone();
    stale.revision += 1;
    assert!(
        f.views
            .catch_up_installed(&tx, &f.installer, &job, &stale, cut(&f, 0), 2)
            .is_err()
    );
    assert!(
        f.views
            .catch_up_installed(
                &tx,
                &f.installer,
                &job,
                &pos,
                SourceCut {
                    projected: 0,
                    ..cut(&f, 0)
                },
                2
            )
            .is_err()
    );
    assert_eq!(f.installer.progress(&tx, &job).unwrap().phase, "catchup");
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(boundary(&f), before);
}

#[test]
fn changed_key_overflow_preserves_source_and_fences_without_advancing_view_cut() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let pos = mutation(&f, &tx, "agent/amber", Some(card("alder", "working")));
        let keys = (0..1025).map(|n| format!("agent/{n}")).collect::<Vec<_>>();
        assert_eq!(
            f.views
                .sync_installed(
                    &tx,
                    &f.installer,
                    NAME,
                    &pos,
                    cut(&f, 1),
                    Changed::Keys(&keys)
                )
                .unwrap(),
            SyncOutcome::Fenced
        );
        tx.commit().unwrap();
    }
    let after = boundary(&f);
    assert_eq!(after.keys, before.keys);
    assert_eq!(after.source_cut, before.source_cut);
    assert!(matches!(after.availability.readiness, Readiness::Fenced));
    let payload: String = f
        .store
        .readers
        .get()
        .query_row(
            "SELECT payload FROM raw_agent_cards WHERE key='agent/amber'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        card("alder", "working")
    );
}

#[test]
fn namespace_registration_does_not_dispatch_legacy_callbacks_and_cannot_adopt_old_root() {
    let f = fixture();
    let job = scanned(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let key = smallclaims::store::canonical::claim_key(&tx, &f.claim.id).unwrap();
        let change = f
            .views
            .change(&tx, None, Some((&f.claim, &key)), 9)
            .unwrap();
        assert!(change.changed.is_empty() && change.deferred.is_empty());
        assert_eq!(
            f.installer.catch_up(&tx, &job, 2).unwrap(),
            Outcome::Published
        );
        let pos = f.installer.position(&tx, SOURCE).unwrap();
        assert_eq!(
            f.views
                .sync_installed(&tx, &f.installer, NAME, &pos, cut(&f, 0), Changed::Refresh)
                .unwrap(),
            SyncOutcome::Fenced
        );
        tx.commit().unwrap();
    }
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::Fenced
    ));
    assert!(
        f.views
            .installed_root(&f.store.readers.get(), &f.installer, NAME)
            .is_err()
    );
}

#[test]
fn binding_sql_failure_reverts_installer_publication_and_preserves_sqlite_cause() {
    let f = fixture();
    let job = scanned(&f);
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute_batch("CREATE TRIGGER reject_binding BEFORE UPDATE ON ivm_installed_bindings WHEN NEW.namespace IS NOT NULL BEGIN SELECT RAISE(ABORT,'binding storage refusal'); END;").unwrap();
        let position = f.installer.position(&tx, SOURCE).unwrap();
        let error = f
            .views
            .catch_up_installed(&tx, &f.installer, &job, &position, cut(&f, 0), 2)
            .unwrap_err();
        assert!(error.chain().any(|cause| cause.is::<rusqlite::Error>()));
        assert_eq!(f.installer.progress(&tx, &job).unwrap().phase, "catchup");
        assert!(f.installer.root(&tx, NAME).is_err());
        tx.rollback().unwrap();
    }
    assert_eq!(boundary(&f), before);
    publish(&f, &job, 0);
}

#[test]
fn re_registration_is_noop_and_namespace_definition_rejects_legacy_register_reads() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let before = boundary(&f);
    {
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        f.views.register_installed(&tx, &f.installer, NAME).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(boundary(&f), before);
    assert!(
        f.views
            .head(
                &f.store.readers.get(),
                NAME,
                "agent/amber",
                "status",
                cut(&f, 0)
            )
            .is_err()
    );
}

#[test]
fn namespace_scope_cannot_leak_an_unpublished_shadow_or_another_owner_row() {
    let f = fixture();
    let job = scanned(&f);
    publish(&f, &job, 0);
    let shadow = scanned(&f);
    assert_ne!(shadow, job);
    let db = f.store.readers.get();
    let root = f.views.installed_root(&db, &f.installer, NAME).unwrap();
    assert_eq!(root.namespace.as_str(), job);
    assert_eq!(db.query_row::<u64,_,_>("SELECT COUNT(*) FROM installed_agent_cards WHERE namespace=?1 AND json_extract(payload,'$.person')=?2",params![root.namespace.as_str(),"birch"],|r|r.get(0)).unwrap(),0);
    // Compatible background installation does not replace the currently published root.
    assert!(matches!(
        boundary(&f).availability.readiness,
        Readiness::Ready(_)
    ));
}
