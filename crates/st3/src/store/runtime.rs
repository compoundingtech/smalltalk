//! smalltalk as a runtime on the graph: the claim kinds it knows, the tables it projects them
//! into, its checkpoint rules, and the caches of its projections. `smallclaims` reaches all of
//! these only through [`smallclaims::store::Runtime`].

use super::*;

/// smalltalk's half of the store. The graph holds it as its runtime, and `Store` keeps it beside
/// the graph for the caches its reads use.
#[derive(Default)]
pub struct SmalltalkRuntime {
    pub(crate) mailbox_wakes: std::sync::OnceLock<Arc<mailbox_wakes::Wakes>>,
    #[cfg(test)]
    pub(crate) work_extension_roots_rebuilt: std::sync::atomic::AtomicUsize,
    /// Simulate different build registries on isolated nodes in compatibility tests.
    #[cfg(test)]
    pub(crate) claim_registry: std::sync::OnceLock<st3_schema::Registry>,
    pub(crate) conversation_owners: Mutex<VecDeque<(u64, Arc<conversation_reads::Owners>)>>,
    pub(crate) actual_cache: Mutex<HashMap<String, (u64, Option<Value>)>>,
    /// Immutable placement ancestry, keyed by the selected declaration claim.
    pub(crate) placement_cache: Mutex<HashMap<String, Option<Arc<crate::placement::Fence>>>>,
    /// Per-subject status reductions and current-view answers, until their claims change.
    pub(crate) subject_cache: Mutex<SubjectCache>,
    pub(crate) message_cache: Mutex<HashMap<String, MessageCacheEntry>>,
    pub(crate) agent_status_cache: Mutex<VecDeque<AgentStatusEntry>>,
    pub(crate) agent_resources_cache: Mutex<VecDeque<AgentResourcesEntry>>,
    /// Ordering and queue metadata for lazy HTTP pages, shared at the same graph cuts.
    pub(crate) agent_page_refs_cache: Mutex<VecDeque<AgentResourcesEntry>>,
    /// Acquire before opening a SQLite snapshot, never while pinning a WAL read mark.
    pub(crate) agent_resources_admission: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    pub(crate) agent_resources_builds: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
pub(crate) struct AgentResourcesEntry {
    pub(crate) index: u64,
    pub(crate) local: u64,
    pub(crate) history: bool,
    /// None certifies the whole roster; Some records the lazily materialized page subjects.
    pub(crate) covered: Option<BTreeSet<String>>,
    /// Earliest wall-clock boundary in queue metadata; absent means no expiring work lease.
    pub(crate) valid_until_unix_ms: Option<u128>,
    pub(crate) items: Arc<Vec<Value>>,
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
    fn idempotency_response_in_use(&self, connection: &Connection, response: &str) -> Result<bool> {
        // Some local entries contain a digest, not JSON. They have no work lifetime to pin.
        let Ok(value) = serde_json::from_str::<Value>(response) else {
            return Ok(false);
        };
        let view = value
            .get("mission_run")
            .filter(|run| run.is_object())
            .unwrap_or(&value);
        for field in ["generation", "source_generation"] {
            if let Some(generation) = view.get(field).and_then(Value::as_str) {
                let active: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM run_generations g JOIN mission_runs r ON r.id=g.run_id
                     WHERE g.id=?1 AND g.status NOT IN ('completed','failed','cancelled','superseded')
                     AND r.phase!='terminal')",
                    [generation.trim_start_matches("run-generation/")],
                    |row| row.get(0),
                )?;
                if active {
                    return Ok(true);
                }
            }
        }
        let run = view
            .get("run")
            .and_then(Value::as_str)
            .or_else(|| view.get("mission_run").and_then(Value::as_str))
            .or_else(|| {
                view.get("subject")
                    .and_then(Value::as_str)
                    .filter(|subject| subject.starts_with("mission-run/"))
            });
        if let Some(run) = run {
            return connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM mission_runs WHERE id=?1 AND phase!='terminal')",
                    [run.trim_start_matches("mission-run/")],
                    |row| row.get(0),
                )
                .map_err(Into::into);
        }
        if let Some(step) = view
            .get("step")
            .and_then(Value::as_str)
            .filter(|step| step.starts_with("step-run/"))
        {
            return connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM step_runs s JOIN mission_runs r ON r.id=s.run_id
                 WHERE s.subject=?1 AND r.phase!='terminal')",
                    [step],
                    |row| row.get(0),
                )
                .map_err(Into::into);
        }
        if let Some(tokens) = view.get("subject_tokens").and_then(Value::as_object) {
            for subject in tokens
                .keys()
                .filter(|subject| subject.starts_with("mission-run/"))
            {
                let active: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM mission_runs WHERE id=?1 AND phase!='terminal')",
                    [subject.trim_start_matches("mission-run/")],
                    |row| row.get(0),
                )?;
                if active {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn migrate_schema(&self, connection: &Connection) -> Result<()> {
        migrate_schema(connection)
    }

    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(arrangements::SCHEMA)?;
        migrate_local_usage_seen(connection)?;
        backfill_message_index(connection)?;
        unread_mail::create_schema(connection)?;
        resources::create_schema(connection)?;
        custom::create_schema(connection)?;
        agent_messages::create_schema(connection)?;
        glass_heads::create_schema(connection)?;
        limits::create_limits_schema(connection)
    }

    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()> {
        custom::open(transaction)?;
        resources::open(transaction)?;
        glass_heads::open(transaction)?;
        agent_messages::open(transaction)?;
        arrangements::open(transaction)?;
        limits::open_limits(transaction)?;
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
    ) -> Result<IncrementalProjection, St3Error> {
        try_project_simple_replication_tx(transaction, origin, through)
    }

    fn replay_from_nothing(&self, transaction: &Transaction<'_>) -> Result<(), St3Error> {
        replay_graph_from_nothing_tx(transaction)
    }

    fn replay_from_nothing_with_progress(
        &self,
        transaction: &Transaction<'_>,
        progress: &mut dyn FnMut(ReplayProgress),
    ) -> Result<(), St3Error> {
        replay_graph_from_nothing_with_progress_tx(transaction, progress)
    }

    fn after_projection(&self, transaction: &Transaction<'_>) -> Result<(), St3Error> {
        custom::flush(transaction).map_err(internal)?;
        resources::flush(transaction).map_err(internal)?;
        glass_heads::flush(transaction).map_err(internal)?;
        agent_messages::flush(transaction).map_err(internal)?;
        limits::flush_limits(transaction).map_err(internal)?;
        reapply_local_work_lease_renewals_tx(transaction)
    }

    fn forget_views(&self) {
        let mut cache = self
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.views.clear();
        cache.statuses.clear();
        cache.card_statuses.clear();
        drop(cache);
        self.conversation_owners.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.agent_status_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.agent_resources_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.agent_page_refs_cache
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
const SHARED_PROJECTION_LAYOUT: &str = "st3.shared-projections.arrangements.v2";

/// The replication `schema_digest`: the claim vocabulary digest and the shared projection layout.
pub(crate) fn compatibility_digest(registry_digest: &str) -> String {
    canonical_hash(&(SHARED_PROJECTION_LAYOUT, registry_digest))
        .expect("projection compatibility identity serializes")
}
