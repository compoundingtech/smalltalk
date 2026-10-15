//! Admitting a replicated envelope in two halves.
//!
//! [`prepare_envelope`] does everything that depends only on the envelope's own bytes: it decodes
//! the payload, checks the envelope and batch hashes, digests each blob, encodes each claim, and
//! classifies each claim when the runtime can do that without the store. It needs no connection,
//! so admission runs it before taking the writer.
//!
//! [`admit_prepared_envelope_tx`] does the rest under the writer, in the order
//! `validate_and_admit_envelope_tx` always has: every read of stored state (prior record states,
//! blobs, batches, tombstones) and every write. A prepared failure surfaces at the same point and
//! with the same error as before, ahead of any write, so the two halves together admit exactly
//! what the single pass did.
//!
//! A preparation owns what it was made from: the envelope's key and the runtime that classified
//! it. Admission takes nothing else, so it cannot write one envelope's records under another's
//! key or apply one runtime's classification under another.
//!
//! An envelope's key names its bytes (the hash covers the payload), so a preparation can never
//! describe other content than the row it was made from. Admission also compares, under the
//! writer, the row it prepared from with the row that is there (see [`EnvelopeRow`]); an envelope
//! whose row is seen to differ stays pending and is prepared again on a later pass, off the
//! writer. Every mutable fact is read again under the writer either way.

use super::*;

/// The `replica_envelopes` row a preparation was made from: its rowid and receipt time. A row
/// deleted and received again after preparation usually differs in one or both, but neither is
/// a token that can never repeat (SQLite can reuse the largest rowid, and two receipts can share
/// a millisecond). A difference proves the preparation stale; equality does not prove it fresh.
/// Only pure content is reused, and the envelope hash binds that either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeRow {
    pub rowid: i64,
    pub received_at_unix_ms: String,
}

/// One envelope, prepared for admission without the store.
pub struct PreparedEnvelope<'r> {
    writer: String,
    sequence: u64,
    hash: String,
    /// The runtime that classified it, and that classifies under the writer what it could not.
    runtime: &'r dyn Runtime,
    /// The row this was prepared from, when the caller read one.
    pub row: Option<EnvelopeRow>,
    /// Time spent decoding, hashing, encoding and classifying.
    pub verify: std::time::Duration,
    /// The envelope's payload bytes, which its decoded content roughly doubles while it waits.
    pub bytes: usize,
    content: Result<PreparedPayload, St3Error>,
}

struct EnvelopeKey {
    writer: String,
    sequence: u64,
    hash: String,
}

struct PreparedPayload {
    payload: ReplicaEnvelopePayload,
    blobs: Vec<PreparedBlob>,
    claims: Vec<PreparedClaim>,
}

struct PreparedBlob {
    position: u64,
    record_ref: String,
    valid: bool,
}

struct PreparedClaim {
    record_ref: String,
    raw: Result<Vec<u8>, St3Error>,
    body: Result<String, St3Error>,
    predecessors: Result<String, St3Error>,
    check: ClaimCheck,
}

/// What the claim checks that need no store found.
enum ClaimCheck {
    /// The claim names another batch or writer, or its id is not its content hash.
    Failed(St3Error),
    /// The runtime classified it without the store.
    Classified(Result<ReplicatedClaimAdmission, St3Error>),
    /// The runtime classifies with the store, under the writer.
    Unclassified,
}

/// Decode, verify, encode and classify `envelope` without the store.
pub fn prepare_envelope<'r>(
    envelope: &ReplicaEnvelope,
    runtime: &'r dyn Runtime,
    row: Option<EnvelopeRow>,
) -> PreparedEnvelope<'r> {
    let started = std::time::Instant::now();
    let content = prepare_payload(envelope, runtime);
    PreparedEnvelope {
        writer: envelope.writer.clone(),
        sequence: envelope.sequence,
        hash: envelope.hash.clone(),
        runtime,
        row,
        verify: started.elapsed(),
        bytes: envelope.payload.bytes().map_or(0, <[u8]>::len),
        content,
    }
}

fn prepare_payload(
    envelope: &ReplicaEnvelope,
    runtime: &dyn Runtime,
) -> Result<PreparedPayload, St3Error> {
    let payload_bytes = envelope.payload.bytes().map_err(|error| {
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
        payload_bytes,
    );
    if expected_hash != envelope.hash {
        return Err(St3Error::new(
            "envelope-hash-mismatch",
            "the envelope hash does not match its payload",
        ));
    }
    let payload: ReplicaEnvelopePayload =
        ciborium::from_reader(payload_bytes).map_err(|error| {
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
    let blobs = payload
        .blobs
        .iter()
        .enumerate()
        .map(|(offset, (hash, bytes))| {
            let position = batch.claims.len() as u64 + offset as u64;
            PreparedBlob {
                position,
                record_ref: replica_record_ref(
                    &envelope.writer,
                    envelope.sequence,
                    &envelope.hash,
                    position,
                ),
                valid: hex::encode(Sha256::digest(bytes)) == *hash,
            }
        })
        .collect();
    let claims = batch
        .claims
        .iter()
        .enumerate()
        .map(|(position, claim)| {
            let mut raw = Vec::new();
            let raw = ciborium::into_writer(claim, &mut raw)
                .map(|()| raw)
                .map_err(internal);
            PreparedClaim {
                record_ref: replica_record_ref(
                    &envelope.writer,
                    envelope.sequence,
                    &envelope.hash,
                    position as u64,
                ),
                raw,
                body: canonical_json_text(&claim.body).map_err(internal),
                predecessors: serde_json::to_string(&claim.predecessors).map_err(internal),
                check: check_claim(batch, claim, runtime),
            }
        })
        .collect();
    Ok(PreparedPayload {
        payload,
        blobs,
        claims,
    })
}

/// The part of `validate_replicated_claim` that reads no stored state, in its order.
fn check_claim(batch: &ReplicaBatch, claim: &ClaimRecord, runtime: &dyn Runtime) -> ClaimCheck {
    if claim.batch_id != batch.id || claim.origin != batch.origin {
        return ClaimCheck::Failed(St3Error::new(
            "claim-batch-mismatch",
            format!(
                "replicated claim `{}` names another batch or writer",
                claim.id
            ),
        ));
    }
    match claim_id_is_content_hash(claim) {
        Ok(true) => {}
        Ok(false) => {
            return ClaimCheck::Failed(St3Error::new(
                "claim-hash-mismatch",
                format!("replicated claim `{}` failed verification", claim.id),
            ));
        }
        Err(error) => return ClaimCheck::Failed(internal(error)),
    }
    match runtime.classify_replicated_claim_unbound(batch, claim) {
        Some(classification) => ClaimCheck::Classified(classification),
        None => ClaimCheck::Unclassified,
    }
}

/// The part of `validate_replicated_claim` that reads stored state: a runtime that needs the
/// store classifies here, and a valid claim's blobs must be stored.
fn finish_claim_check(
    transaction: &Connection,
    batch: &ReplicaBatch,
    claim: &ClaimRecord,
    runtime: &dyn Runtime,
    check: ClaimCheck,
) -> Result<ReplicatedClaimAdmission, St3Error> {
    let admission = match check {
        ClaimCheck::Failed(error) => return Err(error),
        ClaimCheck::Classified(classification) => classification?,
        ClaimCheck::Unclassified => runtime.classify_replicated_claim(transaction, batch, claim)?,
    };
    if matches!(admission, ReplicatedClaimAdmission::Valid) {
        ensure_claim_blobs(transaction, claim)?;
    }
    Ok(admission)
}

/// Admit a prepared envelope: the reads of stored state and the writes, in the order
/// `validate_and_admit_envelope_tx` has always made them, under the key and with the runtime it
/// was prepared from. `outcome.verify` is not charged for the preparation here; the caller adds
/// `prepared.verify` where it prepared.
pub fn admit_prepared_envelope_tx(
    transaction: &Connection,
    prepared: PreparedEnvelope<'_>,
    outcome: &mut ReplicationAdmission,
) -> Result<(), St3Error> {
    let PreparedEnvelope {
        writer,
        sequence,
        hash: envelope_hash,
        runtime,
        content,
        ..
    } = prepared;
    // The writes below name the envelope as the old body did, by these three fields.
    let envelope = EnvelopeKey {
        writer,
        sequence,
        hash: envelope_hash,
    };
    let PreparedPayload {
        payload,
        blobs,
        claims,
    } = content?;
    let batch = &payload.batch;
    let now = now_ms().to_string();
    let mut degraded = false;
    for (blob, (hash, bytes)) in blobs.iter().zip(&payload.blobs) {
        let PreparedBlob {
            position,
            record_ref,
            valid,
        } = blob;
        let previous_state = transaction
            .prepare_cached("SELECT state FROM replica_records WHERE record_ref=?1")
            .map_err(internal)?
            .query_row([record_ref], |row| row.get::<_, String>(0))
            .optional()
            .map_err(internal)?;
        if *valid {
            transaction
                .prepare_cached(
                    "INSERT OR IGNORE INTO blobs(hash, bytes, size) VALUES (?1, ?2, ?3)",
                )
                .map_err(internal)?
                .execute(params![hash, bytes, bytes.len() as u64])
                .map_err(internal)?;
            transaction
                .prepare_cached(
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
                )
                .map_err(internal)?
                .execute(params![
                        record_ref,
                        envelope.writer,
                        envelope.sequence,
                        envelope.hash,
                        position,
                        bytes,
                        format!("blob/{hash}"),
                        now_ms().to_string(),
                    ])
                .map_err(internal)?;
            outcome.valid += 1;
            outcome.changed |= previous_state.as_deref() != Some("valid");
        } else {
            degraded = true;
            transaction
                .prepare_cached(
                    "INSERT INTO replica_records(
                         record_ref, writer, sequence, envelope_hash, position, raw, state,
                         subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'invalid', ?7, 'blob',
                               'blob-hash-mismatch', ?8, ?9)
                     ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'invalid' END,
                        subject_hint=excluded.subject_hint, kind_hint='blob', error_code=excluded.error_code,
                        error_message=excluded.error_message, updated_at_unix_ms=excluded.updated_at_unix_ms",
                )
                .map_err(internal)?
                .execute(params![
                        record_ref,
                        envelope.writer,
                        envelope.sequence,
                        envelope.hash,
                        position,
                        bytes,
                        format!("blob/{hash}"),
                        format!("replicated blob `{hash}` failed verification"),
                        now_ms().to_string(),
                    ])
                .map_err(internal)?;
            outcome.invalid += 1;
        }
    }
    let batch_rowid = transaction
        .query_row(
            "SELECT rowid FROM batches WHERE id=?1",
            [&batch.id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(internal)?
        .map_or_else(|| next_batch_rowid(transaction).map_err(internal), Ok)?;
    transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms, rowid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .map_err(internal)?
        .execute(params![
                batch.id,
                batch.origin,
                batch.replica_sequence,
                batch.previous_hash,
                batch.hash,
                batch.accepted_at_unix_ms.to_string(),
                batch_rowid,
            ])
        .map_err(internal)?;
    for (position, (claim, prepared)) in batch.claims.iter().zip(claims).enumerate() {
        let position = position as u64;
        let PreparedClaim {
            record_ref,
            raw,
            body,
            predecessors,
            check,
        } = prepared;
        let raw = raw?;
        let previous_state = transaction
            .prepare_cached("SELECT state FROM replica_records WHERE record_ref=?1")
            .map_err(internal)?
            .query_row([&record_ref], |row| row.get::<_, String>(0))
            .optional()
            .map_err(internal)?;
        let started = std::time::Instant::now();
        let classification = finish_claim_check(transaction, batch, claim, runtime, check);
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
                    .prepare_cached(
                        "INSERT OR IGNORE INTO claims(id, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )
                    .map_err(internal)?
                    .execute(params![
                        claim.id,
                        claim.batch_id,
                        claim.subject,
                        claim.kind,
                        claim.origin,
                        claim.actor,
                        body?,
                        predecessors?,
                        claim.accepted_at_unix_ms.to_string(),
                    ])
                    .map_err(internal)?
                };
                if let Some(signature) = payload.claim_signatures.get(&claim.id) {
                    principals::store_claim_signature_tx(transaction, &claim.id, signature)
                        .map_err(internal)?;
                }
                transaction
                    .prepare_cached(
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
                    )
                    .map_err(internal)?
                    .execute(params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, now])
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
                    .prepare_cached(
                        "INSERT INTO replica_records(
                             record_ref, writer, sequence, envelope_hash, position, raw, state,
                             claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'unknown', ?7, ?8, ?9, ?10, ?11, ?12)
                         ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'unknown' END,
                            error_code=CASE WHEN state='repaired' THEN error_code ELSE excluded.error_code END,
                            error_message=CASE WHEN state='repaired' THEN error_message ELSE excluded.error_message END,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                    )
                    .map_err(internal)?
                    .execute(params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, error_code, error_message, now])
                    .map_err(internal)?;
                outcome.unknown += 1;
            }
            Err(error) => {
                degraded = true;
                transaction
                    .prepare_cached(
                        "INSERT INTO replica_records(
                             record_ref, writer, sequence, envelope_hash, position, raw, state,
                             claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'invalid', ?7, ?8, ?9, ?10, ?11, ?12)
                         ON CONFLICT(record_ref) DO UPDATE SET state=CASE WHEN state='repaired' THEN state ELSE 'invalid' END,
                            error_code=excluded.error_code, error_message=excluded.error_message,
                            updated_at_unix_ms=excluded.updated_at_unix_ms",
                    )
                    .map_err(internal)?
                    .execute(params![record_ref, envelope.writer, envelope.sequence, envelope.hash, position, raw, claim.id, claim.subject, claim.kind, error.code, error.message, now])
                    .map_err(internal)?;
                outcome.invalid += 1;
            }
        }
    }
    transaction
        .prepare_cached(
            "UPDATE replica_envelopes SET receipt_state=?4, validation_error=NULL, batch_id=?5
             WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
        )
        .map_err(internal)?
        .execute(params![
            envelope.writer,
            envelope.sequence,
            envelope.hash,
            if degraded { "degraded" } else { "validated" },
            batch.id,
        ])
        .map_err(internal)?;
    Ok(())
}
