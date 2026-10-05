//! st-owned observation outbox. Opt-in is per directory, never process-global: an adopted
//! provider keeps its transport while a fresh st seat initializes this database before spawn.
//! A snapshot and its event commit together. The driver removes only acknowledged events;
//! readers of provider-local evidence do not depend on the daemon being reachable.
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness_timeline::{Operation, Record};

pub const WAKE_PIPE: &str = ".st-harness-events-wake";
pub const DATABASE: &str = "st-harness-events.sqlite";
const MAX_PENDING_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub runtime_incarnation: String,
    pub queued_at_ms: u64,
    pub kind: String,
    pub payload: Value,
}

pub fn enabled(agent_dir: &Path) -> bool {
    agent_dir.join(DATABASE).symlink_metadata().is_ok()
}

fn connect(agent_dir: &Path) -> Result<Connection> {
    let path = agent_dir.join(DATABASE);
    anyhow::ensure!(
        path.symlink_metadata()?.file_type().is_file(),
        "event spool must be a regular file"
    );
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA synchronous=FULL;")?;
    Ok(connection)
}

fn open(agent_dir: &Path) -> Result<Connection> {
    let connection = connect(agent_dir)?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    anyhow::ensure!(
        version == 1,
        "unsupported harness event spool version {version}"
    );
    Ok(connection)
}

/// Bind a fresh pipe inode before exposing its name. A predecessor retains its old descriptor
/// and cannot steal its successor's wakeups. Pipe bytes are hints; startup always replays SQLite.
pub fn bind_wake_pipe(agent_dir: &Path) -> Result<std::fs::File> {
    use std::os::unix::{
        ffi::OsStrExt as _,
        fs::{FileTypeExt as _, OpenOptionsExt as _},
    };
    let path = agent_dir.join(WAKE_PIPE);
    if let Ok(metadata) = path.symlink_metadata() {
        anyhow::ensure!(
            metadata.file_type().is_fifo(),
            "event wake path is not a pipe"
        );
    }
    let staging = agent_dir.join(format!(
        ".st-event-wake-{}",
        crate::harness_state::session_token()
    ));
    let name = std::ffi::CString::new(staging.as_os_str().as_bytes())?;
    // SAFETY: name is a terminated path and mkfifo retains no pointer.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let bound = (|| -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&staging)?;
        std::fs::rename(&staging, &path)?;
        Ok(file)
    })();
    if bound.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    bound
}

fn signal_wake(agent_dir: &Path) {
    use std::io::Write as _;
    use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};
    let signal = (|| -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(agent_dir.join(WAKE_PIPE))?;
        if !file.metadata()?.file_type().is_fifo() {
            return Err(io::Error::other("event wake path is not a pipe"));
        }
        file.write_all(&[1])
    })();
    if let Err(error) = signal {
        // Absent/no-reader is the startup/reexec interval, covered by the initial replay. A full
        // pipe already has a pending wake. Never block a harness or report a committed write lost.
        if error.kind() != io::ErrorKind::NotFound
            && error.kind() != io::ErrorKind::WouldBlock
            && error.raw_os_error() != Some(libc::ENXIO)
        {
            tracing::warn!("st harness event wake failed: {error}");
        }
    }
}

/// Called only for a fresh launch. Existing events keep their original runtime provenance.
pub fn enable(agent_dir: &Path, runtime_incarnation: &str) -> Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    let path = agent_dir.join(DATABASE);
    if !enabled(agent_dir) {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
    }
    let mut connection = connect(agent_dir)?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    anyhow::ensure!(
        version <= 1,
        "unsupported harness event spool version {version}"
    );
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA user_version=1;
        CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS snapshots (kind TEXT PRIMARY KEY, body BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            runtime_incarnation TEXT NOT NULL, queued_at_ms INTEGER NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS prepared (
            sequence INTEGER NOT NULL, slot TEXT NOT NULL, body TEXT NOT NULL,
            PRIMARY KEY(sequence,slot));
        CREATE TABLE IF NOT EXISTS timeline (
            id INTEGER PRIMARY KEY AUTOINCREMENT, incarnation TEXT NOT NULL,
            source TEXT NOT NULL, body TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS timeline_source ON timeline(incarnation,source,id);",
    )?;
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO metadata(key,value)
        SELECT 'pending-bytes', COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM events",
        [],
    )?;
    tx.execute(
        "INSERT INTO metadata VALUES ('runtime',?1)
        ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [runtime_incarnation],
    )?;
    tx.commit()?;
    std::fs::File::open(&path)?.sync_all()?;
    std::fs::File::open(agent_dir)?.sync_all()?;
    Ok(())
}

fn append_event(tx: &Connection, kind: &str, body: &Value) -> Result<()> {
    let bytes: u64 = tx.query_row(
        "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='pending-bytes'",
        [],
        |row| row.get(0),
    )?;
    let token = body["incarnation"]
        .as_str()
        .or_else(|| body["incarnationId"].as_str())
        .ok_or_else(|| anyhow::anyhow!("event needs provider ownership"))?;
    let runtime: String = tx.query_row(
        "SELECT value FROM metadata WHERE key=?1",
        [format!("provider-runtime:{token}")],
        |r| r.get(0),
    )?;
    // Capture the binding at the producer. A successor may drain this event after switching
    // accounts, so its own environment cannot supply the event's paying account.
    let mut body = body.clone();
    body["account_ref"] = std::env::var("ST3_ACCOUNT")
        .ok()
        .filter(|name| !name.is_empty())
        .map(Value::String)
        .unwrap_or(Value::Null);
    let body = serde_json::to_string(&body)?;
    anyhow::ensure!(
        bytes.saturating_add(body.len() as u64) <= MAX_PENDING_BYTES,
        "harness event spool is full; observation was not committed"
    );
    tx.execute(
        "INSERT INTO events(runtime_incarnation,queued_at_ms,kind,body) VALUES (?1,?2,?3,?4)",
        params![runtime, crate::message::now_ms(), kind, body],
    )?;
    tx.execute(
        "UPDATE metadata SET value=?1 WHERE key='pending-bytes'",
        [bytes + body.len() as u64],
    )?;
    Ok(())
}

pub fn read_snapshot(agent_dir: &Path, kind: &str) -> Result<Option<Vec<u8>>> {
    Ok(open(agent_dir)?
        .query_row("SELECT body FROM snapshots WHERE kind=?1", [kind], |row| {
            row.get(0)
        })
        .optional()?)
}

/// Read saved provider state only when its producer belongs to `runtime`.
/// A new runtime retains its predecessor's snapshot for durable history, but must not use it
/// as evidence that its own provider has launched. A driver re-exec keeps the same runtime.
pub fn read_runtime_state(agent_dir: &Path, runtime: &str) -> Result<Option<Vec<u8>>> {
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction()?;
    let raw: Option<Vec<u8>> = tx
        .query_row(
            "SELECT body FROM snapshots WHERE kind='harness-state'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let state: Value = serde_json::from_slice(&raw)?;
    let Some(token) = state["incarnation"].as_str() else {
        return Ok(None);
    };
    let owner: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [format!("provider-runtime:{token}")],
            |row| row.get(0),
        )
        .optional()?;
    Ok((owner.as_deref() == Some(runtime)).then_some(raw))
}

fn current_token(connection: &Connection) -> Result<Option<String>> {
    let raw: Option<Vec<u8>> = connection
        .query_row(
            "SELECT body FROM snapshots WHERE kind='harness-state'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|state| state["incarnation"].as_str().map(str::to_owned)))
}

pub fn write_snapshot(agent_dir: &Path, kind: &str, body: &[u8]) -> Result<()> {
    anyhow::ensure!(
        matches!(kind, "harness-state" | "harness-context" | "harness-todo"),
        "unsupported observation kind"
    );
    let value: Value = serde_json::from_slice(body)?;
    anyhow::ensure!(
        value["incarnation"]
            .as_str()
            .is_some_and(|token| !token.is_empty()),
        "observation needs an owner"
    );
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if kind == "harness-state" {
        let token = value["incarnation"].as_str().unwrap();
        if current_token(&tx)?.as_deref() != Some(token) {
            tx.execute("DELETE FROM metadata WHERE key LIKE 'provider-runtime:%' OR key LIKE 'timeline-next:%'", [])?;
            tx.execute(
                "INSERT INTO metadata(key,value) SELECT ?1,value FROM metadata WHERE key='runtime'",
                [format!("provider-runtime:{token}")],
            )?;
        }
    }
    // Context writers used to have no ownership fence. Refuse a delayed predecessor now that
    // its snapshot and event are admitted in the same transaction as the ownership check.
    if matches!(kind, "harness-context" | "harness-todo") {
        anyhow::ensure!(
            current_token(&tx)?.as_deref() == value["incarnation"].as_str(),
            "harness observation owner was superseded"
        );
    }
    tx.execute(
        "INSERT INTO snapshots VALUES (?1,?2)
        ON CONFLICT(kind) DO UPDATE SET body=excluded.body",
        params![kind, body],
    )?;
    append_event(&tx, kind, &value)?;
    tx.commit()?;
    signal_wake(agent_dir);
    Ok(())
}

/// Graph-native channels have no provider record writer. Their spool is scoped to one
/// authenticated runtime incarnation; daemon publication still applies the runtime fence.
pub fn write_channel_todo(agent_dir: &Path, runtime: &str, fields: &Value) -> Result<()> {
    let mut value = fields.clone();
    value["incarnation"] = runtime.into();
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let bound: String = tx.query_row(
        "SELECT value FROM metadata WHERE key='runtime'", [], |row| row.get(0),
    )?;
    anyhow::ensure!(bound == runtime, "channel todo runtime was superseded");
    tx.execute(
        "INSERT OR IGNORE INTO metadata(key,value) VALUES (?1,?2)",
        params![format!("provider-runtime:{runtime}"), runtime],
    )?;
    append_event(&tx, "harness-todo", &value)?;
    tx.execute(
        "INSERT INTO snapshots VALUES ('harness-todo',?1)
        ON CONFLICT(kind) DO UPDATE SET body=excluded.body",
        [serde_json::to_vec(&value)?],
    )?;
    tx.commit()?;
    signal_wake(agent_dir);
    Ok(())
}

/// At a known evidence deadline, queue the derived unknown once. Compare in the same
/// transaction so an observer's concurrent heartbeat always supersedes this deadline.
pub fn expire_state(agent_dir: &Path, expected: &Value) -> Result<()> {
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current: Option<Vec<u8>> = tx
        .query_row(
            "SELECT body FROM snapshots WHERE kind='harness-state'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if current
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .as_ref()
        != Some(expected)
    {
        return Ok(());
    }
    let expired: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='expired-state'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let identity = serde_json::to_string(expected)?;
    if expired.as_deref() != Some(&identity) {
        append_event(&tx, "harness-state-expired", expected)?;
        tx.execute(
            "INSERT INTO metadata VALUES ('expired-state',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&identity],
        )?;
    }
    tx.commit()?;
    signal_wake(agent_dir);
    Ok(())
}

pub fn remove_snapshot(agent_dir: &Path, kind: &str) -> Result<()> {
    open(agent_dir)?.execute("DELETE FROM snapshots WHERE kind=?1", [kind])?;
    Ok(())
}

/// The existing record readers remain useful inside a producer (ownership/coalescing), but
/// fresh st seats read their local snapshot rather than a polled transport file.
pub fn read_record(path: &Path) -> io::Result<Vec<u8>> {
    let dir = path.parent().unwrap_or(Path::new("."));
    if enabled(dir) {
        let kind = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        return read_snapshot(dir, kind)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound));
    }
    std::fs::read(path)
}

pub fn pending(agent_dir: &Path, limit: usize) -> Result<Vec<Event>> {
    let connection = open(agent_dir)?;
    let mut stmt = connection.prepare(
        "SELECT sequence,runtime_incarnation,queued_at_ms,kind,body FROM events ORDER BY sequence LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as u64], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (sequence, runtime_incarnation, queued_at_ms, kind, body) = row?;
        Ok(Event {
            sequence,
            runtime_incarnation,
            queued_at_ms,
            kind,
            payload: serde_json::from_str(&body)?,
        })
    })
    .collect()
}

/// Freeze graph-derived fields before the first HTTP attempt. A lost acknowledgement cannot
/// recompute compaction attribution against a later graph and turn an exact replay into a conflict.
pub fn prepare_publication(
    agent_dir: &Path,
    sequence: u64,
    slot: &str,
    claim: &Value,
) -> Result<Value> {
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE sequence=?1)",
        [sequence],
        |r| r.get(0),
    )?;
    anyhow::ensure!(pending, "event has already been acknowledged");
    tx.execute(
        "INSERT OR IGNORE INTO prepared VALUES (?1,?2,?3)",
        params![sequence, slot, serde_json::to_string(claim)?],
    )?;
    // Rebuild rejected todo claims from their retained event without keeping the malformed
    // pre-normalization slot as a retry target. Preparation and retirement commit together.
    if slot == "harness.todo.observed:normalized" {
        tx.execute(
            "DELETE FROM prepared WHERE sequence=?1 AND slot='harness.todo.observed:'",
            [sequence],
        )?;
    }
    let body: String = tx.query_row(
        "SELECT body FROM prepared WHERE sequence=?1 AND slot=?2",
        params![sequence, slot],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(serde_json::from_str(&body)?)
}

pub fn acknowledge(agent_dir: &Path, sequence: u64) -> Result<()> {
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let removed: u64 = tx.query_row(
        "SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM events WHERE sequence<=?1",
        [sequence],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM prepared WHERE sequence<=?1", [sequence])?;
    tx.execute("DELETE FROM events WHERE sequence<=?1", [sequence])?;
    tx.execute(
        "UPDATE metadata SET value=CAST(value AS INTEGER)-?1 WHERE key='pending-bytes'",
        [removed],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn database_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(DATABASE)
}

// Timeline producer reads only the prior entry and latest status, not its retained history.
// Each new operation is appended independently and queued in the same durable transaction.
pub(crate) fn timeline_for_write(
    agent_dir: &Path,
    driver: &str,
    incarnation: &str,
    source: &str,
) -> Result<Record> {
    let connection = open(agent_dir)?;
    let next_sequence = connection
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [format!("timeline-next:{incarnation}")],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let mut stmt = connection.prepare("SELECT body FROM timeline WHERE incarnation=?1 AND
        (id=(SELECT MAX(id) FROM timeline WHERE incarnation=?1) OR id IN
            (SELECT MAX(id) FROM timeline WHERE incarnation=?1 AND source IN (?2,?3,?4) GROUP BY source)) ORDER BY id")?;
    let operations = stmt
        .query_map(
            params![
                incarnation,
                source,
                format!("{source}:redaction"),
                format!("{source}:truncation")
            ],
            |r| r.get::<_, String>(0),
        )?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<Vec<Operation>>>()?;
    Ok(Record {
        schema: "st.harness-timeline.v1".into(),
        driver: driver.into(),
        incarnation_id: incarnation.into(),
        next_sequence,
        operations,
    })
}

pub(crate) fn write_timeline(
    agent_dir: &Path,
    record: &Record,
    new_operations: &[Operation],
) -> Result<()> {
    let mut connection = open(agent_dir)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        current_token(&tx)?.as_deref() == Some(&record.incarnation_id),
        "harness timeline owner was superseded"
    );
    tx.execute(
        "INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![
            format!("timeline-next:{}", record.incarnation_id),
            record.next_sequence.to_string()
        ],
    )?;
    for operation in new_operations {
        tx.execute(
            "INSERT INTO timeline(incarnation,source,body) VALUES (?1,?2,?3)",
            params![
                record.incarnation_id,
                operation.source_id.as_deref().unwrap_or(""),
                serde_json::to_string(operation)?
            ],
        )?;
        append_event(&tx, "harness-timeline", &serde_json::to_value(operation)?)?;
    }
    tx.execute("DELETE FROM timeline WHERE id NOT IN (SELECT id FROM timeline ORDER BY id DESC LIMIT 4096)", [])?;
    tx.execute("DELETE FROM timeline WHERE id IN (
        SELECT id FROM (SELECT id, SUM(length(CAST(body AS BLOB))) OVER (ORDER BY id DESC) AS retained FROM timeline)
        WHERE retained > 2097152)", [])?;
    tx.commit()?;
    signal_wake(agent_dir);
    Ok(())
}

pub(crate) fn read_timeline(agent_dir: &Path) -> Result<Option<Record>> {
    let connection = open(agent_dir)?;
    let Some(incarnation) = current_token(&connection)? else {
        return Ok(None);
    };
    let mut stmt =
        connection.prepare("SELECT body FROM timeline WHERE incarnation=?1 ORDER BY id")?;
    let operations = stmt
        .query_map([&incarnation], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<Vec<Operation>>>()?;
    let Some(first) = operations.first() else {
        return Ok(None);
    };
    let driver = first.driver.clone();
    let next_sequence = operations
        .iter()
        .map(|op| op.sequence)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    Ok(Some(Record {
        schema: "st.harness-timeline.v1".into(),
        driver,
        incarnation_id: incarnation,
        next_sequence,
        operations,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer, claim};
    #[test]
    fn committed_events_survive_reopen_and_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime-a").unwrap();
        let seq = claim(root.path(), "agent/example", "claude", "provider-a").unwrap();
        let mut writer = Writer::new(
            root.path(),
            "agent/example",
            "claude",
            Some("fixture".into()),
        )
        .with_ownership("provider-a", seq);
        writer
            .observe(Observation::new(
                Activity::Active,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        assert!(!root.path().join("harness-state").exists());
        let before = pending(root.path(), 100).unwrap();
        assert_eq!(before.len(), 2);
        enable(root.path(), "runtime-a").unwrap();
        assert_eq!(
            pending(root.path(), 100).unwrap()[1].sequence,
            before[1].sequence
        );
        acknowledge(root.path(), before[0].sequence).unwrap();
        assert_eq!(pending(root.path(), 100).unwrap().len(), 1);
        acknowledge(root.path(), before[1].sequence).unwrap();
        assert!(pending(root.path(), 100).unwrap().is_empty());
        writer
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        assert!(pending(root.path(), 100).unwrap()[0].sequence > before[1].sequence);
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::harness_context::{Harness, RateLimits, Reading};
    use crate::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer, claim};
    use crate::harness_timeline::{EntryType, Role};
    use serde_json::json;

    fn active() -> Observation {
        Observation::new(Activity::Active, BlockedOn::None, InputBuffer::Unknown)
    }
    fn reading() -> Reading {
        Reading {
            used_tokens: Some(20),
            window_tokens: Some(100),
            used_percent: Some(20.0),
            model: Some("fixture-model".into()),
            cost_usd: Some(0.1),
            session_total_tokens: Some(30),
            rate_limits: RateLimits::default(),
            account: Some("fixture-account".into()),
            plan: None,
        }
    }
    #[test]
    fn every_harness_commits_operations_and_context_without_record_files() {
        for (driver, harness) in [
            ("claude", Harness::Claude),
            ("codex", Harness::Codex),
            ("pi", Harness::Pi),
            ("omp", Harness::Omp),
            ("opencode", Harness::OpenCode),
        ] {
            let root = tempfile::tempdir().unwrap();
            enable(root.path(), "runtime-current").unwrap();
            let seq = claim(root.path(), "example/seat", driver, "provider-current").unwrap();
            Writer::new(
                root.path(),
                "example/seat",
                driver,
                Some("fixture-pty".into()),
            )
            .with_ownership("provider-current", seq)
            .observe(active())
            .unwrap();
            crate::harness_context::Writer::new_paths(root.path(), "example/seat", harness)
                .unwrap()
                .with_session("provider-current")
                .observe(reading())
                .unwrap();
            let mut timeline =
                crate::harness_timeline::Writer::new(root.path(), driver, "provider-current");
            for (text, final_entry) in [("one", false), ("two", false), ("done", true)] {
                timeline
                    .append(
                        "source-a",
                        Role::Assistant,
                        EntryType::Content,
                        json!({"text":text}),
                        final_entry,
                    )
                    .unwrap();
            }
            timeline
                .append(
                    "source-a",
                    Role::Assistant,
                    EntryType::Content,
                    json!({"text":"ignored after final"}),
                    true,
                )
                .unwrap();
            let events = pending(root.path(), 100).unwrap();
            let operations: Vec<_> = events
                .iter()
                .filter(|event| event.kind == "harness-timeline")
                .collect();
            assert_eq!(operations.len(), 3, "{driver}");
            assert_eq!(
                operations
                    .iter()
                    .map(|event| event.payload["operation"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["append", "replace", "finalize"]
            );
            assert!(
                operations
                    .iter()
                    .all(|event| event.payload["sequence"] == 1)
            );
            assert_eq!(operations[2].payload["revision"], 3);
            assert!(
                events
                    .iter()
                    .all(|event| event.runtime_incarnation == "runtime-current")
            );
            for name in ["harness-state", "harness-context", "harness-timeline"] {
                assert!(!root.path().join(name).exists(), "{driver}/{name}");
            }
            let bytes_before = open(root.path())
                .unwrap()
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='pending-bytes'",
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .unwrap();
            assert!(bytes_before > 0);
            acknowledge(root.path(), events.last().unwrap().sequence).unwrap();
            assert!(pending(root.path(), 100).unwrap().is_empty());
            assert!(
                crate::harness_context::read(&crate::harness_context::harness_context_path(
                    root.path()
                ))
                .is_some()
            );
            assert_eq!(
                open(root.path())
                    .unwrap()
                    .query_row(
                        "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='pending-bytes'",
                        [],
                        |r| r.get::<_, u64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }
    #[test]
    fn superseded_producers_cannot_commit_state_context_or_timeline() {
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime").unwrap();
        let old_seq = claim(root.path(), "example/seat", "claude", "old").unwrap();
        let mut old = Writer::new(root.path(), "example/seat", "claude", Some("pty".into()))
            .with_ownership("old", old_seq);
        let new_seq = claim(root.path(), "example/seat", "claude", "new").unwrap();
        assert!(new_seq > old_seq);
        let before = pending(root.path(), 100).unwrap().len();
        old.observe(active()).unwrap();
        let mut context =
            crate::harness_context::Writer::new_paths(root.path(), "example/seat", Harness::Claude)
                .unwrap()
                .with_session("old");
        assert!(context.observe(reading()).is_err());
        let mut timeline = crate::harness_timeline::Writer::new(root.path(), "claude", "old");
        assert!(
            timeline
                .append(
                    "late",
                    Role::User,
                    EntryType::Content,
                    json!({"text":"late"}),
                    true
                )
                .is_err()
        );
        assert_eq!(pending(root.path(), 100).unwrap().len(), before);
        assert_eq!(
            current_token(&open(root.path()).unwrap())
                .unwrap()
                .as_deref(),
            Some("new")
        );
    }
    #[test]
    fn full_spool_rolls_back_snapshot_and_timeline_changes() {
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime").unwrap();
        let seq = claim(root.path(), "example/seat", "claude", "provider").unwrap();
        let before = read_snapshot(root.path(), "harness-state").unwrap();
        open(root.path())
            .unwrap()
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='pending-bytes'",
                [MAX_PENDING_BYTES],
            )
            .unwrap();
        let mut writer = Writer::new(root.path(), "example/seat", "claude", Some("pty".into()))
            .with_ownership("provider", seq);
        assert!(writer.observe(active()).is_err());
        assert_eq!(read_snapshot(root.path(), "harness-state").unwrap(), before);
        let mut timeline = crate::harness_timeline::Writer::new(root.path(), "claude", "provider");
        assert!(
            timeline
                .append(
                    "source",
                    Role::User,
                    EntryType::Content,
                    json!({"text":"hello"}),
                    true
                )
                .is_err()
        );
        assert!(read_timeline(root.path()).unwrap().is_none());
        assert_eq!(pending(root.path(), 100).unwrap().len(), 1);
    }
    #[test]
    fn deadlines_are_once_only_and_cannot_expire_concurrent_evidence() {
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime").unwrap();
        let seq = claim(root.path(), "example/seat", "claude", "provider").unwrap();
        let state: Value = serde_json::from_slice(
            &read_snapshot(root.path(), "harness-state")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        expire_state(root.path(), &state).unwrap();
        expire_state(root.path(), &state).unwrap();
        assert_eq!(pending(root.path(), 100).unwrap().len(), 2);
        let mut writer = Writer::new(root.path(), "example/seat", "claude", Some("pty".into()))
            .with_ownership("provider", seq);
        writer.observe(active()).unwrap();
        expire_state(root.path(), &state).unwrap();
        assert_eq!(pending(root.path(), 100).unwrap().len(), 3);
    }
    #[test]
    fn replacement_pipe_receives_commits_without_database_read_wakeups() {
        use std::io::Read as _;
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime").unwrap();
        let mut old = bind_wake_pipe(root.path()).unwrap();
        let mut current = bind_wake_pipe(root.path()).unwrap();
        claim(root.path(), "example/seat", "claude", "provider").unwrap();
        let mut byte = [0];
        assert_eq!(current.read(&mut byte).unwrap(), 1);
        assert_eq!(
            old.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let events = pending(root.path(), 100).unwrap();
        read_snapshot(root.path(), "harness-state").unwrap();
        let sequence = events[0].sequence;
        let original = json!({"compaction": "original"});
        assert_eq!(
            prepare_publication(root.path(), sequence, "usage", &original).unwrap(),
            original
        );
        assert_eq!(
            prepare_publication(
                root.path(),
                sequence,
                "usage",
                &json!({"compaction":"changed"})
            )
            .unwrap(),
            original
        );
        acknowledge(root.path(), sequence).unwrap();
        assert!(prepare_publication(root.path(), sequence, "usage", &original).is_err());
        assert_eq!(
            current.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn account_capture_child() {
        let Some(directory) = std::env::var_os("ST_ACCOUNT_TEST_DIRECTORY") else {
            return;
        };
        let runtime = std::env::var("ST_ACCOUNT_TEST_RUNTIME").unwrap();
        let directory = Path::new(&directory);
        enable(directory, &runtime).unwrap();
        claim(directory, "example/seat", "claude", &runtime).unwrap();
    }

    #[test]
    fn account_switch_keeps_the_source_account_on_queued_events() {
        let root = tempfile::tempdir().unwrap();
        for (runtime, account) in [("runtime-one", "ada/one"), ("runtime-two", "ada/two")] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "harness_events::protocol_tests::account_capture_child"])
                .env("ST_ACCOUNT_TEST_DIRECTORY", root.path())
                .env("ST_ACCOUNT_TEST_RUNTIME", runtime)
                .env("ST3_ACCOUNT", account)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let events = pending(root.path(), 100).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].runtime_incarnation, "runtime-one");
        assert_eq!(events[0].payload["account_ref"], "ada/one");
        assert_eq!(events[1].runtime_incarnation, "runtime-two");
        assert_eq!(events[1].payload["account_ref"], "ada/two");
    }

    #[test]
    fn new_runtime_does_not_relabel_unacknowledged_history_or_accept_unknown_formats() {
        let root = tempfile::tempdir().unwrap();
        enable(root.path(), "runtime-old").unwrap();
        let old_seq = claim(root.path(), "example/seat", "claude", "old").unwrap();
        enable(root.path(), "runtime-new").unwrap();
        Writer::new(root.path(), "example/seat", "claude", Some("pty".into()))
            .with_ownership("old", old_seq)
            .observe(active())
            .unwrap();
        claim(root.path(), "example/seat", "claude", "new").unwrap();
        let events = pending(root.path(), 100).unwrap();
        assert_eq!(events[0].runtime_incarnation, "runtime-old");
        assert_eq!(events[1].runtime_incarnation, "runtime-old");
        assert_eq!(events[2].runtime_incarnation, "runtime-new");
        open(root.path())
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert!(pending(root.path(), 100).is_err());
        assert!(enable(root.path(), "runtime-newer").is_err());
        assert!(!root.path().join("harness-state").exists());
    }
}
