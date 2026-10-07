//! A claims-only runtime that maintains registered views on local append and replicated ingest.
//! The runtime accepts claim schemas like `Plain`; production runtimes must retain their own
//! authority/schema admission and call the same view engine from their existing seams.
//! Missing/changed projections are fenced; v0 supplies no startup backfill or full replay.
//! Prototype precondition: the file has one runtime owner. Opening a second writer under Plain
//! or another runtime can bypass this runtime's checkpoint refusal; production needs a durable
//! database registration/capability fence and a reviewed checkpoint participation contract.

use super::*;
use crate::claim::ReplicaBatch;
use crate::{
    ClaimInput, Error,
    store::{
        ReplicatedClaimAdmission, Runtime, Store, append_claim_record_tx,
        checkpoint::{DropPlan, SealedSet},
        claim_from_row, current_index_tx,
        runtime::{IncrementalProjection, LegacyDigestTable},
    },
};
use serde_json::json;

pub struct ViewRuntime {
    pub views: Views,
    pub claim_source: Option<super::claim_source::ClaimSource>,
    schema_digest: String,
    pub(super) asynchronous: Option<super::asynchronous::Limits>,
}
impl ViewRuntime {
    pub fn new(views: Views) -> Result<Self> {
        // Repair can precede projection of the original during replicated admission. Until
        // that schedule has a proved per-view eligibility implementation, this reference
        // runtime must not later re-admit an original that a replacement already retracted.
        // Production runtimes retain responsibility for their own admitted input policy.
        ensure!(
            views
                .views
                .iter()
                .all(|view| view.repair_policy() == RepairPolicy::RetainOriginal),
            "IVM reference runtime does not support replacement-repair eligibility"
        );
        let definitions = views
            .views
            .iter()
            .map(|view| {
                let d = view.definition();
                (
                    d.name,
                    d.fingerprint,
                    d.kinds,
                    d.local_kinds,
                    d.max_contributions,
                    format!("{:?}", view.repair_policy()),
                )
            })
            .collect::<Vec<_>>();
        use sha2::{Digest, Sha256};
        let schema_digest =
            hex::encode(Sha256::digest(serde_json::to_vec(&(LAYOUT, definitions))?));
        Ok(Self {
            views,
            claim_source: None,
            schema_digest,
            asynchronous: None,
        })
    }
    /// Explicit fresh-store asynchronous mode. Compatible files resume; existing synchronous
    /// or nonempty unregistered files require a separately reviewed installation and are refused.
    pub fn asynchronous(views: Views, limits: super::asynchronous::Limits) -> Result<Self> {
        limits.validate()?;
        ensure!(views.views.len() <= 256, "async view registry exceeds 256");
        let declared = views
            .views
            .iter()
            .try_fold(0usize, |total, view| {
                total.checked_add(view.definition().max_contributions)
            })
            .context("async declared dependency bound overflow")?;
        ensure!(
            declared <= 256,
            "async combined declared dependency bound exceeds 256"
        );
        ensure!(
            views
                .views
                .iter()
                .all(|view| view.definition().local_kinds.is_empty()),
            "async local-source/deadline adapter is not installed"
        );
        let mut runtime = Self::new(views)?;
        runtime.asynchronous = Some(limits);
        Ok(runtime)
    }
    /// Opt-in claims-only retained source. Registration/installation remain explicit.
    pub fn with_claim_source(
        views: Views,
        source: super::claim_source::ClaimSource,
    ) -> Result<Self> {
        let mut runtime = Self::new(views)?;
        use sha2::{Digest, Sha256};
        runtime.schema_digest = hex::encode(Sha256::digest(serde_json::to_vec(&(
            &runtime.schema_digest,
            source.name(),
            source.fingerprint(),
        ))?));
        runtime.claim_source = Some(source);
        Ok(runtime)
    }
}
fn record(connection: &Connection, id: &str) -> Result<ClaimRecord> {
    Ok(connection.query_row(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE id=?1",
        [id],claim_from_row,
    )?)
}

impl Runtime for ViewRuntime {
    fn migrate_schema(&self, _connection: &Connection) -> Result<()> {
        Ok(())
    }

    fn create_schema(&self, connection: &Connection) -> Result<()> {
        ensure!(
            self.asynchronous.is_none() || self.claim_source.is_none(),
            "asynchronous retained-source installer adapter is not installed"
        );
        super::asynchronous::check_owner(connection, self.asynchronous)?;
        self.views.create_schema(connection)?;
        if let Some(limits) = self.asynchronous {
            super::asynchronous::create_schema(connection, limits)?;
        }
        if let Some(source) = &self.claim_source {
            source.create_schema(connection)?;
        }
        Ok(())
    }

    fn open_projections(&self, transaction: &Transaction<'_>, _shared_memory: bool) -> Result<()> {
        let cut = source_cut(transaction)?.unwrap_or(SourceCut {
            epoch: 1,
            admitted: current_index_tx(transaction)?,
            projected: 0,
            local_generation: 0,
        });
        self.views.initialize_empty(transaction, cut)?;
        if let Some(limits) = self.asynchronous {
            super::asynchronous::initialize(transaction, &self.views, cut, limits)?;
        }
        Ok(())
    }

    fn schema_digest(&self) -> String {
        self.schema_digest.clone()
    }

    fn classify_replicated_claim(
        &self,
        _connection: &Connection,
        _batch: &ReplicaBatch,
        _claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, Error> {
        Ok(ReplicatedClaimAdmission::Valid)
    }

    fn append_claim(
        &self,
        store: &Store,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), Error> {
        let body = json!({ "fields": input.fields, "evidence": input.evidence });
        store
            .connection
            .batched(|transaction| {
                self.append_claim_tx(
                    transaction,
                    &store.origin,
                    &input.subject,
                    &input.kind,
                    input.actor.as_deref(),
                    &body,
                    &[],
                    None,
                )
                .map(|claim| (claim, true))
                .map_err(crate::error::typed)
            })
            .map_err(|error| Error::new("internal", error))?
    }

    fn apply_repair_tx(
        &self,
        transaction: &Transaction<'_>,
        repaired: &str,
        replacement: &str,
    ) -> Result<()> {
        let mut kinds =
            transaction.prepare_cached("SELECT kind FROM claims WHERE id IN (?1,?2)")?;
        let relevant = kinds
            .query_map([repaired, replacement], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|kind| self.views.reads_kind(kind));
        if !relevant {
            return Ok(());
        }
        if self.asynchronous.is_some() {
            let mut affected = BTreeSet::new();
            for kind in kinds.query_map([repaired, replacement], |row| row.get::<_, String>(0))? {
                affected.extend(self.views.subscribers(&kind?));
            }
            for index in affected {
                fence_error(
                    transaction,
                    self.views.views[index].definition().name,
                    &anyhow::anyhow!(
                        "async accepted repair requires explicit bounded view recovery"
                    ),
                )?;
            }
            return Ok(());
        }
        let old = record(transaction, repaired)?;
        let new = record(transaction, replacement)?;
        let cut = source_cut(transaction)?.context("IVM source unready")?;
        let key = canonical::claim_key(transaction, replacement)?;
        self.views
            .repair(transaction, &old, (&new, &key), cut.epoch)?;
        Ok(())
    }

    fn append_claim_tx(
        &self,
        transaction: &Transaction<'_>,
        origin: &str,
        subject: &str,
        kind: &str,
        actor: Option<&str>,
        body: &Value,
        predecessors: &[String],
        forced_batch: Option<&str>,
    ) -> Result<ClaimRecord> {
        let claim = append_claim_record_tx(
            transaction,
            origin,
            subject,
            kind,
            actor,
            body,
            predecessors,
            forced_batch,
        )?;
        let previous = source_cut(transaction)?.context("IVM source unready")?;
        if self.asynchronous.is_none() && self.views.reads_kind(&claim.kind) {
            let key = canonical::claim_key(transaction, &claim.id)?;
            self.views
                .change(transaction, None, Some((&claim, &key)), previous.epoch)?;
        }
        if let Some(source) = &self.claim_source {
            source.capture(transaction, &claim)?;
        }
        let contiguous = self.asynchronous.is_none()
            && previous.projected.checked_add(1) == Some(claim.store_index);
        self.views.publish_cut(
            transaction,
            SourceCut {
                admitted: current_index_tx(transaction)?,
                projected: if contiguous {
                    claim.store_index
                } else {
                    previous.projected
                },
                ..previous
            },
        )?;
        Ok(claim)
    }

    fn project_incremental(
        &self,
        transaction: &Transaction<'_>,
        _origin: &str,
        through: u64,
    ) -> Result<IncrementalProjection, Error> {
        let project = || -> Result<()> {
            let previous = source_cut(transaction)?.context("IVM source unready")?;
            if self.asynchronous.is_some() {
                // The admission transaction's queue trigger already captured every inserted
                // claim. Store's base admission projection can advance without view CPU.
                self.views.publish_cut(
                    transaction,
                    SourceCut {
                        admitted: current_index_tx(transaction)?,
                        ..previous
                    },
                )?;
                return Ok(());
            }
            if through <= previous.projected {
                return Ok(());
            }
            let mut statement = transaction.prepare_cached(
                "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
                 FROM claims WHERE store_index>?1 AND store_index<=?2
                 ORDER BY store_index LIMIT ?3",
            )?;
            let claims = statement
                .query_map(
                    params![
                        previous.projected,
                        through,
                        crate::store::PROJECTION_CHUNK_CLAIMS + 1
                    ],
                    claim_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(
                claims.len() <= crate::store::PROJECTION_CHUNK_CLAIMS,
                "IVM projection chunk exceeds admission bound"
            );
            for claim in &claims {
                if self.views.reads_kind(&claim.kind) {
                    let key = canonical::claim_key(transaction, &claim.id)?;
                    self.views
                        .change(transaction, None, Some((claim, &key)), previous.epoch)?;
                }
                if let Some(source) = &self.claim_source {
                    source.capture(transaction, claim)?;
                }
            }
            self.views.publish_cut(
                transaction,
                SourceCut {
                    admitted: current_index_tx(transaction)?,
                    projected: through,
                    ..previous
                },
            )?;
            Ok(())
        };
        project().map_err(crate::error::typed)?;
        Ok(IncrementalProjection::Projected)
    }

    fn replay_from_nothing(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        Err(Error::new(
            "ivm-unready",
            "registered views require bounded explicit source repair; full replay is disabled",
        ))
    }

    fn after_projection(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        Ok(())
    }

    fn forget_views(&self) {}

    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] {
        &[
            ("operations", &[]),
            ("blobs", &[]),
            ("documents", &["created_index"]),
        ]
    }

    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] {
        &[]
    }

    fn checkpoint_rules_digest(&self) -> String {
        "smallclaims.ivm.checkpoint-unavailable.v1".into()
    }

    fn checkpoint_preflight(&self) -> Result<()> {
        bail!(
            "ivm-checkpoint-unavailable: registered views have no bounded retention/installation contract"
        )
    }

    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan {
        // The infallible trait method must not panic. Direct callers receive a no-drop plan,
        // never a proof; Store's public planner fails at preflight before any history/copy work.
        DropPlan {
            cut_unix_ms: sealed.cut_unix_ms,
            rules_digest: self.checkpoint_rules_digest(),
            sealed_envelopes: sealed.envelopes.len(),
            sealed_claims: sealed.claims.len(),
            sealed_digest: crate::store::checkpoint::sealed_digest(sealed),
            envelopes: Vec::new(),
            claims: Vec::new(),
            by_kind: std::collections::BTreeMap::new(),
            drop_digest: crate::store::checkpoint::drop_digest(&[], &[]),
            retained_digest: crate::store::checkpoint::retained_digest(
                sealed.claims.iter().map(|c| c.claim.id.as_str()),
            ),
        }
    }

    fn clear_checkpoint_projections(&self, _transaction: &Transaction<'_>) -> Result<()> {
        self.checkpoint_preflight()
    }

    fn replay_checkpoint_projections(&self, _transaction: &Transaction<'_>) -> Result<()> {
        bail!("registered views have no checkpoint retention proof yet")
    }

    fn checkpoint_subject_answers(
        &self,
        _connection: &Connection,
        _subject: &str,
        _cut: u128,
    ) -> Result<Value> {
        Ok(Value::Null)
    }
}
