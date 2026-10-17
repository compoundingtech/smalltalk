use super::*;

impl Store {
    #[cfg(any(test, feature = "test-support"))]
    pub fn count_refolded_cards_for_test(&self, cards: usize) {
        self.smalltalk.agent_resources_refolded_cards.fetch_add(cards, std::sync::atomic::Ordering::Relaxed);
        self.smalltalk.agent_resources_largest_fold.fetch_max(cards, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn agent_resources_refolded_cards_for_test(&self) -> usize {
        self.smalltalk.agent_resources_refolded_cards.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Run one refresh, answering the requests made before it began only if it publishes.
    pub fn answer_agent_roster_requests<T>(&self, refresh: impl FnOnce() -> Result<T>) -> Result<T> {
        self.smalltalk.agent_roster_attempt.fetch_or(1, std::sync::atomic::Ordering::AcqRel);
        struct Attempt<'a>(&'a SmalltalkRuntime, bool);
        impl Drop for Attempt<'_> {
            fn drop(&mut self) {
                self.0.agent_roster_attempt.store(if self.1 { 0 } else { 2 }, std::sync::atomic::Ordering::Release);
                // Failure (including an unwind) must also wake readers waiting for publication.
                self.0.agent_roster_published.send_modify(|_| {});
            }
        }
        let mut attempt = Attempt(&self.smalltalk, false);
        let requested = &self.smalltalk.agent_roster_requested_at;
        let pending = requested.swap(0, std::sync::atomic::Ordering::AcqRel);
        let result = refresh();
        attempt.1 = result.is_ok();
        if result.is_err() && pending != 0 {
            // Still unanswered: keep the oldest request time unless a newer request is waiting.
            let _ = requested.compare_exchange(0, pending, std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed);
        }
        if result.is_ok() {
            self.smalltalk.agent_roster_overdue_warned.store(false, std::sync::atomic::Ordering::Release);
        }
        result
    }

    /// Current generation steps for all runs in two SQL scans. Collection reads
    /// never hydrate each run and each step through their detail endpoints.
    pub fn mission_run_summaries(&self) -> Result<Vec<MissionRunView>> {
        self.mission_run_summaries_for_ids(None)
    }

    pub fn mission_run_summaries_for_missions(
        &self,
        ids: &[String],
    ) -> Result<Vec<MissionRunView>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.mission_run_summaries_for_ids(Some(ids))
    }

    pub fn mission_run_summaries_for_ids(&self, ids: Option<&[String]>) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        let placeholders = ids.map(|ids| vec!["?"; ids.len()].join(","));
        let sql = placeholders.as_ref().map(|placeholders| MISSION_RUN_HEADER_SELECT.replace(
            "ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
            &format!("WHERE mission_runs.mission_id IN ({placeholders}) ORDER BY mission_runs.created_at_unix_ms, mission_runs.id"),
        )).unwrap_or_else(|| MISSION_RUN_HEADER_SELECT.to_owned());
        let mut headers = connection
            .prepare(&sql)?
            .query_map(
                rusqlite::params_from_iter(
                    ids.into_iter()
                        .flatten()
                        .map(|id| id.trim_start_matches("mission/")),
                ),
                mission_run_header_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let by_generation = headers
            .iter()
            .enumerate()
            .map(|(index, run)| (run.generation.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let steps_sql = format!(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                    lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints,
                    generation_id
             FROM step_runs {} ORDER BY created_at_unix_ms, step_path",
            placeholders.as_ref().map(|p| format!("WHERE generation_id IN (SELECT current_generation_id FROM mission_runs WHERE mission_id IN ({p}))")).unwrap_or_default(),
        );
        let mut steps = connection.prepare(&steps_sql)?;
        for row in steps.query_map(
            rusqlite::params_from_iter(
                ids.into_iter()
                    .flatten()
                    .map(|id| id.trim_start_matches("mission/")),
            ),
            |row| {
                let step = step_run_from_row(row)?;
                let generation: String = row.get(21)?;
                Ok((generation, step))
            },
        )? {
            let (generation, step) = row?;
            if let Some(index) = by_generation.get(&format!("run-generation/{generation}")) {
                headers[*index].steps.push(step);
            }
        }
        Ok(headers)
    }

    /// Only fields consumed by agent cards, retaining the full reducer's membership,
    /// reachability and operational annotation. Never use this for a public status response.
    pub fn agent_card_status_at(
        &self,
        names: Option<&BTreeSet<String>>,
        index: u64,
        history: bool,
    ) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let index = selected_index(current_index(&connection)?, Some(index)).map_err(anyhow::Error::new)?;
        let names = match names {
            Some(names) => names.clone(),
            None => connection.prepare_cached(RANGE_SUBJECTS)?
                .query_map(params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?,
        };
        // The view and status passes share what they read of each subject.
        let mut reads = card_fold::CardReads::new(index);
        let names = if history { names } else {
            self.current_view_candidates_with(&connection, names, index, true, &mut reads)?
        };
        let newest = self.advance_subject_cache(&connection, index)?;
        let mode = ClaimsReduction::AgentCard;
        let mut kept = HashMap::new();
        let mut missing = Vec::new();
        for name in &names {
            match self.kept_subject_status(name, index, mode) {
                Some((status, _)) => {
                    kept.insert(name.clone(), status);
                }
                None => missing.push(name.clone()),
            }
        }
        for (name, (status, action)) in self.agent_card_statuses(&connection, &missing, index, &mut reads)? {
            if newest {
                self.keep_subject_status(&name, index, mode, &status, &action);
            }
            kept.insert(name, status);
        }
        let mut subjects = Vec::with_capacity(names.len());
        for name in names {
            let status = kept.remove(&name).expect("every name has a status");
            if history || status.projection.layer == "current" {
                subjects.push(status);
            }
        }
        Ok(StatusResponse { store_index: index, subjects, pending_actions: Vec::new() })
    }

    pub fn discard_desired_owned_by(&self, owner_run: &str) -> Result<usize> {
        let connection = self.connection.write();
        connection
            .execute("DELETE FROM desired WHERE owner_run=?1", [owner_run])
            .map_err(Into::into)
    }

    pub fn pending_human_reviews(&self, reviewer: Option<&str>) -> Result<Vec<HumanReviewView>> {
        let connection = self.readers.get();
        pending_human_reviews_tx(&connection, reviewer)
    }

    /// The reviewer's answer to the human gate request `request`, by the same rule that takes
    /// an answered request off `pending_human_reviews`.
    pub fn human_review_answer(&self, request: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        // An answer is written on the request's subject, its gate operation.
        smallclaims::touched::note_read(|| {
            connection
                .query_row("SELECT subject FROM claims WHERE id=?1", [request], |row| {
                    row.get(0)
                })
                .unwrap_or_default()
        });
        human_review_answer_tx(&connection, request)
    }

    /// Why `owner` has no pending human review: answered (how, by whom), stale (which of
    /// generation, revision, definition, attempt or state moved on), or never asked.
    pub fn human_review_refusal(&self, owner: &str) -> Result<String> {
        let connection = self.readers.get();
        human_review_refusal_tx(&connection, owner)
    }

    /// Request attention for a condition the daemon watches itself. The daemon closes it once
    /// that condition clears.
    /// Retained for historical import and audit fixtures. Current requests use `ask_person`.
    pub fn request_attention(
        &self,
        subject: &str,
        request: &AttentionRequest,
    ) -> Result<AttentionRequestView, St3Error> {
        self.request_attention_closing(
            subject,
            request,
            &AttentionClosing {
                closed_by: Some("st".into()),
                ..AttentionClosing::default()
            },
        )
    }

    /// Request attention that names what closes it: a target that can end, an `until` condition
    /// over its targets, a step whose end closes it, or a person. An agent's request that names
    /// none of these closes when the step that agent has claimed ends; a request with nothing
    /// that could close it is refused.
    pub fn request_attention_closing(
        &self,
        subject: &str,
        request: &AttentionRequest,
        closing: &AttentionClosing,
    ) -> Result<AttentionRequestView, St3Error> {
        let until = closing.until.as_deref();
        if let Some(closed_by) = closing.closed_by.as_deref()
            && !matches!(closed_by, "person" | "st")
        {
            return Err(St3Error::new(
                "invalid-attention-closed-by",
                format!("`{closed_by}` cannot close an attention request; use `person`"),
            ));
        }
        if let Some(until) = until {
            if !crate::model::STATUS_WAIT_CONDITIONS.contains(&until) {
                return Err(St3Error::new(
                    "invalid-attention-until",
                    format!(
                        "`{until}` is not a wait condition; use one of {}",
                        crate::model::STATUS_WAIT_CONDITIONS.join(", ")
                    ),
                ));
            }
            if request.targets.is_empty() {
                return Err(St3Error::new(
                    "invalid-attention-until",
                    "an until condition needs at least one target to watch",
                ));
            }
        }
        let reviewer = normalize_actor(&request.reviewer, "person");
        if !reviewer.starts_with("person/") {
            return Err(St3Error::new(
                "invalid-attention-reviewer",
                "an attention reviewer must be a person subject",
            ));
        }
        if request.title.trim().is_empty() || request.reason.trim().is_empty() {
            return Err(St3Error::new(
                "invalid-attention-request",
                "an attention request needs a title and a reason",
            ));
        }
        if !matches!(request.severity.as_str(), "warning" | "error") {
            return Err(St3Error::new(
                "invalid-attention-severity",
                "attention severity must be warning or error",
            ));
        }
        for target in &request.targets {
            st3_schema::registry()
                .validate_subject(target)
                .map_err(|error| St3Error::new(error.code, error.message))?;
        }
        let actor = normalize_actor(&request.actor, "agent");
        let step = attention_closing_step(&self.readers.get(), &actor, request, closing)?;
        let mut fields = BTreeMap::from([
            ("reviewer".into(), Value::String(reviewer)),
            ("title".into(), Value::String(request.title.clone())),
            ("reason".into(), Value::String(request.reason.clone())),
            ("severity".into(), Value::String(request.severity.clone())),
            (
                "targets".into(),
                Value::Array(request.targets.iter().cloned().map(Value::String).collect()),
            ),
        ]);
        if let Some(until) = until {
            fields.insert("until".into(), Value::String(until.into()));
        }
        if let Some((step, attempt)) = step {
            fields.insert("step".into(), Value::String(step));
            fields.insert("step_attempt".into(), Value::from(attempt));
        }
        if let Some(closed_by) = &closing.closed_by {
            fields.insert("closed_by".into(), Value::String(closed_by.clone()));
        }
        self.append_claim(&ClaimInput {
            subject: subject.to_owned(),
            kind: "attention.requested".into(),
            actor: Some(actor),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key.clone()),
        })?;
        self.attention_request(subject)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the attention request was not stored"))
    }

    /// Historical audit closure; current attention has no independent resolution.
    pub fn resolve_attention(
        &self,
        subject: &str,
        request: &AttentionResolveRequest,
    ) -> Result<AttentionRequestView, St3Error> {
        if !matches!(request.outcome.as_str(), "resolved" | "dismissed") {
            return Err(St3Error::new(
                "invalid-attention-outcome",
                "an attention outcome must be resolved or dismissed",
            ));
        }
        let subject = if subject.starts_with("attention/") {
            subject.to_owned()
        } else {
            format!("attention/{subject}")
        };
        // Any person or agent can close any item, whoever it was routed to, so an item never
        // outlives everyone who could close it. The resolution records who closed it.
        let actor = normalize_actor(&request.actor, "person");
        if !(actor.starts_with("person/") || actor.starts_with("agent/")) {
            return Err(St3Error::new(
                "invalid-attention-resolver",
                format!("`{actor}` is neither a person nor an agent"),
            ));
        }
        let current = self
            .attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-attention-request",
                    format!("attention request `{subject}` does not exist"),
                )
            })?;
        self.append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "attention.resolved".into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("request".into(), Value::String(current.request)),
                ("outcome".into(), Value::String(request.outcome.clone())),
                (
                    "reason".into(),
                    request
                        .reason
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key.clone()),
        })?;
        self.attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the attention resolution was not stored"))
    }

    pub fn withdraw_attention(
        &self,
        subject: &str,
        request: &AttentionWithdrawRequest,
    ) -> Result<AttentionRequestView, St3Error> {
        if request.reason.trim().is_empty() {
            return Err(St3Error::new(
                "invalid-attention-withdrawal",
                "an attention withdrawal needs a reason",
            ));
        }
        let subject = if subject.starts_with("attention/") {
            subject.to_owned()
        } else {
            format!("attention/{subject}")
        };
        let current = self
            .attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-attention-request",
                    format!("attention request `{subject}` does not exist"),
                )
            })?;
        let actor = normalize_actor(&request.actor, "agent");
        if actor != current.actor {
            return Err(St3Error::new(
                "wrong-attention-requester",
                format!(
                    "attention request `{subject}` belongs to `{}`",
                    current.actor
                ),
            ));
        }
        self.append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "attention.resolved".into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("request".into(), Value::String(current.request)),
                ("outcome".into(), Value::String("withdrawn".into())),
                ("reason".into(), Value::String(request.reason.clone())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key.clone()),
        })?;
        self.attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the attention withdrawal was not stored"))
    }

    pub fn resolve_attention_automatically(
        &self,
        subject: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<AttentionRequestView, St3Error> {
        let subject = if subject.starts_with("attention/") {
            subject.to_owned()
        } else {
            format!("attention/{subject}")
        };
        let current = self
            .attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-attention-request",
                    format!("attention request `{subject}` does not exist"),
                )
            })?;
        self.append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "attention.resolved".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("request".into(), Value::String(current.request)),
                ("outcome".into(), Value::String("resolved".into())),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(idempotency_key.into()),
        })?;
        self.attention_request(&subject)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the attention resolution was not stored"))
    }

    pub fn attention_request(&self, subject: &str) -> Result<Option<AttentionRequestView>> {
        let subject = if subject.starts_with("attention/") {
            subject.to_owned()
        } else {
            format!("attention/{subject}")
        };
        let connection = self.readers.get();
        attention_request_view_tx(&connection, &subject)
    }

    /// The current state of each fault target that has one. Resources, documents and unknown
    /// kinds have no state to show and are left out.
    pub fn attention_target_states(
        &self,
        targets: &[String],
    ) -> Result<Vec<crate::model::AttentionTargetState>> {
        let connection = self.readers.get();
        let mut states = Vec::new();
        for target in targets {
            if let Some((state, since_unix_ms)) = attention_target_state_tx(&connection, target)? {
                states.push(crate::model::AttentionTargetState {
                    id: target.clone(),
                    state,
                    since_unix_ms,
                });
            }
        }
        Ok(states)
    }

    pub fn attention_requests(
        &self,
        person: Option<&str>,
        include_resolved: bool,
    ) -> Result<Vec<AttentionRequestView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT DISTINCT subject FROM claims
             WHERE kind='attention.requested'
               AND (?1 IS NULL OR json_extract(body, '$.fields.reviewer')=?1)
             ORDER BY CANONICAL_ASC(claims)",
        ))?;
        let subjects = statement
            .query_map([person], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let requests = subjects
            .into_iter()
            .filter_map(|subject| attention_request_view_tx(&connection, &subject).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(requests
            .into_iter()
            .filter(|request| include_resolved || request.status == "pending")
            .collect())
    }

    /// Pending requests that `actor` raised from `origin`, so that host can close its own alerts.
    pub fn pending_attention_requests_raised_by(
        &self,
        actor: &str,
        origin: &str,
    ) -> Result<Vec<AttentionRequestView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT request.subject FROM claims request
             WHERE request.kind='attention.requested'
               AND request.actor=?1
               AND request.origin=?2
               AND NOT EXISTS (
                 SELECT 1 FROM claims resolution
                 WHERE resolution.subject=request.subject
                   AND resolution.kind='attention.resolved'
               )
             ORDER BY CANONICAL_ASC(request)",
        ))?;
        let subjects = statement
            .query_map([actor, origin], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        smallclaims::touched::note_read(|| "kind:attention.requested".to_owned());
        for subject in &subjects {
            smallclaims::touched::note_read(|| subject.clone());
        }
        subjects
            .iter()
            .filter_map(|subject| attention_request_view_tx(&connection, subject).transpose())
            .collect()
    }

    /// Current runs waiting on person steps, human gates or approvals.
    pub fn human_attention_runs(&self) -> Result<BTreeSet<String>> {
        self.human_attention_runs_at(now_ms())
    }

    pub fn human_attention_runs_at(&self, now: u128) -> Result<BTreeSet<String>> {
        Ok(self.human_attention_at(now)?.0)
    }

    /// The runs that wait on a person, with the open human gates among them: each gate's
    /// mission, the step it holds when it holds one, and the person it waits on.
    pub fn human_attention(&self) -> Result<HumanAttention> {
        self.human_attention_at(now_ms())
    }

    /// [`Store::human_attention`] with person asks evaluated at `now`.
    pub fn human_attention_at(&self, now: u128) -> Result<HumanAttention> {
        let mut items = self.mission_run_attention_items(None)?;
        let gates = items
            .iter()
            .filter(|item| item.kind == "human-gate")
            .filter_map(|item| {
                let mission = item.mission.as_deref()?;
                Some(st3_ui_model::missions::Gate {
                    mission: if mission.starts_with("mission/") {
                        mission.to_owned()
                    } else {
                        format!("mission/{mission}")
                    },
                    step: item.step.clone(),
                    person: item.person.clone(),
                })
            })
            .collect();
        items.extend(self.person_attention_items(None, now)?);
        Ok((items.into_iter().filter_map(|item| item.mission_run).collect(), gates))
    }

    /// The attention items that belong to a mission run: gate reviews, launch approvals and
    /// revision approvals. A run's header is all they read of the run.
    pub fn mission_run_attention_items(&self, person: Option<&str>) -> Result<Vec<AttentionItemView>> {
        let mut items = Vec::new();
        let reviews = self.pending_human_reviews(person)?;
        {
            let connection = self.readers.get();
            for review in reviews {
                // The gate's owner is the step whose work it reviews, a loop whose step runs
                // it, or its run.
                let step = if review.owner.starts_with("step-run/") {
                    Some(review.owner.clone())
                } else if review.owner.starts_with("loop-run/") {
                    loop_step_tx(&connection, &review.owner)?.map(|step| step.subject)
                } else {
                    None
                };
                let conversation = attention_snapshot::conversation_agent(
                    &connection,
                    step.as_deref(),
                    Some(&review.mission_run),
                )?;
                items.push(AttentionItemView {
                    conversation,
                    ..attention_item_from_review(review)
                });
            }
        }

        {
            let connection = self.readers.get();
            let mut statement = connection.prepare(
                "SELECT id FROM planning_sessions
                 WHERE status='review' AND (?1 IS NULL OR requester=?1)
                 ORDER BY created_at_unix_ms, id",
            )?;
            let ids = statement
                .query_map([person], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                let Some(session) = planning_session_view_tx(&connection, &id)? else {
                    continue;
                };
                let (Some(candidate), Some(preview)) = (&session.candidate, &session.preview)
                else {
                    continue;
                };
                if preview.candidate_revision != candidate.revision
                    || !preview.mission.blockers.is_empty()
                {
                    continue;
                }
                if let Some(run) = &session.target_mission_run {
                    let Some(run) = mission_run_header_tx(
                        &connection,
                        run.strip_prefix("mission-run/").unwrap_or(run),
                    )
                    .optional()?
                    else {
                        continue;
                    };
                    if session.source_generation.as_deref() != Some(run.generation.as_str())
                        || is_terminal_run_state(&run.status)
                    {
                        continue;
                    }
                }
                items.push(attention_item_from_planning(&session, candidate, preview));
            }
        }

        {
            let connection = self.readers.get();
            let mut statement = connection.prepare(
                "SELECT id FROM revision_proposals
                 WHERE status='pending-approval'
                 ORDER BY created_at_unix_ms, id",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                let proposal = revision_proposal_view_tx(&connection, &id)?;
                let run = mission_run_header_tx(
                    &connection,
                    proposal
                        .run
                        .strip_prefix("mission-run/")
                        .unwrap_or(&proposal.run),
                )
                .optional()?;
                let Some(run) = run else {
                    continue;
                };
                if run.generation != proposal.source_generation
                    || is_terminal_run_state(&run.status)
                {
                    continue;
                }
                for reviewer in proposal
                    .reviewers
                    .iter()
                    .filter(|reviewer| !proposal.approvals.contains(*reviewer))
                    .filter(|reviewer| person.is_none_or(|person| person == reviewer.as_str()))
                {
                    items.push(attention_item_from_revision(&proposal, &run, reviewer));
                }
            }
        }
        Ok(items)
    }

    pub fn attention_items(&self, person: Option<&str>) -> Result<Vec<AttentionItemView>> {
        self.attention_snapshot(person, now_ms())
    }

    /// What waits on `person` and no agent can resolve: requests (person steps and agents'
    /// asks) and reviews (human gates, launch and revision approvals). Messages stay in
    /// conversations, and a fault goes to the agent that owns it, see `fault_snapshot`.
    pub fn attention_snapshot(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let mut items = self.mission_run_attention_items(person)?;
        items.extend(self.person_attention_items(person, as_of)?);
        items.extend(self.harness_login_attention_items(person)?);
        items.extend(self.harness_prompt_attention_items(person, as_of)?);
        items.extend(self.custom_attention_items(person)?);
        // A person who published a broken gate is the one to correct it.
        items.extend(
            self.broken_gate_items(person, as_of)?
                .into_iter()
                .filter(|item| item.person.starts_with("person/")),
        );
        self.current_attention(items, as_of)
    }

    /// Keep the items requested by `as_of` whose run is still current, then order them.
    pub fn current_attention(
        &self,
        mut items: Vec<AttentionItemView>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        items.retain(|item| item.requested_at_unix_ms <= as_of);
        let mut live = Vec::new();
        for mut item in items {
            if let Some(run) = item.mission_run.as_deref() {
                if !person_work::run_live(&connection, run, None, item.kind == "fault")? {
                    continue;
                }
            }
            if item.episode.is_empty() {
                item.episode = connection
                    .query_row(
                        &canonical_sql(
                            "SELECT id FROM claims WHERE subject=?1
                    ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                        ),
                        [&item.subject],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| item.subject.clone());
            }
            live.push(item);
        }
        let priority = |item: &AttentionItemView| match item.priority.as_str() {
            "critical" => 0,
            "high" => 1,
            "normal" => 2,
            _ => 3,
        };
        // Most urgent first, and within that the newest first (Nathan, 2026-10-04).
        live.sort_by(|left, right| {
            priority(left)
                .cmp(&priority(right))
                .then_with(|| right.requested_at_unix_ms.cmp(&left.requested_at_unix_ms))
                .then_with(|| left.subject.cmp(&right.subject))
                .then_with(|| left.person.cmp(&right.person))
                .then_with(|| left.episode.cmp(&right.episode))
        });
        let items = live;
        Ok(items)
    }

    /// Aggregate durable provider usage without mixing context-window occupancy
    /// with spend. Per incarnation, a cumulative total wins over response
    /// deltas; otherwise the non-overlapping response deltas are summed.
    pub fn usage_summary_at(
        &self,
        subject: &str,
        incarnation: Option<&str>,
        at_index: Option<u64>,
    ) -> Result<Option<UsageSummary>> {
        let connection = self.readers.get();
        let at_index = at_index.unwrap_or(i64::MAX as u64);
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT store_index, body, accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='harness.usage' AND store_index<=?2
             ORDER BY CANONICAL_ASC(claims)",
        ))?;
        let rows = statement.query_map(params![subject, at_index], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        Self::usage_summary_from_rows(rows, incarnation)
    }

    /// Fetch usage for the selected subjects, preserving the detail reduction and one
    /// SQLite cut across timestamp streaming and canonical tie resolution.
    pub fn usage_summaries_at(
        &self,
        subjects: &[String],
        at_index: Option<u64>,
    ) -> Result<BTreeMap<String, UsageSummary>> {
        self.read_snapshot(|snapshot| self.usage_summaries_pinned(subjects, at_index, snapshot))
    }

    pub fn usage_summaries_pinned(
        &self,
        subjects: &[String],
        at_index: Option<u64>,
        snapshot: u64,
    ) -> Result<BTreeMap<String, UsageSummary>> {
        let through = at_index.map_or(snapshot, |at| at.min(snapshot));
        let connection = self.readers.get();
        let unique_subjects = subjects
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let kept = {
            let folds = self.smalltalk.usage_folds.lock().expect("usage folds poisoned");
            unique_subjects.iter()
                .filter_map(|subject| folds.get(subject.as_str())
                    .filter(|cached| cached.through <= through)
                    .map(|cached| (subject.as_str(), Arc::clone(cached))))
                .collect::<HashMap<_, _>>()
        };
        let mut folded = BTreeMap::<String, CachedUsageFold>::new();
        let mut unfolded = Vec::new();
        for subject in unique_subjects {
            let resumed = match kept.get(subject.as_str()) {
                Some(cached) => Self::usage_fold_after(&connection, subject, cached, through)?,
                None => None,
            };
            match resumed {
                Some(resumed) => { folded.insert(subject.clone(), resumed); }
                None => unfolded.push(subject),
            }
        }
        let mut grouped = BTreeMap::<String, Vec<rusqlite::Result<(u64, String, String)>>>::new();
        for chunk in unfolded.chunks(500) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; chunk.len()].join(",");
            // The accepted-time index supplies the first two canonical components without
            // a wide-body temporary sorter. Resolve the remaining components only inside
            // equal-timestamp groups, where they can actually decide the reduction order.
            let sql = format!(
                "SELECT subject, id, store_index, body, accepted_at_unix_ms
                 FROM claims INDEXED BY claims_subject_kind_accepted_index
                 WHERE kind='harness.usage' AND store_index<={} AND subject IN ({placeholders})
                 ORDER BY subject, length(accepted_at_unix_ms), accepted_at_unix_ms",
                at_index.unwrap_or(i64::MAX as u64)
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut ties = Vec::<(String, String, u64, String, String)>::new();
            let mut flush = |ties: &mut Vec<(String, String, u64, String, String)>| -> Result<()> {
                if ties.len() > 1 {
                    let mut keyed = ties
                        .drain(..)
                        .map(|row| Ok((canonical::claim_key(&connection, &row.1)?, row)))
                        .collect::<Result<Vec<_>>>()?;
                    keyed.sort_by(|a, b| a.0.cmp(&b.0));
                    ties.extend(keyed.into_iter().map(|(_, row)| row));
                }
                for (subject, _, index, body, accepted) in ties.drain(..) {
                    grouped
                        .entry(subject)
                        .or_default()
                        .push(Ok((index, body, accepted)));
                }
                Ok(())
            };
            for row in rows {
                let row = row?;
                if ties
                    .last()
                    .is_some_and(|previous| previous.0 != row.0 || previous.4 != row.4)
                {
                    flush(&mut ties)?;
                }
                ties.push(row);
            }
            flush(&mut ties)?;
        }
        for subject in unfolded {
            let mut fold = UsageFold::default();
            let mut last_accepted = String::new();
            for row in grouped.remove(subject.as_str()).unwrap_or_default() {
                let (store_index, body, accepted) = row?;
                fold.apply(store_index, &body, &accepted, None)?;
                last_accepted = accepted;
            }
            folded.insert(subject.clone(), CachedUsageFold { through, last_accepted, fold });
        }
        let summaries = folded.iter()
            .filter_map(|(subject, cached)| cached.fold.summary().map(|summary| (subject.clone(), summary)))
            .collect();
        let mut folds = self.smalltalk.usage_folds.lock().expect("usage folds poisoned");
        for (subject, cached) in folded {
            folds.keep(subject, cached);
        }
        Ok(summaries)
    }

    pub fn usage_summary_from_rows<I>(
        rows: I,
        incarnation: Option<&str>,
    ) -> Result<Option<UsageSummary>>
    where
        I: IntoIterator<Item = rusqlite::Result<(u64, String, String)>>,
    {
        let mut fold = UsageFold::default();
        for row in rows {
            let (store_index, body, accepted_at) = row?;
            fold.apply(store_index, &body, &accepted_at, incarnation)?;
        }
        Ok(fold.summary())
    }

    /// A login is work only the seat's person can do. Project it from the current canonical
    /// condition, rather than raising an agent-owned fault or a legacy attention request.
    /// One alert per login that needs a person: seats that share a login directory on a host
    /// fail together and one sign-in fixes them all, so they share an alert that names each seat
    /// and shows in each one's conversation. It clears when the login works again.
    pub fn harness_login_attention_items(
        &self,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        struct Seat {
            subject: String,
            driver: String,
            episode: String,
            requested_at: u128,
        }
        let mut logins: BTreeMap<(String, String, String), Vec<Seat>> = BTreeMap::new();
        for desired in self.desired_harness_login_candidates()? {
            if !person_work::declaration_live(&self.readers.get(), &desired.subject)? {
                continue;
            }
            let Some(harness) = self.current_harness_for_login(&desired.subject)? else {
                continue;
            };
            if harness.state != "needs-login" {
                continue;
            }
            let Some(owner) = self.agent_person(&desired.subject)? else {
                continue;
            };
            if person.is_some_and(|person| person != owner) {
                continue;
            }
            let driver = desired
                .member
                .as_ref()
                .and_then(|m| m.driver.as_deref())
                .unwrap_or("harness")
                .to_owned();
            let host = desired
                .member
                .as_ref()
                .map(|m| m.host.clone())
                .unwrap_or_else(|| "unknown".into());
            let login = self.seat_login(&desired, &host, &driver)?;
            let (fence, episode) =
                self.harness_login_episode_key(&desired.subject, &harness.incarnation_id)?;
            let requested_at = fence.as_ref().map_or(harness.observed_at_unix_ms, |claim| {
                claim.accepted_at_unix_ms
            });
            logins.entry((owner, host, login)).or_default().push(Seat {
                subject: desired.subject,
                driver,
                episode,
                requested_at,
            });
        }
        let mut items = Vec::new();
        for ((owner, host, login), mut seats) in logins {
            // The seat that failed first stands for the login: its terminal is where to sign in,
            // and its episode is the alert's. If it recovers before the others, the next seat
            // stands for the login and the alert's ID changes with it: a login episode of its
            // own would have to be written, and a read never writes.
            seats.sort_by(|left, right| {
                (left.requested_at, &left.subject).cmp(&(right.requested_at, &right.subject))
            });
            let first = &seats[0];
            let how = match first.driver.as_str() {
                "claude" | "pi" | "omp" => "run /login",
                "codex" => {
                    "complete the sign-in prompt or run codex login in a shell using this seat's account configuration"
                }
                "opencode" => {
                    "run opencode auth login in a shell using this seat's account configuration"
                }
                _ => "use this harness's login command",
            };
            let names = seats
                .iter()
                .map(|seat| seat.subject.as_str())
                .collect::<Vec<_>>();
            let (title, detail) = if let [seat] = names.as_slice() {
                (
                    format!("{seat} on {host} needs you to log in"),
                    format!("{seat} on {host} needs you to log in: attach (Ctrl+] in stui) and {how}. A successful authenticated turn clears this alert automatically."),
                )
            } else {
                (
                    format!("{} seats on {host} need you to log in to {login}", names.len()),
                    format!("{} on {host} share the login {login}, which needs you to sign in again: attach to {} (Ctrl+] in stui) and {how}. One sign-in fixes every seat; each clears when its next authenticated turn succeeds.", names.join(", "), first.subject),
                )
            };
            items.push(AttentionItemView {
                episode: first.episode.clone(), priority: "high".into(), kind: "harness-login".into(), review_mode: None,
                subject: first.subject.clone(), person: owner, requester_id: None, launch_id: None,
                variant_id: None, message_id: None, prompt_call: None, answers: Vec::new(), conversation: Some(first.subject.clone()),
                title, detail,
                request: None, mission: None, mission_run: None, step: None,
                targets: names.iter().map(|name| (*name).to_owned()).collect(),
                requested_at_unix_ms: first.requested_at,
                actions: vec![crate::model::AttentionActionView { label: "Attach to log in".into(),
                    argv: vec!["st".into(), "terminals".into(), "attach".into(), first.subject.clone()] }],
            });
        }
        Ok(items)
    }
}

/// Why `owner` has no human review waiting, in words for the person who tried to answer it:
/// its newest request was answered (how, and by whom), no longer matches the owner, or st never
/// asked one.
fn human_review_refusal_tx(connection: &Connection, owner: &str) -> Result<String> {
    let request = connection
        .query_row(
            &canonical_sql(
                "SELECT request.id, request.store_index, request.batch_id, request.subject,
                        request.kind, request.origin, request.actor, request.body,
                        request.predecessors, request.accepted_at_unix_ms
                 FROM claims request
                 WHERE request.kind='gate.requested'
                   AND json_extract(request.body, '$.fields.owner')=?1
                   AND json_extract(request.body, '$.fields.reviewer') IS NOT NULL
                 ORDER BY CANONICAL_DESC(request) LIMIT 1",
            ),
            [owner],
            claim_from_row,
        )
        .optional()?;
    let Some(request) = request else {
        return Ok("st has asked no person to review it".into());
    };
    if let Some(answer) = human_review_answer_tx(connection, &request.id)? {
        let fields = answer.body.get("fields").unwrap_or(&answer.body);
        let decision = fields.get("decision").and_then(Value::as_str).unwrap_or(
            match fields.get("verdict").and_then(Value::as_str) {
                Some("pass") => "approved",
                Some("feedback") => "changes-requested",
                _ => "rejected",
            },
        );
        return Ok(format!(
            "it was already answered: {decision} by {}",
            answer.actor.as_deref().unwrap_or("its reviewer")
        ));
    }
    Ok(match human_review_currency(connection, request)? {
        Err(why) => format!("its review is no longer current: {why}"),
        Ok(review) => format!("its review waits on {}", review.reviewer),
    })
}

/// The step, with its current attempt, whose end closes a new attention request: the step the
/// request names, or, when an agent's request names nothing else that could close it, the step
/// that agent has claimed. A request that nothing could close is refused.
fn attention_closing_step(
    connection: &Connection,
    actor: &str,
    request: &AttentionRequest,
    closing: &AttentionClosing,
) -> Result<Option<(String, u64)>, St3Error> {
    if let Some(step) = &closing.step {
        let row = connection
            .query_row(
                "SELECT status, attempt FROM step_runs WHERE subject=?1",
                [step],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((status, attempt)) = row else {
            return Err(St3Error::new(
                "invalid-attention-step",
                format!("`{step}` is not a step run on this host"),
            ));
        };
        if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
            return Err(St3Error::new(
                "attention-step-ended",
                format!("`{step}` already {status}, so its end cannot close a new request"),
            ));
        }
        return Ok(Some((step.clone(), attempt)));
    }
    if closing.until.is_some() || closing.closed_by.is_some() {
        return Ok(None);
    }
    for target in &request.targets {
        if attention_target_can_end(connection, target).map_err(internal)? {
            return Ok(None);
        }
    }
    let claimed = if actor.starts_with("agent/") {
        connection
            .query_row(
                "SELECT subject, attempt FROM step_runs
                 WHERE lease_owner=?1
                   AND status IN ('claimed','working')
                   AND CAST(lease_expires_at_unix_ms AS INTEGER)>?2
                 ORDER BY CAST(updated_at_unix_ms AS INTEGER) DESC, subject
                 LIMIT 1",
                params![actor, now_ms() as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()
            .map_err(internal)?
    } else {
        None
    };
    claimed.map(Some).ok_or_else(|| {
        let who = if actor.starts_with("agent/") {
            format!("`{actor}` has no claimed step whose end could close it, and it names")
        } else {
            "it names".into()
        };
        St3Error::new(
            "attention-closes-never",
            format!(
                "attention request `{}` would stay open forever: {who} no target that can end (a step, run generation, mission, run, agent, observer, subscription, loop run, message, attention item or observed pull request), no --until condition and no --step; pass --person-closes when only a person can close it",
                request.title
            ),
        )
    })
}

fn attention_target_state_tx(connection: &Connection, target: &str) -> Result<Option<TargetState>> {
    let row = |sql: &str, id: &str| -> Result<Option<TargetState>> {
        connection
            .query_row(sql, [id], |row| {
                let since = row.get::<_, String>(1)?;
                Ok((row.get::<_, String>(0)?, since.parse().ok()))
            })
            .optional()
            .map_err(Into::into)
    };
    if let Some(mission) = target.strip_prefix("mission/") {
        // The same summary `missions ls` shows: an active run wins, then the latest run.
        let mut statement = connection.prepare(
            "SELECT status, updated_at_unix_ms FROM mission_runs WHERE mission_id=?1
             ORDER BY CAST(created_at_unix_ms AS INTEGER) DESC, id DESC",
        )?;
        let runs = statement
            .query_map([mission], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let definition = row(
            "SELECT mission_definitions.state, claims.accepted_at_unix_ms
             FROM mission_definitions JOIN claims ON claims.id=mission_definitions.claim_id
             WHERE mission_definitions.mission_id=?1",
            mission,
        )?;
        if let Some((state, since)) = &definition
            && state == "retired"
        {
            return Ok(Some((state.clone(), *since)));
        }
        let run = ["running", "standing"]
            .iter()
            .find_map(|active| runs.iter().find(|(status, _)| status == active))
            .or(runs.first());
        return Ok(match run {
            Some((status, since)) => Some((status.clone(), since.parse().ok())),
            None => definition.map(|(state, _)| (state, None)),
        });
    }
    if let Some(run) = target.strip_prefix("mission-run/") {
        return row(
            "SELECT status, updated_at_unix_ms FROM mission_runs WHERE id=?1",
            run,
        );
    }
    if let Some(generation) = target.strip_prefix("run-generation/") {
        return row(
            "SELECT status, updated_at_unix_ms FROM run_generations WHERE id=?1",
            generation,
        );
    }
    if target.starts_with("step-run/") {
        return row(
            "SELECT status, updated_at_unix_ms FROM step_runs WHERE subject=?1",
            target,
        );
    }
    if target.starts_with("attention/") {
        if let Some(request) = attention_request_view_tx(connection, target)? {
            return Ok(Some(match request.resolved_at_unix_ms {
                Some(resolved_at) => (request.status, Some(resolved_at)),
                None => ("open".into(), Some(request.requested_at_unix_ms)),
            }));
        }
        // A client attention ID names a human gate by a digest of its owner, as
        // `attention_resource_id` in the API does.
        let pending = pending_human_reviews_tx(connection, None)?
            .into_iter()
            .find(|review| {
                let digest = hex::encode(Sha256::digest(review.owner.as_bytes()));
                format!("attention/{}", &digest[..24]) == target
            });
        return Ok(Some(match pending {
            Some(review) => ("open".into(), Some(review.requested_at_unix_ms)),
            None => ("not pending".into(), None),
        }));
    }
    if target.starts_with("agent/") {
        let stopped = row(
            "SELECT desired.kind, claims.accepted_at_unix_ms
             FROM desired JOIN claims ON claims.id=desired.claim_id
             WHERE desired.subject=?1 AND desired.kind='stop'",
            target,
        )?;
        if let Some((_, since)) = stopped {
            return Ok(Some(("stopped".into(), since)));
        }
        if let Some(harness) = current_harness_at(connection, target, None)? {
            return Ok(Some((harness.state, Some(harness.observed_at_unix_ms))));
        }
        return row(
            &canonical_sql("SELECT coalesce(json_extract(body, '$.fields.status'), 'unknown'), accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind='runtime.observed'
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            target,
        );
    }
    if target.starts_with("observer/") || target.starts_with("subscription/") {
        if let Some((stopped_at, ..)) = declaration_stopped_tx(connection, target)? {
            return Ok(Some(("stopped".into(), Some(stopped_at))));
        }
        return row(
            &canonical_sql("SELECT coalesce(json_extract(body, '$.fields.state'), 'unknown'), accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind IN ('observer.state', 'subscription.state')
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            target,
        );
    }
    if target.starts_with("loop-run/") {
        return row(
            &canonical_sql("SELECT coalesce(json_extract(body, '$.fields.status'), 'unknown'), accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind='loop.state'
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            target,
        );
    }
    if target.starts_with("message/") {
        return row(
            &canonical_sql("SELECT substr(kind, 9), accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind IN
               ('message.sent', 'message.staged', 'message.delivered', 'message.read', 'message.closed')
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            target,
        );
    }
    Ok(pull_request_state_tx(connection, target)?.map(|(state, since)| (state, Some(since))))
}

fn attention_item_from_review(review: HumanReviewView) -> AttentionItemView {
    AttentionItemView {
        episode: review.request.clone(),
        priority: "normal".into(),
        kind: "human-gate".into(),
        review_mode: Some(review.mode.clone()),
        subject: review.owner.clone(),
        person: review.reviewer.clone(),
        requester_id: None,
        launch_id: None,
        variant_id: None,
        message_id: None, prompt_call: None, answers: Vec::new(), conversation: None,
        title: review
            .title
            .clone()
            .or_else(|| review.step.clone())
            .unwrap_or_else(|| "Human review".into()),
        detail: review.question,
        mission: Some(review.mission),
        mission_run: Some(review.mission_run),
        step: review.step,
        targets: review.review_targets,
        requested_at_unix_ms: review.requested_at_unix_ms,
        actions: vec![
            attention_action(
                "approve",
                &[
                    "st",
                    "alerts",
                    "approve",
                    &review.owner,
                    "--as",
                    &review.reviewer,
                ],
            ),
            attention_action(
                if review.mode == "feedback" {
                    "request changes"
                } else {
                    "reject"
                },
                &[
                    "st",
                    "alerts",
                    if review.mode == "feedback" {
                        "request-changes"
                    } else {
                        "reject"
                    },
                    &review.owner,
                    "--as",
                    &review.reviewer,
                ],
            ),
        ],
        request: None,
    }
}

fn attention_item_from_planning(
    session: &PlanningSessionView,
    candidate: &PlanningCandidateView,
    preview: &PlanningPreviewView,
) -> AttentionItemView {
    AttentionItemView {
        episode: preview.hash.clone(),
        priority: "normal".into(),
        kind: "launch-approval".into(),
        review_mode: None,
        subject: session.subject.clone(),
        person: session.requester.clone(),
        requester_id: Some(session.requester.clone()),
        launch_id: Some(format!("launch/{}", session.id)),
        variant_id: Some(format!(
            "launch-variant/{}/{}",
            session.id, candidate.variant
        )),
        message_id: None, prompt_call: None, answers: Vec::new(),
        conversation: attention_snapshot::agent(&session.planner),
        title: format!("Approve mission/{}", session.mission),
        detail: "The current launch preview is ready for approval.".into(),
        mission: Some(format!("mission/{}", session.mission)),
        mission_run: session.target_mission_run.clone(),
        step: None,
        targets: vec![
            session.request.clone(),
            candidate.markdown.clone(),
            candidate.kdl.clone(),
        ],
        requested_at_unix_ms: preview.created_at_unix_ms,
        actions: vec![
            attention_action("show", &["st", "launch", "show", &session.id]),
            attention_action(
                "approve",
                &[
                    "st",
                    "launch",
                    "approve",
                    &session.id,
                    &preview.hash,
                    "--as",
                    &session.requester,
                ],
            ),
            attention_action(
                "cancel",
                &[
                    "st",
                    "launch",
                    "cancel",
                    &session.id,
                    "--as",
                    &session.requester,
                ],
            ),
        ],
        request: None,
    }
}

fn attention_item_from_revision(
    proposal: &RevisionProposalView,
    run: &MissionRunView,
    reviewer: &str,
) -> AttentionItemView {
    let preview_hash = proposal.preview_hash.as_deref().unwrap_or_default();
    AttentionItemView {
        episode: format!("{}:{}", proposal.source_generation, preview_hash),
        priority: "normal".into(),
        kind: "revision-approval".into(),
        review_mode: None,
        subject: proposal.subject.clone(),
        person: reviewer.to_owned(),
        requester_id: None,
        launch_id: None,
        variant_id: None,
        message_id: None, prompt_call: None, answers: Vec::new(),
        conversation: attention_snapshot::agent(&proposal.actor),
        title: format!("Approve a revision of {}", run.mission),
        detail: proposal.reason.clone(),
        mission: Some(run.mission.clone()),
        mission_run: Some(run.subject.clone()),
        step: None,
        targets: vec![format!("{}@{}", run.mission, proposal.candidate_revision)],
        requested_at_unix_ms: proposal.created_at_unix_ms,
        actions: vec![
            attention_action("show", &["st", "work", "revision", "show", &run.subject]),
            attention_action(
                "approve",
                &[
                    "st",
                    "work",
                    "revision",
                    "approve",
                    &proposal.subject,
                    preview_hash,
                    "--as",
                    reviewer,
                ],
            ),
            attention_action(
                "cancel",
                &[
                    "st",
                    "work",
                    "revision",
                    "cancel",
                    &proposal.subject,
                    "--as",
                    reviewer,
                ],
            ),
        ],
        request: None,
    }
}

fn pending_human_reviews_tx(
    connection: &Connection,
    reviewer: Option<&str>,
) -> Result<Vec<HumanReviewView>> {
    let requests = {
        let mut statement = connection.prepare(&canonical_sql(&format!(
            "SELECT request.id, request.store_index, request.batch_id, request.subject,
                    request.kind, request.origin, request.actor, request.body,
                    request.predecessors, request.accepted_at_unix_ms
             FROM claims request INDEXED BY claims_human_gate_request_index
             WHERE request.kind='gate.requested'
               AND json_extract(request.body, '$.fields.reviewer') IS NOT NULL
               AND (?1 IS NULL OR json_extract(request.body, '$.fields.reviewer')=?1)
               AND NOT EXISTS (SELECT 1 FROM claims result WHERE {})
             ORDER BY CANONICAL_ASC(request)",
            reviewer_answer_sql("result")
        )))?;
        statement
            .query_map([reviewer], claim_from_row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    // A build that words a gate's request differently asks the same gate again. The reviewer
    // answers the newest request, which is the one the gate waits on, and has waited since the
    // first.
    let mut reviews: Vec<HumanReviewView> = Vec::new();
    for request in requests {
        let Some(review) = current_human_review(connection, request)? else {
            continue;
        };
        match reviews.iter_mut().find(|kept| {
            kept.owner == review.owner
                && kept.reviewer == review.reviewer
                && kept.attempt == review.attempt
        }) {
            Some(kept) => {
                let first = kept.requested_at_unix_ms.min(review.requested_at_unix_ms);
                if review.requested_at_unix_ms >= kept.requested_at_unix_ms {
                    *kept = review;
                }
                kept.requested_at_unix_ms = first;
            }
            None => reviews.push(review),
        }
    }
    Ok(reviews)
}

fn attention_request_view_tx(
    connection: &Connection,
    subject: &str,
) -> Result<Option<AttentionRequestView>> {
    let requested = connection
        .query_row(
            &canonical_sql(
                "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                    predecessors, accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind='attention.requested'
             ORDER BY CANONICAL_ASC(claims) LIMIT 1",
            ),
            [subject],
            claim_from_row,
        )
        .optional()?;
    let Some(requested) = requested else {
        return Ok(None);
    };
    let resolved = connection
        .query_row(
            &canonical_sql(
                "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                    predecessors, accepted_at_unix_ms
             FROM claims WHERE subject=?1 AND kind='attention.resolved'
             ORDER BY CANONICAL_DESC(claims)
             LIMIT 1",
            ),
            [subject],
            claim_from_row,
        )
        .optional()?;
    let fields = requested.body.get("fields").unwrap_or(&requested.body);
    let strings = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let resolution_fields = resolved
        .as_ref()
        .map(|claim| claim.body.get("fields").unwrap_or(&claim.body));
    let outcome = resolution_fields
        .and_then(|fields| fields.get("outcome"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Some(AttentionRequestView {
        subject: requested.subject,
        request: requested.id,
        reviewer: fields
            .get("reviewer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        title: fields
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        reason: fields
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        severity: fields
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_owned(),
        targets: strings("targets"),
        actor: requested.actor.unwrap_or_default(),
        status: outcome.clone().unwrap_or_else(|| "pending".into()),
        outcome,
        resolution_reason: resolution_fields
            .and_then(|fields| fields.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        requested_at_unix_ms: requested.accepted_at_unix_ms,
        resolved_at_unix_ms: resolved.map(|claim| claim.accepted_at_unix_ms),
        until: fields
            .get("until")
            .and_then(Value::as_str)
            .map(str::to_owned),
        step: fields
            .get("step")
            .and_then(Value::as_str)
            .map(str::to_owned),
        step_attempt: fields.get("step_attempt").and_then(Value::as_u64),
        closed_by: fields
            .get("closed_by")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

fn attention_target_can_end(connection: &Connection, target: &str) -> Result<bool> {
    Ok(ATTENTION_ENDING_TARGETS
        .iter()
        .any(|prefix| target.starts_with(prefix))
        || pull_request_state_tx(connection, target)?.is_some())
}

type TargetState = (String, Option<u128>);

/// Target kinds whose own state can end an attention request; see
/// the retained legacy request metadata. A pull request resource
/// can end one too, once st has observed it.
const ATTENTION_ENDING_TARGETS: &[&str] = &[
    "step-run/",
    "run-generation/",
    "mission/",
    "mission-run/",
    "attention/",
    "agent/",
    "observer/",
    "subscription/",
    "loop-run/",
    "message/",
];

fn pull_request_state_tx(connection: &Connection, target: &str) -> Result<Option<(String, u128)>> {
    let subject = target
        .split_once('@')
        .map_or(target, |(subject, _)| subject);
    if !subject.starts_with("resource/") {
        return Ok(None);
    }
    let observed = connection
        .query_row(
            &canonical_sql(
                "SELECT json_extract(body, '$.fields'), accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='resource.observed'
             ORDER BY COALESCE(json_extract(body, '$.fields.attribution_only'), 0), CANONICAL_DESC(claims) LIMIT 1",
            ),
            [subject],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((Some(fields), accepted_at)) = observed else {
        return Ok(None);
    };
    let fields = serde_json::from_str::<Value>(&fields).unwrap_or(Value::Null);
    if fields.get("kind").and_then(Value::as_str) != Some("vcs.pull-request") {
        return Ok(None);
    }
    let Some(facts) = fields.get("facts") else {
        return Ok(None);
    };
    let merged = |value: &Value| value.get("merged").and_then(Value::as_bool) == Some(true);
    let state = match facts.get("state") {
        _ if merged(facts) => "merged",
        Some(Value::String(state)) if state == "open" => "open",
        Some(Value::String(_)) => "closed",
        Some(state @ Value::Object(_)) if merged(state) => "merged",
        Some(state @ Value::Object(_))
            if state.get("state").and_then(Value::as_str) == Some("closed") =>
        {
            "closed"
        }
        _ => "open",
    };
    Ok(Some((
        state.into(),
        accepted_at.parse().unwrap_or_default(),
    )))
}
