//! Observer-only actual reductions. Native row notifications invalidate at writer resolution;
//! the existing preparation fence remains the authority for using a prepared observation.
use super::*;
use smallclaims::sqlite::WriterObserver;

const LIMIT: usize = 4096;
const BYTE_LIMIT: usize = 8 * 1024 * 1024;
const PENDING_LIMIT: usize = 8192;

#[cfg(test)]
#[path = "observer_actuals_tests.rs"]
mod tests;

#[derive(Default)]
pub struct Actuals(Mutex<State>);

#[derive(Default)]
struct State {
    serial: u64,
    native: Option<(i64, i64)>,
    frontier: i64,
    record_frontier: i64,
    global: bool,
    coverage_failed: bool,
    pending: BTreeSet<(&'static str, i64)>,
    entries: VecDeque<(String, Option<Value>, usize)>,
    bytes: usize,
}

impl State {
    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

impl Actuals {
    pub fn invalidate(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.clear();
        state.serial = state.serial.wrapping_add(1);
    }

    /// Called while holding the autocommit writer, before opening the preparation snapshot.
    pub fn seed(&self, connection: &Connection, reverify: impl FnOnce()) -> Result<u64> {
        let native = (
            connection.query_row("PRAGMA main.data_version", [], |row| row.get(0))?,
            connection.query_row("PRAGMA main.schema_version", [], |row| row.get(0))?,
        );
        let changed = {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if state.native != Some(native) {
                // External DDL bypasses the dispatcher's authorizer. Clearing entries alone
                // cannot establish that subsequent row notifications still cover the fold.
                state.coverage_failed = true;
                state.clear();
                state.serial = state.serial.wrapping_add(1);
                true
            } else {
                false
            }
        };
        if changed {
            // This must run without the cache mutex: verification publishes callbacks here.
            reverify();
        }
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.native != Some(native) || state.global {
            state.clear();
            state.serial = state.serial.wrapping_add(1);
            state.native = Some(native);
            state.global = false;
        }
        if state.entries.is_empty() {
            state.frontier = connection.query_row(
                "SELECT COALESCE(MAX(store_index),0) FROM claims", [], |row| row.get(0))?;
            state.record_frontier = connection.query_row(
                "SELECT COALESCE(MAX(rowid),0) FROM replica_records", [], |row| row.get(0))?;
        }
        Ok(state.serial)
    }

    pub fn read(&self, connection: &Connection, subject: &str, serial: u64) -> Result<Option<Value>> {
        // Arrangement reductions join registers on other subjects, unlike actual claim folds.
        if subject.starts_with("arrangement/") {
            return latest_actual(connection, subject);
        }
        {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.coverage_failed && state.serial == serial
                && let Some(position) = state.entries.iter().position(|(key, _, _)| key == subject)
            {
                let entry = state.entries.remove(position).expect("located entry");
                let value = entry.1.clone();
                state.entries.push_back(entry);
                return Ok(value);
            }
        }
        let value = latest_actual(connection, subject)?;
        let bytes = subject.len() + value.as_ref().map_or(0, value_bytes);
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // A concurrent resolution can invalidate the read's seed. Never install that answer.
        if !state.coverage_failed && state.serial == serial && bytes <= BYTE_LIMIT {
            if let Some(position) = state.entries.iter().position(|(key, _, _)| key == subject) {
                let (_, _, old_bytes) = state.entries.remove(position).expect("located entry");
                state.bytes -= old_bytes;
            }
            while state.entries.len() >= LIMIT || state.bytes + bytes > BYTE_LIMIT {
                if let Some((_, _, old_bytes)) = state.entries.pop_front() {
                    state.bytes -= old_bytes;
                }
            }
            state.bytes += bytes;
            state.entries.push_back((subject.to_owned(), value.clone(), bytes));
        }
        Ok(value)
    }
}

// Conservative retained-payload accounting, without serializing/copying the folded value.
fn value_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>() + match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => values.capacity() * std::mem::size_of::<Value>()
            + values.iter().map(value_bytes).sum::<usize>(),
        Value::Object(values) => values.iter().map(|(key, value)|
            key.capacity() + 128 + value_bytes(value)).sum(),
        _ => 0,
    }
}

/// Sealing local batches adds explicit positions equal to their legacy canonical ranks.
/// Only a changed effective rank invalidates an otherwise unchanged cached subject.
fn record_rank_changed(connection: &Connection, rowid: i64, previous_frontier: i64) -> rusqlite::Result<bool> {
    connection.prepare_cached(
        "SELECT (SELECT MIN(position) FROM replica_records WHERE claim_id=claim.id)
            IS NOT COALESCE(
                (SELECT MIN(position) FROM replica_records
                 WHERE claim_id=claim.id AND rowid<=?2),
                (SELECT COUNT(*) FROM claims legacy_position
                 WHERE legacy_position.batch_id=claim.batch_id
                   AND legacy_position.store_index<claim.store_index))
         FROM replica_records inserted JOIN claims claim ON claim.id=inserted.claim_id
         WHERE inserted.rowid=?1"
    )?.query_row(params![rowid, previous_frontier], |row| row.get(0))
}

impl WriterObserver for Actuals {
    fn tables(&self) -> &'static [(&'static str, &'static str)] {
        &[("main", "claims"), ("main", "batches"), ("main", "replica_records"),
          ("main", "projection_digest_repaired_claims")]
    }

    fn mutated(&self, database: &str, table: &str) {
        if self.tables().contains(&(database, table)) {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).global = true;
        }
    }

    fn row_mutated(&self, action: rusqlite::hooks::Action, database: &str, table: &str, rowid: i64) {
        if database != "main" { return; }
        let table = match table {
            "claims" => "claims",
            "replica_records" => "replica_records",
            "projection_digest_repaired_claims" => "projection_digest_repaired_claims",
            "batches" => "batches",
            _ => return,
        };
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Fence speculative readers immediately, including between commits in one writer loan.
        // Publication can then compare final canonical metadata with the cached prefix safely.
        state.serial = state.serial.wrapping_add(1);
        // Updates/deletions may have changed the old subject/claim identity; bulk work and
        // missing rows are deliberately global. Rollback notifications over-invalidate only.
        if action != rusqlite::hooks::Action::SQLITE_INSERT || state.pending.len() >= PENDING_LIMIT {
            state.global = true;
        } else if !state.global {
            state.pending.insert((table, rowid));
        }
    }

    fn unobserved(&self, _: &str, _: &str) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).global = true;
    }

    fn failed(&self, _: Arc<str>) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.coverage_failed = true;
        state.clear();
        state.serial = state.serial.wrapping_add(1);
        state.global = true;
    }

    fn coverage_verified(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.coverage_failed = false;
        state.clear();
        state.serial = state.serial.wrapping_add(1);
        state.global = true;
    }

    fn published(&self, connection: &Connection) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.global && state.pending.is_empty() { return; }
        state.serial = state.serial.wrapping_add(1);
        let pending = std::mem::take(&mut state.pending);
        let next_record_frontier = pending.iter()
            .filter(|entry| entry.0 == "replica_records").map(|entry| entry.1)
            .max().unwrap_or(state.record_frontier).max(state.record_frontier);
        let mut subjects = BTreeSet::new();
        if !state.global && !state.entries.is_empty() {
            for (table, rowid) in pending {
                if table == "replica_records" && rowid <= state.record_frontier {
                    state.global = true;
                    break;
                }
                if table == "claims" {
                    if rowid <= state.frontier {
                        // Explicit insertion into an old arrival slot can move other subjects'
                        // legacy canonical COUNT ranks. Ordinary append-only arrivals cannot.
                        state.global = true;
                        break;
                    }
                    state.frontier = rowid;
                }
                if table == "batches" {
                    let batch_subjects = (|| -> rusqlite::Result<Vec<String>> {
                        connection.prepare_cached(
                            "SELECT subject FROM claims WHERE batch_id=(SELECT id FROM batches WHERE rowid=?1) LIMIT ?2"
                        )?.query_map(params![rowid, PENDING_LIMIT + 1], |row| row.get(0))?.collect()
                    })();
                    match batch_subjects {
                        Ok(batch_subjects) if batch_subjects.len() <= PENDING_LIMIT => {
                            subjects.extend(batch_subjects);
                            if subjects.len() > PENDING_LIMIT {
                                state.global = true;
                                break;
                            }
                        }
                        _ => { state.global = true; break; }
                    }
                    continue;
                }
                let sql = match table {
                    "claims" => "SELECT subject FROM claims WHERE rowid=?1",
                    "replica_records" => "SELECT claims.subject FROM replica_records LEFT JOIN claims ON claims.id=replica_records.claim_id WHERE replica_records.rowid=?1 AND replica_records.claim_id IS NOT NULL",
                    _ => "SELECT subject FROM claims WHERE id=(SELECT id FROM projection_digest_repaired_claims WHERE rowid=?1)",
                };
                match connection.prepare_cached(sql).and_then(|mut statement|
                    statement.query_row([rowid], |row| row.get::<_, String>(0)).optional()) {
                    Ok(Some(subject)) => {
                        if table == "replica_records" {
                            if !state.entries.iter().any(|(key, _, _)| key == &subject) {
                                continue;
                            }
                            match record_rank_changed(connection, rowid, state.record_frontier) {
                                Ok(false) => continue,
                                Ok(true) => {}
                                Err(_) => { state.global = true; break; }
                            }
                        }
                        subjects.insert(subject);
                    }
                    Ok(None) if table == "replica_records" => {}
                    Ok(None) | Err(_) => { state.global = true; break; }
                }
            }
        }
        state.record_frontier = next_record_frontier;
        if state.global {
            state.clear();
            state.global = false;
        } else {
            state.entries.retain(|(subject, _, _)| !subjects.contains(subject));
            state.bytes = state.entries.iter().map(|(_, _, bytes)| bytes).sum();
        }
    }
}
