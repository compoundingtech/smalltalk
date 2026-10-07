//! Explicit bounded, resumable catch-up for fresh opt-in claims-only stores.
//! Raw claim admission and its queue entry commit together. Queue loss/corruption stops
//! catch-up; reads stay unavailable. No GET/open/repair/checkpoint rebuild is provided.

use super::{Readiness, SourceCut, Views, events, fence_error, runtime::ViewRuntime, source_cut};
use crate::{
    Store,
    store::{canonical, claim_from_row, current_index},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{broadcast, watch};

const FORMAT: &str = "smallclaims.ivm.async.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub queue_rows: usize,
    pub queue_bytes: usize,
    pub page_rows: usize,
    pub page_bytes: usize,
    /// Maximum earlier legacy claims considered by canonical COUNT. Recorded MIN rank
    /// is already indexed. Larger unsupported legacy batches fence the affected view.
    pub legacy_rank_rows: usize,
}
impl Limits {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.queue_rows),
            "async queue row bound outside 1..=4096"
        );
        ensure!(
            (1..=16 * 1024 * 1024).contains(&self.queue_bytes),
            "async queue byte bound outside 1..=16MiB"
        );
        ensure!(
            (1..=128).contains(&self.page_rows) && self.page_rows <= self.queue_rows,
            "async page row bound outside queue/1..=128"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.page_bytes) && self.page_bytes <= self.queue_bytes,
            "async page byte bound outside queue/1..=1MiB"
        );
        ensure!(
            (1..=128).contains(&self.legacy_rank_rows),
            "async legacy rank bound outside 1..=128"
        );
        Ok(())
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            queue_rows: 4096,
            queue_bytes: 16 * 1024 * 1024,
            page_rows: 8,
            page_bytes: 256 * 1024,
            legacy_rank_rows: 32,
        }
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_async_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), format TEXT NOT NULL,
 queue_rows INTEGER NOT NULL, queue_bytes INTEGER NOT NULL, page_rows INTEGER NOT NULL,
 page_bytes INTEGER NOT NULL, legacy_rank_rows INTEGER NOT NULL,
 retained_rows INTEGER NOT NULL DEFAULT 0, retained_bytes INTEGER NOT NULL DEFAULT 0,
 available INTEGER NOT NULL DEFAULT 1, error TEXT,
 capturing INTEGER NOT NULL DEFAULT 0, draining INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS ivm_async_queue (
 store_index INTEGER PRIMARY KEY, claim_id TEXT NOT NULL UNIQUE, bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ivm_async_views (
 view TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL, through INTEGER NOT NULL,
 complete INTEGER NOT NULL DEFAULT 1
);
CREATE TRIGGER IF NOT EXISTS ivm_async_view_fence AFTER UPDATE OF ready ON ivm_views
WHEN NEW.ready=0 AND OLD.ready<>NEW.ready
BEGIN UPDATE ivm_async_views SET complete=0 WHERE view=NEW.name; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_view_delete AFTER DELETE ON ivm_async_views
BEGIN UPDATE ivm_async_state SET available=0,error='async prefix metadata deletion requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_view_update AFTER UPDATE ON ivm_async_views
WHEN OLD.fingerprint<>NEW.fingerprint OR OLD.epoch<>NEW.epoch
 OR (OLD.complete=0 AND NEW.complete<>0)
BEGIN UPDATE ivm_async_state SET available=0,error='async prefix metadata replacement requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_view_insert AFTER INSERT ON ivm_async_views
WHEN (SELECT admitted FROM ivm_source WHERE singleton=1)>0
BEGIN UPDATE ivm_async_state SET available=0,error='async prefix metadata installation requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_admit AFTER INSERT ON claims
BEGIN
 UPDATE ivm_async_state SET capturing=1 WHERE singleton=1;
 INSERT INTO ivm_async_queue SELECT NEW.store_index,NEW.id,
  length(CAST(NEW.body AS BLOB))+length(CAST(NEW.predecessors AS BLOB))+length(CAST(NEW.id AS BLOB))+length(CAST(NEW.batch_id AS BLOB))+length(CAST(NEW.subject AS BLOB))+length(CAST(NEW.kind AS BLOB))+length(CAST(NEW.origin AS BLOB))+length(CAST(COALESCE(NEW.actor,'') AS BLOB))+length(CAST(NEW.accepted_at_unix_ms AS BLOB))
  FROM ivm_async_state WHERE available=1 AND retained_rows<queue_rows
   AND length(CAST(NEW.body AS BLOB))+length(CAST(NEW.predecessors AS BLOB))+length(CAST(NEW.id AS BLOB))+length(CAST(NEW.batch_id AS BLOB))+length(CAST(NEW.subject AS BLOB))+length(CAST(NEW.kind AS BLOB))+length(CAST(NEW.origin AS BLOB))+length(CAST(COALESCE(NEW.actor,'') AS BLOB))+length(CAST(NEW.accepted_at_unix_ms AS BLOB))<=queue_bytes-retained_bytes;
 UPDATE ivm_async_state SET available=0,error='async queue quota exceeded; explicit recovery required'
  WHERE available=1 AND NOT EXISTS(SELECT 1 FROM ivm_async_queue WHERE store_index=NEW.store_index);
 UPDATE ivm_async_state SET retained_rows=retained_rows+1,
  retained_bytes=retained_bytes+length(CAST(NEW.body AS BLOB))+length(CAST(NEW.predecessors AS BLOB))+length(CAST(NEW.id AS BLOB))+length(CAST(NEW.batch_id AS BLOB))+length(CAST(NEW.subject AS BLOB))+length(CAST(NEW.kind AS BLOB))+length(CAST(NEW.origin AS BLOB))+length(CAST(COALESCE(NEW.actor,'') AS BLOB))+length(CAST(NEW.accepted_at_unix_ms AS BLOB))
  WHERE EXISTS(SELECT 1 FROM ivm_async_queue WHERE store_index=NEW.store_index);
 UPDATE ivm_source SET admitted=MAX(admitted,NEW.store_index) WHERE singleton=1;
 UPDATE ivm_async_state SET capturing=0 WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS ivm_async_queue_insert AFTER INSERT ON ivm_async_queue
WHEN (SELECT capturing FROM ivm_async_state WHERE singleton=1)<>1
BEGIN UPDATE ivm_async_state SET available=0,error='async delta insertion requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_queue_update AFTER UPDATE ON ivm_async_queue
BEGIN UPDATE ivm_async_state SET available=0,error='async delta mutation requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_queue_delete AFTER DELETE ON ivm_async_queue
WHEN (SELECT draining FROM ivm_async_state WHERE singleton=1)<>1
BEGIN UPDATE ivm_async_state SET available=0,error='async delta deletion requires explicit recovery'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_claim_delete AFTER DELETE ON claims
BEGIN
 UPDATE ivm_async_state SET available=0,error='async source deletion requires explicit recovery';
END;
CREATE TRIGGER IF NOT EXISTS ivm_async_claim_update AFTER UPDATE ON claims
WHEN OLD.id<>NEW.id OR OLD.store_index<>NEW.store_index OR OLD.kind<>NEW.kind
 OR OLD.subject<>NEW.subject OR OLD.actor IS NOT NEW.actor OR OLD.body<>NEW.body
 OR OLD.batch_id<>NEW.batch_id OR OLD.predecessors<>NEW.predecessors
 OR OLD.origin<>NEW.origin OR OLD.accepted_at_unix_ms<>NEW.accepted_at_unix_ms
BEGIN
 UPDATE ivm_async_state SET available=0,error='async source mutation requires explicit recovery';
END;
CREATE TRIGGER IF NOT EXISTS ivm_async_prefix_mutation AFTER UPDATE OF projected,epoch ON ivm_source
WHEN OLD.epoch<>NEW.epoch OR NEW.projected<OLD.projected
 OR EXISTS(SELECT 1 FROM ivm_async_queue WHERE store_index<=NEW.projected)
BEGIN UPDATE ivm_async_state SET available=0,error='async prefix publication skipped captured input; explicit recovery required'; END;
CREATE TRIGGER IF NOT EXISTS ivm_async_status AFTER UPDATE OF available,error ON ivm_async_state
WHEN OLD.available<>NEW.available OR OLD.error IS NOT NEW.error
BEGIN
 UPDATE ivm_status_frontier SET sequence=sequence+1,source_sequence=sequence+1 WHERE singleton=1;
END;
"#;

pub(super) fn installed(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ivm_async_state')",
        [],
        |r| r.get(0),
    )?)
}
pub(super) fn check_owner(connection: &Connection, limits: Option<Limits>) -> Result<()> {
    if installed(connection)? {
        ensure!(
            limits.is_some(),
            "asynchronous IVM file requires its asynchronous runtime owner"
        );
    } else if limits.is_some() {
        ensure!(
            current_index(connection)? == 0,
            "nonempty source needs explicit asynchronous installation; automatic conversion disabled"
        );
    }
    Ok(())
}
pub(super) fn create_schema(connection: &Connection, limits: Limits) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    connection.execute("INSERT OR IGNORE INTO ivm_async_state(singleton,format,queue_rows,queue_bytes,page_rows,page_bytes,legacy_rank_rows) VALUES(1,?1,?2,?3,?4,?5,?6)",
        params![FORMAT,limits.queue_rows,limits.queue_bytes,limits.page_rows,limits.page_bytes,limits.legacy_rank_rows])?;
    let stored = state(connection)?;
    ensure!(
        stored.limits == limits,
        "asynchronous limits changed; explicit installation required"
    );
    Ok(())
}
pub(super) fn initialize(
    tx: &Transaction<'_>,
    views: &Views,
    cut: SourceCut,
    _limits: Limits,
) -> Result<()> {
    let registered: usize =
        tx.query_row("SELECT COUNT(*) FROM ivm_async_views", [], |r| r.get(0))?;
    ensure!(
        registered == 0 || registered == views.views.len(),
        "async registry changed; explicit installation required"
    );
    for (index, view) in views.views.iter().enumerate() {
        let name = view.definition().name;
        let existing: Option<(String, u64)> = tx
            .query_row(
                "SELECT fingerprint,epoch FROM ivm_async_views WHERE view=?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((fingerprint, epoch)) = existing {
            ensure!(
                fingerprint == views.fingerprints[index] && epoch == cut.epoch,
                "async view version/epoch incompatible"
            );
        } else {
            ensure!(
                cut.admitted == 0,
                "missing async view requires explicit installation"
            );
            tx.execute(
                "INSERT INTO ivm_async_views(view,fingerprint,epoch,through) VALUES(?1,?2,?3,0)",
                params![name, views.fingerprints[index], cut.epoch],
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub limits: Limits,
    pub retained_rows: usize,
    pub retained_bytes: usize,
    pub available: bool,
    pub error: Option<String>,
}
pub fn state(connection: &Connection) -> Result<State> {
    let (format,state):(String,State)=connection.query_row("SELECT format,queue_rows,queue_bytes,page_rows,page_bytes,legacy_rank_rows,retained_rows,retained_bytes,available,error FROM ivm_async_state WHERE singleton=1",[],|r|Ok((r.get(0)?,State {
        limits:Limits {queue_rows:r.get(1)?,queue_bytes:r.get(2)?,page_rows:r.get(3)?,page_bytes:r.get(4)?,legacy_rank_rows:r.get(5)?},
        retained_rows:r.get(6)?,retained_bytes:r.get(7)?,available:r.get(8)?,error:r.get(9)?,
    })))?;
    ensure!(format == FORMAT, "async format incompatible");
    state.limits.validate()?;
    ensure!(
        state.retained_rows <= state.limits.queue_rows
            && state.retained_bytes <= state.limits.queue_bytes,
        "async accounting exceeds quota"
    );
    Ok(state)
}

/// Explicit prefix progress, not MAX of input IDs seen by a reducer. A fenced view never
/// advances this evidence after its first missed input; a rebuilt view needs explicit adoption.
pub fn applied_prefix(
    connection: &Connection,
    views: &Views,
    view: &str,
    epoch: u64,
) -> Result<Option<u64>> {
    if !installed(connection)? {
        return Ok(None);
    }
    let index = views
        .views
        .iter()
        .position(|v| v.definition().name == view)
        .context("unknown IVM view")?;
    let row: Option<(String, u64, bool)> = connection
        .query_row(
            "SELECT fingerprint,epoch,complete FROM ivm_async_views WHERE view=?1",
            [view],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if row.is_some_and(|(fingerprint, stored_epoch, complete)| {
        fingerprint == views.fingerprints[index] && stored_epoch == epoch && complete
    }) {
        return Ok(source_cut(connection)?.map(|cut| cut.projected));
    }
    Ok(None)
}

pub(super) fn readiness(connection: &Connection) -> Result<bool> {
    Ok(!installed(connection)? || state(connection)?.available)
}
pub(super) fn error(connection: &Connection) -> Result<Option<String>> {
    if !installed(connection)? {
        return Ok(None);
    }
    Ok(state(connection)?.error)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageStatus {
    CaughtUp,
    More,
    Stopped(String),
}
#[derive(Clone, Debug)]
pub struct PageReport {
    pub status: PageStatus,
    pub source_cut: SourceCut,
    pub rows: usize,
    pub bytes: usize,
    pub retained_rows: usize,
    pub retained_bytes: usize,
    /// Queue wait is separated from the commit-inclusive held interval.
    pub writer_wait_us: u128,
    /// From obtaining the lent connection through COMMIT, observers and guard return.
    /// Includes scheduling delay while owning it; it is not pre-commit callback duration.
    pub writer_held_us: u128,
}

fn recorded_or_bounded_legacy(
    tx: &Transaction<'_>,
    claim: &crate::ClaimRecord,
    limit: usize,
) -> Result<bool> {
    let recorded: Option<u64> = tx.query_row(
        "SELECT MIN(position) FROM replica_records WHERE claim_id=?1",
        [&claim.id],
        |r| r.get(0),
    )?;
    if recorded.is_some() {
        return Ok(true);
    }
    let mut statement=tx.prepare_cached("SELECT store_index FROM claims WHERE batch_id=?1 AND store_index<?2 ORDER BY store_index LIMIT ?3")?;
    let count = statement
        .query_map(
            params![claim.batch_id, claim.store_index, limit + 1],
            |_| Ok(()),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .len();
    Ok(count <= limit)
}

fn page_tx(tx: &Transaction<'_>, runtime: &ViewRuntime) -> Result<PageReport> {
    let state = state(tx)?;
    let previous = source_cut(tx)?.context("async source missing")?;
    if !state.available {
        return Ok(report(
            PageStatus::Stopped(
                state
                    .error
                    .clone()
                    .unwrap_or_else(|| "async source unavailable".into()),
            ),
            previous,
            0,
            0,
            &state,
        ));
    }
    let limits = runtime
        .asynchronous
        .context("synchronous runtime cannot drain async queue")?;
    ensure!(limits == state.limits, "async runtime bounds incompatible");
    let mut statement = tx.prepare_cached(
        "SELECT store_index,claim_id,bytes FROM ivm_async_queue ORDER BY store_index LIMIT ?1",
    )?;
    let candidates = statement
        .query_map([limits.page_rows + 1], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, usize>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut page = Vec::new();
    let mut bytes = 0;
    for candidate in &candidates {
        if page.len() == limits.page_rows || bytes + candidate.2 > limits.page_bytes {
            break;
        }
        ensure!(
            candidate.0 > previous.projected,
            "async queue precedes its committed prefix"
        );
        bytes += candidate.2;
        page.push(candidate);
    }
    if page.is_empty() && !candidates.is_empty() {
        tx.execute("UPDATE ivm_async_state SET available=0,error='async input exceeds page byte bound; explicit recovery required'",[])?;
        let state = state_after(tx)?;
        return Ok(report(
            PageStatus::Stopped(state.error.clone().unwrap_or_default()),
            previous,
            0,
            0,
            &state,
        ));
    }
    let mut prefixes = BTreeMap::new();
    let mut touched = BTreeSet::new();
    for view in &runtime.views.views {
        let name = view.definition().name;
        let Some(prefix) = applied_prefix(tx, &runtime.views, name, previous.epoch)? else {
            // A fenced view cannot certify a prefix. Healthy views still make progress.
            continue;
        };
        if prefix == previous.projected
            && matches!(
                runtime.views.view_readiness(tx, name, previous.epoch)?,
                Readiness::Ready(_)
            )
        {
            prefixes.insert(name.to_owned(), prefix);
        }
    }
    for &&(index, ref id, _) in &page {
        let claim=tx.query_row("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE id=?1",[id],claim_from_row).optional()?.context("async captured input missing; explicit recovery required")?;
        ensure!(
            claim.store_index == index,
            "async captured input position changed"
        );
        let mut missed = BTreeSet::new();
        if runtime.views.reads_kind(&claim.kind) {
            touched.extend(
                runtime
                    .views
                    .subscribers(&claim.kind)
                    .into_iter()
                    .map(|index| runtime.views.views[index].definition().name.to_owned()),
            );
            if recorded_or_bounded_legacy(tx, &claim, limits.legacy_rank_rows)? {
                let rank = canonical::claim_key(tx, &claim.id)?;
                missed = runtime
                    .views
                    .change(tx, None, Some((&claim, &rank)), previous.epoch)?
                    .deferred;
            } else {
                for subscriber in runtime.views.subscribers(&claim.kind) {
                    let name = runtime.views.views[subscriber].definition().name;
                    fence_error(
                        tx,
                        name,
                        &anyhow::anyhow!(
                            "async legacy rank exceeds bounded prefix; explicit recovery required"
                        ),
                    )?;
                    missed.insert(name.to_owned());
                }
            }
        }
        for name in missed {
            prefixes.remove(&name);
        }
        for through in prefixes.values_mut() {
            *through = index;
        }
    }
    let through = if page.len() == candidates.len() {
        previous.admitted
    } else {
        page.last().map(|c| c.0).unwrap_or(previous.projected)
    };
    // Every queue insertion was captured in the source admission transaction. Holes from
    // duplicate allocation contain no input. Only an empty, intact queue certifies its tail.
    for (name, _) in prefixes {
        if !touched.contains(&name) {
            continue;
        }
        tx.execute(
            "UPDATE ivm_async_views SET through=?2 WHERE view=?1",
            params![name, through],
        )?;
    }
    if !page.is_empty() {
        tx.execute(
            "UPDATE ivm_async_state SET draining=1 WHERE singleton=1",
            [],
        )?;
        tx.execute(
            "DELETE FROM ivm_async_queue WHERE store_index<=?1",
            [through],
        )?;
        tx.execute("UPDATE ivm_async_state SET retained_rows=retained_rows-?1,retained_bytes=retained_bytes-?2 WHERE singleton=1",params![page.len(),bytes])?;
    }
    tx.execute(
        "UPDATE ivm_async_state SET draining=0 WHERE singleton=1",
        [],
    )?;
    let cut = SourceCut {
        projected: through,
        ..previous
    };
    runtime.views.publish_cut(tx, cut)?;
    let state = state_after(tx)?;
    Ok(report(
        if state.retained_rows == 0 {
            PageStatus::CaughtUp
        } else {
            PageStatus::More
        },
        cut,
        page.len(),
        bytes,
        &state,
    ))
}
fn state_after(tx: &Transaction<'_>) -> Result<State> {
    state(tx)
}
fn report(
    status: PageStatus,
    source_cut: SourceCut,
    rows: usize,
    bytes: usize,
    state: &State,
) -> PageReport {
    PageReport {
        status,
        source_cut,
        rows,
        bytes,
        retained_rows: state.retained_rows,
        retained_bytes: state.retained_bytes,
        writer_wait_us: 0,
        writer_held_us: 0,
    }
}

/// Exactly one bounded transaction. The caller yields between pages; no snapshot is retained.
/// Database errors propagate and roll back the page. Operator failures remain fenced while
/// other registered views can complete. Quota overflow preserves raw input and stops recovery.
pub fn step(store: &Store, runtime: &ViewRuntime) -> Result<PageReport> {
    let waiting = Instant::now();
    let mut writer = store.connection.write();
    let held = Instant::now();
    let wait_us = waiting.elapsed().as_micros();
    let tx = writer.transaction()?;
    let mut result = page_tx(&tx, runtime)?;
    tx.commit()?;
    drop(writer);
    result.writer_wait_us = wait_us;
    result.writer_held_us = held.elapsed().as_micros();
    Ok(result)
}

pub struct Worker {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    progress: watch::Receiver<Option<PageReport>>,
}
impl Worker {
    /// Explicit owner scheduling only. Subscribe to the same Store before starting. The
    /// worker drains one page on a blocking task, yields, and awaits commit notices when idle.
    /// No retries on database/error/unknown queue evidence; stop retains resumable progress.
    pub fn start(
        store: Arc<Store>,
        runtime: Arc<ViewRuntime>,
        mut notices: broadcast::Receiver<events::Notice>,
    ) -> Result<Self> {
        ensure!(
            runtime.asynchronous.is_some(),
            "async worker requires asynchronous runtime"
        );
        let (stop, mut cancelled) = watch::channel(false);
        let (progress, receiver) = watch::channel(None);
        let executor = tokio::runtime::Handle::try_current()
            .context("async worker requires a Tokio runtime")?;
        let task = executor.spawn(async move {
            loop {
                if *cancelled.borrow() || cancelled.has_changed().is_err() {
                    return Ok(());
                }
                let page_store = store.clone();
                let page_runtime = runtime.clone();
                let page = tokio::task::spawn_blocking(move || step(&page_store, &page_runtime))
                    .await
                    .context("async page task failed")??;
                let status = page.status.clone();
                let _ = progress.send(Some(page));
                match status {
                    PageStatus::Stopped(reason) => bail!("{reason}"),
                    PageStatus::More => tokio::task::yield_now().await,
                    PageStatus::CaughtUp => tokio::select! {
                        changed=cancelled.changed()=>{if changed.is_err() || *cancelled.borrow_and_update(){return Ok(());}},
                        notice=notices.recv()=>match notice {
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_))=>{},
                            Err(broadcast::error::RecvError::Closed)=>return Ok(()),
                        },
                    },
                }
            }
        });
        Ok(Self {
            stop,
            task: Some(task),
            progress: receiver,
        })
    }
    pub fn progress(&self) -> watch::Receiver<Option<PageReport>> {
        self.progress.clone()
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await.context("async worker stopped unexpectedly")??;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
