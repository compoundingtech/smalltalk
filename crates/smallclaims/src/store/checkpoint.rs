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

/// The envelopes before a cut and their admitted claims, in canonical order.
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
        transaction.execute(
            "DELETE FROM events WHERE store_index IN (SELECT store_index FROM claims WHERE id=?1)",
            [&claim.id],
        )?;
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
    let mut connection = Connection::open(copy)?;
    projection_digest::register(&connection)?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = MEMORY;")?;
    let transaction = connection.transaction()?;
    // Keep only the sealed set.
    transaction.execute_batch(
        "CREATE TEMP TABLE sealed_claims(id TEXT PRIMARY KEY);
         CREATE TEMP TABLE sealed_envelopes(writer TEXT, sequence INTEGER, envelope_hash TEXT,
             PRIMARY KEY(writer, sequence, envelope_hash));",
    )?;
    for sealed_claim in &sealed.claims {
        transaction.execute(
            "INSERT OR IGNORE INTO temp.sealed_claims(id) VALUES (?1)",
            [&sealed_claim.claim.id],
        )?;
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
    transaction.execute_batch(
        "DELETE FROM claims WHERE id NOT IN (SELECT id FROM temp.sealed_claims);
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
    let before = reader_answers(runtime, &transaction, &subjects, sealed.cut_unix_ms)?;
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
    let after = reader_answers(runtime, &transaction, &subjects, sealed.cut_unix_ms)?;
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
    transaction.rollback()?;
    Ok(proof)
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
        {
            // Every local batch has an envelope before the planner reads them.
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            seed_replica_envelopes_tx(&transaction, &self.origin, None)?;
            transaction.commit()?;
        }
        let connection = self.readers.get();
        // One read transaction, so the envelopes, the claims and the high water agree.
        let connection = connection.unchecked_transaction()?;
        let seal_rowid: i64 = connection.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM replica_envelopes",
            [],
            |row| row.get(0),
        )?;
        let seal_rowid = through_rowid.map_or(seal_rowid, |through| through.min(seal_rowid));
        let mut envelopes = connection
            .prepare(
                "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                        envelopes.accepted_at_unix_ms,
                        (SELECT COUNT(*) FROM replica_records records
                         WHERE records.writer=envelopes.writer AND records.sequence=envelopes.sequence
                           AND records.envelope_hash=envelopes.envelope_hash)
                 FROM replica_envelopes envelopes WHERE envelopes.rowid <= ?1",
            )?
            .query_map([seal_rowid], |row| {
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
        envelopes.retain(|envelope| envelope.accepted_at_unix_ms < cut_unix_ms);
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
        let claims = connection
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS}, records.writer, records.sequence, records.envelope_hash,
                        records.state
                 FROM claims JOIN batches ON batches.id=claims.batch_id
                 JOIN replica_records records ON records.claim_id=claims.id
                 ORDER BY {CANONICAL_ORDER}"
            ))?
            .query_map([], |row| {
                Ok((
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
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            seed_replica_envelopes_tx(&transaction, &self.origin, None)?;
            transaction.commit()?;
        }
        let connection = self.readers.get();
        let connection = connection.unchecked_transaction()?;
        let seal_rowid: i64 = connection.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM replica_envelopes",
            [],
            |row| row.get(0),
        )?;
        let seal_rowid = through_rowid.map_or(seal_rowid, |through| through.min(seal_rowid));
        let cut = i64::try_from(cut_unix_ms)?;
        // As in `checkpoint_sealed_set_through`: an envelope is before the cut when it and every
        // claim admitted from it are dated before the cut.
        let identities = connection
            .prepare(
                "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash
                 FROM replica_envelopes AS envelopes
                 WHERE envelopes.rowid <= ?2
                   AND CAST(envelopes.accepted_at_unix_ms AS INTEGER) < ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM replica_records AS records
                       JOIN claims ON claims.id=records.claim_id
                       WHERE records.writer=envelopes.writer
                         AND records.sequence=envelopes.sequence
                         AND records.envelope_hash=envelopes.envelope_hash
                         AND CAST(claims.accepted_at_unix_ms AS INTEGER) >= ?1)
                 UNION
                 SELECT writer, sequence, envelope_hash FROM checkpoint_envelopes
                 WHERE accepted_at_unix_ms < ?1",
            )?
            .query_map(params![cut, seal_rowid], |row| {
                Ok(EnvelopeKey {
                    writer: row.get(0)?,
                    sequence: row.get(1)?,
                    envelope_hash: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
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
