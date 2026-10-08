//! Failed agreement proofs are memoized before any scratch copy or replay is attempted.
use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    claim::ReplicaBatch,
    error::Error,
    store::{
        ReplicatedClaimAdmission,
        checkpoint::{DropPlan, SealedSet, claim_tombstone, drop_digest, retained_digest, sealed_digest},
        checkpoint_agreement::{
            CHECKPOINT_PROOF_BACKOFF_MS, CheckpointAction, CheckpointContext, DAY_MS,
            newest_seals,
        },
        runtime::{IncrementalProjection, LegacyDigestTable, Plain, Runtime},
    },
};
use std::{collections::BTreeMap, path::Path, sync::{Arc, atomic::{AtomicUsize, Ordering}}};

/// A real count reader paired with deliberately unsafe drop rules. Revision 2 fixes the rules
/// by retaining every note; revisions 0 and 1 have the same drop plan but distinct semantics.
#[derive(Default)]
struct CountRuntime {
    revision: AtomicUsize,
    replays: AtomicUsize,
}

impl Runtime for CountRuntime {
    fn migrate_schema(&self, connection: &Connection) -> Result<()> {
        Plain.migrate_schema(connection)
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        Plain.create_schema(connection)?;
        // The shared checkpoint source capture reads these projection-reference registries.
        // This note-count runtime has no desired, mission or document bindings.
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS desired(claim_id TEXT);
             CREATE TABLE IF NOT EXISTS mission_definitions(claim_id TEXT);
             CREATE TABLE IF NOT EXISTS mission_revisions(claim_id TEXT);
             CREATE TABLE IF NOT EXISTS documents(binding_claim_id TEXT);",
        )?;
        Ok(())
    }
    fn open_projections(&self, tx: &Transaction<'_>, shared_memory: bool) -> Result<()> {
        Plain.open_projections(tx, shared_memory)
    }
    fn schema_digest(&self) -> String { Plain.schema_digest() }
    fn classify_replicated_claim(&self, connection: &Connection, batch: &ReplicaBatch, claim: &ClaimRecord) -> Result<ReplicatedClaimAdmission, Error> {
        Plain.classify_replicated_claim(connection, batch, claim)
    }
    fn append_claim(&self, store: &Store, input: &ClaimInput) -> Result<(ClaimRecord, bool), Error> {
        Plain.append_claim(store, input)
    }
    fn apply_repair_tx(&self, tx: &Transaction<'_>, repaired: &str, replacement: &str) -> Result<()> {
        Plain.apply_repair_tx(tx, repaired, replacement)
    }
    #[allow(clippy::too_many_arguments)]
    fn append_claim_tx(&self, tx: &Transaction<'_>, origin: &str, subject: &str, kind: &str, actor: Option<&str>, body: &Value, predecessors: &[String], forced_batch: Option<&str>) -> Result<ClaimRecord> {
        Plain.append_claim_tx(tx, origin, subject, kind, actor, body, predecessors, forced_batch)
    }
    fn project_incremental(&self, tx: &Transaction<'_>, origin: &str, through: u64) -> Result<IncrementalProjection, Error> {
        Plain.project_incremental(tx, origin, through)
    }
    fn replay_from_nothing(&self, tx: &Transaction<'_>) -> Result<(), Error> {
        Plain.replay_from_nothing(tx)
    }
    fn after_projection(&self, tx: &Transaction<'_>) -> Result<(), Error> { Plain.after_projection(tx) }
    fn forget_views(&self) { Plain.forget_views(); }
    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] { Plain.digest_tables() }
    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] { Plain.legacy_digest_tables() }
    fn checkpoint_rules_digest(&self) -> String {
        format!("count-rules-{}", self.revision.load(Ordering::Relaxed))
    }
    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan {
        let claims = sealed.claims.iter().filter(|claim| {
            self.revision.load(Ordering::Relaxed) < 2 && claim.valid && claim.claim.kind == "example.note"
        }).map(claim_tombstone).collect::<Vec<_>>();
        let kept = sealed.claims.iter().filter(|claim| !claims.iter().any(|drop| drop.id == claim.claim.id));
        DropPlan {
            cut_unix_ms: sealed.cut_unix_ms,
            rules_digest: self.checkpoint_rules_digest(),
            sealed_envelopes: sealed.envelopes.len(),
            sealed_claims: sealed.claims.len(),
            sealed_digest: sealed_digest(sealed),
            drop_digest: drop_digest(&[], &claims),
            retained_digest: retained_digest(kept.map(|claim| claim.claim.id.as_str())),
            envelopes: Vec::new(), claims, by_kind: BTreeMap::new(),
        }
    }
    fn clear_checkpoint_projections(&self, tx: &Transaction<'_>) -> Result<()> {
        Plain.clear_checkpoint_projections(tx)
    }
    fn replay_checkpoint_projections(&self, tx: &Transaction<'_>) -> Result<()> {
        self.replays.fetch_add(1, Ordering::Relaxed);
        Plain.replay_checkpoint_projections(tx)
    }
    fn checkpoint_subject_answers(&self, connection: &Connection, subject: &str, _cut: u128) -> Result<Value> {
        let count: u64 = connection.query_row(
            "SELECT COUNT(*) FROM claims WHERE subject=?1 AND kind='example.note'",
            [subject], |row| row.get(0),
        )?;
        Ok(json!({"notes": count}))
    }
}

fn note(store: &Store) -> ClaimRecord {
    store.append_claim(&ClaimInput {
        subject: "note/count".into(), kind: "example.note".into(), actor: None,
        fields: BTreeMap::new(), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap()
}

fn context(scratch: &Path) -> CheckpointContext {
    CheckpointContext {
        now_unix_ms: (smallclaims::store::now_ms() / DAY_MS + 3) * DAY_MS + DAY_MS / 2,
        scratch: scratch.to_owned(), configured_peers: Vec::new(), reviewer: "person/operator".into(),
    }
}

fn failed(store: &Store, context: &CheckpointContext) {
    assert!(matches!(store.checkpoint_step(context).unwrap().as_slice(), [CheckpointAction::ProofFailed { .. }]));
}

fn diagnostics(store: &Store) -> Vec<String> {
    store.claims_for("daemon/alder", Some("daemon.diagnostic")).unwrap().iter()
        .filter(|claim| claim.body["fields"]["code"] == "checkpoint-proof-failed")
        .map(|claim| claim.body["fields"]["reason"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn unchanged_failed_input_is_not_copied_or_replayed_even_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let runtime = Arc::new(CountRuntime::default());
    let store = Store::open(&path, "alder", runtime.clone()).unwrap();
    note(&store);
    let mut context = context(&directory.path().join("scratch"));
    assert!(matches!(store.checkpoint_step(&context).unwrap().as_slice(), [CheckpointAction::Sealed { .. }]));
    failed(&store, &context);
    let before = diagnostics(&store);
    assert_eq!(before.len(), 1);
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 2);
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    // A later claim is outside the sealed proof, and must not invalidate its failure identity.
    note(&store);
    context.now_unix_ms += CHECKPOINT_PROOF_BACKOFF_MS;
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    // A scratch path that cannot be created proves that the copy phase is skipped, not merely
    // that its diagnostic is deduplicated. The persisted decision survives a new Store.
    let blocked = directory.path().join("not-a-directory");
    std::fs::write(&blocked, b"blocked").unwrap();
    context.scratch = blocked;
    drop(store);
    let store = Store::open(&path, "alder", runtime.clone()).unwrap();
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 2);
    assert_eq!(diagnostics(&store), before);
}

#[test]
fn changed_rules_retry_after_backoff_and_can_recover_without_suppressing_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(CountRuntime::default());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    note(&store);
    let mut context = context(directory.path());
    store.checkpoint_step(&context).unwrap();
    failed(&store, &context);
    runtime.revision.store(1, Ordering::Relaxed);
    assert!(matches!(store.checkpoint_step(&context).unwrap().as_slice(), [CheckpointAction::Sealed { .. }]));
    context.now_unix_ms += CHECKPOINT_PROOF_BACKOFF_MS - 1;
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 2);
    context.now_unix_ms += 1;
    failed(&store, &context);
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 4);
    let reasons = diagnostics(&store);
    assert_eq!(reasons.len(), 2);
    assert_ne!(reasons[0], reasons[1], "rule semantics are part of the diagnostic identity");
    // Returning to an already failed input must not forget the first failure.
    runtime.revision.store(0, Ordering::Relaxed);
    store.checkpoint_step(&context).unwrap();
    context.now_unix_ms += CHECKPOINT_PROOF_BACKOFF_MS;
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 4);
    runtime.revision.store(2, Ordering::Relaxed);
    store.checkpoint_step(&context).unwrap();
    assert!(matches!(store.checkpoint_step(&context).unwrap().as_slice(), [CheckpointAction::Verified { .. }]));
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 6);
    assert_eq!(diagnostics(&store), reasons);
}

#[test]
fn changed_admission_input_retries_after_backoff_even_with_the_same_seal_and_rules() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(CountRuntime::default());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    let first = note(&store);
    note(&store);
    let mut context = context(directory.path());
    store.checkpoint_step(&context).unwrap();
    let before = newest_seals(&store.checkpoint_claims().unwrap(), &smallclaims::store::checkpoint::checkpoint_name(smallclaims::store::checkpoint::newest_due_cut(context.now_unix_ms)));
    failed(&store, &context);
    // Admission changes independently of immutable envelope identity, as when a record is
    // reclassified. One remaining admitted note is still unsafe to drop for the count reader.
    store.connection.batched(|tx| {
        tx.execute("UPDATE replica_records SET state='invalid' WHERE claim_id=?1", params![first.id])?;
        Ok::<_, anyhow::Error>(())
    }).unwrap().unwrap();
    assert!(store.checkpoint_step(&context).unwrap().is_empty());
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 2);
    context.now_unix_ms += CHECKPOINT_PROOF_BACKOFF_MS;
    failed(&store, &context);
    let after = newest_seals(&store.checkpoint_claims().unwrap(), &smallclaims::store::checkpoint::checkpoint_name(smallclaims::store::checkpoint::newest_due_cut(context.now_unix_ms)));
    assert_eq!(after, before);
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 4);
    assert_eq!(diagnostics(&store).len(), 2);
}
