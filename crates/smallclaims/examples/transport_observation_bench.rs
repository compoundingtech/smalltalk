//! Isolated transport lookup measurement on a disposable, checkpointed database copy.
//!
//! Build: cargo build --release --locked -p smallclaims --features transport-bench --example transport_observation_bench
//! Run: target/release/examples/transport_observation_bench --store COPY --n 1000 --mode after --bootstrap-yield-ms 0
//! Repeat with --mode before against the SAME unchanged input for the original lookup.
//! Yield defaults to production's 100 ms; 0 skips only example inter-page sleeps.
//! For zero-yield runs, project production elapsed seconds by adding bootstrap_pages*0.1.
//! index_bytes_dbstat selects exact helper-object dbstat bytes versus checkpointed file
//! growth (page_count*page_size delta, which can undercount when free pages are reused).
//!
//! Linux only: a write lease rejects existing openers and aborts on a racing opener.
//! The input is never modified; a private temporary reflink (or full copy) is measured.
//! Refuses sidecars and sibling socket/lock artifacts, including the daemon run directory.
//! Requires space for a full copy when reflinks are unavailable, plus the working WAL.
//! Only aggregate JSON leaves this process; even failures and panics reveal no details.
//!
//! Each receive-shaped iteration persists one invented message claim through Plain, then
//! calls record_transport_observation for the historical peer/origin pair belonging to
//! the newest retained transport observation by store_index. Selection is setup-only and
//! neither identifier is output. A snapshot without such a pair is refused. Setup ends
//! with an up observation; measured up calls carry stale last_success_at=0 to force writes.
//! This exercises the selected peer's retained history, not an empty fabricated subject.
//! It measures claim persistence plus the transport callback, not fleet authentication,
//! admission, runtime projections, HTTP or group-commit contention. No row content is output.
//! Before removes all PR side-index objects and bypasses readiness probes; after rebuilds
//! them from scratch with the real bounded advance API. Setup/copy/open/drop costs are not
//! included. Bootstrap samples include every advance call's BEGIN-through-COMMIT duration;
//! insert samples include each successful batched writer transaction from before BEGIN to
//! after COMMIT, excluding queue wait and subsequent publication. Iteration wall time also
//! includes lookup, queueing and acknowledgment. Both modes run the same untimed setup
//! observations; after additionally warms caches through bootstrap.

#[cfg(target_os = "linux")]
mod harness {
    use std::collections::BTreeMap;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use anyhow::{Result, ensure};
    use serde_json::{Value, json};
    use smallclaims::claim::ClaimInput;
    use smallclaims::sqlite::transport_bench;
    use smallclaims::store::runtime::Plain;
    use smallclaims::store::{Store, transport_observation_index};

    const REMOVE_INDEX: &str = "
        DROP TRIGGER IF EXISTS transport_observation_insert;
        DROP TRIGGER IF EXISTS transport_observation_delete;
        DROP TRIGGER IF EXISTS transport_observation_update;
        DROP TABLE IF EXISTS transport_observation_index;
        DROP TABLE IF EXISTS transport_observation_backfill;";

    extern "C" fn lease_broken(_: libc::c_int) {
        const FAILURE: &[u8] = b"{\"ok\":false}\n";
        // SAFETY: only async-signal-safe syscalls, a static buffer and a valid stdout FD.
        unsafe {
            libc::write(libc::STDOUT_FILENO, FAILURE.as_ptr().cast(), FAILURE.len());
            libc::_exit(1);
        }
    }

    fn reject_artifacts(parent: &Path) -> Result<()> {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            ensure!(!name.ends_with(".sock") && !name.ends_with(".lock"));
        }
        let run = parent.join("run");
        if run.try_exists()? {
            for entry in fs::read_dir(run)? {
                let name = entry?.file_name();
                let name = name.to_string_lossy();
                ensure!(!name.ends_with(".sock") && !name.ends_with(".lock"));
            }
        }
        Ok(())
    }

    fn reject_sidecars(path: &Path) -> Result<()> {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            ensure!(fs::symlink_metadata(PathBuf::from(sidecar)).is_err_and(|error| {
                error.kind() == io::ErrorKind::NotFound
            }));
        }
        Ok(())
    }

    fn isolated_copy(path: &Path, destination: &Path) -> Result<(File, bool)> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        reject_artifacts(parent)?;
        let mut source = OpenOptions::new().read(true).write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        let metadata = source.metadata()?;
        ensure!(metadata.is_file() && metadata.nlink() == 1);
        // SAFETY: installs an async-signal-safe handler before acquiring the lease. F_SETLEASE
        // operates on our live FD. A failed/unsupported lease fails closed; it never falls back
        // to advisory locks. The lease is held until the entire benchmark/report completes.
        unsafe {
            ensure!(libc::signal(libc::SIGIO, lease_broken as libc::sighandler_t) != libc::SIG_ERR);
            ensure!(libc::fcntl(source.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) == 0);
        }
        reject_sidecars(path)?;
        reject_artifacts(parent)?;
        let mut target = OpenOptions::new().read(true).write(true).create_new(true)
            .mode(0o600).open(destination)?;
        // SAFETY: Linux FICLONE receives two live file descriptors; no pointers are passed.
        let reflink = unsafe {
            libc::ioctl(target.as_raw_fd(), 0x4004_9409 as libc::c_ulong, source.as_raw_fd()) == 0
        };
        if !reflink {
            io::copy(&mut source, &mut target)?;
        }
        target.sync_all()?;
        // Confirm the path still names the leased inode. Renames/replacements cannot make the
        // private copy unsafe, but must not silently change the caller's selected dataset.
        let current = fs::symlink_metadata(path)?;
        ensure!(metadata.dev() == current.dev() && metadata.ino() == current.ino());
        reject_sidecars(path)?;
        Ok((source, reflink))
    }

    fn summary(samples: &mut [Duration]) -> Value {
        samples.sort_unstable();
        let percentile = |percent: usize| {
            if samples.is_empty() {
                Value::Null
            } else {
                // Nearest rank, including the maximum for p99 when fewer than 100 samples.
                json!(samples[(samples.len() * percent).div_ceil(100) - 1].as_secs_f64() * 1000.0)
            }
        };
        json!({
            "count": samples.len(),
            "p50_ms": percentile(50),
            "p99_ms": percentile(99),
            "max_ms": samples.last().map(|d| d.as_secs_f64() * 1000.0),
        })
    }

    fn checkpointed_bytes(connection: &rusqlite::Connection) -> Result<u64> {
        let busy: u64 = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        ensure!(busy == 0);
        let pages: u64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let size: u64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(pages * size)
    }

    fn options() -> Result<(PathBuf, usize, bool, u64)> {
        let mut args = std::env::args_os().skip(1);
        let mut store = None;
        let mut n = None;
        let mut before = None;
        let mut bootstrap_yield_ms = None;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--store") if store.is_none() => store = Some(PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("argument"))?)),
                Some("--n") if n.is_none() => n = Some(args.next().and_then(|v| v.to_str().and_then(|v| v.parse::<usize>().ok())).ok_or_else(|| anyhow::anyhow!("argument"))?),
                Some("--bootstrap-yield-ms") if bootstrap_yield_ms.is_none() => {
                    bootstrap_yield_ms = Some(args.next().and_then(|v| v.to_str().and_then(|v| v.parse::<u64>().ok())).ok_or_else(|| anyhow::anyhow!("argument"))?);
                }
                Some("--mode") if before.is_none() => {
                    before = Some(match args.next().as_deref().and_then(|v| v.to_str()) {
                        Some("before") => true,
                        Some("after") => false,
                        _ => anyhow::bail!("argument"),
                    });
                }
                _ => anyhow::bail!("argument"),
            }
        }
        let n = n.ok_or_else(|| anyhow::anyhow!("argument"))?;
        ensure!(n > 0);
        Ok((store.ok_or_else(|| anyhow::anyhow!("argument"))?, n, before.ok_or_else(|| anyhow::anyhow!("argument"))?, bootstrap_yield_ms.unwrap_or(100)))
    }

    pub fn run() -> Result<()> {
        ensure!(!cfg!(debug_assertions));
        let (input, n, before, bootstrap_yield_ms) = options()?;
        let temp = tempfile::Builder::new().prefix("transport-bench-").tempdir()?;
        let path = temp.path().join("claims.db");
        let (_lease, reflink) = isolated_copy(&input, &path)?;
        // Both modes begin with precisely the same pre-PR side-index state, even when the
        // supplied snapshot was taken from a build that already completed the index.
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(REMOVE_INDEX)?;
        // Retain this historical lookup shape privately; only aggregate counts/timings leave
        // the process. Newest retained row gives a deterministic pair on identical inputs.
        let (subject, origin): (String, String) = connection.query_row(
            "SELECT subject, origin FROM claims
             WHERE kind='transport.observed' AND subject LIKE 'host/%'
             ORDER BY store_index DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let peer = subject.strip_prefix("host/").filter(|peer| !peer.is_empty())
            .ok_or_else(|| anyhow::anyhow!("historical peer"))?.to_owned();
        ensure!(!origin.is_empty());
        drop(connection);
        let store = Store::open(&path, origin, Arc::new(Plain))?;
        store.transport_bench_original_lookup.store(before, Ordering::Relaxed);
        let source_claim_rows: u64 = store.readers.get().query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))?;
        let sizing = rusqlite::Connection::open(&path)?;
        let index_bytes_dbstat: bool = sizing.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_module_list WHERE name='dbstat')", [], |r| r.get(0),
        )?;
        let initial_bytes = if index_bytes_dbstat { 0 } else { checkpointed_bytes(&sizing)? };
        let mut bootstrap = Vec::new();
        let mut copied = 0;
        let bootstrap_started = Instant::now();
        if !before {
            loop {
                let page = transport_observation_index::advance(&store.connection)?;
                bootstrap.push(page.write_hold);
                copied += page.copied;
                if page.ready { break; }
                // Only this example overrides pacing; transaction samples are unchanged.
                std::thread::sleep(Duration::from_millis(bootstrap_yield_ms));
            }
        }
        let bootstrap_elapsed = if before { Duration::ZERO } else { bootstrap_started.elapsed() };
        let bootstrap_pages = bootstrap.len();
        // Sizing and any fallback checkpoint are outside bootstrap/insert timing samples.
        let index_bytes: u64 = if before {
            0
        } else if index_bytes_dbstat {
            sizing.query_row(
                "SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name IN (
                 SELECT name FROM sqlite_master WHERE tbl_name IN (
                 'transport_observation_index','transport_observation_backfill'))",
                [], |r| r.get(0),
            )?
        } else {
            checkpointed_bytes(&sizing)?.saturating_sub(initial_bytes)
        };
        drop(sizing);
        // Seed stale evidence. Repeated up/last_success_at=0 calls still write even when
        // canonical timestamp ties choose an earlier up observation in the measured loop.
        store.record_transport_observation(&peer, "unknown", None, Some(0))?;
        store.record_transport_observation(&peer, "up", None, Some(0))?;
        // Discard open/setup batch samples; the measured loop is serial, with no other store
        // in this process, so every subsequent sample belongs to exactly one iteration.
        transport_bench::take();
        let mut writer = Vec::with_capacity(n.saturating_mul(2));
        let mut per_insert_writer = Vec::with_capacity(n);
        let mut wall = Vec::with_capacity(n);
        let payload = "x".repeat(1024);
        for i in 0..n {
            let input = ClaimInput {
                subject: format!("message/transport-bench-{i}"),
                kind: "message.posted".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("from".into(), json!("agent/transport-bench-peer")),
                    ("to".into(), json!("agent/transport-bench-recipient")),
                    ("text".into(), json!(payload)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            };
            let started = Instant::now();
            store.append_claim(&input)?;
            store.record_transport_observation(&peer, "up", None, Some(0))?;
            wall.push(started.elapsed());
            let samples = transport_bench::take();
            ensure!(samples.len() == 2);
            per_insert_writer.push(samples.iter().copied().sum());
            writer.extend(samples);
        }
        let report = json!({
            "ok": true,
            "mode": if before { "before" } else { "after" },
            "iterations": n,
            "source_claim_rows": source_claim_rows,
            "bootstrap_elapsed_ms": bootstrap_elapsed.as_secs_f64() * 1000.0,
            "bootstrap_pages": bootstrap_pages,
            "bootstrap_yield_ms": bootstrap_yield_ms,
            "index_bytes": index_bytes,
            "index_bytes_dbstat": index_bytes_dbstat,
            "reflink": reflink,
            "bootstrap_rows_copied": copied,
            "bootstrap_write_transactions": summary(&mut bootstrap),
            "insert_write_transactions": summary(&mut writer),
            "per_insert_writer_total": summary(&mut per_insert_writer),
            "per_insert_wall": summary(&mut wall),
        });
        // Finish cleanup before the sole success record; failures cannot append another JSON.
        drop(store);
        temp.close()?;
        println!("{report}");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nearest_rank_and_empty_samples() {
            let mut empty = [];
            assert_eq!(summary(&mut empty)["p50_ms"], Value::Null);
            let mut samples = [Duration::from_millis(3), Duration::from_millis(1), Duration::from_millis(2)];
            let result = summary(&mut samples);
            assert_eq!(result["count"], 3);
            assert_eq!(result["p50_ms"], 2.0);
            assert_eq!(result["p99_ms"], 3.0);
            assert_eq!(result["max_ms"], 3.0);
        }
    }
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    #[cfg(target_os = "linux")]
    let success = std::panic::catch_unwind(harness::run).is_ok_and(|result| result.is_ok());
    #[cfg(not(target_os = "linux"))]
    let success = false;
    if !success {
        println!("{{\"ok\":false}}");
        std::process::exit(1);
    }
}
