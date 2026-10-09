//! Actual AFTER mutations append immutable native references into the ONE Installer journal.
use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde_json::json;
use smallclaims::ivm::install::{
    Installer, Limits, Mutation, Namespace, Operator,
    capture::{CapturePlan, Change},
    prepared::{CaptureLimits, PublicationLimits},
};

struct Never;
impl smallclaims::ivm::View for Never {
    fn definition(&self) -> smallclaims::ivm::Definition {
        smallclaims::ivm::Definition {
            name: "cards",
            fingerprint: "captured-images.v1",
            kinds: &[],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn installed_source(&self) -> Option<&'static str> {
        Some("native")
    }
    fn contributions(
        &self,
        _: &smallclaims::ClaimRecord,
        _: &smallclaims::store::canonical::ClaimKey,
    ) -> Result<Vec<smallclaims::ivm::Contribution>> {
        panic!("no legacy callback")
    }
}
impl Operator for Never {
    fn name(&self) -> &'static str {
        "cards"
    }
    fn source(&self) -> &'static str {
        "native"
    }
    fn fingerprint(&self) -> &'static str {
        "captured-images.v1"
    }
    fn create_schema(&self, _: &Connection) -> Result<()> {
        Ok(())
    }
    fn apply(&self, _: &Transaction<'_>, _: &Namespace, _: &[Mutation]) -> Result<bool> {
        panic!("no reducer in admission")
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        panic!("no validation in admission")
    }
    fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
        Ok(true)
    }
}
fn fixture(rows: u64) -> (Connection, Installer, CapturePlan, String) {
    fixture_limits(rows, 32768, 4096)
}
fn fixture_limits(
    rows: u64,
    bytes: u64,
    reference_bytes: usize,
) -> (Connection, Installer, CapturePlan, String) {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE native(id TEXT PRIMARY KEY,body BLOB);
        CREATE TABLE images(revision INTEGER,side INTEGER,id TEXT,body BLOB,PRIMARY KEY(revision,side));").unwrap();
    let installer = Installer::new(vec![Box::new(Never)]).unwrap();
    installer.create_schema(&db).unwrap();
    let tx = db.transaction().unwrap();
    installer
        .register_source(&tx, "native", "batch-manifest-fp;'literal", 7)
        .unwrap();
    let pos = installer.position(&tx, "native").unwrap();
    installer
        .enable_deferred(
            &tx,
            &pos,
            CaptureLimits {
                rows,
                bytes,
                reference_bytes,
            },
        )
        .unwrap();
    let job = installer
        .start(
            &tx,
            "cards",
            Limits {
                page_rows: 8,
                page_bytes: 4096,
                pending_rows: 8,
                pending_bytes: 32768,
                total_rows: 128,
                callback_ms: 1000,
                lifetime_ms: 10000,
            },
            0,
        )
        .unwrap();
    let plan = installer
        .capture_plan(&tx, &pos, "native", &["id"])
        .unwrap();
    tx.commit().unwrap();
    (db, installer, plan, job)
}
fn images(plan: &CapturePlan, prefix: &str, side: usize) -> String {
    format!(
        "INSERT INTO images SELECT {},{side},{prefix}.id,{prefix}.body WHERE {};",
        plan.revision_sql(),
        plan.available_sql()
    )
}
fn triggers(db: &Connection, plan: &CapturePlan) {
    db.execute_batch(&format!("CREATE TRIGGER native_insert AFTER INSERT ON native BEGIN {} {} END;
        CREATE TRIGGER native_delete AFTER DELETE ON native BEGIN {} {} END;
        CREATE TRIGGER native_same AFTER UPDATE ON native WHEN OLD.id IS NEW.id BEGIN {} {} {} END;
        CREATE TRIGGER native_rekey AFTER UPDATE ON native WHEN OLD.id IS NOT NEW.id BEGIN {} {} {} {} END;",
        plan.append_sql(Change::Insert),images(plan,"NEW",1),
        plan.append_sql(Change::Delete),images(plan,"OLD",0),
        plan.append_sql(Change::Replacement),images(plan,"OLD",0),images(plan,"NEW",1),
        plan.append_sql(Change::Delete),images(plan,"OLD",0),
        plan.append_sql(Change::Insert),images(plan,"NEW",1))).unwrap();
}
fn journal(db: &Connection) -> Vec<Mutation> {
    db.prepare("SELECT payload FROM ivm_install_journal ORDER BY revision")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
        .collect()
}
fn publish_empty(db: &mut Connection, installer: &Installer, job: &str) {
    db.execute_batch("CREATE TABLE coverage(namespace TEXT PRIMARY KEY,complete INTEGER);")
        .unwrap();
    let tx = db.transaction().unwrap();
    let mut page = installer
        .prepare_scan(
            &tx,
            &smallclaims::ivm::install::ScanPage {
                job: job.into(),
                expected_cursor: vec![],
                next_cursor: vec![],
                position: installer.position(&tx, "native").unwrap(),
                rows: vec![],
                finished: true,
            },
            PublicationLimits::default(),
        )
        .unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    page.upsert("coverage", vec![rusqlite::types::Value::Integer(1)])
        .unwrap();
    page.require_row(
        "coverage",
        vec![],
        vec![("complete".into(), rusqlite::types::Value::Integer(1))],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(
        installer.publish_prepared(&tx, &page, 1).unwrap(),
        smallclaims::ivm::install::Outcome::Progress
    );
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let mut page = installer
        .prepare_catch_up(&tx, job, PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    page.require_row(
        "coverage",
        vec![],
        vec![("complete".into(), rusqlite::types::Value::Integer(1))],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(
        installer.publish_prepared(&tx, &page, 2).unwrap(),
        smallclaims::ivm::install::Outcome::Published
    );
    tx.commit().unwrap();
    assert!(installer.root(db, "cards").is_ok());
}
#[test]
fn pending_and_gap_fence_previously_published_root_without_advancing_applied_prefix() {
    let (mut db, installer, plan, job) = fixture(1);
    triggers(&db, &plan);
    publish_empty(&mut db, &installer, &job);
    let root = installer.root(&db, "cards").unwrap();
    db.execute("INSERT INTO native VALUES('a',X'01')", [])
        .unwrap();
    assert!(installer.root(&db, "cards").is_err());
    let pending = installer.status(&db, "cards").unwrap();
    assert!(pending.source_available && !pending.ready);
    assert_eq!(pending.generation, root.generation);
    db.execute("INSERT INTO native VALUES('b',X'02')", [])
        .unwrap();
    let fenced = installer.status(&db, "cards").unwrap();
    assert!(!fenced.source_available && !fenced.ready && fenced.error.is_some());
    assert_eq!(fenced.generation, root.generation);
    assert_eq!(
        db.query_row("SELECT revision FROM ivm_install_roots", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    db.execute_batch(plan.gap_sql()).unwrap();
    assert_eq!(
        installer.status(&db, "cards").unwrap().status_revision,
        fenced.status_revision
    );
}
#[test]
fn storage_error_rolls_back_native_mutation_reference_and_revision() {
    let (db, installer, plan, _) = fixture(8);
    triggers(&db, &plan);
    db.execute_batch("CREATE TRIGGER image_refuse BEFORE INSERT ON images BEGIN SELECT RAISE(ABORT,'image storage unavailable'); END;").unwrap();
    assert!(
        db.execute("INSERT INTO native VALUES('a',X'01')", [])
            .is_err()
    );
    assert_eq!(installer.position(&db, "native").unwrap().revision, 0);
    assert!(journal(&db).is_empty());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM native", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn exhausted_revision_never_wraps_or_reuses_image_identity() {
    let (db, installer, plan, job) = fixture(8);
    triggers(&db, &plan);
    db.execute(
        "UPDATE ivm_install_sources SET revision=9223372036854775807",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO native VALUES('raw',X'01')", [])
        .unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert!(journal(&db).is_empty());
    assert_eq!(installer.progress(&db, &job).unwrap().phase, "stopped");
    assert_eq!(
        db.query_row("SELECT revision FROM ivm_install_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        i64::MAX
    );
}
#[test]
fn reference_and_retained_byte_quotas_are_measured_in_serialized_utf8_bytes() {
    // Each reference fits, but two cannot fit the retained-byte quota.
    let (db, installer, plan, _) = fixture_limits(8, 180, 4096);
    triggers(&db, &plan);
    db.execute("INSERT INTO native VALUES('💡',X'01')", [])
        .unwrap();
    let first = journal(&db);
    assert_eq!(first.len(), 1);
    let (bytes, actual): (u64, u64) = db
        .query_row(
            "SELECT bytes,length(CAST(payload AS BLOB)) FROM ivm_install_journal",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(bytes, actual);
    db.execute("INSERT INTO native VALUES('second',X'02')", [])
        .unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert_eq!(journal(&db).len(), 1);
    // The individual reference quota can fence an otherwise valid tiny PK.
    let (db, installer, plan, _) = fixture_limits(8, 32768, 80);
    triggers(&db, &plan);
    db.execute("INSERT INTO native VALUES('a',X'01')", [])
        .unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert!(journal(&db).is_empty());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM native", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn native_insert_replace_rekey_delete_retain_exact_revision_images() {
    let (db, installer, plan, job) = fixture(8);
    triggers(&db, &plan);
    db.execute("INSERT INTO native VALUES('amber',?1)", [vec![0u8, 255]])
        .unwrap();
    db.execute("UPDATE native SET body=?1 WHERE id='amber'", [vec![7u8]])
        .unwrap();
    db.execute("UPDATE native SET id='cobalt' WHERE id='amber'", [])
        .unwrap();
    db.execute("DELETE FROM native WHERE id='cobalt'", [])
        .unwrap();
    let rows = journal(&db);
    assert_eq!(rows.len(), 5);
    for (index, row) in rows.iter().enumerate() {
        let revision = index as u64 + 1;
        for (side, reference) in [(0, &row.old), (1, &row.new)] {
            if let Some(reference) = reference {
                assert_eq!(
                    reference,
                    &json!({"table":"native","revision":revision,"side":side})
                );
                let (id, body): (String, Vec<u8>) = db
                    .query_row(
                        "SELECT id,body FROM images WHERE revision=?1 AND side=?2",
                        params![revision, side],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&row.key).unwrap(),
                    json!(["native", [id]])
                );
                assert_eq!(
                    body,
                    if revision == 1 || revision == 2 && side == 0 {
                        vec![0, 255]
                    } else {
                        vec![7]
                    }
                );
            }
        }
    }
    assert!(rows[0].old.is_none() && rows[1].old.is_some() && rows[1].new.is_some());
    assert!(rows[2].new.is_none() && rows[3].old.is_none() && rows[4].new.is_none());
    assert_eq!(installer.position(&db, "native").unwrap().revision, 5);
    assert_eq!(installer.progress(&db, &job).unwrap().queued_rows, 5);
    let (count, bytes): (u64, u64) = db
        .query_row(
            "SELECT journal_rows,journal_bytes FROM ivm_install_sources",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let actual: u64 = db
        .query_row("SELECT SUM(bytes) FROM ivm_install_journal", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!((count, bytes), (5, actual));
}
#[test]
fn source_and_references_rollback_together_then_reopen() {
    let (mut db, installer, plan, job) = fixture(8);
    triggers(&db, &plan);
    let tx = db.transaction().unwrap();
    tx.execute("INSERT INTO native VALUES('rollback',X'01')", [])
        .unwrap();
    tx.rollback().unwrap();
    assert_eq!(installer.position(&db, "native").unwrap().revision, 0);
    assert!(journal(&db).is_empty());
    assert_eq!(installer.progress(&db, &job).unwrap().queued_rows, 0);
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    // A persisted trigger contains no runtime handle. Close and reopen the actual file.
    let temp = tempfile::NamedTempFile::new().unwrap();
    db.execute("VACUUM INTO ?1", [temp.path().to_str().unwrap()])
        .unwrap();
    drop(db);
    let reopened = Connection::open(temp.path()).unwrap();
    reopened
        .execute("INSERT INTO native VALUES('reopen',X'03')", [])
        .unwrap();
    assert_eq!(journal(&reopened).len(), 1);
    assert_eq!(installer.position(&reopened, "native").unwrap().revision, 1);
}
#[test]
fn exhausted_or_invalid_capture_keeps_raw_admission_and_fences_jobs() {
    for value in [None, Some("z".repeat(1025)), Some("💡".repeat(300))] {
        let (db, installer, plan, job) = fixture(8);
        triggers(&db, &plan);
        db.execute("INSERT INTO native VALUES(?1,X'42')", [value])
            .unwrap();
        assert!(installer.position(&db, "native").is_err());
        assert_eq!(installer.progress(&db, &job).unwrap().phase, "stopped");
        assert!(journal(&db).is_empty());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM native", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            1
        );
    }
    let (db, installer, plan, job) = fixture(1);
    triggers(&db, &plan);
    db.execute_batch("INSERT INTO native VALUES('a',X'01');INSERT INTO native VALUES('b',X'02');")
        .unwrap();
    assert_eq!(journal(&db).len(), 1);
    assert!(installer.position(&db, "native").is_err());
    assert_eq!(installer.progress(&db, &job).unwrap().phase, "stopped");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM native", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT revision FROM ivm_install_sources", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn typed_composite_keys_preserve_integer_real_text_and_blob() {
    let (db, installer, _, _) = fixture(8);
    db.execute_batch("CREATE TABLE typed(a,b,c,d,PRIMARY KEY(a,b,c,d));")
        .unwrap();
    let pos = installer.position(&db, "native").unwrap();
    let plan = installer
        .capture_plan(&db, &pos, "typed", &["a", "b", "c", "d"])
        .unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER typed_insert AFTER INSERT ON typed BEGIN {} END;",
        plan.append_sql(Change::Insert)
    ))
    .unwrap();
    db.execute("INSERT INTO typed VALUES(7,7.0,'7',X'00Ff')", [])
        .unwrap();
    let rows = journal(&db);
    let key: serde_json::Value = serde_json::from_str(&rows[0].key).unwrap();
    assert_eq!(key, json!(["typed",[7,7.0,"7",{"$blob":"00FF"}]]));
    assert!(key[1][0].as_i64().is_some());
    assert!(key[1][1].as_i64().is_none());
    // Oversized blob must fence BEFORE its unbounded hex/JSON rendering.
    db.execute("INSERT INTO typed VALUES(8,8.0,'8',zeroblob(1000000))", [])
        .unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert_eq!(journal(&db).len(), 1);
}
#[test]
fn collated_or_type_changed_key_is_not_a_same_key_replacement() {
    for (schema, insert, update) in [
        (
            "CREATE TABLE collated(id TEXT COLLATE NOCASE PRIMARY KEY)",
            "INSERT INTO collated VALUES('amber')",
            "UPDATE collated SET id='AMBER'",
        ),
        (
            "CREATE TABLE collated(id PRIMARY KEY)",
            "INSERT INTO collated VALUES(7)",
            "UPDATE collated SET id=7.0",
        ),
    ] {
        let (db, installer, _, _) = fixture(8);
        db.execute_batch(schema).unwrap();
        db.execute(insert, []).unwrap();
        let plan = installer
            .capture_plan(
                &db,
                &installer.position(&db, "native").unwrap(),
                "collated",
                &["id"],
            )
            .unwrap();
        db.execute_batch(&format!(
            "CREATE TRIGGER replacement AFTER UPDATE ON collated BEGIN {} END;",
            plan.append_sql(Change::Replacement)
        ))
        .unwrap();
        db.execute(update, []).unwrap();
        assert!(installer.position(&db, "native").is_err());
        assert!(journal(&db).is_empty());
    }
}
#[test]
fn immutable_identity_configuration_and_wrong_replacement_are_fenced() {
    for mutation in [
        "UPDATE ivm_install_sources SET fingerprint='other'",
        "UPDATE ivm_install_sources SET epoch=8",
        "UPDATE ivm_install_deferred SET row_limit=4000",
    ] {
        let (db, installer, plan, job) = fixture(8);
        triggers(&db, &plan);
        db.execute(mutation, []).unwrap();
        db.execute("INSERT INTO native VALUES('raw',X'01')", [])
            .unwrap();
        assert!(installer.position(&db, "native").is_err());
        assert!(journal(&db).is_empty());
        assert_eq!(installer.progress(&db, &job).unwrap().phase, "stopped");
    }
    let (db, installer, plan, _) = fixture(8);
    db.execute("INSERT INTO native VALUES('old',X'01')", [])
        .unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER bad_rekey AFTER UPDATE ON native BEGIN {} END;",
        plan.append_sql(Change::Replacement)
    ))
    .unwrap();
    db.execute("UPDATE native SET id='new'", []).unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert!(journal(&db).is_empty());
    assert_eq!(
        db.query_row("SELECT id FROM native", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "new"
    );
}
#[test]
fn descriptor_and_explicit_gap_cannot_create_ready_or_execute_body_sql() {
    let (db, installer, plan, job) = fixture(8);
    let pos = installer.position(&db, "native").unwrap();
    for (table, key) in [
        ("native;DELETE", vec!["id"]),
        ("native", vec!["body"]),
        ("ivm_install_sources", vec!["name"]),
        ("native", vec!["id)"]),
    ] {
        assert!(installer.capture_plan(&db, &pos, table, &key).is_err());
    }
    db.execute_batch("CREATE TABLE composite(a,b,PRIMARY KEY(a,b));")
        .unwrap();
    assert!(
        installer
            .capture_plan(&db, &pos, "composite", &["b", "a"])
            .is_err()
    );
    db.execute_batch(plan.gap_sql()).unwrap();
    assert!(installer.position(&db, "native").is_err());
    assert_eq!(installer.progress(&db, &job).unwrap().phase, "stopped");
    assert_eq!(
        db.query_row("SELECT ready FROM ivm_install_roots", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn queued_native_refs_are_exact_prepared_prefix_not_latest_rows() {
    let (mut db, installer, plan, job) = fixture(8);
    triggers(&db, &plan);
    let tx = db.transaction().unwrap();
    let pos = installer.position(&tx, "native").unwrap();
    let page = installer
        .prepare_scan(
            &tx,
            &smallclaims::ivm::install::ScanPage {
                job: job.clone(),
                expected_cursor: vec![],
                next_cursor: vec![],
                position: pos,
                rows: vec![],
                finished: true,
            },
            PublicationLimits::default(),
        )
        .unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    installer.publish_prepared(&tx, &page, 1).unwrap();
    tx.commit().unwrap();
    db.execute_batch("INSERT INTO native VALUES('a',X'01'); UPDATE native SET body=X'02';")
        .unwrap();
    let tx = db.transaction().unwrap();
    let page = installer
        .prepare_catch_up(&tx, &job, PublicationLimits::default())
        .unwrap();
    assert_eq!(page.position().revision, 2);
    let body: Vec<u8> = tx
        .query_row(
            "SELECT body FROM images WHERE revision=1 AND side=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(body, vec![1]);
    assert_eq!(page.rows()[0].new.as_ref().unwrap()["revision"], json!(1));
    assert!(!installer.status(&db, "cards").unwrap().ready);
}
#[test]
fn installed_native_capture_wakes_same_publisher_only_after_committed_source_revision() {
    use smallclaims::{
        Store,
        ivm::{SourceCut, Views, events},
        store::runtime::Plain,
    };
    use std::sync::Arc;
    use tokio::sync::broadcast::error::TryRecvError;
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let installer = Installer::new(vec![Box::new(Never)]).unwrap();
    let views = Views::new(vec![Box::new(Never)]).unwrap();
    let plan;
    {
        let mut writer = store.connection.write();
        views.create_schema(&writer).unwrap();
        installer.create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        views
            .initialize_empty(
                &tx,
                SourceCut {
                    epoch: 1,
                    admitted: 0,
                    projected: 0,
                    local_generation: 0,
                },
            )
            .unwrap();
        installer
            .register_source(&tx, "native", "native-version-manifest.v1", 7)
            .unwrap();
        let pos = installer.position(&tx, "native").unwrap();
        installer
            .enable_deferred(
                &tx,
                &pos,
                CaptureLimits {
                    rows: 8,
                    bytes: 32768,
                    reference_bytes: 4096,
                },
            )
            .unwrap();
        tx.execute_batch("CREATE TABLE native(id TEXT PRIMARY KEY,body BLOB);
          CREATE TABLE images(revision INTEGER,side INTEGER,id TEXT,body BLOB,PRIMARY KEY(revision,side));").unwrap();
        plan = installer
            .capture_plan(&tx, &pos, "native", &["id"])
            .unwrap();
        events::install(&tx, 64).unwrap();
        views.register_installed(&tx, &installer, "cards").unwrap();
        triggers(&tx, &plan);
        tx.commit().unwrap();
    }
    let publisher = events::Publisher::attach(&store, 8).unwrap();
    let mut notices = publisher.subscribe();
    let before = events::capture(&store.readers.get(), &views, "cards").unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO native VALUES('rolled-back',X'01')", [])
            .unwrap();
        assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
        tx.rollback().unwrap();
    }
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    let rolled_back = events::capture(&store.readers.get(), &views, "cards").unwrap();
    assert_eq!(before.status, rolled_back.status);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO native VALUES('local-only',X'02')", [])
            .unwrap();
        assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
        tx.commit().unwrap();
    }
    assert!(matches!(
        notices.try_recv(),
        Ok(events::Notice::Committed(_))
    ));
    let after = events::capture(&store.readers.get(), &views, "cards").unwrap();
    assert!(after.status.sequence > before.status.sequence);
    assert_eq!(after.keys, before.keys);
    assert_eq!(after.source_cut, before.source_cut);
    assert_eq!(
        installer
            .position(&store.readers.get(), "native")
            .unwrap()
            .revision,
        1
    );
    assert_eq!(journal(&store.readers.get()).len(), 1);
}
