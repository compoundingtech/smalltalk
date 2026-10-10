//! The bounded operation audit against the hydrated audit it replaced.
//!
//! The reference below is the audit as it stood before bounded paging: it hydrates every
//! operation claim and every tombstone, then compares them with the whole operations table in
//! one read transaction. It stays here, frozen, as the oracle for conflicts, tombstones,
//! repaired claims and malformed operation metadata. The bounded audit must report exactly
//! what it reports, while holding each read transaction for one short page only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};
use smallclaims::store::{append_claim_record_tx, claim_from_row, operation_parts};
use st3::model::ClaimInput;
use st3::store::Store;

pub(crate) const NODE: &str = "audit-node";

pub(crate) type OperationRows = BTreeMap<String, (String, String, String)>;

/// The operation claims the audit read before bounded paging: every body, hydrated.
const OLD_EXPECTED_OPERATIONS_QUERY: &str =
    "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
     FROM claims INDEXED BY claims_operation_index
     WHERE json_extract(body, '$._operation.id') IS NOT NULL
       AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)
     ORDER BY id";

/// The dropped claims the audit read before bounded paging, all at once.
const OLD_CHECKPOINTED_OPERATIONS_QUERY: &str =
    "SELECT operation_id, request_digest, id FROM checkpoint_claims
     WHERE operation_id IS NOT NULL AND request_digest IS NOT NULL
       AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=checkpoint_claims.id)";

/// The operations table's rows as the hydrated audit derived them.
pub(crate) fn old_expected_operations(connection: &Connection) -> Result<OperationRows> {
    let mut statement = connection.prepare(OLD_EXPECTED_OPERATIONS_QUERY)?;
    let claims = statement
        .query_map([], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut grouped = BTreeMap::<String, Vec<(String, String)>>::new();
    let mut stored = BTreeSet::new();
    for claim in claims {
        if let Some((operation_id, request_digest)) = operation_parts(&claim.body) {
            grouped
                .entry(operation_id.to_owned())
                .or_default()
                .push((request_digest.to_owned(), claim.id.clone()));
            stored.insert(claim.id);
        }
    }
    let mut dropped = BTreeMap::<String, Vec<(String, String)>>::new();
    let mut statement = connection.prepare(OLD_CHECKPOINTED_OPERATIONS_QUERY)?;
    let tombstones = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (operation_id, request_digest, claim_id) in tombstones {
        if !stored.contains(&claim_id) {
            dropped
                .entry(operation_id)
                .or_default()
                .push((request_digest, claim_id));
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(operation_id, stored_claims)| {
            let dropped = dropped.remove(&operation_id).unwrap_or_default();
            (operation_id, old_operation_row(&stored_claims, dropped))
        })
        .collect())
}

fn old_operation_row(
    stored_claims: &[(String, String)],
    dropped: Vec<(String, String)>,
) -> (String, String, String) {
    let mut claims = stored_claims.to_vec();
    claims.extend(dropped);
    claims.sort();
    let request_digest = claims[0].0.clone();
    let state = if claims.iter().all(|(digest, _)| digest == &request_digest) {
        "active"
    } else {
        "conflict"
    };
    let canonical_claim_id = stored_claims
        .iter()
        .filter(|(digest, _)| digest == &request_digest)
        .map(|(_, claim)| claim)
        .min()
        .or_else(|| stored_claims.iter().map(|(_, claim)| claim).min())
        .expect("an operation has at least one stored claim")
        .clone();
    (request_digest, canonical_claim_id, state.into())
}

/// The hydrated audit: one read transaction over the whole claim log and operations table.
/// `observer` sees that transaction's duration once it has ended, as the bounded audit's
/// observer sees each of its pages.
pub(crate) fn old_operation_projection_drift(
    connection: &Connection,
    observer: &mut dyn FnMut(Duration),
) -> Result<Vec<String>> {
    let started = Instant::now();
    let transaction = connection.unchecked_transaction()?;
    let expected = old_expected_operations(&transaction)?;
    let mut statement = transaction.prepare(
        "SELECT id, request_digest, canonical_claim_id, state FROM operations ORDER BY id",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })?
        .collect::<rusqlite::Result<OperationRows>>()?;
    drop(statement);
    transaction.commit()?;
    observer(started.elapsed());
    Ok(expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| expected.get(*id) != actual.get(*id))
        .cloned()
        .collect())
}

pub(crate) fn old_drift(store: &Store) -> Vec<String> {
    old_operation_projection_drift(&store.readers.get(), &mut |_| {}).unwrap()
}

/// One operation claim to write: its `_operation.id` and `_operation.request_digest` as JSON,
/// so a fixture can also write the malformed metadata that the audit must skip.
pub(crate) struct OperationClaim {
    pub operation: Value,
    pub digest: Value,
    pub label: String,
}

pub(crate) fn claim(operation: impl Into<Value>, digest: impl Into<Value>, label: &str) -> OperationClaim {
    OperationClaim {
        operation: operation.into(),
        digest: digest.into(),
        label: label.to_owned(),
    }
}

/// Store operation claims as replicated claims arrive: in the log, without touching the
/// operations table, which `settle_operations` then derives from the reference. Each claim
/// carries `payload_bytes` of field data, so the hydrated audit pays for real bodies.
pub(crate) fn write_claims(
    store: &Store,
    claims: Vec<OperationClaim>,
    payload_bytes: usize,
) -> Vec<String> {
    store
        .connection
        .batched(move |transaction: &Transaction<'_>| {
            let payload = "x".repeat(payload_bytes);
            claims
                .iter()
                .map(|claim| {
                    let body = json!({
                        "fields": {"label": claim.label, "payload": payload},
                        "_operation": {"id": claim.operation, "request_digest": claim.digest},
                    });
                    append_claim_record_tx(
                        transaction,
                        NODE,
                        &format!("custom/audit/{}", claim.label),
                        "custom.audit.note",
                        None,
                        &body,
                        &[],
                        None,
                    )
                    .map(|record| record.id)
                })
                .collect::<Result<Vec<_>>>()
        })
        .unwrap()
        .unwrap()
}

/// A dropped claim's tombstone, as a checkpoint trim leaves it.
pub(crate) struct Tombstone {
    pub id: String,
    pub operation: Option<String>,
    pub digest: Option<String>,
}

pub(crate) fn tombstone(id: &str, operation: Option<&str>, digest: Option<&str>) -> Tombstone {
    Tombstone {
        id: id.to_owned(),
        operation: operation.map(str::to_owned),
        digest: digest.map(str::to_owned),
    }
}

pub(crate) fn write_tombstones(store: &Store, tombstones: Vec<Tombstone>) {
    store
        .connection
        .batched(move |transaction: &Transaction<'_>| {
            for (sequence, tombstone) in tombstones.iter().enumerate() {
                transaction.execute(
                    "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
                         actor,predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
                     VALUES (?1,'audit-writer',?2,'fixture-envelope','custom/audit/dropped',
                         'custom.audit.note',NULL,'[]',?3,?4,0,'checkpoint/2026-10-01')",
                    params![tombstone.id, sequence as i64 + 1, tombstone.operation, tombstone.digest],
                )?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
}

/// Exclude claims as an authenticated `record.repaired` claim does.
pub(crate) fn mark_repaired(store: &Store, ids: Vec<String>) {
    store
        .connection
        .batched(move |transaction: &Transaction<'_>| {
            for id in &ids {
                transaction.execute(
                    "INSERT OR IGNORE INTO projection_digest_repaired_claims(id) VALUES (?1)",
                    [id],
                )?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
}

/// Write the operations table exactly as the reference derives it, so the fixture starts with
/// no drift by the old audit's definition, independent of the code under test.
pub(crate) fn settle_operations(store: &Store) {
    store
        .connection
        .batched(|transaction: &Transaction<'_>| {
            let rows = old_expected_operations(transaction)?;
            transaction.execute("DELETE FROM operations", [])?;
            for (id, (request_digest, canonical_claim_id, state)) in rows {
                transaction.execute(
                    "INSERT INTO operations(id, request_digest, canonical_claim_id, state)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id, request_digest, canonical_claim_id, state],
                )?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
}

pub(crate) fn operation_row(store: &Store, id: &str) -> Option<(String, String, String)> {
    smallclaims::store::operation_tx(&store.readers.get(), id).unwrap()
}

fn execute(store: &Store, sql: &str, parameters: impl rusqlite::Params) {
    store.connection.lock().unwrap().execute(sql, parameters).unwrap();
}

/// Rows past one page of 128, for the claim, tombstone and operation walks.
const PAGES: usize = 300;

/// Every case where the operation row is not simply its one claim.
struct EdgeStore {
    store: Store,
    duplicate: Vec<String>,
}

fn edge_store(path: &Path) -> EdgeStore {
    let store = Store::open(path, NODE).unwrap();
    let mut claims = vec![
        claim("op/active", "d/1", "active"),
        claim("op/duplicate", "d/1", "duplicate-a"),
        claim("op/duplicate", "d/1", "duplicate-b"),
        claim("op/conflict", "d/1", "conflict-a"),
        claim("op/conflict", "d/2", "conflict-b"),
        // A smaller digest only a tombstone holds: the row is a conflict on that digest, and its
        // canonical claim falls back to the smallest stored claim.
        claim("op/dropped-min", "d/2", "dropped-min-a"),
        claim("op/dropped-min", "d/2", "dropped-min-b"),
        claim("op/dropped-same", "d/1", "dropped-same"),
        claim("op/tombstone-of-stored", "d/2", "tombstone-of-stored"),
        claim("op/repaired", "d/1", "repaired-kept"),
        claim("op/repaired", "d/0", "repaired-excluded"),
        claim("op/repaired-only", "d/1", "repaired-only"),
        claim("op/repaired-tombstone", "d/2", "repaired-tombstone"),
        // Empty identifiers are text, and count.
        claim("", "d/1", "empty-operation"),
        claim("op/empty-digest", "", "empty-digest-a"),
        claim("op/empty-digest", "d/1", "empty-digest-b"),
        // Operation metadata that is not text: the index holds these claims, the audit skips them.
        claim("op/malformed", 7, "malformed-digest"),
        claim(42, "d/1", "malformed-operation"),
        claim("op/malformed-mixed", "d/1", "malformed-mixed-valid"),
        claim("op/malformed-mixed", Value::Null, "malformed-mixed-null"),
    ];
    // One operation spanning several claim and tombstone pages, with its smallest digest only
    // in a tombstone near the end.
    for index in 0..PAGES {
        let digest = if index % 7 == 0 { "d/4" } else { "d/5" };
        claims.push(claim("op/huge", digest, &format!("huge-{index:04}")));
    }
    for index in 0..PAGES {
        claims.push(claim(
            format!("op/many/{index:04}"),
            "d/1",
            &format!("many-{index:04}"),
        ));
    }
    let ids = write_claims(&store, claims, 64);
    let id = |label: usize| ids[label].clone();
    // Index into the claims vector above.
    let (tombstone_of_stored, repaired_excluded, repaired_only) = (id(8), id(10), id(11));
    let malformed_digest = id(16);
    let mut tombstones = vec![
        tombstone("claim/dropped/min", Some("op/dropped-min"), Some("d/1")),
        tombstone("claim/dropped/same", Some("op/dropped-same"), Some("d/1")),
        // A tombstone for a claim still stored counts once, as that claim.
        tombstone(&tombstone_of_stored, Some("op/tombstone-of-stored"), Some("d/0")),
        tombstone("claim/dropped/only", Some("op/dropped-only"), Some("d/1")),
        tombstone("claim/dropped/repaired", Some("op/repaired-tombstone"), Some("d/0")),
        // The stored claim's digest is not text, so its tombstone is a dropped claim.
        tombstone(&malformed_digest, Some("op/malformed-mixed"), Some("d/0")),
        tombstone("claim/dropped/no-operation", None, Some("d/0")),
        tombstone("claim/dropped/no-digest", Some("op/active"), None),
    ];
    for index in 0..PAGES {
        let digest = if index == PAGES - 3 { "d/3" } else { "d/6" };
        tombstones.push(tombstone(
            &format!("claim/dropped/huge-{index:04}"),
            Some("op/huge"),
            Some(digest),
        ));
    }
    write_tombstones(&store, tombstones);
    mark_repaired(
        &store,
        vec![
            repaired_excluded,
            repaired_only,
            "claim/dropped/repaired".into(),
        ],
    );
    settle_operations(&store);
    EdgeStore {
        duplicate: vec![id(1), id(2)],
        store,
    }
}

#[test]
fn the_fixture_covers_each_edge_by_the_reference() {
    let directory = tempfile::tempdir().unwrap();
    let EdgeStore { store, .. } = edge_store(&directory.path().join("state.sqlite3"));
    let row = |id: &str| operation_row(&store, id);
    let state = |id: &str| row(id).unwrap().2;
    assert_eq!(state("op/conflict"), "conflict");
    assert_eq!(row("op/dropped-min").unwrap().0, "d/1");
    assert_eq!(state("op/dropped-min"), "conflict");
    assert_eq!(state("op/dropped-same"), "active");
    assert_eq!(row("op/tombstone-of-stored").unwrap().0, "d/2");
    assert_eq!(state("op/tombstone-of-stored"), "active");
    assert!(row("op/dropped-only").is_none());
    assert_eq!(state("op/repaired"), "active");
    assert!(row("op/repaired-only").is_none());
    assert_eq!(state("op/repaired-tombstone"), "active");
    assert!(row("").is_some());
    assert_eq!(row("op/empty-digest").unwrap().0, "");
    assert!(row("op/malformed").is_none());
    assert_eq!(row("op/malformed-mixed").unwrap().0, "d/0");
    assert_eq!(row("op/huge").unwrap().0, "d/3");
    assert_eq!(state("op/huge"), "conflict");
    assert!(old_drift(&store).is_empty());
}

#[test]
fn the_bounded_audit_matches_the_hydrated_audit_on_every_edge() {
    let directory = tempfile::tempdir().unwrap();
    let EdgeStore { store, duplicate } = edge_store(&directory.path().join("state.sqlite3"));
    assert_eq!(store.operation_projection_drift().unwrap(), Vec::<String>::new());

    let existing = operation_row(&store, "op/active").unwrap().1;
    let duplicate_canonical = operation_row(&store, "op/duplicate").unwrap().1;
    let noncanonical_duplicate = duplicate.iter().find(|id| *id != &duplicate_canonical).unwrap();
    execute(&store, "UPDATE operations SET state='active' WHERE id='op/conflict'", []);
    execute(&store, "UPDATE operations SET request_digest='d/1' WHERE id='op/empty-digest'", []);
    execute(
        &store,
        "UPDATE operations SET canonical_claim_id=?1 WHERE id='op/duplicate'",
        [noncanonical_duplicate],
    );
    execute(&store, "DELETE FROM operations WHERE id='op/huge'", []);
    execute(&store, "DELETE FROM operations WHERE id=''", []);
    for stray in ["op/dropped-only", "op/repaired-only", "op/malformed", "op/zz-stray"] {
        execute(
            &store,
            "INSERT INTO operations(id, request_digest, canonical_claim_id, state)
             VALUES (?1, 'd/1', ?2, 'active')",
            params![stray, existing],
        );
    }
    let mut changed = vec![
        "",
        "op/conflict",
        "op/dropped-only",
        "op/duplicate",
        "op/empty-digest",
        "op/huge",
        "op/malformed",
        "op/repaired-only",
        "op/zz-stray",
    ];
    changed.sort_unstable();
    let old = old_drift(&store);
    assert_eq!(old, changed);
    assert_eq!(store.operation_projection_drift().unwrap(), old);

    assert!(store.repair_operation_projection_drift().unwrap());
    assert!(old_drift(&store).is_empty());
    assert!(store.operation_projection_drift().unwrap().is_empty());
    assert!(!store.repair_operation_projection_drift().unwrap());
}

/// Each observation is one read transaction that has already ended. A pass over more than one
/// page of claims, tombstones and operations must end several, each of them short; the
/// hydrated audit ends exactly one, as long as the whole pass.
#[test]
fn the_audit_reads_in_pages_and_releases_each_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let EdgeStore { store, .. } = edge_store(&path);
    // Leave frames in the WAL, so a checkpoint has something a held snapshot would pin.
    store
        .append_claim(&note("before-audit"))
        .unwrap();
    let outside = Connection::open(&path).unwrap();
    outside.busy_timeout(Duration::ZERO).unwrap();
    let mut observations = 0;
    let mut checkpointed = false;
    let drift = store
        .operation_projection_drift_with_observer(&mut |elapsed| {
            observations += 1;
            assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
            if !checkpointed {
                // Nothing may hold a read snapshot while the observer runs.
                let busy: i64 = outside
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(busy, 0, "a read snapshot outlived its page");
                checkpointed = true;
            }
        })
        .unwrap();
    assert!(drift.is_empty(), "{drift:?}");
    assert!(checkpointed);
    // At least the claim pages over op/huge and op/many, a tombstone page beyond the first,
    // and two operation pages.
    let pages = (2 * PAGES).div_ceil(128) + PAGES.div_ceil(128) + PAGES.div_ceil(128);
    assert!(observations >= pages, "{observations} observations, {pages} pages");
}

fn note(label: &str) -> ClaimInput {
    ClaimInput {
        subject: format!("custom/audit-note/{label}"),
        kind: "custom.audit.note".into(),
        actor: None,
        fields: BTreeMap::from([("label".into(), json!(label))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }
}

fn operation_note(label: &str) -> ClaimInput {
    ClaimInput {
        idempotency_key: Some(format!("audit-{label}")),
        ..note(label)
    }
}

/// How many pages one quiet pass over the edge store observes.
fn quiet_observations(store: &Store) -> usize {
    let mut observations = 0;
    assert!(
        store
            .operation_projection_drift_with_observer(&mut |_| observations += 1)
            .unwrap()
            .is_empty()
    );
    observations
}

/// A daemon keeps appending operations while doctor or the start-up repair audits. The audit
/// still completes, and reports none of the consistent new rows as drift.
#[test]
fn the_audit_completes_while_new_operations_are_appended_after_every_page() {
    let directory = tempfile::tempdir().unwrap();
    let EdgeStore { store, .. } = edge_store(&directory.path().join("state.sqlite3"));
    let mut appended = 0;
    let drift = store
        .operation_projection_drift_with_observer(&mut |_| {
            store
                .append_claim(&operation_note(&format!("live-{appended}")))
                .unwrap();
            appended += 1;
        })
        .unwrap();
    assert!(drift.is_empty(), "{drift:?}");
    assert!(appended > 1);
    assert!(old_drift(&store).is_empty());
    assert!(store.operation_projection_drift().unwrap().is_empty());
}

#[test]
fn the_audit_completes_with_a_continuously_appending_writer() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
    let directory = tempfile::tempdir().unwrap();
    let EdgeStore { store, .. } = edge_store(&directory.path().join("state.sqlite3"));
    let store = Arc::new(store);
    let running = Arc::new(AtomicBool::new(true));
    let (start, started) = mpsc::channel();
    let (written, writes) = mpsc::channel();
    let writer_store = Arc::clone(&store);
    let writer_running = Arc::clone(&running);
    let writer = std::thread::spawn(move || {
        started.recv().unwrap();
        let mut appended = 0;
        while writer_running.load(Ordering::Acquire) {
            writer_store.append_claim(&operation_note(&format!("concurrent-{appended}"))).unwrap();
            appended += 1;
            written.send(appended).unwrap();
        }
        appended
    });
    let mut pages = 0;
    let result = store.operation_projection_drift_with_observer(&mut |_| {
        if pages == 0 { start.send(()).unwrap(); }
        // A separate writer remains active throughout the scan, including inside page
        // snapshots; these acknowledgements also prove progress without timing sleeps.
        writes.recv().unwrap();
        pages += 1;
    });
    running.store(false, Ordering::Release);
    let appended = writer.join().unwrap();
    assert!(result.unwrap().is_empty());
    assert!(pages > 1 && appended >= pages, "{pages} pages, {appended} appends");
    assert!(store.operation_projection_drift().unwrap().is_empty());
}

/// A write to an operation the audit may already have read, while it runs. `mutate` runs once,
/// after observation `at`, while later pages and candidate rechecks are still to come.
fn audit_with_mutation_at(store: &Store, at: usize, mutate: &dyn Fn(&Store)) -> Vec<String> {
    let mut observations = 0;
    let drift = store
        .operation_projection_drift_with_observer(&mut |_| {
            observations += 1;
            if observations == at {
                mutate(store);
            }
        })
        .unwrap();
    assert!(observations > at, "the mutation ran after the last snapshot");
    drift
}

/// Every observation of a quiet pass but the last, so each mutation lands in every phase:
/// claim pages, tombstone pages, operation pages and between them.
fn mutation_points(store: &Store) -> std::ops::Range<usize> {
    let quiet = quiet_observations(store);
    assert!(quiet >= 6, "{quiet} observations");
    1..quiet
}

fn edge_store_in(directory: &tempfile::TempDir) -> EdgeStore {
    edge_store(&directory.path().join("state.sqlite3"))
}

/// Drift written below the audit's frontiers while it runs. The pass that is running may miss
/// it, because it already read that operation; it reports nothing else, and the next pass,
/// which starts after the write, reports exactly it.
fn assert_caught_by_this_or_the_next_pass(operation: &str, mutate: &dyn Fn(&Store, &EdgeStore)) {
    let probe = tempfile::tempdir().unwrap();
    let points = mutation_points(&edge_store_in(&probe).store);
    for at in points {
        let directory = tempfile::tempdir().unwrap();
        let edge = edge_store_in(&directory);
        let drift = audit_with_mutation_at(&edge.store, at, &|store| mutate(store, &edge));
        assert!(
            drift.is_empty() || drift == [operation],
            "after snapshot {at}: {drift:?}"
        );
        let next = edge.store.operation_projection_drift().unwrap();
        assert_eq!(next, [operation], "after snapshot {at}");
        assert_eq!(next, old_drift(&edge.store));
    }
}

#[test]
fn projection_drift_written_during_the_audit_is_caught_by_the_next_pass() {
    for (operation, sql) in [
        ("op/active", "UPDATE operations SET state='conflict' WHERE id='op/active'"),
        ("op/many/0000", "DELETE FROM operations WHERE id='op/many/0000'"),
        ("op/huge", "UPDATE operations SET request_digest='d/9' WHERE id='op/huge'"),
    ] {
        assert_caught_by_this_or_the_next_pass(operation, &|store, _| execute(store, sql, []));
    }
}

/// A source change below the frontiers: a claim already read is excluded by a repair, without
/// its operation row following.
#[test]
fn a_source_repaired_during_the_audit_is_caught_by_the_next_pass() {
    assert_caught_by_this_or_the_next_pass("op/duplicate", &|store, edge| {
        let canonical = edge.duplicate.iter().min().unwrap().clone();
        assert_eq!(operation_row(store, "op/duplicate").unwrap().1, canonical);
        mark_repaired(store, vec![canonical]);
    });
}

/// A source change below the frontiers that the projection follows, as replication's receive
/// path does: a conflicting request for an operation already read. Whenever it lands, the pass
/// may see the new row against the old claims, a false candidate; the recheck against the
/// current state must suppress it.
#[test]
fn a_consistent_conflict_written_during_the_audit_is_never_reported() {
    let probe = tempfile::tempdir().unwrap();
    let points = mutation_points(&edge_store_in(&probe).store);
    for at in points {
        let directory = tempfile::tempdir().unwrap();
        let EdgeStore { store, .. } = edge_store_in(&directory);
        let drift = audit_with_mutation_at(&store, at, &|store| {
            write_claims(store, vec![claim("op/many/0001", "d/9", "late-conflict")], 64);
            execute(store, "UPDATE operations SET state='conflict' WHERE id='op/many/0001'", []);
        });
        assert!(drift.is_empty(), "after snapshot {at}: {drift:?}");
        assert_eq!(operation_row(&store, "op/many/0001").unwrap().2, "conflict");
        assert!(old_drift(&store).is_empty());
        assert!(store.operation_projection_drift().unwrap().is_empty());
    }
}

/// Drift repaired while the audit runs: the pass may still name the operation if its recheck
/// ran before the repair, never anything else, and the next pass is clean.
#[test]
fn drift_repaired_during_the_audit_is_gone_from_the_next_pass() {
    let probe = tempfile::tempdir().unwrap();
    let points = mutation_points(&edge_store_in(&probe).store);
    for at in points {
        let directory = tempfile::tempdir().unwrap();
        let EdgeStore { store, .. } = edge_store_in(&directory);
        execute(&store, "UPDATE operations SET state='conflict' WHERE id='op/active'", []);
        let drift = audit_with_mutation_at(&store, at, &|store| {
            execute(store, "UPDATE operations SET state='active' WHERE id='op/active'", []);
        });
        assert!(drift.is_empty() || drift == ["op/active"], "after snapshot {at}: {drift:?}");
        assert!(store.operation_projection_drift().unwrap().is_empty());
        assert!(old_drift(&store).is_empty());
    }
}

#[test]
fn a_cancelled_parent_budget_cannot_turn_an_empty_audit_into_a_clean_report() {
    let store = Store::open_memory(NODE).unwrap();
    // Cover the ordinary idle-reader path without opening a new pool connection.
    assert!(store.operation_projection_drift().unwrap().is_empty());
    let budget = smallclaims::read_budget::ReadBudget::new("/operation-audit", Duration::from_secs(1));
    budget.cancel();
    let mut snapshots = 0;
    let result = smallclaims::read_budget::with(Some(budget), || {
        store.operation_projection_drift_with_observer(&mut |_| snapshots += 1)
    });
    assert!(result.is_err());
    assert_eq!(snapshots, 0, "a cancelled unit must not begin a snapshot");
}

#[test]
fn cancellation_between_units_stops_an_empty_audit() {
    let store = Store::open_memory(NODE).unwrap();
    let budget = smallclaims::read_budget::ReadBudget::new("/operation-audit", Duration::from_secs(1));
    let mut snapshots = 0;
    let result = smallclaims::read_budget::with(Some(budget.clone()), || {
        store.operation_projection_drift_with_observer(&mut |_| {
            snapshots += 1;
            budget.cancel();
        })
    });
    assert!(result.is_err());
    assert_eq!(snapshots, 1, "cancellation must stop the next unit before its snapshot");
}

#[test]
fn operation_audit_repair_commits_at_most_sixteen_keys_per_writer_transaction() {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    let directory = tempfile::tempdir().unwrap();
    let EdgeStore { store, .. } = edge_store_in(&directory);
    execute(&store, "UPDATE operations SET request_digest='d/corrupt'", []);
    let drift = store.operation_projection_drift().unwrap();
    assert!(drift.len() > 32, "the fixture must need several repair batches");
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&commits);
    store.connection.write().commit_hook(Some(move || {
        observed.fetch_add(1, Ordering::Relaxed);
        false
    }));
    assert!(store.repair_operation_projection_drift_from_audit(&drift).unwrap());
    store.connection.write().commit_hook(None::<fn() -> bool>);
    assert_eq!(commits.load(Ordering::Relaxed), drift.len().div_ceil(16));
    assert!(old_drift(&store).is_empty());
    assert!(store.operation_projection_drift().unwrap().is_empty());
}
