//! Bounded, opt-in writer evidence for file-backed load fixtures. No SQL is executed here.
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const ACTIVE_LIMIT: usize = 128;
const RECENT_LIMIT: usize = 512;
static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

#[derive(Clone, Serialize)]
pub struct Transaction {
    pub start_ms: f64,
    pub end_ms: f64,
    pub held_ms: f64,
    pub mode: &'static str,
    pub completion: &'static str,
    pub mutations: BTreeSet<String>,
    pub caller_kinds: BTreeSet<String>,
}
struct Active {
    start: Instant,
    mode: &'static str,
    mutations: BTreeSet<String>,
    caller_kinds: BTreeSet<String>,
}
struct State {
    path: Vec<u8>,
    origin: Instant,
    active: BTreeMap<usize, Active>,
    recent: VecDeque<Transaction>,
    longest: Vec<Transaction>,
    completed: u64,
    missed_connections: u64,
}
#[derive(Serialize)]
pub struct Report {
    pub completed: u64,
    pub missed_connections: u64,
    pub active: usize,
    pub longest: Vec<Transaction>,
    pub in_flight: Vec<Transaction>,
    pub recent: Vec<Transaction>,
}
/// Keep capture scoped to one private fixture, excluding peer and unrelated test databases.
pub struct Capture {
    _private: (),
}
pub fn capture(path: &Path) -> Capture {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(state.is_none(), "transaction capture already active");
    *state = Some(State {
        path: path.as_os_str().as_encoded_bytes().to_vec(),
        origin: Instant::now(),
        active: BTreeMap::new(),
        recent: VecDeque::new(),
        longest: Vec::new(),
        completed: 0,
        missed_connections: 0,
    });
    ENABLED.store(true, Ordering::Release);
    Capture { _private: () }
}
impl Drop for Capture {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Release);
        STATE.lock().unwrap_or_else(PoisonError::into_inner).take();
    }
}
impl Capture {
    pub fn report(&self) -> Report {
        let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        let state = state.as_ref().expect("capture alive");
        Report {
            completed: state.completed,
            missed_connections: state.missed_connections,
            active: state.active.len(),
            longest: state.longest.clone(),
            in_flight: in_flight(state),
            recent: state.recent.iter().cloned().collect(),
        }
    }
}
pub fn is_active() -> bool {
    ENABLED.load(Ordering::Acquire)
}
pub fn elapsed_ms() -> f64 {
    STATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map_or(0.0, |state| state.origin.elapsed().as_secs_f64() * 1000.0)
}
pub fn overlapping(since_ms: f64) -> Vec<Transaction> {
    let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(state) = state.as_ref() else {
        return Vec::new();
    };
    state
        .recent
        .iter()
        .filter(|tx| tx.end_ms >= since_ms)
        .cloned()
        .chain(in_flight(state))
        .collect()
}
fn in_flight(state: &State) -> Vec<Transaction> {
    let now = Instant::now();
    state
        .active
        .values()
        .map(|active| Transaction {
            start_ms: active
                .start
                .saturating_duration_since(state.origin)
                .as_secs_f64()
                * 1000.0,
            end_ms: now.saturating_duration_since(state.origin).as_secs_f64() * 1000.0,
            held_ms: now.saturating_duration_since(active.start).as_secs_f64() * 1000.0,
            mode: active.mode,
            completion: "in-flight",
            mutations: active.mutations.clone(),
            caller_kinds: active.caller_kinds.clone(),
        })
        .collect()
}

/// SQLite invokes this while the connection and SQL string remain live. Autocommit distinguishes
/// a successful BEGIN from a busy admission, and a committed/rolled-back end from a failed COMMIT.
pub(super) unsafe fn statement(db: *mut rusqlite::ffi::sqlite3, sql: &str, duration: Duration) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let sql = sql.trim_start();
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(';');
    let begin = first.eq_ignore_ascii_case("BEGIN");
    let end = first.eq_ignore_ascii_case("COMMIT") || first.eq_ignore_ascii_case("ROLLBACK");
    let mutation = [
        "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "ALTER", "DROP",
    ]
    .iter()
    .any(|word| first.eq_ignore_ascii_case(word));
    if !begin && !end && !mutation {
        return;
    }
    // SAFETY: the profiling callback's context is this still-live SQLite connection.
    let autocommit = unsafe { rusqlite::ffi::sqlite3_get_autocommit(db) != 0 };
    let now = Instant::now();
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(state) = state.as_mut() else {
        return;
    };
    let key = db as usize;
    // SAFETY: SQLite owns this filename for the live connection; it is read synchronously.
    let path = unsafe { rusqlite::ffi::sqlite3_db_filename(db, c"main".as_ptr()) };
    if path.is_null()
        || unsafe { std::ffi::CStr::from_ptr(path).to_bytes() } != state.path.as_slice()
    {
        if state.active.remove(&key).is_some() {
            state.missed_connections += 1;
        }
        return;
    }
    // A nested BEGIN or a reused unclosed handle makes the old interval ambiguous. Report
    // the gap rather than attributing that elapsed time to a known writer.
    if begin && !autocommit && state.active.remove(&key).is_some() {
        state.missed_connections += 1;
    }
    if !state.active.contains_key(&key) && (begin && !autocommit || mutation) {
        if state.active.len() == ACTIVE_LIMIT {
            state.missed_connections += 1;
            return;
        }
        let immediate = begin
            && (sql.to_ascii_uppercase().contains("IMMEDIATE")
                || sql.to_ascii_uppercase().contains("EXCLUSIVE"));
        if begin && !immediate {
            return;
        } // Deferred readers hold no writer reservation.
        state.active.insert(
            key,
            Active {
                start: if begin {
                    now
                } else {
                    now.checked_sub(duration + Duration::from_millis(1))
                        .unwrap_or(now)
                },
                mode: if begin {
                    "after-successful-BEGIN"
                } else {
                    "first-write-upper-bound"
                },
                mutations: BTreeSet::new(),
                caller_kinds: BTreeSet::new(),
            },
        );
    }
    if let Some(active) = state.active.get_mut(&key)
        && active.caller_kinds.len() < 8
        && let Some(kind) = crate::performance::current_kind_for_trace()
    {
        active.caller_kinds.insert(kind);
    }
    if let Some(active) = state.active.get_mut(&key)
        && mutation
        && active.mutations.len() < 8
    {
        active.mutations.insert(
            sql.split_whitespace()
                .take(5)
                .map(|word| {
                    word.split('(')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(32)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if autocommit && let Some(active) = state.active.remove(&key) {
        let tx = Transaction {
            start_ms: active
                .start
                .saturating_duration_since(state.origin)
                .as_secs_f64()
                * 1000.0,
            end_ms: now.saturating_duration_since(state.origin).as_secs_f64() * 1000.0,
            held_ms: now.saturating_duration_since(active.start).as_secs_f64() * 1000.0,
            mode: active.mode,
            completion: if first.eq_ignore_ascii_case("COMMIT") {
                "commit"
            } else if first.eq_ignore_ascii_case("ROLLBACK") {
                "rollback"
            } else {
                "autocommit-restored"
            },
            mutations: active.mutations,
            caller_kinds: active.caller_kinds,
        };
        state.completed += 1;
        state.recent.push_back(tx.clone());
        if state.recent.len() > RECENT_LIMIT {
            state.recent.pop_front();
        }
        state.longest.push(tx);
        state
            .longest
            .sort_by(|a, b| b.held_ms.total_cmp(&a.held_ms));
        state.longest.truncate(32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, TransactionBehavior};
    static SERIAL: Mutex<()> = Mutex::new(());
    #[test]
    fn committed_and_rolled_back_writers_have_bounded_connection_scoped_evidence() {
        let _serial = SERIAL.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("trace.sqlite");
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value);")
            .unwrap();
        super::super::observe(&mut connection);
        let capture = capture(&path);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO data VALUES(1)", []).unwrap();
        tx.commit().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO data VALUES(2)", []).unwrap();
        tx.rollback().unwrap();
        let report = capture.report();
        assert_eq!(report.completed, 2);
        assert_eq!((report.active, report.missed_connections), (0, 0));
        assert_eq!(report.recent[0].completion, "commit");
        assert_eq!(report.recent[1].completion, "rollback");
        assert!(report.recent.iter().all(|tx| {
            tx.mode == "after-successful-BEGIN"
                && tx.held_ms >= 0.0
                && tx.end_ms >= tx.start_ms
                && tx
                    .mutations
                    .iter()
                    .any(|kind| kind.starts_with("INSERT INTO data"))
        }));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn busy_begin_is_no_writer_and_other_databases_are_excluded() {
        let _serial = SERIAL.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("trace.sqlite");
        let mut holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value);")
            .unwrap();
        let tx = holder
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut contender = Connection::open(&path).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        super::super::observe(&mut contender);
        let capture = capture(&path);
        assert!(
            contender
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .is_err()
        );
        assert_eq!(capture.report().active, 0);
        assert_eq!(capture.report().completed, 0);
        let mut other = Connection::open(root.path().join("other.sqlite")).unwrap();
        super::super::observe(&mut other);
        other
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE unrelated(value); COMMIT;")
            .unwrap();
        assert_eq!(capture.report().completed, 0);
        tx.rollback().unwrap();
        let tx = contender
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(capture.report().completed, 1);
        assert_eq!(capture.report().active, 0);
    }
    #[test]
    fn trace_storage_is_bounded_and_observation_executes_no_sql() {
        let _serial = SERIAL.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("trace.sqlite");
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value);")
            .unwrap();
        super::super::observe(&mut connection);
        let capture = capture(&path);
        for _ in 0..600 {
            connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap()
                .commit()
                .unwrap();
        }
        let report = capture.report();
        assert_eq!(
            (report.completed, report.recent.len(), report.longest.len()),
            (600, 512, 32)
        );
        assert_eq!((report.active, report.missed_connections), (0, 0));
        assert!(report.in_flight.is_empty());
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
