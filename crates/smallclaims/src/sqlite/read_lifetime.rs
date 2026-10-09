//! Checkout diagnostics, not SQLite lock attribution. A loan need not hold a transaction;
//! only Store::read_snapshot reports a known snapshot start after its first database read.

use super::{AtomicU64, Mutex, Ordering, PoisonError, ReadConnection};
use crate::read_budget::ReadBudget;
use std::collections::BTreeMap;
use std::panic::Location;
use std::time::Instant;

static LIVE_READS: Mutex<BTreeMap<u64, LiveRead>> = Mutex::new(BTreeMap::new());
static NEXT_LIVE_READ: AtomicU64 = AtomicU64::new(1);
static NEXT_POOL: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_pool_id() -> u64 {
    NEXT_POOL.fetch_add(1, Ordering::Relaxed)
}

struct LiveRead {
    started: Instant,
    snapshot_started: Option<Instant>,
    pool: Option<u64>,
    connection: Option<u64>,
    at: &'static Location<'static>,
    snapshot: bool,
    thread: std::thread::ThreadId,
    budget: Option<ReadBudget>,
}

/// Ends the live-read entry after physical connection cleanup on every exit path.
pub struct LiveReadToken(u64);

impl LiveReadToken {
    pub(crate) fn snapshot_started(&self, connection: &ReadConnection) {
        if let Some(read) = LIVE_READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.0)
        {
            read.snapshot_started = Some(Instant::now());
            read.connection = Some(connection.id);
        }
    }
}

impl Drop for LiveReadToken {
    fn drop(&mut self) {
        LIVE_READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

#[track_caller]
fn register(pool: Option<u64>, snapshot: bool, connection: Option<u64>) -> LiveReadToken {
    let id = NEXT_LIVE_READ.fetch_add(1, Ordering::Relaxed);
    LIVE_READS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            id,
            LiveRead {
                started: Instant::now(),
                snapshot_started: None,
                pool,
                connection,
                at: Location::caller(),
                snapshot,
                thread: std::thread::current().id(),
                budget: crate::read_budget::current(),
            },
        );
    LiveReadToken(id)
}

/// Compatibility process-wide entry with unknown pool/connection and transaction age.
#[track_caller]
pub fn register_live_read(snapshot: bool) -> LiveReadToken {
    register(None, snapshot, None)
}

#[track_caller]
pub(super) fn register_in_pool(
    pool: u64,
    snapshot: bool,
    connection: Option<u64>,
) -> LiveReadToken {
    register(Some(pool), snapshot, connection)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldestLiveRead {
    pub age_ms: u128,
    pub snapshot: bool,
    pub at: String,
    pub live: usize,
}

/// Compatibility process-wide oldest checkout. Its age is not WAL pin age.
pub fn oldest_live_read() -> Option<OldestLiveRead> {
    let reads = LIVE_READS.lock().unwrap_or_else(PoisonError::into_inner);
    let oldest = reads.values().min_by_key(|read| read.started)?;
    Some(OldestLiveRead {
        age_ms: oldest.started.elapsed().as_millis(),
        snapshot: oldest.snapshot,
        at: format!("{}:{}", oldest.at.file(), oldest.at.line()),
        live: reads.len(),
    })
}

#[cfg(any(test, feature = "test-support"))]
pub fn live_read_locations() -> Vec<(String, bool)> {
    LIVE_READS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .map(|read| {
            (
                format!("{}:{}", read.at.file(), read.at.line()),
                read.snapshot,
            )
        })
        .collect()
}

/// At most three candidate entries: this pool and separately unpooled process entries.
/// Unpooled database/connection identity is unknown (for example VACUUM INTO). Connection IDs
/// are pool-local; no checkout or oldest known snapshot proves WAL frame ownership.
#[derive(Debug, serde::Serialize)]
pub struct ReadLifetimeReport {
    pub process_id: u32,
    pub pool_id: u64,
    pub live_checkouts: usize,
    pub snapshot_checkouts: usize,
    pub oldest_snapshot: Option<ReadLifetime>,
    pub oldest_loan: Option<ReadLifetime>,
    pub process_unpooled_checkouts: usize,
    pub oldest_process_unpooled: Option<ReadLifetime>,
}

#[derive(Debug, serde::Serialize)]
pub struct ReadLifetime {
    pub read_id: u64,
    pub connection_id: Option<u64>,
    pub snapshot_checkout: bool,
    pub checkout_age_ms: u128,
    /// Since the first snapshot read completed: a lower bound on transaction age.
    /// Unknown for ordinary/raw transactions and snapshots awaiting their first read.
    pub snapshot_confirmed_age_ms: Option<u128>,
    pub at: String,
    pub worker_thread: String,
    /// A route label, bounded to 256 characters; never a request payload.
    pub route: Option<String>,
    pub budget_expired: Option<bool>,
    pub deadline_remaining_ms: Option<u128>,
}

fn describe(id: u64, read: &LiveRead) -> ReadLifetime {
    ReadLifetime {
        read_id: id,
        connection_id: read.connection,
        snapshot_checkout: read.snapshot,
        checkout_age_ms: read.started.elapsed().as_millis(),
        snapshot_confirmed_age_ms: read
            .snapshot_started
            .map(|start| start.elapsed().as_millis()),
        at: format!("{}:{}", read.at.file(), read.at.line()),
        worker_thread: format!("{:?}", read.thread),
        route: read
            .budget
            .as_ref()
            .map(|budget| budget.route().chars().take(256).collect()),
        budget_expired: read.budget.as_ref().map(ReadBudget::expired),
        deadline_remaining_ms: read
            .budget
            .as_ref()
            .map(|budget| budget.remaining().as_millis()),
    }
}

pub(super) fn report(pool: u64) -> ReadLifetimeReport {
    let reads = LIVE_READS.lock().unwrap_or_else(PoisonError::into_inner);
    let mut report = ReadLifetimeReport {
        process_id: std::process::id(),
        pool_id: pool,
        live_checkouts: 0,
        snapshot_checkouts: 0,
        oldest_snapshot: None,
        oldest_loan: None,
        process_unpooled_checkouts: 0,
        oldest_process_unpooled: None,
    };
    let mut snapshot = None;
    let mut loan = None;
    let mut unpooled = None;
    for (&id, read) in &*reads {
        if read.pool.is_none() {
            report.process_unpooled_checkouts += 1;
            if unpooled
                .as_ref()
                .is_none_or(|(_, old): &(u64, &LiveRead)| read.started < old.started)
            {
                unpooled = Some((id, read));
            }
            continue;
        }
        if read.pool != Some(pool) {
            continue;
        }
        report.live_checkouts += 1;
        report.snapshot_checkouts += usize::from(read.snapshot);
        let oldest = if read.snapshot {
            &mut snapshot
        } else {
            &mut loan
        };
        if oldest
            .as_ref()
            .is_none_or(|(_, old): &(u64, &LiveRead)| read.started < old.started)
        {
            *oldest = Some((id, read));
        }
    }
    report.oldest_snapshot = snapshot.map(|(id, read)| describe(id, read));
    report.oldest_loan = loan.map(|(id, read)| describe(id, read));
    report.oldest_process_unpooled = unpooled.map(|(id, read)| describe(id, read));
    report
}

#[cfg(test)]
mod tests;
