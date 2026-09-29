//! Trimming: deleting what the newest stable checkpoint drops, and adopting a checkpoint this
//! node did not verify from a peer's manifest. Sections 6 and 8 of
//! `doc/fleet/smalltalk/checkpoint-design`.
//!
//! Tombstones go first, with the checkpoint marked `trimming`, in one transaction. Deletions
//! follow in chunks, each its own transaction. A crash leaves either nothing recorded, and the
//! next pass starts again, or every tombstone recorded, and the next pass deletes what is left.
//! At every point the inventory lists the same identities, so peers cannot tell.

use super::checkpoint::{
    CheckpointManifest, ClaimTombstone, EnvelopeTombstone, checkpoint_name, delete_dropped_rows_tx,
    plan_drops, record_checkpoint_tombstones_tx, verify_checkpoint_manifest,
};
use super::checkpoint_agreement::{
    Certificate, CheckpointAction, CheckpointClaim, certificates, chosen_certificate,
    stable_checkpoints,
};
use super::*;
use crate::model::InventoryCheckpoint;

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
    /// The first chunk bumps the graph generation without changing the graph.
    TouchGraph,
    /// The first chunk changes a graph table, as a trim must never do.
    ChangeGraph,
}

/// The newest stable checkpoint, when this node cannot trim it from its own plan. A peer that
/// trimmed it serves its manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestNeed {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub drop_digest: String,
}

fn local_checkpoint(
    connection: &Connection,
    checkpoint: &str,
) -> Result<Option<(String, Option<i64>)>> {
    Ok(connection
        .query_row(
            "SELECT state, seal_rowid FROM checkpoints WHERE id=?1",
            [checkpoint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Whether this node has applied the certificate with `drop_digest` for the cut, or anything
/// newer, or set the cut aside. A node that applied the other side's certificate for the same
/// cut has not applied this one.
fn applied(connection: &Connection, cut_unix_ms: u128, drop_digest: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM checkpoints
             WHERE (state='trimmed'
                    AND (cut_unix_ms > ?1 OR (cut_unix_ms = ?1 AND drop_digest = ?2)))
                OR (state IN ('set-aside', 'graph-changed') AND cut_unix_ms >= ?1)
             LIMIT 1",
            params![i64::try_from(cut_unix_ms)?, drop_digest],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Delete every tombstone that is not among `envelopes` and `claims`.
fn keep_only_tombstones_tx(
    transaction: &Transaction<'_>,
    envelopes: &[EnvelopeTombstone],
    claims: &[ClaimTombstone],
) -> Result<()> {
    transaction.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS adopted_envelopes(
             writer TEXT NOT NULL, sequence INTEGER NOT NULL, envelope_hash TEXT NOT NULL,
             PRIMARY KEY (writer, sequence, envelope_hash));
         CREATE TEMP TABLE IF NOT EXISTS adopted_claims(id TEXT PRIMARY KEY);
         DELETE FROM temp.adopted_envelopes;
         DELETE FROM temp.adopted_claims;",
    )?;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT OR IGNORE INTO temp.adopted_envelopes(writer, sequence, envelope_hash)
             VALUES (?1, ?2, ?3)",
        )?;
        for envelope in envelopes {
            insert.execute(params![
                envelope.writer,
                envelope.sequence,
                envelope.envelope_hash
            ])?;
        }
        let mut insert = transaction
            .prepare_cached("INSERT OR IGNORE INTO temp.adopted_claims(id) VALUES (?1)")?;
        for claim in claims {
            insert.execute([&claim.id])?;
        }
    }
    transaction.execute_batch(
        "DELETE FROM checkpoint_envelopes WHERE NOT EXISTS (
             SELECT 1 FROM temp.adopted_envelopes AS adopted
             WHERE adopted.writer=checkpoint_envelopes.writer
               AND adopted.sequence=checkpoint_envelopes.sequence
               AND adopted.envelope_hash=checkpoint_envelopes.envelope_hash);
         DELETE FROM checkpoint_claims WHERE id NOT IN (SELECT id FROM temp.adopted_claims);
         DELETE FROM temp.adopted_envelopes;
         DELETE FROM temp.adopted_claims;",
    )?;
    Ok(())
}

/// How this node applies the newest stable checkpoint.
enum Application {
    /// It already did.
    Done,
    /// It verified the certificate every node applies, so it trims from its own plan.
    OwnPlan {
        checkpoint: String,
        certificate: Box<Certificate>,
        seal_rowid: i64,
    },
    /// It adopts the certificate's manifest from a peer.
    Manifest(CheckpointManifestNeed),
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

    /// Apply a graph fault a test armed, inside the first chunk's transaction.
    fn alter_graph_for_trim_fault(&self, transaction: &Transaction<'_>) -> Result<()> {
        let mut fault = self
            .trim_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *fault {
            Some(TrimFault::TouchGraph) => {
                transaction.execute("UPDATE graph_generation SET value=value+1", [])?;
            }
            Some(TrimFault::ChangeGraph) => {
                transaction.execute(
                    "INSERT INTO desired(subject, kind, revision, claim_id, body)
                     SELECT 'custom/trim-fault/changed', 'custom', '1', id, '{}'
                     FROM claims LIMIT 1",
                    [],
                )?;
            }
            _ => return Ok(()),
        }
        *fault = None;
        Ok(())
    }

    fn stop_for_trim_fault(&self, at: TrimFault) -> Result<()> {
        let mut fault = self
            .trim_fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *fault == Some(at) {
            *fault = None;
            anyhow::bail!("the trim stopped at {at:?} for a test");
        }
        Ok(())
    }

    fn newest_application(&self, claims: &[CheckpointClaim]) -> Result<Option<Application>> {
        let stable = stable_checkpoints(claims);
        let Some((cut, certificates)) = stable.into_iter().next_back() else {
            return Ok(None);
        };
        let certificate = chosen_certificate(&certificates)
            .expect("a stable checkpoint has a certificate")
            .clone();
        let checkpoint = checkpoint_name(cut);
        let connection = self.readers.get();
        if applied(&connection, cut, &certificate.terms.drop_digest)? {
            return Ok(Some(Application::Done));
        }
        if certificate.verifications.contains_key(&self.origin)
            && let Some((state, Some(seal_rowid))) = local_checkpoint(&connection, &checkpoint)?
            && state != "plan-changed"
        {
            return Ok(Some(Application::OwnPlan {
                checkpoint,
                certificate: Box::new(certificate),
                seal_rowid,
            }));
        }
        Ok(Some(Application::Manifest(CheckpointManifestNeed {
            checkpoint,
            cut_unix_ms: cut,
            drop_digest: certificate.terms.drop_digest,
        })))
    }

    /// Finish a trim a crash interrupted, then apply the newest stable checkpoint if this node
    /// has not: trim it from its own plan when it verified the certificate every node applies,
    /// or report that it needs that certificate's manifest. Until it is applied, this node must
    /// not verify a newer checkpoint, because every node trims the same checkpoints in the same
    /// order.
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
        match self.newest_application(claims)? {
            None | Some(Application::Done) => Ok(None),
            Some(Application::Manifest(need)) => Ok(Some(need)),
            Some(Application::OwnPlan {
                checkpoint,
                certificate,
                seal_rowid,
            }) => {
                let cut = certificate.terms.cut_unix_ms;
                let sealed = self.checkpoint_sealed_set_through(cut, Some(seal_rowid))?;
                let plan = plan_drops(&sealed);
                if plan.sealed_digest == certificate.terms.sealed_digest
                    && plan.drop_digest == certificate.terms.drop_digest
                {
                    self.trim_checkpoint(
                        &checkpoint,
                        cut,
                        &plan.drop_digest,
                        &plan.envelopes,
                        &plan.claims,
                        false,
                        actions,
                    )?;
                    return Ok(None);
                }
                // Its own rows no longer give the drop it verified, so it adopts the manifest
                // from a peer like a node that did not take part.
                self.set_checkpoint_state(&checkpoint, cut, "plan-changed")?;
                self.checkpoint_diagnostic(
                    "checkpoint-trim-plan-changed",
                    &format!(
                        "this node no longer plans the drop it verified for {checkpoint}; it \
                         adopts the manifest from a peer instead"
                    ),
                    &checkpoint,
                )?;
                Ok(Some(CheckpointManifestNeed {
                    checkpoint,
                    cut_unix_ms: cut,
                    drop_digest: certificate.terms.drop_digest,
                }))
            }
        }
    }

    /// The newest stable checkpoint this node needs a manifest for, if any. The replication
    /// worker asks when a peer advertises a checkpoint this node has not applied, and fetches
    /// the manifest from a peer that applied this one.
    pub fn checkpoint_manifest_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        let claims = self.checkpoint_claims()?;
        Ok(match self.newest_application(&claims)? {
            Some(Application::Manifest(need)) => Some(need),
            _ => None,
        })
    }

    /// The newest checkpoint this node has trimmed or adopted, as its inventory advertises it.
    pub fn trimmed_checkpoint(&self) -> Result<Option<InventoryCheckpoint>> {
        Ok(self
            .readers
            .get()
            .prepare_cached(
                "SELECT id, cut_unix_ms, drop_digest FROM checkpoints
                 WHERE state='trimmed' AND drop_digest IS NOT NULL
                 ORDER BY cut_unix_ms DESC LIMIT 1",
            )?
            .query_row([], |row| {
                Ok(InventoryCheckpoint {
                    id: row.get(0)?,
                    cut_unix_ms: u128::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                    drop_digest: row.get(2)?,
                })
            })
            .optional()?)
    }

    /// Adopt a stable checkpoint from a peer's manifest. The whole manifest is checked against
    /// the drop digest every participant of the chosen certificate verified before anything
    /// from it is stored. Afterwards this node's tombstones are exactly the manifest's, so it
    /// goes on like every node that applied the same certificate.
    pub fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>, St3Error> {
        let claims = self.checkpoint_claims().map_err(internal)?;
        // Adoption replaces every tombstone this node holds, so only the newest stable
        // checkpoint may be adopted: an older manifest lacks the newer drops.
        if stable_checkpoints(&claims)
            .keys()
            .next_back()
            .is_some_and(|newest| *newest > manifest.cut_unix_ms)
        {
            return Err(St3Error::new(
                "checkpoint-superseded",
                format!(
                    "`{}` is older than the newest stable checkpoint here; a node adopts only \
                     the newest",
                    manifest.checkpoint
                ),
            ));
        }
        let certificates = certificates(&claims, &manifest.checkpoint);
        let Some(certificate) = chosen_certificate(&certificates) else {
            return Err(St3Error::new(
                "checkpoint-not-stable",
                format!(
                    "`{}` is not stable on this node; it adopts a checkpoint once it holds \
                     every participant's verification",
                    manifest.checkpoint
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
        let done = applied(
            &self.readers.get(),
            manifest.cut_unix_ms,
            &certificate.terms.drop_digest,
        )
        .map_err(internal)?;
        if done {
            return Ok(actions);
        }
        self.trim_checkpoint(
            &manifest.checkpoint,
            manifest.cut_unix_ms,
            &certificate.terms.drop_digest,
            &manifest.envelopes,
            &manifest.claims,
            true,
            &mut actions,
        )
        .map_err(internal)?;
        Ok(actions)
    }

    /// A deliberate bug for the convergence suite: adopt a manifest without checking it
    /// against its certificate. Nothing else calls it.
    #[doc(hidden)]
    pub fn adopt_checkpoint_unverified_for_tests(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>> {
        let claims = self.checkpoint_claims()?;
        let certificates = certificates(&claims, &manifest.checkpoint);
        let certificate = chosen_certificate(&certificates)
            .ok_or_else(|| anyhow::anyhow!("{} is not stable here", manifest.checkpoint))?;
        let mut actions = Vec::new();
        self.trim_checkpoint(
            &manifest.checkpoint,
            manifest.cut_unix_ms,
            &certificate.terms.drop_digest,
            &manifest.envelopes,
            &manifest.claims,
            true,
            &mut actions,
        )?;
        Ok(actions)
    }

    /// A deliberate bug for the convergence suite: forget every tombstone, as a trim that
    /// deleted rows without keeping tombstones would. Returns how many it forgot. Nothing else
    /// calls it.
    #[doc(hidden)]
    pub fn forget_tombstones_for_tests(&self) -> Result<usize> {
        let forgotten = {
            let connection = self.connection.write();
            connection.execute("DELETE FROM checkpoint_envelopes", [])?
                + connection.execute("DELETE FROM checkpoint_claims", [])?
        };
        self.replica_rows_changed();
        Ok(forgotten)
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
    ///
    /// With `exact`, as in adoption, `envelopes` and `claims` are every tombstone through the
    /// checkpoint, and any other tombstone this node holds goes. That happens only on a node
    /// that applied the other side's certificate for a cut both sides of a partition certified.
    /// Its identities leave the inventory, and peers that kept those envelopes send them again.
    #[allow(clippy::too_many_arguments)]
    fn trim_checkpoint(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
        drop_digest: &str,
        envelopes: &[EnvelopeTombstone],
        claims: &[ClaimTombstone],
        exact: bool,
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<()> {
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            if exact {
                keep_only_tombstones_tx(&transaction, envelopes, claims)?;
            }
            record_checkpoint_tombstones_tx(&transaction, checkpoint, envelopes, claims)?;
            transaction.execute(
                "INSERT INTO checkpoints(
                     id, cut_unix_ms, state, drop_digest, detail, updated_at_unix_ms)
                 VALUES (?1, ?2, 'trimming', ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET state='trimming',
                     drop_digest=excluded.drop_digest, detail=excluded.detail,
                     updated_at_unix_ms=excluded.updated_at_unix_ms",
                params![
                    checkpoint,
                    i64::try_from(cut_unix_ms)?,
                    drop_digest,
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
                                |row| {
                                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                                },
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
            // Deleting claims touches no graph table, so the generation stays. Only if it moved
            // are the digests themselves compared, before and after, in this transaction.
            let generation = graph_generation(&transaction)?;
            transaction.execute_batch("SAVEPOINT trim_chunk")?;
            delete_dropped_rows_tx(&transaction, &envelopes, &claims)?;
            self.alter_graph_for_trim_fault(&transaction)?;
            let changed = if graph_generation(&transaction)? == generation {
                false
            } else {
                let after = graph_digest(&transaction)?;
                transaction.execute_batch("ROLLBACK TO trim_chunk")?;
                let before = graph_digest(&transaction)?;
                delete_dropped_rows_tx(&transaction, &envelopes, &claims)?;
                before != after || graph_digest(&transaction)? != before
            };
            transaction.execute_batch("RELEASE trim_chunk")?;
            if changed {
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
