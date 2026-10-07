//! Checkpoints: which replicated claims before a cut a stable checkpoint may drop, and the proof
//! that dropping them changes neither the graph nor what any reader returns.
//!
//! `doc/fleet/smalltalk/checkpoint-design` is the design. Each rule keeps, within a slot, the
//! claims a reader's answer depends on. It drops a claim only when a later kept claim of the same
//! slot replaces it for every fold that reads the kind (the claim's witness). The planner is a
//! pure function of the claims before the cut, in canonical order, so every node that holds the
//! same claims drops the same ones.

use super::*;

pub mod tombstones;

pub use tombstones::{
    CHECKPOINT_MANIFEST_PAGE_LIMIT, CheckpointManifest, CheckpointManifestCursor,
    CheckpointManifestPage, CheckpointManifestRequest, verify_checkpoint_manifest,
};
pub use tombstones::{
    checkpointed_operation, checkpointed_operations, claim_or_tombstone_exists, claim_tombstoned,
    envelope_tombstoned,
};

pub const DAY_MS: u128 = 86_400_000;

/// Envelopes and records a sealed-set read takes per page. Each page is a read of its own, so no
/// read holds a snapshot, and with it the WAL, for the length of the whole set.
pub(crate) const SEALED_ENVELOPE_PAGE: i64 = 5_000;
pub(crate) const SEALED_RECORD_PAGE: i64 = 20_000;

/// The name of the checkpoint whose cut is `cut_unix_ms`, for example `checkpoint/2026-09-27`.
pub fn checkpoint_name(cut_unix_ms: u128) -> String {
    let day = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
        i64::try_from(cut_unix_ms).unwrap_or(i64::MAX),
    )
    .map_or_else(
        || cut_unix_ms.to_string(),
        |time| time.format("%Y-%m-%d").to_string(),
    );
    format!("checkpoint/{day}")
}

/// The cut of the checkpoint named for a UTC day, given as `YYYY-MM-DD` or `checkpoint/YYYY-MM-DD`.
pub fn checkpoint_cut(day: &str) -> Result<u128, St3Error> {
    let day = day.strip_prefix("checkpoint/").unwrap_or(day);
    let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| {
        St3Error::new(
            "invalid-checkpoint",
            format!("`{day}` is not a UTC day such as 2026-09-27"),
        )
    })?;
    let start = date
        .and_hms_opt(0, 0, 0)
        .map(|time| time.and_utc().timestamp_millis())
        .and_then(|millis| u128::try_from(millis).ok())
        .ok_or_else(|| St3Error::new("invalid-checkpoint", format!("`{day}` is out of range")))?;
    Ok(start)
}

/// The cut of the newest checkpoint that is due at `now_unix_ms`: checkpoint `D` becomes due at
/// the start of day `D+2`.
pub fn newest_due_cut(now_unix_ms: u128) -> u128 {
    (now_unix_ms / DAY_MS).saturating_sub(2) * DAY_MS
}

/// A dry run of one checkpoint on this node: what it would drop, and the proof on a copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPlanView {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub rules_digest: String,
    pub sealed_envelopes: usize,
    pub sealed_claims: usize,
    pub sealed_digest: String,
    pub dropped_envelopes: usize,
    pub dropped_claims: usize,
    pub by_kind: BTreeMap<String, DropCount>,
    pub drop_digest: String,
    pub retained_digest: String,
    pub proof: CheckpointProof,
}

/// Which checkpoint a dry run plans. Without a day, the newest due one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointPlanRequest {
    #[serde(default)]
    pub day: Option<String>,
}

/// The identity of one envelope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EnvelopeKey {
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
}

/// A dropped envelope. A node keeps it in place of the envelope, so inventories and the
/// authority digest do not change and no peer sends the envelope back.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EnvelopeTombstone {
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub accepted_at_unix_ms: u128,
}

/// A dropped claim. It keeps what evidence checks, ancestry walks and idempotent retries read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClaimTombstone {
    pub id: String,
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub subject: String,
    pub kind: String,
    pub actor: Option<String>,
    pub predecessors: Vec<String>,
    pub operation_id: Option<String>,
    pub request_digest: Option<String>,
    pub accepted_at_unix_ms: u128,
}

/// One claim in the sealed set, with what the planner needs to decide about it.
#[derive(Clone, Debug)]
pub struct SealedClaim {
    pub claim: ClaimRecord,
    pub envelope: EnvelopeKey,
    /// The record admitted this claim as valid.
    pub valid: bool,
    /// A projection row or a repair names this claim, so it stays.
    pub protected: bool,
}

/// One envelope in the sealed set.
#[derive(Clone, Debug)]
pub struct SealedEnvelope {
    pub key: EnvelopeKey,
    pub accepted_at_unix_ms: u128,
    /// Every record the envelope holds, admitted or not.
    pub records: usize,
}

/// The envelopes before a cut and their admitted claims, less repaired originals, in canonical
/// order.
#[derive(Clone, Debug, Default)]
pub struct SealedSet {
    pub cut_unix_ms: u128,
    pub envelopes: Vec<SealedEnvelope>,
    pub claims: Vec<SealedClaim>,
    /// The highest `replica_envelopes` row when the set was read. Reading the set again up to
    /// this row gives the same set, whatever arrived since.
    pub seal_rowid: i64,
    /// Tombstones dated before the cut, from earlier checkpoints. Their envelopes are in
    /// `envelopes` with no claims, so a trimmed and an untrimmed node seal the same identities.
    pub envelope_tombstones: Vec<EnvelopeTombstone>,
    pub claim_tombstones: Vec<ClaimTombstone>,
}

/// What a seal records of a sealed set: its digest, how many envelopes it holds, and the row it
/// was read up to. Reading it needs no claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedIdentities {
    pub digest: String,
    pub count: usize,
    pub seal_rowid: i64,
}

impl SealedIdentities {
    pub fn of(sealed: &SealedSet) -> Self {
        Self {
            digest: sealed_digest(sealed),
            count: sealed.envelopes.len(),
            seal_rowid: sealed.seal_rowid,
        }
    }
}

/// What a checkpoint drops from a sealed set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropPlan {
    pub cut_unix_ms: u128,
    pub rules_digest: String,
    pub sealed_envelopes: usize,
    pub sealed_claims: usize,
    pub sealed_digest: String,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
    pub by_kind: BTreeMap<String, DropCount>,
    pub drop_digest: String,
    pub retained_digest: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCount {
    pub sealed: usize,
    pub dropped: usize,
}

pub fn claim_tombstone(sealed_claim: &SealedClaim) -> ClaimTombstone {
    let claim = &sealed_claim.claim;
    let operation = operation_parts(&claim.body);
    ClaimTombstone {
        id: claim.id.clone(),
        writer: sealed_claim.envelope.writer.clone(),
        sequence: sealed_claim.envelope.sequence,
        envelope_hash: sealed_claim.envelope.envelope_hash.clone(),
        subject: claim.subject.clone(),
        kind: claim.kind.clone(),
        actor: claim.actor.clone(),
        predecessors: claim.predecessors.clone(),
        operation_id: operation.map(|(id, _)| id.to_owned()),
        request_digest: operation.map(|(_, digest)| digest.to_owned()),
        accepted_at_unix_ms: claim.accepted_at_unix_ms,
    }
}

/// SHA-256 over the sealed set's envelope identities, in the inventory's encoding.
pub fn sealed_digest(sealed: &SealedSet) -> String {
    let identities = sealed
        .envelopes
        .iter()
        .map(|envelope| &envelope.key)
        .collect::<BTreeSet<_>>();
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-sealed-v1\0");
    for key in identities {
        update_identity_digest(&mut digest, &key.writer, key.sequence, &key.envelope_hash);
    }
    hex::encode(digest.finalize())
}

pub fn digest_field(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1_u8]);
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        None => digest.update([0_u8]),
    }
}

/// SHA-256 over every field of every tombstone, sorted. A manifest whose tombstones differ in
/// any field from what the participants verified has a different digest.
pub fn drop_digest(envelopes: &[EnvelopeTombstone], claims: &[ClaimTombstone]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-drop-v1\0");
    let envelopes = envelopes.iter().collect::<BTreeSet<_>>();
    let claims = claims.iter().collect::<BTreeSet<_>>();
    digest.update((envelopes.len() as u64).to_be_bytes());
    for envelope in envelopes {
        digest_field(&mut digest, Some(&envelope.writer));
        digest_field(&mut digest, Some(&envelope.sequence.to_string()));
        digest_field(&mut digest, Some(&envelope.envelope_hash));
        digest_field(&mut digest, Some(&envelope.accepted_at_unix_ms.to_string()));
    }
    digest.update((claims.len() as u64).to_be_bytes());
    for claim in claims {
        digest_field(&mut digest, Some(&claim.id));
        digest_field(&mut digest, Some(&claim.writer));
        digest_field(&mut digest, Some(&claim.sequence.to_string()));
        digest_field(&mut digest, Some(&claim.envelope_hash));
        digest_field(&mut digest, Some(&claim.subject));
        digest_field(&mut digest, Some(&claim.kind));
        digest_field(&mut digest, claim.actor.as_deref());
        digest.update((claim.predecessors.len() as u64).to_be_bytes());
        for predecessor in &claim.predecessors {
            digest_field(&mut digest, Some(predecessor));
        }
        digest_field(&mut digest, claim.operation_id.as_deref());
        digest_field(&mut digest, claim.request_digest.as_deref());
        digest_field(&mut digest, Some(&claim.accepted_at_unix_ms.to_string()));
    }
    hex::encode(digest.finalize())
}

/// SHA-256 over the sorted IDs of the claims a checkpoint keeps before its cut.
pub fn retained_digest<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-retained-v1\0");
    for id in ids.into_iter().collect::<BTreeSet<_>>() {
        digest_field(&mut digest, Some(id));
    }
    hex::encode(digest.finalize())
}

/// The outcome of proving a drop plan on a copy of the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointProof {
    /// The graph projected from every claim before the cut.
    pub graph_digest_before: String,
    /// The graph projected from the kept claims before the cut.
    pub graph_digest: String,
    pub reader_digest_before: String,
    pub reader_digest: String,
    /// Subjects whose readers were compared.
    pub subjects: usize,
    pub passed: bool,
    /// The first readers that answered differently, as `subject reader`.
    pub mismatches: Vec<String>,
}

/// Source identity for an array item, used only for bounded scratch-proof failure diagnostics.
/// It is excluded from reader answers, their digests and checkpoint certificates.
#[derive(Clone, Debug)]
pub struct CheckpointItemSource {
    pub claim: String,
    pub order: super::canonical::ClaimKey,
}

pub type CheckpointAnswerSources = BTreeMap<String, Vec<CheckpointItemSource>>;
type ProofSources = BTreeMap<(String, String), Vec<CheckpointItemSource>>;

// Reuse the existing bounded 60-second diagnostic limiter with one fixed stage/code bucket.
// No source/subject IDs are keys, and suppressed failures have no flush, queue or retry.
static CHECKPOINT_DIAGNOSTICS: std::sync::OnceLock<std::sync::Mutex<super::ProjectionDiagnosticState>> = std::sync::OnceLock::new();

/// Record the tombstones of a checkpoint's drop. Recording them again changes nothing.
pub fn record_checkpoint_tombstones_tx(
    transaction: &Transaction<'_>,
    checkpoint: &str,
    envelopes: &[EnvelopeTombstone],
    claims: &[ClaimTombstone],
) -> Result<()> {
    let mut insert_envelope = transaction.prepare_cached(
        "INSERT OR IGNORE INTO checkpoint_envelopes(
             writer, sequence, envelope_hash, accepted_at_unix_ms, checkpoint)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for envelope in envelopes {
        insert_envelope.execute(params![
            envelope.writer,
            envelope.sequence,
            envelope.envelope_hash,
            i64::try_from(envelope.accepted_at_unix_ms)?,
            checkpoint
        ])?;
    }
    let mut insert_claim = transaction.prepare_cached(
        "INSERT OR IGNORE INTO checkpoint_claims(
             id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
             operation_id, request_digest, accepted_at_unix_ms, checkpoint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for claim in claims {
        insert_claim.execute(params![
            claim.id,
            claim.writer,
            claim.sequence,
            claim.envelope_hash,
            claim.subject,
            claim.kind,
            claim.actor,
            serde_json::to_string(&claim.predecessors)?,
            claim.operation_id,
            claim.request_digest,
            i64::try_from(claim.accepted_at_unix_ms)?,
            checkpoint
        ])?;
    }
    Ok(())
}

/// Delete dropped claims and their envelopes from a store. The trim uses the same code.
pub fn delete_dropped_rows_tx(
    transaction: &Transaction<'_>,
    envelopes: &[EnvelopeTombstone],
    claims: &[ClaimTombstone],
) -> Result<()> {
    for claim in claims {
        if let Some(operation) = &claim.operation_id {
            transaction.execute(
                "DELETE FROM operations WHERE id=?1 AND canonical_claim_id=?2",
                params![operation, claim.id],
            )?;
        }
        super::events::remove_claim_tx(transaction, &claim.id)?;
        transaction.execute("DELETE FROM claims WHERE id=?1", [&claim.id])?;
    }
    for envelope in envelopes {
        let batch: Option<String> = transaction
            .query_row(
                "SELECT batch_id FROM replica_envelopes
                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
                params![envelope.writer, envelope.sequence, envelope.envelope_hash],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        for table in [
            "replica_records",
            "replica_envelope_signatures",
            "replica_envelope_holds",
            "replica_envelopes",
        ] {
            transaction.execute(
                &format!(
                    "DELETE FROM {table} WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3"
                ),
                params![envelope.writer, envelope.sequence, envelope.envelope_hash],
            )?;
        }
        if let Some(batch) = batch {
            transaction.execute(
                "DELETE FROM batches WHERE id=?1
                 AND NOT EXISTS (SELECT 1 FROM claims WHERE batch_id=?1)
                 AND NOT EXISTS (SELECT 1 FROM replica_envelopes WHERE batch_id=?1)",
                [&batch],
            )?;
        }
    }
    Ok(())
}

pub fn claims_of_kind_in_order(
    connection: &Connection,
    subject: &str,
    kind: &str,
) -> Result<Vec<ClaimRecord>> {
    connection
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind=?2 ORDER BY {CANONICAL_ORDER}"
        ))?
        .query_map(params![subject, kind], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn reader_answers(
    runtime: &dyn Runtime,
    connection: &Connection,
    subjects: &BTreeSet<String>,
    cut: u128,
) -> Result<BTreeMap<String, Value>> {
    subjects
        .iter()
        .map(|subject| {
            Ok((
                subject.clone(),
                runtime.checkpoint_subject_answers(connection, subject, cut)?,
            ))
        })
        .collect()
}

fn reader_answers_with_sources(
    runtime: &dyn Runtime,
    connection: &Connection,
    subjects: &BTreeSet<String>,
    cut: u128,
) -> Result<(BTreeMap<String, Value>, ProofSources)> {
    let mut answers = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for subject in subjects {
        let (answer, item_sources) = runtime.checkpoint_subject_answers_with_sources(connection, subject, cut)?;
        answers.insert(subject.clone(), answer);
        for (reader, items) in item_sources {
            sources.insert((subject.clone(), reader), items);
        }
    }
    Ok((answers, sources))
}

fn status_history_mismatch_diagnostics(
    sealed: &SealedSet,
    plan: &DropPlan,
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
    before_sources: &ProofSources,
    after_sources: &ProofSources,
) -> Vec<Value> {
    fn text(value: Option<&Value>, limit: usize) -> Value {
        value.and_then(Value::as_str).map_or(Value::Null, |text| json!(text.chars().take(limit).collect::<String>()))
    }
    fn preview(items: &[Value], sources: Option<&[CheckpointItemSource]>, start: usize, plan: &DropPlan) -> Vec<Value> {
        items.iter().enumerate().skip(start).take(3).map(|(index, item)| {
            let source = sources.and_then(|sources| sources.get(index));
            json!({
                "index":index, "source":source.map(|source| source.claim.chars().take(128).collect::<String>()),
                "accepted_at_ms":source.map(|source| source.order.0),
                "order":source.map(|source| (
                    source.order.0, source.order.1.chars().take(256).collect::<String>(), source.order.2,
                    source.order.3.chars().take(256).collect::<String>(), source.order.4,
                    source.order.5.chars().take(128).collect::<String>())),
                "dropped":source.map(|source| plan.claims.iter().any(|claim| claim.id == source.claim)),
                "state":text(item.get("state"), 128),
                "incarnation":text(item.get("runtime_incarnation"), 128),
                "observed_at":text(item.get("observed_at"), 48), "reset":item.get("reset").and_then(Value::as_bool),
            })
        }).collect()
    }
    let mut diagnostics = Vec::new();
    for (subject, answer) in before {
        let Some(old) = answer.get("status_history").and_then(Value::as_array) else { continue; };
        let Some(new) = after.get(subject).and_then(|answer| answer.get("status_history")).and_then(Value::as_array) else { continue; };
        let Some(first) = (0..old.len().max(new.len())).find(|index| old.get(*index) != new.get(*index)) else { continue; };
        let key = (subject.clone(), "status_history".to_owned());
        diagnostics.push(json!({
            "checkpoint":checkpoint_name(sealed.cut_unix_ms), "seal_rowid":sealed.seal_rowid,
            "sealed_digest":plan.sealed_digest, "cut_unix_ms":sealed.cut_unix_ms,
            "source_build":super::checkpoint_agreement::checkpoint_build().chars().take(256).collect::<String>(),
            "rules_digest":plan.rules_digest, "drop_digest":plan.drop_digest,
            "subject":subject.chars().take(256).collect::<String>(), "reader":"status_history",
            "before_len":old.len(), "after_len":new.len(), "first_difference":first,
            "before":preview(old, before_sources.get(&key).map(Vec::as_slice), first, plan),
            "after":preview(new, after_sources.get(&key).map(Vec::as_slice), first, plan),
        }));
        if diagnostics.len() == 3 { break; }
    }
    diagnostics
}

#[test]
fn status_history_diagnostics_bound_items_and_exclude_content() {
    let items = (0..8).map(|index| json!({
        "state":"working", "runtime_incarnation":"x".repeat(1000),
        "observed_at":index.to_string(), "reset":false,
        "body":"private transcript must not appear",
    })).collect::<Vec<_>>();
    let mut changed = items.clone();
    changed.remove(2);
    let subject = "agent/cedar".to_owned();
    let before = BTreeMap::from([(subject.clone(), json!({"status_history":items}))]);
    let after = BTreeMap::from([(subject.clone(), json!({"status_history":changed}))]);
    let sources = BTreeMap::from([((subject, "status_history".to_owned()),
        (0..8).map(|index| CheckpointItemSource {
            claim:format!("claim-{index}"), order:(index, "cedar".into(), index as u64, "batch".into(), 0, format!("claim-{index}")),
        }).collect::<Vec<_>>())]);
    let sealed = SealedSet {
        cut_unix_ms:DAY_MS, envelopes:Vec::new(), claims:Vec::new(), seal_rowid:7,
        envelope_tombstones:Vec::new(), claim_tombstones:Vec::new(),
    };
    let plan = DropPlan {
        cut_unix_ms:DAY_MS, rules_digest:"rules".into(), sealed_envelopes:0, sealed_claims:0,
        sealed_digest:"sealed".into(), drop_digest:"drop".into(), retained_digest:"retained".into(),
        envelopes:Vec::new(), claims:Vec::new(), by_kind:BTreeMap::new(),
    };
    let mut after_sources = sources.clone();
    after_sources.values_mut().next().unwrap().remove(2);
    let diagnostics = status_history_mismatch_diagnostics(&sealed, &plan, &before, &after, &sources, &after_sources);
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic["first_difference"], 2);
    assert_eq!(diagnostic["before_len"], 8);
    assert_eq!(diagnostic["after_len"], 7);
    assert_eq!(diagnostic["before"].as_array().unwrap().len(), 3);
    assert_eq!(diagnostic["after"].as_array().unwrap().len(), 3);
    assert_eq!(diagnostic["before"][0]["source"], "claim-2");
    assert_eq!(diagnostic["after"][0]["source"], "claim-3");
    assert_eq!(diagnostic["before"][0]["incarnation"].as_str().unwrap().len(), 128);
    assert!(!diagnostic.to_string().contains("private transcript"));
    assert!(status_history_mismatch_diagnostics(&sealed, &plan, &before, &before, &sources, &sources).is_empty());
}

pub fn answers_digest(answers: &BTreeMap<String, Value>) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-readers-v1\0");
    for (subject, value) in answers {
        digest_field(&mut digest, Some(subject));
        // Canonical text, so equal answers digest alike whatever order their keys were built in.
        digest_field(&mut digest, Some(&canonical_json_text(value)?));
    }
    Ok(hex::encode(digest.finalize()))
}

pub fn answer_mismatches(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut mismatches = Vec::new();
    for (subject, before) in before {
        let after = after.get(subject).cloned().unwrap_or(Value::Null);
        let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
            mismatches.push(format!("{subject} all"));
            continue;
        };
        for (reader, value) in before {
            if after.get(reader) != Some(value) {
                mismatches.push(format!("{subject} {reader}"));
            }
        }
        for reader in after.keys() {
            if !before.contains_key(reader) {
                mismatches.push(format!("{subject} {reader}"));
            }
        }
    }
    mismatches.truncate(20);
    mismatches
}

/// Project a copy of the sealed set with and without the drop, and compare the graph and every
/// reader answer. `copy` is a store file holding at least the sealed set; it is changed.
pub fn prove_on_copy(
    runtime: &dyn Runtime,
    copy: &Path,
    sealed: &SealedSet,
    plan: &DropPlan,
) -> Result<CheckpointProof> {
    runtime.checkpoint_preflight()?;
    let mut connection = Connection::open(copy)?;
    projection_digest::register(&connection)?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = MEMORY;")?;
    let transaction = connection.transaction()?;
    // Keep only the sealed set.
    transaction.execute_batch(
        "CREATE TEMP TABLE sealed_claims(id TEXT PRIMARY KEY);
         CREATE TEMP TABLE sealed_blobs(hash TEXT PRIMARY KEY);
         CREATE TEMP TABLE sealed_envelopes(writer TEXT, sequence INTEGER, envelope_hash TEXT,
             PRIMARY KEY(writer, sequence, envelope_hash));",
    )?;
    let mut blobs = BTreeSet::new();
    for sealed_claim in &sealed.claims {
        transaction.execute(
            "INSERT OR IGNORE INTO temp.sealed_claims(id) VALUES (?1)",
            [&sealed_claim.claim.id],
        )?;
        collect_hash_fields(&sealed_claim.claim.body, &mut blobs);
    }
    for hash in &blobs {
        transaction.execute("INSERT INTO temp.sealed_blobs(hash) VALUES (?1)", [hash])?;
    }
    for envelope in &sealed.envelopes {
        transaction.execute(
            "INSERT OR IGNORE INTO temp.sealed_envelopes VALUES (?1, ?2, ?3)",
            params![
                envelope.key.writer,
                envelope.key.sequence,
                envelope.key.envelope_hash
            ],
        )?;
    }
    runtime.clear_checkpoint_projections(&transaction)?;
    // Blobs go with the claims that reference them. Admission needs a claim's blobs, so every
    // node that holds the sealed claims holds these, while the blobs of later claims depend on
    // what has arrived since the cut. A trim deletes no blob.
    transaction.execute_batch(
        "DELETE FROM claims WHERE id NOT IN (SELECT id FROM temp.sealed_claims);
         DELETE FROM blobs WHERE hash NOT IN (SELECT hash FROM temp.sealed_blobs);
         DELETE FROM replica_records WHERE NOT EXISTS (
             SELECT 1 FROM temp.sealed_envelopes s WHERE s.writer=replica_records.writer
               AND s.sequence=replica_records.sequence AND s.envelope_hash=replica_records.envelope_hash);",
    )?;
    // Number the claims in canonical order. Every node that holds the sealed set then proves
    // on the same store, whatever order its claims arrived in, so readers that compare store
    // indexes answer alike and the digests can match across nodes.
    transaction.execute_batch(&format!(
        "CREATE TEMP TABLE canonical_index(old INTEGER PRIMARY KEY, new INTEGER NOT NULL);
         INSERT INTO temp.canonical_index(old, new)
             SELECT claims.store_index, ROW_NUMBER() OVER (ORDER BY {CANONICAL_ORDER})
             FROM claims JOIN batches ON batches.id=claims.batch_id;
         UPDATE claims SET store_index=-(SELECT new FROM temp.canonical_index
                                         WHERE old=claims.store_index);
         UPDATE claims SET store_index=-store_index;"
    ))?;
    let subjects = plan
        .claims
        .iter()
        .map(|claim| claim.subject.clone())
        .collect::<BTreeSet<_>>();
    runtime.replay_checkpoint_projections(&transaction)?;
    let graph_digest_before = graph_digest(&transaction)?;
    let digests_before = projection_digest::tables(&transaction)?;
    let (before, before_sources) = reader_answers_with_sources(runtime, &transaction, &subjects, sealed.cut_unix_ms)?;
    // As a trim does: tombstones first, which readers that walk ancestry pass through.
    record_checkpoint_tombstones_tx(
        &transaction,
        &checkpoint_name(sealed.cut_unix_ms),
        &plan.envelopes,
        &plan.claims,
    )?;
    delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims)?;
    runtime.replay_checkpoint_projections(&transaction)?;
    let graph_digest_after = graph_digest(&transaction)?;
    let (after, after_sources) = reader_answers_with_sources(runtime, &transaction, &subjects, sealed.cut_unix_ms)?;
    let mut mismatches = answer_mismatches(&before, &after);
    if graph_digest_before != graph_digest_after {
        mismatches.splice(
            0..0,
            projection_digest::differing(
                &digests_before,
                &projection_digest::tables(&transaction)?,
            )
            .into_iter()
            .map(|table| format!("graph {table}")),
        );
    }
    let proof = CheckpointProof {
        reader_digest_before: answers_digest(&before)?,
        reader_digest: answers_digest(&after)?,
        graph_digest_before,
        graph_digest: graph_digest_after,
        subjects: subjects.len(),
        passed: mismatches.is_empty(),
        mismatches,
    };
    if !proof.passed {
        let diagnostics = status_history_mismatch_diagnostics(sealed, plan, &before, &after, &before_sources, &after_sources);
        if !diagnostics.is_empty() {
            let emission = CHECKPOINT_DIAGNOSTICS.get_or_init(Default::default).lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .observe("checkpoint", "status-history-mismatch", std::time::Instant::now());
            if let Some(emission) = emission {
                for mut diagnostic in diagnostics {
                    diagnostic["suppressed_proof_logs"] = json!(emission.suppressed);
                    diagnostic["rate_bucket_overflow"] = json!(emission.overflow);
                    eprintln!("st3: checkpoint reader mismatch {diagnostic}");
                }
            }
        }
    }
    transaction.rollback()?;
    Ok(proof)
}

/// The query for one window of records in the sealed-set read: the claim columns, the record's
/// identity and state, and the canonical order's components as columns 14 to 20 (so the pages
/// can be sorted together in Rust as `ORDER BY canonical` sorted them), then the record's
/// position as column 21. It must be driven from `replica_records` by its rowid range; a plan
/// that drives from `claims` would read every claim for every window.
pub fn sealed_records_page_sql() -> String {
    let order = super::canonical::components("claims").join(", ");
    format!(
        "SELECT {CLAIM_COLUMNS}, records.writer, records.sequence, records.envelope_hash,
                records.state, {order}, records.position
         FROM claims JOIN batches ON batches.id=claims.batch_id
         JOIN replica_records records ON records.claim_id=claims.id
         WHERE records.rowid > ?1 AND records.rowid <= ?2 AND records.state<>'repaired'"
    )
}

impl Store {
    /// The envelopes this node holds from before `cut_unix_ms`, and their admitted claims in
    /// canonical order. An envelope with any claim dated at or after the cut is not before it.
    pub fn checkpoint_sealed_set(&self, cut_unix_ms: u128) -> Result<SealedSet> {
        self.checkpoint_sealed_set_through(cut_unix_ms, None)
    }

    /// The sealed set as of one `replica_envelopes` row: envelopes that arrived after
    /// `through_rowid` are left out, so a node reads again exactly the set it sealed.
    pub fn checkpoint_sealed_set_through(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
    ) -> Result<SealedSet> {
        self.checkpoint_sealed_set_paged(
            cut_unix_ms,
            through_rowid,
            SEALED_ENVELOPE_PAGE,
            SEALED_RECORD_PAGE,
        )
    }

    /// `checkpoint_sealed_set_through`, reading `envelope_page` envelopes and `record_page`
    /// records per read. Each page is a read of its own, so its snapshot ends with it and the WAL
    /// can be checkpointed between pages; one transaction over the whole set pinned the WAL for
    /// the 20 to 40 seconds a large store took. The set stays the same set: the envelopes are
    /// those up to `seal_rowid`, and a claim is kept only if its envelope is among them, so what
    /// arrives between pages is read and left out. Only a record changing state (a repair)
    /// between two pages of one call could differ from a single snapshot.
    pub fn checkpoint_sealed_set_paged(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
        envelope_page: i64,
        record_page: i64,
    ) -> Result<SealedSet> {
        self.runtime.checkpoint_preflight()?;
        // Seal only new batches. A full history scan under the writer stalls live requests
        // every time a checkpoint is reconsidered, even when no new envelope is needed.
        self.seal_local_batches()?;
        let seal_rowid: i64 = {
            let connection = self.readers.get();
            let high: i64 = connection.query_row(
                "SELECT COALESCE(MAX(rowid), 0) FROM replica_envelopes",
                [],
                |row| row.get(0),
            )?;
            through_rowid.map_or(high, |through| through.min(high))
        };
        let mut envelopes = Vec::new();
        let mut after = 0_i64;
        while after < seal_rowid {
            let upto = after.saturating_add(envelope_page).min(seal_rowid);
            let connection = self.readers.get();
            let page = connection
                .prepare_cached(
                    "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                            envelopes.accepted_at_unix_ms,
                            (SELECT COUNT(*) FROM replica_records records
                             WHERE records.writer=envelopes.writer AND records.sequence=envelopes.sequence
                               AND records.envelope_hash=envelopes.envelope_hash)
                     FROM replica_envelopes envelopes
                     WHERE envelopes.rowid > ?1 AND envelopes.rowid <= ?2",
                )?
                .query_map([after, upto], |row| {
                    Ok(SealedEnvelope {
                        key: EnvelopeKey {
                            writer: row.get(0)?,
                            sequence: row.get(1)?,
                            envelope_hash: row.get(2)?,
                        },
                        accepted_at_unix_ms: row
                            .get::<_, String>(3)?
                            .parse()
                            .unwrap_or(u128::MAX),
                        records: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            envelopes.extend(page);
            after = upto;
        }
        envelopes.retain(|envelope| envelope.accepted_at_unix_ms < cut_unix_ms);
        // A repaired original is left out, as projections leave it out: a node holds its row
        // only when it admitted the original before the repair arrived, so including it would
        // make the set, and every digest of it, differ between nodes that hold the same
        // envelopes.
        // Read the records a window at a time, each window on a read of its own, with the
        // canonical order's components as columns so the pages sort together here exactly as
        // `ORDER BY canonical` sorted them in SQL (binary text, then integers, NULL first).
        let records_sql = sealed_records_page_sql();
        let (first, last): (i64, i64) = {
            let connection = self.readers.get();
            connection.query_row(
                "SELECT COALESCE(MIN(rowid), 0), COALESCE(MAX(rowid), 0) FROM replica_records",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
        };
        // The canonical order, then the record's own identity, so a claim held by two records
        // (the same claim admitted from two envelopes) orders the same way every time.
        type OrderKey = (i64, String, Option<String>, Option<i64>, String, i64, String, String, i64, String, i64);
        let mut keyed: Vec<(OrderKey, ClaimRecord, EnvelopeKey, bool)> = Vec::new();
        let mut after = first.saturating_sub(1);
        while after < last {
            let upto = after.saturating_add(record_page).min(last);
            let connection = self.readers.get();
            let page = connection
                .prepare_cached(&records_sql)?
                .query_map([after, upto], |row| {
                    Ok((
                        (
                            row.get(14)?,
                            row.get(15)?,
                            row.get(16)?,
                            row.get(17)?,
                            row.get(18)?,
                            row.get(19)?,
                            row.get(20)?,
                            row.get(10)?,
                            row.get(11)?,
                            row.get(12)?,
                            row.get(21)?,
                        ),
                        claim_from_row(row)?,
                        EnvelopeKey {
                            writer: row.get(10)?,
                            sequence: row.get(11)?,
                            envelope_hash: row.get(12)?,
                        },
                        row.get::<_, String>(13)? == "valid",
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keyed.extend(page);
            after = upto;
        }
        keyed.sort_by(|left, right| left.0.cmp(&right.0));
        let claims: Vec<(ClaimRecord, EnvelopeKey, bool)> = keyed
            .into_iter()
            .map(|(_, claim, envelope, valid)| (claim, envelope, valid))
            .collect();
        // Read after the record pages, so it is at least as new as the claims: a claim that became
        // protected while the pages were read is protected here, and the set errs toward keeping.
        let connection = self.readers.get();
        let protected = connection
            .prepare(
                "SELECT claim_id FROM desired WHERE claim_id IS NOT NULL
                 UNION SELECT claim_id FROM mission_definitions
                 UNION SELECT claim_id FROM mission_revisions
                 UNION SELECT binding_claim_id FROM documents WHERE binding_claim_id IS NOT NULL
                 UNION SELECT json_extract(body, '$.fields.replacement') FROM claims
                     WHERE kind='record.repaired'
                 UNION SELECT replacement_claim_id FROM replica_records
                     WHERE replacement_claim_id IS NOT NULL",
            )?
            .query_map([], |row| row.get::<_, Option<String>>(0))?
            .filter_map(|row| row.transpose())
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let connection = self.readers.get();
        let mut late = BTreeSet::new();
        for (claim, envelope, _) in &claims {
            if claim.accepted_at_unix_ms >= cut_unix_ms {
                late.insert(envelope.clone());
            }
        }
        envelopes.retain(|envelope| !late.contains(&envelope.key));
        let before = envelopes
            .iter()
            .map(|envelope| envelope.key.clone())
            .collect::<BTreeSet<_>>();
        let claims = claims
            .into_iter()
            .filter(|(_, envelope, _)| before.contains(envelope))
            .map(|(claim, envelope, valid)| SealedClaim {
                protected: protected.contains(&claim.id),
                claim,
                envelope,
                valid,
            })
            .collect();
        // Envelopes an earlier checkpoint dropped are still part of what this node seals.
        let envelope_tombstones = connection
            .prepare(
                "SELECT writer, sequence, envelope_hash, accepted_at_unix_ms
                 FROM checkpoint_envelopes WHERE accepted_at_unix_ms < ?1",
            )?
            .query_map([i64::try_from(cut_unix_ms)?], |row| {
                Ok(EnvelopeTombstone {
                    writer: row.get(0)?,
                    sequence: row.get(1)?,
                    envelope_hash: row.get(2)?,
                    accepted_at_unix_ms: u128::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let claim_tombstones = connection
            .prepare(
                "SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at_unix_ms
                 FROM checkpoint_claims WHERE accepted_at_unix_ms < ?1",
            )?
            .query_map([i64::try_from(cut_unix_ms)?], |row| {
                Ok(ClaimTombstone {
                    id: row.get(0)?,
                    writer: row.get(1)?,
                    sequence: row.get(2)?,
                    envelope_hash: row.get(3)?,
                    subject: row.get(4)?,
                    kind: row.get(5)?,
                    actor: row.get(6)?,
                    predecessors: serde_json::from_str(&row.get::<_, String>(7)?)
                        .unwrap_or_default(),
                    operation_id: row.get(8)?,
                    request_digest: row.get(9)?,
                    accepted_at_unix_ms: u128::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for tombstone in &envelope_tombstones {
            let key = EnvelopeKey {
                writer: tombstone.writer.clone(),
                sequence: tombstone.sequence,
                envelope_hash: tombstone.envelope_hash.clone(),
            };
            if !before.contains(&key) {
                envelopes.push(SealedEnvelope {
                    key,
                    accepted_at_unix_ms: tombstone.accepted_at_unix_ms,
                    records: 0,
                });
            }
        }
        envelopes.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(SealedSet {
            cut_unix_ms,
            envelopes,
            claims,
            seal_rowid,
            envelope_tombstones,
            claim_tombstones,
        })
    }

    /// The identities `checkpoint_sealed_set_through` would read, without the claims: what a
    /// seal and the status need, every few minutes, on a store of any size.
    pub fn checkpoint_sealed_identities(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
    ) -> Result<SealedIdentities> {
        self.checkpoint_sealed_identities_paged(cut_unix_ms, through_rowid, SEALED_ENVELOPE_PAGE)
    }

    /// `checkpoint_sealed_identities`, reading `envelope_page` envelopes per read, each on a read
    /// of its own so the WAL can be checkpointed between pages (see
    /// `checkpoint_sealed_set_paged`). It read the whole set in one transaction before, which
    /// held the WAL pinned for 35 seconds on a large store.
    pub fn checkpoint_sealed_identities_paged(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
        envelope_page: i64,
    ) -> Result<SealedIdentities> {
        self.runtime.checkpoint_preflight()?;
        self.seal_local_batches()?;
        let seal_rowid: i64 = {
            let connection = self.readers.get();
            let high: i64 = connection.query_row(
                "SELECT COALESCE(MAX(rowid), 0) FROM replica_envelopes",
                [],
                |row| row.get(0),
            )?;
            through_rowid.map_or(high, |through| through.min(high))
        };
        let cut = i64::try_from(cut_unix_ms)?;
        // As in `checkpoint_sealed_set_through`: an envelope is before the cut when it and every
        // claim admitted from it, less repaired originals, are dated before the cut.
        let mut identities = BTreeSet::new();
        let mut after = 0_i64;
        while after < seal_rowid {
            let upto = after.saturating_add(envelope_page).min(seal_rowid);
            let connection = self.readers.get();
            let page = connection
                .prepare_cached(
                    "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash
                     FROM replica_envelopes AS envelopes
                     WHERE envelopes.rowid > ?3 AND envelopes.rowid <= ?2
                       AND CAST(envelopes.accepted_at_unix_ms AS INTEGER) < ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM replica_records AS records
                           JOIN claims ON claims.id=records.claim_id
                           WHERE records.writer=envelopes.writer
                             AND records.sequence=envelopes.sequence
                             AND records.envelope_hash=envelopes.envelope_hash
                             AND records.state<>'repaired'
                             AND CAST(claims.accepted_at_unix_ms AS INTEGER) >= ?1)",
                )?
                .query_map(params![cut, upto, after], |row| {
                    Ok(EnvelopeKey {
                        writer: row.get(0)?,
                        sequence: row.get(1)?,
                        envelope_hash: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            identities.extend(page);
            after = upto;
        }
        {
            let connection = self.readers.get();
            let tombstones = connection
                .prepare_cached(
                    "SELECT writer, sequence, envelope_hash FROM checkpoint_envelopes
                     WHERE accepted_at_unix_ms < ?1",
                )?
                .query_map(params![cut], |row| {
                    Ok(EnvelopeKey {
                        writer: row.get(0)?,
                        sequence: row.get(1)?,
                        envelope_hash: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            identities.extend(tombstones);
        }
        let mut digest = Sha256::new();
        digest.update(b"st3-checkpoint-sealed-v1\0");
        for key in &identities {
            update_identity_digest(&mut digest, &key.writer, key.sequence, &key.envelope_hash);
        }
        Ok(SealedIdentities {
            digest: hex::encode(digest.finalize()),
            count: identities.len(),
            seal_rowid,
        })
    }

    /// What the readers a checkpoint must preserve answer for each of `subjects` at
    /// `now_unix_ms`, as canonical JSON text: the answers the proof compares. Nodes holding the
    /// same kept claims answer identically, and so does a node that never trimmed.
    pub fn checkpoint_reader_answers(
        &self,
        subjects: &BTreeSet<String>,
        now_unix_ms: u128,
    ) -> Result<BTreeMap<String, String>> {
        let connection = self.readers.get();
        let connection = connection.unchecked_transaction()?;
        reader_answers(&*self.runtime, &connection, subjects, now_unix_ms)?
            .into_iter()
            .map(|(subject, answer)| Ok((subject, canonical_json_text(&answer)?)))
            .collect()
    }

    /// Copy this store to `copy` from one consistent snapshot, while the writer carries on.
    pub fn copy_store_to(&self, copy: &Path) -> Result<()> {
        // `VACUUM INTO` reads the whole store in one transaction on a connection of its own, which
        // holds the WAL pinned for as long as the copy takes. Register it, so a pinned-WAL
        // report can name it: it was the one pin with no live read to blame.
        let _live = crate::sqlite::register_live_read(true);
        // Readers are read-only, and `VACUUM INTO` needs a connection that may write the copy.
        let flags = if self.shared_memory {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let connection = Connection::open_with_flags(&self.path, flags)?;
        connection.execute_batch("PRAGMA busy_timeout = 5000;")?;
        connection.execute("VACUUM INTO ?1", [copy.to_string_lossy()])?;
        Ok(())
    }

    /// Plan the drop for `cut_unix_ms` and prove it on a copy of this store in `scratch`, which
    /// must be a directory the proof may write to. Nothing in this store changes.
    pub fn plan_checkpoint(
        &self,
        cut_unix_ms: u128,
        scratch: &Path,
    ) -> Result<(DropPlan, CheckpointProof)> {
        self.plan_checkpoint_through(cut_unix_ms, None, scratch)
            .map(|(_, plan, proof)| (plan, proof))
    }

    /// Plan and prove the sealed set as of one `replica_envelopes` row. See
    /// `checkpoint_sealed_set_through`.
    pub fn plan_checkpoint_through(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
        scratch: &Path,
    ) -> Result<(SealedSet, DropPlan, CheckpointProof)> {
        self.runtime.checkpoint_preflight()?;
        let sealed = self.checkpoint_sealed_set_through(cut_unix_ms, through_rowid)?;
        let plan = self.runtime.plan_checkpoint_drops(&sealed);
        fs::create_dir_all(scratch)?;
        let copy = scratch.join(format!("proof-{}.sqlite3", Uuid::now_v7().simple()));
        let result = self
            .copy_store_to(&copy)
            .and_then(|()| prove_on_copy(&*self.runtime, &copy, &sealed, &plan));
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", copy.display()));
        }
        let proof = result?;
        Ok((sealed, plan, proof))
    }

    /// `st replication checkpoint plan`: plan and prove one checkpoint without changing anything.
    pub fn checkpoint_plan_view(
        &self,
        cut_unix_ms: u128,
        scratch: &Path,
    ) -> Result<CheckpointPlanView> {
        let (plan, proof) = self.plan_checkpoint(cut_unix_ms, scratch)?;
        Ok(CheckpointPlanView {
            checkpoint: checkpoint_name(cut_unix_ms),
            cut_unix_ms,
            rules_digest: plan.rules_digest,
            sealed_envelopes: plan.sealed_envelopes,
            sealed_claims: plan.sealed_claims,
            sealed_digest: plan.sealed_digest,
            dropped_envelopes: plan.envelopes.len(),
            dropped_claims: plan.claims.len(),
            by_kind: plan.by_kind,
            drop_digest: plan.drop_digest,
            retained_digest: plan.retained_digest,
            proof,
        })
    }
}
