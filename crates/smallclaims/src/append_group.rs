//! Opt-in replica grouping within the existing writer transaction and savepoints.
//!
//! A runtime supplies an exact retention policy. No job is delayed to fill a group. Only
//! successful, released savepoints publish reusable state; the scope ends before COMMIT.
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
#[cfg(debug_assertions)]
use anyhow::ensure;
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::{
    hash::{batch_header_hash, claim_hash},
    store::{
        checkpoint_agreement::WriteClock, collect_hash_fields, next_replica_sequence,
        previous_batch_hash,
    },
};

static RESTORES_SKIPPED: AtomicU64 = AtomicU64::new(0);
static UNOBSERVED_MUTATIONS: AtomicU64 = AtomicU64::new(0);
static WITNESS_FAILURES: AtomicU64 = AtomicU64::new(0);
static NEXT_WITNESS: AtomicU64 = AtomicU64::new(0);
const WITNESS_PREFIX: &str = "_smallclaims_append_witness_";
static REUSED_BATCHES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub restores_skipped: u64,
    pub unobserved_mutations: u64,
    pub reused_batches: u64,
    pub witness_failures: u64,
}
pub fn counters() -> Counters {
    Counters {
        restores_skipped: RESTORES_SKIPPED.load(Ordering::Relaxed),
        unobserved_mutations: UNOBSERVED_MUTATIONS.load(Ordering::Relaxed),
        reused_batches: REUSED_BATCHES.load(Ordering::Relaxed),
        witness_failures: WITNESS_FAILURES.load(Ordering::Relaxed),
    }
}

/// The class must have no drop rule under the runtime's current checkpoint rules.
pub type AppendPolicy = fn(&str, Option<&str>, &Value) -> Option<&'static str>;
pub const REPLICA_BATCH_CLAIMS: usize = 32;
pub const REPLICA_BATCH_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
struct Batch {
    id: String,
    sequence: u64,
    previous: Option<String>,
    hash: String,
    at: u128,
    class: &'static str,
    actor: Option<String>,
    claims: HashSet<String>,
    bytes: usize,
}

#[derive(Clone, Debug)]
struct Frontier {
    origin: String,
    sequence: u64,
    previous: Option<String>,
    clock: WriteClock,
    batch: Option<Batch>,
}

struct Active {
    connection: usize,
    epoch: u64,
    changes: u64,
    notifications: u64,
    disabled: bool,
    policy: Option<AppendPolicy>,
    committed: Option<Frontier>,
    working: Option<Frontier>,
    witness: String,
    without_rowid: Vec<(String, String)>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

fn connection_key(connection: &Connection) -> usize {
    // SAFETY: the SQLite handle is used only as a stable identity, never dereferenced.
    unsafe { connection.handle() as usize }
}

fn relevant(database: &str, table: &str) -> bool {
    (database == "main"
        && matches!(
            table,
            "batches"
                | "claims"
                | "meta"
                | "checkpoints"
                | "replica_envelopes"
                | "replica_envelope_signatures"
                | "replica_records"
                | "held_keys"
                | "expected_claim_signatures"
                | "claim_signatures"
                | "claim_verdicts"
                | "claim_verdict_links"
                | "claim_verdict_queue"
                | "claim_verdict_fresh"
        ))
        || (database == "temp" && table == "write_clock")
}

pub(crate) struct Scope<'a> {
    connection: &'a Connection,
    previous: Option<Active>,
    observer: Option<Arc<crate::sqlite::writer_observer::MutationState>>,
    witness: Option<Witness>,
}

impl<'a> Scope<'a> {
    #[cfg(test)]
    pub(crate) fn begin(connection: &'a Connection) -> Option<Self> {
        Self::begin_observed(connection, None).expect("witness setup succeeds")
    }

    pub(crate) fn begin_observed(
        connection: &'a Connection,
        observer: Option<Arc<crate::sqlite::writer_observer::MutationState>>,
    ) -> rusqlite::Result<Option<Self>> {
        // The row hook cannot see WITHOUT ROWID writes. Temporary after-row triggers
        // witness each such mutation through one private rowid table. Its hook counts both
        // the original write and the witness UPDATE; unknown writes still fail the total-
        // changes guard. Main schema/history and the observer's rowid-only contract stay intact.
        let tables = (|| -> rusqlite::Result<Vec<(String, String, String, bool)>> {
            connection
                .prepare("PRAGMA table_list")?
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(4)?))
                })?
                .collect()
        })();
        let Ok(tables) = tables else { return Ok(None) };
        for (database, table, kind, without_rowid) in &tables {
            let requires_rows = (database == "main"
                && matches!(table.as_str(), "batches" | "claims"))
                || (database == "temp" && table == "write_clock");
            if (requires_rows && *without_rowid) || (relevant(database, table) && kind != "table") {
                return Ok(None);
            }
        }
        let without_rowid = tables
            .into_iter()
            .filter_map(|(database, table, kind, wr)| {
                (wr && kind == "table" && matches!(database.as_str(), "main" | "temp"))
                    .then_some((database, table))
            })
            .collect::<Vec<_>>();
        let witness = match Witness::install(connection, &without_rowid) {
            Ok(witness) => witness,
            Err(error) => {
                WITNESS_FAILURES.fetch_add(1, Ordering::Relaxed);
                eprintln!("smallclaims: shared append witness setup failed: {error}");
                return Err(error);
            }
        };
        let key = connection_key(connection);
        let previous = ACTIVE.with(|slot| {
            slot.replace(Some(Active {
                connection: key,
                epoch: 0,
                changes: connection.total_changes(),
                notifications: 0,
                disabled: false,
                policy: None,
                committed: None,
                working: None,
                witness: witness.name.clone(),
                without_rowid,
            }))
        });
        crate::sqlite::writer_observer::install_rows(connection, observer.clone(), true);
        Ok(Some(Self {
            connection,
            previous,
            observer,
            witness: Some(witness),
        }))
    }

    pub(crate) fn close(mut self) -> rusqlite::Result<()> {
        self.teardown()
    }

    fn teardown(&mut self) -> rusqlite::Result<()> {
        ACTIVE.with(|slot| {
            slot.borrow_mut().take();
        });
        crate::sqlite::writer_observer::install_rows(self.connection, self.observer.clone(), false);
        let result = self
            .witness
            .take()
            .map_or(Ok(()), |witness| witness.remove(self.connection));
        ACTIVE.with(|slot| *slot.borrow_mut() = self.previous.take());
        if result.is_err() {
            WITNESS_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if self.witness.is_some()
            && let Err(error) = self.teardown()
        {
            eprintln!("smallclaims: shared append witness cleanup failed: {error}");
        }
    }
}

struct Witness {
    name: String,
    triggers: Vec<String>,
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Witness {
    fn install(connection: &Connection, tables: &[(String, String)]) -> rusqlite::Result<Self> {
        let name = format!(
            "{WITNESS_PREFIX}{}",
            NEXT_WITNESS.fetch_add(1, Ordering::Relaxed)
        );
        let mut witness = Self {
            name,
            triggers: Vec::new(),
        };
        let result = (|| {
            connection.execute_batch(&format!(
                "CREATE TEMP TABLE {}(source INTEGER PRIMARY KEY,touch INTEGER NOT NULL)",
                identifier(&witness.name)
            ))?;
            for (index, (database, table)) in tables.iter().enumerate() {
                connection.execute(
                    &format!(
                        "INSERT INTO temp.{} VALUES(?1,0)",
                        identifier(&witness.name)
                    ),
                    [index as i64 + 1],
                )?;
                for action in ["INSERT", "UPDATE", "DELETE"] {
                    let trigger = format!("{}_{}_{}", witness.name, index, action);
                    connection.execute_batch(&format!(
                        "CREATE TEMP TRIGGER {} AFTER {action} ON {}.{} BEGIN UPDATE {} SET touch=1-touch WHERE source={}; END",
                        identifier(&trigger), identifier(database), identifier(table), identifier(&witness.name), index + 1))?;
                    witness.triggers.push(trigger);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Partial setup never publishes a frontier. Cleanup is local to this transaction.
            witness.remove(connection)?;
            return Err(error);
        }
        Ok(witness)
    }

    fn remove(self, connection: &Connection) -> rusqlite::Result<()> {
        for trigger in self.triggers {
            connection.execute_batch(&format!(
                "DROP TRIGGER IF EXISTS temp.{}",
                identifier(&trigger)
            ))?;
        }
        connection.execute_batch(&format!(
            "DROP TABLE IF EXISTS temp.{}",
            identifier(&self.name)
        ))
    }
}

pub(crate) fn row_mutated(key: usize, database: &str, table: &str, rowid: i64) {
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut()
            && active.connection == key
        {
            if database == "temp" && table == active.witness {
                if let Some((database, table)) = rowid
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| active.without_rowid.get(index))
                {
                    // SQLite reports both writes in total_changes, while only the private
                    // witness UPDATE reaches update_hook. Never subtract unknown changes.
                    active.notifications = active.notifications.saturating_add(2);
                    if relevant(database, table) {
                        clear(active);
                    }
                } else {
                    active.notifications = active.notifications.saturating_add(1);
                    clear(active);
                    active.disabled = true;
                }
            } else {
                active.notifications = active.notifications.saturating_add(1);
                if relevant(database, table) {
                    clear(active);
                }
            }
        }
    });
}

fn clear(active: &mut Active) {
    active.committed = None;
    active.working = None;
    active.epoch = active.epoch.wrapping_add(1);
}

pub(crate) fn invalidate() {
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut() {
            clear(active);
        }
    });
}

pub(crate) fn start_job(policy: Option<AppendPolicy>) {
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut() {
            if policy.is_none() {
                clear(active);
            }
            active.policy = policy;
            active.working = active.committed.clone();
        }
    });
}

pub(crate) fn finish_job(connection: &Connection, succeeded: bool) {
    check_mutations(connection);
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut() {
            if succeeded {
                active.committed = active.working.take();
            } else {
                clear(active);
            }
            active.policy = None;
        }
    });
}

/// Truncate DELETE and mutations of WITHOUT ROWID/virtual tables can omit update_hook.
/// SQLite still includes their changes in total_changes. Check after statements and before
/// publishing a savepoint; any discrepancy discards reuse rather than guessing its source.
/// REPLACE's removed row is excluded from both counters; its inserted frontier row invalidates
/// through the hook. Rollback and schema changes have explicit SQL boundaries below.
fn check_mutations(connection: &Connection) {
    let key = connection_key(connection);
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut()
            && active.connection == key
        {
            let changes = connection.total_changes();
            if changes.saturating_sub(active.changes) != active.notifications {
                UNOBSERVED_MUTATIONS.fetch_add(1, Ordering::Relaxed);
                clear(active);
            }
            active.changes = changes;
            active.notifications = 0;
        }
    });
}

/// The completed-statement callback also runs for cached statements. Rollback clears
/// provisional state, and DDL disables reuse for the transaction. Data changes use both the
/// row hook and the total_changes discrepancy guard; projection DELETEs remain eligible.
pub(crate) fn sql_boundary(mut sql: &str) {
    if !ACTIVE.with(|slot| slot.borrow().is_some()) {
        return;
    }
    // A direct write to private bookkeeping must never impersonate a trigger witness.
    // Profile callbacks carry the outer statement, not a trigger subprogram's UPDATE.
    if sql.contains(WITNESS_PREFIX) {
        ACTIVE.with(|slot| {
            if let Some(active) = slot.borrow_mut().as_mut() {
                clear(active);
                active.disabled = true;
            }
        });
        return;
    }
    let word = sql_word(&mut sql).unwrap_or("");
    let ddl = [
        "CREATE", "DROP", "ALTER", "ATTACH", "DETACH", "VACUUM", "PRAGMA",
    ]
    .iter()
    .any(|boundary| word.eq_ignore_ascii_case(boundary));
    if ddl
        || ["ROLLBACK", "COMMIT", "END", "WITH"]
            .iter()
            .any(|boundary| word.eq_ignore_ascii_case(boundary))
    {
        ACTIVE.with(|slot| {
            if let Some(active) = slot.borrow_mut().as_mut() {
                clear(active);
                active.disabled |= ddl;
            }
        });
    }
}

// SQLite accepts comments between keywords as well as before them. Do not treat SQL as
// whitespace-separated prose: DELETE/**/FROM can use the truncate optimisation too.
fn sql_word<'a>(sql: &mut &'a str) -> Option<&'a str> {
    loop {
        *sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            *sql = rest.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(rest) = sql.strip_prefix("/*") {
            *sql = rest.split_once("*/").map_or("", |(_, rest)| rest);
        } else if let Some(rest) = sql.strip_prefix(';') {
            *sql = rest;
        } else {
            break;
        }
    }
    let length = sql.bytes().take_while(u8::is_ascii_alphabetic).count();
    if length == 0 {
        return None;
    }
    let (word, rest) = (*sql).split_at(length);
    *sql = rest;
    Some(word)
}

pub(crate) struct Prepared {
    pub(crate) batch_id: String,
    pub(crate) accepted_at: u128,
    pub(crate) id: String,
    frontier: Frontier,
    connection: usize,
    expected_epoch: u64,
}

impl Prepared {
    /// Called after claim insertion and signature attachment succeed. A trigger or recursive
    /// rules append that changed frontier inputs prevents stale state from being restored.
    pub(crate) fn complete(self) {
        ACTIVE.with(|slot| {
            if let Some(active) = slot.borrow_mut().as_mut()
                && active.connection == self.connection
                && active.epoch == self.expected_epoch
                && !active.disabled
            {
                active.working = Some(self.frontier);
            } else {
                RESTORES_SKIPPED.fetch_add(1,Ordering::Relaxed);
                #[cfg(debug_assertions)]
                eprintln!("smallclaims: shared append frontier not restored after extra mutation or boundary");
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced: Option<&str>,
) -> Option<Result<Prepared>> {
    check_mutations(transaction);
    let key = connection_key(transaction);
    let (epoch, policy, frontier) = ACTIVE.with(|slot| {
        let slot = slot.borrow();
        let active = slot.as_ref()?;
        if active.connection != key || active.disabled {
            return None;
        }
        Some((active.epoch, active.policy?, active.working.clone()))
    })?;
    let class = if forced.is_some()
        || ["fleet.", "principal.", "person.", "rule.", "checkpoint."]
            .iter()
            .any(|prefix| kind.starts_with(prefix))
    {
        None
    } else {
        policy(kind, actor, body)
    };
    let mut blobs = BTreeSet::new();
    collect_hash_fields(body, &mut blobs);
    let Some(class) = class.filter(|_| blobs.is_empty()) else {
        invalidate();
        return None;
    };
    // Device-provided signatures have variable field lists/nonces/chain sizes. Keep their
    // existing path separate. Automatic signatures are produced at the unchanged sealing
    // point; their delegation metadata still needs an encoded-bound proof before enabling.
    if let Some(actor) = actor {
        let supplied=transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM expected_claim_signatures WHERE subject=?1 AND kind=?2 AND actor=?3)",
            params![subject,kind,actor],|row|row.get::<_,bool>(0));
        match supplied {
            Ok(true) => {
                invalidate();
                return None;
            }
            Ok(false) => {}
            Err(error) => return Some(Err(error.into())),
        }
    }
    let bytes = match serde_json::to_vec(&(subject, kind, origin, actor, body, predecessors)) {
        Ok(bytes) => bytes.len().saturating_mul(8).saturating_add(4096),
        Err(error) => return Some(Err(error.into())),
    };
    if bytes > REPLICA_BATCH_BYTES {
        invalidate();
        return None;
    }
    Some((|| {
        let mut frontier = match frontier.filter(|frontier| frontier.origin == origin) {
            Some(frontier) => {
                #[cfg(debug_assertions)]
                check_frontier(transaction, &frontier)?;
                frontier
            }
            None => {
                let _span = crate::profile::span("append/frontier-read");
                Frontier {
                    origin: origin.into(),
                    sequence: next_replica_sequence(transaction, origin)?,
                    previous: previous_batch_hash(transaction, origin)?,
                    clock: WriteClock::read(transaction, origin)?,
                    batch: None,
                }
            }
        };
        let now = frontier.clock.now();
        // Payloads referring to blobs are excluded. The allowance covers each claim's IDs,
        // metadata and signatures in ordinary fixtures. An encoded bound for arbitrary
        // signing metadata is still required before this experimental path is enabled.
        let reusable = frontier.batch.as_ref().is_some_and(|batch| {
            batch.at == now
                && batch.class == class
                && batch.actor.as_deref() == actor
                && batch.claims.len() < REPLICA_BATCH_CLAIMS
                && batch.bytes.saturating_add(bytes) <= REPLICA_BATCH_BYTES
        });
        if !reusable {
            frontier.batch = None;
        }
        let mut id = frontier
            .batch
            .as_ref()
            .map(|batch| claim_hash(&batch.id, subject, kind, origin, actor, body, predecessors))
            .transpose()?;
        if frontier
            .batch
            .as_ref()
            .zip(id.as_ref())
            .is_some_and(|(batch, id)| batch.claims.contains(id))
        {
            frontier.batch = None;
            id = None;
        }
        let inserted = frontier.batch.is_none();
        let mut marker_mutations = 0;
        if !inserted {
            REUSED_BATCHES.fetch_add(1, Ordering::Relaxed);
        }
        if inserted {
            let _span = crate::profile::span("append/batch-header");
            let hash =
                batch_header_hash(origin, frontier.sequence, frontier.previous.as_deref(), now)?;
            let batch = format!("batch/{origin}/{}/{hash}", frontier.sequence);
            transaction.execute("INSERT INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms) VALUES (?1,?2,?3,?4,?5,?6)",
                params![batch, origin, frontier.sequence, frontier.previous, hash, now.to_string()])?;
            marker_mutations = crate::shared_append_fault::mark(transaction, &batch)?;
            let sequence = frontier.sequence;
            let previous = frontier.previous.clone();
            frontier.sequence += 1;
            frontier.previous = Some(hash.clone());
            frontier.clock.advance(now);
            frontier.batch = Some(Batch {
                id: batch,
                sequence,
                previous,
                hash,
                at: now,
                class,
                actor: actor.map(str::to_owned),
                claims: HashSet::new(),
                bytes: 0,
            });
        }
        let batch = frontier
            .batch
            .as_mut()
            .expect("a shared append selected or created a header");
        let id = match id {
            Some(id) => id,
            None => {
                let _span = crate::profile::span("append/claim-hash");
                claim_hash(&batch.id, subject, kind, origin, actor, body, predecessors)?
            }
        };
        batch.claims.insert(id.clone());
        batch.bytes += bytes;
        Ok(Prepared {
            batch_id: batch.id.clone(),
            accepted_at: now,
            id,
            frontier,
            connection: key,
            // One claim INSERT, optional batch INSERT and explicit marker/summary mutations.
            // Extra trigger mutations still prevent restoration of provisional frontier state.
            expected_epoch: epoch.wrapping_add(1 + u64::from(inserted) + marker_mutations),
        })
    })())
}

#[cfg(debug_assertions)]
fn check_frontier(transaction: &Transaction<'_>, frontier: &Frontier) -> Result<()> {
    ensure!(
        frontier.sequence == next_replica_sequence(transaction, &frontier.origin)?,
        "stale shared sequence"
    );
    ensure!(
        frontier.previous == previous_batch_hash(transaction, &frontier.origin)?,
        "stale shared previous hash"
    );
    ensure!(
        frontier.clock == WriteClock::read(transaction, &frontier.origin)?,
        "stale shared clock"
    );
    if let Some(batch) = &frontier.batch {
        let header = transaction.query_row(
            "SELECT origin,replica_sequence,previous_hash,hash,accepted_at_unix_ms FROM batches WHERE id=?1",
            [&batch.id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,
                row.get::<_,Option<String>>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?)))?;
        ensure!(
            header
                == (
                    frontier.origin.clone(),
                    batch.sequence,
                    batch.previous.clone(),
                    batch.hash.clone(),
                    batch.at.to_string()
                ),
            "stale shared header"
        );
        let ids = transaction
            .prepare("SELECT id FROM claims WHERE batch_id=?1")?
            .query_map([&batch.id], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?;
        ensure!(ids == batch.claims, "stale shared batch claims");
        let finalized: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM replica_envelopes WHERE batch_id=?1)",
            [&batch.id],
            |row| row.get(0),
        )?;
        ensure!(!finalized, "shared batch already finalized");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        claim::ClaimRecord,
        sqlite::WriterJob,
        store::{Store, runtime::Plain},
    };
    use serde_json::json;
    use std::sync::{Arc, Mutex, mpsc};

    fn policy(kind: &str, _: Option<&str>, _: &Value) -> Option<&'static str> {
        (kind == "example.note").then_some("always-retained")
    }

    fn node() -> Store {
        let store = Store::open_memory("sample", Arc::new(Plain)).unwrap();
        store.set_write_clock_at(1234).unwrap();
        store
            .connection
            .write()
            .execute(
                "INSERT INTO meta(key,value) VALUES('sample_marker','1')",
                [],
            )
            .unwrap();
        store
    }

    fn append(tx: &Transaction<'_>, n: usize, actor: Option<&str>) -> Result<ClaimRecord> {
        crate::store::append_claim_record_tx(
            tx,
            "sample",
            &format!("note/{n}"),
            "example.note",
            actor,
            &json!({"fields":{"text":n}}),
            &[],
            None,
        )
    }

    type Action = Box<dyn FnOnce(&Transaction<'_>) -> bool + Send>;

    /// Hold a loan while enqueueing all jobs in an exact FIFO sequence, then take another
    /// loan after them. This proves durable completion without sleeps or scheduler guesses.
    fn queued(store: &Store, jobs: Vec<(Option<AppendPolicy>, Action)>) {
        let held = store.connection.write();
        let mut replies = Vec::new();
        for (append_policy, run) in jobs {
            let (done, reply) = mpsc::sync_channel(1);
            store.connection.send(WriterJob::Batched {
                run,
                append_policy,
                profile: None,
                wait: None,
                done,
            });
            replies.push(reply);
        }
        drop(held);
        let _committed = store.connection.write();
        for reply in replies {
            reply.recv().unwrap().unwrap();
        }
    }

    fn jobs(
        count: usize,
        out: &Arc<Mutex<Vec<ClaimRecord>>>,
    ) -> Vec<(Option<AppendPolicy>, Action)> {
        (0..count)
            .map(|n| {
                let out = out.clone();
                (
                    Some(policy as AppendPolicy),
                    Box::new(move |tx: &Transaction<'_>| {
                        out.lock().unwrap().push(append(tx, n, None).unwrap());
                        true
                    }) as Action,
                )
            })
            .collect()
    }

    #[test]
    fn frozen_clock_group_caps_preserve_relative_order() {
        let store = node();
        let mut held = store.connection.write();
        let tx = held.transaction().unwrap();
        let mut claims = vec![append(&tx, 0, None).unwrap()];
        let scope = Scope::begin(&tx).unwrap();
        for n in 1..70 {
            start_job(Some(policy));
            tx.execute_batch("SAVEPOINT sample_job").unwrap();
            claims.push(append(&tx, n, None).unwrap());
            tx.execute_batch("RELEASE sample_job").unwrap();
            finish_job(&tx, true);
        }
        drop(scope);
        tx.commit().unwrap();
        drop(held);
        assert_ne!(claims[0].batch_id, claims[1].batch_id);
        assert_eq!(claims[1].batch_id, claims[32].batch_id);
        assert_ne!(claims[32].batch_id, claims[33].batch_id);
        let mut groups = std::collections::BTreeMap::new();
        for (n, claim) in claims.iter().enumerate() {
            assert_eq!(claim.subject, format!("note/{n}"));
            assert!(store.claim_by_id(&claim.id).unwrap().is_some());
            *groups.entry(&claim.batch_id).or_insert(0) += 1;
        }
        assert_eq!(groups.len(), 4);
        assert!(groups.values().all(|count| *count <= REPLICA_BATCH_CLAIMS));
    }

    #[test]
    fn a_single_job_uses_serial_path_and_acknowledges_only_committed_claims() {
        let store = node();
        store.connection.set_shared_appends(true);
        let claim = store
            .connection
            .batched_append(policy, |tx| {
                assert!(ACTIVE.with(|slot| slot.borrow().is_none()));
                append(tx, 0, None)
            })
            .unwrap()
            .unwrap();
        assert!(store.claim_by_id(&claim.id).unwrap().is_some());
    }

    fn oversized_signing_metadata(store: &Store) -> Vec<ClaimRecord> {
        let out = Arc::new(Mutex::new(Vec::new()));
        queued(store, jobs(4, &out));
        let claims = out.lock().unwrap().clone();
        let last = claims.last().unwrap();
        let key = crate::fleet::MemberKey::generate().unwrap().0;
        let signature = crate::principal::ClaimSignature::sign(
            &key,
            &crate::principal::content_digest(&last.subject, &last.kind, None, &last.body),
            "host/sample",
            None,
            vec!["x".repeat(REPLICA_BATCH_BYTES)],
            1234,
        );
        let mut conn = store.connection.write();
        let tx = conn.transaction().unwrap();
        crate::store::principals::store_claim_signature_tx(&tx, &last.id, &signature).unwrap();
        tx.commit().unwrap();
        claims
    }

    #[test]
    fn real_sealer_rolls_back_the_whole_chunk_and_retains_a_bounded_fault() {
        use std::sync::atomic::Ordering;
        let store = node();
        let claims = oversized_signing_metadata(&store);
        let before = store.seeded_batch_rowid.load(Ordering::Acquire);
        let error = store.seal_local_batches().unwrap_err();
        let fault = error
            .downcast_ref::<crate::shared_append_fault::SealingFailure>()
            .unwrap();
        assert!(fault.report.has_faults());
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), before);
        let read = store.readers.get();
        let envelopes: u64 = read
            .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r.get(0))
            .unwrap();
        let signatures: u64 = read
            .query_row("SELECT COUNT(*) FROM claim_signatures", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            envelopes, 0,
            "the earlier provisional serial envelope also rolls back"
        );
        assert_eq!(
            signatures, 1,
            "pre-existing signature metadata remains unchanged"
        );
        drop(read);
        for claim in &claims {
            assert!(store.claim_by_id(&claim.id).unwrap().is_some());
        }
        let report = store.shared_append_faults().unwrap();
        assert_eq!(report.durable.as_ref().unwrap().faults, 1);
        assert!(!report.verified());
        let conn = store.connection.write();
        conn.execute(
            "DELETE FROM claims WHERE batch_id=?1",
            [&claims.last().unwrap().batch_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM batches WHERE id=?1",
            [&claims.last().unwrap().batch_id],
        )
        .unwrap();
        drop(conn);
        assert!(
            store.seal_local_batches().is_err(),
            "deleting the batch cannot clear retained evidence"
        );
        assert_eq!(
            store
                .shared_append_faults()
                .unwrap()
                .durable
                .unwrap()
                .faults,
            1
        );
    }

    #[test]
    fn partial_witness_setup_and_cleanup_failure_roll_back_without_a_success_ack() {
        use rusqlite::hooks::{AuthAction, Authorization};
        let store = node();
        let held = store.connection.write();
        held.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::CreateTempTrigger { .. } | AuthAction::DropTempTable { .. }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut replies = Vec::new();
        for (append_policy, run) in jobs(2, &out) {
            let (done, reply) = mpsc::sync_channel(1);
            store.connection.send(WriterJob::Batched {
                run,
                append_policy,
                profile: None,
                wait: None,
                done,
            });
            replies.push(reply);
        }
        drop(held);
        for reply in replies {
            assert!(reply.recv().unwrap().is_err());
        }
        let conn = store.connection.write();
        conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>);
        let claims: u64 = conn
            .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
            .unwrap();
        let temp: u64 = conn.query_row("SELECT COUNT(*) FROM sqlite_temp_schema WHERE name LIKE '_smallclaims_append_witness_%'", [], |r| r.get(0)).unwrap();
        assert_eq!(claims, 0);
        assert_eq!(temp, 0);
        assert_eq!(
            out.lock().unwrap().len(),
            1,
            "the first job had run before setup failed"
        );
    }

    #[test]
    fn evidence_commit_failure_preserves_the_real_observation_and_restart_is_uncertified() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.sqlite3");
        let store = Store::open(&path, "sample", Arc::new(Plain)).unwrap();
        store.set_write_clock_at(1234).unwrap();
        oversized_signing_metadata(&store);
        let conn = store.connection.write();
        conn.commit_hook(Some(|| true));
        drop(conn);
        let error = store.seal_local_batches().unwrap_err();
        let fault = error
            .downcast_ref::<crate::shared_append_fault::SealingFailure>()
            .unwrap();
        assert!(fault.report.observed.is_some());
        assert!(fault.report.publication_failed);
        let conn = store.connection.write();
        conn.commit_hook(None::<fn() -> bool>);
        drop(conn);
        let report = store.shared_append_faults().unwrap();
        assert_eq!(report.durable.as_ref().unwrap().faults, 0);
        assert!(report.has_faults());
        assert!(report.publication_failed);
        drop(store);
        let reopened = Store::open(&path, "sample", Arc::new(Plain)).unwrap();
        let report = reopened.shared_append_faults().unwrap();
        assert!(report.durable.unwrap().pending > 0);
        assert!(!reopened.shared_append_faults().unwrap().verified());
    }

    #[test]
    fn failed_job_rolls_back_and_does_not_publish_provisional_frontier() {
        let store = node();
        const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";
        store.bind_fleet(FLEET).unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(4, &out);
        actions.insert(
            3,
            (
                Some(policy),
                Box::new(|tx| {
                    append(tx, 999, None).unwrap();
                    false
                }),
            ),
        );
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_eq!(claims.len(), 4);
        assert_eq!(claims[1].batch_id, claims[2].batch_id);
        assert_ne!(
            claims[2].batch_id, claims[3].batch_id,
            "rollback splits reuse"
        );
        assert!(store.claims_for("note/999", None).unwrap().is_empty());
        // Signing happens later, after COMMIT; the payload contains only surviving claims.
        store.seal_local_batches().unwrap();
        let tx = store.connection.write();
        let present: u64 = tx
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(present, 4);
        let orphan: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM batches WHERE NOT EXISTS(SELECT 1 FROM claims WHERE batch_id=batches.id))", [], |row| row.get(0)).unwrap();
        assert!(!orphan);
        drop(tx);
        drop(claims);
        let peer = Store::open_memory("peer", Arc::new(Plain)).unwrap();
        peer.bind_fleet(FLEET).unwrap();
        let exchange = store
            .export_replication_exchange(FLEET, &peer.replication_inventory().unwrap())
            .unwrap();
        peer.receive_replication_exchange("sample", FLEET, &exchange)
            .unwrap();
        peer.validate_replication_backlog().unwrap();
        peer.apply_replication_repairs().unwrap();
        assert!(peer.project_replication_backlog().unwrap());
        for n in 0..4 {
            assert_eq!(
                serde_json::to_value(store.claims_for(&format!("note/{n}"), None).unwrap())
                    .unwrap(),
                serde_json::to_value(peer.claims_for(&format!("note/{n}"), None).unwrap()).unwrap()
            );
        }
        assert!(peer.claims_for("note/999", None).unwrap().is_empty());
    }

    #[test]
    fn signed_agent_and_person_groups_verify_and_replicate_with_only_surviving_claims() {
        const FLEET: &str = "6f1e8a52-3c4d-4b7e-9a10-2d5c8e7f9b31";
        let store = node();
        let member = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        store.bind_fleet(FLEET).unwrap();
        store.pin_fleet_anchor(member.public()).unwrap();
        store.set_member_key(Some(member.clone())).unwrap();
        store
            .append_claim(&crate::claim::ClaimInput {
                subject: "host/sample".into(),
                kind: "fleet.member-admitted".into(),
                actor: None,
                fields: serde_json::from_value(
                    json!({"fleet_id":FLEET,"member_key":member.public(),
                "via":"anchor","mode":"listening"}),
                )
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        for actor in ["agent/sample/writer", "person/ada"] {
            store.ensure_principal_key(actor).unwrap();
        }
        let old = store
            .export_replication_exchange(FLEET, &Default::default())
            .unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = Vec::new();
        for (n, actor) in (0..7).map(|n| {
            (
                n,
                if n < 4 {
                    "agent/sample/writer"
                } else {
                    "person/ada"
                },
            )
        }) {
            let out = out.clone();
            actions.push((
                Some(policy as AppendPolicy),
                Box::new(move |tx: &Transaction<'_>| {
                    out.lock()
                        .unwrap()
                        .push(append(tx, n, Some(actor)).unwrap());
                    true
                }) as Action,
            ));
        }
        actions.push((
            Some(policy),
            Box::new(|tx| {
                append(tx, 999, Some("person/ada")).unwrap();
                false
            }),
        ));
        queued(&store, actions);
        store.seal_local_batches().unwrap();
        // Sealing queues signed claims for the normal verdict reducer; the read accessor
        // deliberately does not materialize verdicts itself.
        store.judge_claims(true).unwrap();
        let claims = out.lock().unwrap();
        assert_eq!(claims[1].batch_id, claims[3].batch_id);
        assert_ne!(claims[3].batch_id, claims[4].batch_id);
        assert_eq!(claims[4].batch_id, claims[6].batch_id);
        for claim in claims.iter() {
            assert_eq!(
                store.claim_verdict(&claim.id).unwrap(),
                crate::principal::Verdict::Verified
            );
            let signature = store.claim_signature(&claim.id).unwrap().unwrap();
            assert_eq!(signature.signer, claim.actor.as_deref().unwrap());
            assert_eq!(
                signature.chain.len(),
                if signature.signer.starts_with("person/") {
                    2
                } else {
                    1
                }
            );
        }
        let peer = Store::open_memory("peer", Arc::new(Plain)).unwrap();
        peer.bind_fleet(FLEET).unwrap();
        peer.pin_fleet_anchor(member.public()).unwrap();
        let exchange = store
            .export_replication_exchange_answering(
                FLEET,
                &peer.replication_inventory().unwrap(),
                &peer.replication_signature_requests().unwrap(),
            )
            .unwrap();
        for envelope in &old.envelopes {
            let surviving = exchange
                .envelopes
                .iter()
                .find(|candidate| {
                    candidate.writer == envelope.writer
                        && candidate.sequence == envelope.sequence
                        && candidate.hash == envelope.hash
                })
                .unwrap();
            assert_eq!(
                serde_json::to_value(envelope).unwrap(),
                serde_json::to_value(surviving).unwrap()
            );
        }
        peer.receive_replication_exchange("sample", FLEET, &exchange)
            .unwrap();
        peer.validate_replication_backlog().unwrap();
        peer.apply_replication_repairs().unwrap();
        assert!(peer.project_replication_backlog().unwrap());
        for claim in claims.iter() {
            let actual = peer.claim_by_id(&claim.id).unwrap().unwrap();
            assert_eq!(actual.body, claim.body);
            assert_eq!(actual.batch_id, claim.batch_id);
            assert_eq!(
                peer.claim_verdict(&claim.id).unwrap(),
                crate::principal::Verdict::Verified
            );
        }
        assert!(peer.claims_for("note/999", None).unwrap().is_empty());
    }

    #[test]
    fn panic_after_partial_append_is_isolated_and_dropped_ack_receivers_do_not_cancel_success() {
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(4, &out);
        actions.insert(
            3,
            (
                Some(policy),
                Box::new(|tx| {
                    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        append(tx, 999, None).unwrap();
                        panic!("injected job panic");
                    }));
                    assert!(panic.is_err());
                    false
                }),
            ),
        );
        queued(&store, actions);
        assert!(store.claims_for("note/999", None).unwrap().is_empty());
        let claims = out.lock().unwrap();
        assert_eq!(claims[1].batch_id, claims[2].batch_id);
        assert_ne!(claims[2].batch_id, claims[3].batch_id);
        drop(claims);
        let held = store.connection.write();
        for n in 100..104 {
            let (done, receiver) = mpsc::sync_channel(1);
            drop(receiver);
            store.connection.send(WriterJob::Batched {
                run: Box::new(move |tx| {
                    append(tx, n, None).unwrap();
                    true
                }),
                append_policy: Some(policy),
                profile: None,
                wait: None,
                done,
            });
        }
        drop(held);
        drop(store.connection.write());
        for n in 100..104 {
            assert_eq!(
                store.claims_for(&format!("note/{n}"), None).unwrap().len(),
                1
            );
        }
        // Exercise the public catch/rethrow contract as well as the queued savepoint path.
        store.connection.set_shared_appends(true);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.connection.batched_append(policy, |tx| -> Result<()> {
                append(tx, 999, None)?;
                panic!("public caller panic");
            });
        }));
        assert!(panic.is_err());
        assert!(store.claims_for("note/999", None).unwrap().is_empty());
        store
            .connection
            .batched_append(policy, |tx| append(tx, 104, None))
            .unwrap()
            .unwrap();
        assert_eq!(store.claims_for("note/104", None).unwrap().len(), 1);
    }

    #[test]
    fn file_wal_crash_before_commit_keeps_none_and_after_ack_keeps_all() {
        const CHILD: &str = "SMALLCLAIMS_SHARED_CRASH_CHILD";
        const PHASE: &str = "SMALLCLAIMS_SHARED_CRASH_PHASE";
        if let Some(path) = std::env::var_os(CHILD) {
            let store =
                Store::open(std::path::Path::new(&path), "sample", Arc::new(Plain)).unwrap();
            store.set_write_clock_at(1234).unwrap();
            let out = Arc::new(Mutex::new(Vec::new()));
            let mut actions = jobs(4, &out);
            if std::env::var(PHASE).unwrap() == "before" {
                actions.push((
                    Some(policy),
                    Box::new(|_| {
                        // Simulate sudden loss after successful savepoints, before COMMIT/ACK.
                        // SAFETY: this is a dedicated test subprocess, never the parent harness.
                        unsafe {
                            libc::_exit(71);
                        }
                    }),
                ));
            }
            queued(&store, actions);
            // No Store/Connection destructor or SQLite checkpoint is allowed to run here.
            unsafe {
                libc::_exit(72);
            }
        }
        for (phase, exit, expected) in [("before", 71, 0), ("after", 72, 4)] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("claims.sqlite");
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","append_group::tests::file_wal_crash_before_commit_keeps_none_and_after_ack_keeps_all","--nocapture"])
                .env(CHILD,&path).env(PHASE,phase).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(exit),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let store = Store::open(&path, "sample", Arc::new(Plain)).unwrap();
            assert_eq!(
                store.claims_for("note/0", None).unwrap().len(),
                usize::from(expected > 0)
            );
            let count: u64 = store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, expected);
            if expected > 0 {
                let claims = (0..4)
                    .map(|n| {
                        store
                            .claims_for(&format!("note/{n}"), None)
                            .unwrap()
                            .pop()
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(claims[1].batch_id, claims[3].batch_id);
                store.seal_local_batches().unwrap();
                let integrity: String = store
                    .readers
                    .get()
                    .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(integrity, "ok");
            }
        }
    }

    #[test]
    fn duplicate_hashes_and_actor_changes_split_groups() {
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(3, &out);
        for (n, actor) in [(2, None), (3, Some("person/ada")), (4, None)] {
            let out = out.clone();
            actions.push((
                Some(policy),
                Box::new(move |tx| {
                    out.lock().unwrap().push(append(tx, n, actor).unwrap());
                    true
                }),
            ));
        }
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_ne!(claims[2].id, claims[3].id);
        for pair in claims[2..].windows(2) {
            assert_ne!(pair[0].batch_id, pair[1].batch_id);
        }
    }

    #[test]
    fn generic_job_and_direct_metadata_mutations_split_frontiers() {
        for sql in [
            "UPDATE temp.write_clock SET at_ms=2000",
            "INSERT OR REPLACE INTO meta(key,value) VALUES('writer_floor/sample','100')",
            "/* cached boundary */ DELETE FROM meta",
        ] {
            let store = node();
            let out = Arc::new(Mutex::new(Vec::new()));
            let mut actions = jobs(3, &out);
            actions.insert(
                2,
                (
                    Some(policy),
                    Box::new(move |tx| {
                        tx.prepare_cached(sql).unwrap().execute([]).unwrap();
                        true
                    }),
                ),
            );
            queued(&store, actions);
            let claims = out.lock().unwrap();
            assert_ne!(claims[1].batch_id, claims[2].batch_id, "{sql}");
        }
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(3, &out);
        actions.insert(2, (None, Box::new(|_| true)));
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_ne!(claims[1].batch_id, claims[2].batch_id);
    }

    #[test]
    fn without_rowid_projections_preserve_groups_and_private_witness_writes_fence_them() {
        let store = node();
        store
            .connection
            .write()
            .execute_batch("CREATE TABLE projection_rows(subject TEXT PRIMARY KEY) WITHOUT ROWID")
            .unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = Vec::new();
        for n in 0..5 {
            let out = out.clone();
            actions.push((
                Some(policy as AppendPolicy),
                Box::new(move |tx: &Transaction<'_>| {
                    out.lock().unwrap().push(append(tx, n, None).unwrap());
                    tx.execute(
                        "INSERT INTO projection_rows VALUES(?1)",
                        [format!("note/{n}")],
                    )
                    .unwrap();
                    if n == 3 {
                        let (name, row) = ACTIVE.with(|slot| {
                            let slot = slot.borrow();
                            let active = slot.as_ref().unwrap();
                            (
                                active.witness.clone(),
                                active
                                    .without_rowid
                                    .iter()
                                    .position(|(_, table)| table == "projection_rows")
                                    .unwrap()
                                    + 1,
                            )
                        });
                        tx.execute(
                            &format!(
                                "UPDATE temp.{} SET touch=touch WHERE source=?1",
                                identifier(&name)
                            ),
                            [row as i64],
                        )
                        .unwrap();
                    }
                    true
                }) as Action,
            ));
        }
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_eq!(claims[1].batch_id, claims[3].batch_id);
        assert_ne!(claims[3].batch_id, claims[4].batch_id);
        let held = store.connection.write();
        let rows: usize = held
            .query_row("SELECT COUNT(*) FROM projection_rows", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 5);
        let private_schema: usize = held.query_row("SELECT COUNT(*) FROM temp.sqlite_schema WHERE name LIKE '_smallclaims_append_witness_%'", [], |row| row.get(0)).unwrap();
        assert_eq!(
            private_schema, 0,
            "witness tables and triggers must not escape the shared scope"
        );
    }

    #[test]
    fn signature_and_verdict_queue_mutations_split_authority_boundaries() {
        for sql in [
            "INSERT INTO claim_verdict_queue VALUES('unknown-example')",
            "INSERT INTO claim_verdict_fresh VALUES('unknown-example')",
            "INSERT INTO claim_verdict_links VALUES('root','unknown-example')",
            "INSERT INTO claim_signatures VALUES('unknown-example','host/sample','example-key','example-nonce','{}')",
        ] {
            let store = node();
            let out = Arc::new(Mutex::new(Vec::new()));
            let mut actions = jobs(3, &out);
            actions.insert(
                2,
                (
                    Some(policy),
                    Box::new(move |tx| {
                        tx.execute_batch(sql).unwrap();
                        true
                    }),
                ),
            );
            queued(&store, actions);
            let claims = out.lock().unwrap();
            assert_ne!(
                claims[1].batch_id, claims[2].batch_id,
                "authority boundary: {sql}"
            );
        }
    }

    #[test]
    fn existing_without_rowid_authority_tables_do_not_block_sharing_but_mutations_split_it() {
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(5, &out);
        actions.insert(3, (Some(policy), Box::new(|tx| {
            tx.execute("INSERT INTO expected_claim_signatures VALUES('example/other','example.note','person/ada','{}')", []).unwrap();
            true
        })));
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_eq!(claims[1].batch_id, claims[2].batch_id);
        assert_ne!(claims[2].batch_id, claims[3].batch_id);
        assert_eq!(claims[3].batch_id, claims[4].batch_id);
    }

    #[test]
    fn schema_change_and_without_rowid_disable_reuse() {
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(4, &out);
        actions.insert(
            2,
            (
                Some(policy),
                Box::new(|tx| {
                    tx.execute_batch(
                        "CREATE TABLE sample_table(id TEXT PRIMARY KEY) WITHOUT ROWID",
                    )
                    .unwrap();
                    true
                }),
            ),
        );
        queued(&store, actions);
        let claims = out.lock().unwrap();
        assert_ne!(claims[1].batch_id, claims[2].batch_id);
        assert_ne!(claims[2].batch_id, claims[3].batch_id);
        drop(claims);
        let mut held = store.connection.write();
        held.execute_batch("CREATE TEMP TABLE write_clock_new(offset_ms INTEGER,at_ms INTEGER, PRIMARY KEY(offset_ms)) WITHOUT ROWID; INSERT INTO write_clock_new VALUES(0,1234); DROP TABLE temp.write_clock; ALTER TABLE temp.write_clock_new RENAME TO write_clock").unwrap();
        let tx = held.transaction().unwrap();
        assert!(Scope::begin(&tx).is_none());
    }

    #[test]
    fn logical_order_matches_serial_across_origins_clock_changes_and_legacy_positions() {
        type LogicalClaim = (String, String, String, u128);
        fn run(shared: bool) -> (Vec<LogicalClaim>, Vec<ClaimRecord>) {
            let store = node();
            let out = Arc::new(Mutex::new(Vec::new()));
            let actions = (0..8)
                .map(|n| {
                    let out = out.clone();
                    let policy = shared.then_some(policy as AppendPolicy);
                    (
                        policy,
                        Box::new(move |tx: &Transaction<'_>| {
                            if n == 5 {
                                tx.execute("UPDATE temp.write_clock SET at_ms=2000", [])
                                    .unwrap();
                            }
                            let origin = if n == 4 { "peer" } else { "sample" };
                            let claim = crate::store::append_claim_record_tx(
                                tx,
                                origin,
                                &format!("note/{n}"),
                                "example.note",
                                None,
                                &json!({"fields":{"text":n}}),
                                &[],
                                None,
                            )
                            .unwrap();
                            out.lock().unwrap().push(claim);
                            true
                        }) as Action,
                    )
                })
                .collect();
            queued(&store, actions);
            let reader = store.readers.get();
            let query = format!(
                "SELECT subject,kind,body,accepted_at_unix_ms FROM claims ORDER BY {}",
                crate::store::CANONICAL_ORDER
            );
            let order = reader
                .prepare(&query)
                .unwrap()
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, String>(3)?.parse().unwrap(),
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            drop(reader);
            let claims = out.lock().unwrap().clone();
            (order, claims)
        }
        let (serial, old_ids) = run(false);
        let (grouped, new_ids) = run(true);
        assert_eq!(serial, grouped);
        // Explicit logical identity mapping: subject/kind/body, rather than new hash IDs.
        for (old, new) in old_ids.iter().zip(&new_ids) {
            assert_eq!(
                (&old.subject, &old.kind, &old.body),
                (&new.subject, &new.kind, &new.body)
            );
        }
        assert!(
            old_ids
                .iter()
                .zip(&new_ids)
                .any(|(old, new)| old.id != new.id)
        );
    }

    #[test]
    fn comments_between_keywords_do_not_hide_unhooked_sql_boundaries() {
        for sql in [
            "DELETE/**/FROM meta",
            "INSERT/**/OR/**/REPLACE INTO meta(key,value) VALUES('sample','1')",
            "; /* schema */ CREATE/**/TABLE sample_extra(id INTEGER)",
        ] {
            let store = node();
            let out = Arc::new(Mutex::new(Vec::new()));
            let mut actions = jobs(3, &out);
            actions.insert(
                2,
                (
                    Some(policy),
                    Box::new(move |tx| {
                        tx.execute_batch(sql).unwrap();
                        true
                    }),
                ),
            );
            queued(&store, actions);
            let claims = out.lock().unwrap();
            assert_ne!(claims[1].batch_id, claims[2].batch_id, "{sql}");
        }
    }

    #[test]
    fn oversized_and_blob_claims_follow_serial_semantics() {
        let store = node();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut actions = jobs(3, &out);
        for body in [
            json!({"fields":{"text":"x".repeat(REPLICA_BATCH_BYTES)}}),
            json!({"fields":{"hash":"a".repeat(64)}}),
        ] {
            let out = out.clone();
            actions.insert(
                2,
                (
                    Some(policy),
                    Box::new(move |tx| {
                        let claim = crate::store::append_claim_record_tx(
                            tx,
                            "sample",
                            "note/large",
                            "example.note",
                            None,
                            &body,
                            &[],
                            None,
                        )
                        .unwrap();
                        out.lock().unwrap().push(claim);
                        true
                    }),
                ),
            );
        }
        queued(&store, actions);
        let claims = out.lock().unwrap();
        for pair in claims[1..].windows(2) {
            assert_ne!(pair[0].batch_id, pair[1].batch_id);
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn missed_invalidation_is_detected_by_the_sql_oracle() {
        let store = node();
        let mut held = store.connection.write();
        let tx = held.transaction().unwrap();
        let _scope = Scope::begin(&tx).unwrap();
        start_job(Some(policy));
        append(&tx, 0, None).unwrap();
        finish_job(&tx, true);
        start_job(Some(policy));
        // Deliberately bypass both mutation invalidators, simulating a missed release edge.
        let saved = ACTIVE.with(|slot| slot.borrow().as_ref().unwrap().working.clone());
        tx.execute("UPDATE temp.write_clock SET at_ms=9999", [])
            .unwrap();
        ACTIVE.with(|slot| slot.borrow_mut().as_mut().unwrap().working = saved);
        let error = append(&tx, 1, None).unwrap_err();
        assert!(error.to_string().contains("stale shared clock"));
    }
}
