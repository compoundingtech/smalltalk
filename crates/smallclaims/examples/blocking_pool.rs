//! Linux microbenchmark: raw read checkout plus a synchronous writer queue wait.
//! This deliberately exercises the legacy/background path, not API read admission.
use smallclaims::sqlite::{ReadPool, WriterConnection};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

fn sample(label: &str, readers: &ReadPool) {
    let threads = std::fs::read_dir("/proc/self/task").unwrap().count();
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .unwrap();
    let usage = readers.usage();
    println!(
        "{label}: threads={threads} readers_open={} readers_idle={} readers_peak={} {rss}",
        usage.open, usage.idle, usage.peak
    );
}

fn main() -> anyhow::Result<()> {
    let admitted = std::env::args().any(|arg| arg == "--admitted");
    let writer_only = std::env::args().any(|arg| arg == "--writer-only");
    let n: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "128".into())
        .parse()?;
    let scratch = tempfile::tempdir()?;
    let path = scratch.path().join("claims.sqlite3");
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE claims(store_index INTEGER PRIMARY KEY); INSERT INTO claims VALUES(1); CREATE TABLE payload(data BLOB); WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1024) INSERT INTO payload SELECT zeroblob(4096) FROM n;")?;
    let writer = Arc::new(WriterConnection::new(
        connection,
        Arc::new(AtomicU64::new(1)),
    ));
    let readers = Arc::new(ReadPool::new(&path, false)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    sample("baseline", &readers);
    // No reader barrier or SQL work: writer contention alone occupies N blocking workers.
    let writer_hold = writer.write();
    let writer_waiters = Arc::new(AtomicUsize::new(0));
    let writer_jobs: Vec<_> = (0..n)
        .map(|_| {
            let writer = writer.clone();
            let writer_waiters = writer_waiters.clone();
            if admitted {
                let writing = writer.clone();
                runtime.spawn(async move {
                    writer
                        .spawn_blocking(move || {
                            writer_waiters.fetch_add(1, Ordering::SeqCst);
                            drop(writing.write());
                        })
                        .await
                        .unwrap();
                })
            } else {
                runtime.spawn_blocking(move || {
                    writer_waiters.fetch_add(1, Ordering::SeqCst);
                    drop(writer.write());
                })
            }
        })
        .collect();
    let wait_started = Instant::now();
    let expected = if admitted {
        n.min(smallclaims::sqlite::max_write_workers())
    } else {
        n
    };
    while writer_waiters.load(Ordering::SeqCst) < expected {
        anyhow::ensure!(
            wait_started.elapsed() < Duration::from_secs(60),
            "writer workers did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    sample("writer-only-held", &readers);
    drop(writer_hold);
    runtime.block_on(async {
        for job in writer_jobs {
            job.await.unwrap();
        }
    });
    sample("writer-only-released", &readers);
    std::thread::sleep(Duration::from_secs(12));
    sample("writer-only-idle-12s", &readers);
    if writer_only {
        return Ok(());
    }
    // Keep the author's raw-reader phase unchanged; it deliberately bypasses admission.
    let hold = writer.write();
    let ready = Arc::new(AtomicUsize::new(0));
    let queued = Arc::new(AtomicUsize::new(0));
    let cache_bytes = Arc::new(AtomicUsize::new(0));
    let statement_bytes = Arc::new(AtomicUsize::new(0));
    let read_barrier = Arc::new(std::sync::Barrier::new(n + 1));
    let started = Instant::now();
    let jobs: Vec<_> = (0..n)
        .map(|_| {
            let writer = writer.clone();
            let readers = readers.clone();
            let ready = ready.clone();
            let read_barrier = read_barrier.clone();
            let queued = queued.clone();
            let cache_bytes = cache_bytes.clone();
            let statement_bytes = statement_bytes.clone();
            runtime.spawn_blocking(move || {
                let reader = readers.get();
                reader
                    .query_row("SELECT sum(length(hex(data))) FROM payload", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap();
                for i in 0..128 {
                    reader
                        .prepare_cached(&format!("SELECT {i}"))
                        .unwrap()
                        .query_row([], |row| row.get::<_, i64>(0))
                        .unwrap();
                }
                for (operation, total) in [
                    (rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED, &cache_bytes),
                    (rusqlite::ffi::SQLITE_DBSTATUS_STMT_USED, &statement_bytes),
                ] {
                    let mut current = 0;
                    let mut highwater = 0;
                    // SAFETY: this worker exclusively owns its connection; counters are output pointers.
                    let result = unsafe {
                        rusqlite::ffi::sqlite3_db_status(
                            reader.handle(),
                            operation,
                            &mut current,
                            &mut highwater,
                            0,
                        )
                    };
                    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
                    total.fetch_add(current as usize, Ordering::SeqCst);
                }
                ready.fetch_add(1, Ordering::SeqCst);
                read_barrier.wait();
                // Release the read before waiting on the writer, as real mixed handlers do.
                drop(reader);
                queued.fetch_add(1, Ordering::SeqCst);
                drop(writer.write());
            })
        })
        .collect();
    while ready.load(Ordering::SeqCst) < n {
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(60),
            "workers did not reach writer wait"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    sample("writer-held", &readers);
    println!(
        "sqlite_cache_bytes={} sqlite_statement_bytes={}",
        cache_bytes.load(Ordering::SeqCst),
        statement_bytes.load(Ordering::SeqCst)
    );
    read_barrier.wait();
    while queued.load(Ordering::SeqCst) < n {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    sample("writer-queued", &readers);
    drop(hold);
    runtime.block_on(async {
        for job in jobs {
            job.await.unwrap();
        }
    });
    sample("released", &readers);
    std::thread::sleep(Duration::from_secs(12));
    sample("idle-12s", &readers);
    Ok(())
}
