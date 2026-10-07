//! Explicit full diagnostics on private database snapshots, without opening a Store or
//! migrating/repairing the evidence before inspecting it. This module has no HTTP entry point.
use super::*;
use crate::model::{DoctorCheck, DoctorReport};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub static AUDIT_INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub const DEFAULT_SCRATCH_LIMIT: u64 = 32 * 1024 * 1024 * 1024;

/// Shared by the isolated CLI and fixture audit. SQLite progress checks bound private work;
/// cancellation never repairs evidence and ordinary unwinding removes all scratch files.
struct AuditBudget<'a> {
    root: &'a Path,
    max_bytes: u64,
    started: Instant,
    interrupted: &'a AtomicBool,
    available: &'a dyn Fn(&Path) -> std::io::Result<u64>,
}
impl AuditBudget<'_> {
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.interrupted.load(Ordering::Relaxed),
            "offline audit interrupted"
        );
        anyhow::ensure!(
            self.started.elapsed() < Duration::from_secs(600),
            "offline audit exceeded its ten-minute budget"
        );
        let mut bytes = 0_u64;
        for (count, entry) in std::fs::read_dir(self.root)?.enumerate() {
            anyhow::ensure!(count < 32, "offline audit scratch file count exceeded");
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("st3-offline-audit-")
            {
                for (count, child) in std::fs::read_dir(entry.path())?.enumerate() {
                    anyhow::ensure!(count < 32, "offline audit scratch file count exceeded");
                    bytes = bytes
                        .checked_add(child?.metadata()?.len())
                        .context("scratch size overflow")?;
                }
            } else if metadata.is_file() {
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("scratch size overflow")?;
            }
        }
        anyhow::ensure!(
            bytes <= self.max_bytes,
            "offline audit scratch size limit exceeded"
        );
        anyhow::ensure!(
            (self.available)(self.root)? >= (1 << 30),
            "offline audit scratch space exhausted"
        );
        Ok(())
    }
}

struct ProgressGuard<'a, 'b> {
    connection: &'a Connection,
    _budget: &'b AuditBudget<'b>,
}
impl Drop for ProgressGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: the connection outlives this guard, and removal retains no callback data.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(self.connection.handle(), 0, None, std::ptr::null_mut());
        }
    }
}
fn progress<'a, 'b>(
    connection: &'a Connection,
    budget: &'b AuditBudget<'b>,
) -> ProgressGuard<'a, 'b> {
    unsafe extern "C" fn check(raw: *mut std::ffi::c_void) -> std::ffi::c_int {
        // SAFETY: the stack budget outlives the guard; SQLite calls this synchronously.
        let budget = unsafe { &*(raw.cast::<AuditBudget<'_>>()) };
        i32::from(budget.check().is_err())
    }
    // SAFETY: the guard removes the callback before the budget or connection can be dropped.
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            connection.handle(),
            100_000,
            Some(check),
            std::ptr::from_ref(budget).cast_mut().cast(),
        );
    }
    ProgressGuard {
        connection,
        _budget: budget,
    }
}

fn copy_input(file: &mut std::fs::File, target: &Path, budget: &AuditBudget<'_>) -> Result<()> {
    let before = file.metadata()?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        budget.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
    }
    let after = file.metadata()?;
    anyhow::ensure!(
        before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "offline audit input changed while copying; supply a frozen copy"
    );
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct OfflineAuditReport {
    pub mode: &'static str,
    pub schema_version: u32,
    pub store_index: Option<u64>,
    #[serde(flatten)]
    pub report: DoctorReport,
}

/// The input must be a caller-provided snapshot, distinct from the configured live store.
/// The input is opened read-only; SQLite takes a consistent snapshot into a private directory.
/// Inspection never calls Store::open, so missing/corrupt projection evidence stays missing.
pub fn offline_full_audit(database: &Path, live_database: &Path) -> Result<OfflineAuditReport> {
    offline_full_audit_in(
        database,
        live_database,
        database.parent().unwrap_or(Path::new(".")),
        DEFAULT_SCRATCH_LIMIT,
    )
}

/// The CLI supplies an explicit scratch filesystem and sets SQLITE_TMPDIR before starting
/// threads. Caller input is never opened by SQLite, including when it has a WAL sidecar.
pub fn offline_full_audit_in(
    database: &Path,
    live_database: &Path,
    scratch_root: &Path,
    max_scratch_bytes: u64,
) -> Result<OfflineAuditReport> {
    offline_full_audit_with_limits(
        database,
        live_database,
        scratch_root,
        max_scratch_bytes,
        &AUDIT_INTERRUPTED,
        &|path| crate::disk::disk_space(path).map(|space| space.available),
    )
}

fn offline_full_audit_with_limits(
    database: &Path,
    live_database: &Path,
    scratch_root: &Path,
    max_scratch_bytes: u64,
    interrupted: &AtomicBool,
    available: &dyn Fn(&Path) -> std::io::Result<u64>,
) -> Result<OfflineAuditReport> {
    let mut input_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(database)
        .context("open offline audit input")?;
    let input = input_file.metadata().context("fstat offline audit input")?;
    anyhow::ensure!(
        input.is_file(),
        "offline audit input must be a database file"
    );
    let live_file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(live_database)
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("identify configured live database"),
    };
    if let Some(live_file) = live_file {
        let live = live_file.metadata()?;
        anyhow::ensure!(
            input.dev() != live.dev() || input.ino() != live.ino(),
            "offline audit refuses the live database; supply a private copy"
        );
    }
    let mut wal_name = database.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let mut wal = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&wal_path)
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(file) = &wal {
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "offline WAL input must be a regular file"
        );
    }
    let wal_size = wal
        .as_ref()
        .map(|file| file.metadata().map(|meta| meta.len()))
        .transpose()?
        .unwrap_or(0);
    let size = input
        .len()
        .checked_add(wal_size)
        .context("offline input size overflow")?;
    let needed = size
        .checked_mul(6)
        .and_then(|size| size.checked_add(1 << 30))
        .context("offline audit scratch size overflow")?;
    anyhow::ensure!(
        needed <= max_scratch_bytes,
        "offline audit requires {needed} bytes, exceeding the scratch size limit {max_scratch_bytes}"
    );
    let scratch = tempfile::Builder::new()
        .prefix("st3-offline-audit-")
        .tempdir_in(scratch_root)?;
    let space = available(scratch.path())?;
    anyhow::ensure!(
        space >= needed,
        "offline audit needs {needed} bytes of scratch space; {space} are available"
    );
    let budget = AuditBudget {
        root: scratch_root,
        max_bytes: max_scratch_bytes,
        started: Instant::now(),
        interrupted,
        available,
    };
    let copied = scratch.path().join("input.sqlite3");
    copy_input(&mut input_file, &copied, &budget)?;
    if let Some(wal) = &mut wal {
        copy_input(wal, &scratch.path().join("input.sqlite3-wal"), &budget)?;
    }
    let after_copy = input_file.metadata()?;
    anyhow::ensure!(
        input.len() == after_copy.len()
            && input.mtime() == after_copy.mtime()
            && input.mtime_nsec() == after_copy.mtime_nsec()
            && input.ctime() == after_copy.ctime()
            && input.ctime_nsec() == after_copy.ctime_nsec(),
        "offline audit input changed while copying its WAL; supply a frozen copy"
    );
    anyhow::ensure!(
        wal.is_some() || !wal_path.exists(),
        "offline audit input acquired a WAL while copying; supply a frozen copy"
    );
    // SQLite can create/rebuild SHM only beside this private copy. The original descriptor
    // and WAL are never handed to SQLite and are never migrated or checkpointed.
    let source = Connection::open_with_flags(&copied, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let source_progress = progress(&source, &budget);
    let snapshot = scratch.path().join("evidence.sqlite3");
    source.execute("VACUUM INTO ?1", [snapshot.to_string_lossy()])?;
    drop(source_progress);
    drop(source);
    budget.check()?;
    let evidence =
        Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let evidence_progress = progress(&evidence, &budget);
    let schema_version = evidence.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let mut checks = vec![
        audit_check("sqlite-integrity", || {
            let rows = evidence
                .prepare("PRAGMA integrity_check")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().filter(|row| row != "ok").collect())
        }),
        audit_check("foreign-keys", || {
            Ok(evidence
                .prepare("PRAGMA foreign_key_check")?
                .query_map([], |row| {
                    Ok(format!(
                        "{} row {:?} references {}",
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }),
    ];
    let store_index = current_index(&evidence).ok();
    checks.push(audit_check("operation-projection", || {
        operation_drift(&evidence)
    }));
    checks.push(audit_check("projection-digest-evidence", || {
        let expected = full_digests(&evidence)?;
        Ok(projection_digest::differing(
            &expected,
            &projection_digest::tables(&evidence)?,
        ))
    }));
    checks.push(audit_check("graph-references", || {
        unresolved_graph_references(&evidence, "offline-audit").map_err(Into::into)
    }));
    checks.push(audit_check("operational-repair", || {
        Ok(operational_repair_plan_tx(&evidence, now_ms())?
            .items
            .into_iter()
            .map(|item| format!("{}: {}", item.class, item.subject))
            .collect())
    }));
    // Compare raw evidence with a full canonical replay on a second private copy. Neither
    // schema upgrades nor startup repair are permitted to bless the original evidence.
    checks.push(audit_check("canonical-projections", || {
        let actual = full_digests(&evidence)?;
        let replay_path = scratch.path().join("oracle.sqlite3");
        std::fs::copy(&snapshot, &replay_path)?;
        let mut replay = Connection::open(&replay_path)?;
        configure_projection_writer(&replay)?;
        replay.execute_batch(smallclaims::store::WRITE_CLOCK)?;
        let pages: u64 = replay.pragma_query_value(None, "page_size", |row| row.get(0))?;
        replay.pragma_update(None, "max_page_count", max_scratch_bytes / pages / 2)?;
        let transaction = replay.transaction()?;
        let replay_progress = progress(&transaction, &budget);
        let runtime = SmalltalkRuntime::default();
        smallclaims::Runtime::replay_from_nothing(&runtime, &transaction)?;
        smallclaims::Runtime::after_projection(&runtime, &transaction)?;
        let expected = full_digests(&transaction)?;
        drop(replay_progress);
        transaction.rollback()?;
        budget.check()?;
        Ok(projection_digest::differing(&actual, &expected))
    }));
    checks.push(DoctorCheck {
        name: "claim-signatures".into(), status: "warn".into(),
        message: "evidence incomplete; this audit does not yet recompute signature authority and verdicts".into(),
    });
    for name in [
        "terminal-exec-gates",
        "account-limits",
        "runtime-ownership",
        "runtime-drift",
        "driver-readiness",
        "replication",
        "shared-projections",
        "fleet-admission",
        "idempotency-keys",
        "mission-first-readiness",
        "message-delivery",
        "mail-backlog",
        "delivery-probes",
        "attention-age",
        "checkpoint-evidence",
        "claude-hooks",
        "member-reconcile",
    ] {
        checks.push(DoctorCheck {
            name: name.into(),
            status: "warn".into(),
            message: "evidence incomplete; this private-copy audit has not implemented this oracle"
                .into(),
        });
    }
    let status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "pass"
    };
    drop(evidence_progress);
    budget.check()?;
    Ok(OfflineAuditReport {
        mode: "offline-full-audit",
        schema_version,
        store_index,
        report: DoctorReport {
            machine_version: Some(st_drivers::version::machine_version()),
            status: status.into(),
            checks,
            performance: json!({}),
        },
    })
}

fn audit_check(name: &str, audit: impl FnOnce() -> Result<Vec<String>>) -> DoctorCheck {
    let (status, message) = match audit() {
        Ok(issues) if issues.is_empty() => ("pass", "full private-copy oracle agrees".into()),
        Ok(issues) => (
            "fail",
            format!(
                "{} discrepancies: {}",
                issues.len(),
                issues
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ),
        Err(error) => (
            "warn",
            format!("evidence incomplete; audit could not finish: {error:#}"),
        ),
    };
    DoctorCheck {
        name: name.into(),
        status: status.into(),
        message,
    }
}

fn operation_drift(connection: &Connection) -> Result<Vec<String>> {
    let expected = expected_operations(connection)?;
    let actual = connection
        .prepare("SELECT id,request_digest,canonical_claim_id,state FROM operations ORDER BY id")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    Ok(expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| expected.get(*key) != actual.get(*key))
        .cloned()
        .collect())
}

fn full_digests(connection: &Connection) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (table, excluded) in PROJECTION_DIGEST_TABLES {
        let columns = projection_digest::columns(connection, table, excluded)?;
        let encoded = serde_json::to_string(&columns)?;
        let query = if *table == "operations" {
            projection_digest::operation_rows()
        } else {
            format!(
                "SELECT {} FROM {table}",
                projection_digest::row_sql(&columns, "")
            )
        };
        let (count, hash) = projection_digest::scan(connection, table, &encoded, &query)?;
        result.insert(
            (*table).into(),
            projection_digest::table_digest(table, &encoded, count, &hash),
        );
    }
    let columns = "[\"id\",\"accepted_at_unix_ms\"]";
    let (count, hash) = projection_digest::scan(
        connection,
        "claim_sources",
        columns,
        projection_digest::source_query(),
    )?;
    result.insert(
        "claim_sources".into(),
        projection_digest::table_digest("claim_sources", columns, count, &hash),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_audit_preserves_corrupt_and_missing_evidence() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("copy.sqlite3");
        let store = Store::open(&path, "alder").unwrap();
        store
            .append_client_claim(&ClaimInput {
                subject: "resource/example".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), json!("custom.test.example"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some("example-operation".into()),
            })
            .unwrap();
        let raw_before = std::fs::read(&path).unwrap();
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        let wal_before = std::fs::read(&wal).ok();
        let shm_before = std::fs::read(&shm).ok();
        let healthy = offline_full_audit(&path, &root.path().join("live.sqlite3")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), raw_before);
        assert_eq!(std::fs::read(&wal).ok(), wal_before);
        assert_eq!(std::fs::read(&shm).ok(), shm_before);
        for check in &healthy.report.checks {
            if matches!(
                check.name.as_str(),
                "sqlite-integrity"
                    | "foreign-keys"
                    | "operation-projection"
                    | "projection-digest-evidence"
                    | "graph-references"
                    | "operational-repair"
                    | "canonical-projections"
            ) {
                assert_eq!(check.status, "pass", "{healthy:?}");
            } else {
                assert_eq!(check.status, "warn", "{healthy:?}");
            }
        }
        store
            .connection
            .write()
            .execute("UPDATE operations SET state='conflict'", [])
            .unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let audit = offline_full_audit(&path, &root.path().join("live.sqlite3")).unwrap();
        assert!(
            audit
                .report
                .checks
                .iter()
                .any(|check| check.name == "operation-projection" && check.status == "fail"),
            "{audit:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("DELETE FROM projection_digest_state", [])
            .unwrap();
        drop(connection);
        let missing = offline_full_audit(&path, &root.path().join("live.sqlite3")).unwrap();
        assert!(
            missing
                .report
                .checks
                .iter()
                .any(|check| check.name == "projection-digest-evidence" && check.status == "fail"),
            "{missing:?}"
        );
    }

    #[test]
    fn offline_audit_rejects_live_database_aliases_and_absent_inputs() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live.sqlite3");
        drop(Store::open(&live, "alder").unwrap());
        let hard = root.path().join("hard.sqlite3");
        let symbolic = root.path().join("symbolic.sqlite3");
        std::fs::hard_link(&live, &hard).unwrap();
        std::os::unix::fs::symlink(&live, &symbolic).unwrap();
        let directory_alias = root.path().join("directory-alias");
        std::os::unix::fs::symlink(root.path(), &directory_alias).unwrap();
        let directory_input = directory_alias.join("live.sqlite3");
        for input in [&live, &hard, &symbolic, &directory_input] {
            assert!(
                offline_full_audit(input, &live)
                    .unwrap_err()
                    .to_string()
                    .contains("refuses the live database")
            );
        }
        assert!(offline_full_audit(&root.path().join("missing"), &live).is_err());
        let copy = root.path().join("private.sqlite3");
        std::fs::copy(&live, &copy).unwrap();
        let error = offline_full_audit_in(&copy, &live, root.path(), 1).unwrap_err();
        assert!(error.to_string().contains("scratch size limit"));
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("st3-offline-audit-")
        }));
        let cancelled = AtomicBool::new(false);
        let before = std::fs::read(&copy).unwrap();
        let no_space = offline_full_audit_with_limits(
            &copy,
            &live,
            root.path(),
            DEFAULT_SCRATCH_LIMIT,
            &cancelled,
            &|_| Ok(0),
        )
        .unwrap_err();
        assert!(no_space.to_string().contains("bytes of scratch space"));
        assert_eq!(std::fs::read(&copy).unwrap(), before);
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("st3-offline-audit-")
        }));
        // SIGINT/SIGTERM set this same cancellation flag in the single-threaded CLI. This
        // fixture proves cancellation unwinds scratch files; actual signal wiring needs the CLI unit.
        cancelled.store(true, Ordering::Relaxed);
        let interrupted = offline_full_audit_with_limits(
            &copy,
            &live,
            root.path(),
            DEFAULT_SCRATCH_LIMIT,
            &cancelled,
            &|path| crate::disk::disk_space(path).map(|space| space.available),
        )
        .unwrap_err();
        assert!(interrupted.to_string().contains("interrupted"));
        assert_eq!(std::fs::read(&copy).unwrap(), before);
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("st3-offline-audit-")
        }));
        std::fs::write(&copy, b"not a sqlite database").unwrap();
        assert!(offline_full_audit(&copy, &live).is_err());
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("st3-offline-audit-")
        }));
    }
}
