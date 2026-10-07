//! Measure the daemon's event migration worker on a private, previously copied store.
//! Usage: cargo run --release -p st3 --example event_migration -- CLONE OUTPUT.json
//! Never supply a live store or the immutable capture: this opens and migrates CLONE.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde_json::json;
use st3::store::Store;

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // getrusage initializes the complete struct on success.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(status, 0);
    let usage = unsafe { usage.assume_init() };
    usage.ru_utime.tv_sec as f64
        + usage.ru_stime.tv_sec as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().context("private clone path is required")?);
    let output = PathBuf::from(args.next().context("private output path is required")?);
    ensure!(args.next().is_none(), "expected CLONE OUTPUT.json");
    ensure!(!path.is_symlink(), "clone must not be a symlink");
    ensure!(
        path.metadata()?.permissions().mode() & 0o777 == 0o600,
        "clone must be mode 0600"
    );
    ensure!(
        path.parent()
            .context("clone directory")?
            .metadata()?
            .permissions()
            .mode()
            & 0o777
            == 0o700,
        "clone directory must be mode 0700"
    );
    ensure!(!output.exists(), "output must be a new file");
    ensure!(
        output
            .parent()
            .context("output directory")?
            .metadata()?
            .permissions()
            .mode()
            & 0o777
            == 0o700,
        "output directory must be mode 0700"
    );
    let captured_bytes = path.metadata()?.len();
    let cpu_before = cpu_seconds();
    let started = Instant::now();
    let opened = Instant::now();
    let store = Arc::new(Store::open(&path, "node")?);
    let open_ms = opened.elapsed().as_secs_f64() * 1_000.0;
    let counts = || -> Result<(u64, u64, u64)> {
        let connection = store.readers.get();
        Ok((
            connection.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))?,
            connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?,
            connection.query_row("SELECT COUNT(*) FROM idempotency", [], |row| row.get(0))?,
        ))
    };
    let before = counts()?;
    let cpu_migration = cpu_seconds();
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        st3::maintenance::migrate_event_payloads(store.clone()),
    )
    .await
    .context("migration exceeded its 600 second cap")??;
    let migration_cpu_seconds = cpu_seconds() - cpu_migration;
    let after = counts()?;
    ensure!(
        before == after,
        "claim, event and receipt counts must be preserved"
    );
    ensure!(
        report.pending_at_start && report.completed,
        "an actual pending migration must complete"
    );
    ensure!(
        !store.event_payload_migration_pending()?,
        "legacy migration must be absent"
    );
    let result = json!({
        "sqlite_version": rusqlite::version(),
        "release_build": !cfg!(debug_assertions),
        "source_bytes": captured_bytes,
        "rows_before": {"claims":before.0,"events":before.1,"receipts":before.2},
        "rows_after": {"claims":after.0,"events":after.1,"receipts":after.2},
        "store_open_ms": open_ms,
        "migration": report,
        "migration_process_cpu_seconds": migration_cpu_seconds,
        "whole_process_cpu_seconds": cpu_seconds() - cpu_before,
        "whole_elapsed_ms": started.elapsed().as_secs_f64() * 1_000.0,
        "limits": "private copied store, no server/reconciler/peers/providers; wall includes chunk pauses; chunk calls include writer wait and commit, not exclusive writer hold; not upgrade-under-load, cold-cache, fleet or p99 evidence"
    });
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)?;
    file.write_all(&serde_json::to_vec_pretty(&result)?)?;
    file.sync_all()?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
