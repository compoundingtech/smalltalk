//! Sealing this node's batches in two halves.
//!
//! [`Store::prepare_seal`] reads the next chunk of unsealed batches in one read transaction and
//! does all the work that needs no writer. It reads each batch's claims and the blobs they name,
//! signs each claim that has no stored signature, encodes and hashes each envelope, encodes each
//! record, and signs each envelope with the member key.
//!
//! [`Store::seal_prepared_tx`] runs under the writer. It writes the prepared rows in the order
//! `seed_replica_envelopes_signed_tx` always has, but only after reading again every fact the
//! preparation read and finding each unchanged:
//! - the chunk's unsealed batches;
//! - each batch's claims;
//! - which named blobs are stored;
//! - each claim's stored signature;
//! - the member key and the fleet id;
//! - for the claims it signed itself, who the keyring would have them signed as now: the key, the
//!   principal, the principal acted for and the chain. Preparation reduces those claims to their
//!   distinct signer inputs (a key grant's issuer and issuer key, and the actor; see
//!   `SignerInput`), so the writer selects once per input, not once per claim, in one settled
//!   view of the keyring (see `Keyring::settled`). A change to keys no claim of the chunk is
//!   signed with does not matter.
//!
//! When any fact has changed, the transaction is not committed and the caller prepares the
//! chunk again, off the writer. A committed chunk therefore holds what sealing under the writer
//! would have stored from the same state, apart from the nonces of claim signatures made now.
//! A retry follows a change to one of these facts, a keyring change that overlaps the writer's
//! brief read of it, or a preparation handed to another store.
//!
//! Claims and blobs are compared by id and hash, not by content: both are content-addressed and
//! never rewritten in place, so equal ids name equal bytes. The rereads do not detect a raw
//! rewrite of a row under the same id.
//!
//! `seed_replica_envelopes_signed_tx` itself prepares and writes each batch in one transaction,
//! so its other callers store exactly what they did before.

use super::*;
use crate::principal::ClaimSignature;

/// Batches one sealing pass seals at most.
pub const SEAL_CHUNK_BATCHES: usize = 64;
/// Payload bytes one sealing pass prepares before it takes the writer: a budget on payload, not
/// a bound on resident memory. A batch larger than this is prepared alone.
pub const SEAL_PREPARED_BYTES: usize = 16 * 1024 * 1024;

/// How sealing signs a claim that has no stored signature.
pub(crate) type SignUnsealed<'a> = &'a dyn Fn(&principals::Unsealed<'_>) -> Option<ClaimSignature>;

/// A batch with no envelope yet, as `batches` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnsealedBatch {
    pub(crate) rowid: i64,
    pub(crate) id: String,
    pub(crate) writer: String,
    pub(crate) sequence: u64,
    pub(crate) previous_hash: Option<String>,
    pub(crate) hash: String,
    pub(crate) accepted_at: String,
}

/// The batches in `(after_rowid, through_rowid]` that have no envelope, in the order sealing
/// writes them: by rowid after a frontier, and by writer and sequence without one.
pub(crate) fn unsealed_batches(
    connection: &Connection,
    after_rowid: Option<i64>,
    through_rowid: Option<i64>,
) -> Result<Vec<UnsealedBatch>> {
    let order = if after_rowid.is_some() {
        "batches.rowid"
    } else {
        "origin, replica_sequence, id"
    };
    let mut statement = connection.prepare(&format!(
        "SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms,
                batches.rowid
         FROM batches
         WHERE batches.rowid>=?1 AND batches.rowid<=?2
           AND NOT EXISTS (
             SELECT 1 FROM replica_envelopes WHERE replica_envelopes.batch_id=batches.id
         )
         ORDER BY {order}"
    ))?;
    let batches = statement
        .query_map(
            params![
                after_rowid.map_or(i64::MIN, |rowid| rowid.saturating_add(1)),
                through_rowid.unwrap_or(i64::MAX)
            ],
            |row| {
                Ok(UnsealedBatch {
                    id: row.get(0)?,
                    writer: row.get(1)?,
                    sequence: row.get(2)?,
                    previous_hash: row.get(3)?,
                    hash: row.get(4)?,
                    accepted_at: row.get(5)?,
                    rowid: row.get(6)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(batches)
}

/// The envelopes of `origin` whose batches follow `after_batch_rowid` (or, without it, the
/// envelope rows after it), through `through_batch_rowid`, that `member_key` has not signed.
pub(crate) fn own_unsigned_envelopes(
    connection: &Connection,
    origin: &str,
    member_key: &str,
    after_batch_rowid: Option<i64>,
    through_batch_rowid: Option<i64>,
) -> Result<Vec<(u64, String)>> {
    let from = if after_batch_rowid.is_some() {
        "FROM batches CROSS JOIN replica_envelopes AS envelopes
         ON envelopes.batch_id=batches.id WHERE batches.rowid>?2 AND batches.rowid<=?4 AND envelopes.writer=?1"
    } else {
        "FROM replica_envelopes AS envelopes WHERE envelopes.writer=?1 AND envelopes.rowid>?2 AND envelopes.rowid<=?4"
    };
    let mut statement = connection.prepare(&format!(
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
                origin,
                after_batch_rowid.unwrap_or(i64::MIN),
                member_key,
                through_batch_rowid.unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(unsigned)
}

/// One record of a sealed batch: a claim, encoded as the envelope carries it.
pub(crate) struct SealedRecord {
    claim_id: String,
    subject: String,
    kind: String,
    raw: Vec<u8>,
}

/// One batch sealed into an envelope, with what sealing read to make it.
pub(crate) struct SealedBatch {
    pub(crate) batch: UnsealedBatch,
    /// Each claim's id and store index, in store order.
    claims: Vec<(String, u64)>,
    /// Each blob hash the claims name, and whether `blobs` held it. A named blob that is not
    /// stored is left out of the payload.
    named_blobs: Vec<(String, bool)>,
    /// Each claim's `claim_signatures` text, in claim order.
    stored_signatures: Vec<Option<String>>,
    /// The signature each signed claim carries, stored before or made now.
    signatures: Vec<(String, ClaimSignature)>,
    pub(crate) payload: Vec<u8>,
    pub(crate) envelope_hash: String,
    records: Vec<SealedRecord>,
    /// For each claim this sealing signed itself, having found no stored signature: its signer
    /// input and the signature made (`None` when the keyring had no key to sign with).
    fresh: Vec<(principals::SignerInput, Option<ClaimSignature>)>,
}

/// Whether two signatures name the same signer: key, principal, principal acted for and chain.
fn same_signer(one: &Option<ClaimSignature>, other: &Option<ClaimSignature>) -> bool {
    match (one, other) {
        (Some(one), Some(other)) => {
            one.key == other.key
                && one.signer == other.signer
                && one.on_behalf == other.on_behalf
                && one.chain == other.chain
        }
        (None, None) => true,
        _ => false,
    }
}

/// Seal `batch`: read its claims and the blobs they name, sign each claim without a stored
/// signature with `sign`, and encode and hash the envelope and its records. Writes nothing.
pub(crate) fn seal_batch(
    connection: &Connection,
    batch: UnsealedBatch,
    sign: Option<SignUnsealed<'_>>,
) -> Result<SealedBatch> {
    let mut claim_statement = connection.prepare(
        "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
         FROM claims WHERE batch_id=?1 ORDER BY store_index",
    )?;
    let claims = claim_statement
        .query_map([&batch.id], claim_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(claim_statement);
    let mut blobs = BTreeMap::new();
    collect_referenced_blobs(connection, &claims, &mut blobs)?;
    let mut named = BTreeSet::new();
    for claim in &claims {
        collect_hash_fields(&claim.body, &mut named);
    }
    let named_blobs = named
        .into_iter()
        .map(|hash| {
            let stored = blobs.contains_key(&hash);
            (hash, stored)
        })
        .collect();
    let mut stored_signatures = Vec::with_capacity(claims.len());
    let mut signatures = Vec::new();
    let mut claim_signatures = BTreeMap::new();
    let mut fresh = Vec::new();
    for claim in &claims {
        let stored = connection
            .prepare_cached("SELECT signature FROM claim_signatures WHERE claim_id=?1")?
            .query_row([&claim.id], |row| row.get::<_, String>(0))
            .optional()?;
        let existing = stored
            .as_deref()
            .and_then(|text| serde_json::from_str::<ClaimSignature>(text).ok());
        let signature = match (existing, sign) {
            (Some(signature), _) => Some(signature),
            (None, Some(sign)) => {
                let unsealed = principals::Unsealed {
                    subject: &claim.subject,
                    kind: &claim.kind,
                    actor: claim.actor.as_deref(),
                    body: &claim.body,
                    accepted_at_unix_ms: claim.accepted_at_unix_ms,
                };
                let signature = sign(&unsealed);
                fresh.push((principals::SignerInput::of(&unsealed), signature.clone()));
                signature
            }
            (None, None) => None,
        };
        stored_signatures.push(stored);
        if let Some(signature) = signature {
            claim_signatures.insert(claim.id.clone(), signature.clone());
            signatures.push((claim.id.clone(), signature));
        }
    }
    let mut records = Vec::with_capacity(claims.len());
    for claim in &claims {
        let mut raw = Vec::new();
        ciborium::into_writer(claim, &mut raw)?;
        records.push(SealedRecord {
            claim_id: claim.id.clone(),
            subject: claim.subject.clone(),
            kind: claim.kind.clone(),
            raw,
        });
    }
    let claim_rows = claims
        .iter()
        .map(|claim| (claim.id.clone(), claim.store_index))
        .collect();
    let accepted_at: u128 = batch.accepted_at.parse().unwrap_or_default();
    let mut payload = Vec::new();
    ciborium::into_writer(
        &ReplicaEnvelopePayload {
            batch: ReplicaBatch {
                id: batch.id.clone(),
                origin: batch.writer.clone(),
                replica_sequence: batch.sequence,
                previous_hash: batch.previous_hash.clone(),
                hash: batch.hash.clone(),
                accepted_at_unix_ms: accepted_at,
                claims,
            },
            blobs,
            claim_signatures,
        },
        &mut payload,
    )?;
    let envelope_hash = replica_envelope_hash(
        &batch.writer,
        batch.sequence,
        batch.previous_hash.as_deref(),
        accepted_at,
        &payload,
    );
    Ok(SealedBatch {
        batch,
        claims: claim_rows,
        named_blobs,
        stored_signatures,
        signatures,
        payload,
        envelope_hash,
        records,
        fresh,
    })
}

/// Store a sealed batch: each claim's signature, the envelope as validated, and one record per
/// claim.
pub(crate) fn write_sealed_batch_tx(
    transaction: &Connection,
    relay: &str,
    sealed: &SealedBatch,
) -> Result<()> {
    for (claim_id, signature) in &sealed.signatures {
        principals::store_claim_signature_tx(transaction, claim_id, signature)?;
    }
    let batch = &sealed.batch;
    transaction.execute(
        "INSERT OR IGNORE INTO replica_envelopes(
             writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms,
             payload, batch_id, relay, receipt_state, received_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'validated', ?5)",
        params![
            batch.writer,
            batch.sequence,
            sealed.envelope_hash,
            batch.previous_hash,
            batch.accepted_at,
            sealed.payload,
            batch.id,
            relay
        ],
    )?;
    for (position, record) in sealed.records.iter().enumerate() {
        transaction.execute(
            "INSERT OR IGNORE INTO replica_records(
                 record_ref, writer, sequence, envelope_hash, position, raw, state,
                 claim_id, subject_hint, kind_hint, updated_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'valid', ?7, ?8, ?9, ?10)",
            params![
                replica_record_ref(
                    &batch.writer,
                    batch.sequence,
                    &sealed.envelope_hash,
                    position as u64
                ),
                batch.writer,
                batch.sequence,
                sealed.envelope_hash,
                position as u64,
                record.raw,
                record.claim_id,
                record.subject,
                record.kind,
                batch.accepted_at,
            ],
        )?;
    }
    Ok(())
}

/// Whether the claims, named blobs and stored signatures `sealed` was made from are what the
/// store holds now.
fn sealed_batch_current(connection: &Connection, sealed: &SealedBatch) -> Result<bool> {
    let claims = connection
        .prepare_cached(
            "SELECT id, store_index FROM claims WHERE batch_id=?1 ORDER BY store_index",
        )?
        .query_map([&sealed.batch.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if claims != sealed.claims {
        return Ok(false);
    }
    for (hash, stored) in &sealed.named_blobs {
        let now: bool = connection
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)")?
            .query_row([hash], |row| row.get(0))?;
        if now != *stored {
            return Ok(false);
        }
    }
    for ((claim_id, _), stored) in claims.iter().zip(&sealed.stored_signatures) {
        let now = connection
            .prepare_cached("SELECT signature FROM claim_signatures WHERE claim_id=?1")?
            .query_row([claim_id], |row| row.get::<_, String>(0))
            .optional()?;
        if now != *stored {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What sealing read to sign envelopes: the member key's public half and the fleet id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnvelopeSigner {
    member_key: String,
    fleet_id: Option<String>,
}

/// The next chunk of this node's batches, sealed and signed without the writer.
pub(crate) struct PreparedSeal {
    pub(crate) seeded_through: i64,
    pub(crate) through: i64,
    /// The store it was prepared from: a preparation is written only to its own store.
    origin: String,
    signer: Option<EnvelopeSigner>,
    pub(crate) batches: Vec<SealedBatch>,
    /// Each distinct signer input among the claims the chunk signed itself, with the signer
    /// preparation signed them as.
    signers: BTreeMap<principals::SignerInput, Option<ClaimSignature>>,
    /// Whether every claim with the same signer input was signed by the same signer. Not when a
    /// keyring change landed while preparation signed.
    signers_agree: bool,
    /// A member-key signature for each envelope the writer may find unsigned: those unsigned
    /// at preparation and every envelope this chunk seals for this node.
    envelope_signatures: BTreeMap<(u64, String), String>,
}

impl Store {
    fn envelope_signer(
        &self,
        connection: &Connection,
    ) -> Result<Option<(Arc<crate::fleet::MemberKey>, EnvelopeSigner)>> {
        let Some(key) = self
            .member_key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return Ok(None);
        };
        let fleet_id = fleet_meta(connection, "fleet_id")?;
        let signer = EnvelopeSigner {
            member_key: key.public().to_owned(),
            fleet_id,
        };
        Ok(Some((key, signer)))
    }

    /// Prepare the batches after the sealing frontier, through at most `SEAL_CHUNK_BATCHES`
    /// batches up to `target`, keeping at most `budget` bytes of payload: a batch that would take
    /// the chunk past it is left for the next chunk, unless it is the first, which is kept alone.
    /// `None` when the frontier has reached `target`.
    pub(crate) fn prepare_seal(&self, target: i64, budget: usize) -> Result<Option<PreparedSeal>> {
        let seeded_through = self.seeded_batch_rowid.load(Ordering::Acquire);
        if seeded_through >= target {
            return Ok(None);
        }
        let reader = self.readers.get();
        let snapshot = reader.unchecked_transaction()?;
        let chunk_through: i64 = snapshot.query_row(
            "SELECT COALESCE(MAX(rowid), ?2) FROM (
                SELECT rowid FROM batches WHERE rowid>?1 AND rowid<=?2
                ORDER BY rowid LIMIT ?3)",
            params![seeded_through, target, SEAL_CHUNK_BATCHES],
            |row| row.get(0),
        )?;
        let mut through = chunk_through;
        let mut batches: Vec<SealedBatch> = Vec::new();
        let mut bytes = 0;
        for batch in unsealed_batches(&snapshot, Some(seeded_through), Some(chunk_through))? {
            let sealed = seal_batch(
                &snapshot,
                batch,
                Some(&|claim: &principals::Unsealed<'_>| self.sign_unsealed(claim)),
            )?;
            if !batches.is_empty() && bytes + sealed.payload.len() > budget {
                // This batch would take the chunk past its budget, so it starts the next chunk,
                // prepared again off the writer. The frontier stops after the last batch kept.
                through = batches
                    .last()
                    .map_or(seeded_through, |kept| kept.batch.rowid);
                break;
            }
            bytes += sealed.payload.len();
            batches.push(sealed);
        }
        let mut signers = BTreeMap::new();
        let mut signers_agree = true;
        for (input, signature) in batches.iter().flat_map(|sealed| &sealed.fresh) {
            match signers.get(input) {
                Some(seen) => signers_agree &= same_signer(seen, signature),
                None => {
                    signers.insert(input.clone(), signature.clone());
                }
            }
        }
        let signer = self.envelope_signer(&snapshot)?;
        let mut envelope_signatures = BTreeMap::new();
        if let Some((
            key,
            EnvelopeSigner {
                fleet_id: Some(fleet_id),
                ..
            },
        )) = &signer
        {
            let _timing = time_stage(&self.replication_timers.signing);
            let mut unsigned = own_unsigned_envelopes(
                &snapshot,
                &self.origin,
                key.public(),
                Some(seeded_through),
                Some(through),
            )?;
            unsigned.extend(
                batches
                    .iter()
                    .filter(|sealed| sealed.batch.writer == self.origin)
                    .map(|sealed| (sealed.batch.sequence, sealed.envelope_hash.clone())),
            );
            for (sequence, envelope_hash) in unsigned {
                let signature = key.sign(&crate::fleet::envelope_signature_message(
                    fleet_id,
                    &self.origin,
                    sequence,
                    &envelope_hash,
                ));
                envelope_signatures.insert((sequence, envelope_hash), signature);
            }
        }
        Ok(Some(PreparedSeal {
            seeded_through,
            through,
            origin: self.origin.clone(),
            signer: signer.map(|(_, signer)| signer),
            batches,
            signers,
            signers_agree,
            envelope_signatures,
        }))
    }

    /// Under the writer, write `prepared` when nothing it was prepared from has changed, and
    /// return whether it did. When this returns `false`, the caller must not commit
    /// `transaction`: it may hold part of the chunk.
    pub(crate) fn seal_prepared_tx(
        &self,
        transaction: &Transaction<'_>,
        prepared: &PreparedSeal,
    ) -> Result<bool> {
        if prepared.origin != self.origin {
            return Ok(false);
        }
        let signer = self.envelope_signer(transaction)?;
        if signer.as_ref().map(|(_, signer)| signer) != prepared.signer.as_ref() {
            return Ok(false);
        }
        let unsealed = unsealed_batches(
            transaction,
            Some(prepared.seeded_through),
            Some(prepared.through),
        )?;
        if unsealed.len() != prepared.batches.len()
            || unsealed
                .iter()
                .zip(&prepared.batches)
                .any(|(now, sealed)| *now != sealed.batch)
        {
            return Ok(false);
        }
        for sealed in &prepared.batches {
            if !sealed_batch_current(transaction, sealed)? {
                return Ok(false);
            }
        }
        // Each claim sealing signed itself is signed as the keyring would sign it now: one
        // selection per distinct signer input. A change to the keyring in progress or
        // overlapping this read counts as a change.
        if !prepared.signers_agree {
            return Ok(false);
        }
        if !prepared.signers.is_empty() {
            let signers_unchanged = self.keyring.settled(|| {
                prepared.signers.iter().all(|(input, signature)| {
                    #[cfg(test)]
                    SIGNERS_SELECTED.with(|count| count.set(count.get() + 1));
                    match (self.claim_signer_for(input), signature) {
                        (Some(signer), Some(signature)) => signer.signed(signature),
                        (None, None) => true,
                        _ => false,
                    }
                })
            });
            if signers_unchanged != Some(true) {
                return Ok(false);
            }
        }
        for sealed in &prepared.batches {
            write_sealed_batch_tx(transaction, &self.origin, sealed)?;
        }
        if let Some((
            key,
            EnvelopeSigner {
                fleet_id: Some(_), ..
            },
        )) = &signer
        {
            let now = now_ms().to_string();
            for envelope in own_unsigned_envelopes(
                transaction,
                &self.origin,
                key.public(),
                Some(prepared.seeded_through),
                Some(prepared.through),
            )? {
                let Some(signature) = prepared.envelope_signatures.get(&envelope) else {
                    return Ok(false);
                };
                transaction.execute(
                    "INSERT OR IGNORE INTO replica_envelope_signatures(
                         writer, sequence, envelope_hash, member_key, signature, stored_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        self.origin,
                        envelope.0,
                        envelope.1,
                        key.public(),
                        signature,
                        now
                    ],
                )?;
            }
        }
        Ok(true)
    }
}
