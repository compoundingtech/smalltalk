//! The checkpoint proof's scratch copy: a filesystem clone where the filesystem can make one,
//! otherwise a full `VACUUM INTO` copy. Either way the copy is one consistent snapshot.
use rusqlite::{Connection, OpenFlags};
use smallclaims::{
    claim::ClaimInput,
    store::{Store, StoreCopy, runtime::Plain},
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn note(store: &Store, n: usize) {
    store
        .append_claim(&ClaimInput {
            subject: format!("note/{n}"),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("text".into(), "x".repeat(2_000).into())]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn claims_in(copy: &Path) -> i64 {
    let copy = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let check: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
    assert_eq!(check, "ok");
    let mode: String = copy.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
    assert_eq!(mode, "delete", "the copy is one file, with no WAL beside it");
    copy.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0)).unwrap()
}

#[test]
fn copy_is_one_consistent_snapshot_while_the_writer_commits() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("claims.sqlite3"), "alder", Arc::new(Plain)).unwrap();
    for n in 0..200 {
        note(&store, n);
    }
    let done = AtomicBool::new(false);
    let methods = std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut n = 200;
            while !done.load(Ordering::Relaxed) {
                note(&store, n);
                n += 1;
            }
        });
        let mut methods = Vec::new();
        for round in 0..8 {
            let committed = store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get::<_, i64>(0))
                .unwrap();
            let copy = root.path().join(format!("copy-{round}.sqlite3"));
            methods.push(store.copy_store_to(&copy).unwrap());
            assert!(!Path::new(&format!("{}-wal", copy.display())).exists());
            assert!(claims_in(&copy) >= committed, "the copy lost a committed claim");
        }
        done.store(true, Ordering::Relaxed);
        methods
    });
    // Every Mac's temporary directory is APFS, which clones. Linux clones on btrfs or XFS and
    // copies in full on ext4.
    if cfg!(target_os = "macos") {
        assert!(methods.iter().all(|method| *method == StoreCopy::Clone), "{methods:?}");
    }
    assert!(methods.windows(2).all(|pair| pair[0] == pair[1]), "{methods:?}");
}

#[test]
fn an_in_memory_store_is_copied_in_full() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    note(&store, 0);
    let root = tempfile::tempdir().unwrap();
    let copy = root.path().join("copy.sqlite3");
    assert_eq!(store.copy_store_to(&copy).unwrap(), StoreCopy::Full);
    assert_eq!(claims_in(&copy), 1);
}
