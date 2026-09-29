//! Trimming: deleting what the newest stable checkpoint drops, and adopting a checkpoint this
//! node did not verify from a peer's manifest. Sections 6 and 8 of
//! `doc/fleet/smalltalk/checkpoint-design`.
//!
//! Tombstones go first, with the checkpoint marked `trimming`, in one transaction. Deletions
//! follow in chunks, each its own transaction. A crash leaves either nothing recorded, and the
//! next pass starts again, or every tombstone recorded, and the next pass deletes what is left.
//! At every point the inventory lists the same identities, so peers cannot tell.

use super::checkpoint::{
    CheckpointManifest, ClaimTombstone, EnvelopeTombstone, checkpoint_name,
    delete_dropped_rows_tx, plan_drops, record_checkpoint_tombstones_tx,
    verify_checkpoint_manifest,
};
use super::checkpoint_agreement::{CheckpointAction, CheckpointClaim, stable_checkpoints};
use super::*;

/// Envelopes deleted per transaction, so a trim never holds the writer for long.
pub const TRIM_CHUNK_ENVELOPES: usize = 2_000;

/// A point in a trim where a test makes it stop, as a crash would there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrimFault {
    /// Tombstones are recorded and nothing is deleted yet.
    AfterTombstones,
    /// This many chunks are deleted.
    AfterChunk(usize),
    /// Every row is deleted and the checkpoint is not yet marked trimmed.
    BeforeFinish,
}

/// The newest stable checkpoint, when this node cannot trim it from its own plan. A peer that
/// trimmed it serves its manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestNeed {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub drop_digest: String,
}

fn local_checkpoint(connection: &Connection, checkpoint: &str) -> Result<Option<(String, Option<i64>)>> {
    Ok(connection
        .query_row(
            "SELECT state, seal_rowid FROM checkpoints WHERE id=?1",
            [checkpoint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Whether this node has applied `checkpoint` or a newer one, by trimming or adopting it, or
/// set it aside.
fn applied_through(connection: &Connection, cut_unix_ms: u128) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM checkpoints
             WHERE cut_unix_ms >= ?1 AND state IN ('trimmed', 'set-aside', 'graph-changed')
             LIMIT 1",
            [i64::try_from(cut_unix_ms)?],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

impl Store {
    /// Make the next trim stop at `fault`, as a crash would. Tests only.
    pub fn set_trim_fault(&self, fault: Option<TrimFault>) {
        *self
            .trim_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fault;
    }

    /// Delete this many envelopes per transaction. Tests only.
    pub fn set_trim_chunk_envelopes(&self, envelopes: usize) {
        self.trim_chunk_envelopes
            .store(envelopes.max(1), Ordering::Release);
    }

    fn stop_for_trim_fault(&self, at: TrimFault) -> Result<()> {
        let mut fault = self.trim_fault.lock().unwrap_or_else(PoisonError::into_inner);
        if *fault == Some(at) {
            *fault = None;
            anyhow::bail!("the trim stopped at {at:?} for a test");
        }
        Ok(())
    }

    /// Finish a trim a crash interrupted, then apply the newest stable checkpoint if this node
    /// has not: trim it from its own plan when it verified it, or report that it needs the
    /// manifest. Until it is applied, this node must not verify a newer checkpoint, because
    /// every node trims the same checkpoints in the same order.
    pub(super) fn apply_stable_checkpoints(
        &self,
        claims: &[CheckpointClaim],
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<Option<CheckpointManifestNeed>> {
        let interrupted = {
            let connection = self.readers.get();
            connection
                .prepare_cached("SELECT id FROM checkpoints WHERE state='trimming' ORDER BY id")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for checkpoint in interrupted {
            self.finish_trim(&checkpoint, actions)?;
        }
        let stable = stable_checkpoints(claims);
        let Some((cut, certificates)) = stable.into_iter().next_back() else {
            return Ok(None);
        };
        let checkpoint = checkpoint_name(cut);
        let local = {
            let connection = self.readers.get();
            if applied_through(&connection, cut)? {
                return Ok(None);
            }
            local_checkpoint(&connection, &checkpoint)?
        };
        let own = certificates
            .iter()
            .find(|certificate| certificate.verifications.contains_key(&self.origin));
        if let (Some(certificate), Some((_, Some(seal_rowid)))) = (own, local) {
            let sealed = self.checkpoint_sealed_set_through(cut, Some(seal_rowid))?;
            let plan = plan_drops(&sealed);
            if plan.sealed_digest == certificate.terms.sealed_digest
                && plan.drop_digest == certificate.terms.drop_digest
            {
                self.trim_checkpoint(&checkpoint, cut, &plan.envelopes, &plan.claims, actions)?;
                return Ok(None);
            }
            self.checkpoint_diagnostic(
                "checkpoint-trim-plan-changed",
                &format!(
                    "this node no longer plans the drop it verified for {checkpoint}; it adopts \
                     the manifest from a peer instead"
                ),
                &checkpoint,
            )?;
        }
        if certificates.len() > 1 && own.is_some() {
            // People excused each side of a partition and both sides certified this cut. This
            // node applied its own side above; the other side's drops stay here as claims.
            return Ok(None);
        }
        if certificates.len() > 1 {
            // Neither side's manifest can be checked against a single certificate, so this
            // node keeps every claim of the cut. Keeping a claim is always safe.
            self.set_checkpoint_state(&checkpoint, cut, "set-aside")?;
            return Ok(None);
        }
        Ok(Some(CheckpointManifestNeed {
            checkpoint,
            cut_unix_ms: cut,
            drop_digest: certificates[0].terms.drop_digest.clone(),
        }))
    }

    /// The newest stable checkpoint this node needs a manifest for, if any. The replication
    /// worker asks after each exchange and fetches the manifest from that peer.
    pub fn checkpoint_manifest_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        let claims = self.checkpoint_claims()?;
        let stable = stable_checkpoints(&claims);
        let Some((cut, certificates)) = stable.into_iter().next_back() else {
            return Ok(None);
        };
        let [certificate] = certificates.as_slice() else {
            return Ok(None);
        };
        let connection = self.readers.get();
        if applied_through(&connection, cut)? {
            return Ok(None);
        }
        let checkpoint = checkpoint_name(cut);
        let verified_here = certificate.verifications.contains_key(&self.origin)
            && local_checkpoint(&connection, &checkpoint)?
                .is_some_and(|(_, seal_rowid)| seal_rowid.is_some());
        Ok((!verified_here).then(|| CheckpointManifestNeed {
            checkpoint,
            cut_unix_ms: cut,
            drop_digest: certificate.terms.drop_digest.clone(),
        }))
    }

    /// Adopt a stable checkpoint from a peer's manifest. The whole manifest is checked against
    /// the drop digest every participant verified before anything from it is stored.
    pub fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>, St3Error> {
        let claims = self.checkpoint_claims().map_err(internal)?;
        let certificates = super::checkpoint_agreement::certificates(&claims, &manifest.checkpoint);
        let [certificate] = certificates.as_slice() else {
            return Err(St3Error::new(
                "checkpoint-not-adoptable",
                format!(
                    "`{}` has {} certificates here; a node adopts a checkpoint with exactly one",
                    manifest.checkpoint,
                    certificates.len()
                ),
            ));
        };
        if certificate.terms.cut_unix_ms != manifest.cut_unix_ms {
            return Err(St3Error::new(
                "checkpoint-manifest-invalid",
                "the manifest's cut is not the certified one",
            ));
        }
        verify_checkpoint_manifest(manifest, &certificate.terms.drop_digest)?;
        let mut actions = Vec::new();
        if applied_through(&self.readers.get(), manifest.cut_unix_ms).map_err(internal)? {
            return Ok(actions);
        }
        self.trim_checkpoint(
            &manifest.checkpoint,
            manifest.cut_unix_ms,
            &manifest.envelopes,
            &manifest.claims,
            &mut actions,
        )
        .map_err(internal)?;
        Ok(actions)
    }

    fn set_checkpoint_state(&self, checkpoint: &str, cut_unix_ms: u128, state: &str) -> Result<()> {
        let connection = self.connection.write();
        connection.execute(
            "INSERT INTO checkpoints(id, cut_unix_ms, state, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state,
                 updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![
                checkpoint,
                i64::try_from(cut_unix_ms)?,
                state,
                i64::try_from(now_ms())?
            ],
        )?;
        Ok(())
    }

    fn checkpoint_diagnostic(&self, code: &str, reason: &str, checkpoint: &str) -> Result<()> {
        self.append_claim(&ClaimInput {
            subject: format!("daemon/{}", self.origin),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), json!("error")),
                ("code".into(), json!(code)),
                ("reason".into(), json!(reason)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("{code}:{}:{checkpoint}", self.origin)),
        })
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
        Ok(())
    }

    /// Record every tombstone and mark the checkpoint `trimming` in one transaction, then
    /// delete. The clamp's floor comes with the row, so nothing this node writes afterwards is
    /// dated before the cut.
    fn trim_checkpoint(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
        envelopes: &[EnvelopeTombstone],
        claims: &[ClaimTombstone],
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<()> {
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            record_checkpoint_tombstones_tx(&transaction, checkpoint, envelopes, claims)?;
            transaction.execute(
                "INSERT INTO checkpoints(id, cut_unix_ms, state, detail, updated_at_unix_ms)
                 VALUES (?1, ?2, 'trimming', ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET state='trimming', detail=excluded.detail,
                     updated_at_unix_ms=excluded.updated_at_unix_ms",
                params![
                    checkpoint,
                    i64::try_from(cut_unix_ms)?,
                    json!({"envelopes": envelopes.len(), "claims": claims.len()}).to_string(),
                    i64::try_from(now_ms())?
                ],
            )?;
            transaction.commit()?;
        }
        self.replica_rows_changed();
        self.stop_for_trim_fault(TrimFault::AfterTombstones)?;
        self.finish_trim(checkpoint, actions)
    }

    /// Delete every row a tombstone stands for, in chunks, then mark the checkpoint trimmed.
    /// Running it again after a crash deletes only what is left. Each chunk checks, inside its
    /// own transaction, that the graph did not change; the proof showed it cannot.
    fn finish_trim(&self, checkpoint: &str, actions: &mut Vec<CheckpointAction>) -> Result<()> {
        let mut chunks = 0;
        let mut deleted_envelopes = 0;
        let mut deleted_claims = 0;
        loop {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            let envelopes = transaction
                .prepare_cached(
                    "SELECT tombstone.writer, tombstone.sequence, tombstone.envelope_hash,
                            tombstone.accepted_at_unix_ms
                     FROM checkpoint_envelopes AS tombstone
                     WHERE EXISTS (
                         SELECT 1 FROM replica_envelopes AS held
                         WHERE held.writer=tombstone.writer AND held.sequence=tombstone.sequence
                           AND held.envelope_hash=tombstone.envelope_hash)
                     LIMIT ?1",
                )?
                .query_map([self.trim_chunk_envelopes.load(Ordering::Acquire)], |row| {
                    Ok(EnvelopeTombstone {
                        writer: row.get(0)?,
                        sequence: row.get(1)?,
                        envelope_hash: row.get(2)?,
                        accepted_at_unix_ms: u128::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut claims = Vec::new();
            {
                let mut statement = transaction.prepare_cached(
                    "SELECT tombstone.id, tombstone.operation_id FROM checkpoint_claims AS tombstone
                     WHERE tombstone.writer=?1 AND tombstone.sequence=?2
                       AND tombstone.envelope_hash=?3
                       AND EXISTS (SELECT 1 FROM claims WHERE claims.id=tombstone.id)",
                )?;
                for envelope in &envelopes {
                    claims.extend(
                        statement
                            .query_map(
                                params![envelope.writer, envelope.sequence, envelope.envelope_hash],
                                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            // A claim whose envelope is already gone, left by an older build's trim or a
            // crash between chunks of another checkpoint.
            if envelopes.is_empty() {
                claims = transaction
                    .prepare_cached(
                        "SELECT tombstone.id, tombstone.operation_id FROM checkpoint_claims AS tombstone
                         WHERE EXISTS (SELECT 1 FROM claims WHERE claims.id=tombstone.id)
                         LIMIT ?1",
                    )?
                    .query_map([self.trim_chunk_envelopes.load(Ordering::Acquire)], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if claims.is_empty() {
                    break;
                }
            }
            let claims = claims
                .into_iter()
                .map(|(id, operation_id)| ClaimTombstone {
                    id,
                    operation_id,
                    writer: String::new(),
                    sequence: 0,
                    envelope_hash: String::new(),
                    subject: String::new(),
                    kind: String::new(),
                    actor: None,
                    predecessors: Vec::new(),
                    request_digest: None,
                    accepted_at_unix_ms: 0,
                })
                .collect::<Vec<_>>();
            let before = graph_digest(&transaction)?;
            delete_dropped_rows_tx(&transaction, &envelopes, &claims)?;
            let after = graph_digest(&transaction)?;
            if before != after {
                transaction.rollback()?;
                drop(connection);
                self.set_checkpoint_state(checkpoint, 0, "graph-changed")?;
                self.checkpoint_diagnostic(
                    "checkpoint-trim-graph-changed",
                    &format!(
                        "deleting what {checkpoint} drops would change the graph; nothing more \
                         is deleted and no checkpoint is sealed until a person looks"
                    ),
                    checkpoint,
                )?;
                actions.push(CheckpointAction::TrimGraphChanged {
                    checkpoint: checkpoint.to_owned(),
                });
                return Ok(());
            }
            transaction.commit()?;
            drop(connection);
            self.replica_rows_changed();
            deleted_envelopes += envelopes.len();
            deleted_claims += claims.len();
            chunks += 1;
            self.stop_for_trim_fault(TrimFault::AfterChunk(chunks))?;
        }
        self.stop_for_trim_fault(TrimFault::BeforeFinish)?;
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            transaction.execute(
                "UPDATE checkpoints SET state='trimmed', updated_at_unix_ms=?2 WHERE id=?1",
                params![checkpoint, i64::try_from(now_ms())?],
            )?;
            // Every snapshot and page cursor taken before the trim expires, since the claims
            // they read may be gone.
            transaction.execute(
                "UPDATE sqlite_sequence SET seq=seq+1 WHERE name='claims'",
                [],
            )?;
            transaction.commit()?;
        }
        self.replica_rows_changed();
        actions.push(CheckpointAction::Trimmed {
            checkpoint: checkpoint.to_owned(),
            envelopes: deleted_envelopes,
            claims: deleted_claims,
        });
        Ok(())
    }
}
