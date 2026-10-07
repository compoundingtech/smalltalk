//! Opt-in committed invalidations, not historical answers or action occurrences.
//!
//! Install explicitly inside a writer transaction. A single [`Publisher`] observes committed
//! writer returns and wakes a transport bridge; the bridge reads indexed, bounded pages in a
//! short read snapshot. No network work, body decoding, or client fanout belongs in the writer
//! callback. Subscribe before capturing/rechecking authoritative state and release the snapshot
//! before awaiting. A lagged receiver requires rechecking the durable cursors, not polling.
//!
//! Event rows name changed keys; their payload is the *current* source cut/status in the reading
//! snapshot. They do not retain the intermediate answer at each sequence. Restore/clone owners
//! must rotate database identity before exposing a restored file; this is not automatic restore
//! certification. Production authority, deadlines and external inputs remain adapter-owned.

use super::{Availability, AvailabilityToken, Readiness, SourceCut, Views, source_cut};
use crate::{Store, sqlite::CommitObserver};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const FORMAT: &str = "smallclaims.ivm.events.v1";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_event_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), format TEXT NOT NULL,
 database_id TEXT NOT NULL, capacity INTEGER NOT NULL CHECK(capacity BETWEEN 1 AND 4096),
 available INTEGER NOT NULL CHECK(available IN (0,1)),
 key_floor INTEGER NOT NULL, status_floor INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ivm_event_keys (
 sequence INTEGER PRIMARY KEY, view TEXT NOT NULL, key TEXT NOT NULL,
 key_generation INTEGER NOT NULL, semantic_generation INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ivm_event_keys_view ON ivm_event_keys(view,sequence);
CREATE TABLE IF NOT EXISTS ivm_event_status (
 sequence INTEGER PRIMARY KEY, view TEXT
);
CREATE INDEX IF NOT EXISTS ivm_event_status_view ON ivm_event_status(view,sequence);
CREATE TRIGGER IF NOT EXISTS ivm_event_key_insert AFTER INSERT ON ivm_keys
BEGIN
 INSERT INTO ivm_event_keys SELECT NEW.changed_sequence,NEW.view,NEW.key,NEW.generation,generation
  FROM ivm_views WHERE name=NEW.view AND length(CAST(NEW.key AS BLOB))<=4096
  AND length(CAST(NEW.view AS BLOB))<=1024;
 UPDATE ivm_event_state SET available=0 WHERE available<>0 AND
  (length(CAST(NEW.key AS BLOB))>4096 OR length(CAST(NEW.view AS BLOB))>1024);
 UPDATE ivm_event_state SET key_floor=MAX(key_floor,NEW.changed_sequence-capacity);
 DELETE FROM ivm_event_keys WHERE sequence<=(SELECT key_floor FROM ivm_event_state);
END;
CREATE TRIGGER IF NOT EXISTS ivm_event_key_update AFTER UPDATE OF changed_sequence ON ivm_keys
WHEN NEW.changed_sequence<>OLD.changed_sequence
BEGIN
 INSERT INTO ivm_event_keys SELECT NEW.changed_sequence,NEW.view,NEW.key,NEW.generation,generation
  FROM ivm_views WHERE name=NEW.view AND length(CAST(NEW.key AS BLOB))<=4096
  AND length(CAST(NEW.view AS BLOB))<=1024;
 UPDATE ivm_event_state SET available=0 WHERE available<>0 AND
  (length(CAST(NEW.key AS BLOB))>4096 OR length(CAST(NEW.view AS BLOB))>1024);
 UPDATE ivm_event_state SET key_floor=MAX(key_floor,NEW.changed_sequence-capacity);
 DELETE FROM ivm_event_keys WHERE sequence<=(SELECT key_floor FROM ivm_event_state);
END;
CREATE TRIGGER IF NOT EXISTS ivm_event_status_insert AFTER INSERT ON ivm_view_status
BEGIN
 INSERT INTO ivm_event_status SELECT NEW.sequence,NEW.view WHERE length(CAST(NEW.view AS BLOB))<=1024;
 UPDATE ivm_event_state SET available=0 WHERE available<>0 AND length(CAST(NEW.view AS BLOB))>1024;
 UPDATE ivm_event_state SET status_floor=MAX(status_floor,NEW.sequence-capacity);
 DELETE FROM ivm_event_status WHERE sequence<=(SELECT status_floor FROM ivm_event_state);
END;
CREATE TRIGGER IF NOT EXISTS ivm_event_status_update AFTER UPDATE OF sequence ON ivm_view_status
WHEN NEW.sequence<>OLD.sequence
BEGIN
 INSERT INTO ivm_event_status SELECT NEW.sequence,NEW.view WHERE length(CAST(NEW.view AS BLOB))<=1024;
 UPDATE ivm_event_state SET available=0 WHERE available<>0 AND length(CAST(NEW.view AS BLOB))>1024;
 UPDATE ivm_event_state SET status_floor=MAX(status_floor,NEW.sequence-capacity);
 DELETE FROM ivm_event_status WHERE sequence<=(SELECT status_floor FROM ivm_event_state);
END;
CREATE TRIGGER IF NOT EXISTS ivm_event_source_update
AFTER UPDATE OF source_sequence ON ivm_status_frontier
WHEN NEW.source_sequence<>OLD.source_sequence
BEGIN
 INSERT INTO ivm_event_status VALUES(NEW.source_sequence,NULL);
 UPDATE ivm_event_state SET status_floor=MAX(status_floor,NEW.source_sequence-capacity);
 DELETE FROM ivm_event_status WHERE sequence<=(SELECT status_floor FROM ivm_event_state);
END;
"#;

/// Stable across semantic updates; never compare snapshot generation as reconnect identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderIdentity {
    pub database_id: String,
    pub view: String,
    pub fingerprint: String,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Keys,
    Availability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeCursor {
    pub identity: ProviderIdentity,
    pub stream: Stream,
    /// Invalidation sequence, never a claim store_index or a canonical winner rank.
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotVersion {
    pub semantic_generation: Option<u64>,
    pub availability: AvailabilityToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub identity: ProviderIdentity,
    pub snapshot: SnapshotVersion,
    pub source_cut: SourceCut,
    pub availability: Availability,
    pub keys: ChangeCursor,
    pub status: ChangeCursor,
    /// Inclusive expired positions. Cursors strictly below the floor have a gap.
    pub key_floor: u64,
    pub status_floor: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewChanged {
    pub view: String,
    pub key: String,
    pub key_generation: u64,
    pub sequence: u64,
    /// Version at invalidation; consumers read current rows using the page's snapshot.
    pub recorded_semantic_generation: u64,
    pub source_cut: SourceCut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewAvailabilityChanged {
    /// None invalidates shared source availability for this provider.
    pub view: Option<String>,
    pub sequence: u64,
    pub source_cut: SourceCut,
    pub current: Availability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gap {
    ProviderReplaced,
    WrongStream,
    Expired { floor: u64 },
    AheadOfSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page<T> {
    Changes {
        items: Vec<T>,
        next: ChangeCursor,
        more: bool,
        /// Current output/status and new claim frontier captured with the event rows.
        boundary: Boundary,
    },
    Resync {
        reason: Gap,
        boundary: Boundary,
    },
    SnapshotChanged {
        boundary: Boundary,
    },
}

/// Initialize without scanning previous keys. Existing readers start from a captured boundary;
/// pre-install cursors below that boundary cannot be treated as complete reconnect history.
pub fn install(tx: &Transaction<'_>, capacity: usize) -> Result<()> {
    ensure!(
        (1..=4096).contains(&capacity),
        "event retention outside 1..=4096"
    );
    source_cut(tx)?.context("IVM source unavailable")?;
    ensure!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_views WHERE length(CAST(name AS BLOB))>1024)",
            [],
            |row| row.get::<_, bool>(0)
        )?,
        "registered view name exceeds event byte bound"
    );
    tx.execute_batch(SCHEMA)?;
    tx.execute(
        "INSERT OR IGNORE INTO ivm_event_state SELECT 1,?1,?2,?3,1,k.sequence,s.sequence
         FROM ivm_key_frontier k,ivm_status_frontier s WHERE k.singleton=1 AND s.singleton=1",
        params![FORMAT, uuid::Uuid::now_v7().to_string(), capacity],
    )?;
    let (format, stored_capacity): (String, usize) = tx.query_row(
        "SELECT format,capacity FROM ivm_event_state WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        format == FORMAT && stored_capacity == capacity,
        "event installation incompatible"
    );
    Ok(())
}

/// Explicit restore/clone fence. The owner must certify its source separately. This rotates
/// only cursor identity; it does not repair output, replay claims or authorize stale reads.
pub fn rotate_identity(tx: &Transaction<'_>) -> Result<()> {
    let changed = tx.execute(
        "UPDATE ivm_event_state SET database_id=?1 WHERE singleton=1 AND format=?2",
        params![uuid::Uuid::now_v7().to_string(), FORMAT],
    )?;
    ensure!(changed == 1, "event source unavailable");
    Ok(())
}

/// Call inside the same Store read_snapshot as the predicate or output being exposed.
pub fn capture(connection: &Connection, views: &Views, view: &str) -> Result<Boundary> {
    let frontiers = frontiers(connection)?;
    let availability = views.availability(connection, view, frontiers.source_cut.epoch)?;
    let generation = match &availability.readiness {
        Readiness::Ready(token) => Some(token.generation),
        _ => connection
            .query_row(
                "SELECT generation FROM ivm_views WHERE name=?1",
                [view],
                |r| r.get(0),
            )
            .optional()?,
    };
    let identity = ProviderIdentity {
        database_id: frontiers.database_id,
        view: view.into(),
        fingerprint: format!("{FORMAT}:{}", availability.token.fingerprint),
        epoch: availability.token.epoch,
    };
    Ok(Boundary {
        keys: ChangeCursor {
            identity: identity.clone(),
            stream: Stream::Keys,
            sequence: frontiers.keys,
        },
        status: ChangeCursor {
            identity: identity.clone(),
            stream: Stream::Availability,
            sequence: frontiers.status,
        },
        snapshot: SnapshotVersion {
            semantic_generation: generation,
            availability: availability.token.clone(),
        },
        identity,
        source_cut: frontiers.source_cut,
        availability,
        key_floor: frontiers.key_floor,
        status_floor: frontiers.status_floor,
    })
}

fn check<T>(
    cursor: &ChangeCursor,
    stream: Stream,
    expected: Option<&SnapshotVersion>,
    boundary: &Boundary,
) -> Option<Page<T>> {
    let (floor, through) = match stream {
        Stream::Keys => (boundary.key_floor, boundary.keys.sequence),
        Stream::Availability => (boundary.status_floor, boundary.status.sequence),
    };
    let reason = if cursor.identity != boundary.identity {
        Some(Gap::ProviderReplaced)
    } else if cursor.stream != stream {
        Some(Gap::WrongStream)
    } else if cursor.sequence < floor {
        Some(Gap::Expired { floor })
    } else if cursor.sequence > through {
        Some(Gap::AheadOfSource)
    } else {
        None
    };
    if let Some(reason) = reason {
        return Some(Page::Resync {
            reason,
            boundary: boundary.clone(),
        });
    }
    if expected.is_some_and(|snapshot| snapshot != &boundary.snapshot) {
        return Some(Page::SnapshotChanged {
            boundary: boundary.clone(),
        });
    }
    None
}

pub fn keys(
    connection: &Connection,
    views: &Views,
    cursor: &ChangeCursor,
    limit: usize,
    expected: Option<&SnapshotVersion>,
) -> Result<Page<ViewChanged>> {
    ensure!((1..=1024).contains(&limit), "event page outside 1..=1024");
    let boundary = capture(connection, views, &cursor.identity.view)?;
    if let Some(result) = check(cursor, Stream::Keys, expected, &boundary) {
        return Ok(result);
    }
    let mut statement = connection.prepare_cached(
        "SELECT view,key,key_generation,sequence,semantic_generation FROM ivm_event_keys
         WHERE view=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
    )?;
    let mut items = statement
        .query_map(
            params![
                cursor.identity.view,
                cursor.sequence,
                boundary.keys.sequence,
                limit + 1
            ],
            |r| {
                Ok(ViewChanged {
                    view: r.get(0)?,
                    key: r.get(1)?,
                    key_generation: r.get(2)?,
                    sequence: r.get(3)?,
                    recorded_semantic_generation: r.get(4)?,
                    source_cut: boundary.source_cut,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = items.len() > limit;
    items.truncate(limit);
    let mut next = boundary.keys.clone();
    if more {
        next.sequence = items.last().context("missing event page row")?.sequence;
    }
    Ok(Page::Changes {
        items,
        next,
        more,
        boundary,
    })
}

pub fn availability(
    connection: &Connection,
    views: &Views,
    cursor: &ChangeCursor,
    limit: usize,
    expected: Option<&SnapshotVersion>,
) -> Result<Page<ViewAvailabilityChanged>> {
    ensure!((1..=1024).contains(&limit), "event page outside 1..=1024");
    let boundary = capture(connection, views, &cursor.identity.view)?;
    if let Some(result) = check(cursor, Stream::Availability, expected, &boundary) {
        return Ok(result);
    }
    let mut statement = connection.prepare_cached(
        "SELECT sequence,view FROM ivm_event_status WHERE (view=?1 OR view IS NULL)
         AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
    )?;
    let mut items = statement
        .query_map(
            params![
                cursor.identity.view,
                cursor.sequence,
                boundary.status.sequence,
                limit + 1
            ],
            |r| {
                Ok(ViewAvailabilityChanged {
                    sequence: r.get(0)?,
                    view: r.get(1)?,
                    source_cut: boundary.source_cut,
                    current: boundary.availability.clone(),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = items.len() > limit;
    items.truncate(limit);
    let mut next = boundary.status.clone();
    if more {
        next.sequence = items.last().context("missing event page row")?.sequence;
    }
    Ok(Page::Changes {
        items,
        next,
        more,
        boundary,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFrontiers {
    pub database_id: String,
    pub source_cut: SourceCut,
    pub keys: u64,
    pub status: u64,
    pub key_floor: u64,
    pub status_floor: u64,
}

fn frontiers(connection: &Connection) -> Result<CommitFrontiers> {
    let (format, value): (String, CommitFrontiers) = connection.query_row(
        "SELECT e.format,e.database_id,c.epoch,c.admitted,c.projected,c.local_generation,
                k.sequence,s.sequence,e.key_floor,e.status_floor
         FROM ivm_event_state e,ivm_source c,ivm_key_frontier k,ivm_status_frontier s
         WHERE e.singleton=1 AND e.available=1 AND c.singleton=1 AND k.singleton=1 AND s.singleton=1", [], |r| Ok((r.get(0)?, CommitFrontiers {
            database_id: r.get(1)?, source_cut: SourceCut { epoch:r.get(2)?,admitted:r.get(3)?,projected:r.get(4)?,local_generation:r.get(5)? },
            keys:r.get(6)?,status:r.get(7)?,key_floor:r.get(8)?,status_floor:r.get(9)?,
        })),
    ).context("event source missing or unavailable")?;
    ensure!(
        format == FORMAT && value.key_floor <= value.keys && value.status_floor <= value.status,
        "event source incompatible or replaced"
    );
    value.source_cut.validate()?;
    Ok(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Committed(CommitFrontiers),
    /// Recheck source/readiness; never convert a failed capture into a successful event.
    Unavailable(String),
}

/// One bridge per Store; graph-watch owns subscriber fanout outside the writer. The callback
/// performs one fixed metadata query per committed batch/returned writer, and never scans keys.
/// A returned lent writer can be a rollback/no-op; unchanged metadata emits no notice. Lagged
/// notices require a fresh snapshot and floor check. No notice proves authorization or output.
pub struct Publisher {
    notices: broadcast::Sender<Notice>,
    _observer: CommitObserver,
}

impl Publisher {
    pub fn attach(store: &Store, capacity: usize) -> Result<Self> {
        ensure!(
            (1..=256).contains(&capacity),
            "notice capacity outside 1..=256"
        );
        let initial = store.read_snapshot(|_| frontiers(&store.readers.get()))?;
        let last = Arc::new(Mutex::new(Notice::Committed(initial)));
        let (notices, _) = broadcast::channel(capacity);
        let send = notices.clone();
        let observer = store.connection.observe_commits(move |connection| {
            let notice = if !connection.is_autocommit() {
                Notice::Unavailable("writer returned with an unfinished transaction".into())
            } else {
                match frontiers(connection) {
                    Ok(frontiers) => Notice::Committed(frontiers),
                    Err(error) => Notice::Unavailable(format!("{error:#}")),
                }
            };
            let mut last = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *last != notice {
                *last = notice.clone();
                // No receivers is normal. Network work and bounded page draining happen
                // after the commit, on the transport task, not inside this callback.
                let _ = send.send(notice);
            }
        });
        Ok(Self {
            notices,
            _observer: observer,
        })
    }

    /// Subscribe before an authoritative snapshot/recheck. A commit before that snapshot is
    /// reflected by its durable boundary; one after it remains buffered on this receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }
}
