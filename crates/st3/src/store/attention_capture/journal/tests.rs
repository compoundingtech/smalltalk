use super::*;
use serde_json::json;
use smallclaims::ivm::install::prepared::CaptureLimits;

const TABLES: &[Table] = &[Table {
    name: "native",
    columns: &["id", "body", "number"],
    key: &["id"],
}];
fn empty_fixture() -> Result<(Connection, Installer, NativeCapture)> {
    let mut c = Connection::open_in_memory()?;
    c.execute_batch("PRAGMA recursive_triggers=ON; CREATE TABLE main.native(id TEXT PRIMARY KEY,body BLOB UNIQUE,number INTEGER)")?;
    let installer = Installer::new(vec![])?;
    installer.create_schema(&c)?;
    let tx = c.transaction()?;
    installer.register_source(&tx, "attention-native", "journal-fixture.v1", 7)?;
    let position = installer.position(&tx, "attention-native")?;
    installer.enable_deferred(
        &tx,
        &position,
        CaptureLimits {
            rows: 64,
            bytes: 1024 * 1024,
            reference_bytes: 4096,
        },
    )?;
    let capture = NativeCapture::install(&tx, &installer, &position, TABLES)?;
    tx.commit()?;
    Ok((c, installer, capture))
}
fn admit(c: &mut Connection, installer: &Installer, capture: &NativeCapture) -> Result<()> {
    let tx = c.transaction()?;
    capture.begin(&tx, installer)?;
    tx.execute("INSERT INTO main.native VALUES('a',X'00FF',1)", [])?;
    tx.execute("UPDATE main.native SET number=2 WHERE id='a'", [])?;
    tx.execute("UPDATE main.native SET id='b' WHERE id='a'", [])?;
    capture.commit(&tx)?;
    tx.commit()?;
    Ok(())
}
fn fixture() -> Result<(Connection, NativeCapture, SourcePosition, Vec<Mutation>)> {
    let (mut c, installer, capture) = empty_fixture()?;
    admit(&mut c, &installer, &capture)?;
    let snapshot = installer.position(&c, "attention-native")?;
    assert_eq!(snapshot.revision, 4);
    let rows = c.prepare("SELECT payload FROM main.ivm_install_journal WHERE source='attention-native' ORDER BY revision LIMIT 5")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter().map(|s| serde_json::from_str::<Mutation>(&s))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(rows.len(), 4);
    Ok((c, capture, snapshot, rows))
}
fn budget() -> Budget {
    Budget {
        rows: ROWS,
        bytes: BYTES,
    }
}
fn prefix(snapshot: &SourcePosition, revision: u64) -> SourcePosition {
    let mut prefix = snapshot.clone();
    prefix.revision = revision;
    prefix
}
fn reader(c: &mut Connection) -> Result<rusqlite::Transaction<'_>> {
    let tx = c.transaction()?;
    tx.query_row(
        "SELECT revision FROM main.ivm_install_sources WHERE name='attention-native'",
        [],
        |r| r.get::<_, u64>(0),
    )?;
    Ok(tx)
}

#[test]
fn owned_prefixes_keep_rekey_sides_and_decode_after_connection_drop() -> Result<()> {
    let (mut c, capture, snapshot, rows) = fixture()?;
    c.execute_batch("PRAGMA query_only=ON")?;
    let before = c.total_changes();
    let tx = reader(&mut c)?;
    let first =
        capture.capture_journal(&tx, &prefix(&snapshot, 2), &snapshot, &rows[..2], budget())?;
    let second = capture.capture_journal(&tx, &snapshot, &snapshot, &rows[2..], budget())?;
    tx.rollback()?;
    assert_eq!(c.total_changes(), before);
    drop(c);
    let first = first.decode()?;
    let second = second.decode()?;
    assert_eq!(first.position.revision, 2);
    assert_eq!(first.snapshot_position, snapshot);
    assert_eq!(second.position, snapshot);
    assert_eq!(first.changes.len(), 2);
    assert!(first.changes[0].old.is_none());
    assert_eq!(first.changes[0].new, first.changes[1].old);
    let updated = first.changes[1].new.as_ref().unwrap();
    assert_eq!(updated["body"], Cell::Blob(vec![0, 255]));
    assert_eq!(updated["number"], Cell::Integer(2));
    assert_eq!(second.changes.len(), 2);
    assert_eq!(second.changes[0].old.as_ref().unwrap(), updated);
    assert!(second.changes[0].new.is_none());
    assert!(second.changes[1].old.is_none());
    assert_eq!(second.changes[0].revision, 3);
    assert_eq!(second.changes[1].revision, 4);
    assert_eq!(
        second.changes[1].new.as_ref().unwrap()["id"],
        Cell::Text("b".into())
    );
    assert_eq!(
        serde_json::from_str::<Value>(&second.changes[0].key)?,
        json!(["native", ["a"]])
    );
    assert_eq!(
        serde_json::from_str::<Value>(&second.changes[1].key)?,
        json!(["native", ["b"]])
    );
    for page in [&first, &second] {
        assert!(page.work_rows <= ROWS);
        assert!(page.work_bytes <= BYTES);
    }
    Ok(())
}

#[test]
fn cut_locator_identity_and_budget_refusals_do_not_mutate_or_repair() -> Result<()> {
    let (mut c, capture, snapshot, rows) = fixture()?;
    assert!(
        capture
            .capture_journal(&c, &snapshot, &snapshot, &rows[2..], budget())
            .is_err()
    );
    for behavior in [
        rusqlite::TransactionBehavior::Deferred,
        rusqlite::TransactionBehavior::Immediate,
    ] {
        let tx = c.transaction_with_behavior(behavior)?;
        assert!(
            capture
                .capture_journal(&tx, &snapshot, &snapshot, &rows[2..], budget())
                .is_err()
        );
        tx.rollback()?;
    }
    c.execute_batch("PRAGMA query_only=ON")?;
    let before = c.total_changes();
    let tx = reader(&mut c)?;
    assert!(
        capture
            .capture_journal(&tx, &prefix(&snapshot, 0), &snapshot, &[], budget())
            .is_err()
    );
    assert!(
        capture
            .capture_journal(&tx, &prefix(&snapshot, 1), &snapshot, &rows[..2], budget())
            .is_err()
    );
    assert!(
        capture
            .capture_journal(&tx, &prefix(&snapshot, 3), &snapshot, &rows[..2], budget())
            .is_err()
    );
    let mut changed = snapshot.clone();
    changed.epoch += 1;
    assert!(
        capture
            .capture_journal(&tx, &changed, &snapshot, &rows[2..], budget())
            .is_err()
    );
    let mut changed = snapshot.clone();
    changed.revision += 1;
    assert!(
        capture
            .capture_journal(&tx, &changed, &changed, &[], budget())
            .is_err()
    );
    for locator in [
        json!({"table":"native", "revision":1, "side":0}),
        json!({"table":"native", "revision":2, "side":1}),
        json!({"table":"other", "revision":1, "side":1}),
        json!({"table":"native", "revision":1, "side":1, "extra":0}),
    ] {
        let mut changed = rows[0].clone();
        changed.new = Some(locator);
        assert!(
            capture
                .capture_journal(&tx, &prefix(&snapshot, 1), &snapshot, &[changed], budget())
                .is_err()
        );
    }
    for budget in [
        Budget {
            rows: 4,
            bytes: BYTES,
        },
        Budget {
            rows: ROWS,
            bytes: 1,
        },
    ] {
        assert!(
            capture
                .capture_journal(&tx, &prefix(&snapshot, 2), &snapshot, &rows[..2], budget)
                .is_err()
        );
    }
    assert!(
        capture
            .capture_journal(&tx, &snapshot, &snapshot, &[], budget())?
            .decode()?
            .changes
            .is_empty()
    );
    tx.rollback()?;
    assert_eq!(c.total_changes(), before);
    Ok(())
}

#[test]
fn owned_decode_refuses_full_key_storage_class_layout_and_image_substitution() -> Result<()> {
    let (mut c, capture, snapshot, rows) = fixture()?;
    let tx = reader(&mut c)?;
    for key in [
        json!(["native", ["other"]]),
        json!(["native", []]),
        json!(["native", [1]]),
    ] {
        let mut row = rows[0].clone();
        row.key = serde_json::to_string(&key)?;
        let batch =
            capture.capture_journal(&tx, &prefix(&snapshot, 1), &snapshot, &[row], budget())?;
        assert!(batch.decode().is_err());
    }
    for corrupt in 0..4 {
        let mut batch =
            capture.capture_journal(&tx, &prefix(&snapshot, 1), &snapshot, &rows[..1], budget())?;
        match corrupt {
            0 => batch.images[0].revision += 1,
            1 => batch.images[0].side = 0,
            2 => {
                let mut body: Value = serde_json::from_str(&batch.images[0].body)?;
                body.as_object_mut().unwrap().remove("number");
                batch.images[0].body = serde_json::to_string(&body)?;
            }
            _ => {
                let mut body: Value = serde_json::from_str(&batch.images[0].body)?;
                body["number"] = json!(["real", 1.25]);
                batch.images[0].body = serde_json::to_string(&body)?;
            }
        }
        assert!(batch.decode().is_err());
    }
    assert!(key_matches(
        &json!({"$blob":"00FF"}),
        &Cell::Blob(vec![0, 255])
    ));
    assert!(!key_matches(
        &json!({"$blob":"00ff"}),
        &Cell::Blob(vec![0, 255])
    ));
    assert!(key_matches(&json!(i64::MAX), &Cell::Integer(i64::MAX)));
    assert!(!key_matches(&json!(u64::MAX), &Cell::Integer(-1)));
    assert!(!key_matches(&json!("1"), &Cell::Integer(1)));
    assert!(!key_matches(&json!(1.25), &Cell::Real(1.25)));
    assert!(!key_matches(&Value::Null, &Cell::Null));
    tx.rollback()?;
    Ok(())
}

#[test]
fn public_journal_bridge_accepts_real_installer_prefix_without_certifying_publication() -> Result<()>
{
    use smallclaims::ivm::install::{
        Limits, Namespace, Operator, ScanPage, prepared::PublicationLimits,
    };
    struct Probe;
    impl Operator for Probe {
        fn name(&self) -> &'static str {
            "attention-journal-fixture"
        }
        fn fingerprint(&self) -> &'static str {
            "attention-journal-fixture.v1"
        }
        fn source(&self) -> &'static str {
            "attention-native"
        }
        fn create_schema(&self, _c: &Connection) -> Result<()> {
            Ok(())
        }
        fn apply(
            &self,
            _tx: &Transaction<'_>,
            _ns: &Namespace,
            _rows: &[Mutation],
        ) -> Result<bool> {
            anyhow::bail!("fixture must not invoke Operator::apply")
        }
        fn validate_publication(&self, _tx: &Transaction<'_>, _ns: &Namespace) -> Result<()> {
            anyhow::bail!("fixture does not certify publication")
        }
        fn reclaim(&self, _tx: &Transaction<'_>, _ns: &Namespace, _rows: usize) -> Result<bool> {
            anyhow::bail!("fixture does not reclaim")
        }
    }
    let (mut c, _, capture) = empty_fixture()?;
    let installer = Installer::new(vec![Box::new(Probe)])?;
    installer.create_schema(&c)?;
    let tx = c.transaction()?;
    let job = installer.start(
        &tx,
        "attention-journal-fixture",
        Limits {
            page_rows: 2,
            page_bytes: 4096,
            pending_rows: 64,
            pending_bytes: 1024 * 1024,
            total_rows: 100,
            callback_ms: 1000,
            lifetime_ms: 60_000,
        },
        0,
    )?;
    tx.commit()?;
    let tx = reader(&mut c)?;
    let scan = installer.prepare_scan(
        &tx,
        &ScanPage {
            job: job.clone(),
            expected_cursor: vec![],
            next_cursor: vec![],
            position: installer.position(&tx, "attention-native")?,
            rows: vec![],
            finished: true,
        },
        PublicationLimits {
            rows: 2,
            bytes: 4096,
            tables: 1,
        },
    )?;
    tx.rollback()?;
    let tx = c.transaction()?;
    installer.publish_prepared(&tx, &scan, 0)?;
    tx.commit()?;
    admit(&mut c, &installer, &capture)?;
    c.execute_batch("PRAGMA query_only=ON")?;
    let before = c.total_changes();
    let tx = reader(&mut c)?;
    let page = installer.prepare_catch_up(
        &tx,
        &job,
        PublicationLimits {
            rows: 2,
            bytes: 4096,
            tables: 1,
        },
    )?;
    assert_eq!(page.position().revision, 2);
    assert_eq!(page.snapshot_position().revision, 4);
    let batch = capture.journal_batch(&tx, &page, budget())?;
    tx.rollback()?;
    assert_eq!(c.total_changes(), before);
    drop(c);
    let bound = batch.decode()?;
    assert_eq!(bound.position.revision, 2);
    assert_eq!(bound.snapshot_position.revision, 4);
    assert_eq!(bound.changes.len(), 2);
    assert_eq!(bound.changes[0].revision, 1);
    assert_eq!(bound.changes[1].revision, 2);
    assert!(bound.changes[1].old.is_some() && bound.changes[1].new.is_some());
    Ok(())
}
