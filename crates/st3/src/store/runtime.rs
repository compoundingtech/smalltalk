//! smalltalk as a runtime on the graph: the claim kinds it knows, the tables it projects them
//! into, its checkpoint rules, and the caches of its projections. `smallclaims` reaches all of
//! these only through [`smallclaims::store::Runtime`].

use super::*;

/// smalltalk's half of the store. The graph holds it as its runtime, and `Store` keeps it beside
/// the graph for the caches its reads use.
#[derive(Default)]
pub struct SmalltalkRuntime {
    #[cfg(test)]
    pub(crate) work_extension_roots_rebuilt: std::sync::atomic::AtomicUsize,
    /// Simulate different build registries on isolated nodes in compatibility tests.
    #[cfg(test)]
    pub(crate) claim_registry: std::sync::OnceLock<st3_schema::Registry>,
    pub(crate) actual_cache: Mutex<HashMap<String, (u64, Option<Value>)>>,
    /// Per-subject status reductions and current-view answers, until their claims change.
    pub(crate) subject_cache: Mutex<SubjectCache>,
    pub(crate) message_cache: Mutex<HashMap<String, MessageCacheEntry>>,
    pub(crate) agent_status_cache: Mutex<VecDeque<AgentStatusEntry>>,
    pub(crate) agent_resources_cache: Mutex<VecDeque<(u64, u64, bool, Arc<Vec<Value>>)>>,
}

impl SmalltalkRuntime {
    pub(crate) fn claim_registry(&self) -> &st3_schema::Registry {
        #[cfg(test)]
        if let Some(registry) = self.claim_registry.get() {
            return registry;
        }
        st3_schema::registry()
    }
}

impl Runtime for SmalltalkRuntime {
    fn migrate_schema(&self, connection: &Connection) -> Result<()> {
        migrate_schema(connection)
    }

    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(arrangements::SCHEMA)?;
        migrate_local_usage_seen(connection)?;
        backfill_message_index(connection)?;
        resources::create_schema(connection)
    }

    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()> {
        resources::open(transaction)?;
        arrangements::open(transaction)?;
        if shared_memory {
            rebuild_operations_tx(transaction)?;
            rebuild_planning_tx(transaction)?;
            return Ok(());
        }
        let upgraded: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='canonical_shared_projection_rules' AND value='2')",
            [],
            |row| row.get(0),
        )?;
        if !upgraded {
            replay_graph_from_nothing_tx(transaction)?;
            transaction.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES('derived_tables_version',?1)",
                [DERIVED_TABLES_VERSION],
            )?;
            transaction.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES('canonical_shared_projection_rules','2')",
                [],
            )?;
            if !work_extension_roots_tx(transaction)?.1 {
                mark_work_extensions_projected_tx(transaction)?;
            }
        } else {
            rebuild_derived_tables_once_tx(transaction)?;
            let rebuilt = migrate_work_extension_projections_tx(transaction)?;
            #[cfg(test)]
            self.work_extension_roots_rebuilt
                .store(rebuilt, Ordering::Relaxed);
            #[cfg(not(test))]
            let _ = rebuilt;
        }
        migrate_occurrence_creation_projections_tx(transaction)?;
        Ok(())
    }

    fn schema_digest(&self) -> String {
        compatibility_digest(&self.claim_registry().digest())
    }

    fn classify_replicated_claim(
        &self,
        _connection: &Connection,
        _batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, St3Error> {
        classify_replicated_claim_with_registry(claim, self.claim_registry())
    }

    fn append_claim(
        &self,
        store: &GraphStore,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        append_claim_fenced_outcome(store, input, None)
    }

    fn apply_repair_tx(
        &self,
        transaction: &Transaction<'_>,
        repaired: &str,
        replacement: &str,
    ) -> Result<()> {
        select_desired_repair_tx(transaction, repaired, replacement)
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
        append_claim_tx(
            transaction,
            origin,
            subject,
            kind,
            actor,
            body,
            predecessors,
            forced_batch,
        )
    }

    fn project_incremental(
        &self,
        transaction: &Transaction<'_>,
        origin: &str,
        through: u64,
    ) -> Result<bool, St3Error> {
        try_project_simple_replication_tx(transaction, origin, through)
    }

    fn replay_from_nothing(&self, transaction: &Transaction<'_>) -> Result<(), St3Error> {
        replay_graph_from_nothing_tx(transaction)
    }

    fn after_projection(&self, transaction: &Transaction<'_>) -> Result<(), St3Error> {
        resources::flush(transaction).map_err(internal)?;
        reapply_local_work_lease_renewals_tx(transaction)
    }

    fn forget_views(&self) {
        let mut cache = self
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.views.clear();
        cache.statuses.clear();
        drop(cache);
        self.agent_status_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.agent_resources_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] {
        PROJECTION_DIGEST_TABLES
    }

    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] {
        &GRAPH_DIGEST_TABLES
    }

    fn checkpoint_rules_digest(&self) -> String {
        checkpoint_rules::rules_digest()
    }

    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan {
        checkpoint_rules::plan_drops(sealed)
    }

    fn clear_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()> {
        checkpoint_rules::clear_projections(transaction)
    }

    fn replay_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()> {
        checkpoint_rules::replay_from_nothing(transaction)
    }

    fn checkpoint_subject_answers(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<Value> {
        checkpoint_rules::subject_answers(connection, subject, cut)
    }
}

/// The version of smalltalk's shared projection layout, beside the claim vocabulary. Nodes whose
/// layouts differ keep exchanging claim authority but do not compare projection maps.
const SHARED_PROJECTION_LAYOUT: &str = "st3.shared-projections.arrangements.v1";

/// The replication `schema_digest`: the claim vocabulary digest and the shared projection layout.
pub(crate) fn compatibility_digest(registry_digest: &str) -> String {
    canonical_hash(&(SHARED_PROJECTION_LAYOUT, registry_digest))
        .expect("projection compatibility identity serializes")
}
