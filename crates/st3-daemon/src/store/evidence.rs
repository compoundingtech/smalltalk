use super::*;

impl Store {
    /// Lazily attach the one Store publisher shared by receipt waits and collection
    /// consumers. Registration and journal installation happen when the Store opens.
    /// Attachment errors are returned on each call until attachment succeeds; failures
    /// are not cached. A successful attachment is shared by all later callers.
    pub fn ivm_publisher(&self) -> Result<Option<Arc<smallclaims::ivm::events::Publisher>>> {
        if self.smalltalk.ivm_views.is_none() {
            return Ok(None);
        }
        let mut publisher = self
            .smalltalk
            .ivm_publisher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(publisher) = &*publisher {
            return Ok(Some(publisher.clone()));
        }
        let attached = Arc::new(smallclaims::ivm::events::Publisher::attach(&self.graph, 128)?);
        *publisher = Some(attached.clone());
        Ok(Some(attached))
    }

    /// Whether the newest complete roster published at or before `index` already shows every
    /// card as it would at `index`: no claim since its cut changes a card's inputs (the same
    /// delta the incremental fold trusts), no agent timeline row arrived, and no queue deadline
    /// passed. A read that must see what was written before it need not wait for a refresh
    /// that would fold nothing. One range read over the claims since the cut; it never folds.
    pub(crate) fn published_agent_roster_unchanged_through(
        &self,
        index: u64,
        history: bool,
    ) -> Result<bool> {
        if self.smalltalk.agent_roster_refresh.get().is_none()
            || self.agent_roster_faults_pending(history) {
            return Ok(false);
        }
        let Some((cut, local, valid_until, items)) = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter()
            .filter(|entry| entry.history == history && entry.covered.is_none() && entry.index <= index)
            .max_by_key(|entry| (entry.index, entry.local))
            .map(|entry| (entry.index, entry.local, entry.valid_until_unix_ms, Arc::clone(&entry.items)))
        else {
            return Ok(false);
        };
        if valid_until.is_some_and(|expiry| now_ms() >= expiry)
            || roster_local_frontier(&self.readers.get(), index)? != local
        {
            return Ok(false);
        }
        if cut == index {
            return Ok(true);
        }
        Ok(match self.changed_agent_resources(cut, index, &items)? {
            AgentResourcesChange::Delta(delta) => delta.subjects.is_empty() && !delta.queues && !delta.membership,
            AgentResourcesChange::Invalidated { .. } => false,
        })
    }

    /// The newest complete current roster published at or before `index`, with its own graph
    /// index, while a refresher keeps advancing it. A reader serves these rows under that
    /// index's snapshot, never under its own newer one, and wakes the refresher. Without a
    /// refresher there is none: readers then fold exactly their own cut, as before.
    pub(crate) fn published_agent_roster(
        &self,
        index: u64,
        history: bool,
    ) -> Option<(u64, Arc<Vec<Value>>, u128)> {
        self.smalltalk.agent_roster_refresh.get()?;
        self.warn_if_agent_roster_overdue();
        self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter()
            .filter(|entry| entry.history == history && entry.covered.is_none() && entry.index <= index)
            .max_by_key(|entry| (entry.index, entry.local))
            .map(|entry| {
                crate::otel::record_roster("hit", entry.items.len());
                (entry.index, Arc::clone(&entry.items), entry.published_at_unix_ms)
            })
    }

    /// The complete roster published at exactly `cut`, for a page continuing from a first page
    /// served at that cut, with when it was published.
    pub(crate) fn published_agent_roster_at(
        &self,
        cut: u64,
        history: bool,
    ) -> Option<(Arc<Vec<Value>>, u128)> {
        self.smalltalk.agent_roster_refresh.get()?;
        self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter()
            .filter(|entry| entry.history == history && entry.covered.is_none() && entry.index == cut)
            .max_by_key(|entry| entry.local)
            .map(|entry| {
                crate::otel::record_roster("hit", entry.items.len());
                (Arc::clone(&entry.items), entry.published_at_unix_ms)
            })
    }

    /// When no complete roster is published yet: the newest current refs at or before `index`
    /// (membership, order and queue metadata of every agent) and the cards of their first
    /// `count` agents, all at that one cut. The refresher publishes this head first as the
    /// daemon starts, so a window or first page need not wait for every card to fold.
    pub(crate) fn published_agent_roster_head(
        &self,
        index: u64,
        count: usize,
    ) -> Option<PublishedRosterHead> {
        self.smalltalk.agent_roster_refresh.get()?;
        self.warn_if_agent_roster_overdue();
        let (cut, refs) = self.smalltalk.agent_page_refs_cache.lock()
            .expect("agent page refs cache poisoned").iter()
            .filter(|entry| !entry.history && entry.index <= index)
            .max_by_key(|entry| entry.index)
            .map(|entry| (entry.index, Arc::clone(&entry.items)))?;
        let cache = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned");
        let head = cache.iter().filter(|entry| entry.index == cut && !entry.history)
            .find_map(|entry| {
                let cards = entry.items.iter()
                    .filter_map(|card| Some((card["id"].as_str()?, card)))
                    .collect::<HashMap<_, _>>();
                refs.iter().take(count)
                    .map(|reference| cards.get(reference["id"].as_str()?).map(|card| (*card).clone()))
                    .collect::<Option<Vec<_>>>()
                    .map(|head| (head, entry.published_at_unix_ms))
            })?;
        crate::otel::record_roster("hit", head.0.len());
        Some((cut, refs, head.0, head.1))
    }

    /// [`Self::agent_resources_cached_at`], with when the rows were published.
    pub(crate) fn agent_resources_published_at(
        &self,
        index: u64,
        history: bool,
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Option<(Arc<Vec<Value>>, u128)>> {
        let fault_retries = self.agent_roster_fault_retries(history);
        if selected.map_or(!fault_retries.is_empty(), |names| !names.is_disjoint(&fault_retries)) {
            return Ok(None);
        }
        if index < current_index(&self.readers.get())? {
            return Ok(None);
        }
        let now = now_ms();
        let local = crate::performance::task("roster/frontier-read", || {
            roster_local_frontier(&self.readers.get(), index)
        })?;
        let cache = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned");
        let hit = cache.iter().rev()
            .find(|entry| entry.index == index && entry.local == local && entry.history == history)
            .filter(|entry| agent_resources_entry_hits(entry, now, index, local, history, selected))
            .map(|entry| (Arc::clone(&entry.items), entry.published_at_unix_ms));
        drop(cache);
        Ok(hit.map(|hit| crate::performance::task("roster/cache-hit", || {
            crate::otel::record_roster("hit", hit.0.len());
            hit
        })))
    }

    /// The actor that published the mission revision `run` works to: a broken gate's attention
    /// goes to it. A nested run works to its root run's revision.
    pub(crate) fn mission_run_publisher(&self, run: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        let id = run.strip_prefix("mission-run/").unwrap_or(run);
        let Some(header) = mission_run_header_tx(&connection, id).optional()? else {
            return Ok(None);
        };
        let root_id = header
            .root_mission_run
            .strip_prefix("mission-run/")
            .unwrap_or(&header.root_mission_run);
        let root = if root_id == header.id {
            header
        } else {
            let Some(root) = mission_run_header_tx(&connection, root_id).optional()? else {
                return Ok(None);
            };
            root
        };
        let mission = root
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&root.mission);
        Ok(connection
            .query_row(
                "SELECT claims.actor FROM mission_revisions
                 JOIN claims ON claims.id=mission_revisions.claim_id
                 WHERE mission_revisions.mission_id=?1 AND mission_revisions.revision=?2",
                params![mission, root.revision],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Admission's transition decision, from the same transaction as its native runtime fence.
    /// The echoed producer record stays unchanged; it need not carry derived history metadata.
    pub(crate) fn append_harness_event_publication(
        &self,
        publication: &crate::harness_events::Publication,
    ) -> Result<(ClaimRecord, bool, bool), St3Error> {
        let mut input = publication.claim.clone();
        if publication.sequence == 0
            || publication.runtime_incarnation.is_empty()
            || !matches!(
                input.kind.as_str(),
                "harness.observed" | "harness.usage" | "harness.limits" | "harness.timeline"
                    | "harness.todo.observed"
            )
            || input.actor.as_deref() != Some(input.subject.as_str())
            || input.idempotency_key.is_none()
        {
            return Err(St3Error::new(
                "invalid-harness-event",
                "invalid native observation envelope",
            ));
        }
        st3_schema::registry()
            .validate_public_claim(
                &input.subject,
                &input.kind,
                &input.fields,
                input.actor.as_deref(),
            )
            .map_err(|error| St3Error::new(error.code, error.message))?;
        // A context event can yield two independently acknowledged usage semantics. Its slot
        // is stable even if a caller mistakenly changes its body on retry.
        let source_runtime = input
            .fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .filter(|runtime| !runtime.is_empty())
            .ok_or_else(|| {
                St3Error::new(
                    "invalid-harness-event",
                    "event has no source runtime provenance",
                )
            })?;
        let slot = input
            .fields
            .get("semantics")
            .and_then(Value::as_str)
            .unwrap_or("");
        input.idempotency_key = Some(format!(
            "harness-event:{}:{}:{}:{}:{}",
            input.subject, source_runtime, publication.sequence, input.kind, slot
        ));
        if input.kind == "harness.observed" {
            validate_claim_input(&input)?;
            append_latest_observation_publication(
                &self.graph, &input, now_ms(), Some(&publication.runtime_incarnation),
            )
        } else {
            append_claim_with_fences(
                &self.graph, &input, None, Some(&publication.runtime_incarnation),
            ).map(|(record, appended)| (record, appended, false))
        }
    }

    /// The newest eligible local driver receipt after this exact check's request in writer
    /// sequence/position order. The same statement reads current SystemLocal starts and
    /// legacy admitted starts. A local row's after_store_index places it after that claim;
    /// this is local log ordering, not the driver's original writer/deadline proof.
    /// Multiple launches cannot bind a no-incarnation driver exit to the current process.
    pub(crate) fn mechanical_gate_exit_receipt(
        &self,
        requested: &ClaimRecord,
    ) -> Result<Option<(ClaimRecord, Option<String>)>> {
        smallclaims::touched::note_read(|| requested.subject.clone());
        smallclaims::touched::note_read(|| "kind:record.repaired".into());
        if requested.origin != self.origin() {
            return Ok(None);
        }
        let request_position = smallclaims::store::canonical::position_sql("request");
        let launch_position = smallclaims::store::canonical::position_sql("created");
        let exit_position = smallclaims::store::canonical::position_sql("claims");
        let connection = self.readers.get();
        let found = connection
            .prepare_cached(&canonical_sql(&format!(
                "WITH current_request AS MATERIALIZED (
                   SELECT request.subject, request.batch_id, request.store_index,
                          request_batch.replica_sequence AS sequence,
                          {request_position} AS position
                   FROM claims request JOIN batches request_batch ON request_batch.id=request.batch_id
                   WHERE request.id=?1 AND request.subject=?2 AND request.kind='gate.requested'
                     AND request.origin=?3 AND request_batch.origin=?3
                     AND NOT EXISTS (SELECT 1 FROM replica_records repaired
                       WHERE repaired.claim_id=request.id AND repaired.state='repaired')
                 ), legacy_launches AS MATERIALIZED (
                   SELECT json_extract(created.body, '$.fields.incarnation_id') AS incarnation,
                          json_extract(created.body, '$.fields.runtime_id') AS runtime_id
                   FROM claims created JOIN batches launch_batch ON launch_batch.id=created.batch_id
                     JOIN current_request
                   WHERE created.subject=?2 AND created.kind='runtime.action.succeeded'
                     AND created.actor IS NULL
                     AND created.origin=?3 AND launch_batch.origin=?3
                     AND json_extract(created.body, '$.fields.action')='start'
                     AND (launch_batch.replica_sequence, {launch_position})
                         > (current_request.sequence, current_request.position)
                     AND (launch_batch.replica_sequence<>current_request.sequence
                          OR created.batch_id=current_request.batch_id)
                     AND NOT EXISTS (SELECT 1 FROM replica_records repaired
                       WHERE repaired.claim_id=created.id AND repaired.state='repaired')
                   ORDER BY CANONICAL_DESC(created) LIMIT 2
                 ), local_launches AS MATERIALIZED (
                   SELECT json_extract(local.body, '$.fields.incarnation_id') AS incarnation,
                          json_extract(local.body, '$.fields.runtime_id') AS runtime_id
                   FROM local_observations local JOIN current_request
                   WHERE local.subject=?2 AND local.kind='runtime.action.succeeded'
                     AND local.actor IS NULL
                     AND json_extract(local.body, '$.fields.action')='start'
                     AND local.after_store_index>=current_request.store_index
                   ORDER BY local.id DESC LIMIT 2
                 ), launches AS MATERIALIZED (
                   SELECT incarnation, runtime_id FROM legacy_launches
                   UNION ALL SELECT incarnation, runtime_id FROM local_launches
                 )
                 SELECT {CLAIM_COLUMNS},
                        (SELECT incarnation FROM launches LIMIT 1),
                        (SELECT COUNT(*) FROM launches),
                        (SELECT runtime_id FROM launches LIMIT 1)
                 FROM claims JOIN batches ON batches.id=claims.batch_id JOIN current_request
                 WHERE claims.subject=?2 AND claims.kind='runtime.observed' AND claims.actor=?2
                   AND claims.origin=?3 AND batches.origin=?3
                   AND (batches.replica_sequence, {exit_position})
                       > (current_request.sequence, current_request.position)
                   AND (batches.replica_sequence<>current_request.sequence
                        OR claims.batch_id=current_request.batch_id)
                   AND NOT EXISTS (SELECT 1 FROM replica_records repaired
                     WHERE repaired.claim_id=claims.id AND repaired.state='repaired')
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1"
            )))?
            .query_row(
                params![requested.id, requested.subject, self.origin()],
                |row| {
                    Ok((
                        claim_from_row(row)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, u64>(11)?,
                        row.get::<_, Option<String>>(12)?,
                    ))
                },
            )
            .optional()?;
        Ok(found
            .filter(|(_, _, launches, runtime_id)| {
                *launches <= 1 && runtime_id.as_deref()
                    .is_none_or(|runtime_id| runtime_id == requested.subject.replace('/', "."))
            })
            .map(|(receipt, incarnation, _, _)| (receipt, incarnation)))
    }

    /// Gate runners are launched directly rather than through desired declarations. Their
    /// owner is durable in the request; older mechanical/LLM requests encode it in the subject.
    pub(crate) fn mission_gate_runners(&self) -> Result<Vec<MissionGateRunner>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT DISTINCT subject, origin,
                    COALESCE(json_extract(body, '$.fields.owner'),
                      'legacy:' || substr(subject, 16, instr(substr(subject, 16), '/')-1))
             FROM claims request WHERE kind='gate.requested'
               AND (json_extract(body, '$.fields.runner') IN ('exec','loop-metric')
                    OR json_extract(body, '$.fields.model') IS NOT NULL)
               AND NOT EXISTS (
                 SELECT 1 FROM claims stopped
                 WHERE stopped.subject=request.subject AND stopped.kind='runtime.observed'
                   AND json_extract(stopped.body, '$.fields.status')='stopped'
               )",
        )?;
        let mut requests = BTreeMap::<String, Vec<(String, String)>>::new();
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (subject, host, owner) = row?;
            // A runner leaves this list once its stop is observed on it.
            smallclaims::touched::note_read(|| subject.clone());
            requests.entry(owner).or_default().push((subject, host));
        }
        // New requests, and owners that finish or move on.
        for kind in [
            "gate.requested",
            "step-run.state",
            "mission-run.state",
            "run-generation.state",
        ] {
            smallclaims::touched::note_read(|| format!("kind:{kind}"));
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        // Match by a map rather than an OR/prefix SQL join. That join visits every step
        // for every historical gate request and stalls reconciliation as runs accumulate.
        let mut owners = connection.prepare(
            "SELECT step.subject, 'mission-run/' || run.id,
                    step.status IN ('completed','failed','cancelled')
                    OR generation.status IN ('completed','failed','cancelled','superseded')
                    OR run.phase='terminal' OR run.phase LIKE 'cleanup-%'
                    OR root.phase='terminal' OR root.phase LIKE 'cleanup-%'
             FROM step_runs step
             JOIN mission_runs run ON run.id=step.run_id
             JOIN mission_runs root ON root.id=run.root_run_id
             JOIN run_generations generation ON generation.id=step.generation_id
             UNION ALL
             SELECT 'mission-run/' || run.id, 'mission-run/' || run.id,
                    run.phase IN ('final-cancelled','terminal') OR run.phase LIKE 'cleanup-%'
                    OR run.status IN ('completed','failed','cancelled')
                    OR root.phase='terminal' OR root.phase LIKE 'cleanup-%'
             FROM mission_runs run
             JOIN mission_runs root ON root.id=run.root_run_id",
        )?;
        let mut runners = Vec::new();
        for row in owners.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })? {
            let (owner, owner_run, retired) = row?;
            for key in [owner.clone(), format!("legacy:{}", owner.replace('/', "."))] {
                if let Some(requests) = requests.get(&key) {
                    runners.extend(requests.iter().map(|(subject, host)| MissionGateRunner {
                        subject: subject.clone(),
                        host: host.clone(),
                        owner_run: owner_run.clone(),
                        owner_step: owner.starts_with("step-run/").then(|| owner.clone()),
                        retired,
                    }));
                }
            }
        }
        runners.sort_by(|left, right| left.subject.cmp(&right.subject));
        Ok(runners)
    }

    pub fn gate_request_for_owner(&self, owner: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql("SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
                 FROM claims WHERE kind='gate.requested'
                 AND json_extract(body, '$.fields.owner')=?1
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                [owner],
                claim_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every human gate request as (request, owner, reviewer): the episodes a human gate's
    /// attention card can name, open or long closed.
    pub fn human_gate_requests(&self) -> Result<Vec<(String, String, String)>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id, json_extract(body, '$.fields.owner'), json_extract(body, '$.fields.reviewer')
             FROM claims INDEXED BY claims_human_gate_request_index
             WHERE kind='gate.requested' AND json_extract(body, '$.fields.reviewer') IS NOT NULL
               AND json_extract(body, '$.fields.owner') IS NOT NULL",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Explicit credential evidence from the currently running native epoch only.
    pub(crate) fn harness_auth_evidence(
        &self,
        subject: &str,
        incarnation: &str,
    ) -> Result<Option<Value>> {
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql(
                    "SELECT body FROM claims INDEXED BY claims_harness_auth_incarnation_index WHERE subject=?1 AND kind='harness.observed'
             AND json_extract(body, '$.fields.incarnation_id')=?2
             AND json_type(body, '$.fields.provider_auth') IN ('true','false')
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                ),
                params![subject, incarnation],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|body| Ok(serde_json::from_str::<Value>(&body)?["fields"].clone()))
            .transpose()
    }
}

/// A roster head: its cut, every agent's refs, the first cards in order, and when published.
pub(crate) type PublishedRosterHead = (u64, Arc<Vec<Value>>, Vec<Value>, u128);
