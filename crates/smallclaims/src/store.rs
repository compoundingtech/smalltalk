//! The claim store: the claim log and its batches, replica envelopes and their admission,
//! replication between members, documents and blobs, checkpoints and heals.
//!
//! A runtime built on the graph, such as smalltalk, keeps its projections in the same database
//! and plugs them in through [`Runtime`]: the store calls it to create its tables, to validate a
//! replicated claim's kind and fields, and to project admitted claims.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::claim::{
    ClaimInput, ClaimRecord, DocumentVersion, ReplicaBatch, ReplicaEnvelope, ReplicaEnvelopeId,
    ReplicaEnvelopePayload,
};
use crate::error::{Error as St3Error, internal};
use crate::hash::{
    batch_header_hash, canonical_hash, canonical_json_text, canonical_json_value, claim_hash,
    claim_id_is_content_hash, replica_envelope_hash, replica_record_ref,
    verify_replica_batch_header,
};
use crate::replication::*;
use crate::sqlite::{
    PINNED_READER, PinnedRead, ReadPool, SQLITE_COMMIT_NANOS, SQLITE_COMMITS, SQLITE_NANOS,
    STATEMENT_CACHE_CAPACITY, WriterConnection,
};

pub mod canonical;
pub mod checkpoint;
pub mod checkpoint_agreement;
pub mod checkpoint_trim;
pub mod document_index;
pub mod heal;
pub mod principals;
pub mod projection_digest;
pub mod runtime;

pub use runtime::Runtime;

pub use canonical::{CANONICAL_ORDER, CANONICAL_ORDER_DESC, canonical_sql};
pub use checkpoint::{
    CHECKPOINT_MANIFEST_PAGE_LIMIT, CheckpointManifest, CheckpointManifestCursor,
    CheckpointManifestPage, CheckpointManifestRequest, CheckpointPlanRequest, CheckpointPlanView,
    CheckpointProof, ClaimTombstone, DropCount, DropPlan, EnvelopeKey, EnvelopeTombstone,
    SealedSet, checkpoint_cut, checkpoint_name, drop_digest, newest_due_cut,
    verify_checkpoint_manifest,
};
pub use checkpoint_agreement::write_time;
pub use checkpoint_agreement::{
    CHECKPOINT_ATTENTION_AFTER_MS, CHECKPOINT_EXCUSED, CHECKPOINT_PROTOCOL, CHECKPOINT_SEALED,
    CHECKPOINT_VERIFIED, Certificate, CheckpointAction, CheckpointClaim, CheckpointContext,
    CheckpointExcuseRequest, CheckpointResumeRequest, CheckpointStatusView, PendingCheckpointView,
    SealTerms, VerifiedTerms, certificates, checkpoint_build, chosen_certificate, excused_writers,
    first_verifications, newest_seals, participants as checkpoint_participants, stable_checkpoints,
};
pub use checkpoint_trim::{CheckpointManifestNeed, TRIM_CHUNK_ENVELOPES, TrimFault};

/// The graph's tables: the claim log and its batches, operations, documents and blobs, replica
/// envelopes and records, peers, fleet invites and checkpoints. A runtime adds its own.
pub const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS batches (
    id TEXT PRIMARY KEY,
    origin TEXT NOT NULL,
    replica_sequence INTEGER NOT NULL,
    previous_hash TEXT,
    hash TEXT NOT NULL,
    accepted_at_unix_ms TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS batches_writer_sequence
ON batches(origin, replica_sequence);

CREATE TABLE IF NOT EXISTS claims (
    store_index INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    batch_id TEXT NOT NULL REFERENCES batches(id),
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    origin TEXT NOT NULL,
    actor TEXT,
    body TEXT NOT NULL,
    predecessors TEXT NOT NULL,
    accepted_at_unix_ms TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS claims_subject_index ON claims(subject, store_index);
CREATE INDEX IF NOT EXISTS claims_subject_accepted_index
ON claims(subject, length(accepted_at_unix_ms), accepted_at_unix_ms);
CREATE INDEX IF NOT EXISTS claims_kind_index ON claims(kind, store_index);
CREATE INDEX IF NOT EXISTS claims_subject_kind_index ON claims(subject, kind, store_index);
CREATE INDEX IF NOT EXISTS claims_subject_kind_accepted_index
ON claims(subject, kind, length(accepted_at_unix_ms), accepted_at_unix_ms);
CREATE INDEX IF NOT EXISTS claims_batch_index ON claims(batch_id, store_index);
CREATE INDEX IF NOT EXISTS claims_accepted_order_index
ON claims(length(accepted_at_unix_ms), accepted_at_unix_ms, store_index);
CREATE INDEX IF NOT EXISTS claims_operation_index
ON claims(json_extract(body, '$._operation.id'))
WHERE json_extract(body, '$._operation.id') IS NOT NULL;

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    request_digest TEXT NOT NULL,
    canonical_claim_id TEXT NOT NULL REFERENCES claims(id),
    state TEXT NOT NULL CHECK(state IN ('active','conflict'))
);
CREATE INDEX IF NOT EXISTS operations_conflict_index ON operations(id) WHERE state='conflict';
-- Foreign keys are on, so deleting a claim looks for rows that still reference it. Without an
-- index that look read every operation, 25 ms a claim, and a checkpoint trim that drops a hundred
-- thousand claims held the writer for an hour.
CREATE INDEX IF NOT EXISTS operations_canonical_claim_index ON operations(canonical_claim_id);

CREATE TABLE IF NOT EXISTS blobs (
    hash TEXT PRIMARY KEY,
    bytes BLOB NOT NULL,
    size INTEGER NOT NULL
);
-- Uploaded bytes become shared authority only when a durable claim references them.
CREATE TABLE IF NOT EXISTS local_blobs (
    hash TEXT PRIMARY KEY,
    bytes BLOB NOT NULL,
    size INTEGER NOT NULL
);

-- Who uploaded an attachment file the daemon holds outside the graph, for the per-actor quota and
-- for who may fetch it before a message names it. Local only: never replicated, in no digest.
CREATE TABLE IF NOT EXISTS local_blob_uploads (
    hash TEXT NOT NULL,
    actor TEXT NOT NULL,
    media_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    uploaded_ms INTEGER NOT NULL,
    PRIMARY KEY(hash, actor)
);

CREATE TABLE IF NOT EXISTS documents (
    name TEXT NOT NULL,
    hash TEXT NOT NULL REFERENCES blobs(hash),
    created_index INTEGER NOT NULL,
    binding_claim_id TEXT NOT NULL DEFAULT '',
    binding_key BLOB NOT NULL DEFAULT x'',
    PRIMARY KEY(name, hash)
);
CREATE INDEX IF NOT EXISTS document_latest ON documents(name, created_index DESC);
CREATE INDEX IF NOT EXISTS documents_hash_index ON documents(hash);

-- Local answers to idempotent requests, by operation.
CREATE TABLE IF NOT EXISTS idempotency (
    operation_id TEXT PRIMARY KEY,
    response TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    store_index INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS peer_cursors (
    peer TEXT PRIMARY KEY,
    accepted_through INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS peer_replica_cursors (
    peer TEXT NOT NULL,
    origin TEXT NOT NULL,
    accepted_through INTEGER NOT NULL,
    PRIMARY KEY(peer, origin)
);

CREATE TABLE IF NOT EXISTS replica_envelopes (
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    previous_hash TEXT,
    accepted_at_unix_ms TEXT NOT NULL,
    payload BLOB NOT NULL,
    batch_id TEXT,
    relay TEXT NOT NULL,
    receipt_state TEXT NOT NULL CHECK(receipt_state IN ('pending','validated','degraded')),
    validation_error TEXT,
    received_at_unix_ms TEXT NOT NULL,
    PRIMARY KEY(writer, sequence, envelope_hash)
);
CREATE INDEX IF NOT EXISTS replica_envelopes_state
ON replica_envelopes(receipt_state, writer, sequence);
CREATE INDEX IF NOT EXISTS replica_envelopes_batch
ON replica_envelopes(batch_id);

CREATE TABLE IF NOT EXISTS replica_records (
    record_ref TEXT PRIMARY KEY,
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    position INTEGER NOT NULL,
    raw BLOB NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','valid','unknown','invalid','repaired')),
    claim_id TEXT,
    subject_hint TEXT,
    kind_hint TEXT,
    error_code TEXT,
    error_message TEXT,
    replacement_claim_id TEXT,
    updated_at_unix_ms TEXT NOT NULL,
    UNIQUE(writer, sequence, envelope_hash, position)
);
CREATE INDEX IF NOT EXISTS replica_records_state
ON replica_records(state, writer, sequence);
CREATE INDEX IF NOT EXISTS replica_records_claim
ON replica_records(claim_id, position);

CREATE TABLE IF NOT EXISTS projection_health (
    aggregate TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK(status IN ('healthy','stale','indeterminate')),
    last_good_store_index INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    error_message TEXT,
    updated_at_unix_ms TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS replication_peers (
    peer TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK(status IN ('unknown','up','down','auth-failed')),
    last_success_at_unix_ms TEXT,
    last_error TEXT,
    schema_digest TEXT,
    authority_digest TEXT,
    graph_digest TEXT,
    updated_at_unix_ms TEXT NOT NULL
);

-- Direct route policy is local cache state, outside the replicated claim vocabulary.
CREATE TABLE IF NOT EXISTS replication_refusals (
    peer TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    updated_at_unix_ms TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_generation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);
INSERT OR IGNORE INTO graph_generation(id, value) VALUES (1, 0);

CREATE TABLE IF NOT EXISTS replica_envelope_signatures (
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    member_key TEXT NOT NULL,
    signature TEXT NOT NULL,
    stored_at_unix_ms TEXT NOT NULL,
    PRIMARY KEY(writer, sequence, envelope_hash, member_key)
);

CREATE TABLE IF NOT EXISTS fleet_invite_tokens (
    invite_id TEXT PRIMARY KEY,
    token TEXT,
    expires_at_unix_ms TEXT NOT NULL,
    name TEXT,
    migrate INTEGER NOT NULL DEFAULT 0,
    bound_key TEXT,
    bound_name TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    admitted_claim TEXT,
    writer_floor INTEGER,
    created_by TEXT
);

CREATE TABLE IF NOT EXISTS replica_envelope_holds (
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('unsigned','fenced')),
    updated_at_unix_ms TEXT NOT NULL,
    PRIMARY KEY(writer, sequence, envelope_hash)
);

-- A dropped envelope's identity. See `store/checkpoint.rs`.
CREATE TABLE IF NOT EXISTS checkpoint_envelopes (
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    accepted_at_unix_ms INTEGER NOT NULL,
    checkpoint TEXT NOT NULL,
    PRIMARY KEY(writer, sequence, envelope_hash)
);
-- A dropped claim: what evidence checks, ancestry walks and idempotent retries still read.
CREATE TABLE IF NOT EXISTS checkpoint_claims (
    id TEXT PRIMARY KEY,
    writer TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL,
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT,
    predecessors TEXT NOT NULL,
    operation_id TEXT,
    request_digest TEXT,
    accepted_at_unix_ms INTEGER NOT NULL,
    checkpoint TEXT NOT NULL
);
-- The checkpoints this node sealed, verified, trimmed or adopted. `seal_rowid` is the
-- `replica_envelopes` high water of the set it sealed or verified, so it can read exactly that
-- set again. Every write this node makes is dated at or after the highest cut here.
CREATE TABLE IF NOT EXISTS checkpoints (
    id TEXT PRIMARY KEY,
    cut_unix_ms INTEGER NOT NULL,
    state TEXT NOT NULL,
    seal_rowid INTEGER,
    sealed_digest TEXT,
    drop_digest TEXT,
    graph_digest TEXT,
    detail TEXT NOT NULL DEFAULT '{}',
    updated_at_unix_ms INTEGER NOT NULL
);
-- Backup seeks only admitted wire records and the newest recorded manifest.
CREATE INDEX IF NOT EXISTS replica_envelopes_admitted
ON replica_envelopes(writer, sequence, envelope_hash) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS checkpoints_manifest_cut
ON checkpoints(cut_unix_ms DESC)
WHERE state IN ('trimming','trimmed') AND drop_digest IS NOT NULL;
CREATE INDEX IF NOT EXISTS checkpoint_claims_subject ON checkpoint_claims(subject);
-- A trim reads each envelope's dropped claims.
CREATE INDEX IF NOT EXISTS checkpoint_claims_envelope
ON checkpoint_claims(writer, sequence, envelope_hash);
CREATE INDEX IF NOT EXISTS checkpoint_claims_operation
ON checkpoint_claims(operation_id) WHERE operation_id IS NOT NULL;
"#;

/// The store's schema version, set once the graph's and the runtime's tables exist.
pub const SCHEMA_VERSION: &str = "PRAGMA user_version = 15;";

/// The graph half of a store. A runtime's store wraps it and derefs to it, so the runtime's
/// projections read and write through the same connections.
pub struct Store {
    pub connection: WriterConnection,
    pub readers: ReadPool,
    pub committed_index: Arc<AtomicU64>,
    pub seeded_batch_rowid: AtomicI64,
    pub replica_generation: AtomicU64,
    pub replication_snapshot: Mutex<Option<Arc<ReplicationSnapshot>>>,
    /// Held while one thread builds the next replication snapshot, so concurrent callers reuse
    /// it instead of building their own.
    pub replication_snapshot_build: Mutex<()>,
    pub replication_sync: Mutex<BTreeMap<String, PeerSyncProgress>>,
    /// Held while replicated envelopes are admitted; see `validate_replication_backlog`.
    pub admission: Mutex<()>,
    /// Serializes projection passes while they lend the writer back between chunks.
    pub projection: Mutex<()>,
    pub replication_timers: ReplicationTimers,
    /// Low bit means deferred; each new deferral advances the generation by two so an
    /// older projection pass cannot clear a newer admission or catch-up deferral.
    replication_projection_state: AtomicU64,
    /// When this process last projected replicated claims, in Unix milliseconds.
    pub last_replication_projection_unix_ms: AtomicU64,
    /// The heals this node asks its peers, and when it last replayed its graph for one.
    pub heal: Mutex<heal::HealState>,
    /// This node's fleet member key. Set, it signs every envelope of this node's writer.
    pub member_key: std::sync::RwLock<Option<Arc<crate::fleet::MemberKey>>>,
    /// The keys this node signs claims with. See `principals`.
    pub keyring: crate::principal::Keyring,
    /// Held while this node mints a key for a person or an agent.
    pub principal_minting: Mutex<()>,
    /// The rules, read from the graph when `rules_stale` says they may have changed.
    pub rules: std::sync::RwLock<Option<Arc<Vec<crate::rules::NamedRule>>>>,
    pub rules_stale: AtomicBool,
    /// Admission took in claims whose verdicts the next projection pass judges.
    pub verdicts_due: AtomicBool,
    pub origin: String,
    /// The database file, or the shared-memory URI of an in-memory store. A checkpoint proof
    /// opens its own connection here to copy the store.
    pub path: PathBuf,
    pub shared_memory: bool,
    /// Where the next trim stops, as a crash would, and how many envelopes it deletes per
    /// transaction. Only tests change them.
    pub trim_fault: Mutex<Option<checkpoint_trim::TrimFault>>,
    pub trim_chunk_envelopes: AtomicUsize,
    /// Extra time each trimmed row takes. Tests only.
    pub trim_row_cost_micros: AtomicU64,
    /// The shortest wait between two replays for heals. A runtime's own tests set it to zero.
    pub heal_replay_backoff_ms: u128,
    /// The runtime whose projections this store keeps.
    pub runtime: Arc<dyn Runtime>,
}

impl Store {
    /// Open the store at `path`, creating it when it does not exist, with `runtime`'s tables
    /// and projections beside the graph's.
    pub fn open(path: &Path, origin: impl Into<String>, runtime: Arc<dyn Runtime>) -> Result<Self> {
        let origin = origin.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("open st database {}", path.display()))?;
        let has_meta: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
            [],
            |row| row.get(0),
        )?;
        if has_meta
            && let Some(writer) = connection
                .query_row(
                    "SELECT value FROM meta WHERE key='backup_restore_writer'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        {
            anyhow::ensure!(
                origin == writer,
                "restored database requires fresh writer `{writer}`; configure node to that identity before starting"
            );
        }
        projection_digest::register(&connection)?;
        crate::sqlite::observe(&mut connection);
        connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        // Keep the hot graph and replication index pages in SQLite's bounded
        // page cache. The default (~2 MiB per connection) churns against the
        // large durable claim store during otherwise quiet replication.
        connection.execute_batch("PRAGMA cache_size = -32768;")?;
        Self::create_schema(&connection, &*runtime)?;
        separate_staged_blobs(&mut connection)?;
        {
            let transaction = connection.transaction()?;
            runtime.open_projections(&transaction, false)?;
            seed_replica_envelopes_tx(&transaction, &origin, None)?;
            transaction.commit()?;
        }
        let readers = ReadPool::new(path, false)?;
        Self::from_connection(
            connection,
            readers,
            origin,
            path.to_path_buf(),
            false,
            runtime,
        )
    }

    /// Open a new store in shared memory, as tests and short-lived tools use.
    /// Shared-cache read/write contention returns `SQLITE_LOCKED` immediately;
    /// concurrent server tests should use the file-backed [`Self::open`] instead.
    pub fn open_memory(origin: impl Into<String>, runtime: Arc<dyn Runtime>) -> Result<Self> {
        let origin = origin.into();
        let uri = PathBuf::from(format!(
            "file:st3-{}?mode=memory&cache=shared",
            Uuid::now_v7().simple()
        ));
        let mut connection = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        projection_digest::register(&connection)?;
        crate::sqlite::observe(&mut connection);
        connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        Self::create_schema(&connection, &*runtime)?;
        {
            let transaction = connection.transaction()?;
            runtime.open_projections(&transaction, true)?;
            seed_replica_envelopes_tx(&transaction, &origin, None)?;
            transaction.commit()?;
        }
        let readers = ReadPool::new(&uri, true)?;
        Self::from_connection(connection, readers, origin, uri, true, runtime)
    }

    /// Check and migrate the schema, then create the graph's tables, the runtime's, and the
    /// triggers that keep digests current.
    fn create_schema(connection: &Connection, runtime: &dyn Runtime) -> Result<()> {
        reject_old_schema(connection)?;
        runtime.migrate_schema(connection)?;
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(principals::PRINCIPAL_SCHEMA)?;
        runtime.create_schema(connection)?;
        connection.execute_batch(SCHEMA_VERSION)?;
        document_index::initialize(connection)?;
        connection.execute_batch(WRITE_CLOCK)?;
        create_graph_generation_triggers(connection, runtime.legacy_digest_tables())?;
        projection_digest::initialize(connection, runtime.digest_tables())?;
        Ok(())
    }

    /// Wrap an opened, initialized writer connection and its read pool.
    pub fn from_connection(
        connection: Connection,
        readers: ReadPool,
        origin: String,
        path: PathBuf,
        shared_memory: bool,
        runtime: Arc<dyn Runtime>,
    ) -> Result<Self> {
        let seeded_batch_rowid = max_batch_rowid(&connection)?;
        let index = current_index(&connection)?;
        // Older stores have no admission watermark. Startup recovery projects this index
        // before serving; subsequent admission chunks update it in their own transaction.
        connection.execute(
            "INSERT OR IGNORE INTO meta(key,value) VALUES('replication_admitted_index',?1)",
            [index.to_string()],
        )?;
        let committed_index = Arc::new(AtomicU64::new(index));
        Ok(Self {
            connection: WriterConnection::new(connection, committed_index.clone()),
            readers,
            committed_index,
            seeded_batch_rowid: AtomicI64::new(seeded_batch_rowid),
            replica_generation: AtomicU64::new(0),
            replication_snapshot: Mutex::new(None),
            replication_snapshot_build: Mutex::new(()),
            replication_sync: Mutex::new(BTreeMap::new()),
            admission: Mutex::new(()),
            projection: Mutex::new(()),
            replication_timers: ReplicationTimers::default(),
            replication_projection_state: AtomicU64::new(0),
            last_replication_projection_unix_ms: AtomicU64::new(0),
            heal: Mutex::default(),
            member_key: std::sync::RwLock::new(None),
            keyring: crate::principal::Keyring::default(),
            principal_minting: Mutex::new(()),
            rules: std::sync::RwLock::new(None),
            rules_stale: AtomicBool::new(true),
            verdicts_due: AtomicBool::new(false),
            origin,
            path,
            shared_memory,
            trim_fault: Mutex::new(None),
            trim_chunk_envelopes: AtomicUsize::new(checkpoint_trim::TRIM_CHUNK_ENVELOPES),
            trim_row_cost_micros: AtomicU64::new(0),
            heal_replay_backoff_ms: heal::HEAL_REPLAY_BACKOFF_MS,
            runtime,
        })
    }
}

/// The writer connection's clock offset. Only a simulation sets it; see `write_time`.
pub const WRITE_CLOCK: &str =
    "CREATE TEMP TABLE IF NOT EXISTS write_clock(offset_ms INTEGER NOT NULL, at_ms INTEGER);";

/// Columns shared claim readers deserialize; canonical ordering lives in `store/canonical.rs`.
pub const CLAIM_COLUMNS: &str = "claims.id, claims.store_index, claims.batch_id, claims.subject,
     claims.kind, claims.origin, claims.actor, claims.body, claims.predecessors,
     claims.accepted_at_unix_ms";

/// The batches of the claims accepted at `?2` at or before store index `?1`. The `length` term is
/// the first column of `claims_accepted_order_index`, so SQLite seeks the index instead of
/// reading every claim.
#[cfg(any(test, feature = "test-support"))]
pub const LAST_ACCEPTED_BATCHES: &str = "SELECT batch_id FROM claims
     WHERE length(accepted_at_unix_ms)=length(?2) AND accepted_at_unix_ms=?2 AND store_index<=?1";

#[derive(Clone)]
pub struct ReplicationSnapshot {
    pub store_index: u64,
    pub replica_generation: u64,
    pub max_envelope_rowid: i64,
    /// Rows in `replica_envelopes` and `checkpoint_envelopes` when this snapshot was built. A
    /// snapshot extends its predecessor only when both moved exactly by the new envelopes.
    pub envelope_rows: usize,
    pub tombstone_rows: usize,
    pub inventory: CompactReplicationInventory,
    pub buckets: Vec<ReplicationInventoryBucket>,
    /// The inventory digest state before each range in `buckets`.
    pub digest_prefixes: Vec<Sha256>,
    pub authority_digest: String,
    pub graph_generation: i64,
    pub projection_generation: i64,
    pub graph_digest: String,
    pub legacy_graph_digest: String,
    pub projection_digests: BTreeMap<String, String>,
    /// Whether a local batch had no envelope yet when this snapshot was read. Its projection
    /// digests then count claims its inventory does not, so `replication_snapshot` seals and
    /// reads again rather than offer it.
    pub unsealed: bool,
}

/// Cumulative replication time by stage, in nanoseconds.
#[derive(Default)]
pub struct ReplicationTimers {
    pub exchanges: AtomicU64,
    pub envelopes_received: AtomicU64,
    pub round_trip: AtomicU64,
    pub export: AtomicU64,
    pub snapshot: AtomicU64,
    pub receipt: AtomicU64,
    pub admission: AtomicU64,
    pub verify: AtomicU64,
    pub projection: AtomicU64,
    pub repair: AtomicU64,
    pub signing: AtomicU64,
}

/// Adds the time until it drops to one stage counter.
pub struct StageTimer<'a> {
    pub counter: &'a AtomicU64,
    pub started: std::time::Instant,
}

pub fn time_stage(counter: &AtomicU64) -> StageTimer<'_> {
    StageTimer {
        counter,
        started: std::time::Instant::now(),
    }
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.counter
            .fetch_add(self.started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Live sync measurements for one peer. They are rebuilt by the first exchange after a restart,
/// so they stay in memory rather than in the graph.
#[derive(Clone, Debug, Default)]
pub struct PeerSyncProgress {
    pub measured: Option<ReplicationPeerSync>,
    pub window_started_at_unix_ms: u128,
    pub window_peer_only: u64,
    pub window_received: u64,
    pub graph_compared_at_unix_ms: Option<u128>,
    pub graph_differs_since_unix_ms: Option<u128>,
    /// When the last heal with this peer started, and how long until the next may.
    pub heal_started_at_unix_ms: Option<u128>,
    pub heal_backoff_ms: u128,
    pub heal: Option<ReplicationHealReport>,
    /// How many envelopes this node held when it took the last measurement.
    pub measured_inventory_envelopes: u64,
}

impl PeerSyncProgress {
    /// Record one comparison of this node's graph digest with the peer's, made while both nodes
    /// held the same envelopes.
    pub fn compare_graphs(&mut self, equal: bool, now: u128) {
        self.graph_compared_at_unix_ms = Some(now);
        if equal {
            self.graph_differs_since_unix_ms = None;
            self.heal_backoff_ms = 0;
        } else {
            self.graph_differs_since_unix_ms.get_or_insert(now);
        }
    }

    /// Record one receipt from the peer and, when the peer's inventory allowed it, the measured
    /// difference `(peer_only, local_only)`. Rates are sampled over windows of at least
    /// `REPLICATION_SYNC_WINDOW_MS` and smoothed so one slow exchange does not swing the estimate.
    pub fn observe(&mut self, received: usize, difference: Option<(u64, u64)>, now: u128) {
        self.window_received = self.window_received.saturating_add(received as u64);
        let Some((peer_only, local_only)) = difference else {
            return;
        };
        let mut sync = self.measured.take().unwrap_or_default();
        sync.peer_only_envelopes = peer_only;
        sync.local_only_envelopes = local_only;
        sync.measured_at_unix_ms = now;
        let elapsed = now.saturating_sub(self.window_started_at_unix_ms);
        if self.window_started_at_unix_ms != 0 && elapsed >= REPLICATION_SYNC_WINDOW_MS {
            if elapsed <= REPLICATION_SYNC_STALE_MS {
                let seconds = elapsed as f64 / 1000.0;
                let smooth = |previous: Option<f64>, sample: f64| {
                    Some(previous.map_or(sample, |previous| (previous + sample) / 2.0))
                };
                sync.receive_rate_per_second = smooth(
                    sync.receive_rate_per_second,
                    self.window_received as f64 / seconds,
                );
                let caught_up = self.window_peer_only as f64 - peer_only as f64;
                sync.catch_up_rate_per_second =
                    smooth(sync.catch_up_rate_per_second, caught_up.max(0.0) / seconds);
            }
            self.window_started_at_unix_ms = 0;
        }
        if self.window_started_at_unix_ms == 0 {
            self.window_started_at_unix_ms = now;
            self.window_peer_only = peer_only;
            self.window_received = 0;
        }
        sync.estimated_catch_up_seconds = if peer_only == 0 {
            Some(0)
        } else {
            sync.catch_up_rate_per_second
                .filter(|rate| *rate > 0.0)
                .map(|rate| (peer_only as f64 / rate).ceil() as u64)
        };
        self.measured = Some(sync);
    }

    /// The last measurement, marked as catching up while it is recent and the peer holds more
    /// than one exchange of envelopes this node lacks.
    pub fn view(&self, now: u128) -> Option<ReplicationPeerSync> {
        let mut sync = self.measured.clone()?;
        sync.stale = now.saturating_sub(sync.measured_at_unix_ms) >= PEER_QUIET_EXCHANGE_MS;
        sync.catching_up = sync.peer_only_envelopes > REPLICATION_EXCHANGE_ENVELOPE_LIMIT as u64
            && now.saturating_sub(sync.measured_at_unix_ms) <= REPLICATION_SYNC_STALE_MS;
        sync.graph_compared_at_unix_ms = self.graph_compared_at_unix_ms;
        sync.graph_differs_since_unix_ms = self.graph_differs_since_unix_ms;
        sync.diverged = self
            .graph_differs_since_unix_ms
            .zip(self.graph_compared_at_unix_ms)
            .is_some_and(|(since, compared)| {
                compared.saturating_sub(since) >= REPLICATION_DIVERGED_AFTER_MS
            });
        sync.heal = self.heal.clone();
        Some(sync)
    }
}

/// The replica envelope identities a snapshot keeps for its whole life. Writers are interned
/// and SHA-256 hashes are kept as bytes, so an identity takes 48 bytes instead of two heap
/// strings. Public identities are expanded only for the ranges an exchange lists.
#[derive(Clone, Default)]
pub struct CompactReplicationInventory {
    pub digest: String,
    /// Sorted, so writer indexes order identities the way writer names do.
    pub writers: Vec<String>,
    /// Hashes a peer sent that are not lowercase SHA-256 hex, kept verbatim.
    pub irregular_hashes: Vec<String>,
    /// In `ReplicaEnvelopeId` order: writer, sequence, then hash text.
    pub envelopes: Vec<CompactEnvelopeId>,
    /// Identities a checkpoint dropped. They are listed and digested like the others, so peers
    /// see the same inventory, but their payloads are gone and never sent. An envelope hash
    /// commits to its writer and sequence, so the hash alone names the identity.
    pub payloadless: HashSet<[u8; 32]>,
    pub payloadless_irregular: BTreeSet<ReplicaEnvelopeId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactEnvelopeId {
    pub writer: u32,
    /// Zero for a SHA-256 `hash`; otherwise one more than an `irregular_hashes` index.
    pub irregular: u32,
    pub sequence: u64,
    pub hash: [u8; 32],
}

impl CompactReplicationInventory {
    /// Build from identities already in `ReplicaEnvelopeId` order.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_sorted(identities: impl IntoIterator<Item = ReplicaEnvelopeId>) -> Self {
        let mut inventory = Self::default();
        for identity in identities {
            inventory.push_sorted(identity);
        }
        inventory.refresh_digest();
        inventory
    }

    /// Whether this node still holds the envelope's payload, so it can send it.
    pub fn has_payload(&self, envelope: &CompactEnvelopeId) -> bool {
        if envelope.irregular == 0 {
            !self.payloadless.contains(&envelope.hash)
        } else {
            !self
                .payloadless_irregular
                .contains(&self.identity(envelope))
        }
    }

    /// Mark the identity last pushed as one whose payload a checkpoint dropped.
    pub fn mark_last_payloadless(&mut self) {
        let Some(envelope) = self.envelopes.last().copied() else {
            return;
        };
        if envelope.irregular == 0 {
            self.payloadless.insert(envelope.hash);
        } else {
            let identity = self.identity(&envelope);
            self.payloadless_irregular.insert(identity);
        }
    }

    /// Append an identity that sorts after every held one. The digest is left for the caller.
    pub fn push_sorted(&mut self, identity: ReplicaEnvelopeId) {
        if self.writers.last() != Some(&identity.writer) {
            self.writers.push(identity.writer.clone());
        }
        let writer = self.writers.len() as u32 - 1;
        let envelope = self.compact(writer, identity);
        self.envelopes.push(envelope);
    }

    pub fn refresh_digest(&mut self) {
        self.digest = self.digest_of(INVENTORY_DIGEST_DOMAIN, &self.envelopes);
    }

    /// Set the inventory digest by resuming at range `from`; the value equals
    /// `refresh_digest`. `prefixes[i]` holds the digest state before `buckets[i]`, which must
    /// list the held identities range by range, and no range before `from` may have changed
    /// since `prefixes` was built. Ranges are ordered by writer, so new envelopes re-hash only
    /// their own later ranges and those of later writers.
    pub fn resume_digest(
        &mut self,
        buckets: &[ReplicationInventoryBucket],
        prefixes: &mut Vec<Sha256>,
        from: usize,
    ) {
        let from = from.min(prefixes.len().saturating_sub(1));
        let mut digest = match prefixes.get(from) {
            Some(state) => state.clone(),
            None => {
                let mut digest = Sha256::new();
                digest.update(INVENTORY_DIGEST_DOMAIN);
                digest
            }
        };
        prefixes.truncate(from);
        let mut position = buckets[..from.min(buckets.len())]
            .iter()
            .map(|bucket| bucket.count as usize)
            .sum::<usize>();
        let mut buffer = [0; 64];
        for bucket in buckets.iter().skip(from) {
            prefixes.push(digest.clone());
            let end = position + bucket.count as usize;
            for envelope in &self.envelopes[position..end] {
                update_identity_digest(
                    &mut digest,
                    &self.writers[envelope.writer as usize],
                    envelope.sequence,
                    self.hash_text(envelope, &mut buffer),
                );
            }
            #[cfg(any(test, feature = "test-support"))]
            INVENTORY_IDENTITIES_HASHED.with(|hashed| hashed.set(hashed.get() + end - position));
            position = end;
        }
        debug_assert_eq!(position, self.envelopes.len());
        self.digest = hex::encode(digest.finalize());
    }

    pub fn compact(&mut self, writer: u32, identity: ReplicaEnvelopeId) -> CompactEnvelopeId {
        let mut hash = [0_u8; 32];
        let canonical = identity.hash.len() == 64
            && identity
                .hash
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            && hex::decode_to_slice(&identity.hash, &mut hash).is_ok();
        let irregular = if canonical {
            0
        } else {
            self.irregular_hashes.push(identity.hash);
            self.irregular_hashes.len() as u32
        };
        CompactEnvelopeId {
            writer,
            irregular,
            sequence: identity.sequence,
            hash,
        }
    }

    /// Insert one identity in order. An identity already held as a tombstone gets its payload
    /// back instead, as `full_compact_replication_inventory` reads a held and tombstoned one.
    /// The digest is left for the caller to recompute.
    pub fn insert(&mut self, identity: ReplicaEnvelopeId) {
        let writer = match self.writers.binary_search(&identity.writer) {
            Ok(writer) => writer,
            Err(writer) => {
                self.writers.insert(writer, identity.writer.clone());
                for envelope in &mut self.envelopes {
                    if envelope.writer as usize >= writer {
                        envelope.writer += 1;
                    }
                }
                writer
            }
        };
        let envelope = self.compact(writer as u32, identity);
        match self
            .envelopes
            .binary_search_by(|probe| self.order(probe, &envelope))
        {
            Ok(_) if envelope.irregular == 0 => {
                self.payloadless.remove(&envelope.hash);
            }
            Ok(_) => {
                let identity = self.identity(&envelope);
                self.payloadless_irregular.remove(&identity);
                self.irregular_hashes.pop();
            }
            Err(position) => self.envelopes.insert(position, envelope),
        }
    }

    pub fn order(&self, left: &CompactEnvelopeId, right: &CompactEnvelopeId) -> std::cmp::Ordering {
        left.writer
            .cmp(&right.writer)
            .then(left.sequence.cmp(&right.sequence))
            .then_with(|| {
                if left.irregular == 0 && right.irregular == 0 {
                    // Byte order of two hashes is the order of their lowercase hex text.
                    left.hash.cmp(&right.hash)
                } else {
                    let (mut left_buffer, mut right_buffer) = ([0; 64], [0; 64]);
                    self.hash_text(left, &mut left_buffer)
                        .cmp(self.hash_text(right, &mut right_buffer))
                }
            })
    }

    pub fn hash_text<'a>(
        &'a self,
        envelope: &CompactEnvelopeId,
        buffer: &'a mut [u8; 64],
    ) -> &'a str {
        if envelope.irregular == 0 {
            hex::encode_to_slice(envelope.hash, buffer).expect("a SHA-256 hash has 64 hex bytes");
            std::str::from_utf8(buffer).expect("hex is ASCII")
        } else {
            &self.irregular_hashes[envelope.irregular as usize - 1]
        }
    }

    pub fn identity(&self, envelope: &CompactEnvelopeId) -> ReplicaEnvelopeId {
        let mut buffer = [0; 64];
        ReplicaEnvelopeId {
            writer: self.writers[envelope.writer as usize].clone(),
            sequence: envelope.sequence,
            hash: self.hash_text(envelope, &mut buffer).to_owned(),
        }
    }

    pub fn identities(&self, range: &[CompactEnvelopeId]) -> Vec<ReplicaEnvelopeId> {
        range
            .iter()
            .map(|envelope| self.identity(envelope))
            .collect()
    }

    pub fn public(&self) -> ReplicationInventory {
        ReplicationInventory {
            digest: self.digest.clone(),
            envelopes: self.identities(&self.envelopes),
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        }
    }

    pub fn digest_of(&self, domain: &[u8], range: &[CompactEnvelopeId]) -> String {
        let mut digest = Sha256::new();
        digest.update(domain);
        let mut buffer = [0; 64];
        for envelope in range {
            update_identity_digest(
                &mut digest,
                &self.writers[envelope.writer as usize],
                envelope.sequence,
                self.hash_text(envelope, &mut buffer),
            );
        }
        hex::encode(digest.finalize())
    }

    /// The identities of one writer sequence range.
    pub fn range(&self, writer: &str, start: u64) -> &[CompactEnvelopeId] {
        let Ok(writer) = self
            .writers
            .binary_search_by(|name| name.as_str().cmp(writer))
        else {
            return &[];
        };
        let writer = writer as u32;
        let end = start.saturating_add(REPLICATION_BUCKET_WIDTH);
        let from = self
            .envelopes
            .partition_point(|envelope| (envelope.writer, envelope.sequence) < (writer, start));
        let to = self
            .envelopes
            .partition_point(|envelope| (envelope.writer, envelope.sequence) < (writer, end));
        &self.envelopes[from..to]
    }

    /// Summarize one non-empty range of a single writer.
    pub fn bucket(&self, range: &[CompactEnvelopeId]) -> ReplicationInventoryBucket {
        ReplicationInventoryBucket {
            writer: self.writers[range[0].writer as usize].clone(),
            start: replication_bucket_start(range[0].sequence),
            count: range.len() as u64,
            digest: self.digest_of(BUCKET_DIGEST_DOMAIN, range),
        }
    }

    /// One digest per writer sequence range.
    pub fn buckets(&self) -> Vec<ReplicationInventoryBucket> {
        self.envelopes
            .chunk_by(|left, right| {
                left.writer == right.writer
                    && replication_bucket_start(left.sequence)
                        == replication_bucket_start(right.sequence)
            })
            .map(|range| self.bucket(range))
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReplicationAdmission {
    pub valid: usize,
    pub unknown: usize,
    pub invalid: usize,
    /// Envelopes held as `unsigned` or `fenced` by the fleet membership rules.
    pub held: usize,
    pub changed: bool,
    /// Time spent decoding envelopes, checking their hashes and claim schemas, and looking up
    /// their stored signatures.
    pub verify: std::time::Duration,
}

pub enum ReplicatedClaimAdmission {
    Valid,
    UnknownKind,
    UnknownField,
}

pub fn reject_old_schema(connection: &Connection) -> Result<()> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let table_count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        table_count == 0 || matches!(version, 10..=15),
        "this database uses an unsupported st schema; start with a new state directory"
    );
    anyhow::ensure!(
        matches!(version, 0 | 10 | 11 | 12 | 13 | 14 | 15),
        "this database uses unsupported st schema version {version}"
    );
    Ok(())
}

pub fn claims_page_query(subject: bool, descending: bool) -> String {
    let subject_filter = if subject { "subject=?3 AND " } else { "" };
    let order = if descending { "DESC" } else { "ASC" };
    format!(
        "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
         FROM claims WHERE subject NOT LIKE 'glass/%' AND {subject_filter}store_index>?1 AND (?2 IS NULL OR store_index<?2)
         ORDER BY store_index {order} LIMIT ?4"
    )
}

/// Configure a separately opened writer that changes shared projection rows, such as an
/// offline maintenance or fault-injection connection. Store's own writers configure this
/// automatically; readers do not need the SQL functions.
pub fn configure_projection_writer(connection: &Connection) -> Result<()> {
    projection_digest::register(connection)
}

#[allow(clippy::too_many_arguments)]
pub fn insert_claim(
    transaction: &Transaction<'_>,
    id: &str,
    batch_id: &str,
    subject: &str,
    kind: &str,
    origin: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    now: u128,
) -> Result<u64> {
    // Every local write comes through here; rules see each one that names its writer.
    if let Some(actor) = actor {
        principals::rules_gate_tx(transaction, origin, actor, kind, subject)?;
    }
    promote_claim_blobs(transaction, body)?;
    crate::touched::note_wrote(|| format!("{kind} {subject}"));
    transaction.execute(
        "INSERT INTO claims(id, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            batch_id,
            subject,
            kind,
            origin,
            actor,
            canonical_json_text(body)?,
            serde_json::to_string(predecessors)?,
            now.to_string(),
        ],
    )?;
    Ok(transaction.last_insert_rowid() as u64)
}

/// The next sequence for a local batch: one past the writer's highest batch, and past the
/// writer floor this store took when it joined under a name the fleet had used before.
pub fn next_replica_sequence(transaction: &Transaction<'_>, origin: &str) -> Result<u64> {
    transaction
        .query_row(
            "SELECT MAX(
                 COALESCE((SELECT MAX(replica_sequence) FROM batches WHERE origin=?1), 0),
                 COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='writer_floor/' || ?1), 0)
             ) + 1",
            [origin],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

pub fn previous_batch_hash(transaction: &Transaction<'_>, origin: &str) -> Result<Option<String>> {
    transaction
        .query_row(
            "SELECT hash FROM batches WHERE origin=?1 ORDER BY replica_sequence DESC LIMIT 1",
            [origin],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

// Seek the newest time block before evaluating canonical writer/sequence/position ties.
pub const LATEST_CLAIM_QUERY: &str =
    "SELECT id FROM claims INDEXED BY claims_subject_accepted_index
    WHERE subject=?1 ORDER BY CANONICAL_DESC(claims) LIMIT 1";

pub fn latest_claim_id_tx(transaction: &Transaction<'_>, subject: &str) -> Result<Option<String>> {
    transaction
        .query_row(&canonical_sql(LATEST_CLAIM_QUERY), [subject], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
}

/// The committed index: the highest store index ever assigned. A checkpoint deletes claims,
/// possibly the newest, so this reads the `AUTOINCREMENT` high water rather than the newest
/// remaining claim, and never moves backwards.
pub fn current_index(connection: &Connection) -> Result<u64> {
    connection
        .query_row(
            "SELECT MAX(
                 COALESCE((SELECT MAX(store_index) FROM claims), 0),
                 COALESCE((SELECT seq FROM sqlite_sequence WHERE name='claims'), 0)
             )",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

#[cfg(any(test, feature = "test-support"))]
pub fn replica_heads(connection: &Connection) -> Result<BTreeMap<String, u64>> {
    let mut statement = connection.prepare(
        "SELECT origin, MAX(replica_sequence) FROM batches GROUP BY origin ORDER BY origin",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })?;
    rows.collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(Into::into)
}

pub fn current_index_tx(transaction: &Transaction<'_>) -> Result<u64> {
    current_index(transaction)
}

pub fn selected_index(current: u64, requested: Option<u64>) -> Result<u64, St3Error> {
    match requested {
        Some(requested) if requested > current => Err(St3Error::new(
            "invalid-snapshot-index",
            format!("snapshot index {requested} is after current index {current}"),
        )),
        Some(requested) => Ok(requested),
        None => Ok(current),
    }
}

pub fn claim_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClaimRecord> {
    let body = row.get::<_, String>(7)?;
    let predecessors = row.get::<_, String>(8)?;
    let accepted = row.get::<_, String>(9)?;
    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
    Ok(ClaimRecord {
        id: row.get(0)?,
        store_index: row.get(1)?,
        batch_id: row.get(2)?,
        subject: row.get(3)?,
        kind: row.get(4)?,
        origin: row.get(5)?,
        actor: row.get(6)?,
        operation_id: operation_parts(&body).map(|(id, _)| id.to_owned()),
        request_digest: operation_parts(&body).map(|(_, digest)| digest.to_owned()),
        body,
        predecessors: serde_json::from_str(&predecessors).unwrap_or_default(),
        accepted_at_unix_ms: accepted.parse().unwrap_or_default(),
    })
}

thread_local! {
    /// A clock a simulation sets for its own thread; see [`set_thread_clock`].
    static THREAD_CLOCK: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) };
}

/// The time every reader and the reconciler compare against: the system clock, or the time a
/// simulation set for this thread.
pub fn now_ms() -> u128 {
    THREAD_CLOCK.with(std::cell::Cell::get).unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    })
}

/// Fix the clock this thread reads at `at` (unix ms), or give it back the system clock with
/// `None`. Only simulations set it; pair it with `set_write_clock_at` so writes are dated alike.
pub fn set_thread_clock(at: Option<u128>) {
    THREAD_CLOCK.with(|clock| clock.set(at));
}

/// The kinds the membership fold reads.
pub const FLEET_CLAIM_KINDS: &str = "'fleet.member-admitted','fleet.member-endpoints','fleet.member-left',\
                                 'fleet.member-removed','fleet.invite-created','fleet.invite-redeemed',\
                                 'fleet.invite-revoked'";

/// Upper bound on signature requests or answers in one exchange.
pub const REPLICATION_SIGNATURE_LIMIT: usize = 512;

/// One kind of envelope that `st3 doctor` reports: admitted before this node knew better.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FleetAdmissionResidue {
    pub writer: String,
    /// `admitted-beyond-high-water` or `admitted-unsigned`.
    pub reason: String,
    pub envelopes: u64,
}

impl Store {
    /// Set this node's member key. Every envelope of this node's writer gets its signature,
    /// including envelopes written before the key existed. Returns the number signed now.
    pub fn set_member_key(&self, key: Option<Arc<crate::fleet::MemberKey>>) -> Result<usize> {
        if key.is_none() {
            *self
                .member_key
                .write()
                .unwrap_or_else(PoisonError::into_inner) = None;
            return Ok(0);
        }
        // Seed envelopes for any local batch first, so the full pass below sees all of them.
        self.replication_snapshot()?;
        if let Some(key) = &key {
            self.set_node_key(key.clone())?;
        }
        *self
            .member_key
            .write()
            .unwrap_or_else(PoisonError::into_inner) = key;
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let signed = self.sign_own_envelopes_tx(&transaction, None)?;
        transaction.commit()?;
        Ok(signed)
    }

    /// Pin the fleet's anchor key. A node pins it once, from its own founding or from an
    /// authenticated join or migration handshake, and never replaces it.
    pub fn pin_fleet_anchor(&self, anchor: &str) -> Result<()> {
        anyhow::ensure!(!anchor.is_empty(), "the fleet anchor key is empty");
        let connection = self.connection.write();
        let stored = connection
            .query_row(
                "SELECT value FROM meta WHERE key='fleet_anchor_key'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(stored) = stored {
            anyhow::ensure!(
                stored == anchor,
                "this store already pins another fleet anchor key"
            );
            return Ok(());
        }
        connection.execute(
            "INSERT INTO meta(key, value) VALUES ('fleet_anchor_key', ?1)",
            [anchor],
        )?;
        drop(connection);
        // Held envelopes may be decidable now.
        self.replica_generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Start this store's own writer above `floor`. A node that joins under a name the fleet
    /// has used before sets this before it writes anything.
    pub fn set_writer_floor(&self, floor: u64) -> Result<()> {
        let connection = self.connection.write();
        connection.execute(
            "INSERT INTO meta(key, value) VALUES ('writer_floor/' || ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER))",
            params![self.origin, floor.to_string()],
        )?;
        Ok(())
    }

    pub fn fleet_anchor(&self) -> Result<Option<String>> {
        let connection = self.readers.get();
        fleet_meta(&connection, "fleet_anchor_key")
    }

    /// The current fleet membership, folded from admitted `fleet.*` claims.
    pub fn fleet_membership(&self) -> Result<crate::fleet::Membership> {
        // A local claim counts only once its batch is an envelope with this node's signature.
        self.replication_snapshot()?;
        let connection = self.readers.get();
        fleet_membership_tx(&connection)
    }

    /// Whether transport observations about `peer` belong in the graph. A dial-out member is
    /// never dialed and never reports on others, so neither side records one.
    pub fn observes_transport_to(&self, peer: &str) -> Result<bool> {
        if self.fleet_leaving()? {
            return Ok(false);
        }
        let membership = self.fleet_membership()?;
        let dial_out = |name: &str| {
            matches!(
                membership.state(name),
                crate::fleet::MemberState::Current(incarnation) if incarnation.mode == "dial-out"
            )
        };
        Ok(!dial_out(&self.origin) && !dial_out(peer))
    }

    /// This node's member public key, when it has one.
    pub fn member_public_key(&self) -> Option<String> {
        self.member_key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|key| key.public().to_owned())
    }

    /// Publish this member's mode and endpoints when they differ from its last announcement.
    /// Returns whether a claim was written.
    pub fn publish_fleet_endpoints(
        &self,
        mode: &str,
        endpoints: &[Value],
        build: &str,
    ) -> Result<bool> {
        let Some(member_key) = self.member_public_key() else {
            return Ok(false);
        };
        let subject = format!("host/{}", self.origin);
        if let Some(latest) = self.latest_claim(&subject, Some("fleet.member-endpoints"))? {
            let fields = latest.body.get("fields").cloned().unwrap_or_default();
            if fields["member_key"] == member_key
                && fields["mode"] == mode
                && fields["endpoints"].as_array().map(Vec::as_slice) == Some(endpoints)
            {
                return Ok(false);
            }
        }
        self.append_claim(&ClaimInput {
            subject,
            kind: "fleet.member-endpoints".into(),
            actor: None,
            fields: BTreeMap::from([
                ("member_key".into(), Value::String(member_key)),
                ("mode".into(), Value::String(mode.into())),
                ("endpoints".into(), Value::Array(endpoints.to_vec())),
                ("build".into(), Value::String(build.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .map_err(anyhow::Error::from)?;
        // Seed and sign at once, so the announcement counts in this node's own fold.
        self.replication_snapshot()?;
        Ok(true)
    }

    /// The membership view the replication worker uses to decide who may exchange.
    pub fn fleet_view(&self) -> Result<crate::fleet::FleetView> {
        Ok(crate::fleet::FleetView::from_membership(
            &self.fleet_membership()?,
        ))
    }

    /// The fleet as its sealed envelopes show it, without taking the writer, for reads. A local
    /// membership claim shows once the next exchange seals its batch.
    pub fn fleet_view_sealed(&self) -> Result<crate::fleet::FleetView> {
        self.sealed_replication_snapshot()?;
        let connection = self.readers.get();
        Ok(crate::fleet::FleetView::from_membership(
            &fleet_membership_tx(&connection)?,
        ))
    }

    /// Read the already admitted membership for client projections. Seeding a
    /// replication snapshot can take the writer lock and must stay out of a
    /// person's read request while replication is busy.
    pub fn fleet_view_for_client(&self) -> Result<crate::fleet::FleetView> {
        let member_key = self
            .member_key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|key| key.public().to_owned());
        let connection = self.readers.get();
        Ok(crate::fleet::FleetView::from_membership(
            &fleet_membership_tx_with_local_signer(
                &connection,
                member_key.as_deref().map(|key| (self.origin.as_str(), key)),
            )?,
        ))
    }

    /// Sign every envelope of this node's writer that has no signature by its member key,
    /// optionally only those whose batches follow the last processed batch. Another process
    /// can seal a batch before this keyed worker sees it; an envelope-row frontier would skip it.
    pub fn sign_own_envelopes_tx(
        &self,
        transaction: &Transaction<'_>,
        after_batch_rowid: Option<i64>,
    ) -> Result<usize> {
        self.sign_own_envelopes_range_tx(transaction, after_batch_rowid, None)
    }

    fn sign_own_envelopes_range_tx(
        &self,
        transaction: &Transaction<'_>,
        after_batch_rowid: Option<i64>,
        through_batch_rowid: Option<i64>,
    ) -> Result<usize> {
        let _timing = time_stage(&self.replication_timers.signing);
        let Some(key) = self
            .member_key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return Ok(0);
        };
        let Some(fleet_id) = fleet_meta(transaction, "fleet_id")? else {
            return Ok(0);
        };
        let from = if after_batch_rowid.is_some() {
            "FROM batches CROSS JOIN replica_envelopes AS envelopes
             ON envelopes.batch_id=batches.id WHERE batches.rowid>?2 AND batches.rowid<=?4 AND envelopes.writer=?1"
        } else {
            "FROM replica_envelopes AS envelopes WHERE envelopes.writer=?1 AND envelopes.rowid>?2 AND envelopes.rowid<=?4"
        };
        let mut statement = transaction.prepare(&format!(
            "SELECT envelopes.sequence, envelopes.envelope_hash {from}
               AND NOT EXISTS (
                 SELECT 1 FROM replica_envelope_signatures AS signatures
                 WHERE signatures.writer=envelopes.writer
                   AND signatures.sequence=envelopes.sequence
                   AND signatures.envelope_hash=envelopes.envelope_hash
                   AND signatures.member_key=?3
               )
             ORDER BY envelopes.sequence"
        ))?;
        let unsigned = statement
            .query_map(
                params![
                    self.origin,
                    after_batch_rowid.unwrap_or(i64::MIN),
                    key.public(),
                    through_batch_rowid.unwrap_or(i64::MAX),
                ],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let now = now_ms().to_string();
        for (sequence, envelope_hash) in &unsigned {
            let signature = key.sign(&crate::fleet::envelope_signature_message(
                &fleet_id,
                &self.origin,
                *sequence,
                envelope_hash,
            ));
            transaction.execute(
                "INSERT OR IGNORE INTO replica_envelope_signatures(
                     writer, sequence, envelope_hash, member_key, signature, stored_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    self.origin,
                    sequence,
                    envelope_hash,
                    key.public(),
                    signature,
                    now
                ],
            )?;
        }
        Ok(unsigned.len())
    }

    /// Envelopes this node holds but cannot admit until their writer's signature arrives.
    pub fn replication_signature_requests(&self) -> Result<Vec<ReplicaEnvelopeId>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT writer, sequence, envelope_hash FROM replica_envelope_holds
             WHERE reason='unsigned' ORDER BY writer, sequence, envelope_hash LIMIT ?1",
        )?;
        let requests = statement
            .query_map([REPLICATION_SIGNATURE_LIMIT as i64], |row| {
                Ok(ReplicaEnvelopeId {
                    writer: row.get(0)?,
                    sequence: row.get(1)?,
                    hash: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(requests)
    }

    /// The signatures this node holds for the requested envelopes.
    pub fn replication_signatures_for(
        &self,
        requests: &[ReplicaEnvelopeId],
    ) -> Result<Vec<crate::claim::ReplicaEnvelopeSignature>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT member_key, signature FROM replica_envelope_signatures
             WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3 ORDER BY member_key",
        )?;
        let mut signatures = Vec::new();
        for request in requests.iter().take(REPLICATION_SIGNATURE_LIMIT) {
            let rows = statement
                .query_map(
                    params![request.writer, request.sequence, request.hash],
                    |row| {
                        Ok(crate::claim::ReplicaEnvelopeSignature {
                            writer: request.writer.clone(),
                            sequence: request.sequence,
                            hash: request.hash.clone(),
                            member_key: row.get(0)?,
                            signature: row.get(1)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            signatures.extend(rows);
        }
        Ok(signatures)
    }

    /// Envelopes this node admitted before it knew that their writer is keyed or that their
    /// incarnation had ended. Admission decisions are not revisited, so these stay admitted;
    /// `st3 doctor` reports them.
    pub fn fleet_admission_residue(&self) -> Result<Vec<FleetAdmissionResidue>> {
        let connection = self.readers.get();
        let membership = fleet_membership_tx(&connection)?;
        let writers = membership
            .incarnations()
            .map(|incarnation| incarnation.name.clone())
            .filter(|writer| *writer != self.origin)
            .collect::<BTreeSet<_>>();
        let mut statement = connection.prepare(
            "SELECT envelopes.sequence,
                    (SELECT group_concat(member_key, ' ') FROM replica_envelope_signatures AS signatures
                     WHERE signatures.writer=envelopes.writer
                       AND signatures.sequence=envelopes.sequence
                       AND signatures.envelope_hash=envelopes.envelope_hash)
             FROM replica_envelopes AS envelopes
             WHERE envelopes.writer=?1 AND envelopes.receipt_state='validated'",
        )?;
        let mut residue = Vec::new();
        for writer in writers {
            let mut beyond = 0;
            let mut unsigned = 0;
            let rows = statement
                .query_map([&writer], |row| {
                    Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (sequence, signers) in rows {
                match membership.window(&writer, sequence) {
                    crate::fleet::Window::Legacy => {}
                    crate::fleet::Window::Fenced => beyond += 1,
                    crate::fleet::Window::Keyed(keys) => {
                        let signed = signers
                            .as_deref()
                            .unwrap_or_default()
                            .split(' ')
                            .any(|signer| keys.contains(signer));
                        if !signed {
                            unsigned += 1;
                        }
                    }
                }
            }
            for (reason, envelopes) in [
                ("admitted-beyond-high-water", beyond),
                ("admitted-unsigned", unsigned),
            ] {
                if envelopes != 0 {
                    residue.push(FleetAdmissionResidue {
                        writer: writer.clone(),
                        reason: reason.into(),
                        envelopes,
                    });
                }
            }
        }
        Ok(residue)
    }
}

pub fn fleet_meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?)
}

/// The highest sequence this node holds of a writer, counting envelopes a checkpoint dropped.
pub fn writer_high_water(connection: &Connection, writer: &str) -> Result<Option<u64>> {
    Ok(connection.query_row(
        "SELECT MAX(sequence) FROM (
             SELECT MAX(sequence) AS sequence FROM replica_envelopes WHERE writer=?1
             UNION ALL
             SELECT MAX(sequence) FROM checkpoint_envelopes WHERE writer=?1
         )",
        [writer],
        |row| row.get(0),
    )?)
}

pub fn max_envelope_rowid(connection: &Connection) -> Result<i64> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM replica_envelopes",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

/// Fold the admitted `fleet.*` claims from the pinned anchor. A store without an anchor has an
/// empty membership, in which every writer is legacy.
pub fn fleet_membership_tx(connection: &Connection) -> Result<crate::fleet::Membership> {
    fleet_membership_tx_with_local_signer(connection, None)
}

pub fn fleet_membership_tx_with_local_signer(
    connection: &Connection,
    local_signer: Option<(&str, &str)>,
) -> Result<crate::fleet::Membership> {
    let Some(anchor) = fleet_meta(connection, "fleet_anchor_key")? else {
        return Ok(crate::fleet::Membership::default());
    };
    let mut statement = connection.prepare(&format!(
        "SELECT claims.id, claims.kind, claims.subject, claims.body,
                batches.origin, batches.replica_sequence, envelopes.envelope_hash
         FROM claims JOIN batches ON batches.id=claims.batch_id
         LEFT JOIN replica_envelopes AS envelopes ON envelopes.batch_id=claims.batch_id
         WHERE claims.kind IN ({FLEET_CLAIM_KINDS})
         ORDER BY claims.id"
    ))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut signers_statement = connection.prepare(
        "SELECT member_key FROM replica_envelope_signatures
         WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
    )?;
    let mut claims: Vec<crate::fleet::FleetClaim> = Vec::with_capacity(rows.len());
    for (id, kind, subject, body, writer, sequence, envelope_hash) in rows {
        let mut signers = match &envelope_hash {
            Some(hash) => signers_statement
                .query_map(params![writer, sequence, hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<BTreeSet<_>, _>>()?,
            None => BTreeSet::new(),
        };
        // A locally appended batch has not been seeded into an envelope yet.
        // Its eventual signature uses this node's key; include it in the client
        // view without doing that write while a request is being served.
        if envelope_hash.is_none()
            && let Some((local_origin, key)) = local_signer
            && writer == local_origin
        {
            signers.insert(key.to_owned());
        }
        if let Some(existing) = claims.iter_mut().find(|claim| claim.id == id) {
            existing.signers.extend(signers);
            continue;
        }
        let fields = serde_json::from_str::<Value>(&body)?
            .get("fields")
            .and_then(Value::as_object)
            .map(|fields| {
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        claims.push(crate::fleet::FleetClaim {
            id,
            kind,
            subject,
            fields,
            writer,
            sequence,
            signers,
        });
    }
    Ok(crate::fleet::Membership::fold(Some(&anchor), &claims))
}

/// Whether the membership rules hold this envelope back, and why.
pub fn fleet_admission_hold(
    connection: &Connection,
    membership: &crate::fleet::Membership,
    envelope: &ReplicaEnvelope,
) -> Result<Option<&'static str>> {
    match membership.window(&envelope.writer, envelope.sequence) {
        crate::fleet::Window::Legacy => Ok(None),
        crate::fleet::Window::Fenced => Ok(Some("fenced")),
        crate::fleet::Window::Keyed(keys) => {
            let mut statement = connection.prepare_cached(
                "SELECT member_key FROM replica_envelope_signatures
                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
            )?;
            let signers = statement
                .query_map(
                    params![envelope.writer, envelope.sequence, envelope.hash],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            // A signature by any other key proves nothing, so it cannot fence the envelope:
            // it waits, asking for its writer's signature, until one arrives. Only a sequence
            // outside every window is fenced.
            if signers.iter().any(|signer| keys.contains(signer)) {
                Ok(None)
            } else {
                Ok(Some("unsigned"))
            }
        }
    }
}

pub fn hold_replica_envelope(
    connection: &Connection,
    envelope: &ReplicaEnvelope,
    reason: &str,
) -> Result<()> {
    connection.execute(
        "INSERT INTO replica_envelope_holds(writer, sequence, envelope_hash, reason, updated_at_unix_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(writer, sequence, envelope_hash) DO UPDATE SET reason=excluded.reason,
            updated_at_unix_ms=excluded.updated_at_unix_ms
         WHERE reason<>excluded.reason",
        params![
            envelope.writer,
            envelope.sequence,
            envelope.hash,
            reason,
            now_ms().to_string()
        ],
    )?;
    Ok(())
}

/// Whether an admitted envelope carried a `fleet.*` claim, which can change membership.
pub fn envelope_carries_fleet_claims(
    connection: &Connection,
    envelope: &ReplicaEnvelope,
) -> Result<bool> {
    Ok(connection.query_row(
        &format!(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_envelopes AS envelopes
                 JOIN claims ON claims.batch_id=envelopes.batch_id
                 WHERE envelopes.writer=?1 AND envelopes.sequence=?2 AND envelopes.envelope_hash=?3
                   AND claims.kind IN ({FLEET_CLAIM_KINDS})
             )"
        ),
        params![envelope.writer, envelope.sequence, envelope.hash],
        |row| row.get(0),
    )?)
}

/// Verify one envelope signature and store it. Returns 1 when it is new. A signature that does
/// not verify is dropped: the envelope then counts as unsigned.
#[allow(clippy::too_many_arguments)]
pub fn store_envelope_signature_tx(
    transaction: &Transaction<'_>,
    fleet_id: &str,
    writer: &str,
    sequence: u64,
    envelope_hash: &str,
    member_key: &str,
    signature: &str,
    now: &str,
) -> Result<usize> {
    let message =
        crate::fleet::envelope_signature_message(fleet_id, writer, sequence, envelope_hash);
    if !crate::fleet::verify_signature(member_key, &message, signature) {
        return Ok(0);
    }
    Ok(transaction.execute(
        "INSERT OR IGNORE INTO replica_envelope_signatures(
             writer, sequence, envelope_hash, member_key, signature, stored_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![writer, sequence, envelope_hash, member_key, signature, now],
    )?)
}

/// What `st fleet remove` did.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FleetRemoval {
    pub name: String,
    pub keys: Vec<String>,
    pub high_water: u64,
    pub revoked_invites: Vec<String>,
}

/// An invite this node just created. The token is returned once, for the join code.
#[derive(Clone, Debug)]
pub struct CreatedFleetInvite {
    pub invite: String,
    pub token: [u8; 16],
    pub expires_at_unix_ms: u64,
}

/// The sponsor's answer to one join request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FleetRedemption {
    /// This node holds no open invite, so the join route does not exist.
    Closed,
    /// Refused. The reason is for this node's records only; the joiner learns nothing.
    Refused(&'static str),
    Admitted {
        token: [u8; 16],
        writer_floor: Option<u64>,
        admitted_claim: Option<String>,
        first: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FleetInviteView {
    pub invite: String,
    pub sponsor: String,
    pub name: Option<String>,
    pub expires_at_unix_ms: u64,
    pub created_by: Option<String>,
    /// `open`, `redeemed`, `revoked`, or `expired`.
    pub state: String,
    pub redeemed_name: Option<String>,
    pub redeemed_key: Option<String>,
    pub redeemed_at_unix_ms: Option<u128>,
    pub revoked_reason: Option<String>,
}

pub const FLEET_INVITE_FAILURE_LIMIT: u64 = 5;

/// Node names: a letter or digit, then letters, digits, `.`, `_`, or `-`, at most 63 bytes.
pub fn valid_fleet_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name != "local"
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

impl Store {
    /// Create an invite that this node sponsors. Only a current listening member can.
    pub fn create_fleet_invite(
        &self,
        name: Option<&str>,
        lifetime: std::time::Duration,
        transports: &[String],
        person: &str,
        migrate: bool,
    ) -> Result<CreatedFleetInvite, St3Error> {
        if let Some(name) = name
            && !valid_fleet_node_name(name)
        {
            return Err(St3Error::new(
                "invalid-node-name",
                "a node name is a letter or digit followed by letters, digits, `.`, `_`, or `-`",
            ));
        }
        if !(10..=86_400).contains(&lifetime.as_secs()) {
            return Err(St3Error::new(
                "invalid-invite-lifetime",
                "an invite lasts between 10 seconds and 24 hours",
            ));
        }
        let membership = self.fleet_membership().map_err(internal)?;
        match membership.state(&self.origin) {
            crate::fleet::MemberState::Current(own) if own.mode == "listening" => {}
            crate::fleet::MemberState::Current(_) => {
                return Err(St3Error::new(
                    "dial-out-cannot-sponsor",
                    "a dial-out member accepts no connections, so it cannot sponsor an invite; run this on a listening member",
                ));
            }
            _ => {
                return Err(St3Error::new(
                    "not-a-member",
                    "this node is not a current fleet member",
                ));
            }
        }
        let mut invite = [0_u8; 16];
        let mut token = [0_u8; 16];
        getrandom::fill(&mut invite).map_err(internal)?;
        getrandom::fill(&mut token).map_err(internal)?;
        let invite = hex::encode(invite);
        let expires_at = now_ms() + lifetime.as_millis();
        let expires_at = u64::try_from(expires_at).map_err(internal)?;
        let mut fields = BTreeMap::from([
            (
                "sponsor".into(),
                Value::String(format!("host/{}", self.origin)),
            ),
            ("expires_at_unix_ms".into(), Value::from(expires_at)),
            (
                "transports".into(),
                Value::Array(transports.iter().cloned().map(Value::String).collect()),
            ),
            ("created_by".into(), Value::String(person.into())),
        ]);
        if let Some(name) = name {
            fields.insert("name".into(), Value::String(name.into()));
        }
        self.append_claim(&ClaimInput {
            subject: format!("fleet-invite/{invite}"),
            kind: "fleet.invite-created".into(),
            actor: Some(person.into()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        {
            let connection = self.connection.write();
            connection
                .execute(
                    "INSERT INTO fleet_invite_tokens(
                         invite_id, token, expires_at_unix_ms, name, migrate, created_by
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        invite,
                        hex::encode(token),
                        expires_at.to_string(),
                        name,
                        migrate,
                        person
                    ],
                )
                .map_err(internal)?;
        }
        self.replication_snapshot().map_err(internal)?;
        Ok(CreatedFleetInvite {
            invite,
            token,
            expires_at_unix_ms: expires_at,
        })
    }

    /// Remove a member, or a config peer that was never one. Ends every current incarnation of
    /// `name` at the highest sequence this node holds for that writer, and revokes the invites
    /// it sponsored.
    pub fn remove_fleet_member(
        &self,
        name: &str,
        reason: &str,
        person: &str,
    ) -> Result<FleetRemoval, St3Error> {
        use crate::fleet::MemberState;
        if name == self.origin {
            return Err(St3Error::new(
                "cannot-remove-self",
                "a member cannot remove itself; use st fleet leave",
            ));
        }
        let own_key = self.member_public_key().ok_or_else(|| {
            St3Error::new(
                "not-a-member",
                "only a member with a member key can remove; this node has none",
            )
        })?;
        let membership = self.fleet_membership().map_err(internal)?;
        if !matches!(membership.state(&self.origin), MemberState::Current(own) if own.member_key == own_key)
        {
            return Err(St3Error::new(
                "not-a-member",
                "this node is not a current fleet member",
            ));
        }
        let keys = match membership.state(name) {
            MemberState::Current(incarnation) => vec![Some(incarnation.member_key.clone())],
            MemberState::Conflicted(current) => current
                .iter()
                .map(|incarnation| Some(incarnation.member_key.clone()))
                .collect(),
            MemberState::Ended(_) | MemberState::LegacyRemoved(_) => {
                return Err(St3Error::new(
                    "already-removed",
                    format!("`{name}` is already out of the fleet"),
                ));
            }
            MemberState::NotMember => vec![None],
        };
        let high_water = {
            let connection = self.readers.get();
            writer_high_water(&connection, name)
                .map_err(internal)?
                .unwrap_or(0)
        };
        if keys == [None] && high_water == 0 {
            return Err(St3Error::new(
                "unknown-member",
                format!("`{name}` has never been a member or written to this fleet"),
            ));
        }
        for key in &keys {
            let mut fields = BTreeMap::from([
                ("high_water".into(), Value::from(high_water)),
                ("reason".into(), Value::String(reason.into())),
                ("removed_by".into(), Value::String(person.into())),
            ]);
            if let Some(key) = key {
                fields.insert("member_key".into(), Value::String(key.clone()));
            }
            self.append_claim(&ClaimInput {
                subject: format!("host/{name}"),
                kind: "fleet.member-removed".into(),
                actor: Some(person.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })?;
        }
        let sponsored = self
            .fleet_invites(false)
            .map_err(internal)?
            .into_iter()
            .filter(|invite| invite.state == "open" && invite.sponsor == format!("host/{name}"))
            .collect::<Vec<_>>();
        for invite in &sponsored {
            self.revoke_fleet_invite(
                invite.invite.trim_start_matches("fleet-invite/"),
                "sponsor-removed",
                Some(person),
            )
            .map_err(internal)?;
        }
        self.replication_snapshot().map_err(internal)?;
        Ok(FleetRemoval {
            name: name.into(),
            keys: keys.into_iter().flatten().collect(),
            high_water,
            revoked_invites: sponsored.into_iter().map(|invite| invite.invite).collect(),
        })
    }

    /// Append this member's leave as its writer's last batch.
    /// Write this node's leave, once per member key: running `st fleet leave` again returns the
    /// leave already written, so nothing follows it.
    pub fn leave_fleet(&self, person: &str) -> Result<ClaimRecord, St3Error> {
        let key = self
            .member_public_key()
            .ok_or_else(|| St3Error::new("not-a-member", "this node has no member key"))?;
        let subject = format!("host/{}", self.origin);
        if let Some(written) = self
            .latest_claim(&subject, Some("fleet.member-left"))
            .map_err(internal)?
            .filter(|claim| claim.body["fields"]["member_key"] == key.as_str())
        {
            return Ok(written);
        }
        let record = self.append_claim(&ClaimInput {
            subject,
            kind: "fleet.member-left".into(),
            actor: Some(person.into()),
            fields: BTreeMap::from([
                ("member_key".into(), Value::String(key)),
                ("high_water".into(), Value::from(0)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        self.replication_snapshot().map_err(internal)?;
        Ok(record)
    }

    /// While leaving, this node refuses new local writes, so its leave stays its last.
    pub fn set_fleet_leaving(&self, leaving: bool) -> Result<()> {
        let connection = self.connection.write();
        if leaving {
            connection.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('fleet_leaving', '1')",
                [],
            )?;
        } else {
            connection.execute("DELETE FROM meta WHERE key='fleet_leaving'", [])?;
        }
        Ok(())
    }

    pub fn fleet_leaving(&self) -> Result<bool> {
        let connection = self.readers.get();
        Ok(fleet_meta(&connection, "fleet_leaving")?.is_some())
    }

    /// The anchor admits itself, once.
    pub fn admit_fleet_anchor(&self, fleet_id: &str, member_key: &str, mode: &str) -> Result<()> {
        self.append_claim(&ClaimInput {
            subject: format!("host/{}", self.origin),
            kind: "fleet.member-admitted".into(),
            actor: None,
            fields: BTreeMap::from([
                ("fleet_id".into(), Value::String(fleet_id.into())),
                ("member_key".into(), Value::String(member_key.into())),
                ("via".into(), Value::String("anchor".into())),
                ("mode".into(), Value::String(mode.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("fleet-anchor:{member_key}")),
        })
        .map_err(anyhow::Error::from)?;
        self.replication_snapshot()?;
        Ok(())
    }

    /// This store's own highest batch under `writer`, if it ever wrote as that name.
    pub fn writer_head(&self, writer: &str) -> Result<Option<(u64, String)>> {
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT replica_sequence, hash FROM batches WHERE origin=?1
                 ORDER BY replica_sequence DESC LIMIT 1",
                [writer],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// The fleet this store is bound to, if any.
    pub fn bound_fleet(&self) -> Result<Option<String>> {
        let connection = self.readers.get();
        fleet_meta(&connection, "fleet_id")
    }

    /// Erase the tokens of invites that expired or were revoked anywhere in the fleet.
    pub fn sweep_fleet_invites(&self) -> Result<()> {
        let connection = self.connection.write();
        connection.execute(
            "UPDATE fleet_invite_tokens SET token=NULL
             WHERE token IS NOT NULL AND (
               CAST(expires_at_unix_ms AS INTEGER) <= ?1
               OR EXISTS (
                 SELECT 1 FROM claims
                 WHERE claims.subject='fleet-invite/' || fleet_invite_tokens.invite_id
                   AND claims.kind='fleet.invite-revoked'
               )
             )",
            [now_ms().to_string()],
        )?;
        Ok(())
    }

    /// Whether this node holds an invite that can still be redeemed.
    pub fn has_open_fleet_invites(&self) -> Result<bool> {
        self.sweep_fleet_invites()?;
        let connection = self.readers.get();
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM fleet_invite_tokens WHERE token IS NOT NULL)",
            [],
            |row| row.get(0),
        )?)
    }

    /// Decide one join request against the invites this node sponsors.
    pub fn redeem_fleet_invite(
        &self,
        request: &crate::fleet::handshake::JoinRequest,
    ) -> Result<FleetRedemption> {
        use crate::fleet::handshake::{RequestFault, verify_request};
        if !self.has_open_fleet_invites()? {
            return Ok(FleetRedemption::Closed);
        }
        type InviteRow = (
            String,
            Option<String>,
            bool,
            Option<String>,
            Option<String>,
            u64,
            Option<String>,
            Option<u64>,
            Option<String>,
        );
        let row: Option<InviteRow> = {
            let connection = self.readers.get();
            connection
                .query_row(
                    "SELECT token, name, migrate, bound_key, bound_name, failures, admitted_claim,
                            writer_floor, created_by
                     FROM fleet_invite_tokens WHERE invite_id=?1 AND token IS NOT NULL",
                    [&request.invite],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ))
                    },
                )
                .optional()?
        };
        let Some((
            token,
            pinned,
            migrate,
            bound_key,
            bound_name,
            failures,
            admitted_claim,
            writer_floor,
            created_by,
        )) = row
        else {
            return Ok(FleetRedemption::Refused("unknown-invite"));
        };
        let token: [u8; 16] = hex::decode(&token)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("a stored invite token is damaged")?;
        match verify_request(request, &token) {
            Ok(()) => {}
            Err(RequestFault::Proof) => {
                let failures = failures + 1;
                let connection = self.connection.write();
                connection.execute(
                    "UPDATE fleet_invite_tokens SET failures=?2 WHERE invite_id=?1",
                    params![request.invite, failures],
                )?;
                drop(connection);
                if failures >= FLEET_INVITE_FAILURE_LIMIT {
                    self.revoke_fleet_invite(&request.invite, "too-many-failures", None)?;
                }
                return Ok(FleetRedemption::Refused("proof"));
            }
            Err(_) => return Ok(FleetRedemption::Refused("request")),
        }
        if request.migrate != migrate {
            return Ok(FleetRedemption::Refused("kind"));
        }
        if pinned
            .as_deref()
            .is_some_and(|pinned| pinned != request.name)
            || !valid_fleet_node_name(&request.name)
            || !matches!(request.mode.as_str(), "listening" | "dial-out")
        {
            return Ok(FleetRedemption::Refused("name"));
        }
        if let Some(bound) = bound_key {
            if bound != request.member_key || bound_name.as_deref() != Some(&request.name) {
                return Ok(FleetRedemption::Refused("bound-to-another-key"));
            }
            // The same joiner again: admit it again without new claims, unless the claims were
            // interrupted before they were written.
            let admitted_claim = match admitted_claim {
                Some(claim) => Some(claim),
                None => Some(self.append_fleet_admission(
                    request,
                    migrate,
                    writer_floor,
                    created_by.as_deref(),
                )?),
            };
            return Ok(FleetRedemption::Admitted {
                token,
                writer_floor,
                admitted_claim,
                first: false,
            });
        }
        let floor =
            match self.fleet_name_floor(&request.name, request.writer_head.is_some(), migrate)? {
                Ok(floor) => floor,
                Err(reason) => return Ok(FleetRedemption::Refused(reason)),
            };
        let bound = {
            let connection = self.connection.write();
            connection.execute(
                "UPDATE fleet_invite_tokens SET bound_key=?2, bound_name=?3, writer_floor=?4
                 WHERE invite_id=?1 AND bound_key IS NULL AND token IS NOT NULL",
                params![request.invite, request.member_key, request.name, floor],
            )?
        };
        if bound == 0 {
            // Another request bound it first.
            return Ok(FleetRedemption::Refused("bound-to-another-key"));
        }
        let admitted =
            self.append_fleet_admission(request, migrate, floor, created_by.as_deref())?;
        Ok(FleetRedemption::Admitted {
            token,
            writer_floor: floor,
            admitted_claim: Some(admitted),
            first: true,
        })
    }

    /// The name rules from the design: a name with history can be joined again only after its
    /// incarnations ended, and only by a store that never wrote as it; then the new window
    /// starts above everything known of the old ones. A migration keeps the node's own history.
    pub fn fleet_name_floor(
        &self,
        name: &str,
        joiner_wrote_as_name: bool,
        migrate: bool,
    ) -> Result<Result<Option<u64>, &'static str>> {
        use crate::fleet::MemberState;
        let membership = self.fleet_membership()?;
        let state = membership.state(name);
        if matches!(state, MemberState::Current(_) | MemberState::Conflicted(_)) {
            return Ok(Err("name-in-use"));
        }
        let ended_end = match &state {
            MemberState::Ended(_) => membership
                .incarnations()
                .filter(|incarnation| incarnation.name == name)
                .filter_map(|incarnation| incarnation.end)
                .max(),
            MemberState::LegacyRemoved(high_water) => Some(*high_water),
            _ => None,
        };
        if migrate {
            return Ok(if ended_end.is_some() {
                Err("name-was-removed")
            } else {
                Ok(None)
            });
        }
        let connection = self.readers.get();
        let held = writer_high_water(&connection, name)?;
        let claimed: bool = connection.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind IN ({FLEET_CLAIM_KINDS}))"
            ),
            [format!("host/{name}")],
            |row| row.get(0),
        )?;
        if held.is_none() && !claimed {
            return Ok(Ok(None));
        }
        let Some(ended_end) = ended_end else {
            return Ok(Err("name-has-unremoved-history"));
        };
        if joiner_wrote_as_name {
            return Ok(Err("store-already-wrote-as-name"));
        }
        Ok(Ok(Some(held.unwrap_or(0).max(ended_end))))
    }

    pub fn append_fleet_admission(
        &self,
        request: &crate::fleet::handshake::JoinRequest,
        migrate: bool,
        writer_floor: Option<u64>,
        created_by: Option<&str>,
    ) -> Result<String> {
        let fleet_id = {
            let connection = self.readers.get();
            fleet_meta(&connection, "fleet_id")?.context("this store is not in a fleet")?
        };
        let invite_subject = format!("fleet-invite/{}", request.invite);
        self.append_claim(&ClaimInput {
            subject: invite_subject.clone(),
            kind: "fleet.invite-redeemed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("name".into(), Value::String(request.name.clone())),
                (
                    "member_key".into(),
                    Value::String(request.member_key.clone()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("fleet-invite-redeemed:{}", request.invite)),
        })?;
        let mut fields = BTreeMap::from([
            ("fleet_id".into(), Value::String(fleet_id)),
            (
                "member_key".into(),
                Value::String(request.member_key.clone()),
            ),
            (
                "via".into(),
                Value::String(if migrate { "migration" } else { "invite" }.into()),
            ),
            (
                "sponsor".into(),
                Value::String(format!("host/{}", self.origin)),
            ),
            ("invite".into(), Value::String(invite_subject)),
            ("mode".into(), Value::String(request.mode.clone())),
        ]);
        if let Some(floor) = writer_floor {
            fields.insert("writer_floor".into(), Value::from(floor));
        }
        if let Some(person) = created_by {
            fields.insert("admitted_by".into(), Value::String(person.into()));
        }
        let admitted = self.append_claim(&ClaimInput {
            subject: format!("host/{}", request.name),
            kind: "fleet.member-admitted".into(),
            actor: created_by.map(str::to_owned),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("fleet-invite-admitted:{}", request.invite)),
        })?;
        {
            let connection = self.connection.write();
            connection.execute(
                "UPDATE fleet_invite_tokens SET admitted_claim=?2 WHERE invite_id=?1",
                params![request.invite, admitted.id],
            )?;
        }
        // Seed and sign at once: the joiner's first exchange must find itself admitted.
        self.replication_snapshot()?;
        Ok(admitted.id)
    }

    /// Revoke an invite. Any member can; the sponsor erases the token when it sees the claim.
    pub fn revoke_fleet_invite(
        &self,
        invite: &str,
        reason: &str,
        person: Option<&str>,
    ) -> Result<()> {
        let mut fields = BTreeMap::from([("reason".into(), Value::String(reason.into()))]);
        if let Some(person) = person {
            fields.insert("revoked_by".into(), Value::String(person.into()));
        }
        self.append_claim(&ClaimInput {
            subject: format!("fleet-invite/{invite}"),
            kind: "fleet.invite-revoked".into(),
            actor: person.map(str::to_owned),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("fleet-invite-revoked:{invite}")),
        })?;
        self.sweep_fleet_invites()?;
        self.replication_snapshot()?;
        Ok(())
    }

    /// Invites as the fleet knows them, from their claims.
    pub fn fleet_invites(&self, all: bool) -> Result<Vec<FleetInviteView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT subject, kind, body, accepted_at_unix_ms FROM claims
             WHERE kind IN ('fleet.invite-created','fleet.invite-redeemed','fleet.invite-revoked')
             ORDER BY CANONICAL_ASC(claims)",
        ))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut invites: BTreeMap<String, FleetInviteView> = BTreeMap::new();
        let now = now_ms();
        for (subject, kind, body, accepted_at) in rows {
            let Some(invite) = subject.strip_prefix("fleet-invite/") else {
                continue;
            };
            let body: Value = serde_json::from_str(&body)?;
            let fields = &body["fields"];
            let text = |name: &str| fields[name].as_str().map(str::to_owned);
            match kind.as_str() {
                "fleet.invite-created" => {
                    invites.insert(
                        invite.to_owned(),
                        FleetInviteView {
                            invite: subject.clone(),
                            sponsor: text("sponsor").unwrap_or_default(),
                            name: text("name"),
                            expires_at_unix_ms: fields["expires_at_unix_ms"].as_u64().unwrap_or(0),
                            created_by: text("created_by"),
                            state: "open".into(),
                            redeemed_name: None,
                            redeemed_key: None,
                            redeemed_at_unix_ms: None,
                            revoked_reason: None,
                        },
                    );
                }
                "fleet.invite-redeemed" => {
                    if let Some(view) = invites.get_mut(invite) {
                        view.state = "redeemed".into();
                        view.redeemed_name = text("name");
                        view.redeemed_key = text("member_key");
                        view.redeemed_at_unix_ms = accepted_at.parse().ok();
                    }
                }
                "fleet.invite-revoked" => {
                    if let Some(view) = invites.get_mut(invite)
                        && view.state == "open"
                    {
                        view.state = "revoked".into();
                        view.revoked_reason = text("reason");
                    }
                }
                _ => {}
            }
        }
        let mut views = invites.into_values().collect::<Vec<_>>();
        for view in &mut views {
            if view.state == "open" && u128::from(view.expires_at_unix_ms) <= now {
                view.state = "expired".into();
            }
        }
        if !all {
            views.retain(|view| view.state == "open" || view.state == "redeemed");
        }
        Ok(views)
    }
}

/// Record an envelope that failed verification, keeping it for inspection and repair.
pub fn record_invalid_replica_envelope(
    connection: &Connection,
    envelope: &ReplicaEnvelope,
    error: &St3Error,
) -> Result<()> {
    let record_ref = replica_record_ref(&envelope.writer, envelope.sequence, &envelope.hash, 0);
    connection.execute(
        "INSERT INTO replica_records(
             record_ref, writer, sequence, envelope_hash, position, raw, state,
             error_code, error_message, updated_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, 0, ?5, 'invalid', ?6, ?7, ?8)
         ON CONFLICT(record_ref) DO UPDATE SET state='invalid', error_code=excluded.error_code,
            error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
        params![
            record_ref,
            envelope.writer,
            envelope.sequence,
            envelope.hash,
            envelope.payload,
            error.code,
            error.message,
            now_ms().to_string(),
        ],
    )?;
    connection.execute(
        "UPDATE replica_envelopes SET receipt_state='degraded', validation_error=?4
         WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
        params![
            envelope.writer,
            envelope.sequence,
            envelope.hash,
            error.message
        ],
    )?;
    Ok(())
}

pub fn max_batch_rowid(connection: &Connection) -> Result<i64> {
    connection
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM batches", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
}

pub fn seed_replica_envelopes_tx(
    transaction: &Transaction<'_>,
    relay: &str,
    after_rowid: Option<i64>,
) -> Result<()> {
    seed_replica_envelopes_range_tx(transaction, relay, after_rowid, None)
}

fn seed_replica_envelopes_range_tx(
    transaction: &Transaction<'_>,
    relay: &str,
    after_rowid: Option<i64>,
    through_rowid: Option<i64>,
) -> Result<()> {
    seed_replica_envelopes_signed_tx(transaction, relay, after_rowid, through_rowid, None)
}

/// Seal batches into envelopes. With `sign`, each claim without a stored signature is signed
/// and its signature travels in the payload.
#[allow(clippy::type_complexity)]
fn seed_replica_envelopes_signed_tx(
    transaction: &Transaction<'_>,
    relay: &str,
    after_rowid: Option<i64>,
    through_rowid: Option<i64>,
    sign: Option<&dyn Fn(&principals::Unsealed<'_>) -> Option<crate::principal::ClaimSignature>>,
) -> Result<()> {
    let order = if after_rowid.is_some() {
        "batches.rowid"
    } else {
        "origin, replica_sequence, id"
    };
    let mut statement = transaction.prepare(&format!(
        "SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms
         FROM batches
         WHERE batches.rowid>=?1 AND batches.rowid<=?2
           AND NOT EXISTS (
             SELECT 1 FROM replica_envelopes WHERE replica_envelopes.batch_id=batches.id
         )
         ORDER BY {order}"
    ))?;
    let headers = statement
        .query_map(
            params![
                after_rowid.map_or(i64::MIN, |rowid| rowid.saturating_add(1)),
                through_rowid.unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (id, writer, sequence, previous_hash, legacy_hash, accepted_at) in headers {
        let mut claim_statement = transaction.prepare(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims WHERE batch_id=?1 ORDER BY store_index",
        )?;
        let claims = claim_statement
            .query_map([&id], claim_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(claim_statement);
        let mut blobs = BTreeMap::new();
        collect_referenced_blobs(transaction, &claims, &mut blobs)?;
        let mut claim_signatures = BTreeMap::new();
        for claim in &claims {
            let existing = transaction
                .prepare_cached("SELECT signature FROM claim_signatures WHERE claim_id=?1")?
                .query_row([&claim.id], |row| row.get::<_, String>(0))
                .optional()?
                .and_then(|text| serde_json::from_str(&text).ok());
            let signature = match (existing, sign) {
                (Some(signature), _) => Some(signature),
                (None, Some(sign)) => sign(&principals::Unsealed {
                    subject: &claim.subject,
                    kind: &claim.kind,
                    actor: claim.actor.as_deref(),
                    body: &claim.body,
                    accepted_at_unix_ms: claim.accepted_at_unix_ms,
                }),
                (None, None) => None,
            };
            if let Some(signature) = signature {
                principals::store_claim_signature_tx(transaction, &claim.id, &signature)?;
                claim_signatures.insert(claim.id.clone(), signature);
            }
        }
        let batch = ReplicaBatch {
            id: id.clone(),
            origin: writer.clone(),
            replica_sequence: sequence,
            previous_hash: previous_hash.clone(),
            hash: legacy_hash,
            accepted_at_unix_ms: accepted_at.parse().unwrap_or_default(),
            claims: claims.clone(),
        };
        let mut payload = Vec::new();
        ciborium::into_writer(
            &ReplicaEnvelopePayload {
                batch,
                blobs,
                claim_signatures,
            },
            &mut payload,
        )?;
        let envelope_hash = replica_envelope_hash(
            &writer,
            sequence,
            previous_hash.as_deref(),
            accepted_at.parse().unwrap_or_default(),
            &payload,
        );
        let encoded_payload = base64::engine::general_purpose::STANDARD.encode(&payload);
        transaction.execute(
            "INSERT OR IGNORE INTO replica_envelopes(
                 writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms,
                 payload, batch_id, relay, receipt_state, received_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'validated', ?5)",
            params![
                writer,
                sequence,
                envelope_hash,
                previous_hash,
                accepted_at,
                encoded_payload,
                id,
                relay
            ],
        )?;
        for (position, claim) in claims.iter().enumerate() {
            let mut raw = Vec::new();
            ciborium::into_writer(claim, &mut raw)?;
            transaction.execute(
                "INSERT OR IGNORE INTO replica_records(
                     record_ref, writer, sequence, envelope_hash, position, raw, state,
                     claim_id, subject_hint, kind_hint, updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'valid', ?7, ?8, ?9, ?10)",
                params![
                    replica_record_ref(&writer, sequence, &envelope_hash, position as u64),
                    writer,
                    sequence,
                    envelope_hash,
                    position as u64,
                    raw,
                    claim.id,
                    claim.subject,
                    claim.kind,
                    accepted_at,
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn full_replication_inventory_rows(
    connection: &Connection,
) -> Result<(Vec<ReplicaEnvelopeId>, i64)> {
    let mut statement = connection.prepare(
        "SELECT rowid, writer, sequence, envelope_hash FROM replica_envelopes
         ORDER BY writer, sequence, envelope_hash",
    )?;
    let mut max_rowid = 0;
    let envelopes = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ReplicaEnvelopeId {
                    writer: row.get(1)?,
                    sequence: row.get(2)?,
                    hash: row.get(3)?,
                },
            ))
        })?
        .map(|row| {
            let (rowid, identity) = row?;
            max_rowid = max_rowid.max(rowid);
            Ok(identity)
        })
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    Ok((envelopes, max_rowid))
}

/// Read every envelope identity this node holds straight into compact form, without a public
/// copy: the envelopes it stores and those a checkpoint dropped, which it keeps as tombstones.
pub fn full_compact_replication_inventory(
    connection: &Connection,
) -> Result<(CompactReplicationInventory, i64)> {
    let mut statement = connection.prepare(
        "SELECT rowid, writer, sequence, envelope_hash, 1 FROM replica_envelopes
         UNION ALL
         SELECT 0, writer, sequence, envelope_hash, 0 FROM checkpoint_envelopes AS dropped
         WHERE NOT EXISTS (
             SELECT 1 FROM replica_envelopes AS held
             WHERE held.writer=dropped.writer AND held.sequence=dropped.sequence
               AND held.envelope_hash=dropped.envelope_hash
         )
         ORDER BY 2, 3, 4",
    )?;
    let mut rows = statement.query([])?;
    let mut inventory = CompactReplicationInventory::default();
    let mut max_rowid = 0;
    while let Some(row) = rows.next()? {
        max_rowid = max_rowid.max(row.get::<_, i64>(0)?);
        inventory.push_sorted(ReplicaEnvelopeId {
            writer: row.get(1)?,
            sequence: row.get(2)?,
            hash: row.get(3)?,
        });
        if row.get::<_, i64>(4)? == 0 {
            inventory.mark_last_payloadless();
        }
    }
    inventory.refresh_digest();
    Ok((inventory, max_rowid))
}

/// The most envelopes one exchange carries to a peer that does not say how many it takes, and
/// the most identities one divergent exchange lists beyond its first differing range.
pub const REPLICATION_EXCHANGE_ENVELOPE_LIMIT: usize = 512;

/// Envelopes admitted per writer transaction. Admission takes 2-3 ms per envelope on a populated
/// store, so a chunk holds the writer for well under a second.
pub const ADMISSION_CHUNK_ENVELOPES: usize = 256;

/// Newly admitted claims projected per writer transaction. A catch-up page must not keep
/// queued lease renewals and messages behind its entire incremental projection.
pub const PROJECTION_CHUNK_CLAIMS: usize = 128;

/// The most envelopes this build takes in one exchange, which it says in each inventory it
/// sends. Admission commits once per pass, so a page this size admits well within the peer
/// request timeout, and a first sync needs a few dozen exchanges instead of hundreds.
pub const REPLICATION_PAGE_LIMIT: u32 = 4_096;

/// The page this node asks peers for: `REPLICATION_PAGE_LIMIT`, or less when
/// `ST3_REPLICATION_PAGE_LIMIT` caps it, so tests can make a backlog of several pages cheaply.
pub fn replication_accepts() -> u32 {
    static CAP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("ST3_REPLICATION_PAGE_LIMIT")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .map_or(REPLICATION_PAGE_LIMIT, |cap| {
                cap.clamp(1, REPLICATION_PAGE_LIMIT)
            })
    })
}

/// How many envelopes one exchange carries to a peer, from the inventory it sent. An older
/// peer says nothing and takes the classic 512.
pub fn replication_page_limit(remote: &ReplicationInventory) -> usize {
    remote
        .accepts
        .map_or(REPLICATION_EXCHANGE_ENVELOPE_LIMIT, |accepts| {
            (accepts as usize).clamp(1, REPLICATION_PAGE_LIMIT as usize)
        })
}

/// How often a node catching up with a peer projects the claims it has admitted.
pub const CATCH_UP_PROJECTION_INTERVAL_MS: u64 = 30_000;

/// The shortest span one sync rate sample covers.
pub const REPLICATION_SYNC_WINDOW_MS: u128 = 10_000;

/// A sync measurement older than this no longer says the node is catching up, and a longer gap
/// between measurements gives no rate sample.
pub const REPLICATION_SYNC_STALE_MS: u128 = 300_000;

/// A peer counts as up for this long after its last exchange in either direction.
pub const PEER_UP_MS: u128 = 90_000;

/// Healthy peers exchange at least once per 30-second quiet interval. Once this long has passed
/// since the last exchange and an attempt since then failed, the peer is not up: it missed an
/// exchange, and the one this node tried did not happen. A failure sooner than this can be a
/// one-way route while the peer still reaches this node.
pub const PEER_QUIET_EXCHANGE_MS: u128 = 35_000;

/// Whether a peer counts as up `now`, from its last exchange and whether an attempt failed since.
pub fn peer_up(last_success_at: Option<u128>, failed_since: bool, now: u128) -> bool {
    last_success_at.is_some_and(|at| {
        let age = now.saturating_sub(at);
        age < PEER_UP_MS && !(failed_since && age >= PEER_QUIET_EXCHANGE_MS)
    })
}

/// How long comparisons must keep finding the same envelopes projecting different graphs before
/// the two nodes count as diverged. A peer can export between storing envelopes and projecting
/// them, and a catching-up peer defers projection for up to `CATCH_UP_PROJECTION_INTERVAL_MS`,
/// so a shorter difference can still settle by itself.
pub const REPLICATION_DIVERGED_AFTER_MS: u128 = 2 * CATCH_UP_PROJECTION_INTERVAL_MS as u128;

/// Sequences per compact inventory range. A range digest lets two peers skip every range they
/// already share, so an exchange lists only the identities in ranges that differ.
pub const REPLICATION_BUCKET_WIDTH: u64 = 256;

pub const INVENTORY_DIGEST_DOMAIN: &[u8] = b"st3-replication-inventory-v1\0";

pub const BUCKET_DIGEST_DOMAIN: &[u8] = b"st3-replication-bucket-v1\0";

pub fn replication_inventory_digest(envelopes: &[ReplicaEnvelopeId]) -> String {
    replication_identity_digest(INVENTORY_DIGEST_DOMAIN, envelopes)
}

pub fn replication_bucket_digest<'a>(
    envelopes: impl IntoIterator<Item = &'a ReplicaEnvelopeId>,
) -> String {
    replication_identity_digest(BUCKET_DIGEST_DOMAIN, envelopes)
}

pub fn replication_bucket_start(sequence: u64) -> u64 {
    sequence - sequence % REPLICATION_BUCKET_WIDTH
}

/// Compare a local inventory with a peer's compact inventory. Returns the local envelopes the
/// peer provably lacks, bounded per exchange, and the local identities of the ranges both
/// sides hold with different digests, so the peer can compute the reverse difference. Whole
/// ranges are listed in order while they fit `listing_limit`. The first differing range is
/// always listed, so each exchange settles at least one range and later ones follow.
pub fn compact_replication_difference(
    inventory: &CompactReplicationInventory,
    buckets: &[ReplicationInventoryBucket],
    remote: &ReplicationInventory,
    listing_limit: usize,
) -> (Vec<ReplicaEnvelopeId>, Vec<ReplicaEnvelopeId>) {
    let remote_buckets = remote
        .buckets
        .iter()
        .map(|bucket| ((bucket.writer.as_str(), bucket.start), bucket))
        .collect::<BTreeMap<_, _>>();
    let mut remote_listed = BTreeMap::<(&str, u64), Vec<&ReplicaEnvelopeId>>::new();
    for identity in &remote.envelopes {
        remote_listed
            .entry((
                identity.writer.as_str(),
                replication_bucket_start(identity.sequence),
            ))
            .or_default()
            .push(identity);
    }
    let page_limit = replication_page_limit(remote);
    let mut missing = Vec::new();
    let mut listed = Vec::new();
    let mut listing_full = false;
    for bucket in buckets {
        let key = (bucket.writer.as_str(), bucket.start);
        let local = inventory.range(&bucket.writer, bucket.start);
        let room = page_limit.saturating_sub(missing.len());
        let Some(theirs) = remote_buckets.get(&key) else {
            // The peer holds nothing in this range.
            missing.extend(
                local
                    .iter()
                    .filter(|envelope| inventory.has_payload(envelope))
                    .take(room)
                    .map(|envelope| inventory.identity(envelope)),
            );
            continue;
        };
        if theirs.digest == bucket.digest {
            continue;
        }
        let compact = local;
        let local = inventory.identities(compact);
        if !listing_full && (listed.is_empty() || listed.len() + local.len() <= listing_limit) {
            listed.extend_from_slice(&local);
        } else {
            listing_full = true;
        }
        // Only the peer's complete listing of this range proves which identities it lacks. A
        // range that changed after the peer chose what to list waits for the next exchange.
        let Some(known) = remote_listed.get_mut(&key) else {
            continue;
        };
        known.sort_unstable();
        known.dedup();
        if known.len() as u64 != theirs.count
            || replication_bucket_digest(known.iter().copied()) != theirs.digest
        {
            continue;
        }
        missing.extend(
            compact
                .iter()
                .zip(local)
                .filter(|(envelope, identity)| {
                    inventory.has_payload(envelope) && known.binary_search(&identity).is_err()
                })
                .map(|(_, identity)| identity)
                .take(room),
        );
    }
    (missing, listed)
}

/// Count the envelopes only the peer holds and only this node holds, as `(peer_only,
/// local_only)`, from the inventory the peer sent. A range both sides hold with different
/// digests counts exactly when the peer listed it, and otherwise counts the difference in range
/// sizes, which is a lower bound. `None` means the inventory cannot tell, such as a bare digest
/// that this node has since moved past.
pub fn replication_inventory_difference(
    inventory: &CompactReplicationInventory,
    buckets: &[ReplicationInventoryBucket],
    remote: &ReplicationInventory,
) -> Option<(u64, u64)> {
    if !remote.digest.is_empty() && remote.digest == inventory.digest {
        return Some((0, 0));
    }
    if !remote.buckets.is_empty() {
        let local_buckets = buckets
            .iter()
            .map(|bucket| ((bucket.writer.as_str(), bucket.start), bucket))
            .collect::<BTreeMap<_, _>>();
        let mut remote_listed = BTreeMap::<(&str, u64), Vec<&ReplicaEnvelopeId>>::new();
        for identity in &remote.envelopes {
            remote_listed
                .entry((
                    identity.writer.as_str(),
                    replication_bucket_start(identity.sequence),
                ))
                .or_default()
                .push(identity);
        }
        let mut shared = BTreeSet::new();
        let (mut peer_only, mut local_only) = (0_u64, 0_u64);
        for theirs in &remote.buckets {
            let key = (theirs.writer.as_str(), theirs.start);
            let Some(ours) = local_buckets.get(&key) else {
                peer_only += theirs.count;
                continue;
            };
            shared.insert(key);
            if ours.digest == theirs.digest {
                continue;
            }
            let known = remote_listed.get_mut(&key).and_then(|known| {
                known.sort_unstable();
                known.dedup();
                (known.len() as u64 == theirs.count
                    && replication_bucket_digest(known.iter().copied()) == theirs.digest)
                    .then_some(&*known)
            });
            if let Some(known) = known {
                let local = inventory.identities(inventory.range(&ours.writer, ours.start));
                peer_only += known
                    .iter()
                    .filter(|identity| local.binary_search(**identity).is_err())
                    .count() as u64;
                local_only += local
                    .iter()
                    .filter(|identity| known.binary_search(identity).is_err())
                    .count() as u64;
            } else {
                peer_only += theirs.count.saturating_sub(ours.count);
                local_only += ours.count.saturating_sub(theirs.count);
            }
        }
        local_only += buckets
            .iter()
            .filter(|bucket| !shared.contains(&(bucket.writer.as_str(), bucket.start)))
            .map(|bucket| bucket.count)
            .sum::<u64>();
        return Some((peer_only, local_only));
    }
    if !remote.digest.is_empty() && remote.digest == replication_inventory_digest(&remote.envelopes)
    {
        // A peer without range digests, or with an empty store, lists its whole inventory once
        // per identity. Look each one up in its range rather than expanding every local one.
        let mut buffer = [0; 64];
        let shared = remote
            .envelopes
            .iter()
            .filter(|identity| {
                inventory
                    .range(
                        &identity.writer,
                        replication_bucket_start(identity.sequence),
                    )
                    .iter()
                    .any(|envelope| {
                        envelope.sequence == identity.sequence
                            && inventory.hash_text(envelope, &mut buffer) == identity.hash
                    })
            })
            .count();
        // A peer may list an identity twice, so neither count may go below zero.
        return Some((
            remote.envelopes.len().saturating_sub(shared) as u64,
            inventory.envelopes.len().saturating_sub(shared) as u64,
        ));
    }
    None
}

pub fn replication_identity_digest<'a>(
    domain: &[u8],
    envelopes: impl IntoIterator<Item = &'a ReplicaEnvelopeId>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for envelope in envelopes {
        update_identity_digest(
            &mut digest,
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
        );
    }
    hex::encode(digest.finalize())
}

pub fn update_identity_digest(digest: &mut Sha256, writer: &str, sequence: u64, hash: &str) {
    for field in [writer, &sequence.to_string(), hash] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
}

/// Range digests computed from public identities, independent of the compact inventory.
#[cfg(any(test, feature = "test-support"))]
pub fn test_replication_buckets(
    envelopes: &[ReplicaEnvelopeId],
) -> Vec<ReplicationInventoryBucket> {
    envelopes
        .chunk_by(|left, right| {
            left.writer == right.writer
                && replication_bucket_start(left.sequence)
                    == replication_bucket_start(right.sequence)
        })
        .map(|range| ReplicationInventoryBucket {
            writer: range[0].writer.clone(),
            start: replication_bucket_start(range[0].sequence),
            count: range.len() as u64,
            digest: replication_bucket_digest(range),
        })
        .collect()
}

#[cfg(test)]
#[test]
pub fn compact_replication_inventory_matches_its_public_identities() {
    let mut identities = [
        test_envelope_ids("example-mac-like", 1..=300, "a"),
        test_envelope_ids("example-linux-like", 250..=600, "a"),
        test_envelope_ids("example-linux-like", [255, 256, 511], "b"),
    ]
    .concat();
    // Hashes a peer could send that are not lowercase SHA-256 hex keep their exact text.
    let upper = identities[0].hash.to_uppercase();
    identities.extend([
        ReplicaEnvelopeId {
            writer: "example-linux-like".into(),
            sequence: 256,
            hash: upper.clone(),
        },
        ReplicaEnvelopeId {
            writer: "example-peer-like".into(),
            sequence: 7,
            hash: "not-a-hash".into(),
        },
        ReplicaEnvelopeId {
            writer: "example-mac-like".into(),
            sequence: 7,
            hash: String::new(),
        },
    ]);
    let mut sorted = identities.clone();
    sorted.sort();

    let bulk = CompactReplicationInventory::from_sorted(sorted.clone());
    // Insert in a scattered order so new writers and hashes land between held ones.
    let mut incremental = CompactReplicationInventory::default();
    for (index, identity) in identities.iter().enumerate().rev() {
        if index % 2 == 0 {
            incremental.insert(identity.clone());
        }
    }
    for (index, identity) in identities.iter().enumerate() {
        if index % 2 == 1 {
            incremental.insert(identity.clone());
        }
    }
    incremental.refresh_digest();

    for inventory in [&bulk, &incremental] {
        assert_eq!(inventory.public().envelopes, sorted);
        assert_eq!(inventory.digest, replication_inventory_digest(&sorted));
        assert_eq!(inventory.buckets(), test_replication_buckets(&sorted));
        assert_eq!(inventory.irregular_hashes.len(), 3);
        let range = inventory.range("example-linux-like", 256);
        assert_eq!(
            inventory.identities(range),
            sorted
                .iter()
                .filter(|id| id.writer == "example-linux-like" && (256..512).contains(&id.sequence))
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    assert!(inventory_holds(
        &incremental,
        "example-linux-like",
        256,
        &upper
    ));
    assert!(inventory_holds(
        &incremental,
        "example-peer-like",
        7,
        "not-a-hash"
    ));
    assert!(bulk.range("absent", 0).is_empty());

    fn inventory_holds(
        inventory: &CompactReplicationInventory,
        writer: &str,
        start: u64,
        hash: &str,
    ) -> bool {
        inventory
            .identities(inventory.range(writer, replication_bucket_start(start)))
            .iter()
            .any(|id| id.hash == hash)
    }
}

/// An envelope a node holds again after a checkpoint dropped it is listed once, with its
/// payload, whether the inventory was read whole or extended by the new envelope.
#[cfg(test)]
#[test]
pub fn a_tombstoned_identity_held_again_is_listed_once() {
    let mut identities = test_envelope_ids("example-linux-like", [1, 2], "a");
    identities.push(ReplicaEnvelopeId {
        writer: "example-linux-like".into(),
        sequence: 3,
        hash: "not-a-hash".into(),
    });
    let mut inventory = CompactReplicationInventory::default();
    for identity in &identities {
        inventory.push_sorted(identity.clone());
        inventory.mark_last_payloadless();
    }
    for identity in &identities {
        inventory.insert(identity.clone());
    }
    inventory.refresh_digest();

    assert_eq!(inventory.public().envelopes, identities);
    assert_eq!(inventory.digest, replication_inventory_digest(&identities));
    assert!(
        inventory
            .envelopes
            .iter()
            .all(|envelope| inventory.has_payload(envelope))
    );
    assert_eq!(inventory.irregular_hashes.len(), 1);
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_envelope_ids(
    writer: &str,
    sequences: impl IntoIterator<Item = u64>,
    fork: &str,
) -> Vec<ReplicaEnvelopeId> {
    sequences
        .into_iter()
        .map(|sequence| ReplicaEnvelopeId {
            writer: writer.into(),
            sequence,
            hash: hex::encode(Sha256::digest(format!("{fork}-{writer}-{sequence}"))),
        })
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestReplica(BTreeSet<ReplicaEnvelopeId>);

#[cfg(any(test, feature = "test-support"))]
impl TestReplica {
    pub fn inventory(&self) -> (CompactReplicationInventory, Vec<ReplicationInventoryBucket>) {
        let inventory = CompactReplicationInventory::from_sorted(self.0.iter().cloned());
        let buckets = inventory.buckets();
        (inventory, buckets)
    }

    pub fn summary(&self) -> ReplicationInventory {
        let (inventory, buckets) = self.inventory();
        ReplicationInventory {
            digest: inventory.digest,
            envelopes: Vec::new(),
            buckets,
            accepts: None,
            checkpoint: None,
        }
    }

    /// Answer a peer inventory the way `export_replication_exchange` does and return the
    /// envelopes sent plus the compact inventory the peer receives.
    pub fn answer(
        &self,
        remote: &ReplicationInventory,
    ) -> (Vec<ReplicaEnvelopeId>, ReplicationInventory) {
        self.answer_listing(remote, REPLICATION_EXCHANGE_ENVELOPE_LIMIT)
    }

    /// Answer with a listing limit. `usize::MAX` lists every differing range, as builds before
    /// the listing limit do.
    pub fn answer_listing(
        &self,
        remote: &ReplicationInventory,
        listing_limit: usize,
    ) -> (Vec<ReplicaEnvelopeId>, ReplicationInventory) {
        let (inventory, buckets) = self.inventory();
        let (missing, listed) =
            compact_replication_difference(&inventory, &buckets, remote, listing_limit);
        let inventory = ReplicationInventory {
            digest: inventory.digest,
            envelopes: listed,
            buckets,
            accepts: None,
            checkpoint: None,
        };
        (missing, inventory)
    }

    /// Run one two-phase outbound exchange from `self` to `peer`, as `peer::exchange` does.
    pub fn exchange(&mut self, peer: &mut Self) -> usize {
        let limit = REPLICATION_EXCHANGE_ENVELOPE_LIMIT;
        self.exchange_listing(limit, peer, limit)
    }

    /// Run one exchange where each side lists differing ranges up to its own limit, and return
    /// the largest listing either side sent.
    pub fn exchange_listing(
        &mut self,
        own_limit: usize,
        peer: &mut Self,
        peer_limit: usize,
    ) -> usize {
        let (pulled, response) = peer.answer_listing(&self.summary(), peer_limit);
        let mut listed = response.envelopes.len();
        self.0.extend(pulled);
        let (pushed, push) = self.answer_listing(&response, own_limit);
        listed = listed.max(push.envelopes.len());
        peer.0.extend(pushed);
        let (pulled, _) = peer.answer_listing(&push, peer_limit);
        self.0.extend(pulled);
        listed
    }
}

#[cfg(test)]
#[test]
pub fn compact_replication_exchange_lists_only_ranges_that_differ() {
    let shared = [
        test_envelope_ids("example-linux-like", 1..=20_000, "a"),
        test_envelope_ids("example-mac-like", 1..=20_000, "a"),
    ]
    .concat();
    let mut left = TestReplica(shared.iter().cloned().collect());
    let mut right = TestReplica(shared.iter().cloned().collect());
    // A new publish on one side, a sparse gap relayed around the other, and two candidates
    // at one writer sequence all fall inside ranges both peers already hold.
    left.0
        .extend(test_envelope_ids("example-mac-like", [20_001], "a"));
    right.0.extend(test_envelope_ids(
        "example-linux-like",
        20_001..=20_003,
        "a",
    ));
    left.0
        .remove(&test_envelope_ids("example-linux-like", [19_990], "a")[0]);
    right
        .0
        .extend(test_envelope_ids("example-mac-like", [20_000], "b"));

    let listed = left.exchange(&mut right);
    assert_eq!(left.0, right.0, "one exchange converges both peers");
    assert!(
        listed <= 2 * REPLICATION_BUCKET_WIDTH as usize,
        "only the differing ranges are listed, not all {} identities: {listed}",
        shared.len()
    );

    let full = serde_json::to_vec(&ReplicationInventory {
        digest: String::new(),
        envelopes: left.0.iter().cloned().collect(),
        buckets: Vec::new(),
        accepts: None,
        checkpoint: None,
    })
    .unwrap()
    .len();
    let compact = serde_json::to_vec(&left.summary()).unwrap().len();
    assert!(
        compact * 50 < full,
        "the compact inventory ({compact} bytes) must be far smaller than the full one ({full} bytes)"
    );
}

/// Two peers that each miss one envelope in every range. Each exchange lists a bounded prefix
/// of the differing ranges, so no response carries every identity, and the peers still
/// converge, including with a peer that lists every differing range as earlier builds do.
#[cfg(test)]
#[test]
pub fn compact_replication_exchange_bounds_the_listing_of_many_differing_ranges() {
    let all = test_envelope_ids("example-linux-like", 1..=5_000, "a");
    let ranges = 5_000 / REPLICATION_BUCKET_WIDTH as usize + 1;
    let replica = |skip: u64| {
        TestReplica(
            all.iter()
                .filter(|id| id.sequence % REPLICATION_BUCKET_WIDTH != skip)
                .cloned()
                .collect(),
        )
    };
    let limit = REPLICATION_EXCHANGE_ENVELOPE_LIMIT;
    for (own, peer_limit) in [(limit, limit), (limit, usize::MAX), (usize::MAX, limit)] {
        let (mut left, mut right) = (replica(1), replica(2));
        let (_, response) = right.answer_listing(&left.summary(), peer_limit);
        if peer_limit == limit {
            let bytes = serde_json::to_vec(&response).unwrap().len();
            assert!(
                response.envelopes.len() <= limit && bytes < 128 * 1024,
                "a divergent answer lists {} identities in {bytes} bytes",
                response.envelopes.len()
            );
        }
        let mut exchanges = 0;
        while left.0 != right.0 {
            exchanges += 1;
            assert!(
                exchanges <= ranges,
                "every exchange settles at least one differing range"
            );
            let listed = left.exchange_listing(own, &mut right, peer_limit);
            if own == limit && peer_limit == limit {
                assert!(listed <= limit, "one exchange listed {listed} identities");
            }
        }
        assert_eq!(left.0.len(), all.len());
        if own == limit && peer_limit == limit {
            assert!(
                exchanges > 1,
                "the listing limit spreads the ranges over exchanges"
            );
        }
    }
}

#[cfg(test)]
#[test]
pub fn compact_replication_exchange_drains_a_backlog_in_bounded_batches() {
    let mut fresh = TestReplica(BTreeSet::new());
    let mut source = TestReplica(
        test_envelope_ids("origin", 1..=2_000, "a")
            .into_iter()
            .collect(),
    );
    let mut exchanges = 0;
    while fresh.0 != source.0 {
        exchanges += 1;
        assert!(exchanges <= 4, "each exchange moves a full bounded batch");
        let (pulled, _) = source.answer(&fresh.summary());
        assert!(pulled.len() <= REPLICATION_EXCHANGE_ENVELOPE_LIMIT);
        fresh.exchange(&mut source);
    }
    assert!(
        exchanges >= 2,
        "one exchange must not exceed the batch limit"
    );
}

#[cfg(test)]
#[test]
pub fn compact_replication_exchange_waits_for_a_complete_listing() {
    let (inventory, buckets) = TestReplica(
        test_envelope_ids("origin", 1..=10, "a")
            .into_iter()
            .collect(),
    )
    .inventory();
    let peer = TestReplica(
        test_envelope_ids("origin", 1..=8, "a")
            .into_iter()
            .collect(),
    );
    let limit = REPLICATION_EXCHANGE_ENVELOPE_LIMIT;
    let mut listing = ReplicationInventory {
        digest: String::new(),
        envelopes: test_envelope_ids("origin", 1..=8, "a"),
        buckets: peer.summary().buckets,
        accepts: None,
        checkpoint: None,
    };
    let (missing, _) = compact_replication_difference(&inventory, &buckets, &listing, limit);
    assert_eq!(missing, test_envelope_ids("origin", 9..=10, "a"));

    // A truncated listing cannot prove what the peer lacks, so nothing is resent blindly.
    listing.envelopes.truncate(4);
    let (missing, listed) = compact_replication_difference(&inventory, &buckets, &listing, limit);
    assert!(missing.is_empty());
    assert_eq!(listed, inventory.public().envelopes);
}

#[cfg(test)]
#[test]
pub fn replication_difference_counts_what_each_side_lacks() {
    let local = TestReplica(
        [
            test_envelope_ids("origin", 1..=2_000, "a"),
            test_envelope_ids("relay", 1..=10, "a"),
        ]
        .concat()
        .into_iter()
        .collect(),
    );
    let mut peer = TestReplica(
        [
            test_envelope_ids("origin", 1..=1_500, "a"),
            test_envelope_ids("relay", 1..=10, "a"),
            test_envelope_ids("newcomer", 1..=5, "a"),
            // A second candidate at one writer sequence inside a range both sides hold.
            test_envelope_ids("origin", [700], "b"),
        ]
        .concat()
        .into_iter()
        .collect(),
    );
    let (inventory, buckets) = local.inventory();
    let difference = |remote: &ReplicationInventory| -> Option<(u64, u64)> {
        replication_inventory_difference(&inventory, &buckets, remote)
    };
    // The newcomer's five and the fork are only on the peer; origin 1501..=2000 only here.
    assert_eq!(difference(&peer.summary()), Some((6, 500)));
    let (_, listed) = peer.answer(&local.summary());
    assert_eq!(difference(&listed), Some((6, 500)));

    // Equal range sizes with different members: the summary can only bound the difference, and
    // the peer's listing makes it exact.
    peer.0.remove(&test_envelope_ids("relay", [3], "a")[0]);
    peer.0.extend(test_envelope_ids("relay", [11], "a"));
    assert_eq!(difference(&peer.summary()), Some((6, 500)));
    let (_, listed) = peer.answer(&local.summary());
    assert_eq!(difference(&listed), Some((7, 501)));

    // An older peer lists its whole inventory, and so does a peer with an empty store.
    let full = peer.inventory().0.public();
    assert_eq!(difference(&full), Some((7, 501)));
    let empty = ReplicationInventory {
        digest: replication_inventory_digest(&[]),
        ..ReplicationInventory::default()
    };
    assert_eq!(
        difference(&empty),
        Some((0, inventory.envelopes.len() as u64))
    );

    // A matching digest needs nothing else; a bare different digest cannot tell.
    assert_eq!(difference(&local.summary()), Some((0, 0)));
    let bare = ReplicationInventory {
        digest: peer.summary().digest,
        ..ReplicationInventory::default()
    };
    assert_eq!(difference(&bare), None);
}

#[cfg(test)]
#[test]
pub fn sync_progress_estimates_catch_up_from_net_progress() {
    let mut progress = PeerSyncProgress::default();
    progress.observe(0, Some((10_000, 0)), 1_000);
    let sync = progress.view(1_000).unwrap();
    assert!(sync.catching_up);
    assert_eq!(sync.estimated_catch_up_seconds, None, "no rate sample yet");

    // One full window: 1,000 envelopes in 10 seconds.
    progress.observe(500, Some((9_500, 0)), 6_000);
    progress.observe(500, Some((9_000, 2)), 11_000);
    let sync = progress.view(11_000).unwrap();
    assert_eq!(sync.local_only_envelopes, 2);
    assert_eq!(sync.receive_rate_per_second, Some(100.0));
    assert_eq!(sync.catch_up_rate_per_second, Some(100.0));
    assert_eq!(sync.estimated_catch_up_seconds, Some(90));

    // The peer keeps writing, so only half of what arrives closes the gap.
    progress.observe(1_000, Some((8_500, 0)), 21_000);
    let sync = progress.view(21_000).unwrap();
    assert_eq!(sync.receive_rate_per_second, Some(100.0));
    assert_eq!(sync.catch_up_rate_per_second, Some(75.0));
    assert_eq!(sync.estimated_catch_up_seconds, Some(114));

    // A receipt whose inventory could not be measured keeps the last measurement.
    progress.observe(5, None, 22_000);
    assert_eq!(progress.view(22_000).unwrap().peer_only_envelopes, 8_500);

    // One exchange carries the rest, so this is no longer catching up.
    progress.observe(
        0,
        Some((REPLICATION_EXCHANGE_ENVELOPE_LIMIT as u64, 0)),
        23_000,
    );
    assert!(!progress.view(23_000).unwrap().catching_up);

    // A measurement from a peer that went quiet stops claiming the node is behind.
    progress.observe(0, Some((5_000, 0)), 24_000);
    assert!(progress.view(24_000).unwrap().catching_up);
    assert!(
        !progress
            .view(24_000 + REPLICATION_SYNC_STALE_MS + 1)
            .unwrap()
            .catching_up
    );
    progress.observe(0, Some((0, 0)), 30_000);
    assert_eq!(
        progress.view(30_000).unwrap().estimated_catch_up_seconds,
        Some(0)
    );
}

pub fn collect_referenced_blobs(
    connection: &Connection,
    claims: &[ClaimRecord],
    output: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let mut hashes = BTreeSet::new();
    for claim in claims {
        collect_hash_fields(&claim.body, &mut hashes);
    }
    for hash in hashes {
        if let Some(bytes) = connection
            .query_row("SELECT bytes FROM blobs WHERE hash=?1", [&hash], |row| {
                row.get(0)
            })
            .optional()?
        {
            output.insert(hash, bytes);
        }
    }
    Ok(())
}

/// Keep uncommitted uploads outside the shared graph. Promotion is part of the claim's
/// transaction, so a failed write cannot change the shared blob digest.
pub fn promote_claim_blobs(transaction: &Transaction<'_>, body: &Value) -> Result<()> {
    let mut hashes = BTreeSet::new();
    collect_hash_fields(body, &mut hashes);
    for hash in hashes {
        transaction.execute(
            "INSERT OR IGNORE INTO blobs(hash,bytes,size)
             SELECT hash,bytes,size FROM local_blobs WHERE hash=?1",
            [&hash],
        )?;
        transaction.execute("DELETE FROM local_blobs WHERE hash=?1", [&hash])?;
    }
    Ok(())
}

/// Older put_blob calls mixed staged uploads with authority. Separate only bytes we can
/// prove were never shared: retained claim references and received blob records stay shared.
/// After a historical checkpoint removed those references, preserve all existing authority.
pub fn separate_staged_blobs(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    let done: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='staged_blob_scope')",
        [],
        |row| row.get(0),
    )?;
    if done {
        return Ok(());
    }
    let trimmed: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM checkpoint_claims)",
        [],
        |row| row.get(0),
    )?;
    if !trimmed {
        let mut hashes = BTreeSet::new();
        let mut statement = transaction.prepare("SELECT body FROM claims")?;
        for body in statement.query_map([], |row| row.get::<_, String>(0))? {
            collect_hash_fields(&serde_json::from_str::<Value>(&body?)?, &mut hashes);
        }
        drop(statement);
        transaction
            .execute_batch("CREATE TEMP TABLE staged_blob_shared_hashes(hash TEXT PRIMARY KEY);")?;
        let mut insert =
            transaction.prepare("INSERT OR IGNORE INTO staged_blob_shared_hashes VALUES(?1)")?;
        for hash in hashes {
            insert.execute([hash])?;
        }
        drop(insert);
        transaction.execute_batch(
            "INSERT OR IGNORE INTO staged_blob_shared_hashes SELECT hash FROM documents;
             INSERT OR IGNORE INTO staged_blob_shared_hashes
                 SELECT substr(subject_hint,6) FROM replica_records WHERE kind_hint='blob' AND state='valid';
             INSERT OR IGNORE INTO local_blobs SELECT * FROM blobs
                 WHERE hash NOT IN (SELECT hash FROM staged_blob_shared_hashes);
             DELETE FROM blobs WHERE hash NOT IN (SELECT hash FROM staged_blob_shared_hashes);
             DROP TABLE staged_blob_shared_hashes;",
        )?;
    }
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES('staged_blob_scope','1')",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn collect_hash_fields(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "hash" | "blob_hash" | "bundle_hash")
                    && let Some(hash) = value.as_str().filter(|hash| hash.len() == 64)
                {
                    output.insert(hash.into());
                }
                collect_hash_fields(value, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_hash_fields(value, output);
            }
        }
        _ => {}
    }
}

pub fn ensure_claim_blobs(transaction: &Connection, claim: &ClaimRecord) -> Result<(), St3Error> {
    let mut hashes = BTreeSet::new();
    collect_hash_fields(&claim.body, &mut hashes);
    for hash in hashes {
        let exists = transaction
            .query_row("SELECT 1 FROM blobs WHERE hash=?1", [&hash], |_| Ok(()))
            .optional()
            .map_err(internal)?
            .is_some();
        if !exists {
            return Err(St3Error::new(
                "missing-replicated-blob",
                format!("claim `{}` references missing blob `{hash}`", claim.id),
            ));
        }
    }
    Ok(())
}

pub fn validate_and_admit_envelope_tx(
    transaction: &Connection,
    envelope: &ReplicaEnvelope,
    runtime: &dyn Runtime,
    outcome: &mut ReplicationAdmission,
) -> Result<(), St3Error> {
    let started = std::time::Instant::now();
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|error| {
            St3Error::new(
                "invalid-envelope-payload",
                format!("the envelope payload is not valid base64: {error}"),
            )
        })?;
    let expected_hash = replica_envelope_hash(
        &envelope.writer,
        envelope.sequence,
        envelope.previous_hash.as_deref(),
        envelope.accepted_at_unix_ms,
        &payload_bytes,
    );
    if expected_hash != envelope.hash {
        return Err(St3Error::new(
            "envelope-hash-mismatch",
            "the envelope hash does not match its payload",
        ));
    }
    let payload: ReplicaEnvelopePayload =
        ciborium::from_reader(payload_bytes.as_slice()).map_err(|error| {
            St3Error::new(
                "invalid-envelope-payload",
                format!("the envelope payload is not valid CBOR: {error}"),
            )
        })?;
    let batch = &payload.batch;
    if batch.origin != envelope.writer
        || batch.replica_sequence != envelope.sequence
        || batch.previous_hash != envelope.previous_hash
        || batch.accepted_at_unix_ms != envelope.accepted_at_unix_ms
    {
        return Err(St3Error::new(
            "envelope-batch-mismatch",
            "the envelope and its batch identify different writes",
        ));
    }
    verify_replica_batch_header(batch)?;
    outcome.verify += started.elapsed();
    let now = now_ms().to_string();
    let mut degraded = false;
    for (offset, (hash, bytes)) in payload.blobs.iter().enumerate() {
        let position = batch.claims.len() as u64 + offset as u64;
        let record_ref = replica_record_ref(
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
            position,
        );
        let valid = hex::encode(Sha256::digest(bytes)) == *hash;
        let previous_state = transaction
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record_ref],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?;
        if valid {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO blobs(hash, bytes, size) VALUES (?1, ?2, ?3)",
                    params![hash, bytes, bytes.len() as u64],
                )
                .map_err(internal)?;
            transaction
                .execute(
                    "INSERT INTO replica_records(
                         record_ref, writer, sequence, envelope_hash, position, raw, state,
                         subject_hint, kind_hint, updated_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'valid', ?7, 'blob', ?8)
                     ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'valid' END,
                        subject_hint=excluded.subject_hint,
                            kind_hint='blob',
                            error_code=CASE WHEN state='repaired' THEN error_code ELSE NULL END,
                            error_message=CASE WHEN state='repaired' THEN error_message ELSE NULL END,
                        updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![
                        record_ref,
                        envelope.writer,
                        envelope.sequence,
                        envelope.hash,
                        position,
                        bytes,
                        format!("blob/{hash}"),
                        now_ms().to_string(),
                    ],
                )
                .map_err(internal)?;
            outcome.valid += 1;
            outcome.changed |= previous_state.as_deref() != Some("valid");
        } else {
            degraded = true;
            transaction
                .execute(
                    "INSERT INTO replica_records(
                         record_ref, writer, sequence, envelope_hash, position, raw, state,
                         subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'invalid', ?7, 'blob',
                               'blob-hash-mismatch', ?8, ?9)
                     ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'invalid' END,
                        subject_hint=excluded.subject_hint, kind_hint='blob', error_code=excluded.error_code,
                        error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![
                        record_ref,
                        envelope.writer,
                        envelope.sequence,
                        envelope.hash,
                        position,
                        bytes,
                        format!("blob/{hash}"),
                        format!("replicated blob `{hash}` failed verification"),
                        now_ms().to_string(),
                    ],
                )
                .map_err(internal)?;
            outcome.invalid += 1;
        }
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                batch.id,
                batch.origin,
                batch.replica_sequence,
                batch.previous_hash,
                batch.hash,
                batch.accepted_at_unix_ms.to_string(),
            ],
        )
        .map_err(internal)?;
    for (position, claim) in batch.claims.iter().enumerate() {
        let position = position as u64;
        let record_ref = replica_record_ref(
            &envelope.writer,
            envelope.sequence,
            &envelope.hash,
            position,
        );
        let mut raw = Vec::new();
        ciborium::into_writer(claim, &mut raw).map_err(internal)?;
        let previous_state = transaction
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record_ref],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?;
        let started = std::time::Instant::now();
        let classification = validate_replicated_claim(transaction, batch, claim, runtime);
        outcome.verify += started.elapsed();
        match classification {
            Ok(ReplicatedClaimAdmission::Valid) => {
                // A checkpoint dropped this claim here, and another envelope carries it again.
                // A node that still held it would ignore the copy as already stored.
                let inserted = if checkpoint::claim_tombstoned(transaction, &claim.id)
                    .map_err(internal)?
                {
                    0
                } else {
                    transaction
                    .execute(
                        "INSERT OR IGNORE INTO claims(id, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            claim.id,
                            claim.batch_id,
                            claim.subject,
                            claim.kind,
                            claim.origin,
                            claim.actor,
                            canonical_json_text(&claim.body).map_err(internal)?,
                            serde_json::to_string(&claim.predecessors).map_err(internal)?,
                            claim.accepted_at_unix_ms.to_string(),
                        ],
                    )
                    .map_err(internal)?
                };
                if let Some(signature) = payload.claim_signatures.get(&claim.id) {
                    principals::store_claim_signature_tx(transaction, &claim.id, signature)
                        .map_err(internal)?;
                }
                transaction
                    .execute(
                        "INSERT INTO replica_records(
                             record_ref, writer, sequence, envelope_hash, position, raw, state,
                             claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'valid', ?7, ?8, ?9, NULL, NULL, ?10)
                         ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'valid' END,
                            claim_id=excluded.claim_id,
                            subject_hint=excluded.subject_hint, kind_hint=excluded.kind_hint,
                            error_code=CASE WHEN state='repaired' THEN error_code ELSE NULL END,
                            error_message=CASE WHEN state='repaired' THEN error_message ELSE NULL END,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                        params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, now],
                    )
                    .map_err(internal)?;
                outcome.valid += 1;
                outcome.changed |= inserted != 0 || previous_state.as_deref() != Some("valid");
            }
            Ok(
                classification @ (ReplicatedClaimAdmission::UnknownKind
                | ReplicatedClaimAdmission::UnknownField),
            ) => {
                let (error_code, error_message) = match classification {
                    ReplicatedClaimAdmission::UnknownKind => (
                        "unknown-claim-kind",
                        "this st build does not know the claim kind",
                    ),
                    ReplicatedClaimAdmission::UnknownField => (
                        "unknown-claim-field",
                        "this st build does not know every field on the claim kind",
                    ),
                    ReplicatedClaimAdmission::Valid => unreachable!(),
                };
                transaction
                    .execute(
                        "INSERT INTO replica_records(
                             record_ref, writer, sequence, envelope_hash, position, raw, state,
                             claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'unknown', ?7, ?8, ?9, ?10, ?11, ?12)
                         ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'unknown' END,
                            error_code=CASE WHEN state='repaired' THEN error_code ELSE excluded.error_code END,
                            error_message=CASE WHEN state='repaired' THEN error_message ELSE excluded.error_message END,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                        params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, error_code, error_message, now],
                    )
                    .map_err(internal)?;
                outcome.unknown += 1;
            }
            Err(error) => {
                degraded = true;
                transaction
                    .execute(
                        "INSERT INTO replica_records(
                             record_ref, writer, sequence, envelope_hash, position, raw, state,
                             claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'invalid', ?7, ?8, ?9, ?10, ?11, ?12)
                         ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'invalid' END,
                            error_code=excluded.error_code, error_message=excluded.error_message,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                        params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, error.code, error.message, now],
                    )
                    .map_err(internal)?;
                outcome.invalid += 1;
            }
        }
    }
    transaction
        .execute(
            "UPDATE replica_envelopes SET receipt_state=?4, validation_error=NULL, batch_id=?5
             WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
            params![
                envelope.writer,
                envelope.sequence,
                envelope.hash,
                if degraded { "degraded" } else { "validated" },
                batch.id,
            ],
        )
        .map_err(internal)?;
    Ok(())
}

impl Store {
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn index(&self) -> Result<u64> {
        Ok(self.committed_index.load(Ordering::Acquire))
    }

    /// Run `read` with every read this thread makes through the store seeing one SQLite
    /// snapshot, and give it that snapshot's store index. Rows read inside always match the
    /// index, however many commits land meanwhile. A nested call joins the outer snapshot.
    /// The latest claim of a subject, or of one kind of it, in canonical order.
    pub fn latest_claim(&self, subject: &str, kind: Option<&str>) -> Result<Option<ClaimRecord>> {
        crate::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        // With a kind, walk the accepted-time index newest first and sort only claims accepted in
        // the same millisecond, as `newest_claims_of_kind_query` does. Sorting every claim of the
        // kind made the reconciler's once-per-pass checks of every settled stop cost a pass 70 ms.
        let query = if kind.is_some() {
            latest_claim_of_kind_query()
        } else {
            format!(
                "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.subject=?1 AND ?2 IS NULL ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
            )
        };
        connection
            .prepare_cached(&query)?
            .query_row(params![subject, kind], claim_from_row)
            .optional()
            .map_err(Into::into)
    }

    /// Append a claim this node writes, through the runtime that knows its kind.
    pub fn append_claim(&self, input: &ClaimInput) -> Result<ClaimRecord, St3Error> {
        self.append_claim_outcome(input).map(|(claim, _)| claim)
    }

    /// Append a claim this node writes, and say whether it is new rather than an idempotent
    /// repeat. Every local write from a client's input comes through here.
    pub fn append_claim_outcome(
        &self,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        if let Some(actor) = &input.actor {
            self.ensure_principal_key(actor)?;
        }
        let appended = self.runtime.append_claim(self, input);
        if input.kind == crate::rules::RULE_SET {
            self.rules_stale.store(true, Ordering::Release);
        }
        appended
    }

    pub fn read_snapshot<T>(&self, read: impl FnOnce(u64) -> Result<T>) -> Result<T> {
        let key = self.readers.key();
        if PINNED_READER.with(|slot| slot.borrow().as_ref().is_some_and(|(pool, _)| *pool == key)) {
            let index = current_index(&self.readers.get())?;
            return read(index);
        }
        let mut guard = self.readers.get();
        // Declared first so it drops last: on every exit it ends the transaction, releases the
        // pin, and returns the connection to the pool.
        let pinned = PinnedRead {
            pool: &self.readers,
            connection: Some(Rc::new(
                guard
                    .connection
                    .take()
                    .expect("an unpinned read guard holds a pooled connection"),
            )),
        };
        drop(guard);
        let connection = pinned
            .connection
            .clone()
            .expect("the pin holds its connection");
        connection.execute_batch("BEGIN")?;
        // The first read starts the snapshot; every later read in `read` sees the same one.
        let index = current_index(&connection)?;
        PINNED_READER.with(|slot| *slot.borrow_mut() = Some((key, connection)));
        read(index)
    }

    pub fn put_document(
        &self,
        name: &str,
        bytes: &[u8],
        expected_document: &Option<String>,
        idempotency_key: &str,
    ) -> Result<DocumentVersion, St3Error> {
        self.put_document_as(name, bytes, expected_document, idempotency_key, None)
    }

    /// Store a document for `writer`, whom the rules judge as the publisher. The binding claim
    /// itself is the node's, as every document binding is.
    pub fn put_document_as(
        &self,
        name: &str,
        bytes: &[u8],
        expected_document: &Option<String>,
        idempotency_key: &str,
        writer: Option<&str>,
    ) -> Result<DocumentVersion, St3Error> {
        validate_document_name(name)?;
        if bytes.len() > 1024 * 1024 {
            return Err(St3Error::new(
                "document-too-large",
                "one document cannot exceed 1 MiB",
            ));
        }
        std::str::from_utf8(bytes).map_err(|_| {
            St3Error::new("document-not-text", "a document must contain valid UTF-8")
        })?;
        let hash = hex::encode(Sha256::digest(bytes));
        self.connection
            .batched(|transaction| -> Result<DocumentVersion, St3Error> {
                if let Some(response) = transaction
                    .query_row(
                        "SELECT response FROM idempotency WHERE operation_id=?1",
                        [opaque_cache_key(idempotency_key)],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(internal)?
                {
                    return serde_json::from_str(&response).map_err(internal);
                }
                if let Some(version) = find_document(transaction, name, &hash).map_err(internal)? {
                    return Ok(version);
                }
                let current: Option<String> = transaction
                    .query_row(
                        &canonical_sql("SELECT documents.binding_claim_id FROM documents WHERE documents.name=?1 ORDER BY binding_key DESC LIMIT 1"),
                        [name],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if &current != expected_document {
                    return Err(St3Error::new(
                        "stale-document-token",
                        format!("the selected binding for `{name}` changed before the post"),
                    )
                    .with_detail("subject", name.to_owned())
                    .with_detail("expected_head", json!(expected_document))
                    .with_detail("current_head", json!(current)));
                }
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO blobs(hash, bytes, size) VALUES (?1, ?2, ?3)",
                        params![hash, bytes, bytes.len() as u64],
                    )
                    .map_err(internal)?;
                if let Some(writer) = writer {
                    principals::rules_gate_tx(transaction, &self.origin, writer, "doc.bound", name)
                        .map_err(crate::error::typed)?;
                }
                let body = json!({ "name": name, "hash": hash, "size": bytes.len() });
                let record = self.runtime.append_claim_tx(
                    transaction,
                    &self.origin,
                    name,
                    "doc.bound",
                    None,
                    &body,
                    &[],
                    None,
                )
                .map_err(internal)?;
                select_replicated_document(transaction, &record, record.store_index)?;
                let version = DocumentVersion {
                    name: name.into(),
                    hash,
                    size: bytes.len() as u64,
                    created_index: record.store_index,
                    latest: true,
                    binding_claim_id: record.id,
                    created_at_unix_ms: record.accepted_at_unix_ms,
                    owner: record.actor.or(Some(record.origin)),
                };
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                        params![
                            opaque_cache_key(idempotency_key),
                            serde_json::to_string(&version).map_err(internal)?
                        ],
                    )
                    .map_err(internal)?;
                Ok(version)
            })
            .map_err(|error| St3Error::new("internal", error))?
    }

    pub fn get_document(&self, name: &str, hash: &str) -> Result<Option<Vec<u8>>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT b.bytes FROM documents d JOIN blobs b ON b.hash=d.hash WHERE d.name=?1 AND d.hash=?2",
                params![name, hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_documents(
        &self,
        name: Option<&str>,
        history: bool,
        limit: usize,
    ) -> Result<Vec<DocumentVersion>> {
        self.list_documents_page(name, None, history, None, limit)
    }

    pub fn list_documents_page(
        &self,
        name: Option<&str>,
        prefix: Option<&str>,
        history: bool,
        after: Option<(&str, u64)>,
        limit: usize,
    ) -> Result<Vec<DocumentVersion>> {
        let connection = self.readers.get();
        let query = "SELECT d.name, d.hash, b.size, d.created_index,
                     d.hash=(SELECT n.hash FROM documents n
                 WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1)
                     ,d.binding_claim_id, c.accepted_at_unix_ms, c.actor, c.origin
                     FROM documents d JOIN blobs b ON b.hash=d.hash
                     JOIN claims c ON c.id=d.binding_claim_id
                     WHERE (?1 IS NULL OR d.name=?1)
                       AND (?2 OR d.hash=(SELECT n.hash FROM documents n
                 WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1))
                       AND (?3 IS NULL OR substr(d.name,1,length(?3))=?3)
                       AND (?4 IS NULL OR d.name>?4 OR (d.name=?4 AND EXISTS (
                           SELECT 1 FROM documents cursor_document
                           WHERE cursor_document.name=?4 AND cursor_document.created_index=?5
                             AND cursor_document.binding_key>d.binding_key)))
                     ORDER BY d.name, d.binding_key DESC LIMIT ?6";
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map(
            params![
                name,
                history,
                prefix,
                after.map(|v| v.0),
                after.map(|v| v.1),
                limit
            ],
            |row| {
                Ok(DocumentVersion {
                    name: row.get(0)?,
                    hash: row.get(1)?,
                    size: row.get(2)?,
                    created_index: row.get(3)?,
                    latest: row.get::<_, i64>(4)? != 0,
                    binding_claim_id: row.get(5)?,
                    created_at_unix_ms: row.get::<_, String>(6)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    owner: row.get::<_, Option<String>>(7)?.or(row.get(8)?),
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn claim_by_id(&self, id: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
                 FROM claims WHERE id=?1",
                [id],
                claim_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The acceptance time of each of `ids` that names a claim, as `claim_by_id` reads it, in
    /// one statement.
    pub fn claim_acceptance_times(&self, ids: &[&str]) -> Result<BTreeMap<String, u128>> {
        let connection = self.readers.get();
        connection
            .prepare_cached(
                "SELECT id, accepted_at_unix_ms FROM claims
                 WHERE id IN (SELECT value FROM json_each(?1))",
            )?
            .query_map([serde_json::to_string(ids)?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?.parse().unwrap_or_default(),
                ))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(Into::into)
    }

    pub fn claims_for(&self, subject: &str, kind: Option<&str>) -> Result<Vec<ClaimRecord>> {
        crate::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        // One statement with `(?2 IS NULL OR kind=?2)` hides the kind from the planner, which then
        // reads every claim of the subject to test it. A busy agent holds thousands of observations,
        // and the reconciler asks for one kind of each agent's claims on every pass.
        let rows = match kind {
            Some(kind) => connection
                .prepare_cached(&claims_for_subject_query(true))?
                .query_map(params![subject, kind], claim_from_row)?
                .collect::<Result<Vec<_>, _>>(),
            None => connection
                .prepare_cached(&claims_for_subject_query(false))?
                .query_map([subject], claim_from_row)?
                .collect::<Result<Vec<_>, _>>(),
        };
        rows.map_err(Into::into)
    }

    pub fn latest_document_hash(&self, name: &str) -> Result<Option<String>> {
        crate::touched::note_read(|| name.to_owned());
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql("SELECT documents.hash FROM documents WHERE documents.name=?1 ORDER BY binding_key DESC LIMIT 1"),
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn document_bindings(
        &self,
        references: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, String>> {
        self.document_bindings_at(references, None)
    }

    pub fn document_bindings_at(
        &self,
        references: &BTreeSet<String>,
        at_index: Option<u64>,
    ) -> Result<BTreeMap<String, String>> {
        let connection = self.readers.get();
        let current = current_index(&connection)?;
        let selected = selected_index(current, at_index).map_err(anyhow::Error::new)?;
        let mut bindings = BTreeMap::new();
        for reference in references
            .iter()
            .filter(|reference| !reference.contains('@'))
        {
            if let Some(hash) = connection
                .query_row(
                    &canonical_sql("SELECT documents.hash FROM documents WHERE documents.name=?1 AND documents.created_index<=?2 ORDER BY binding_key DESC LIMIT 1"),
                    params![reference, selected],
                    |row| row.get(0),
                )
                .optional()?
            {
                bindings.insert(reference.clone(), hash);
            }
        }
        Ok(bindings)
    }

    pub fn latest_document_token(&self, name: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql("SELECT documents.binding_claim_id FROM documents WHERE documents.name=?1 ORDER BY binding_key DESC LIMIT 1"),
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex::encode(Sha256::digest(bytes));
        let connection = self.connection.write();
        connection.execute(
            "INSERT OR IGNORE INTO local_blobs(hash, bytes, size)
             SELECT ?1, ?2, ?3 WHERE NOT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
            params![hash, bytes, bytes.len() as u64],
        )?;
        Ok(hash)
    }

    /// Record that `actor` uploaded `size` bytes of `hash`, for the per-actor quota and for who
    /// may fetch them before a message names them. Rows older than `ttl_ms` are dropped first.
    /// The same bytes uploaded again by the same actor cost nothing. The bytes themselves are
    /// not here: they are files the daemon owns, and never part of replication.
    pub fn record_blob_upload(
        &self,
        actor: &str,
        hash: &str,
        media_type: &str,
        size: u64,
        quota: u64,
        ttl_ms: u64,
    ) -> Result<(), St3Error> {
        let now = now_ms() as u64;
        let connection = self.connection.write();
        let result = (|| -> rusqlite::Result<Result<(), St3Error>> {
            connection.execute(
                "DELETE FROM local_blob_uploads WHERE uploaded_ms<?1",
                [now.saturating_sub(ttl_ms)],
            )?;
            let already: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM local_blob_uploads WHERE hash=?1 AND actor=?2)",
                params![hash, actor],
                |row| row.get(0),
            )?;
            if !already {
                let held: u64 = connection.query_row(
                    "SELECT COALESCE(SUM(size),0) FROM local_blob_uploads WHERE actor=?1",
                    [actor],
                    |row| row.get(0),
                )?;
                if held.saturating_add(size) > quota {
                    return Ok(Err(St3Error::new(
                        "blob-quota-exceeded",
                        format!(
                            "{actor} already holds {held} bytes of uploads; the limit is {quota}"
                        ),
                    )));
                }
            }
            connection.execute(
                "INSERT INTO local_blob_uploads(hash, actor, media_type, size, uploaded_ms)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(hash, actor) DO UPDATE SET uploaded_ms=excluded.uploaded_ms",
                params![hash, actor, media_type, size, now],
            )?;
            Ok(Ok(()))
        })();
        match result {
            Ok(outcome) => outcome,
            Err(error) => Err(internal(error)),
        }
    }

    /// The type `actor` gave when it uploaded these bytes, if it did within the retention window.
    pub fn blob_upload_media_type(&self, hash: &str, actor: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT media_type FROM local_blob_uploads WHERE hash=?1 AND actor=?2",
                params![hash, actor],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Forget uploads older than `ttl_ms`; the daemon deletes the files in the same sweep.
    pub fn expire_blob_uploads(&self, ttl_ms: u64) -> Result<usize> {
        let cutoff = (now_ms() as u64).saturating_sub(ttl_ms);
        let connection = self.connection.write();
        Ok(connection.execute("DELETE FROM local_blob_uploads WHERE uploaded_ms<?1", [cutoff])?)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT bytes FROM blobs WHERE hash=?1
                        UNION ALL SELECT bytes FROM local_blobs WHERE hash=?1 LIMIT 1",
                [hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn bind_fleet(&self, fleet_id: &str) -> Result<()> {
        let connection = self.connection.write();
        let stored = connection
            .query_row("SELECT value FROM meta WHERE key='fleet_id'", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if let Some(stored) = stored {
            anyhow::ensure!(
                stored == fleet_id,
                "this nonempty store belongs to fleet `{stored}`"
            );
            return Ok(());
        }
        connection.execute(
            "INSERT INTO meta(key, value) VALUES ('fleet_id', ?1)",
            [fleet_id],
        )?;
        Ok(())
    }

    pub fn replication_inventory(&self) -> Result<ReplicationInventory> {
        Ok(self.replication_snapshot()?.inventory.public())
    }

    /// The replication snapshot with every local batch sealed into a signed envelope first. The
    /// exchange paths use it; they must offer peers everything this node wrote.
    pub fn replication_snapshot(&self) -> Result<Arc<ReplicationSnapshot>> {
        // A write that lands between sealing and reading would be counted by the projection
        // digests but not by the inventory, and a peer comparing graphs at equal inventories
        // would heal for nothing. Seal again until the snapshot holds no unsealed batch.
        let mut snapshot;
        let mut attempts = 0;
        loop {
            self.seal_local_batches()?;
            snapshot = self.sealed_replication_snapshot()?;
            attempts += 1;
            if !snapshot.unsealed || attempts == 8 {
                break;
            }
        }
        // Judge the claims just sealed only after the snapshot is taken: a verdict pass takes the
        // writer, and writes queued behind it must not land between sealing and the snapshot.
        self.judge_claims(false)?;
        Ok(snapshot)
    }

    /// Seal this node's batches that have no envelope yet, and sign them. Only this takes the
    /// writer, and only when there are such batches.
    pub fn seal_local_batches(&self) -> Result<()> {
        // A checkpoint on a standalone busy member can accumulate ten minutes of writes.
        // Bound each writer loan, not just the scan range, so queued live writes run between
        // chunks. Capture the target once; concurrent writes belong to the next pass.
        const SEAL_CHUNK_BATCHES: usize = 64;
        let target = max_batch_rowid(&self.readers.get())?;
        while target > self.seeded_batch_rowid.load(Ordering::Acquire) {
            let mut connection = self.connection.write();
            let _timing = time_stage(&self.replication_timers.snapshot);
            let seeded_through = self.seeded_batch_rowid.load(Ordering::Acquire);
            if seeded_through >= target {
                break;
            }
            let through: i64 = connection.query_row(
                "SELECT COALESCE(MAX(rowid), ?2) FROM (
                    SELECT rowid FROM batches WHERE rowid>?1 AND rowid<=?2
                    ORDER BY rowid LIMIT ?3)",
                params![seeded_through, target, SEAL_CHUNK_BATCHES],
                |row| row.get(0),
            )?;
            let transaction = connection.transaction()?;
            seed_replica_envelopes_signed_tx(
                &transaction,
                &self.origin,
                Some(seeded_through),
                Some(through),
                Some(&|claim: &principals::Unsealed<'_>| self.sign_unsealed(claim)),
            )?;
            self.sign_own_envelopes_range_tx(&transaction, Some(seeded_through), Some(through))?;
            transaction.commit()?;
            self.seeded_batch_rowid.store(through, Ordering::Release);
            // The FIFO writer services any already queued request before the next loan.
        }
        Ok(())
    }

    /// The replication snapshot of the envelopes already sealed, built on a read connection. A
    /// read such as `st replication status` uses it and never waits for the writer; a batch
    /// written since the last exchange shows once the next exchange seals it.
    pub fn sealed_replication_snapshot(&self) -> Result<Arc<ReplicationSnapshot>> {
        let current = |store: &Self| -> Result<Option<Arc<ReplicationSnapshot>>> {
            let store_index = store.index()?;
            let replica_generation = store.replica_generation.load(Ordering::Acquire);
            // The store index never moves back, so deleting the newest claim leaves it
            // unchanged, and a projection or a replay writes no claims at all. The graph
            // generation moves with every change to a digested table, and sealing a batch adds
            // an envelope row.
            let reader = store.readers.get();
            let current_graph_generation = graph_generation(&reader)?;
            let current_projection_generation = projection_digest::generation(&reader)?;
            let envelope_rowid = max_envelope_rowid(&reader)?;
            Ok(store
                .replication_snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .filter(|snapshot| {
                    snapshot.store_index == store_index
                        && snapshot.replica_generation == replica_generation
                        && snapshot.graph_generation == current_graph_generation
                        && snapshot.projection_generation == current_projection_generation
                        && snapshot.max_envelope_rowid == envelope_rowid
                })
                .cloned())
        };
        if let Some(snapshot) = current(self)? {
            return Ok(snapshot);
        }
        let _building = self
            .replication_snapshot_build
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(snapshot) = current(self)? {
            return Ok(snapshot);
        }
        let _timing = time_stage(&self.replication_timers.snapshot);
        self.read_snapshot(|_| {
            let connection = self.readers.get();
            self.build_replication_snapshot(&connection)
        })
    }

    pub fn build_replication_snapshot(
        &self,
        connection: &Connection,
    ) -> Result<Arc<ReplicationSnapshot>> {
        let previous = self
            .replication_snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // The six-table digest stays on the legacy peer wire format. Its generation lets us
        // retain compatibility without rescanning those tables for unrelated source changes.
        let graph_generation = graph_generation(connection)?;
        let reusable_graph_digest = previous
            .as_ref()
            .filter(|previous| previous.graph_generation == graph_generation)
            .map(|previous| previous.legacy_graph_digest.clone());
        let envelope_count: usize =
            connection.query_row("SELECT COUNT(*) FROM replica_envelopes", [], |row| {
                row.get(0)
            })?;
        let tombstone_count: usize =
            connection.query_row("SELECT COUNT(*) FROM checkpoint_envelopes", [], |row| {
                row.get(0)
            })?;
        let full = |connection: &Connection| -> Result<_> {
            let (inventory, max_rowid) = full_compact_replication_inventory(connection)?;
            let buckets = inventory.buckets();
            Ok((inventory, max_rowid, buckets, Vec::new(), 0))
        };
        let (mut inventory, max_envelope_rowid, buckets, mut digest_prefixes, resume_from) =
            if let Some(previous) = previous {
                let mut statement = connection.prepare(
                    "SELECT rowid, writer, sequence, envelope_hash FROM replica_envelopes
                     WHERE rowid>?1 ORDER BY rowid",
                )?;
                let additions = statement
                    .query_map([previous.max_envelope_rowid], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            ReplicaEnvelopeId {
                                writer: row.get(1)?,
                                sequence: row.get(2)?,
                                hash: row.get(3)?,
                            },
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                if envelope_count == previous.envelope_rows + additions.len()
                    && tombstone_count == previous.tombstone_rows
                {
                    let mut max_rowid = previous.max_envelope_rowid;
                    // Most snapshots are owned only by this cache. Move their inventory
                    // into the successor so a graph write does not allocate and free
                    // every envelope ID. Keep the old snapshot intact for concurrent
                    // callers that still hold it.
                    let (mut inventory, mut buckets, digest_prefixes) =
                        match Arc::try_unwrap(previous) {
                            Ok(snapshot) => (
                                snapshot.inventory,
                                snapshot.buckets,
                                snapshot.digest_prefixes,
                            ),
                            Err(shared) => (
                                shared.inventory.clone(),
                                shared.buckets.clone(),
                                shared.digest_prefixes.clone(),
                            ),
                        };
                    let mut touched = BTreeSet::new();
                    for (rowid, identity) in additions {
                        max_rowid = max_rowid.max(rowid);
                        touched.insert((
                            identity.writer.clone(),
                            replication_bucket_start(identity.sequence),
                        ));
                        inventory.insert(identity);
                    }
                    // Only the ranges that gained an envelope need a new digest.
                    for (writer, start) in &touched {
                        let bucket = inventory.bucket(inventory.range(writer, *start));
                        match buckets.binary_search_by(|existing| {
                            (existing.writer.as_str(), existing.start)
                                .cmp(&(writer.as_str(), *start))
                        }) {
                            Ok(position) => buckets[position] = bucket,
                            Err(position) => buckets.insert(position, bucket),
                        }
                    }
                    // Every range before the first one that gained an envelope is unchanged, so
                    // the inventory digest resumes there instead of hashing every identity again.
                    let resume_from = touched
                        .iter()
                        .map(|(writer, start)| {
                            buckets.partition_point(|existing| {
                                (existing.writer.as_str(), existing.start)
                                    < (writer.as_str(), *start)
                            })
                        })
                        .min()
                        .unwrap_or(buckets.len());
                    (inventory, max_rowid, buckets, digest_prefixes, resume_from)
                } else {
                    full(connection)?
                }
            } else {
                full(connection)?
            };
        inventory.resume_digest(&buckets, &mut digest_prefixes, resume_from);
        // Envelope hashes already commit the complete payload (and chain metadata). The
        // inventory digest therefore commits the authority log without hex-encoding and hashing
        // every payload again on each graph change.
        let authority_digest = inventory.digest.clone();
        let legacy_graph_digest = match reusable_graph_digest {
            Some(digest) => digest,
            None => legacy_graph_digest(connection, self.runtime.legacy_digest_tables())?,
        };
        let projection_generation = projection_digest::generation(connection)?;
        let projection_digests = projection_digest::tables(connection)?;
        let graph_digest = projection_digest::root(&projection_digests);
        let snapshot = Arc::new(ReplicationSnapshot {
            store_index: current_index(connection)?,
            replica_generation: self.replica_generation.load(Ordering::Acquire),
            max_envelope_rowid,
            envelope_rows: envelope_count,
            tombstone_rows: tombstone_count,
            inventory,
            buckets,
            digest_prefixes,
            authority_digest,
            graph_generation,
            projection_generation,
            graph_digest,
            legacy_graph_digest,
            projection_digests,
            unsealed: connection
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM batches WHERE rowid>?1 AND NOT EXISTS(
                         SELECT 1 FROM replica_envelopes WHERE replica_envelopes.batch_id=batches.id))",
                )?
                .query_row([self.seeded_batch_rowid.load(Ordering::Acquire)], |row| {
                    row.get(0)
                })?,
        });
        *self
            .replication_snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub fn export_replication_summary(&self, fleet_id: &str) -> Result<ReplicationExchange> {
        let snapshot = self.replication_snapshot()?;
        let _timing = time_stage(&self.replication_timers.export);
        let signature_requests = self.replication_signature_requests()?;
        Ok(ReplicationExchange {
            peer: self.origin.clone(),
            fleet_id: fleet_id.to_owned(),
            schema_digest: self.runtime.schema_digest(),
            authority_digest: snapshot.authority_digest.clone(),
            graph_digest: snapshot.legacy_graph_digest.clone(),
            projection_digests: snapshot.projection_digests.clone(),
            inventory: ReplicationInventory {
                digest: snapshot.inventory.digest.clone(),
                envelopes: Vec::new(),
                buckets: snapshot.buckets.clone(),
                accepts: Some(replication_accepts()),
                checkpoint: self.trimmed_checkpoint()?,
            },
            envelopes: Vec::new(),
            signature_requests,
            signatures: Vec::new(),
        })
    }

    pub fn export_replication_exchange(
        &self,
        fleet_id: &str,
        remote: &ReplicationInventory,
    ) -> Result<ReplicationExchange> {
        self.export_replication_exchange_answering(fleet_id, remote, &[])
    }

    /// Export what the peer lacks, answer its signature requests, and ask for the signatures
    /// this node still needs.
    pub fn export_replication_exchange_answering(
        &self,
        fleet_id: &str,
        remote: &ReplicationInventory,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExchange> {
        let mut exchange = self.export_replication_difference(fleet_id, remote)?;
        let _timing = time_stage(&self.replication_timers.export);
        exchange.signatures = self.replication_signatures_for(signature_requests)?;
        exchange.signature_requests = self.replication_signature_requests()?;
        Ok(exchange)
    }

    pub fn export_replication_difference(
        &self,
        fleet_id: &str,
        remote: &ReplicationInventory,
    ) -> Result<ReplicationExchange> {
        let snapshot = self.replication_snapshot()?;
        let _timing = time_stage(&self.replication_timers.export);
        let same = !remote.digest.is_empty() && remote.digest == snapshot.inventory.digest;
        if !same && !remote.buckets.is_empty() {
            // The peer sent range digests, so only differing ranges need identities.
            let (missing, listed) = compact_replication_difference(
                &snapshot.inventory,
                &snapshot.buckets,
                remote,
                REPLICATION_EXCHANGE_ENVELOPE_LIMIT,
            );
            return Ok(ReplicationExchange {
                peer: self.origin.clone(),
                fleet_id: fleet_id.to_owned(),
                schema_digest: self.runtime.schema_digest(),
                authority_digest: snapshot.authority_digest.clone(),
                graph_digest: snapshot.legacy_graph_digest.clone(),
                projection_digests: snapshot.projection_digests.clone(),
                inventory: ReplicationInventory {
                    digest: snapshot.inventory.digest.clone(),
                    envelopes: listed,
                    buckets: snapshot.buckets.clone(),
                    accepts: Some(replication_accepts()),
                    checkpoint: self.trimmed_checkpoint()?,
                },
                envelopes: self.replica_envelopes(missing)?,
                signature_requests: Vec::new(),
                signatures: Vec::new(),
            });
        }
        // A peer without range digests (an older build, or an explicit full-inventory request)
        // still exchanges complete inventories.
        let remote_is_complete = if remote.digest.is_empty() {
            true
        } else {
            remote.digest == replication_inventory_digest(&remote.envelopes)
        };
        let missing = if same || !remote_is_complete {
            Vec::new()
        } else {
            // Only membership is needed. Borrow IDs from the request instead of
            // duplicating the remote's entire inventory on every divergent sync.
            let known = remote.envelopes.iter().collect::<BTreeSet<_>>();
            let inventory = &snapshot.inventory;
            inventory
                .envelopes
                .iter()
                .filter(|envelope| inventory.has_payload(envelope))
                .map(|envelope| inventory.identity(envelope))
                .filter(|identity| !known.contains(identity))
                .take(replication_page_limit(remote))
                .collect::<Vec<_>>()
        };
        Ok(ReplicationExchange {
            peer: self.origin.clone(),
            fleet_id: fleet_id.to_owned(),
            schema_digest: self.runtime.schema_digest(),
            authority_digest: snapshot.authority_digest.clone(),
            graph_digest: snapshot.legacy_graph_digest.clone(),
            projection_digests: snapshot.projection_digests.clone(),
            inventory: ReplicationInventory {
                accepts: Some(replication_accepts()),
                checkpoint: self.trimmed_checkpoint()?,
                ..if same {
                    ReplicationInventory {
                        digest: snapshot.inventory.digest.clone(),
                        ..Default::default()
                    }
                } else {
                    snapshot.inventory.public()
                }
            },
            envelopes: self.replica_envelopes(missing)?,
            signature_requests: Vec::new(),
            signatures: Vec::new(),
        })
    }

    pub fn replica_envelopes(
        &self,
        missing: Vec<ReplicaEnvelopeId>,
    ) -> Result<Vec<ReplicaEnvelope>> {
        let connection = self.readers.get();
        let mut envelopes = Vec::with_capacity(missing.len());
        for identity in missing {
            // A trim can delete the payload after the snapshot listed the identity. The
            // tombstone stays in the inventory and the peer never needs the envelope.
            let envelope = connection.query_row(
                "SELECT envelopes.previous_hash, envelopes.accepted_at_unix_ms, envelopes.payload,
                        (SELECT member_key FROM replica_envelope_signatures AS signatures
                         WHERE signatures.writer=envelopes.writer
                           AND signatures.sequence=envelopes.sequence
                           AND signatures.envelope_hash=envelopes.envelope_hash
                         ORDER BY member_key LIMIT 1),
                        (SELECT signature FROM replica_envelope_signatures AS signatures
                         WHERE signatures.writer=envelopes.writer
                           AND signatures.sequence=envelopes.sequence
                           AND signatures.envelope_hash=envelopes.envelope_hash
                         ORDER BY member_key LIMIT 1)
                 FROM replica_envelopes AS envelopes
                 WHERE envelopes.writer=?1 AND envelopes.sequence=?2 AND envelopes.envelope_hash=?3",
                params![identity.writer, identity.sequence, identity.hash],
                |row| {
                    let accepted_at = row.get::<_, String>(1)?;
                    Ok(ReplicaEnvelope {
                        writer: identity.writer.clone(),
                        sequence: identity.sequence,
                        previous_hash: row.get(0)?,
                        hash: identity.hash.clone(),
                        accepted_at_unix_ms: accepted_at.parse().unwrap_or_default(),
                        payload: row.get(2)?,
                        member_key: row.get(3)?,
                        signature: row.get(4)?,
                    })
                },
            )
            .optional()?;
            let Some(envelope) = envelope else {
                continue;
            };
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    pub fn receive_replication_exchange(
        &self,
        relay: &str,
        fleet_id: &str,
        input: &ReplicationExchange,
    ) -> Result<ReplicationReceipt, St3Error> {
        self.receive_replication_exchange_asking(relay, fleet_id, input, false)
    }

    /// Receive an exchange. `asks` says the exchange answers this node's own request, so the
    /// worker that made it heals with the peer when the receipt says so; an exchange the peer
    /// started leaves the heal to this node's own requests.
    pub fn receive_replication_exchange_asking(
        &self,
        relay: &str,
        fleet_id: &str,
        input: &ReplicationExchange,
        asks: bool,
    ) -> Result<ReplicationReceipt, St3Error> {
        if input.peer != relay {
            return Err(St3Error::new(
                "peer-label-mismatch",
                "the authenticated peer label does not match the exchange label",
            ));
        }
        if input.fleet_id != fleet_id {
            return Err(St3Error::new(
                "fleet-id-mismatch",
                "the peer belongs to another fleet",
            ));
        }
        let timing = time_stage(&self.replication_timers.receipt);
        // The envelopes wait as pending in a savepoint of the writer's next batch; admission and
        // projection take them from there.
        let (received, duplicate, signatures) = self
            .connection
            .batched(|transaction| -> Result<(usize, usize, usize), St3Error> {
                let mut received = 0;
                let mut duplicate = 0;
                let mut signatures = 0;
                let now = now_ms().to_string();
                for envelope in &input.envelopes {
                    // A checkpoint dropped this envelope here. Its tombstone already stands for it.
                    if checkpoint::envelope_tombstoned(
                        transaction,
                        &envelope.writer,
                        envelope.sequence,
                        &envelope.hash,
                    )
                    .map_err(internal)?
                    {
                        duplicate += 1;
                        continue;
                    }
                    if let (Some(member_key), Some(signature)) = (&envelope.member_key, &envelope.signature)
                    {
                        signatures += store_envelope_signature_tx(
                            transaction,
                            fleet_id,
                            &envelope.writer,
                            envelope.sequence,
                            &envelope.hash,
                            member_key,
                            signature,
                            &now,
                        )
                        .map_err(internal)?;
                    }
                    let inserted = transaction
                        .execute(
                            "INSERT OR IGNORE INTO replica_envelopes(
                                 writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms,
                                 payload, relay, receipt_state, received_at_unix_ms
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
                            params![
                                envelope.writer,
                                envelope.sequence,
                                envelope.hash,
                                envelope.previous_hash,
                                envelope.accepted_at_unix_ms.to_string(),
                                envelope.payload,
                                relay,
                                now,
                            ],
                        )
                        .map_err(internal)?;
                    if inserted == 0 {
                        duplicate += 1;
                    } else {
                        received += 1;
                    }
                }
                for signature in &input.signatures {
                    if checkpoint::envelope_tombstoned(
                        transaction,
                        &signature.writer,
                        signature.sequence,
                        &signature.hash,
                    )
                    .map_err(internal)?
                    {
                        continue;
                    }
                    signatures += store_envelope_signature_tx(
                        transaction,
                        fleet_id,
                        &signature.writer,
                        signature.sequence,
                        &signature.hash,
                        &signature.member_key,
                        &signature.signature,
                        &now,
                    )
                    .map_err(internal)?;
                }
                transaction
                    .execute(
                        "INSERT INTO replication_peers(peer, status, last_success_at_unix_ms, schema_digest,
                                                        authority_digest, graph_digest, updated_at_unix_ms)
                         VALUES (?1, 'up', ?2, ?3, ?4, ?5, ?2)
                         ON CONFLICT(peer) DO UPDATE SET status='up', last_success_at_unix_ms=excluded.last_success_at_unix_ms,
                            last_error=NULL, schema_digest=excluded.schema_digest,
                            authority_digest=excluded.authority_digest, graph_digest=excluded.graph_digest,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                        params![relay, now, input.schema_digest, input.authority_digest, input.graph_digest],
                    )
                    .map_err(internal)?;
                transaction.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    params![format!("peer_projection_digests:{relay}"),
                        serde_json::to_string(&input.projection_digests).map_err(internal)?],
                ).map_err(internal)?;
                transaction.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    params![format!("peer_projection_inventory:{relay}"), input.inventory.digest],
                ).map_err(internal)?;
                // A response to our own dial proves the outbound grants now permit it.
                // Incoming exchanges only prove the reverse route.
                if asks {
                    transaction.execute("DELETE FROM replication_refusals WHERE peer=?1", [relay]).map_err(internal)?;
                }
                Ok((received, duplicate, signatures))
            })
            .map_err(|error| St3Error::new("internal", error))??;
        drop(timing);
        self.replication_timers
            .exchanges
            .fetch_add(1, Ordering::Relaxed);
        self.replication_timers
            .envelopes_received
            .fetch_add(received as u64, Ordering::Relaxed);
        if received != 0 {
            self.replica_generation.fetch_add(1, Ordering::AcqRel);
        }
        let snapshot = self.replication_snapshot().map_err(internal)?;
        let difference = replication_inventory_difference(
            &snapshot.inventory,
            &snapshot.buckets,
            &input.inventory,
        );
        // Each graph projects the envelopes its node holds, so the digests are comparable only
        // while both nodes hold the same ones, and only once this node has projected them all:
        // nothing new arrived that still waits for admission, and no projection is deferred.
        let (local_graph, remote_graph) = if input.projection_digests.is_empty() {
            (
                snapshot.legacy_graph_digest.clone(),
                input.graph_digest.clone(),
            )
        } else {
            (
                snapshot.graph_digest.clone(),
                projection_digest::root(&input.projection_digests),
            )
        };
        // Registries from different builds may legitimately produce different projections.
        // Their complete wire logs remain comparable through the authority digest.
        let same_registry = input.schema_digest == self.runtime.schema_digest();
        let waiting: bool = self
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM replica_records WHERE state='unknown')",
                [],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let graph_equal = (same_registry
            && !waiting
            && !input.inventory.digest.is_empty()
            && input.inventory.digest == snapshot.inventory.digest
            && !input.graph_digest.is_empty()
            && received == 0
            && signatures == 0
            && !self.replication_projection_deferred())
        .then(|| remote_graph == local_graph);
        if !same_registry
            && input.inventory.digest == snapshot.inventory.digest
            && received == 0
            && signatures == 0
            && !self.replication_projection_deferred()
        {
            // Pending, invalid and signature-held envelopes cannot complete admission. Unknown
            // kinds alone are validated envelopes and can complete a mixed-build log sync.
            let unvalidated: bool = self
                .readers
                .get()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM replica_envelopes
                     WHERE receipt_state IN ('pending','degraded'))",
                    [],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            if !unvalidated {
                self.observe_first_sync_log(
                    relay,
                    snapshot.inventory.envelopes.len() as u64,
                    &snapshot.authority_digest,
                )
                .map_err(internal)?;
            }
        }
        // A first sync ends at its first comparison, and a difference there heals at once.
        let first_sync_differs = match graph_equal {
            Some(equal) => self
                .observe_first_sync(
                    relay,
                    equal,
                    snapshot.inventory.envelopes.len() as u64,
                    &local_graph,
                    &remote_graph,
                )
                .map_err(internal)?,
            None => false,
        };
        let now = now_ms();
        let mut sync = self
            .replication_sync
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let progress = sync.entry(relay.to_owned()).or_default();
        progress.observe(received, difference, now);
        if difference.is_some() {
            progress.measured_inventory_envelopes = snapshot.inventory.envelopes.len() as u64;
        }
        if !same_registry || waiting {
            // Retire a comparison from before a rolling upgrade or a newly waiting claim.
            progress.graph_compared_at_unix_ms = None;
            progress.graph_differs_since_unix_ms = None;
            progress.heal = None;
        }
        let heal = match graph_equal {
            Some(equal) => {
                progress.compare_graphs(equal, now);
                asks && progress.heal_due(now, first_sync_differs)
            }
            None => false,
        };
        drop(sync);
        Ok(ReplicationReceipt {
            received,
            duplicate,
            signatures,
            inventory: ReplicationInventory {
                digest: snapshot.inventory.digest.clone(),
                envelopes: Vec::new(),
                buckets: Vec::new(),
                accepts: None,
                checkpoint: None,
            },
            heal,
        })
    }

    pub fn validate_replication_backlog(&self) -> Result<ReplicationAdmission> {
        // Seed and sign local batches first, so local membership claims decide admission.
        self.replication_snapshot()?;
        // Admission lends the writer back between chunks, so two must not run at once and
        // admit the same pending envelopes.
        let _admitting = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _timing = time_stage(&self.replication_timers.admission);
        let (retry_hash_mismatches, envelopes) = {
            let connection = self.connection.write();
            // Builds before the insertion-order hash fallback rejected genuine claims from
            // 2026-09-16 as hash mismatches. Check those records once more, once.
            let retry_hash_mismatches = connection
                .query_row(
                    "SELECT 1 FROM meta WHERE key='legacy_claim_hash_retried'",
                    [],
                    |_| Ok(()),
                )
                .optional()?
                .is_none();
            let mut statement = connection.prepare(
                "WITH retry_ids AS (
                     SELECT writer, sequence, envelope_hash FROM replica_envelopes
                     WHERE receipt_state='pending'
                     UNION
                     SELECT writer, sequence, envelope_hash FROM replica_records
                     WHERE state='unknown'
                        OR (state='invalid' AND error_code='invalid-replicated-claim'
                            AND error_message LIKE '%violates unknown-claim-field:%')
                        OR (?1 AND state='invalid' AND error_code='claim-hash-mismatch')
                 )
                 SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                        envelopes.previous_hash, envelopes.accepted_at_unix_ms, envelopes.payload
                 FROM retry_ids JOIN replica_envelopes AS envelopes
                   ON envelopes.writer=retry_ids.writer AND envelopes.sequence=retry_ids.sequence
                  AND envelopes.envelope_hash=retry_ids.envelope_hash
                 ORDER BY envelopes.writer, envelopes.sequence, envelopes.envelope_hash",
            )?;
            let envelopes = statement
                .query_map([retry_hash_mismatches], |row| {
                    Ok(ReplicaEnvelope {
                        writer: row.get(0)?,
                        sequence: row.get(1)?,
                        hash: row.get(2)?,
                        previous_hash: row.get(3)?,
                        accepted_at_unix_ms: row.get::<_, String>(4)?.parse().unwrap_or_default(),
                        payload: row.get(5)?,
                        member_key: None,
                        signature: None,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (retry_hash_mismatches, envelopes)
        };
        let mut outcome = ReplicationAdmission::default();
        let mut membership = fleet_membership_tx(&self.connection.write())?;
        let mut pending = envelopes;
        // Admitting one envelope can admit a membership claim that decides another envelope,
        // so held envelopes get another pass whenever membership changes.
        loop {
            let mut held = Vec::new();
            let mut membership_changed = false;
            // One transaction, and so one disk flush, per chunk. A catch-up page holds thousands
            // of envelopes, and admitting them in one transaction held the only writer for
            // seconds, so every write behind it waited. Between chunks the writer serves what
            // queued meanwhile. Each envelope is admitted in its own savepoint, so an invalid
            // one is rolled back and recorded alone.
            for chunk in pending.chunks(ADMISSION_CHUNK_ENVELOPES) {
                let mut connection = self.connection.write();
                let mut pass = connection.transaction()?;
                for envelope in chunk {
                    let started = std::time::Instant::now();
                    let hold = fleet_admission_hold(&pass, &membership, envelope)?;
                    outcome.verify += started.elapsed();
                    if let Some(reason) = hold {
                        hold_replica_envelope(&pass, envelope, reason)?;
                        held.push(envelope.clone());
                        continue;
                    }
                    let mut savepoint = pass.savepoint()?;
                    let result = validate_and_admit_envelope_tx(
                        &savepoint,
                        envelope,
                        &*self.runtime,
                        &mut outcome,
                    );
                    match result {
                        Ok(()) => {
                            savepoint.execute(
                                "DELETE FROM replica_envelope_holds
                                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
                                params![envelope.writer, envelope.sequence, envelope.hash],
                            )?;
                            membership_changed |=
                                envelope_carries_fleet_claims(&savepoint, envelope)?;
                            savepoint.commit()?;
                        }
                        Err(error) => {
                            savepoint.rollback()?;
                            drop(savepoint);
                            record_invalid_replica_envelope(&pass, envelope, &error)?;
                            outcome.invalid += 1;
                        }
                    }
                }
                if outcome.changed {
                    self.defer_replication_projection();
                    // Persist with the admission commit: a backup reader in another process
                    // must distinguish the admitted log from a projection still catching up.
                    pass.execute(
                        "INSERT OR REPLACE INTO meta(key,value) VALUES('replication_admitted_index',?1)",
                        [current_index_tx(&pass)?.to_string()],
                    )?;
                }
                pass.commit()?;
                #[cfg(any(test, feature = "test-support"))]
                ADMISSION_TRANSACTIONS.with(|count| count.set(count.get() + 1));
            }
            if !membership_changed || held.is_empty() {
                outcome.held = held.len();
                break;
            }
            membership = fleet_membership_tx(&self.connection.write())?;
            pending = held;
        }
        self.replication_timers
            .verify
            .fetch_add(outcome.verify.as_nanos() as u64, Ordering::Relaxed);
        if retry_hash_mismatches {
            self.connection.write().execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('legacy_claim_hash_retried', ?1)",
                [now_ms().to_string()],
            )?;
        }
        // Admitted claims, and any change to membership's trust roots, get their verdicts once
        // they are projected: judging here would hold the writer between admission and
        // projection, and a snapshot taken in between would offer an inventory its projections
        // do not yet show. A replicated rule takes effect here.
        if outcome.changed {
            self.rules_stale.store(true, Ordering::Release);
            self.verdicts_due.store(true, Ordering::Release);
        }
        Ok(outcome)
    }

    /// Project admitted replicated claims, unless this node is catching up with a peer and
    /// projected within the last `CATCH_UP_PROJECTION_INTERVAL_MS`. History arrives older than
    /// this node's own claims, so a catching-up node cannot extend its projection and would
    /// replay the whole graph after every exchange. It projects at most once per interval
    /// instead, and again as soon as it has caught up. `None` means it deferred.
    pub fn project_replication_backlog_unless_catching_up(&self) -> Result<Option<bool>> {
        let since = (now_ms() as u64).saturating_sub(
            self.last_replication_projection_unix_ms
                .load(Ordering::Acquire),
        );
        if since < CATCH_UP_PROJECTION_INTERVAL_MS && self.replication_catching_up() {
            self.defer_replication_projection();
            return Ok(None);
        }
        self.project_replication_backlog().map(Some)
    }

    /// Whether admitted replicated claims wait for a deferred projection.
    pub fn replication_projection_deferred(&self) -> bool {
        self.replication_projection_state.load(Ordering::Acquire) & 1 != 0
    }

    fn defer_replication_projection(&self) {
        let _ = self.replication_projection_state.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |state| Some(state.wrapping_add(2) | 1),
        );
    }

    /// Replay the graph from nothing now, as a heal does when two nodes project different graphs
    /// from the same claims.
    pub fn replay_replication_graph(&self) -> Result<()> {
        crate::profile::note("projection: full replay for a heal");
        let _replay = crate::profile::span("projection/heal-replay");
        let mut connection = self.connection.write();
        let _timing = time_stage(&self.replication_timers.projection);
        let transaction = connection.transaction()?;
        self.runtime.replay_from_nothing(&transaction)?;
        self.runtime.after_projection(&transaction)?;
        transaction.execute(
            "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
             VALUES ('graph', 'healthy', ?1, ?2)
             ON CONFLICT(aggregate) DO UPDATE SET status='healthy', last_good_store_index=excluded.last_good_store_index,
                error_code=NULL, error_message=NULL, updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![current_index_tx(&transaction)?, now_ms().to_string()],
        )?;
        transaction.commit()?;
        drop(connection);
        self.runtime.forget_views();
        Ok(())
    }

    pub fn project_replication_backlog(&self) -> Result<bool> {
        self.project_replication_backlog_chunks(|| {}, || {})
    }

    /// Exercise reads and queued writes between committed projection chunks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn project_replication_backlog_with_yield(&self, between: impl FnMut()) -> Result<bool> {
        self.project_replication_backlog_chunks(between, || {})
    }

    /// Force an admission or deferral after the final index read, before clearing its state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn project_replication_backlog_before_clear(&self, before_clear: impl FnMut()) -> Result<bool> {
        self.project_replication_backlog_chunks(|| {}, before_clear)
    }

    fn project_replication_backlog_chunks(
        &self,
        mut between: impl FnMut(),
        mut before_clear: impl FnMut(),
    ) -> Result<bool> {
        let _projecting = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _timing = time_stage(&self.replication_timers.projection);
        self.defer_replication_projection();
        self.last_replication_projection_unix_ms
            .store(now_ms() as u64, Ordering::Release);
        // Finish the backlog observed at entry. New receives can admit more between chunks;
        // their own projection pass will take those up without keeping this one alive forever.
        let target = self.index()?;
        let mut chunked = false;
        loop {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            let frontier: u64 = transaction
                .query_row(
                    "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
                    [],
                    |row| row.get::<_, Option<u64>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(0);
            let through = transaction
                .query_row(
                    "SELECT MAX(store_index) FROM (
                SELECT store_index FROM claims WHERE store_index>?1 AND store_index<=?2
                ORDER BY store_index LIMIT ?3)",
                    params![frontier, target, PROJECTION_CHUNK_CLAIMS as i64],
                    |row| row.get::<_, Option<u64>>(0),
                )?
                .unwrap_or(target.max(frontier));
            let result = (|| -> Result<bool, St3Error> {
                // An incremental projection that fails is rolled back and replaced by a full replay,
                // which quarantines the claim it cannot project instead of failing the graph.
                transaction
                    .execute_batch("SAVEPOINT project_incremental")
                    .map_err(internal)?;
                let incremental = crate::profile::span("projection/incremental");
                let projected =
                    match self
                        .runtime
                        .project_incremental(&transaction, &self.origin, through)
                    {
                        Ok(projected) => {
                            transaction
                                .execute_batch("RELEASE project_incremental")
                                .map_err(internal)?;
                            projected
                        }
                        Err(error) => {
                            crate::profile::note(&format!(
                                "replay: incremental failed: {}",
                                error.code
                            ));
                            transaction
                                .execute_batch(
                                    "ROLLBACK TO project_incremental; RELEASE project_incremental",
                                )
                                .map_err(internal)?;
                            false
                        }
                    };
                drop(incremental);
                if !projected {
                    #[cfg(any(test, feature = "test-support"))]
                    FULL_REPLAYS.with(|replays| replays.set(replays.get() + 1));
                    let _replay = crate::profile::span("projection/full-replay");
                    crate::profile::note("projection: full replay");
                    self.runtime.replay_from_nothing(&transaction)?;
                } else {
                    crate::profile::note("projection: incremental");
                }
                self.runtime.after_projection(&transaction)?;
                Ok(!projected)
            })();
            match result {
                Ok(replayed) => {
                    // A fallback replay folds the whole log, including anything admitted after
                    // this pass started. An incremental chunk advances only its bounded frontier.
                    let through = if replayed {
                        current_index_tx(&transaction)?
                    } else {
                        through
                    };
                    transaction.execute(
                    "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
                     VALUES ('graph', 'healthy', ?1, ?2)
                     ON CONFLICT(aggregate) DO UPDATE SET status='healthy', last_good_store_index=excluded.last_good_store_index,
                        error_code=NULL, error_message=NULL, updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![through, now_ms().to_string()],
                )?;
                    transaction.commit()?;
                    // Snapshot while admission is excluded by the writer. Admission marks
                    // deferred before its commit, so sampling during one could otherwise
                    // mistake its not-yet-committed claims for an empty backlog.
                    let projection_state =
                        self.replication_projection_state.load(Ordering::Acquire);
                    drop(connection);
                    // Readers may have cached the preceding prefix at the same admitted store
                    // index. Its projection changed even when no additional claim arrived.
                    chunked |= through < target;
                    if replayed || chunked {
                        self.runtime.forget_views();
                    }
                    if through < target {
                        between();
                        continue;
                    }
                    if self.verdicts_due.swap(false, Ordering::AcqRel) {
                        self.judge_claims(true)?;
                    }
                    // Admission and catch-up deferral can run after this index read. Clear
                    // only the generation observed before it; a newer deferral must survive.
                    let pending = through < self.index()?;
                    before_clear();
                    if !pending {
                        let _ = self.replication_projection_state.compare_exchange(
                            projection_state,
                            projection_state & !1,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    return Ok(true);
                }
                Err(error) => {
                    transaction.rollback()?;
                    connection.execute(
                    "INSERT INTO projection_health(aggregate, status, error_code, error_message, updated_at_unix_ms)
                     VALUES ('graph', 'stale', ?1, ?2, ?3)
                     ON CONFLICT(aggregate) DO UPDATE SET status='stale', error_code=excluded.error_code,
                        error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![error.code, error.message, now_ms().to_string()],
                )?;
                    return Ok(false);
                }
            }
        }
    }

    /// A quiet peer wake does not need to replay the entire graph. A stale or
    /// missing projection still gets a retry, including after a failed wake.
    pub fn replication_projection_needs_recovery(&self) -> Result<bool> {
        let connection = self.readers.get();
        let status: Option<String> = connection
            .query_row(
                "SELECT status FROM projection_health WHERE aggregate='graph'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status.as_deref() != Some("healthy"))
    }

    /// Apply each recorded repair on its own. A repair that cannot be applied is recorded as an
    /// unhealthy projection that names it, so one bad repair never stops replication or startup.
    pub fn apply_replication_repairs(&self) -> Result<usize> {
        let mut connection = self.connection.write();
        let _timing = time_stage(&self.replication_timers.repair);
        let transaction = connection.transaction()?;
        let mut statement = transaction
            .prepare("SELECT id, body FROM claims WHERE kind='record.repaired' ORDER BY id")?;
        let repairs = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut changed = 0;
        for (repair_claim, body) in repairs {
            transaction.execute_batch("SAVEPOINT apply_repair")?;
            let result =
                apply_replication_repair_tx(&*self.runtime, &transaction, &repair_claim, &body);
            match result {
                Ok(applied) => {
                    transaction.execute_batch("RELEASE apply_repair")?;
                    changed += applied;
                }
                Err(error) => {
                    transaction.execute_batch("ROLLBACK TO apply_repair; RELEASE apply_repair")?;
                    transaction.execute(
                        "INSERT INTO projection_health(aggregate, status, error_code, error_message, updated_at_unix_ms)
                         VALUES (?1, 'stale', 'repair-failed', ?2, ?3)
                         ON CONFLICT(aggregate) DO UPDATE SET status='stale', error_code=excluded.error_code,
                            error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
                        params![
                            format!("repair:{repair_claim}"),
                            format!("repair {repair_claim}: {error:#}"),
                            now_ms().to_string()
                        ],
                    )?;
                }
            }
        }
        if changed != 0 {
            rebuild_operations_tx(&transaction)?;
        }
        transaction.commit()?;
        drop(connection);
        if changed != 0 {
            self.runtime.forget_views();
        }
        Ok(changed)
    }

    pub fn repair_replica_record(
        &self,
        record_ref: &str,
        replacement_claim_id: &str,
        reason: &str,
        actor: &str,
        idempotency_key: &str,
    ) -> Result<ClaimRecord, St3Error> {
        {
            let connection = self.connection.write();
            let state = connection
                .query_row(
                    "SELECT state FROM replica_records WHERE record_ref=?1",
                    [record_ref],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?
                .ok_or_else(|| {
                    St3Error::new(
                        "unknown-replica-record",
                        "the replica record does not exist",
                    )
                })?;
            if !matches!(state.as_str(), "invalid" | "unknown" | "repaired") {
                return Err(St3Error::new(
                    "record-does-not-need-repair",
                    "only an invalid or unknown replica record can be repaired",
                ));
            }
            let replacement_exists = connection
                .query_row(
                    "SELECT 1 FROM claims WHERE id=?1",
                    [replacement_claim_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some();
            if !replacement_exists {
                return Err(St3Error::new(
                    "unknown-replacement-claim",
                    "the replacement claim does not exist or is not valid",
                ));
            }
        }
        let record_id = record_ref.strip_prefix("record/").unwrap_or(record_ref);
        let claim = self.append_claim(&ClaimInput {
            subject: format!("repair/{record_id}"),
            kind: "record.repaired".into(),
            actor: Some(actor.into()),
            fields: BTreeMap::from([
                ("record".into(), Value::String(record_ref.into())),
                (
                    "replacement".into(),
                    Value::String(replacement_claim_id.into()),
                ),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(idempotency_key.into()),
        })?;
        self.apply_replication_repairs().map_err(internal)?;
        self.project_replication_backlog().map_err(internal)?;
        Ok(claim)
    }

    pub fn replica_records(&self, unresolved_only: bool) -> Result<Vec<ReplicaRecordView>> {
        let connection = self.readers.get();
        let filter = if unresolved_only {
            " WHERE state IN ('invalid','unknown')"
        } else {
            ""
        };
        let mut statement = connection.prepare(&format!(
            "SELECT record_ref, writer, sequence, envelope_hash, position, state, claim_id,
                    subject_hint, kind_hint, error_code, error_message, replacement_claim_id
             FROM replica_records{filter} ORDER BY writer, sequence, envelope_hash, position"
        ))?;
        Ok(statement
            .query_map([], |row| {
                Ok(ReplicaRecordView {
                    record_ref: row.get(0)?,
                    writer: row.get(1)?,
                    sequence: row.get(2)?,
                    envelope_hash: row.get(3)?,
                    position: row.get(4)?,
                    state: row.get(5)?,
                    claim_id: row.get(6)?,
                    subject: row.get(7)?,
                    kind: row.get(8)?,
                    error_code: row.get(9)?,
                    error_message: row.get(10)?,
                    replacement_claim_id: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn replica_record(&self, record_ref: &str) -> Result<Option<ReplicaRecordView>> {
        Ok(self
            .replica_records(false)?
            .into_iter()
            .find(|record| record.record_ref == record_ref))
    }

    pub fn record_peer_failure(&self, peer: &str, status: &str, error: &str) -> Result<bool> {
        if status == "refused" {
            self.connection.batched(|transaction| {
                transaction.execute(
                    "INSERT INTO replication_refusals(peer, reason, updated_at_unix_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(peer) DO UPDATE SET reason=excluded.reason, updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![peer, error, now_ms().to_string()],
                )
            }).map_err(anyhow::Error::msg)??;
            return Ok(true);
        }
        // Keep the existing storage and claim vocabulary for mixed-version fleets.
        // Unknown reachability projects as last-seen in current product views.
        let status = if status == "down" { "unknown" } else { status };
        // Every failure is evidence a person needs: it stays as the peer's last error until the
        // next exchange in either direction clears it, and its time is the row's update time.
        self.connection
            .batched(|transaction| {
                transaction.execute(
                    "INSERT INTO replication_peers(peer, status, last_error, updated_at_unix_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(peer) DO UPDATE SET last_error=excluded.last_error,
                        updated_at_unix_ms=excluded.updated_at_unix_ms",
                    params![peer, status, error, now_ms().to_string()],
                )
            })
            .map_err(anyhow::Error::msg)??;
        // An inbound exchange is just as good evidence of reachability as an outbound one.
        // Keep the last success during a short missed-exchange window, so a failed dial on
        // one side cannot flap a peer that is still exchanging in the other direction.
        let last_success = self.replication_peer_last_success(peer)?;
        let recent_exchange =
            last_success.is_some_and(|last| now_ms().saturating_sub(last) < PEER_UP_MS);
        if recent_exchange {
            return Ok(false);
        }
        // A peer may also have a fresh up observation without a matching peer-row success
        // (for example, after a worker restart). Its published status must get the same
        // missed-exchange grace period or one outbound timeout reverses it immediately. A row
        // success, once recorded, is the newer evidence: every exchange updates it, while the
        // observation changes only with the status.
        let recent_observation = last_success.is_none()
            && self
                .latest_claim(&format!("host/{peer}"), Some("transport.observed"))?
                .filter(|claim| {
                    claim.origin == self.origin && claim.body["fields"]["status"] == "up"
                })
                .and_then(|claim| claim.body["fields"]["last_success_at"].as_u64())
                .is_some_and(|last| now_ms().saturating_sub(u128::from(last)) < PEER_UP_MS);
        if recent_observation {
            return Ok(false);
        }
        if self
            .readers
            .get()
            .query_row(
                "SELECT status FROM replication_peers WHERE peer=?1",
                [peer],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            == Some(status)
        {
            return Ok(true);
        }
        self.connection
            .batched(|transaction| {
                transaction.execute(
                    "UPDATE replication_peers SET status=?2 WHERE peer=?1",
                    params![peer, status],
                )
            })
            .map_err(anyhow::Error::msg)??;
        Ok(true)
    }

    /// The transport links the fleet currently observes as up, as `(observer, observed)` node
    /// names: each observer's latest observation of each peer, from every replicated node.
    pub fn transport_links(&self) -> Result<Vec<(String, String)>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&canonical_sql(
            "SELECT origin, subject, json_extract(body, '$.fields.status') FROM (
                SELECT origin, subject, body,
                    ROW_NUMBER() OVER (PARTITION BY origin, subject ORDER BY CANONICAL_DESC(claims)) AS canonical_rank
                FROM claims WHERE kind='transport.observed'
             ) WHERE canonical_rank=1 ORDER BY origin, subject",
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut links = Vec::new();
        for row in rows {
            let (observer, subject, status) = row?;
            if let (Some(observed), Some("up")) = (subject.strip_prefix("host/"), status.as_deref())
            {
                links.push((observer, observed.to_owned()));
            }
        }
        Ok(links)
    }

    pub fn record_transport_observation(
        &self,
        peer: &str,
        status: &str,
        reason: Option<&str>,
        last_success_at: Option<u128>,
    ) -> Result<()> {
        // Keep route refusals local so older members can admit transport observations.
        let status = if matches!(status, "down" | "refused") {
            "unknown"
        } else {
            status
        };
        let reason = if status == "unknown" { None } else { reason };
        let subject = format!("host/{peer}");
        let already_current = self
            .latest_claim(&subject, Some("transport.observed"))?
            .and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/status")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(status);
        if already_current {
            return Ok(());
        }
        let mut fields = BTreeMap::from([
            ("status".into(), Value::String(status.to_owned())),
            ("protocol".into(), Value::String("http-replication".into())),
        ]);
        if let Some(reason) = reason {
            fields.insert("reason".into(), Value::String(reason.to_owned()));
        }
        let last_success_at = last_success_at
            .or_else(|| (status == "up").then(now_ms))
            .or(self.replication_peer_last_success(peer)?);
        if let Some(last_success_at) = last_success_at.and_then(|value| u64::try_from(value).ok()) {
            fields.insert("last_success_at".into(), Value::from(last_success_at));
        }
        self.append_claim(&ClaimInput {
            subject,
            kind: "transport.observed".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "replication-transport:{peer}:{status}:{}",
                now_ms()
            )),
        })?;
        Ok(())
    }

    /// A direct Fabric refusal is local route state; it does not assert that the member is away.
    pub fn replication_peer_refusal(&self, peer: &str) -> Result<Option<String>> {
        Ok(self
            .readers
            .get()
            .query_row(
                "SELECT reason FROM replication_refusals WHERE peer=?1",
                [peer],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// When this replica last exchanged records with `peer`. The peer row records every
    /// success, while the `transport.observed` claim changes only with the peer's status.
    pub fn replication_peer_last_success(&self, peer: &str) -> Result<Option<u128>> {
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT last_success_at_unix_ms FROM replication_peers WHERE peer=?1",
                [peer],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .and_then(|value| value.parse().ok()))
    }

    /// Whether `peer` counts as up now (see [`peer_up`]), and when it last exchanged.
    pub fn replication_peer_up(&self, peer: &str) -> Result<(bool, Option<u128>)> {
        let connection = self.readers.get();
        let (last_success, failed_since) = connection
            .query_row(
                "SELECT last_success_at_unix_ms, last_error IS NOT NULL
                 FROM replication_peers WHERE peer=?1",
                [peer],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?
            .unwrap_or_default();
        let last_success = last_success.and_then(|value| value.parse().ok());
        Ok((peer_up(last_success, failed_since, now_ms()), last_success))
    }

    /// Whether any peer's latest measurement says this node is catching up with it.
    pub fn replication_catching_up(&self) -> bool {
        let now = now_ms();
        self.replication_sync
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|progress| progress.view(now).is_some_and(|sync| sync.catching_up))
    }

    /// Whether any peer's comparisons say this node's graph has diverged from that peer's.
    pub fn replication_diverged(&self) -> bool {
        let now = now_ms();
        self.replication_sync
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|progress| progress.view(now).is_some_and(|sync| sync.diverged))
    }

    /// The latest sync measurement for each configured peer that has one.
    pub fn replication_peer_sync(
        &self,
        configured_peers: &[String],
    ) -> BTreeMap<String, ReplicationPeerSync> {
        self.replication_peer_sync_held(configured_peers, None)
    }

    /// The latest sync measurements, each with the envelopes this node gained since it when
    /// `held` gives how many this node holds now.
    pub fn replication_peer_sync_held(
        &self,
        configured_peers: &[String],
        held: Option<u64>,
    ) -> BTreeMap<String, ReplicationPeerSync> {
        let now = now_ms();
        let progress = self
            .replication_sync
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        configured_peers
            .iter()
            .filter_map(|peer| {
                let progress = progress.get(peer)?;
                let mut sync = progress.view(now)?;
                if let Some(held) = held {
                    sync.added_since_measured_envelopes =
                        held.saturating_sub(progress.measured_inventory_envelopes);
                }
                Some((peer.clone(), sync))
            })
            .collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn age_replication_peer_for_test(&self, peer: &str) {
        self.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE replication_peers SET last_success_at_unix_ms=?2 WHERE peer=?1",
                params![peer, now_ms().saturating_sub(86_400_000).to_string()],
            )
            .unwrap();
    }

    /// Count one of this node's requests to a peer, from send to response.
    pub fn record_replication_round_trip(&self, duration: std::time::Duration) {
        self.replication_timers
            .round_trip
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Where this process has spent replication time since it started.
    pub fn replication_timings(&self) -> ReplicationTimings {
        let timers = &self.replication_timers;
        let ms = |counter: &AtomicU64| counter.load(Ordering::Relaxed) / 1_000_000;
        ReplicationTimings {
            exchanges: timers.exchanges.load(Ordering::Relaxed),
            envelopes_received: timers.envelopes_received.load(Ordering::Relaxed),
            round_trip_ms: ms(&timers.round_trip),
            export_ms: ms(&timers.export),
            snapshot_ms: ms(&timers.snapshot),
            receipt_ms: ms(&timers.receipt),
            admission_ms: ms(&timers.admission),
            verify_ms: ms(&timers.verify),
            projection_ms: ms(&timers.projection),
            repair_ms: ms(&timers.repair),
            signing_ms: ms(&timers.signing),
            sqlite_ms: ms(&SQLITE_NANOS),
            commits: SQLITE_COMMITS.load(Ordering::Relaxed),
            commit_ms: ms(&SQLITE_COMMIT_NANOS),
        }
    }

    pub fn replication_status(
        &self,
        configured: bool,
        fleet_id: Option<&str>,
        configured_peers: &[String],
    ) -> Result<ReplicationStatus> {
        self.seal_local_batches()?;
        self.replication_status_sealed(configured, fleet_id, configured_peers)
    }

    /// Replication status from what is committed and sealed, without taking the writer: the
    /// status a person reads. A batch written since the last exchange counts once it is sealed.
    pub fn replication_status_sealed(
        &self,
        configured: bool,
        fleet_id: Option<&str>,
        configured_peers: &[String],
    ) -> Result<ReplicationStatus> {
        let snapshot = self.sealed_replication_snapshot()?;
        let connection = self.readers.get();
        // Projection caches include committed local batches before their envelopes are sealed.
        // Equal sealed inventories cannot diagnose those pending writes as peer divergence.
        let unsealed_local: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM batches WHERE rowid>?1 AND origin=?2)",
            params![self.seeded_batch_rowid.load(Ordering::Acquire), self.origin],
            |row| row.get(0),
        )?;
        let count = |state: &str| -> Result<u64> {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM replica_records WHERE state=?1",
                [state],
                |row| row.get(0),
            )?)
        };
        let waiting_claims = count("unknown")?;
        let registry_digest = self.runtime.schema_digest();
        let now = now_ms();
        let sync = self.replication_peer_sync_held(
            configured_peers,
            Some(snapshot.inventory.envelopes.len() as u64),
        );
        let mut peers = Vec::new();
        for peer in configured_peers {
            let (mut status, updated_at) = connection
                .query_row(
                    "SELECT status, last_success_at_unix_ms, last_error, schema_digest,
                                authority_digest, graph_digest, updated_at_unix_ms
                         FROM replication_peers WHERE peer=?1",
                    [peer],
                    |row| {
                        let updated_at = row.get::<_, String>(6)?.parse::<u128>().ok();
                        Ok((
                            ReplicationPeerStatus {
                                peer: peer.clone(),
                                status: row.get(0)?,
                                last_success_at_unix_ms: row
                                    .get::<_, Option<String>>(1)?
                                    .and_then(|value| value.parse().ok()),
                                last_error: row.get(2)?,
                                refusal_reason: None,
                                schema_digest: row.get(3)?,
                                authority_digest: row.get(4)?,
                                graph_digest: row.get(5)?,
                                projection_digests: BTreeMap::new(),
                                differing_tables: Vec::new(),
                                projection_comparison_waiting: false,
                                sync: None,
                                last_failure_at_unix_ms: None,
                            },
                            updated_at,
                        ))
                    },
                )
                .optional()?
                .unwrap_or((
                    ReplicationPeerStatus {
                        peer: peer.clone(),
                        status: "unknown".into(),
                        last_success_at_unix_ms: None,
                        last_error: None,
                        refusal_reason: None,
                        schema_digest: None,
                        authority_digest: None,
                        graph_digest: None,
                        projection_digests: BTreeMap::new(),
                        differing_tables: Vec::new(),
                        projection_comparison_waiting: false,
                        sync: None,
                        last_failure_at_unix_ms: None,
                    },
                    None,
                ));
            if matches!(
                status.status.as_str(),
                "up" | "unknown" | "down" | "last-seen"
            ) {
                // A failure stays recorded until the next exchange, so a frozen or vanished
                // peer shows its error and stops counting as up once it misses an exchange.
                let failed_since = status.last_error.is_some();
                status.status = if peer_up(status.last_success_at_unix_ms, failed_since, now) {
                    "up"
                } else {
                    "last-seen"
                }
                .into();
                status.last_failure_at_unix_ms = updated_at.filter(|_| failed_since);
                status.last_error = status.last_error.filter(|error| !error.is_empty());
            }
            let refusal = connection
                .query_row(
                    "SELECT reason FROM replication_refusals WHERE peer=?1",
                    [peer],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(reason) = refusal {
                status.status = "refused".into();
                status.refusal_reason = Some(reason);
                status.last_error = None;
            }
            status.sync = sync.get(peer).cloned();
            let encoded: Option<String> = connection
                .query_row(
                    "SELECT value FROM meta WHERE key=?1",
                    [format!("peer_projection_digests:{peer}")],
                    |row| row.get(0),
                )
                .optional()?;
            status.projection_digests = encoded
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or_default();
            if !status.projection_digests.is_empty() {
                status.graph_digest = Some(projection_digest::root(&status.projection_digests));
                let peer_inventory: Option<String> = connection
                    .query_row(
                        "SELECT value FROM meta WHERE key=?1",
                        [format!("peer_projection_inventory:{peer}")],
                        |row| row.get(0),
                    )
                    .optional()?;
                if peer_inventory.as_deref() == Some(snapshot.inventory.digest.as_str())
                    && !self.replication_projection_deferred()
                    && !unsealed_local
                    && status.schema_digest.as_deref() == Some(registry_digest.as_str())
                    && waiting_claims == 0
                {
                    status.differing_tables = projection_digest::differing(
                        &snapshot.projection_digests,
                        &status.projection_digests,
                    );
                }
            }
            status.projection_comparison_waiting = waiting_claims != 0
                || status
                    .schema_digest
                    .as_deref()
                    .is_some_and(|digest| digest != registry_digest);
            peers.push(status);
        }
        Ok(ReplicationStatus {
            configured,
            fleet_id: fleet_id.map(str::to_owned),
            authority_digest: snapshot.authority_digest.clone(),
            graph_digest: snapshot.graph_digest.clone(),
            projection_digests: snapshot.projection_digests.clone(),
            received_envelopes: connection.query_row(
                "SELECT COUNT(*) FROM replica_envelopes",
                [],
                |row| row.get(0),
            )?,
            pending_records: count("pending")?,
            valid_records: count("valid")?,
            unknown_records: waiting_claims,
            waiting_claims,
            invalid_records: count("invalid")?,
            repaired_records: count("repaired")?,
            unsigned_envelopes: connection.query_row(
                "SELECT COUNT(*) FROM replica_envelope_holds WHERE reason='unsigned'",
                [],
                |row| row.get(0),
            )?,
            fenced_envelopes: connection.query_row(
                "SELECT COUNT(*) FROM replica_envelope_holds WHERE reason='fenced'",
                [],
                |row| row.get(0),
            )?,
            checkpointed_envelopes: connection.query_row(
                "SELECT COUNT(*) FROM checkpoint_envelopes",
                [],
                |row| row.get(0),
            )?,
            unhealthy_projections: connection.query_row(
                "SELECT COUNT(*) FROM projection_health WHERE status<>'healthy'",
                [],
                |row| row.get(0),
            )?,
            unhealthy: connection
                .prepare(
                    "SELECT aggregate, status, error_code, error_message FROM projection_health
                     WHERE status<>'healthy' ORDER BY aggregate LIMIT 50",
                )?
                .query_map([], |row| {
                    Ok(crate::replication::UnhealthyProjection {
                        aggregate: row.get(0)?,
                        status: row.get(1)?,
                        error_code: row.get(2)?,
                        error_message: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?,
            peers,
            timings: self.replication_timings(),
            first_sync: self.first_sync()?,
            removed: None,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn export_replication(&self, after_sequence: u64) -> Result<ReplicationBatch> {
        let mut heads = self.replica_heads()?;
        heads.insert(self.origin.clone(), after_sequence);
        self.export_replication_for_heads(&heads)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replica_heads(&self) -> Result<BTreeMap<String, u64>> {
        let connection = self.readers.get();
        replica_heads(&connection)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn export_replication_for_heads(
        &self,
        heads: &BTreeMap<String, u64>,
    ) -> Result<ReplicationBatch> {
        let connection = self.readers.get();
        let mut batches_statement = connection.prepare(
            "SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms FROM batches
             ORDER BY origin, replica_sequence",
        )?;
        let headers = batches_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .filter_map(|row| match row {
                Ok(row) if row.2 > heads.get(&row.1).copied().unwrap_or(0) => Some(Ok(row)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .take(512)
            .collect::<Result<Vec<_>, _>>()?;
        let mut batches = Vec::new();
        let mut blobs = BTreeMap::new();
        for (id, origin, replica_sequence, previous_hash, hash, accepted_at) in headers {
            let mut claim_statement = connection.prepare(
                "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
                 FROM claims WHERE batch_id=?1 ORDER BY store_index",
            )?;
            let claims = claim_statement
                .query_map([&id], claim_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            collect_referenced_blobs(&connection, &claims, &mut blobs)?;
            batches.push(ReplicaBatch {
                id,
                origin,
                replica_sequence,
                previous_hash,
                hash,
                accepted_at_unix_ms: accepted_at.parse().unwrap_or_default(),
                claims,
            });
        }
        Ok(ReplicationBatch {
            peer: self.origin.clone(),
            replica_heads: replica_heads(&connection)?,
            batches,
            blobs,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn peer_cursor(&self, peer: &str) -> Result<u64> {
        let connection = self.readers.get();
        connection
            .query_row(
                "SELECT MAX(replica_sequence) FROM batches WHERE origin=?1",
                [peer],
                |row| row.get::<_, Option<u64>>(0),
            )
            .map(|value| value.unwrap_or(0))
            .map_err(Into::into)
    }
}

/// Name an actor by kind, as `person/NAME`, when it was given as a bare name.
pub fn normalize_actor(value: &str, default_kind: &str) -> String {
    if value.contains('/') {
        value.to_owned()
    } else {
        format!("{default_kind}/{value}")
    }
}

/// Subject `?1`'s newest claim of kind `?2` in canonical order. See [`Store::latest_claim`].
pub fn latest_claim_of_kind_query() -> String {
    format!(
        "SELECT {CLAIM_COLUMNS} FROM claims INDEXED BY claims_subject_kind_accepted_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind=?2 ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    )
}

pub fn claims_for_subject_query(kind: bool) -> String {
    let kind = if kind { " AND claims.kind=?2" } else { "" };
    format!(
        "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1{kind} ORDER BY {CANONICAL_ORDER}"
    )
}

pub fn split_document_ref(reference: &str) -> Result<(&str, &str), St3Error> {
    reference.rsplit_once('@').ok_or_else(|| {
        St3Error::new(
            "invalid-document-reference",
            format!("document reference `{reference}` has no hash"),
        )
    })
}

pub fn validate_document_name(name: &str) -> Result<(), St3Error> {
    if !name.starts_with("doc/")
        || name.len() <= 4
        || name.contains('@')
        || name.contains("..")
        || name.ends_with('/')
    {
        return Err(St3Error::new(
            "invalid-document-name",
            "a document name must use `doc/NAME` and cannot contain `@` or `..`",
        ));
    }
    Ok(())
}

pub fn find_document(
    connection: &Connection,
    name: &str,
    hash: &str,
) -> Result<Option<DocumentVersion>> {
    connection
        .query_row(
            "SELECT d.name, d.hash, b.size, d.created_index,
             d.hash=(SELECT n.hash FROM documents n
                 WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1),
             d.binding_claim_id, c.accepted_at_unix_ms, c.actor, c.origin
             FROM documents d JOIN blobs b ON b.hash=d.hash
             JOIN claims c ON c.id=d.binding_claim_id
             WHERE d.name=?1 AND d.hash=?2",
            params![name, hash],
            |row| {
                Ok(DocumentVersion {
                    name: row.get(0)?,
                    hash: row.get(1)?,
                    size: row.get(2)?,
                    created_index: row.get(3)?,
                    latest: row.get::<_, i64>(4)? != 0,
                    binding_claim_id: row.get(5)?,
                    created_at_unix_ms: row.get::<_, String>(6)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    owner: row.get::<_, Option<String>>(7)?.or(row.get(8)?),
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

pub fn opaque_cache_key(key: &str) -> String {
    format!(
        "cache/{}",
        hex::encode(Sha256::digest(
            [b"st3.idempotency-cache.v1\0".as_slice(), key.as_bytes()].concat()
        ))
    )
}

pub fn operation_id_for_key(key: &str) -> String {
    format!(
        "op/{}",
        hex::encode(Sha256::digest(
            [b"st3.idempotency.v1\0".as_slice(), key.as_bytes()].concat()
        ))
    )
}

pub fn claim_operation(input: &ClaimInput) -> Result<Option<(String, String)>, St3Error> {
    let Some(key) = input.idempotency_key.as_deref() else {
        return Ok(None);
    };
    if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
        return Err(St3Error::new(
            "invalid-idempotency-key",
            "an idempotency key must contain 1 through 512 non-control characters",
        ));
    }
    let operation_id = operation_id_for_key(key);
    let mut evidence = input.evidence.clone();
    evidence.sort();
    evidence.dedup();
    let fields = input
        .fields
        .iter()
        .map(|(name, value)| (name.clone(), canonical_json_value(value)))
        .collect::<BTreeMap<_, _>>();
    let request_digest = canonical_hash(&(
        "st3.claim-request.v1",
        &input.subject,
        &input.kind,
        &input.actor,
        fields,
        evidence,
        &input.expected_subject,
    ))
    .map_err(internal)?;
    Ok(Some((operation_id, request_digest)))
}

pub fn operation_parts(body: &Value) -> Option<(&str, &str)> {
    Some((
        body.pointer("/_operation/id")?.as_str()?,
        body.pointer("/_operation/request_digest")?.as_str()?,
    ))
}

pub fn operation_tx(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<(String, String, String)>> {
    connection
        .query_row(
            "SELECT request_digest, canonical_claim_id, state FROM operations WHERE id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
}

/// Refuse to write a claim for an operation whose claims a checkpoint dropped. A retry of the
/// same request learns the original claim ID; a different request is refused as an
/// idempotency mismatch or conflict, as it would be while the claim was stored.
pub fn checkpointed_operation_outcome(
    connection: &Connection,
    operation_id: &str,
    request_digest: &str,
) -> Result<(), St3Error> {
    let dropped = checkpoint::checkpointed_operation(connection, operation_id).map_err(internal)?;
    let Some((stored_digest, claim_id)) = dropped.first() else {
        return Ok(());
    };
    if dropped.iter().any(|(digest, _)| digest != stored_digest) {
        return Err(St3Error::new(
            "idempotency-conflict",
            "the replicated operation has conflicting requests",
        )
        .with_detail("operation_id", operation_id.to_owned()));
    }
    if stored_digest != request_digest {
        return Err(St3Error::new(
            "idempotency-mismatch",
            "the idempotency key already identifies a different request",
        )
        .with_detail("operation_id", operation_id.to_owned())
        .with_detail("stored_digest", stored_digest.clone())
        .with_detail("request_digest", request_digest.to_owned()));
    }
    Err(St3Error::new(
        "claim-checkpointed",
        "this request was already recorded, and a checkpoint has since dropped its claim",
    )
    .with_detail("operation_id", operation_id.to_owned())
    .with_detail("claim_id", claim_id.clone()))
}

pub fn expected_operations(
    connection: &Connection,
) -> Result<BTreeMap<String, (String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
         FROM claims
         WHERE json_extract(body, '$._operation.id') IS NOT NULL
           AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)
         ORDER BY id",
    )?;
    let claims = statement
        .query_map([], claim_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
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
    // Claims a checkpoint dropped still decide an operation's digest and state. An operation
    // left with no stored claim has no row, since the row must name a stored claim; a retry
    // finds its tombstones instead.
    let mut dropped = BTreeMap::<String, Vec<(String, String)>>::new();
    for (operation_id, request_digest, claim_id) in checkpoint::checkpointed_operations(connection)?
    {
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
            (operation_id, operation_row(&stored_claims, dropped))
        })
        .collect())
}

/// One operation's row from its stored claims and the claims a checkpoint dropped, each as
/// `(request digest, claim)`: `(request digest, canonical claim, state)`.
fn operation_row(
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

/// What `expected_operations` holds for one operation, read through the operation index.
pub fn expected_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<(String, String, String)>> {
    let mut statement = connection.prepare_cached(
        "SELECT id, body FROM claims
         WHERE json_extract(body, '$._operation.id')=?1
           AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)
         ORDER BY id",
    )?;
    let mut stored = Vec::new();
    for row in statement.query_map([operation_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (claim_id, body) = row?;
        let body: Value = serde_json::from_str(&body)?;
        if let Some((_, request_digest)) = operation_parts(&body) {
            stored.push((request_digest.to_owned(), claim_id));
        }
    }
    if stored.is_empty() {
        return Ok(None);
    }
    let dropped = checkpoint::checkpointed_operation(connection, operation_id)?
        .into_iter()
        .filter(|(_, claim_id)| !stored.iter().any(|(_, stored)| stored == claim_id))
        .collect();
    Ok(Some(operation_row(&stored, dropped)))
}

/// Bring the rows of `operation_ids` to what their claims say, and leave every other row alone.
/// Returns how many rows it changed.
pub fn repair_operations_tx(
    transaction: &Transaction<'_>,
    operation_ids: &[String],
) -> Result<usize> {
    let mut changed = 0;
    for operation_id in operation_ids {
        changed += match expected_operation(transaction, operation_id)? {
            Some((request_digest, canonical_claim_id, state)) => transaction.execute(
                "INSERT INTO operations(id, request_digest, canonical_claim_id, state)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET request_digest=excluded.request_digest,
                    canonical_claim_id=excluded.canonical_claim_id, state=excluded.state
                 WHERE request_digest IS NOT excluded.request_digest
                    OR canonical_claim_id IS NOT excluded.canonical_claim_id
                    OR state IS NOT excluded.state",
                params![operation_id, request_digest, canonical_claim_id, state],
            )?,
            None => transaction.execute("DELETE FROM operations WHERE id=?1", [operation_id])?,
        };
    }
    Ok(changed)
}

pub fn rebuild_operations_tx(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute("DELETE FROM operations", [])?;
    for (id, (request_digest, canonical_claim_id, state)) in expected_operations(transaction)? {
        transaction.execute(
            "INSERT INTO operations(id, request_digest, canonical_claim_id, state) VALUES (?1, ?2, ?3, ?4)",
            params![id, request_digest, canonical_claim_id, state],
        )?;
    }
    Ok(())
}

pub fn register_operation_tx(transaction: &Transaction<'_>, claim: &ClaimRecord) -> Result<bool> {
    let Some((operation_id, request_digest)) = operation_parts(&claim.body) else {
        return Ok(true);
    };
    let current = operation_tx(transaction, operation_id)?;
    match current {
        None => {
            // A checkpoint may have dropped earlier claims of this operation. They still count,
            // as `expected_operations` counts them: this claim is then not the first, and a
            // different request digest is a conflict.
            let dropped = checkpoint::checkpointed_operation(transaction, operation_id)?;
            let conflict = dropped.iter().any(|(digest, _)| digest != request_digest);
            let stored_digest = dropped
                .first()
                .map(|(digest, _)| digest.as_str())
                .filter(|digest| *digest < request_digest)
                .unwrap_or(request_digest);
            transaction.execute(
                "INSERT INTO operations(id, request_digest, canonical_claim_id, state) VALUES (?1, ?2, ?3, ?4)",
                params![
                    operation_id,
                    stored_digest,
                    claim.id,
                    if conflict { "conflict" } else { "active" }
                ],
            )?;
            Ok(dropped.is_empty())
        }
        Some((stored_digest, canonical, state)) if stored_digest == request_digest => {
            if claim.id < canonical {
                transaction.execute(
                    "UPDATE operations SET canonical_claim_id=?2 WHERE id=?1",
                    params![operation_id, claim.id],
                )?;
            }
            Ok(state == "active" && canonical == claim.id)
        }
        Some(_) => {
            transaction.execute(
                "UPDATE operations SET state='conflict' WHERE id=?1",
                [operation_id],
            )?;
            Ok(false)
        }
    }
}

pub fn graph_digest(connection: &Connection) -> Result<String> {
    Ok(projection_digest::root(&projection_digest::tables(
        connection,
    )?))
}

pub fn legacy_graph_digest(
    connection: &Connection,
    tables: &[runtime::LegacyDigestTable],
) -> Result<String> {
    #[cfg(any(test, feature = "test-support"))]
    GRAPH_DIGESTS_COMPUTED.with(|computed| computed.set(computed.get() + 1));
    let queries = tables
        .iter()
        .map(|(label, table, columns, order)| {
            (
                *label,
                format!(
                    "SELECT json_array({}) FROM {table} ORDER BY {order}",
                    columns.join(", ")
                ),
            )
        })
        .collect::<Vec<_>>();
    let queries = queries
        .iter()
        .map(|(label, query)| (*label, query.as_str()))
        .collect::<Vec<_>>();
    digest_queries(connection, &queries)
}

/// Bump `graph_generation` in the same transaction as any change to a digested column, so a
/// replication snapshot can reuse the graph digest across writes that change none of them.
pub fn create_graph_generation_triggers(
    connection: &Connection,
    tables: &[runtime::LegacyDigestTable],
) -> Result<()> {
    for (_, table, columns, _) in tables {
        let changed = columns
            .iter()
            .map(|column| format!("OLD.{column} IS NOT NEW.{column}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_graph_insert AFTER INSERT ON {table}
             BEGIN UPDATE graph_generation SET value=value+1 WHERE id=1; END;
             CREATE TRIGGER IF NOT EXISTS {table}_graph_update AFTER UPDATE ON {table}
             WHEN {changed}
             BEGIN UPDATE graph_generation SET value=value+1 WHERE id=1; END;
             CREATE TRIGGER IF NOT EXISTS {table}_graph_delete AFTER DELETE ON {table}
             BEGIN UPDATE graph_generation SET value=value+1 WHERE id=1; END;"
        ))?;
    }
    Ok(())
}

pub fn graph_generation(connection: &Connection) -> Result<i64> {
    Ok(
        connection.query_row("SELECT value FROM graph_generation WHERE id=1", [], |row| {
            row.get(0)
        })?,
    )
}

pub fn digest_queries(connection: &Connection, queries: &[(&str, &str)]) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"st3-logical-digest-v1\0");
    for (name, query) in queries {
        digest.update(name.as_bytes());
        digest.update([0]);
        let mut statement = connection.prepare(query)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let encoded: String = row.get(0)?;
            digest.update((encoded.len() as u64).to_be_bytes());
            digest.update(encoded.as_bytes());
        }
    }
    Ok(hex::encode(digest.finalize()))
}

pub fn select_replicated_document(
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
    created_index: u64,
) -> Result<(), St3Error> {
    let name = claim
        .body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-document-claim",
                format!("replicated document claim `{}` has no name", claim.id),
            )
        })?;
    let hash = claim
        .body
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-document-claim",
                format!("replicated document claim `{}` has no hash", claim.id),
            )
        })?;
    validate_document_name(name)?;
    let binding_key =
        canonical::sortable_key(&canonical::claim_key(transaction, &claim.id).map_err(internal)?);
    let previous: Option<String> = transaction
        .query_row(
            "SELECT binding_claim_id FROM documents WHERE name=?1 AND hash=?2",
            params![name, hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    if let Some(previous) = previous {
        if canonical::claim_key(transaction, &claim.id).map_err(internal)?
            < canonical::claim_key(transaction, &previous).map_err(internal)?
        {
            // The earliest binding also gives the version its arrival index, as a replay in
            // canonical order would, so the order bindings arrive in never matters.
            transaction
                .execute(
                    "UPDATE documents SET binding_claim_id=?3,binding_key=?4,created_index=?5
                     WHERE name=?1 AND hash=?2",
                    params![name, hash, claim.id, binding_key, created_index],
                )
                .map_err(internal)?;
        }
        return Ok(());
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO documents(name, hash, created_index, binding_claim_id, binding_key) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, hash, created_index, claim.id, binding_key],
        )
        .map_err(internal)?;
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Replica envelope identities this thread hashed into inventory digests.
    pub static INVENTORY_IDENTITIES_HASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Legacy graph digests this thread computed.
    pub static GRAPH_DIGESTS_COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Projections this thread replayed from nothing.
    pub static FULL_REPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Writer transactions this thread admitted replicated envelopes in.
    pub static ADMISSION_TRANSACTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn apply_replication_repair_tx(
    runtime: &dyn Runtime,
    transaction: &Transaction<'_>,
    repair_claim: &str,
    body: &str,
) -> Result<usize> {
    let body: Value = serde_json::from_str(body)?;
    let fields = body.get("fields").unwrap_or(&body);
    let Some(record_ref) = fields.get("record").and_then(Value::as_str) else {
        return Ok(0);
    };
    let Some(replacement) = fields.get("replacement").and_then(Value::as_str) else {
        return Ok(0);
    };
    let replacement_exists = transaction
        .query_row(
            "SELECT 1 FROM claims WHERE id=?1",
            [replacement],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !replacement_exists {
        return Ok(0);
    }
    let repaired_claim = transaction
        .query_row(
            "SELECT claim_id FROM replica_records WHERE record_ref=?1",
            [record_ref],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let changed = transaction.execute(
        "UPDATE replica_records SET state='repaired', replacement_claim_id=?2,
                error_code=NULL, error_message=NULL, updated_at_unix_ms=?3
             WHERE record_ref=?1 AND state IN ('valid','invalid','unknown','repaired')
               AND (replacement_claim_id IS NULL OR replacement_claim_id<>?2)",
        params![record_ref, replacement, now_ms().to_string()],
    )?;
    if let Some(repaired_claim) = repaired_claim {
        runtime.apply_repair_tx(transaction, &repaired_claim, replacement)?;
    }
    transaction.execute(
            "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
             VALUES (?1, 'healthy', ?2, ?3)
             ON CONFLICT(aggregate) DO UPDATE SET status='healthy',
                last_good_store_index=excluded.last_good_store_index, error_code=NULL,
                error_message=NULL, updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![format!("repair:{record_ref}:{repair_claim}"), current_index_tx(transaction)?, now_ms().to_string()],
        )?;
    // A repair that failed before and applies now is no longer unhealthy.
    transaction.execute(
        "DELETE FROM projection_health WHERE aggregate=?1",
        [format!("repair:{repair_claim}")],
    )?;
    Ok(changed)
}

/// Verify a replicated claim against its batch and its content hash, then ask the runtime
/// whether it knows the claim's kind and fields. A valid claim's blobs must be stored.
pub fn validate_replicated_claim(
    transaction: &Connection,
    batch: &ReplicaBatch,
    claim: &ClaimRecord,
    runtime: &dyn Runtime,
) -> Result<ReplicatedClaimAdmission, St3Error> {
    if claim.batch_id != batch.id || claim.origin != batch.origin {
        return Err(St3Error::new(
            "claim-batch-mismatch",
            format!(
                "replicated claim `{}` names another batch or writer",
                claim.id
            ),
        ));
    }
    if !claim_id_is_content_hash(claim).map_err(internal)? {
        return Err(St3Error::new(
            "claim-hash-mismatch",
            format!("replicated claim `{}` failed verification", claim.id),
        ));
    }
    let admission = runtime.classify_replicated_claim(transaction, batch, claim)?;
    if matches!(admission, ReplicatedClaimAdmission::Valid) {
        ensure_claim_blobs(transaction, claim)?;
    }
    Ok(admission)
}

/// The graph half of a claim this node writes: give it a batch (this node's next, or
/// `forced_batch`) and its content-hash ID, and store it in the log. The runtime validates the
/// claim before and projects it after.
#[allow(clippy::too_many_arguments)]
pub fn append_claim_record_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced_batch: Option<&str>,
) -> Result<ClaimRecord> {
    let now = write_time(transaction, origin)?;
    let batch_id = if let Some(batch) = forced_batch {
        batch.to_owned()
    } else {
        let sequence = next_replica_sequence(transaction, origin)?;
        let previous_hash = previous_batch_hash(transaction, origin)?;
        let hash = batch_header_hash(origin, sequence, previous_hash.as_deref(), now)?;
        let id = format!("batch/{origin}/{sequence}/{hash}");
        transaction.execute(
            "INSERT INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, origin, sequence, previous_hash, hash, now.to_string()],
        )?;
        id
    };
    let id = claim_hash(&batch_id, subject, kind, origin, actor, body, predecessors)?;
    let store_index = insert_claim(
        transaction,
        &id,
        &batch_id,
        subject,
        kind,
        origin,
        actor,
        body,
        predecessors,
        now,
    )?;
    // A device signed this claim before the node wrote it: keep the signature with it.
    principals::attach_expected_signature_tx(transaction, &id, subject, kind, actor)?;
    Ok(ClaimRecord {
        id,
        store_index,
        batch_id,
        subject: subject.into(),
        kind: kind.into(),
        origin: origin.into(),
        actor: actor.map(str::to_owned),
        operation_id: operation_parts(body).map(|(id, _)| id.to_owned()),
        request_digest: operation_parts(body).map(|(_, digest)| digest.to_owned()),
        body: body.clone(),
        predecessors: predecessors.to_vec(),
        accepted_at_unix_ms: now,
    })
}
