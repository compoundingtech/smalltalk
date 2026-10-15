//! Opened database paths, not caller aliases, select readers and checkpoint sidecars.
use rusqlite::{Connection, OpenFlags};
use smallclaims::{
    claim::ClaimInput,
    sqlite::ReadPool,
    store::{Store, StoreCopy, runtime::Plain},
};
use std::{collections::BTreeMap, path::{Path, PathBuf}, sync::Arc};

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn append(store: &Store) {
    store.append_claim(&ClaimInput {
        subject: "note/wal-path".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::from([("text".into(), "committed in the target WAL".into())]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
}

fn claims(connection: &Connection) -> i64 {
    connection.query_row("SELECT count(*) FROM claims", [], |row| row.get(0)).unwrap()
}

fn assert_opened_target(alias: &Path, target: &Path, scratch: &Path) {
    let store = Store::open(alias, "alder", Arc::new(Plain)).unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    assert_eq!(store.path, target);
    assert_eq!(store.readers.path, target);
    assert_eq!(store.origin, "alder", "physical spelling is not logical identity");
    assert!(!store.shared_memory);
    {
        let writer = store.connection.write();
        let synchronous: i64 = writer.pragma_query_value(None, "synchronous", |row| row.get(0)).unwrap();
        assert_eq!(synchronous, 2, "FULL durability is unchanged");
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        let busy: i64 = writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)).unwrap();
        assert_eq!(busy, 0);
    }
    append(&store);
    assert_eq!(claims(&store.connection.write()), 1);
    assert_eq!(claims(&store.readers.get()), 1);
    let wal = sidecar(&target, "-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);

    // Discriminate active committed WAL content from a main-file-only fixture. No writer
    // runs concurrently here, and autocheckpoint is disabled until these reads/copies finish.
    let main_only = scratch.join("main-only.sqlite3");
    std::fs::copy(&target, &main_only).unwrap();
    let main_only = Connection::open_with_flags(&main_only, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(claims(&main_only), 0, "the committed claim must require the WAL");

    // A file-alias sidecar must not be selected. Parent aliases address the SAME sidecar,
    // so they get their own test without creating a conflicting filesystem entry.
    let alias_wal = sidecar(alias, "-wal");
    let decoy = std::fs::canonicalize(alias.parent().unwrap()).unwrap() == target.parent().unwrap()
        && alias.file_name() != target.file_name();
    if decoy {
        std::fs::write(&alias_wal, b"not the target WAL").unwrap();
    }
    let copy = scratch.join("complete.sqlite3");
    store.copy_store_to(&copy).unwrap();
    let copied = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let integrity: String = copied.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
    assert_eq!(integrity, "ok");
    assert_eq!(claims(&copied), 1);
    let mode: String = copied.pragma_query_value(None, "journal_mode", |row| row.get(0)).unwrap();
    assert_eq!(mode, "delete");
    assert!(!sidecar(&copy, "-wal").exists());
    if decoy {
        assert_eq!(std::fs::read(alias_wal).unwrap(), b"not the target WAL");
    }
    assert_eq!(claims(&store.readers.get()), 1, "copying preserves the source");
}

#[cfg(unix)]
#[test]
fn existing_file_symlink_uses_target_readers_and_committed_wal_copy() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target.sqlite3");
    drop(Store::open(&target, "alder", Arc::new(Plain)).unwrap());
    let alias = root.path().join("alias.sqlite3");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    assert_opened_target(&alias, &target, root.path());
}

#[cfg(unix)]
#[test]
fn symlinked_parent_uses_target_readers_and_committed_wal_copy() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let target = real.join("claims.sqlite3");
    drop(Store::open(&target, "alder", Arc::new(Plain)).unwrap());
    let alias = root.path().join("linked");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert_opened_target(&alias.join("claims.sqlite3"), &target, root.path());
}

#[test]
fn fresh_relative_store_creates_parents_and_retains_the_opened_absolute_path() {
    // No process-wide chdir: this fixture remains safe beside parallel tests.
    let cwd = std::env::current_dir().unwrap();
    let root = tempfile::Builder::new().prefix(".store-path-").tempdir_in(&cwd).unwrap();
    let relative = root.path().strip_prefix(&cwd).unwrap().join("new/nested/claims.sqlite3");
    assert!(relative.is_relative());
    let store = Store::open(&relative, "fresh", Arc::new(Plain)).unwrap();
    assert!(store.path.is_absolute());
    assert_eq!(std::fs::canonicalize(&store.path).unwrap(), std::fs::canonicalize(&relative).unwrap());
    assert_eq!(store.readers.path, store.path);
    append(&store);
    assert_eq!(claims(&store.readers.get()), 1);
}

#[test]
fn shared_memory_uri_stays_shared_and_copies_in_full() {
    let store = Store::open_memory("memory", Arc::new(Plain)).unwrap();
    assert!(store.shared_memory && store.readers.shared_memory);
    assert_eq!(store.path, store.readers.path);
    let uri = store.path.to_str().unwrap();
    assert!(uri.starts_with("file:st3-") && uri.ends_with("?mode=memory&cache=shared"));
    append(&store);
    assert_eq!(claims(&store.readers.get()), 1);
    let root = tempfile::tempdir().unwrap();
    let copy = root.path().join("memory.sqlite3");
    assert_eq!(store.copy_store_to(&copy).unwrap(), StoreCopy::Full);
    let copied = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(claims(&copied), 1);
}

#[test]
fn invalid_database_targets_are_refused_without_replacing_them() {
    let root = tempfile::tempdir().unwrap();
    assert!(Store::open(root.path(), "invalid", Arc::new(Plain)).is_err());
    let invalid = root.path().join("not-a-database");
    let original = b"not a sqlite database: preserve this source";
    std::fs::write(&invalid, original).unwrap();
    assert!(Store::open(&invalid, "invalid", Arc::new(Plain)).is_err());
    assert_eq!(std::fs::read(invalid).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn rebinding_the_caller_alias_does_not_redirect_new_readers_or_copies() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.sqlite3");
    let b = root.path().join("b.sqlite3");
    drop(Store::open(&a, "alder", Arc::new(Plain)).unwrap());
    drop(Store::open(&b, "birch", Arc::new(Plain)).unwrap());
    let alias = root.path().join("alias.sqlite3");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let store = Store::open(&alias, "alder", Arc::new(Plain)).unwrap();
    store.connection.write().pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    append(&store);
    // Hold the initial pooled reader so the next acquisition really opens another connection.
    let cached = store.readers.get();
    assert_eq!(claims(&cached), 1);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&b, &alias).unwrap();
    let fresh = store.readers.get();
    assert_eq!(claims(&fresh), 1, "a new reader must still use the opened target a");
    drop(fresh);
    drop(cached);
    let copy = root.path().join("copy.sqlite3");
    store.copy_store_to(&copy).unwrap();
    let copy = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(claims(&copy), 1);
    let other = Connection::open_with_flags(&b, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(claims(&other), 0);
    assert_eq!(std::fs::read_link(alias).unwrap(), b);
    // This changes only the original alias after open. It does not certify security against
    // replacing a resolved target/parent, or changing the path concurrently during open.
}

#[test]
fn wrapping_mismatched_writer_and_pool_refuses() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.sqlite3");
    let b = root.path().join("b.sqlite3");
    drop(Store::open(&a, "alder", Arc::new(Plain)).unwrap());
    drop(Store::open(&b, "birch", Arc::new(Plain)).unwrap());
    let connection = Connection::open(&a).unwrap();
    let readers = ReadPool::new(&b, false).unwrap();
    let result = Store::from_connection(connection, readers, "alder".into(), a, false, Arc::new(Plain));
    let error = result.err().expect("different databases must not form one store");
    assert!(error.to_string().contains("same opened database path and mode"));
}

#[cfg(unix)]
#[test]
fn non_utf8_target_filename_is_preserved_in_readers_and_copy_source() {
    use std::os::unix::ffi::OsStringExt as _;
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join(std::ffi::OsString::from_vec(b"claims-\xff.sqlite3".to_vec()));
    drop(Store::open(&target, "alder", Arc::new(Plain)).unwrap());
    let alias = root.path().join("alias.sqlite3");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    assert_opened_target(&alias, &target, root.path());
}
