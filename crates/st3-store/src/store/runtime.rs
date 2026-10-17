//! smalltalk as a runtime on the graph: the claim kinds it knows, the tables it projects them
//! into, its checkpoint rules, and the caches of its projections. `smallclaims` reaches all of
//! these only through [`smallclaims::store::Runtime`].

use super::*;

/// smalltalk's half of the store. The graph holds it as its runtime, and `Store` keeps it beside
/// the graph for the caches its reads use.
#[derive(Default)]
pub struct SmalltalkRuntime {
    /// Committed launch-source invalidation, independent of append frontiers.
    pub publication_cache_epoch: Mutex<Option<(i64, bool)>>,
    pub publication_cache_observer: Mutex<Option<smallclaims::sqlite::CommitObserver>>,
    pub git_change_registry: std::sync::OnceLock<Arc<crate::changes::service::Registry>>,
    /// One registry per graph. Registration alone does not certify source coverage.
    pub ivm_views: Option<Arc<smallclaims::ivm::Views>>,
    /// Receipt waits and collection sockets share this one commit observer. Publisher
    /// construction is serialized, including simultaneous first subscriptions.
    pub ivm_publisher: Mutex<Option<Arc<smallclaims::ivm::events::Publisher>>>,
    pub mailbox_wakes: std::sync::OnceLock<Arc<mailbox_wakes::Wakes>>,
    #[cfg(any(test, feature = "test-support"))]
    pub work_extension_roots_rebuilt: std::sync::atomic::AtomicUsize,
    /// Simulate different build registries on isolated nodes in compatibility tests.
    #[cfg(any(test, feature = "test-support"))]
    pub claim_registry: std::sync::OnceLock<st3_schema::Registry>,
    pub conversation_owner_generation: std::sync::atomic::AtomicU64,
    pub conversation_owners: Mutex<VecDeque<(u64, Arc<conversation_reads::Owners>)>>,
    pub actual_cache: Mutex<HashMap<String, (u64, Option<Value>)>>,
    pub observer_actuals: Arc<observer_actuals::Actuals>,
    /// Immutable placement ancestry, keyed by the selected declaration claim.
    pub placement_cache: Mutex<HashMap<String, Option<Arc<crate::model::PlacementFence>>>>,
    /// Per-subject status reductions and current-view answers, until their claims change.
    pub subject_cache: Mutex<SubjectCache>,
    pub message_cache: Mutex<HashMap<String, MessageCacheEntry>>,
    pub agent_status_cache: Mutex<VecDeque<AgentStatusEntry>>,
    pub agent_resources_cache: Mutex<VecDeque<AgentResourcesEntry>>,
    /// Each agent's usage fold at the newest cut a read reached, so a card refold reads only
    /// the usage claims after it rather than the agent's whole usage history.
    pub usage_folds: Mutex<UsageFolds>,
    /// Ordering and queue metadata for lazy HTTP pages, shared at the same graph cuts.
    pub agent_page_refs_cache: Mutex<VecDeque<AgentResourcesEntry>>,
    /// Acquire before opening a SQLite snapshot, never while pinning a WAL read mark.
    pub agent_resources_admission: Arc<tokio::sync::Mutex<()>>,
    pub agent_resources_admission_state: Mutex<roster_admission::AdmissionState>,
    /// Set once a daemon keeps the complete roster published off the request path; readers
    /// wake it instead of folding cards themselves.
    pub agent_roster_refresh: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
    /// When the oldest refresh request no refresh has answered yet was made, in Unix ms; 0
    /// when none waits. A refresh clears it only once it publishes.
    pub agent_roster_requested_at: std::sync::atomic::AtomicU64,
    /// Bit 0: a refresh is in flight. Bit 1: the last attempt failed.
    pub agent_roster_attempt: std::sync::atomic::AtomicU8,
    /// Fault retries due in this fold, and deadlines deferred to the next refresh.
    pub agent_roster_fault_retries: Mutex<AgentRosterFaultRetries>,
    /// When an agents list read last asked for the roster, in Unix ms; 0 before the first.
    pub agent_roster_read_at: std::sync::atomic::AtomicU64,
    /// Whether the current overdue refresh request was already reported.
    pub agent_roster_overdue_warned: std::sync::atomic::AtomicBool,
    /// Whether a reader asked for the history roster since the refresher last folded it.
    pub agent_roster_history_wanted: std::sync::atomic::AtomicBool,
    /// Wakes a pausing refresher for a reader that waits for a fresh roster, so that read
    /// waits for one fold rather than the rest of the minimum pause.
    pub agent_roster_fresh_wanted: tokio::sync::Notify,
    /// Rosters assembled in chunks because no short fold could complete them, by why.
    pub agent_roster_chunked: Mutex<BTreeMap<String, u64>>,
    /// Counts complete current roster publications, same graph index or not, so collection
    /// streams that read an earlier one reread the newer.
    pub agent_roster_published: tokio::sync::watch::Sender<u64>,
    /// Every other collection's published view revisions, for the same rereads.
    pub published_views: published_views::PublishedViews,
    /// The attention list a daemon's refresher publishes for collection windows.
    pub attention_list: super::attention_list::AttentionList,
    /// Every person's glasses and arrangements, published by the same refresher.
    pub glass_list: super::owner_lists::OwnerList,
    pub arrangement_list: super::owner_lists::OwnerList,
    /// Each selected person's summary row, published by the same refresher.
    pub summary_list: super::summary_list::SummaryList,
    /// Open projection divergence faults at the last divergence check; `None` until a check
    /// runs, so the SLO reads no claims for it and shows nothing on a daemon that never checks.
    pub open_divergences: Mutex<Option<u64>>,
    /// The current missions list, while a refresher keeps it published.
    pub published_missions: published_list::PublishedList<mission_list::MissionRows>,
    /// The current work list, while a refresher keeps it published.
    pub published_work: published_list::PublishedList<work_list::WorkRows>,
    #[cfg(any(test, feature = "test-support"))]
    pub agent_resources_builds: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    pub agent_resources_refolded_cards: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    pub agent_resources_largest_fold: std::sync::atomic::AtomicUsize,
    /// Direct current-work reads, which an enabled work list never makes.
    #[cfg(any(test, feature = "test-support"))]
    pub direct_work_reads: std::sync::atomic::AtomicUsize,
    /// Seats whose queue order a work fold read; runs a warm fold looked up in its reverse
    /// index; and the seats the last fold read.
    #[cfg(any(test, feature = "test-support"))]
    pub seat_order_reads: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    pub seat_lookups: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "test-support"))]
    pub last_seats_read: std::sync::Mutex<BTreeSet<String>>,
}

#[derive(Default)]
pub struct AgentRosterFaultRetries {
    pub due: [BTreeSet<String>; 2],
    pub pending: [BTreeSet<String>; 2],
}

#[derive(Clone)]
pub struct AgentResourcesEntry {
    pub index: u64,
    pub local: u64,
    pub history: bool,
    /// None certifies the whole roster; Some records the lazily materialized page subjects.
    pub covered: Option<BTreeSet<String>>,
    /// Earliest wall-clock boundary in queue metadata; absent means no expiring work lease.
    pub valid_until_unix_ms: Option<u128>,
    pub items: Arc<Vec<Value>>,
    /// When these rows were folded and published, in Unix ms: the roster's "as of".
    pub published_at_unix_ms: u128,
}

impl SmalltalkRuntime {
    fn finish_schema(&self, connection: &Connection, fresh: bool) -> Result<()> {
        runtime_starts::create_schema(connection)?;
        migrate_mission_run_deadlines(connection)?;
        connection.execute_batch(git_changes::SCHEMA)?;
        retirements::create_selection_triggers(connection)?;
        usage_period::create_schema(connection)?;
        migrate_local_usage_seen(connection)?;
        backfill_message_index(connection)?;
        idle_mail::create_schema(connection)?;
        unread_mail::create_schema(connection)?;
        // The remaining initial DDL has no backfill transaction. Keep existing-store
        // upgrade progress unchanged, and commit this chunk only for a fresh store.
        // These helpers must not control transactions, change connection PRAGMAs or VACUUM.
        let transaction = if fresh {
            Some(connection.unchecked_transaction()?)
        } else {
            None
        };
        resources::create_schema(connection)?;
        custom::create_schema(connection)?;
        agent_messages::create_schema(connection)?;
        coordination::create_schema(connection)?;
        stop_deferral::create_schema(connection)?;
        publication_facts::create_schema(connection)?;
        glass_heads::create_schema(connection)?;
        directive_notes::create_schema(connection)?;
        limits::create_limits_schema(connection)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        if let Some(views) = &self.ivm_views {
            // Registered stores run this on every open; each view's schema hook must
            // be idempotent, including reopen with persisted outputs and event cursors.
            views.create_schema(connection)?;
        }
        Ok(())
    }

    /// Explicitly register shadow views without changing the production readers.
    /// The source adapter must initialize/install and certify them separately.
    pub fn with_ivm_views(views: Arc<smallclaims::ivm::Views>) -> Self {
        Self {
            ivm_views: Some(views),
            ..Self::default()
        }
    }

    pub fn claim_registry(&self) -> &st3_schema::Registry {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(registry) = self.claim_registry.get() {
            return registry;
        }
        st3_schema::registry()
    }
}

impl Runtime for SmalltalkRuntime {
    fn writer_observer(&self) -> Option<Arc<dyn smallclaims::sqlite::WriterObserver>> {
        Some(self.observer_actuals.clone())
    }
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
        install_initial_schema(connection, false)?;
        self.finish_schema(connection, false)
    }

    fn create_schema_for_fresh_store(&self, connection: &Connection) -> Result<()> {
        // Store-open calls this in autocommit after the core installation has committed.
        // Existing stores use the per-statement path, preserving completed upgrade indexes.
        install_initial_schema(connection, true)?;
        self.finish_schema(connection, true)
    }

    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()> {
        if let Some(views) = &self.ivm_views {
            // An admitted frontier identifies pending work, never successful projection.
            // Existing sources require an explicit installer and dependency certificate.
            let admitted = current_index_tx(transaction)?;
            let cut = match smallclaims::ivm::source_cut(transaction)? {
                Some(mut cut) => {
                    // A plain or older writer can admit source while no view adapter is
                    // attached. Reopen preserves the certified prefix, never promotes it.
                    cut.admitted = cut.admitted.max(admitted);
                    cut
                }
                None => smallclaims::ivm::SourceCut {
                    epoch: 1,
                    admitted,
                    projected: 0,
                    local_generation: 0,
                },
            };
            views.initialize_empty(transaction, cut)?;
            smallclaims::ivm::events::install(transaction, 4096)?;
        }
        projectors::run_hooks(projectors::OPEN, transaction)?;
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
            #[cfg(any(test, feature = "test-support"))]
            self.work_extension_roots_rebuilt
                .store(rebuilt, Ordering::Relaxed);
            #[cfg(not(test))]
            let _ = rebuilt;
        }
        migrate_occurrence_creation_projections_tx(transaction)?;
        Ok(())
    }

    fn schema_digest(&self) -> String {
        // Older peers cannot compare individual definition versions. Their existing
        // registry comparison suppresses graph heals, while signed log admission continues.
        canonical_hash(&(self.projection_registry_digest(), self.projection_versions()))
            .expect("projection definition identity serializes")
    }

    fn projection_registry_digest(&self) -> String {
        compatibility_digest(&self.claim_registry().digest())
    }

    fn projection_versions(&self) -> BTreeMap<String, u64> {
        BTreeMap::from([("mission_run_deadlines".into(), 3)])
    }

    fn classify_replicated_claim(
        &self,
        _connection: &Connection,
        _batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, St3Error> {
        classify_replicated_claim_with_registry(claim, self.claim_registry())
    }

    fn classify_replicated_claim_unbound(
        &self,
        _batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Option<Result<ReplicatedClaimAdmission, St3Error>> {
        // The registry is this build's schema, fixed for the process.
        Some(classify_replicated_claim_with_registry(claim, self.claim_registry()))
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
        projectors::run_hooks(projectors::FLUSH, transaction).map_err(internal)?;
        reapply_local_work_lease_renewals_tx(transaction)
    }

    fn forget_views(&self) {
        self.observer_actuals.invalidate();
        let mut cache = self
            .subject_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.views.clear();
        cache.statuses.clear();
        cache.summary_statuses.clear();
        cache.runtime_statuses.clear();
        cache.card_statuses.clear();
        drop(cache);
        let mut owners = self.conversation_owners.lock().unwrap_or_else(PoisonError::into_inner);
        self.conversation_owner_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        owners.clear();
        drop(owners);
        self.agent_status_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.agent_resources_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        *self.usage_folds.lock().unwrap_or_else(PoisonError::into_inner) = UsageFolds::default();
        self.agent_roster_published.send_modify(|revision| *revision += 1);
        self.attention_list.forget();
        self.glass_list.forget();
        self.arrangement_list.forget();
        self.summary_list.forget();
        // Projections replaced without a new claim: the lists fold from nothing, dropping any
        // fold under way, before the windows that read them are told to reread.
        self.published_missions.forget();
        self.published_work.forget();
        self.published_views.invalidate();
        self.agent_page_refs_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // Nothing is published now, and a refold from the log takes seconds on a large store.
        // Where agents lists are being read, start it here rather than when the next read finds
        // no roster and waits for it. A node nobody reads keeps folding only on demand, so a
        // long catch-up or trim that forgets once per chunk does not refold it once per chunk.
        let read_at = self.agent_roster_read_at.load(std::sync::atomic::Ordering::Acquire);
        let read_lately = read_at != 0
            && (now_ms() as u64).saturating_sub(read_at) < AGENT_ROSTER_READ_LATELY.as_millis() as u64;
        if let Some(wake) = self.agent_roster_refresh.get().filter(|_| read_lately) {
            let _ = self.agent_roster_requested_at.compare_exchange(
                0, now_ms() as u64, std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            );
            wake.notify_one();
        }
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

    fn checkpoint_claim_facts(
        &self,
        claim: &ClaimRecord,
    ) -> smallclaims::store::checkpoint_staging::CheckpointClaimFacts {
        checkpoint_rules::claim_facts(claim)
    }

    fn plan_staged_checkpoint_drops(
        &self,
        planner: &smallclaims::store::checkpoint_staging::StagedPlanner<'_>,
    ) -> Result<()> {
        checkpoint_rules::plan_staged(planner)
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

    fn checkpoint_global_answers(&self, transaction: &Transaction<'_>) -> Result<Option<Value>> {
        checkpoint_rules::global_answers(transaction).map(Some)
    }

    fn checkpoint_subject_answers_with_sources(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<(Value, smallclaims::store::checkpoint::CheckpointAnswerSources)> {
        checkpoint_rules::subject_answers_with_sources(connection, subject, cut)
    }
}

/// The version of smalltalk's shared projection layout, beside the claim vocabulary. Nodes whose
/// layouts differ keep exchanging claim authority but do not compare projection maps.
/// How recently an agents list read must have asked for the roster for a forget to refold it
/// at once rather than on the next read.
const AGENT_ROSTER_READ_LATELY: std::time::Duration = std::time::Duration::from_secs(300);

const SHARED_PROJECTION_LAYOUT: &str =
    "st3.shared-projections.person-directive-notes.forward-compat.v1";

/// The replication `schema_digest`: the claim vocabulary digest and the shared projection layout.
pub fn compatibility_digest(registry_digest: &str) -> String {
    canonical_hash(&(SHARED_PROJECTION_LAYOUT, registry_digest))
        .expect("projection compatibility identity serializes")
}

fn install_initial_schema(connection: &Connection, fresh: bool) -> Result<()> {
    if fresh {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.execute_batch(&RUNTIME_ID_INDEX)?;
        transaction.execute_batch(arrangements::SCHEMA)?;
        transaction.execute_batch(retirements::SCHEMA)?;
        transaction.commit()?;
    } else {
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(&RUNTIME_ID_INDEX)?;
        connection.execute_batch(arrangements::SCHEMA)?;
        connection.execute_batch(retirements::SCHEMA)?;
    }
    Ok(())
}

#[cfg(test)]
mod schema_install_tests {
    use super::*;

    #[test]
    fn complete_runtime_schema_does_not_take_a_held_writer_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schema.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(smallclaims::store::SCHEMA).unwrap();
        install_initial_schema(&writer, true).unwrap();
        writer
            .execute_batch("PRAGMA user_version=18; BEGIN IMMEDIATE;")
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).unwrap();
        install_initial_schema(&reader, false).unwrap();
        assert!(reader.is_autocommit());
        assert!(reader.execute_batch("BEGIN IMMEDIATE;").is_err());
    }

    #[test]
    fn incomplete_runtime_schema_keeps_completed_ddl() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(smallclaims::store::SCHEMA)
            .unwrap();
        connection.execute_batch("CREATE TABLE mission_runs(id TEXT PRIMARY KEY); INSERT INTO mission_runs VALUES('preserved');").unwrap();
        assert!(
            SmalltalkRuntime::default()
                .create_schema(&connection)
                .is_err()
        );
        assert!(connection.is_autocommit());
        assert!(connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='claims_owned_set_subject_index')", [], |row| row.get::<_, bool>(0)).unwrap());
        assert_eq!(
            connection
                .query_row("SELECT id FROM mission_runs", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }

    #[test]
    fn failed_runtime_schema_install_rolls_back_its_ddl_and_preserves_existing_data() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(smallclaims::store::SCHEMA)
            .unwrap();
        // This damaged table fails after the runtime's early claim indexes were created.
        connection.execute_batch("CREATE TABLE mission_runs(id TEXT PRIMARY KEY); INSERT INTO mission_runs VALUES('existing');").unwrap();
        let error = SmalltalkRuntime::default()
            .create_schema_for_fresh_store(&connection)
            .unwrap_err();
        assert!(error.to_string().contains("mission_id"), "{error:#}");
        assert!(connection.is_autocommit());
        assert_eq!(
            connection
                .query_row("SELECT id FROM mission_runs", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "existing"
        );
        assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='claims_owned_set_subject_index')", [], |row| row.get::<_, bool>(0)).unwrap());
    }

    fn late_install_error_fixture() -> (Connection, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(smallclaims::store::SCHEMA)
            .unwrap();
        connection.execute_batch("CREATE TABLE fixture_marker(value TEXT); INSERT INTO fixture_marker VALUES('preserved');").unwrap();
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = reached.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::CreateTable { table_name } if table_name == "coordination_sends") {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                return Authorization::Deny;
            }
            Authorization::Allow
        }));
        (connection, reached)
    }

    #[test]
    fn failed_late_fresh_install_rolls_back_only_its_chunk() {
        use rusqlite::hooks::{AuthContext, Authorization};
        let (connection, reached) = late_install_error_fixture();
        SmalltalkRuntime::default()
            .create_schema_for_fresh_store(&connection)
            .unwrap_err();
        connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            "the injected late failure must run"
        );
        assert!(connection.is_autocommit());
        assert_eq!(
            connection
                .query_row("SELECT value FROM fixture_marker", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserved"
        );
        assert!(connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='claims_owned_set_subject_index')", [], |row| row.get::<_, bool>(0)).unwrap(), "the earlier committed initial chunk must survive");
        for name in [
            "resource_observations",
            "custom_registrations",
            "local_custom_dirty",
        ] {
            assert!(
                !connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                        [name],
                        |row| row.get::<_, bool>(0)
                    )
                    .unwrap(),
                "late chunk object {name} survived rollback"
            );
        }
    }

    #[test]
    fn failed_late_populated_install_keeps_completed_ddl() {
        use rusqlite::hooks::{AuthContext, Authorization};
        let (connection, reached) = late_install_error_fixture();
        SmalltalkRuntime::default()
            .create_schema(&connection)
            .unwrap_err();
        connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            "the injected late failure must run"
        );
        assert!(connection.is_autocommit());
        assert_eq!(
            connection
                .query_row("SELECT value FROM fixture_marker", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserved"
        );
        for name in [
            "resource_observations",
            "custom_registrations",
            "local_custom_dirty",
        ] {
            assert!(
                connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                        [name],
                        |row| row.get::<_, bool>(0)
                    )
                    .unwrap(),
                "completed populated-store DDL {name} was lost"
            );
        }
    }

    #[test]
    fn fresh_late_runtime_ddl_has_one_transaction_boundary() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(smallclaims::store::SCHEMA)
            .unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let committed = events.clone();
        connection.commit_hook(Some(move || {
            committed.lock().unwrap().push("commit".into());
            false
        }));
        let observed = events.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            let event = match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => Some("begin".into()),
                AuthAction::CreateTable { table_name } => Some(format!("table:{table_name}")),
                AuthAction::Pragma { pragma_name, .. }
                    if pragma_name.eq_ignore_ascii_case("journal_mode")
                        || pragma_name.eq_ignore_ascii_case("foreign_keys") =>
                {
                    Some(format!("connection-pragma:{pragma_name}"))
                }
                _ => None,
            };
            if let Some(event) = event {
                observed.lock().unwrap().push(event);
            }
            Authorization::Allow
        }));
        SmalltalkRuntime::default()
            .create_schema_for_fresh_store(&connection)
            .unwrap();
        // SQLite silently ignores this connection change within a transaction. The
        // authorizer must still see it, or the late-chunk guard could give false assurance.
        connection.execute_batch("BEGIN; PRAGMA foreign_keys=ON; ROLLBACK;").unwrap();
        connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        connection.commit_hook(None::<fn() -> bool>);
        let events = events.lock().unwrap();
        let first = events
            .iter()
            .position(|event| event == "table:resource_observations")
            .unwrap();
        let last = events
            .iter()
            .position(|event| event == "table:account_limit_seats")
            .unwrap();
        assert!(first < last);
        assert_eq!(
            events[..first]
                .iter()
                .rev()
                .find(|event| *event == "begin" || *event == "commit")
                .unwrap(),
            "begin",
            "the late chunk must have acquired its transaction"
        );
        assert!(
            !events[first..=last]
                .iter()
                .any(|event| event == "begin" || event == "commit"),
            "late DDL must not commit between objects: {events:?}"
        );
        assert_eq!(
            events[last + 1..]
                .iter()
                .find(|event| *event == "begin" || *event == "commit")
                .unwrap(),
            "commit"
        );
        let start = events[..first]
            .iter()
            .rposition(|event| event == "begin")
            .unwrap();
        let end = events[last + 1..]
            .iter()
            .position(|event| event == "commit")
            .unwrap()
            + last
            + 1;
        assert!(
            !events[start..=end]
                .iter()
                .any(|event| event.starts_with("connection-pragma:")),
            "connection PRAGMAs must remain outside the fresh late chunk: {events:?}"
        );
        assert!(events[end + 1..].iter().any(|event| event == "connection-pragma:foreign_keys"),
            "the ignored-PRAGMA positive control must be recorded");
        // VACUUM or a nested BEGIN in any helper makes the real fresh install above fail.
        assert!(connection.is_autocommit());
    }

    #[test]
    fn crashed_schema_chunks_recover_without_losing_existing_data() {
        if std::env::var_os("ST3_SCHEMA_CRASH_PHASE").is_some() {
            crash_during_schema_install_child();
            unreachable!("the private child must exit at its crash point");
        }
        for phase in ["core", "runtime", "late-runtime"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("schema.sqlite");
            let marker = directory.path().join("crash-marker");
            {
                let connection = Connection::open(&path).unwrap();
                connection.execute_batch("CREATE TABLE fixture_marker(value TEXT); INSERT INTO fixture_marker VALUES('preserved'); PRAGMA user_version=18;").unwrap();
                if phase != "core" {
                    connection
                        .execute_batch(smallclaims::store::SCHEMA)
                        .unwrap();
                }
            }
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "store::runtime::schema_install_tests::crashed_schema_chunks_recover_without_losing_existing_data",
                    "--nocapture",
                ])
                .env("ST3_SCHEMA_CRASH_PATH", &path)
                .env("ST3_SCHEMA_CRASH_MARKER", &marker)
                .env("ST3_SCHEMA_CRASH_PHASE", phase)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(86), "{phase}: crash hook must run");
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), phase);
            {
                let connection = Connection::open(&path).unwrap();
                let partial = if phase == "core" {
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name IN ('meta','batches'))"
                } else if phase == "runtime" {
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='claims_owned_set_subject_index')"
                } else {
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='resource_observations')"
                };
                assert!(
                    !connection
                        .query_row(partial, [], |row| row.get::<_, bool>(0))
                        .unwrap(),
                    "{phase}: uncommitted DDL survived the process crash"
                );
            }
            let recovered = Store::open(&path, "birch").unwrap();
            let reader = recovered.readers.get();
            assert_eq!(
                reader
                    .query_row("SELECT value FROM fixture_marker", [], |row| row
                        .get::<_, String>(0))
                    .unwrap(),
                "preserved"
            );
            assert_eq!(
                reader
                    .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                st3_schema::STORAGE_VERSION
            );
            assert_eq!(
                recovered
                    .connection
                    .write()
                    .pragma_query_value(None, "synchronous", |row| row.get::<_, u32>(0))
                    .unwrap(),
                2
            );
        }
    }

    fn crash_during_schema_install_child() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let path = std::env::var_os("ST3_SCHEMA_CRASH_PATH").expect("private child database path");
        let marker =
            std::env::var_os("ST3_SCHEMA_CRASH_MARKER").expect("private child marker path");
        let phase = std::env::var("ST3_SCHEMA_CRASH_PHASE").expect("private child phase");
        let target = match phase.as_str() {
            "core" => "claims",
            "runtime" => "mission_runs",
            "late-runtime" => "coordination_sends",
            _ => panic!("unknown private crash phase"),
        };
        let connection = Connection::open(path).unwrap();
        let marked_phase = phase.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::CreateTable { table_name } if table_name == target) {
                std::fs::write(&marker, &marked_phase).unwrap();
                // SAFETY: this private child must exit without SQLite or Rust cleanup.
                unsafe { libc::_exit(86) };
            }
            Authorization::Allow
        }));
        if phase == "core" {
            connection
                .execute_batch(smallclaims::store::FRESH_SCHEMA)
                .unwrap();
        } else {
            SmalltalkRuntime::default()
                .create_schema_for_fresh_store(&connection)
                .unwrap();
        }
        panic!("the crash point was not reached");
    }
}

#[cfg(test)]
mod ivm_attachment_tests {
    use super::*;
    use smallclaims::ivm::{Contribution, Definition, Readiness, View, Views, events};
    use smallclaims::store::canonical;
    use std::sync::Barrier;

    struct Shadow;
    impl View for Shadow {
        fn definition(&self) -> Definition {
            Definition {
                name: "fixture.shadow",
                fingerprint: "fixture.shadow.v1",
                kinds: &[],
                local_kinds: &["fixture.clock"],
                max_contributions: 1,
            }
        }
        fn contributions(
            &self,
            _claim: &ClaimRecord,
            _rank: &canonical::ClaimKey,
        ) -> Result<Vec<Contribution>> {
            Ok(Vec::new())
        }
    }
    fn registry() -> Arc<Views> {
        Arc::new(Views::new(vec![Box::new(Shadow)]).unwrap())
    }
    fn boundary(store: &Store, views: &Views) -> events::Boundary {
        store
            .read_snapshot(|_| events::capture(&store.readers.get(), views, "fixture.shadow"))
            .unwrap()
    }
    fn admit_agent(store: &Store) {
        let source = "version 2\nagent \"ember\" { workspace \"/tmp\"; command \"true\" }\n";
        let intent = crate::graph::parse_test_intent(source, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &plan.subject_tokens, "fixture")
            .unwrap();
        assert!(store.index().unwrap() > 0);
    }

    #[test]
    fn ordinary_store_does_not_install_or_attach_shadow_views() {
        let store = Store::open_memory("alder").unwrap();
        assert!(store.ivm_views().is_none());
        assert!(store.ivm_publisher().unwrap().is_none());
        let installed: bool = store
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ivm_event_state')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!installed);
    }

    #[test]
    fn concurrent_consumers_share_one_store_publisher_and_registry() {
        let root = tempfile::tempdir().unwrap();
        let views = registry();
        let store = Arc::new(
            Store::open_with_ivm_views(&root.path().join("claims.db"), "alder", views.clone())
                .unwrap(),
        );
        assert!(Arc::ptr_eq(&store.ivm_views().unwrap(), &views));
        assert!(matches!(
            boundary(&store, &views).availability.readiness,
            Readiness::Ready(_)
        ));
        let start = Arc::new(Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let (store, start) = (store.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    store.ivm_publisher().unwrap().unwrap()
                })
            })
            .collect::<Vec<_>>();
        let publishers = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(
            publishers
                .iter()
                .all(|publisher| Arc::ptr_eq(publisher, &publishers[0]))
        );
        assert!(Arc::ptr_eq(
            &store.ivm_publisher().unwrap().unwrap(),
            &publishers[0]
        ));
        let mut socket = publishers[0].subscribe();
        let mut receipt = publishers[1].subscribe();
        store
            .connection
            .batched(events::rotate_identity)
            .unwrap()
            .unwrap();
        let socket_notice = socket.try_recv().unwrap();
        let receipt_notice = receipt.try_recv().unwrap();
        assert_eq!(socket_notice, receipt_notice);
        assert!(matches!(socket_notice, events::Notice::Committed(_)));
    }

    #[test]
    fn populated_install_and_plain_writer_reopen_preserve_uncertified_prefix() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claims.db");
        {
            let store = Store::open(&path, "alder").unwrap();
            admit_agent(&store);
        }
        let views = registry();
        let old = {
            let store = Store::open_with_ivm_views(&path, "alder", views.clone()).unwrap();
            let boundary = boundary(&store, &views);
            assert!(boundary.source_cut.admitted > 0);
            assert_eq!(boundary.source_cut.projected, 0);
            assert!(!matches!(
                boundary.availability.readiness,
                Readiness::Ready(_)
            ));
            boundary
        };
        {
            let store = Store::open(&path, "alder").unwrap();
            let source = "version 2\nagent \"cinder\" { workspace \"/tmp\"; command \"true\" }\n";
            let intent = crate::graph::parse_test_intent(source, "alder").unwrap();
            let plan = store
                .mission(
                    &intent,
                    IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            store
                .apply(&intent, &plan.subject_tokens, "second-fixture")
                .unwrap();
            assert!(store.index().unwrap() > old.source_cut.admitted);
        }
        let store = Store::open_with_ivm_views(&path, "alder", views.clone()).unwrap();
        let current = boundary(&store, &views);
        assert_eq!(current.identity, old.identity);
        assert!(current.source_cut.admitted > old.source_cut.admitted);
        assert_eq!(current.source_cut.projected, old.source_cut.projected);
        assert!(!matches!(
            current.availability.readiness,
            Readiness::Ready(_)
        ));
    }
}
