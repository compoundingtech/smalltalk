//! Bounded, invented-data reader-cache measurement; never opens a live store.
//! cargo run -p smallclaims --example reader_memory -- 8192 32 3
use std::{path::Path, time::Instant};

use anyhow::{Result, ensure};
use rusqlite::Connection;
use smallclaims::sqlite::ReadPool;

fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the provided structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

fn scan(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare_cached("SELECT data FROM payload")?;
    let mut rows = statement.query([])?;
    let mut bytes = 0;
    while let Some(row) = rows.next()? {
        bytes += row.get_ref(0)?.as_blob()?.len();
    }
    ensure!(
        bytes == 8192 * 2000,
        "all synthetic payload bytes were read"
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = std::env::args()
        .skip(1)
        .map(|n| n.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let cache_kib = args.first().copied().unwrap_or(8192);
    let readers = args.get(1).copied().unwrap_or(32);
    let waves = args.get(2).copied().unwrap_or(3);
    ensure!(
        (1..=8192).contains(&cache_kib) && (1..=40).contains(&readers) && (1..=10).contains(&waves),
        "bounded measurement: cache 1..8192 KiB, readers 1..40, waves 1..10"
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("synthetic.sqlite3");
    let mut writer = Connection::open(&path)?;
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE payload(data BLOB);")?;
    let transaction = writer.transaction()?;
    let data = vec![b'x'; 2000];
    for _ in 0..8192 {
        transaction.execute("INSERT INTO payload VALUES(?1)", [&data])?;
    }
    // Schema parsing and prepared-statement retention as well as data-page caches.
    for n in 0..128 {
        transaction.execute_batch(&format!("CREATE TABLE synthetic_{n}(id INTEGER PRIMARY KEY, value TEXT); CREATE INDEX synthetic_{n}_value ON synthetic_{n}(value);"))?;
    }
    transaction.commit()?;
    let version: String = writer.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    let memstatus: String = writer.query_row("SELECT compile_options FROM pragma_compile_options WHERE compile_options LIKE 'DEFAULT_MEMSTATUS=%'", [], |row| row.get(0))?;
    drop(writer);
    let pool = ReadPool::new(Path::new(&path), false)?;
    println!("SQLite {version} {memstatus}; cache_kib={cache_kib}; readers={readers}");
    for wave in 1..=waves {
        let before = cpu_ms();
        let started = Instant::now();
        let guards: Vec<_> = (0..readers).map(|_| pool.get()).collect();
        let checkout_ms = started.elapsed().as_secs_f64() * 1000.0;
        for connection in &guards {
            connection.pragma_update(None, "cache_size", -(cache_kib as i64))?;
            scan(connection)?;
            for n in 0..128 {
                let sql = format!("SELECT value FROM synthetic_{n} WHERE id=?1");
                let mut statement = connection.prepare_cached(&sql)?;
                let _ = statement.exists([1])?;
            }
        }
        println!(
            "wave={wave} cpu_ms={:.3} wall_ms={:.3} checkout_ms={checkout_ms:.3} held_rss_kib={:?}",
            cpu_ms() - before,
            started.elapsed().as_secs_f64() * 1000.0,
            rss_kib()
        );
        drop(guards);
        println!(
            "idle={} retained_rss_kib={:?}",
            pool.idle.lock().unwrap().len(),
            rss_kib()
        );
    }
    Ok(())
}
