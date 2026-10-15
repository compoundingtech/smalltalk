//! A node trims its own verified checkpoint from its staged plan, a page at a time, and adopts a
//! manifest instead when its plan no longer matches the certificate.
use super::*;
use crate::store::checkpoint::{DropPlan, SealedSet, claim_tombstone, drop_digest, retained_digest, sealed_digest};
use crate::store::checkpoint_agreement::CHECKPOINT_VERIFIED;
use crate::store::runtime::{IncrementalProjection, LegacyDigestTable, Plain};
use crate::store::checkpoint::{DropCount, EnvelopeTombstone};
use crate::store::checkpoint_staging::StagedPlanner;
use std::sync::Arc;

#[derive(Default)]
struct CaptureRuntime {
    stage_checks: AtomicUsize,
}

impl Runtime for CaptureRuntime {
    fn migrate_schema(&self, connection: &Connection) -> Result<()> {
        Plain.migrate_schema(connection)
    }

    fn create_schema(&self, connection: &Connection) -> Result<()> {
        Plain.create_schema(connection)?;
        connection.execute_batch(
            /* sqlite-large-value: allow-for-now lv-hc3a032b75010 owner=smallclaims::store::checkpoint_trim::capture_lifetime_tests TODO=chunk-or-reference-variable-columns */ "CREATE TABLE IF NOT EXISTS desired(claim_id TEXT);
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

    fn classify_replicated_claim(&self, connection: &Connection, batch: &ReplicaBatch, claim: &ClaimRecord) -> Result<ReplicatedClaimAdmission, St3Error> {
        Plain.classify_replicated_claim(connection, batch, claim)
    }

    fn append_claim(&self, store: &Store, input: &ClaimInput) -> Result<(ClaimRecord, bool), St3Error> {
        Plain.append_claim(store, input)
    }

    fn apply_repair_tx(&self, tx: &Transaction<'_>, repaired: &str, replacement: &str) -> Result<()> {
        Plain.apply_repair_tx(tx, repaired, replacement)
    }

    #[allow(clippy::too_many_arguments)]
    fn append_claim_tx(&self, tx: &Transaction<'_>, origin: &str, subject: &str, kind: &str, actor: Option<&str>, body: &Value, predecessors: &[String], forced_batch: Option<&str>) -> Result<ClaimRecord> {
        Plain.append_claim_tx(tx, origin, subject, kind, actor, body, predecessors, forced_batch)
    }

    fn project_incremental(&self, tx: &Transaction<'_>, origin: &str, through: u64) -> Result<IncrementalProjection, St3Error> {
        Plain.project_incremental(tx, origin, through)
    }

    fn replay_from_nothing(&self, tx: &Transaction<'_>) -> Result<(), St3Error> {
        Plain.replay_from_nothing(tx)
    }

    fn after_projection(&self, tx: &Transaction<'_>) -> Result<(), St3Error> {
        Plain.after_projection(tx)
    }

    fn forget_views(&self) { Plain.forget_views(); }

    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] {
        Plain.digest_tables()
    }

    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] {
        Plain.legacy_digest_tables()
    }

    fn checkpoint_rules_digest(&self) -> String { "capture-lifetime".into() }

    fn checkpoint_preflight(&self) -> Result<()> {
        self.stage_checks.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Drop every claim, and so every envelope that holds one: the reference plan in memory.
    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan {
        let mut claims = sealed.claims.iter().map(claim_tombstone).collect::<Vec<_>>();
        claims.sort();
        let held = sealed.claims.iter().map(|claim| &claim.envelope).collect::<BTreeSet<_>>();
        let envelopes = sealed.envelopes.iter().filter(|envelope| held.contains(&envelope.key))
            .map(|envelope| EnvelopeTombstone {
                writer: envelope.key.writer.clone(),
                sequence: envelope.key.sequence,
                envelope_hash: envelope.key.envelope_hash.clone(),
                accepted_at_unix_ms: envelope.accepted_at_unix_ms,
            })
            .collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        DropPlan {
            cut_unix_ms: sealed.cut_unix_ms,
            rules_digest: self.checkpoint_rules_digest(),
            sealed_envelopes: sealed.envelopes.len(),
            sealed_claims: sealed.claims.len(),
            sealed_digest: sealed_digest(sealed),
            drop_digest: drop_digest(&envelopes, &claims),
            retained_digest: retained_digest(std::iter::empty::<&str>()),
            envelopes,
            by_kind: BTreeMap::from([("example.note".into(), DropCount { sealed: claims.len(), dropped: claims.len() })]),
            claims,
        }
    }

    /// Drop every claim, as the reference does. Each staged claim is read back by ID once.
    fn plan_staged_checkpoint_drops(&self, planner: &StagedPlanner<'_>) -> Result<()> {
        let ids = planner.staging().prepare("SELECT id FROM claims ORDER BY pos")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut body_bytes = planner.claims(&ids)?.iter()
            .map(|claim| claim.body["fields"]["payload"].as_str().unwrap().len())
            .collect::<Vec<_>>();
        body_bytes.sort_unstable();
        assert_eq!(body_bytes, [1024, 4096, 16384]);
        planner.staging().execute(/* sqlite-large-value: allow-for-now lv-hfba5f6c160ce owner=smallclaims::store::checkpoint_trim::capture_lifetime_tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET dropped=1 WHERE first=1", [])?;
        Ok(())
    }

    fn clear_checkpoint_projections(&self, tx: &Transaction<'_>) -> Result<()> {
        Plain.clear_checkpoint_projections(tx)
    }

    fn replay_checkpoint_projections(&self, tx: &Transaction<'_>) -> Result<()> {
        Plain.replay_checkpoint_projections(tx)
    }

    fn checkpoint_subject_answers(&self, connection: &Connection, subject: &str, cut: u128) -> Result<Value> {
        Plain.checkpoint_subject_answers(connection, subject, cut)
    }
}

fn exercise(certified: bool, fault: Option<TrimFault>) {
    let runtime = Arc::new(CaptureRuntime::default());
    let store = Store::open_memory("alder", runtime.clone()).unwrap();
    for bytes in [1024, 4096, 16384] {
        store.append_claim(&ClaimInput {
            subject: "note/lifetime".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("payload".into(), json!("x".repeat(bytes)))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
    }
    let scratch = tempfile::tempdir().unwrap();
    let cut = now_ms() + 60_000;
    let checkpoint = checkpoint_name(cut);
    let staged = store.checkpoint_staged_set(cut, None, scratch.path()).unwrap();
    let plan = store.plan_staged_checkpoint(&staged).unwrap();
    let seal_rowid = staged.seal_rowid;
    let mut expected_claims = staged.dropped_claims(0, 16).unwrap();
    expected_claims.sort_by(|left, right| left.id.cmp(&right.id));
    let staging_file = staged.path.clone();
    drop(staged);
    store.connection.write().execute(
        /* sqlite-large-value: allow-for-now lv-he57c3e1c3fb2 owner=smallclaims::store::checkpoint_trim::capture_lifetime_tests TODO=stage-generation-and-keyed-reclamation */ "INSERT INTO checkpoints(id, cut_unix_ms, state, seal_rowid, updated_at_unix_ms)
         VALUES (?1, ?2, 'verified', ?3, ?2)",
        params![checkpoint, i64::try_from(cut).unwrap(), seal_rowid],
    ).unwrap();
    let drop = if certified { plan.drop_digest.clone() } else { "uncertified-plan".into() };
    let certificate = CheckpointClaim {
        id: "verification/alder".into(), kind: CHECKPOINT_VERIFIED.into(),
        subject: checkpoint.clone(), writer: "alder".into(), actor: None,
        fields: json!({
            "cut_unix_ms": u64::try_from(cut).unwrap(), "participants": ["alder"],
            "sealed_digest": plan.sealed_digest, "rules_digest": plan.rules_digest,
            "drop_digest": drop, "retained_digest": plan.retained_digest,
            "graph_digest": "graph", "reader_digest": "reader"
        }).as_object().unwrap().clone(),
    };
    store.set_trim_chunk_envelopes(1);
    let interrupt = fault == Some(TrimFault::AfterChunk(1));
    store.set_trim_fault(fault);
    let graph_before = graph_digest(&store.readers.get()).unwrap();
    let mut actions = Vec::new();
    let result = store.apply_stable_checkpoints(&[certificate], scratch.path(), &mut actions);
    if !certified {
        assert_eq!(result.unwrap().unwrap().drop_digest, "uncertified-plan");
        assert_eq!(local_checkpoint(&store.readers.get(), &checkpoint).unwrap().unwrap().0, "plan-changed");
        assert_eq!(store.claims_for("note/lifetime", None).unwrap().len(), 3);
    } else {
        if interrupt {
            assert!(result.unwrap_err().to_string().contains("AfterChunk(1)"));
            assert_eq!(store.claims_for("note/lifetime", None).unwrap().len(), 2);
            store.finish_trim(&checkpoint, &mut actions).unwrap();
        } else {
            assert!(result.unwrap().is_none());
        }
        assert_eq!(local_checkpoint(&store.readers.get(), &checkpoint).unwrap().unwrap().0, "trimmed");
        assert!(store.claims_for("note/lifetime", None).unwrap().is_empty());
        assert!(matches!(actions.last(), Some(CheckpointAction::Trimmed { .. })));
        assert_eq!(graph_digest(&store.readers.get()).unwrap(), graph_before);
        let manifest = store.checkpoint_manifest(&checkpoint, cut).unwrap();
        assert_eq!(manifest.claims, expected_claims);
        assert_eq!(drop_digest(&manifest.envelopes, &manifest.claims), plan.drop_digest);
        assert!(runtime.stage_checks.load(Ordering::Relaxed) >= 2);
    }
    // The trim reused the staged plan where it was, then removed it.
    if !interrupt {
        assert!(!staging_file.exists(), "the staged capture outlived its trim");
    }
}

#[test]
fn own_plan_trims_from_its_staged_plan_through_a_trim_error() {
    exercise(true, Some(TrimFault::AfterChunk(1)));
}

#[test]
fn own_plan_trims_from_its_staged_plan() {
    exercise(true, None);
}

#[test]
fn an_uncertified_staged_plan_leaves_claims_for_manifest_adoption() {
    exercise(false, None);
}

#[test]
fn an_expired_initial_budget_still_deletes_every_tombstoned_row() {
    exercise(true, Some(TrimFault::ExpireChunkBudget));
}
