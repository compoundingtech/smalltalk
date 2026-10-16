//! These controls are absent from non-test builds and never open a production Store.
use super::super::operator_error::{Cell, Disposition, Origin, classify};
use super::*;
use rusqlite::{ErrorCode, ffi};

fn sqlite(code: i32) -> anyhow::Error {
    rusqlite::Error::SqliteFailure(
        ffi::Error::new(code),
        Some("private classifier input".into()),
    )
    .into()
}

#[test]
fn managed_operator_error_typed_primary_extended_and_unknown_policy() {
    // Synthetic code classification, not an assertion that each I/O failure occurred.
    for code in [
        ffi::SQLITE_BUSY,
        ffi::SQLITE_LOCKED,
        ffi::SQLITE_NOMEM,
        ffi::SQLITE_READONLY,
        ffi::SQLITE_INTERRUPT,
        ffi::SQLITE_IOERR,
        ffi::SQLITE_CORRUPT,
        ffi::SQLITE_FULL,
        ffi::SQLITE_CANTOPEN,
        ffi::SQLITE_NOTADB,
        ffi::SQLITE_IOERR_READ,
        ffi::SQLITE_BUSY_SNAPSHOT,
        ffi::SQLITE_READONLY_DBMOVED,
    ] {
        let original = sqlite(code).context("original operator context");
        assert_eq!(classify(&original), Disposition::Infrastructure);
        let cell = Cell::default();
        assert!(cell.preserve(original, Origin::Witness).unwrap().is_none());
        let retained = cell.take().unwrap().unwrap();
        assert_eq!(
            retained
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error()
                .unwrap()
                .extended_code,
            code
        );
        assert!(format!("{retained:#}").contains("original operator context"));
        assert!(cell.take().unwrap().is_none());
    }
    for code in [
        ffi::SQLITE_CONSTRAINT,
        ffi::SQLITE_CONSTRAINT_TRIGGER,
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
        ffi::SQLITE_CONSTRAINT_UNIQUE,
        ffi::SQLITE_CONSTRAINT_CHECK,
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
        ffi::SQLITE_CONSTRAINT_NOTNULL,
    ] {
        let cell = Cell::default();
        assert_eq!(classify(&sqlite(code)), Disposition::Fence);
        let original = cell
            .preserve(sqlite(code), Origin::Witness)
            .unwrap()
            .unwrap();
        assert_eq!(
            original
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error()
                .unwrap()
                .extended_code,
            code
        );
        assert!(cell.take().unwrap().is_none());
        assert!(cell.constraint_reason().unwrap().unwrap().len() <= 96);
    }
    for original in [
        sqlite(ffi::SQLITE_ERROR),
        sqlite(ffi::SQLITE_SCHEMA),
        sqlite(ffi::SQLITE_MISUSE),
        sqlite(255),
        rusqlite::Error::InvalidQuery.into(),
    ] {
        assert_eq!(classify(&original), Disposition::Unclassified);
        let cell = Cell::default();
        assert!(cell.preserve(original, Origin::Witness).unwrap().is_none());
        assert!(cell.take().unwrap().unwrap().is::<rusqlite::Error>());
    }
    assert_eq!(
        classify(&anyhow::anyhow!("explicit logical completeness refusal")),
        Disposition::Fence
    );
}

#[derive(Debug)]
struct Pages {
    count: i64,
    max: i64,
    free: i64,
    size: i64,
    journal: String,
}
fn pages(c: &Connection) -> Pages {
    Pages {
        count: c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap(),
        max: c
            .query_row("PRAGMA max_page_count", [], |r| r.get(0))
            .unwrap(),
        free: c
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap(),
        size: c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap(),
        journal: c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap(),
    }
}
fn report(f: &Fixture, phase: &str) {
    f.raw(|c| eprintln!("operator FULL {phase}: {:?}", pages(c)));
}

#[test]
fn managed_operator_error_real_full_aborts_all_callers_and_writer_recovers() {
    // As in2293, only fresh layout setup may repeat, never semantic assertions.
    for setup in 0..4 {
        let f = Fixture::new(true);
        let mut old_max = None;
        f.raw(|c| {
            c.execute_batch(
                "CREATE TABLE test_terminal_operator_probe(payload BLOB NOT NULL); VACUUM;",
            )
            .unwrap();
            let initial = pages(c);
            eprintln!("operator FULL setup={setup} after-table-seed-vacuum: {initial:?}");
            assert_eq!(initial.free, 0, "probe allocation must not use free pages");
            assert!(initial.size > 0 && initial.size <= 65536);
            assert_eq!(initial.journal, "wal");
            old_max = Some(initial.max);
            let actual: i64 = c
                .query_row(
                    &format!("PRAGMA max_page_count={}", initial.count),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, initial.count);
            let capped = pages(c);
            assert_eq!(capped.count, initial.count);
            assert_eq!(capped.max, initial.count);
            assert_eq!(capped.free, 0);
            eprintln!("operator FULL setup={setup} before: {capped:?}");
        });
        let before = f.saved();
        f.reset_counts();
        f.adapter.operator_error.arm_probe(true);
        let ran = Arc::new(AtomicUsize::new(0));
        let writer = f.store.connection.write();
        let a_ran = ran.clone();
        let a = f.enqueue(move |tx| {
            update_exit(tx, 7);
            a_ran.fetch_add(1, Ordering::Relaxed);
            true
        });
        let b_ran = ran.clone();
        let b = f.enqueue(move |tx| {
            update_exit(tx, 8);
            b_ran.fetch_add(1, Ordering::Relaxed);
            true
        });
        drop(writer);
        let outcomes = [answer(a), answer(b)];
        let counts = f.counts();
        eprintln!("operator FULL setup={setup} layout={counts:?} acknowledgements={outcomes:?}");
        report(&f, "after-fault");
        if counts != (1, 1) {
            continue;
        }
        assert_eq!(
            ran.load(Ordering::Relaxed),
            2,
            "native FULL before callback is setup failure"
        );
        let seen = f.adapter.operator_error.observed().unwrap().unwrap();
        assert_eq!(seen.primary, Some(ErrorCode::DiskFull));
        assert_eq!(seen.origin, Origin::Probe);
        assert_eq!(seen.disposition, Disposition::Infrastructure);
        assert!(outcomes.iter().all(Result::is_err));
        assert_eq!(f.saved(), before); // native/capture/root/witness/guard all restored
        assert!(f.adapter.operator_error.take().unwrap().is_none());
        f.raw(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM test_terminal_operator_probe",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            let max = old_max.unwrap();
            let restored: i64 = c
                .query_row(&format!("PRAGMA max_page_count={max}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(restored, max);
        });
        f.adapter.operator_error.arm_probe(false);
        report(&f, "after-restore");
        // Actual public batched writer and persistent output, after possible automatic rollback.
        f.store.append_claim(&observation(&f, 7)).unwrap();
        assert!(
            f.line()["message"]
                .as_str()
                .unwrap()
                .contains("exit code 7")
        );
        assert_eq!(f.line()["status"], "warn");
        assert!(f.saved().0.generation > before.0.generation);
        assert!(f.adapter.operator_error.observed().unwrap().is_none());
        assert!(!f.read(|tx| f.source().scoped_state(tx).unwrap().3));
        report(&f, "after-clean-commit");
        return;
    }
    panic!("failed to establish one outer batch for FULL control");
}

#[test]
fn managed_operator_error_witness_constraint_fences_without_aborting_admission() {
    let f = Fixture::new(true);
    let before = f.saved();
    f.raw(|c| c.execute_batch("CREATE TEMP TRIGGER test_terminal_operator_constraint BEFORE UPDATE OF body ON main.test_terminal_members WHEN NEW.key='gate/native' BEGIN SELECT RAISE(ABORT,'private witness constraint'); END;").unwrap());
    let native = f.store.append_claim(&observation(&f, 7)).unwrap();
    assert_eq!(f.counts(), (1, 1));
    let seen = f.adapter.operator_error.observed().unwrap().unwrap();
    assert_eq!(seen.primary, Some(ErrorCode::ConstraintViolation));
    assert_eq!(seen.extended, Some(ffi::SQLITE_CONSTRAINT_TRIGGER));
    assert_eq!(seen.origin, Origin::Witness);
    assert_eq!(seen.disposition, Disposition::Fence);
    assert!(f.adapter.operator_error.take().unwrap().is_none());
    f.read(|tx| {
        assert!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1)",
                [&native.id],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert!(f.source().installer.root(tx, VIEW).is_err());
        let body: String = tx
            .query_row(
                "SELECT body FROM test_terminal_members WHERE key='gate/native'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, before.3); // callback DML rolled back, old witness cannot be read as current
    });
    assert_eq!(f.line()["status"], "unknown");
    assert!(f.line()["message"].as_str().unwrap().contains("constraint"));
    f.raw(|c| {
        c.execute_batch("DROP TRIGGER test_terminal_operator_constraint")
            .unwrap()
    });
    f.store.append_claim(&observation(&f, 0)).unwrap();
    assert_eq!(f.line()["status"], "unknown"); // sticky gap: no automatic re-attestation
    assert!(f.adapter.operator_error.observed().unwrap().is_none());
}

#[test]
fn managed_operator_error_first_fault_reentry_and_lent_reset_are_private() {
    let fenced = Cell::default();
    assert!(
        fenced
            .preserve(sqlite(ffi::SQLITE_CONSTRAINT_TRIGGER), Origin::Witness)
            .unwrap()
            .is_some()
    );
    let first_reason = fenced.constraint_reason().unwrap().unwrap();
    assert!(
        fenced
            .preserve(anyhow::anyhow!("later logical refusal"), Origin::Witness)
            .unwrap()
            .is_some()
    );
    assert_eq!(fenced.constraint_reason().unwrap().unwrap(), first_reason);
    assert!(fenced.take().unwrap().is_none());
    let cell = Arc::new(Cell::default());
    assert!(
        cell.preserve(sqlite(ffi::SQLITE_FULL), Origin::Probe)
            .unwrap()
            .is_none()
    );
    assert!(
        cell.preserve(sqlite(ffi::SQLITE_BUSY), Origin::Witness)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cell.observed().unwrap().unwrap().primary,
        Some(ErrorCode::DiskFull)
    );
    assert_eq!(
        cell.take()
            .unwrap()
            .unwrap()
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(ErrorCode::DiskFull)
    );
    let scope = cell.enter().unwrap();
    assert!(cell.enter().is_err());
    assert!(cell.reset().is_err());
    drop(scope);
    assert_eq!(cell.observed().unwrap().unwrap().origin, Origin::Reentry);
    assert!(cell.take().unwrap().is_some());
    cell.reset().unwrap();
    assert!(cell.observed().unwrap().is_none());
    let f = Fixture::new(true);
    let other = Fixture::new(true);
    for rollback in [true, false] {
        let before = f.saved();
        f.reset_counts();
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        // Synthetic stale callback memory; proves reset when rollback/drop never finalized.
        assert!(
            f.adapter
                .operator_error
                .preserve(sqlite(ffi::SQLITE_FULL), Origin::Witness)
                .unwrap()
                .is_none()
        );
        update_exit(&tx, 7);
        if rollback {
            tx.rollback().unwrap();
        } else {
            drop(tx);
        }
        drop(writer);
        assert_eq!(f.counts(), (1, 0));
        assert_eq!(f.saved(), before);
        assert!(other.adapter.operator_error.observed().unwrap().is_none());
        f.commit(|_| {});
        assert_eq!(f.counts(), (2, 1));
        assert!(f.adapter.operator_error.observed().unwrap().is_none());
        assert!(f.adapter.operator_error.take().unwrap().is_none());
        assert_eq!(f.saved().0, before.0);
    }
}

#[test]
fn managed_operator_error_bootstrap_reopen_and_policy_lifetime_are_not_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
    let adapter = Adapter::install(&store).unwrap();
    let weak = Arc::downgrade(&adapter.operator_error);
    adapter.operator_error.arm_probe(true); // nonexistent probe: unclassified SQL ERROR
    assert!(adapter.bootstrap(&store, binding(), true).is_err());
    assert_eq!(
        adapter
            .operator_error
            .observed()
            .unwrap()
            .unwrap()
            .disposition,
        Disposition::Unclassified
    );
    assert!(store.connection.write().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='test_native_terminal_guard')", [], |r|r.get::<_,bool>(0)).is_ok_and(|v| !v));
    drop(adapter);
    drop(store);
    assert!(weak.upgrade().is_none());
    let f = Fixture::new(true);
    let path = f.dir.path().join("claims.sqlite3");
    let weak = Arc::downgrade(&f.adapter.operator_error);
    let before = f.saved().0;
    drop(f.adapter);
    drop(f.store);
    assert!(weak.upgrade().is_none());
    let store = Store::open(&path, "node").unwrap();
    let adapter = Adapter::matching_reopen(&store, binding()).unwrap();
    let f = Fixture {
        adapter,
        store,
        dir: f.dir,
    };
    assert!(f.adapter.operator_error.observed().unwrap().is_none());
    let pending = f.read(|tx| f.source().state(tx).unwrap().1);
    f.commit(|_| {});
    if pending {
        assert_eq!(f.line()["status"], "unknown");
    } else {
        assert_eq!(f.saved().0, before);
        assert_eq!(f.line()["status"], "warn");
    }
    assert!(f.adapter.operator_error.observed().unwrap().is_none());
}

#[test]
fn managed_operator_error_old_policy_reopen_refuses_without_apply_or_adoption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let source = NativeSource::attach(&tx, binding()).unwrap();
        super::super::super::tests::seed_native(&tx, &source.binding);
        source.finish(&tx).unwrap();
        assert_eq!(source.doctor_line(&tx)["status"], "warn");
        tx.commit().unwrap();
    }
    drop(store);
    let store = Store::open(&path, "node").unwrap();
    let adapter = Adapter::matching_reopen(&store, binding()).unwrap();
    let f = Fixture {
        adapter,
        store,
        dir,
    };
    assert_eq!(f.line()["status"], "unknown"); // wrapper policy fingerprint differs
    assert!(f.read(|tx| f.source().installer.root(tx, VIEW).is_err()));
    f.store.append_claim(&observation(&f, 7)).unwrap(); // binding failure fences, not callback storage
    assert_eq!(f.line()["status"], "unknown");
    assert!(f.adapter.operator_error.observed().unwrap().is_none());
    assert!(f.read(|tx| f.source().installer.root(tx, VIEW).is_err()));
}
