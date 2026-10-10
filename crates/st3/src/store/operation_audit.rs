//! Operation projection audits retain narrow per-operation reductions, never claim bodies.
//!
//! Correctness: fix claim, checkpoint and operation row frontiers at the start; later
//! appends cannot extend the scan. Pages may observe different committed states. Every
//! drift candidate is re-verified against current metadata in one short snapshot before
//! reporting; repair re-derives it again under the writer. A concurrent below-frontier
//! mutation can introduce an inconsistency this pass misses: the next scheduled audit
//! catches it. This is a detector and repair source, not a global consistency gate.
//! Pages contain at most 128 rows, yield after 20 ms, and interrupt SQLite after 40 ms.
//! The 100 ms snapshot target is cooperative: OS scheduling, an individual filesystem call,
//! and a single SQLite JSON extraction cannot be preempted by its VM progress handler.
use super::*;
use std::time::{Duration, Instant};

// The callback consumes each row immediately. No body, page vector, or actual-table map
// survives a page. The observer runs after ROLLBACK has released the read snapshot.
fn page(
    connection: &Connection,
    sql: &str,
    parameters: &[&dyn rusqlite::ToSql],
    consume: &mut dyn FnMut(&rusqlite::Row<'_>) -> Result<()>,
    observer: &mut dyn FnMut(Duration),
) -> Result<usize> {
    connection.busy_timeout(Duration::from_millis(40))?;
    for attempt in 0..3 {
        // Cheap empty/index seeks may not reach a VM progress callback. Preserve the
        // caller's cancellation before replacing the pool's one-step cancelled handler.
        smallclaims::read_budget::check()?;
        let started = Instant::now();
        connection.progress_handler(1000, Some(move || {
            started.elapsed() >= Duration::from_millis(40) || smallclaims::read_budget::check().is_err()
        }));
        let transaction = connection.unchecked_transaction()?;
        let mut count = 0;
        let result = (|| {
            let mut statement = transaction.prepare_cached(sql)?;
            let mut rows = statement.query(parameters)?;
            while let Some(row) = rows.next()? {
                consume(row)?;
                count += 1;
                if started.elapsed() >= Duration::from_millis(20) { break; }
            }
            Ok(count)
        })();
        // Clear the deadline before ROLLBACK, including an interrupted first SQLite step.
        connection.progress_handler(0, None::<fn() -> bool>);
        drop(transaction);
        observer(started.elapsed());
        let interrupted = matches!(
            result.as_ref().err().and_then(|error: &anyhow::Error| error.downcast_ref::<rusqlite::Error>()),
            Some(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::OperationInterrupted
        );
        if interrupted && smallclaims::read_budget::check().is_ok() {
            // Consumed rows have already advanced the caller's cursor. Resume in a new
            // snapshot rather than throwing away useful metadata after a cold-page stall.
            if count != 0 { return Ok(count); }
            if attempt < 2 { continue; }
        }
        return result;
    }
    unreachable!("the final bounded page attempt always returns")
}

impl Store {
    pub(super) fn operation_audit(&self, observer: &mut dyn FnMut(Duration)) -> Result<Vec<String>> {
        smallclaims::read_budget::check()?;
        let connection = self.readers.get();
        let mut claim_cut = 0_i64;
        let mut checkpoint_cut = 0_i64;
        let mut operation_cut = 0_i64;
        page(&connection,
            "SELECT COALESCE((SELECT MAX(store_index) FROM claims),0),
                    COALESCE((SELECT MAX(rowid) FROM checkpoint_claims),0),
                    COALESCE((SELECT MAX(rowid) FROM operations),0)",
            &[], &mut |row| {
                claim_cut = row.get(0)?;
                checkpoint_cut = row.get(1)?;
                operation_cut = row.get(2)?;
                Ok(())
            }, observer)?;
        let mut expected = BTreeMap::<String, OperationMetadata>::new();
        let mut operation = String::new();
        let mut claim_rowid = 0_i64;
        let mut remainder = true;
        loop {
            let cursor = operation.clone();
            let rowid = claim_rowid;
            // Separate seeks avoid a row-value SCAN and a UNION merge's unbounded
            // temporary sort for a single operation with many claims.
            let sql = if remainder {
                "SELECT json_extract(body,'$._operation.id'),
                        json_extract(body,'$._operation.request_digest'), id, rowid
                 FROM claims INDEXED BY claims_operation_index
                 WHERE json_extract(body,'$._operation.id')=?1 AND rowid>?2 AND store_index<=?3
                   AND json_type(body,'$._operation.id')='text'
                   AND json_type(body,'$._operation.request_digest')='text'
                   AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)
                 ORDER BY rowid LIMIT 128"
            } else {
                "SELECT json_extract(body,'$._operation.id'),
                        json_extract(body,'$._operation.request_digest'), id, rowid
                 FROM claims INDEXED BY claims_operation_index
                 WHERE json_extract(body,'$._operation.id')>?1 AND ?2 IS NOT NULL AND store_index<=?3
                   AND json_type(body,'$._operation.id')='text'
                   AND json_type(body,'$._operation.request_digest')='text'
                   AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)
                 ORDER BY json_extract(body,'$._operation.id'),rowid LIMIT 128"
            };
            let count = page(&connection, sql, &[&cursor, &rowid, &claim_cut], &mut |row| {
                operation = row.get(0)?;
                let digest: String = row.get(1)?;
                let id: String = row.get(2)?;
                claim_rowid = row.get(3)?;
                expected.entry(operation.clone()).or_default().include(digest, id, true);
                Ok(())
            }, observer)?;
            if count == 0 {
                if !remainder { break; }
                remainder = false;
            } else {
                remainder = true;
            }
        }
        let mut tombstone_rowid = 0_i64;
        loop {
            let cursor = tombstone_rowid;
            let count = page(&connection,
                "SELECT rowid,operation_id,request_digest,id FROM checkpoint_claims
                 WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT 128", &[&cursor, &checkpoint_cut], &mut |row| {
                    let operation: Option<String> = row.get(1)?;
                    let digest: Option<String> = row.get(2)?;
                    if let (Some(operation), Some(digest)) = (operation, digest) {
                        if let Some(reduction) = expected.get_mut(&operation) {
                            let id: String = row.get(3)?;
                            let excluded: bool = connection.query_row(
                                "SELECT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=?1)
                                 OR EXISTS(SELECT 1 FROM claims WHERE id=?1
                                   AND json_type(body,'$._operation.id')='text'
                                   AND json_type(body,'$._operation.request_digest')='text'
                                   AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id))",
                                [&id], |row| row.get(0))?;
                            if !excluded { reduction.include(digest, id, false); }
                        }
                    }
                    tombstone_rowid = row.get(0)?;
                    Ok(())
                }, observer)?;
            if count == 0 { break; }
        }
        let mut expected: BTreeMap<_, _> = expected.into_iter().filter_map(|(id, value)| value.row().map(|row| (id, row))).collect();
        let mut drift = Vec::new();
        let mut actual_cursor: Option<String> = None;
        loop {
            let first = actual_cursor.is_none();
            let cursor = actual_cursor.clone().unwrap_or_default();
            let count = page(&connection,
                "SELECT id,request_digest,canonical_claim_id,state FROM operations
                 WHERE id>=?1 AND (?2 OR id>?1) AND rowid<=?3 ORDER BY id LIMIT 128", &[&cursor, &first, &operation_cut], &mut |row| {
                    let id: String = row.get(0)?;
                    let actual = (row.get(1)?, row.get(2)?, row.get(3)?);
                    if expected.remove(&id).as_ref() != Some(&actual) { drift.push(id.clone()); }
                    actual_cursor = Some(id);
                    Ok(())
                }, observer)?;
            if count == 0 { break; }
        }
        drift.extend(expected.into_keys());
        drift.sort();
        let mut verified = Vec::new();
        for id in drift {
            let mut current_drift = false;
            page(&connection,
                "SELECT (SELECT request_digest FROM operations WHERE id=?1),
                        (SELECT canonical_claim_id FROM operations WHERE id=?1),
                        (SELECT state FROM operations WHERE id=?1)",
                &[&id], &mut |row| {
                    let digest: Option<String> = row.get(0)?;
                    let actual = match digest {
                        Some(digest) => Some((digest, row.get(1)?, row.get(2)?)),
                        None => None,
                    };
                    current_drift = expected_operation(&connection, &id)? != actual;
                    Ok(())
                }, observer)?;
            if current_drift { verified.push(id); }
        }
        Ok(verified)
    }

    #[cfg(feature = "test-support")]
    pub fn operation_projection_drift_with_observer(&self, observer: &mut dyn FnMut(Duration)) -> Result<Vec<String>> {
        self.operation_audit(observer)
    }
}
