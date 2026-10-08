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
pub use source_progress::{
    SourceCommit, SourceIdentity, SourceInvalidation, SourceProgress, SourceToken, SourceWake,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
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

pub(super) fn frontiers(connection: &Connection) -> Result<CommitFrontiers> {
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
    /// Source-owner wake only; does not certify a prepared output, authorization or readiness.
    Source(SourceWake),
}

/// One bridge per Store; graph-watch owns subscriber fanout outside the writer. The callback
/// performs one fixed metadata query per committed batch/returned writer, and never scans keys.
/// A returned lent writer can be a rollback/no-op; unchanged metadata emits no notice. Lagged
/// notices require a fresh snapshot and floor check. No notice proves authorization or output.
pub struct Publisher {
    notices: broadcast::Sender<Notice>,
    _observer: CommitObserver,
    source_scope: Option<Arc<source_progress::Scope>>,
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
            source_scope: None,
        })
    }

    /// Explicitly observe at most sixteen already-installed named sources on this same bridge.
    /// Setup is read-only and must follow source/capture schema installation. Invalid configured
    /// identities or unsupported metadata schema/storage reject setup; a missing or mismatched
    /// persisted source initializes its own invalidation without rebinding its identity or
    /// suppressing valid peers. Common metadata/schema/storage failure affects the whole scope.
    /// Initial state is retained, not broadcast: subscribers must perform their first independent
    /// coverage check after subscribing, even when no later commit produces a wake.
    /// Source owners prove complete native capture and lifetime coverage; tuples only wake
    /// consumers. A schema-cookie change revalidates metadata before refreshing the cookie.
    /// Ordinary captures add two metadata statements; schema revalidation adds bounded result
    /// sets, but their actual SQLite work still needs qualification by the adopting owner.
    pub fn attach_with_sources(
        store: &Store,
        capacity: usize,
        sources: &[SourceIdentity],
    ) -> Result<Self> {
        ensure!(
            (1..=256).contains(&capacity),
            "notice capacity outside 1..=256"
        );
        let (initial, scope, progress, highwater) = store.read_snapshot(|_| {
            let connection = store.readers.get();
            let initial = frontiers(&connection)?;
            let scope = source_progress::Scope::attach(&connection, &initial.database_id, sources)?;
            // Structural/storage failures reject setup; each logical identity mismatch is
            // retained as an individual invalidation without suppressing its valid peers.
            let captured = scope.capture(&connection, &initial.database_id)?;
            let mut highwater = BTreeMap::new();
            let progress = captured.into_wakes(&mut highwater);
            Ok((initial, Arc::new(scope), progress, highwater))
        })?;
        let last = Arc::new(Mutex::new((
            Notice::Committed(initial),
            progress,
            highwater,
        )));
        let (notices, _) = broadcast::channel(capacity);
        let send = notices.clone();
        let capture_scope = scope.clone();
        let observer = store.connection.observe_commits(move |connection| {
            let captured = if connection.is_autocommit() {
                frontiers(connection)
            } else {
                Err(anyhow::anyhow!(
                    "writer returned with an unfinished transaction"
                ))
            };
            let (notice, captured) = match captured {
                Ok(frontiers) => {
                    let progress = capture_scope.capture(connection, &frontiers.database_id);
                    (Notice::Committed(frontiers), progress)
                }
                Err(error) => (Notice::Unavailable(format!("{error:#}")), Err(error)),
            };
            // All SQLite work precedes this lock. A returned rollback/no-op emits nothing
            // when its committed metadata is unchanged. Source availability is independent.
            let mut last = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let progress = match captured {
                Ok(captured) => captured.into_wakes(&mut last.2),
                Err(error) => vec![capture_scope.unavailable(&format!("{error:#}"))],
            };
            if last.0 != notice {
                last.0 = notice.clone();
                let _ = send.send(notice);
            }
            if last.1 != progress {
                last.1 = progress.clone();
                for wake in progress {
                    let _ = send.send(Notice::Source(wake));
                }
            }
        });
        Ok(Self {
            notices,
            _observer: observer,
            source_scope: Some(scope),
        })
    }

    /// Call only after the source's successful AfterCommit check and immutable cache swap,
    /// outside the writer. The token names an already-committed source position, not a new
    /// counter. Always broadcasts, including when the raw tuple is unchanged. This wake does
    /// not replace an authoritative read/coverage check or establish retained-root eligibility.
    pub fn publication_completed(&self, token: &SourceToken) -> Result<()> {
        self.source_scope
            .as_ref()
            .context("publisher has no named sources")?
            .validate_token(token)?;
        let _ = self
            .notices
            .send(Notice::Source(SourceWake::Published(token.clone())));
        Ok(())
    }

    /// Wake after the source owner has guarded/fenced a failed post-commit publication.
    /// Does not write a fence, change availability or undo an accepted source commit.
    pub fn publication_invalidated(&self, source: &SourceIdentity, reason: &str) -> Result<()> {
        let scope = self
            .source_scope
            .as_ref()
            .context("publisher has no named sources")?;
        scope.validate_identity(source)?;
        let _ = self
            .notices
            .send(Notice::Source(scope.invalidated(source, reason)));
        Ok(())
    }

    /// Identity captured at explicit named-source attachment. It scopes wake tokens only;
    /// the source owner still checks its native database/lifetime and full publication cut.
    pub fn source_database_id(&self) -> Option<&str> {
        self.source_scope.as_ref().map(|scope| scope.database_id())
    }

    /// Subscribe before an authoritative snapshot/recheck. A commit before that snapshot is
    /// reflected by its durable boundary; one after it remains buffered on this receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }
}

mod source_progress {
    use super::super::install::SourcePosition;
    use anyhow::{Context, Result, ensure};
    use rusqlite::{Connection, params_from_iter};
    use std::collections::BTreeMap;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    const MAX_SOURCES: usize = 16;
    const MAX_NAME_BYTES: usize = 128;
    const MAX_FINGERPRINT_BYTES: usize = 4096;
    const MAX_METADATA_BYTES: usize = 64 * 1024;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SourceIdentity {
        pub name: String,
        pub fingerprint: String,
        pub epoch: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SourceProgress {
        pub identity: SourceIdentity,
        pub revision: u64,
        pub available: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SourceCommit {
        pub database_id: String,
        /// Valid attached identities in source-name order, including available=false.
        /// Missing/replaced/malformed identities instead receive source-specific invalidations;
        /// no revision is invented for them. This subset never certifies the complete scope.
        pub sources: Vec<SourceProgress>,
    }

    /// A wake token, not a publication/authorization/retention certificate. The source owner
    /// supplies the already-committed position captured with its complete native cut.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SourceToken {
        pub database_id: String,
        pub position: SourcePosition,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SourceInvalidation {
        pub database_id: String,
        /// None invalidates all sources explicitly attached to this Publisher.
        pub source: Option<SourceIdentity>,
        pub reason: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum SourceWake {
        Committed(SourceCommit),
        Published(SourceToken),
        Invalidated(SourceInvalidation),
    }

    pub(super) struct Scope {
        database_id: String,
        identities: Vec<SourceIdentity>,
        schema: Mutex<SchemaStamp>,
        schema_invalidated: AtomicBool,
        query: String,
    }

    pub(super) struct Captured {
        commit: SourceCommit,
        invalidations: Vec<SourceInvalidation>,
    }

    impl Captured {
        pub(super) fn into_wakes(self, highwater: &mut BTreeMap<String, u64>) -> Vec<SourceWake> {
            let Self {
                mut commit,
                mut invalidations,
            } = self;
            commit.sources.retain(|source| {
                if highwater
                    .get(&source.identity.name)
                    .is_some_and(|previous| source.revision < *previous)
                {
                    invalidations.push(SourceInvalidation {
                        database_id: commit.database_id.clone(),
                        source: Some(source.identity.clone()),
                        reason: "named-source revision moved backwards".into(),
                    });
                    false
                } else {
                    highwater.insert(source.identity.name.clone(), source.revision);
                    true
                }
            });
            invalidations.sort_by(|a, b| {
                a.source
                    .as_ref()
                    .map(|s| &s.name)
                    .cmp(&b.source.as_ref().map(|s| &s.name))
            });
            let mut wakes = Vec::with_capacity(invalidations.len() + 1);
            if !commit.sources.is_empty() {
                wakes.push(SourceWake::Committed(commit));
            }
            wakes.extend(invalidations.into_iter().map(SourceWake::Invalidated));
            wakes
        }
    }

    impl Scope {
        pub(super) fn attach(
            db: &Connection,
            database_id: &str,
            identities: &[SourceIdentity],
        ) -> Result<Self> {
            ensure!(
                (1..=MAX_SOURCES).contains(&identities.len()),
                "named-source count outside 1..=16"
            );
            ensure!(
                !database_id.is_empty() && database_id.len() <= MAX_NAME_BYTES,
                "invalid publisher database identity"
            );
            let mut identities = identities.to_vec();
            identities.sort_by(|a, b| a.name.cmp(&b.name));
            let mut bytes = database_id.len();
            for (index, identity) in identities.iter().enumerate() {
                ensure!(
                    !identity.name.is_empty()
                        && identity.name.len() <= MAX_NAME_BYTES
                        && !identity.name.contains('\0'),
                    "invalid named-source name"
                );
                ensure!(
                    !identity.fingerprint.is_empty()
                        && identity.fingerprint.len() <= MAX_FINGERPRINT_BYTES
                        && !identity.fingerprint.contains('\0'),
                    "invalid named-source fingerprint"
                );
                ensure!(
                    identity.epoch <= i64::MAX as u64,
                    "invalid named-source epoch"
                );
                ensure!(
                    index == 0 || identities[index - 1].name != identity.name,
                    "duplicate named source"
                );
                bytes += identity.name.len()
                    + identity.fingerprint.len()
                    + 3 * std::mem::size_of::<u64>();
            }
            ensure!(
                bytes <= MAX_METADATA_BYTES,
                "named-source metadata exceeds bound"
            );
            let schema = validate_schema(db)?;
            let placeholders = (1..=identities.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            // Direct-column octet_length avoids copying a malformed oversized fingerprint
            // into Rust. All dynamic SQL consists of generated parameter numbers only.
            let query = format!("SELECT name,
                CASE WHEN typeof(fingerprint)='text' AND octet_length(fingerprint)<={MAX_FINGERPRINT_BYTES} THEN fingerprint END,
                CASE WHEN typeof(epoch)='integer' THEN epoch END,
                CASE WHEN typeof(revision)='integer' THEN revision END,
                CASE WHEN typeof(available)='integer' THEN available END
                FROM main.ivm_install_sources WHERE name COLLATE BINARY IN ({placeholders}) ORDER BY name COLLATE BINARY");
            Ok(Self {
                database_id: database_id.into(),
                identities,
                schema: Mutex::new(schema),
                schema_invalidated: AtomicBool::new(false),
                query,
            })
        }

        pub(super) fn capture(&self, db: &Connection, database_id: &str) -> Result<Captured> {
            ensure!(
                database_id == self.database_id,
                "publisher database identity replaced"
            );
            let cookie: i64 = db.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?;
            ensure!(
                !self.schema_invalidated.load(Ordering::Acquire),
                "named-source schema invalidated; explicit reattachment required"
            );
            let previous = {
                let schema = self
                    .schema
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (cookie != schema.version).then(|| schema.clone())
            };
            if let Some(previous) = previous {
                // No SQL runs while the metadata-cache mutex is held. Unrelated DDL may
                // refresh the cookie only after the actual source shape/index is checked.
                let current = match validate_schema(db) {
                    Ok(current) => current,
                    Err(error) => {
                        self.schema_invalidated.store(true, Ordering::Release);
                        return Err(error);
                    }
                };
                if current.root_page != previous.root_page
                    || current.declaration != previous.declaration
                {
                    self.schema_invalidated.store(true, Ordering::Release);
                    anyhow::bail!(
                        "named-source metadata table changed; explicit reattachment required"
                    );
                }
                ensure!(
                    current.version == cookie,
                    "schema changed during named-source revalidation"
                );
                *self
                    .schema
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = current;
            }
            let mut statement = db.prepare_cached(&self.query)?;
            let rows = statement.query_map(
                params_from_iter(self.identities.iter().map(|i| &i.name)),
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )?;
            let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut sources = Vec::with_capacity(self.identities.len());
            let mut invalidations = Vec::new();
            for expected in &self.identities {
                let progress = (|| -> Result<SourceProgress> {
                    let (_, fingerprint, epoch, revision, available) = rows
                        .iter()
                        .find(|row| row.0 == expected.name)
                        .context("named source missing")?;
                    ensure!(
                        fingerprint.as_deref() == Some(expected.fingerprint.as_str())
                            && *epoch == Some(expected.epoch as i64),
                        "named-source identity missing or replaced"
                    );
                    let revision = (*revision)
                        .filter(|r| *r >= 0)
                        .context("invalid named-source revision")?
                        as u64;
                    let available = (*available)
                        .filter(|a| *a == 0 || *a == 1)
                        .context("invalid named-source availability")?
                        == 1;
                    Ok(SourceProgress {
                        identity: expected.clone(),
                        revision,
                        available,
                    })
                })();
                match progress {
                    Ok(progress) => sources.push(progress),
                    Err(error) => invalidations.push(SourceInvalidation {
                        database_id: self.database_id.clone(),
                        source: Some(expected.clone()),
                        reason: bounded_reason(&format!("{error:#}")),
                    }),
                }
            }
            Ok(Captured {
                commit: SourceCommit {
                    database_id: self.database_id.clone(),
                    sources,
                },
                invalidations,
            })
        }

        pub(super) fn validate_identity(&self, identity: &SourceIdentity) -> Result<()> {
            ensure!(
                self.identities.iter().any(|expected| expected == identity),
                "source not attached to this publisher"
            );
            Ok(())
        }

        pub(super) fn database_id(&self) -> &str {
            &self.database_id
        }

        pub(super) fn validate_token(&self, token: &SourceToken) -> Result<()> {
            ensure!(
                token.database_id == self.database_id && token.position.revision <= i64::MAX as u64,
                "invalid source publication token"
            );
            ensure!(
                self.identities
                    .iter()
                    .any(|identity| identity.name == token.position.source
                        && identity.fingerprint == token.position.fingerprint
                        && identity.epoch == token.position.epoch),
                "source not attached to this publisher"
            );
            Ok(())
        }

        pub(super) fn invalidated(&self, source: &SourceIdentity, reason: &str) -> SourceWake {
            SourceWake::Invalidated(SourceInvalidation {
                database_id: self.database_id.clone(),
                source: Some(source.clone()),
                reason: bounded_reason(reason),
            })
        }

        pub(super) fn unavailable(&self, reason: &str) -> SourceWake {
            SourceWake::Invalidated(SourceInvalidation {
                database_id: self.database_id.clone(),
                source: None,
                reason: bounded_reason(reason),
            })
        }
    }

    // A repeated root page/DDL is not an incarnation token: an identical DROP/recreate may
    // reuse both. Source owners must fence such replacement, file restore and cookie resets
    // with their native lifetime guard. This stamp only permits compatible unrelated DDL.
    #[derive(Clone)]
    struct SchemaStamp {
        version: i64,
        root_page: i64,
        declaration: String,
    }

    fn validate_schema(db: &Connection) -> Result<SchemaStamp> {
        // Refuse oversized/altered declarations before collecting column/index metadata.
        // Direct-column length avoids fetching an unbounded declaration into Rust.
        let (root_page, declaration): (i64, Option<String>) = db.query_row(
            "SELECT rootpage, CASE WHEN typeof(sql)='text' AND octet_length(sql)<=16384 THEN sql END FROM main.sqlite_schema WHERE type='table' AND name='ivm_install_sources'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(root_page > 0, "named-source table root missing");
        let declaration = declaration.context("named-source table declaration unsupported")?;
        let kind: (String, i64, i64) = db.query_row(
        "SELECT type,wr,ncol FROM pragma_table_list WHERE schema='main' AND name='ivm_install_sources'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        ensure!(
            kind == ("table".into(), 0, 7),
            "named-source table schema incompatible"
        );
        let mut statement = db.prepare("SELECT name,type,pk,hidden FROM pragma_table_xinfo('ivm_install_sources','main') LIMIT 8")?;
        let columns = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected = [
            ("name", "TEXT", 1),
            ("fingerprint", "TEXT", 0),
            ("epoch", "INTEGER", 0),
            ("revision", "INTEGER", 0),
            ("available", "INTEGER", 0),
            ("journal_rows", "INTEGER", 0),
            ("journal_bytes", "INTEGER", 0),
        ];
        ensure!(
            columns.len() == expected.len()
                && columns
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.0 == expected.0
                        && actual.1 == expected.1
                        && actual.2 == expected.2
                        && actual.3 == 0),
            "named-source columns incompatible"
        );
        // Check the actual ordered primary-key index and BINARY comparison, not just
        // the column declaration, so the fixed exact-name queries remain point seeks.
        let mut indexes = db.prepare("SELECT name FROM pragma_index_list('ivm_install_sources','main') WHERE origin='pk' LIMIT 2")?;
        let indexes = indexes
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(indexes.len() == 1, "named-source primary index missing");
        let mut keys = db.prepare(
            "SELECT name,coll,desc FROM pragma_index_xinfo(?1,'main') WHERE key=1 LIMIT 2",
        )?;
        let keys = keys
            .query_map([&indexes[0]], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            keys == vec![(Some("name".into()), "BINARY".into(), 0)],
            "named-source primary key incompatible"
        );
        let version = db.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?;
        Ok(SchemaStamp {
            version,
            root_page,
            declaration,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn fixture() -> Result<(Connection, Vec<SourceIdentity>)> {
            let db = Connection::open_in_memory()?;
            db.execute_batch(
                "CREATE TABLE ivm_install_sources (
                name TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL,
                revision INTEGER NOT NULL, available INTEGER NOT NULL,
                journal_rows INTEGER NOT NULL DEFAULT 0, journal_bytes INTEGER NOT NULL DEFAULT 0);
                INSERT INTO ivm_install_sources(name,fingerprint,epoch,revision,available)
                VALUES ('a','a.v1',1,4,1),('b','b.v1',1,9,1);",
            )?;
            let identities = ["a", "b"]
                .into_iter()
                .map(|name| SourceIdentity {
                    name: name.into(),
                    fingerprint: format!("{name}.v1"),
                    epoch: 1,
                })
                .collect();
            Ok((db, identities))
        }

        #[test]
        fn mismatched_source_does_not_suppress_valid_peer_progress() -> Result<()> {
            let (db, identities) = fixture()?;
            db.execute(
                "UPDATE ivm_install_sources SET fingerprint='wrong' WHERE name='a'",
                [],
            )?;
            let scope = Scope::attach(&db, "db", &identities)?;
            let mut highwater = BTreeMap::new();
            let wakes = scope.capture(&db, "db")?.into_wakes(&mut highwater);
            assert!(
                matches!(&wakes[0], SourceWake::Committed(c) if c.sources.len()==1 && c.sources[0].identity.name=="b" && c.sources[0].revision==9)
            );
            assert!(
                matches!(&wakes[1], SourceWake::Invalidated(i) if i.source.as_ref()==Some(&identities[0]))
            );
            assert!(!highwater.contains_key("a"));
            db.execute(
                "UPDATE ivm_install_sources SET revision=10 WHERE name='b'",
                [],
            )?;
            let next = scope.capture(&db, "db")?.into_wakes(&mut highwater);
            assert_ne!(wakes, next);
            assert!(matches!(&next[0], SourceWake::Committed(c) if c.sources[0].revision==10));
            assert!(
                matches!(&next[1], SourceWake::Invalidated(i) if i.source.as_ref()==Some(&identities[0]))
            );
            Ok(())
        }

        #[test]
        fn missing_and_regressing_sources_do_not_invent_or_reset_revisions() -> Result<()> {
            let (db, identities) = fixture()?;
            let scope = Scope::attach(&db, "db", &identities)?;
            let mut highwater = BTreeMap::new();
            scope.capture(&db, "db")?.into_wakes(&mut highwater);
            db.execute_batch(
                "DELETE FROM ivm_install_sources WHERE name='a';
                UPDATE ivm_install_sources SET revision=8 WHERE name='b';",
            )?;
            let wakes = scope.capture(&db, "db")?.into_wakes(&mut highwater);
            assert_eq!(wakes.len(), 2);
            assert!(
                wakes
                    .iter()
                    .all(|w| matches!(w, SourceWake::Invalidated(i) if i.source.is_some()))
            );
            assert_eq!(highwater.get("a"), Some(&4));
            assert_eq!(highwater.get("b"), Some(&9));
            db.execute(
                "UPDATE ivm_install_sources SET revision=10 WHERE name='b'",
                [],
            )?;
            let next = scope.capture(&db, "db")?.into_wakes(&mut highwater);
            assert!(
                matches!(&next[0], SourceWake::Committed(c) if c.sources.len()==1 && c.sources[0].revision==10)
            );
            Ok(())
        }

        #[test]
        fn unrelated_namespace_ddl_refreshes_cookie_but_malformed_schema_latches_refusal()
        -> Result<()> {
            let (db, identities) = fixture()?;
            let scope = Scope::attach(&db, "db", &identities)?;
            let mut highwater = BTreeMap::new();
            let before = scope.capture(&db, "db")?.into_wakes(&mut highwater);
            db.execute_batch("CREATE TABLE staged_namespace(key TEXT PRIMARY KEY, value BLOB);")?;
            assert_eq!(before, scope.capture(&db, "db")?.into_wakes(&mut highwater));
            db.execute_batch("ALTER TABLE ivm_install_sources ADD COLUMN unsupported INTEGER;")?;
            assert!(scope.capture(&db, "db").is_err());
            db.execute_batch("ALTER TABLE ivm_install_sources DROP COLUMN unsupported;")?;
            assert!(scope.capture(&db, "db").is_err());
            Ok(())
        }
    }

    fn bounded_reason(reason: &str) -> String {
        let mut end = reason.len().min(1024);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason[..end].into()
    }
}
