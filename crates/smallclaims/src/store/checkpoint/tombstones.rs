//! Tombstones: what a node keeps of the envelopes and claims a checkpoint dropped, and the
//! manifest a node that did not take part adopts them from. Inventories list a dropped
//! envelope's identity, so the authority digest does not change and no peer sends it back.
//! Evidence checks, ancestry walks and idempotent retries read a dropped claim's tombstone.

use super::*;

/// The most tombstones one page of a checkpoint manifest carries.
pub const CHECKPOINT_MANIFEST_PAGE_LIMIT: usize = 10_000;

/// Every tombstone a node holds for one checkpoint and the checkpoints before it: what a node
/// that did not take part needs to adopt it. Its `drop_digest` must equal the one every
/// participant verified.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
}

/// Where a manifest page starts: after this envelope identity, or, once every envelope has been
/// listed, after this claim ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "part", content = "after")]
pub enum CheckpointManifestCursor {
    Envelope(EnvelopeKey),
    Claim(String),
}

/// One page of a checkpoint's manifest, as `/v1/peer/checkpoint` asks for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestRequest {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    #[serde(default)]
    pub after: Option<CheckpointManifestCursor>,
}

/// One page of a manifest. `next` is set while more tombstones follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestPage {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
    #[serde(default)]
    pub next: Option<CheckpointManifestCursor>,
}

impl CheckpointManifest {
    /// Add the next page of this manifest, and return where the following page starts.
    pub fn append(
        &mut self,
        page: CheckpointManifestPage,
    ) -> Result<Option<CheckpointManifestCursor>, St3Error> {
        if page.checkpoint != self.checkpoint || page.cut_unix_ms != self.cut_unix_ms {
            return Err(St3Error::new(
                "checkpoint-manifest-invalid",
                format!(
                    "a manifest page for `{}` does not continue `{}`",
                    page.checkpoint, self.checkpoint
                ),
            ));
        }
        self.envelopes.extend(page.envelopes);
        self.claims.extend(page.claims);
        Ok(page.next)
    }
}

pub fn invalid_manifest(message: impl Into<String>) -> St3Error {
    St3Error::new("checkpoint-manifest-invalid", message)
}

/// Check a manifest against the `drop_digest` that a stable checkpoint's participants verified,
/// before anything from it is stored. The digest covers every field of every tombstone, so a
/// changed field, or a tombstone added or left out, is rejected here.
pub fn verify_checkpoint_manifest(
    manifest: &CheckpointManifest,
    expected_drop_digest: &str,
) -> Result<(), St3Error> {
    let mut envelopes = BTreeSet::new();
    for envelope in &manifest.envelopes {
        if envelope.accepted_at_unix_ms >= manifest.cut_unix_ms {
            return Err(invalid_manifest(format!(
                "envelope {}/{} is not before the cut",
                envelope.writer, envelope.sequence
            )));
        }
        if !envelopes.insert((
            envelope.writer.as_str(),
            envelope.sequence,
            envelope.envelope_hash.as_str(),
        )) {
            return Err(invalid_manifest(format!(
                "envelope {}/{} is listed twice",
                envelope.writer, envelope.sequence
            )));
        }
    }
    let mut claims = BTreeSet::new();
    for claim in &manifest.claims {
        // A checkpoint drops a claim only with its whole envelope.
        if !envelopes.contains(&(
            claim.writer.as_str(),
            claim.sequence,
            claim.envelope_hash.as_str(),
        )) {
            return Err(invalid_manifest(format!(
                "claim `{}` names an envelope the manifest keeps",
                claim.id
            )));
        }
        if claim.accepted_at_unix_ms >= manifest.cut_unix_ms {
            return Err(invalid_manifest(format!(
                "claim `{}` is not before the cut",
                claim.id
            )));
        }
        if !claims.insert(claim.id.as_str()) {
            return Err(invalid_manifest(format!(
                "claim `{}` is listed twice",
                claim.id
            )));
        }
    }
    let digest = drop_digest(&manifest.envelopes, &manifest.claims);
    if digest != expected_drop_digest {
        return Err(St3Error::new(
            "checkpoint-manifest-mismatch",
            format!(
                "the manifest of `{}` does not match the drop its participants verified",
                manifest.checkpoint
            ),
        )
        .with_detail("drop_digest", digest)
        .with_detail("expected_drop_digest", expected_drop_digest.to_owned()));
    }
    Ok(())
}

pub fn row_millis(value: i64) -> u128 {
    u128::try_from(value).unwrap_or_default()
}

/// Whether a claim is stored here, or was and a checkpoint dropped it. Evidence may cite either.
pub fn claim_or_tombstone_exists(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1)
                 OR EXISTS(SELECT 1 FROM checkpoint_claims WHERE id=?1)",
        )?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a checkpoint dropped this claim here.
pub fn claim_tombstoned(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM checkpoint_claims WHERE id=?1)")?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a checkpoint dropped this envelope here.
pub fn envelope_tombstoned(
    connection: &Connection,
    writer: &str,
    sequence: u64,
    envelope_hash: &str,
) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM checkpoint_envelopes
                           WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3)",
        )?
        .query_row(params![writer, sequence, envelope_hash], |row| row.get(0))?)
}

/// The request digest and claim ID of each dropped claim of an operation.
pub fn checkpointed_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Vec<(String, String)>> {
    connection
        .prepare_cached(
            "SELECT request_digest, id FROM checkpoint_claims
             WHERE operation_id=?1 AND request_digest IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=checkpoint_claims.id)
             ORDER BY request_digest, id",
        )?
        .query_map([operation_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Every operation with a dropped claim, as `(operation, request digest, claim)`.
pub fn checkpointed_operations(connection: &Connection) -> Result<Vec<(String, String, String)>> {
    connection
        .prepare_cached(
            "SELECT operation_id, request_digest, id FROM checkpoint_claims
             WHERE operation_id IS NOT NULL AND request_digest IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=checkpoint_claims.id)",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

impl Store {
    /// One page of the manifest of a checkpoint: the tombstones this node holds for it and for
    /// every earlier checkpoint, envelopes first, each in sorted order. Checkpoints are named
    /// for UTC days, so their names sort in the order of their cuts.
    pub fn checkpoint_manifest_page(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.manifest_page(request, CHECKPOINT_MANIFEST_PAGE_LIMIT)
    }

    pub fn manifest_page(
        &self,
        request: &CheckpointManifestRequest,
        limit: usize,
    ) -> Result<CheckpointManifestPage> {
        let connection = self.readers.get();
        // One snapshot for the page, so a trim that commits meanwhile cannot tear it.
        let transaction = connection.unchecked_transaction()?;
        let mut page = CheckpointManifestPage {
            checkpoint: request.checkpoint.clone(),
            cut_unix_ms: request.cut_unix_ms,
            envelopes: Vec::new(),
            claims: Vec::new(),
            next: None,
        };
        let after_claim = match &request.after {
            None | Some(CheckpointManifestCursor::Envelope(_)) => {
                let (writer, sequence, hash) = match &request.after {
                    Some(CheckpointManifestCursor::Envelope(key)) => (
                        key.writer.as_str(),
                        i64::try_from(key.sequence).unwrap_or(i64::MAX),
                        key.envelope_hash.as_str(),
                    ),
                    _ => ("", i64::MIN, ""),
                };
                page.envelopes = transaction
                    .prepare_cached(
                        "SELECT writer, sequence, envelope_hash, accepted_at_unix_ms
                         FROM checkpoint_envelopes
                         WHERE checkpoint<=?1 AND (writer, sequence, envelope_hash) > (?2, ?3, ?4)
                         ORDER BY writer, sequence, envelope_hash LIMIT ?5",
                    )?
                    .query_map(
                        params![request.checkpoint, writer, sequence, hash, limit + 1],
                        |row| {
                            Ok(EnvelopeTombstone {
                                writer: row.get(0)?,
                                sequence: row.get(1)?,
                                envelope_hash: row.get(2)?,
                                accepted_at_unix_ms: row_millis(row.get(3)?),
                            })
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if page.envelopes.len() > limit {
                    page.envelopes.truncate(limit);
                    let last = page.envelopes.last().expect("the page is not empty");
                    page.next = Some(CheckpointManifestCursor::Envelope(EnvelopeKey {
                        writer: last.writer.clone(),
                        sequence: last.sequence,
                        envelope_hash: last.envelope_hash.clone(),
                    }));
                    return Ok(page);
                }
                String::new()
            }
            Some(CheckpointManifestCursor::Claim(after)) => after.clone(),
        };
        let room = limit - page.envelopes.len();
        page.claims = transaction
            .prepare_cached(
                "SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at_unix_ms
                 FROM checkpoint_claims WHERE checkpoint<=?1 AND id>?2
                 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![request.checkpoint, after_claim, room + 1], |row| {
                Ok((
                    ClaimTombstone {
                        id: row.get(0)?,
                        writer: row.get(1)?,
                        sequence: row.get(2)?,
                        envelope_hash: row.get(3)?,
                        subject: row.get(4)?,
                        kind: row.get(5)?,
                        actor: row.get(6)?,
                        predecessors: Vec::new(),
                        operation_id: row.get(8)?,
                        request_digest: row.get(9)?,
                        accepted_at_unix_ms: row_millis(row.get(10)?),
                    },
                    row.get::<_, String>(7)?,
                ))
            })?
            .map(|row| {
                let (mut claim, predecessors) = row?;
                claim.predecessors = serde_json::from_str(&predecessors)?;
                Ok(claim)
            })
            .collect::<Result<Vec<_>>>()?;
        if page.claims.len() > room {
            page.claims.truncate(room);
            if let Some(last) = page.claims.last() {
                page.next = Some(CheckpointManifestCursor::Claim(last.id.clone()));
            } else {
                page.next = Some(CheckpointManifestCursor::Claim(after_claim));
            }
        }
        Ok(page)
    }

    /// The whole manifest of a checkpoint, read page by page.
    pub fn checkpoint_manifest(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
    ) -> Result<CheckpointManifest> {
        self.read_manifest(checkpoint, cut_unix_ms, CHECKPOINT_MANIFEST_PAGE_LIMIT)
    }

    pub fn read_manifest(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
        limit: usize,
    ) -> Result<CheckpointManifest> {
        let mut manifest = CheckpointManifest {
            checkpoint: checkpoint.to_owned(),
            cut_unix_ms,
            ..CheckpointManifest::default()
        };
        let mut after = None;
        loop {
            let request = CheckpointManifestRequest {
                checkpoint: checkpoint.to_owned(),
                cut_unix_ms,
                after,
            };
            let page = self.manifest_page(&request, limit)?;
            after = manifest.append(page)?;
            if after.is_none() {
                return Ok(manifest);
            }
        }
    }

    /// Record a checkpoint's tombstones and delete the rows they stand for, in one transaction.
    /// A trim deletes in chunks with `record_checkpoint_tombstones_tx` and
    /// `delete_dropped_rows_tx` instead, and calls `replica_rows_changed` after each.
    #[cfg_attr(not(test), allow(dead_code))] // Adoption calls it (P5).
    pub fn apply_checkpoint_drop(
        &self,
        checkpoint: &str,
        envelopes: &[EnvelopeTombstone],
        claims: &[ClaimTombstone],
    ) -> Result<()> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        record_checkpoint_tombstones_tx(&transaction, checkpoint, envelopes, claims)?;
        delete_dropped_rows_tx(&transaction, envelopes, claims)?;
        transaction.commit()?;
        drop(connection);
        self.replica_rows_changed();
        Ok(())
    }

    /// Envelope rows changed without a new store index, as when a checkpoint records tombstones
    /// or deletes dropped rows. The next replication snapshot reads the inventory again.
    #[cfg_attr(not(test), allow(dead_code))] // The trim calls it (P5).
    pub fn replica_rows_changed(&self) {
        self.replica_generation.fetch_add(1, Ordering::AcqRel);
        // A trim deleted claims, so answers read from them may no longer hold.
        self.runtime.forget_views();
    }

    /// Whether evidence may cite this claim: it is stored, or a checkpoint dropped it.
    pub fn evidence_exists(&self, id: &str) -> Result<bool> {
        claim_or_tombstone_exists(&self.readers.get(), id)
    }

    /// How many envelopes checkpoints have dropped here.
    pub fn checkpointed_envelopes(&self) -> Result<u64> {
        let connection = self.readers.get();
        Ok(
            connection.query_row("SELECT COUNT(*) FROM checkpoint_envelopes", [], |row| {
                row.get(0)
            })?,
        )
    }
}
