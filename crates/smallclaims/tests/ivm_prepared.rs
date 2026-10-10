//! Actual SQLite lifecycles for queue-only admission and owned off-writer publication.
use anyhow::Result;
use rusqlite::{Connection, Transaction, params, types::Value as Sql};
use serde_json::json;
use smallclaims::ivm::install::{
    Installer, Limits, Mutation, Namespace, Operator, Outcome, ScanPage,
    prepared::{Capture, CaptureLimits, PreparedPage, PublicationLimits},
};
struct Never;
impl Operator for Never {
    fn name(&self) -> &'static str {
        "cards"
    }
    fn source(&self) -> &'static str {
        "native"
    }
    fn fingerprint(&self) -> &'static str {
        "owned-input.v1"
    }
    fn create_schema(&self, db: &Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE cards(namespace TEXT,k TEXT,body TEXT,PRIMARY KEY(namespace,k));
            CREATE TABLE coverage(namespace TEXT PRIMARY KEY,complete INTEGER);
            CREATE TABLE raw(k TEXT PRIMARY KEY,version INTEGER);",
        )?;
        Ok(())
    }
    fn apply(&self, _: &Transaction<'_>, _: &Namespace, _: &[Mutation]) -> Result<bool> {
        panic!("operator must never run on writer")
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        panic!("validation callback must not run on writer")
    }
    fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
        Ok(true)
    }
}
fn fixture() -> (Connection, Installer, String) {
    let mut db = Connection::open_in_memory().unwrap();
    let install = Installer::new(vec![Box::new(Never)]).unwrap();
    install.create_schema(&db).unwrap();
    let tx = db.transaction().unwrap();
    install
        .register_source(&tx, "native", "immutable-native-refs.v1", 7)
        .unwrap();
    let pos = install.position(&tx, "native").unwrap();
    install
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
    let job = install
        .start(
            &tx,
            "cards",
            Limits {
                page_rows: 2,
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
    tx.commit().unwrap();
    (db, install, job)
}
fn scan(db: &mut Connection, i: &Installer, job: &str) -> PreparedPage {
    let tx = db.transaction().unwrap();
    let position = i.position(&tx, "native").unwrap();
    let mut page = i
        .prepare_scan(
            &tx,
            &ScanPage {
                job: job.into(),
                expected_cursor: vec![],
                next_cursor: vec![],
                position,
                rows: vec![],
                finished: true,
            },
            PublicationLimits::default(),
        )
        .unwrap();
    page.capture_table(&tx, "cards").unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    page
}
fn evidence(page: &mut PreparedPage) {
    page.upsert("coverage", vec![Sql::Integer(1)]).unwrap();
    page.require_row(
        "coverage",
        vec![],
        vec![("complete".into(), Sql::Integer(1))],
    )
    .unwrap();
}
fn ready(db: &mut Connection, i: &Installer, job: &str) {
    let page = scan(db, i, job);
    let tx = db.transaction().unwrap();
    assert_eq!(
        i.publish_prepared(&tx, &page, 1).unwrap(),
        Outcome::Progress
    );
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let mut page = i
        .prepare_catch_up(&tx, job, PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    evidence(&mut page);
    let tx = db.transaction().unwrap();
    assert_eq!(
        i.publish_prepared(&tx, &page, 2).unwrap(),
        Outcome::Published
    );
    tx.commit().unwrap();
}
fn admit(db: &mut Connection, i: &Installer, key: &str, version: i64) -> Capture {
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO raw VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET version=excluded.version",
        params![key, version],
    )
    .unwrap();
    let result = i
        .capture_deferred(
            &tx,
            "native",
            &Mutation {
                key: key.into(),
                old: Some(json!({"version":version-1})),
                new: Some(json!({"version":version})),
            },
        )
        .unwrap();
    tx.commit().unwrap();
    result
}
fn live(db: &mut Connection, i: &Installer) -> PreparedPage {
    let tx = db.transaction().unwrap();
    let mut page = i
        .prepare_live(&tx, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "cards").unwrap();
    tx.commit().unwrap();
    page
}
#[test]
fn queue_only_admission_owned_computation_and_newer_pending_prefix() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    assert!(matches!(admit(&mut db, &i, "amber", 1), Capture::Queued(_)));
    assert!(!i.status(&db, "cards").unwrap().ready);
    let mut page = live(&mut db, &i);
    assert_eq!(page.rows().len(), 1);
    // No DB borrow survives; native writes can proceed while pure computation runs.
    admit(&mut db, &i, "amber", 2);
    assert_eq!(page.rows()[0].old, Some(json!({"version":0})));
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("v1".into())],
    )
    .unwrap();
    page.mark_visible_change();
    let tx = db.transaction().unwrap();
    i.publish_prepared(&tx, &page, 3).unwrap();
    tx.commit().unwrap();
    assert!(!i.status(&db, "cards").unwrap().ready);
    let mut page = live(&mut db, &i);
    assert_eq!(page.position().revision, 2);
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("v2".into())],
    )
    .unwrap();
    page.mark_visible_change();
    let tx = db.transaction().unwrap();
    i.publish_prepared(&tx, &page, 4).unwrap();
    tx.commit().unwrap();
    let root = i.root(&db, "cards").unwrap();
    assert_eq!(root.revision, 2);
    assert_eq!(
        db.query_row(
            "SELECT body FROM cards WHERE namespace=?1 AND k='amber'",
            [root.namespace.as_str()],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "v2"
    );
    let tx = db.transaction().unwrap();
    assert!(
        i.record(
            &tx,
            "native",
            &Mutation {
                key: "x".into(),
                old: None,
                new: None
            }
        )
        .is_err()
    );
    assert!(i.catch_up(&tx, &job, 4).is_err());
}
#[test]
fn point_absence_tombstone_and_read_conflict_roll_back_progress() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    admit(&mut db, &i, "amber", 1);
    let tx = db.transaction().unwrap();
    let mut page = i
        .prepare_live(&tx, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "cards").unwrap();
    assert_eq!(
        page.observe_row(
            &tx,
            "cards",
            vec![Sql::Text("amber".into())],
            vec!["body".into()]
        )
        .unwrap(),
        None
    );
    tx.commit().unwrap();
    let ns = page.namespace().as_str().to_string();
    db.execute("INSERT INTO cards VALUES(?1,'amber','conflicting')", [&ns])
        .unwrap();
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("prepared".into())],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    assert!(i.publish_prepared(&tx, &page, 3).is_err());
    tx.commit().unwrap();
    assert!(!i.status(&db, "cards").unwrap().ready);
    let mut page = live(&mut db, &i);
    page.delete("cards", vec![Sql::Text("amber".into())])
        .unwrap();
    page.mark_visible_change();
    let tx = db.transaction().unwrap();
    i.publish_prepared(&tx, &page, 4).unwrap();
    tx.commit().unwrap();
    assert!(i.status(&db, "cards").unwrap().ready);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM cards WHERE namespace=?1",
            [&ns],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}
#[test]
fn failed_complete_evidence_rolls_back_rows_and_job_and_can_retry() {
    let (mut db, i, job) = fixture();
    let page = scan(&mut db, &i, &job);
    let tx = db.transaction().unwrap();
    i.publish_prepared(&tx, &page, 1).unwrap();
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    let mut page = i
        .prepare_catch_up(&tx, &job, PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    page.upsert("coverage", vec![Sql::Integer(0)]).unwrap();
    page.require_row(
        "coverage",
        vec![],
        vec![("complete".into(), Sql::Integer(1))],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    assert!(i.publish_prepared(&tx, &page, 2).is_err());
    tx.commit().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM coverage", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(i.progress(&db, &job).unwrap().phase, "catchup");
    assert!(!i.status(&db, "cards").unwrap().ready);
    let tx = db.transaction().unwrap();
    let mut page = i
        .prepare_catch_up(&tx, &job, PublicationLimits::default())
        .unwrap();
    page.capture_table(&tx, "coverage").unwrap();
    tx.commit().unwrap();
    evidence(&mut page);
    let tx = db.transaction().unwrap();
    assert_eq!(
        i.publish_prepared(&tx, &page, 3).unwrap(),
        Outcome::Published
    );
    tx.commit().unwrap();
}
#[test]
fn queue_exhaustion_preserves_raw_admission_and_queue_rollback_is_atomic() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    let tx = db.transaction().unwrap();
    i.capture_deferred(
        &tx,
        "native",
        &Mutation {
            key: "rolled-back".into(),
            old: None,
            new: None,
        },
    )
    .unwrap();
    drop(tx);
    assert_eq!(i.position(&db, "native").unwrap().revision, 0);
    for version in 1..=8 {
        assert!(matches!(
            admit(&mut db, &i, "amber", version),
            Capture::Queued(_)
        ));
    }
    assert!(matches!(admit(&mut db, &i, "amber", 9), Capture::Fenced(_)));
    assert_eq!(
        db.query_row("SELECT version FROM raw WHERE k='amber'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        9
    );
    assert!(!i.status(&db, "cards").unwrap().source_available);
}
#[test]
fn unconsumed_delete_and_rewrite_fence_but_consumed_pruning_is_safe() {
    for rewrite in [false, true] {
        let (mut db, i, job) = fixture();
        ready(&mut db, &i, &job);
        admit(&mut db, &i, "amber", 1);
        db.execute(
            if rewrite {
                "UPDATE ivm_install_journal SET payload='{}'"
            } else {
                "DELETE FROM ivm_install_journal"
            },
            [],
        )
        .unwrap();
        assert!(!i.status(&db, "cards").unwrap().source_available);
        assert!(
            i.prepare_live(&db, "cards", PublicationLimits::default())
                .is_err()
        );
    }
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    admit(&mut db, &i, "amber", 1);
    let page = live(&mut db, &i);
    let tx = db.transaction().unwrap();
    i.publish_prepared(&tx, &page, 3).unwrap();
    i.prune_journal(&tx, "native", 8).unwrap();
    tx.commit().unwrap();
    assert!(i.status(&db, "cards").unwrap().ready);
}
#[test]
fn schema_changes_hidden_fanout_and_bounds_are_refused() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    admit(&mut db, &i, "amber", 1);
    let mut page = live(&mut db, &i);
    assert!(
        page.upsert("cards", vec![Sql::Null, Sql::Text("bad".into())])
            .is_err()
    );
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("x".repeat(65537))]
        )
        .is_err()
    );
    db.execute_batch("CREATE TABLE schema_change(x)").unwrap();
    let tx = db.transaction().unwrap();
    assert!(i.publish_prepared(&tx, &page, 3).is_err());
    tx.commit().unwrap();
    db.execute_batch("CREATE TRIGGER fanout AFTER INSERT ON cards BEGIN INSERT INTO schema_change VALUES(1); END;").unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    assert!(page.capture_table(&db, "cards").is_err());
    db.execute_batch("DROP TRIGGER fanout; CREATE TABLE child(namespace TEXT,k TEXT,FOREIGN KEY(namespace,k) REFERENCES cards(namespace,k) ON DELETE CASCADE)").unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    assert!(page.capture_table(&db, "cards").is_err());
}

#[test]
fn grouped_metadata_publication_matches_single_table_wrapper() {
    for grouped in [false, true] {
        let (mut db, i, job) = fixture();
        ready(&mut db, &i, &job);
        admit(&mut db, &i, "amber", 1);
        let mut page = {
            let tx = db.transaction().unwrap();
            let mut page = i
                .prepare_live(&tx, "cards", PublicationLimits::default())
                .unwrap();
            if grouped {
                page.capture_tables(&tx, &["cards", "coverage"]).unwrap();
            } else {
                page.capture_table(&tx, "cards").unwrap();
                page.capture_table(&tx, "coverage").unwrap();
            }
            // Recapturing an already captured table retains the old one-table semantics.
            page.capture_table(&tx, "cards").unwrap();
            tx.commit().unwrap();
            page
        };
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("v1".into())],
        )
        .unwrap();
        evidence(&mut page);
        let tx = db.transaction().unwrap();
        i.publish_prepared(&tx, &page, 3).unwrap();
        tx.commit().unwrap();
        let root = i.root(&db, "cards").unwrap();
        assert_eq!(
            db.query_row(
                "SELECT k,body FROM cards WHERE namespace=?1",
                [root.namespace.as_str()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            ("amber".to_owned(), "v1".to_owned())
        );
        assert_eq!(
            db.query_row(
                "SELECT complete FROM coverage WHERE namespace=?1",
                [root.namespace.as_str()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert!(i.status(&db, "cards").unwrap().ready);
    }
}

#[test]
fn grouped_metadata_checks_request_union_before_discovery_and_keeps_existing_metadata() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    let mut page = i
        .prepare_live(
            &db,
            "cards",
            PublicationLimits {
                tables: 1,
                ..PublicationLimits::default()
            },
        )
        .unwrap();
    page.capture_table(&db, "cards").unwrap();
    let error = page.capture_tables(&db, &["absent_output"]).unwrap_err();
    assert!(
        format!("{error:#}").contains("prepared table bound exceeded"),
        "reject existing+requested table union before attempting absent table discovery"
    );
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("v1".into())],
    )
    .unwrap();
    db.execute_batch("CREATE TABLE side_effect(k); CREATE TRIGGER hidden_fanout AFTER INSERT ON coverage BEGIN INSERT INTO side_effect VALUES(1); END").unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_table(&db, "cards").unwrap();
    assert!(page.capture_tables(&db, &["cards", "coverage"]).is_err());
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("v1".into())],
    )
    .unwrap();
    assert!(page.upsert("coverage", vec![Sql::Integer(1)]).is_err());
}

#[test]
fn grouped_metadata_rejects_incoming_outgoing_and_case_variant_foreign_keys() {
    for ddl in [
        // Incoming composite edge and case-insensitive target name.
        "CREATE TABLE child(namespace TEXT,k TEXT,FOREIGN KEY(namespace,k) REFERENCES CaRdS(namespace,k) ON DELETE CASCADE)",
        // Outgoing edge to an unrelated parent is also unsupported.
        "CREATE TABLE parent(k TEXT PRIMARY KEY); DROP TABLE coverage;
         CREATE TABLE coverage(namespace TEXT PRIMARY KEY REFERENCES parent(k),complete INTEGER)",
        // An incoming edge touching the second requested output cannot be missed.
        "CREATE TABLE child(namespace TEXT REFERENCES coverage(namespace))",
    ] {
        let (mut db, i, job) = fixture();
        ready(&mut db, &i, &job);
        db.execute_batch(ddl).unwrap();
        let mut page = i
            .prepare_live(&db, "cards", PublicationLimits::default())
            .unwrap();
        let error = page
            .capture_tables(&db, &["cards", "coverage"])
            .unwrap_err();
        assert!(format!("{error:#}").contains("prepared output foreign keys unsupported"));
        assert!(
            page.upsert(
                "cards",
                vec![Sql::Text("amber".into()), Sql::Text("body".into())]
            )
            .is_err(),
            "a rejected group must not accept its first table's metadata"
        );
        let mut single = i
            .prepare_live(&db, "cards", PublicationLimits::default())
            .unwrap();
        let affected = if ddl.contains("REFERENCES CaRdS") {
            "cards"
        } else {
            "coverage"
        };
        assert!(
            single.capture_table(&db, affected).is_err(),
            "single-table wrapper retains fanout refusal"
        );
    }
}

#[test]
fn grouped_metadata_main_foreign_key_inventory_is_not_hidden_by_temp_shadow() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    db.execute_batch("CREATE TABLE main.child(namespace TEXT,k TEXT,FOREIGN KEY(namespace,k) REFERENCES cards(namespace,k) ON DELETE CASCADE);
        CREATE TEMP TABLE child(k INTEGER)").unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('child','main')",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('child','temp')",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    let error = page
        .capture_tables(&db, &["cards", "coverage"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared output foreign keys unsupported"));
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
}

#[test]
fn grouped_metadata_accepts_exact_schema_table_cap_and_refuses_one_over() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    let existing: usize = db
        .query_row(
            "SELECT COUNT(*) FROM main.sqlite_schema WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(existing < 256);
    for n in existing..256 {
        db.execute_batch(&format!("CREATE TABLE main.inventory_{n}(k INTEGER)"))
            .unwrap();
    }
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM main.sqlite_schema WHERE type='table'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        256
    );
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_tables(&db, &["cards", "coverage"]).unwrap();
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("body".into())],
    )
    .unwrap();
    db.execute_batch("CREATE TABLE main.one_table_over(k INTEGER)")
        .unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    let error = page
        .capture_tables(&db, &["cards", "coverage"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared schema inventory exceeds bound"));
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
}

#[test]
fn grouped_metadata_accepts_exact_fk_row_cap_and_refuses_one_over() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    db.execute_batch("CREATE TABLE main.other_parent(k INTEGER PRIMARY KEY)")
        .unwrap();
    let columns = |count| {
        (0..count)
            .map(|n| format!("k{n} INTEGER REFERENCES other_parent(k)"))
            .collect::<Vec<_>>()
            .join(",")
    };
    db.execute_batch(&format!("CREATE TABLE main.fk_inventory({})", columns(128)))
        .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('fk_inventory','main')",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        128
    );
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_tables(&db, &["cards", "coverage"]).unwrap();
    db.execute_batch(&format!(
        "DROP TABLE main.fk_inventory; CREATE TABLE main.fk_inventory({})",
        columns(129)
    ))
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('fk_inventory','main')",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        129
    );
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    let error = page
        .capture_tables(&db, &["cards", "coverage"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared foreign-key inventory exceeds bound"));
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
}

#[test]
fn grouped_metadata_refuses_unsupported_second_table_without_partial_acceptance() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    db.execute_batch("CREATE TABLE side_effect(k); CREATE TRIGGER hidden_fanout AFTER INSERT ON coverage BEGIN INSERT INTO side_effect VALUES(1); END").unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    assert!(page.capture_tables(&db, &[]).is_err());
    assert!(page.capture_tables(&db, &["cards", "cards"]).is_err());
    let error = page
        .capture_tables(&db, &["cards", "coverage"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared output triggers unsupported"));
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
    // The original one-table API still accepts the valid sibling, never the rejected group.
    page.capture_table(&db, "cards").unwrap();
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("body".into())],
    )
    .unwrap();
}

#[test]
fn grouped_shape_accepts_exact_column_cap_and_refuses_one_over_atomically() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    let columns = |count: usize| {
        let mut columns = vec!["namespace TEXT PRIMARY KEY".to_owned()];
        columns.extend((1..count).map(|n| format!("c{n} TEXT")));
        columns.join(",")
    };
    db.execute_batch(&format!("CREATE TABLE main.wide_output({})", columns(128)))
        .unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_tables(&db, &["cards", "wide_output"]).unwrap();
    page.upsert("wide_output", vec![Sql::Null; 127]).unwrap();
    db.execute_batch(&format!(
        "DROP TABLE main.wide_output; CREATE TABLE main.wide_output({})",
        columns(129)
    ))
    .unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    let error = page
        .capture_tables(&db, &["cards", "wide_output"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared generated/oversized table unsupported"));
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
    assert!(page.upsert("wide_output", vec![Sql::Null; 128]).is_err());
    page.capture_table(&db, "cards").unwrap();
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("body".into())],
    )
    .unwrap();
}

#[test]
fn grouped_shape_reads_main_columns_and_refuses_generated_or_missing_sibling() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    db.execute_batch("CREATE TEMP TABLE cards(unrelated INTEGER); CREATE TEMP VIEW coverage AS SELECT 1 AS other").unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_tables(&db, &["cards", "coverage"]).unwrap();
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("main body".into())],
    )
    .unwrap();
    page.upsert("coverage", vec![Sql::Integer(1)]).unwrap();
    db.execute_batch("CREATE TABLE main.generated_output(namespace TEXT PRIMARY KEY,k TEXT,g TEXT GENERATED ALWAYS AS (k) VIRTUAL)").unwrap();
    for sibling in ["generated_output", "absent_output"] {
        let mut page = i
            .prepare_live(&db, "cards", PublicationLimits::default())
            .unwrap();
        assert!(page.capture_tables(&db, &["cards", sibling]).is_err());
        assert!(
            page.upsert(
                "cards",
                vec![Sql::Text("amber".into()), Sql::Text("body".into())]
            )
            .is_err()
        );
    }
}

#[test]
fn grouped_metadata_final_schema_check_rejects_changed_cut_without_accepting_sibling() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_table(&db, "cards").unwrap();
    db.execute_batch("CREATE TABLE main.later_namespace(k INTEGER)")
        .unwrap();
    let error = page
        .capture_tables(&db, &["cards", "coverage"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared table schema changed"));
    assert!(page.upsert("coverage", vec![Sql::Integer(1)]).is_err());
    // Prior metadata remains owned, but its old schema cut can never publish now.
    page.upsert(
        "cards",
        vec![Sql::Text("amber".into()), Sql::Text("stale body".into())],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    let error = i.publish_prepared(&tx, &page, 3).unwrap_err();
    assert!(format!("{error:#}").contains("prepared publication schema changed"));
    tx.commit().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM main.cards", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_worker_preserves_notice_during_owned_page_and_propagates_failure() {
    use smallclaims::ivm::{
        SourceCut,
        asynchronous::{PageReport, PageStatus, Worker},
        events::Notice,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (notices, receiver) = tokio::sync::broadcast::channel(4);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (entered, started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let released = std::sync::Mutex::new(released);
    let worker = Worker::start_pages(
        Arc::new(move || {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
            Ok(PageReport {
                status: PageStatus::CaughtUp,
                source_cut: SourceCut {
                    epoch: 1,
                    admitted: 0,
                    projected: 0,
                    local_generation: 0,
                },
                rows: 0,
                bytes: 0,
                retained_rows: 0,
                retained_bytes: 0,
                writer_wait_us: 0,
                writer_held_us: 0,
            })
        }),
        receiver,
    )
    .unwrap();
    tokio::task::spawn_blocking(move || started.recv().unwrap())
        .await
        .unwrap();
    // A commit arriving while a page executes must remain pending after CaughtUp.
    notices
        .send(Notice::Unavailable("recheck source only".into()))
        .unwrap();
    release.send(()).unwrap();
    let mut progress = worker.progress();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while calls.load(Ordering::SeqCst) < 2 {
            progress.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    worker.stop().await.unwrap();
    let (_, receiver) = tokio::sync::broadcast::channel(4);
    let worker = Worker::start_pages(
        Arc::new(|| anyhow::bail!("native preparation failed")),
        receiver,
    )
    .unwrap();
    let mut progress = worker.progress();
    tokio::time::timeout(std::time::Duration::from_secs(3), progress.changed())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        worker
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("native preparation failed")
    );
}

#[test]
fn canonical_image_keeps_unrelated_shapes_and_complete_inbound_refusal() {
    let (mut db, i, job) = fixture();
    ready(&mut db, &i, &job);
    // Only requested outputs need namespace/PK/column/trigger eligibility. This
    // unrelated generated shape and trigger must not become new API refusals.
    db.execute_batch(
        "CREATE TABLE main.unrelated_shape(\"bad-name\" TEXT,
           computed TEXT GENERATED ALWAYS AS (\"bad-name\") VIRTUAL);
         CREATE TRIGGER main.unrelated_trigger AFTER INSERT ON main.unrelated_shape
           BEGIN UPDATE unrelated_shape SET \"bad-name\"='unrelated'; END;",
    )
    .unwrap();
    for grouped in [false, true] {
        let mut page = i
            .prepare_live(&db, "cards", PublicationLimits::default())
            .unwrap();
        if grouped {
            page.capture_tables(&db, &["cards", "coverage"]).unwrap();
        } else {
            page.capture_table(&db, "cards").unwrap();
            page.capture_table(&db, "coverage").unwrap();
        }
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())],
        )
        .unwrap();
        page.upsert("coverage", vec![Sql::Integer(1)]).unwrap();
    }
    db.execute_batch(
        "CREATE TABLE main.unrelated_inbound(namespace TEXT,k TEXT,
           FOREIGN KEY(namespace,k) REFERENCES CaRdS(namespace,k));",
    )
    .unwrap();
    let mut page = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    page.capture_table(&db, "coverage").unwrap();
    let error = page
        .capture_tables(&db, &["coverage", "cards"])
        .unwrap_err();
    assert!(format!("{error:#}").contains("prepared output foreign keys unsupported"));
    // Failed image construction cannot lose existing metadata or accept a sibling.
    page.upsert("coverage", vec![Sql::Integer(1)]).unwrap();
    assert!(
        page.upsert(
            "cards",
            vec![Sql::Text("amber".into()), Sql::Text("body".into())]
        )
        .is_err()
    );
    let mut single = i
        .prepare_live(&db, "cards", PublicationLimits::default())
        .unwrap();
    let error = single.capture_table(&db, "cards").unwrap_err();
    assert!(format!("{error:#}").contains("prepared output foreign keys unsupported"));
}
