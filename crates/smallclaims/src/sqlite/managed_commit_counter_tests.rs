//! Isolated controls for the diagnostic managed-outer-commit unit.
use super::*;

fn writer() -> WriterConnection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON;
        CREATE TABLE claims(store_index INTEGER PRIMARY KEY);
        CREATE TABLE parent(id INTEGER PRIMARY KEY);
        CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();
    WriterConnection::new(connection, Arc::new(AtomicU64::new(0)))
}

#[test]
fn managed_outer_commits_count_success_once_and_exclude_other_units() {
    // Other tests may commit on their own threads. An isolated process owns this global counter.
    const CHILD: &str = "SMALLCLAIMS_MANAGED_COMMIT_COUNTER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sqlite::managed_commit_counter_tests::managed_outer_commits_count_success_once_and_exclude_other_units", "--nocapture"])
            .env(CHILD, "1").env_remove("ST3_PROFILE_DIR").status().unwrap();
        assert!(status.success(), "isolated managed-commit control failed");
        return;
    }
    assert!(!crate::profile::enabled());
    assert_eq!(crate::profile::cost_snapshot()["state"], "disabled");
    assert!(
        !crate::profile::enabled(),
        "a snapshot must not enable the profiler"
    );
    let writer = writer();
    let start = crate::profile::managed_commit_count();
    let mut expected = start;
    let check = |expected| assert_eq!(crate::profile::managed_commit_count(), expected);
    {
        let mut guard = writer.write();
        guard.transaction().unwrap().commit().unwrap(); // No-op counts, once through commit_checked.
        expected += 1;
        check(expected);
        let mut tx = guard.transaction().unwrap();
        {
            let nested = tx.savepoint().unwrap();
            nested.execute("INSERT INTO parent VALUES(1)", []).unwrap();
            nested.commit().unwrap();
        }
        check(expected); // Successful nested commit is not an outer commit.
        {
            let nested = tx.savepoint().unwrap();
            nested.execute("INSERT INTO parent VALUES(2)", []).unwrap();
            // Savepoint drop rolls back; neither attempt counts.
        }
        tx.commit().unwrap();
        expected += 1;
        check(expected);
        assert_eq!(
            guard
                .query_row("SELECT COUNT(*) FROM parent", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        guard.transaction().unwrap().rollback().unwrap();
        drop(guard.transaction().unwrap());
        assert!(
            guard
                .transaction()
                .unwrap()
                .commit_checked(|| anyhow::bail!("refused check"))
                .is_err()
        );
        let tx = guard.transaction().unwrap();
        tx.execute("INSERT INTO child VALUES(999)", []).unwrap();
        assert!(tx.commit().is_err()); // Real outer SQLite COMMIT failure.
        check(expected);
        guard.execute("INSERT INTO parent VALUES(3)", []).unwrap(); // Raw autocommit excluded.
        guard
            .execute_batch("BEGIN; INSERT INTO parent VALUES(4); COMMIT")
            .unwrap();
        let tx = rusqlite::Connection::transaction(&mut guard).unwrap();
        tx.commit().unwrap(); // Raw API transaction excluded.
        let tx = guard.transaction().unwrap();
        tx.execute_batch("COMMIT").unwrap();
        assert!(tx.commit().is_err()); // Raw-ended wrapper cannot count again.
        check(expected);
    }
    writer
        .batched(|_| Ok::<_, anyhow::Error>(()))
        .unwrap()
        .unwrap();
    expected += 1;
    check(expected); // Batched no-op outer commit counts once.
    // A failed job savepoint is different from a failed outer commit: its empty outer commit counts.
    assert!(
        writer
            .batched(|_| Err::<(), _>(anyhow::anyhow!("job rolled back")))
            .unwrap()
            .is_err()
    );
    expected += 1;
    check(expected);
    assert!(
        writer
            .batched(|tx| {
                tx.execute("INSERT INTO child VALUES(999)", [])?;
                Ok::<_, anyhow::Error>(())
            })
            .is_err()
    );
    check(expected);
    writer
        .install_transaction_finalizer(|_| anyhow::bail!("finalizer refused"))
        .unwrap();
    assert!(writer.batched(|_| Ok::<_, anyhow::Error>(())).is_err());
    {
        let mut guard = writer.write();
        assert!(guard.transaction().unwrap().commit().is_err());
    }
    check(expected);
    assert_eq!(crate::profile::cost_snapshot()["state"], "disabled");
}
