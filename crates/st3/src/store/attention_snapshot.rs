//! Derived person work and operational episodes. Legacy attention claims are audit data only.
use super::*;
use crate::model::LoopSpec;

/// How long a run whose cleanup failed shows that fault after it finishes.
const FINAL_STEP_FAULT_MS: u128 = 24 * 60 * 60 * 1000;
/// How long a declared seat may go without any runtime observation before that is a fault. A seat
/// that never starts parks nothing and fails nothing, so nobody finds it by looking.
const SEAT_NOT_STARTED_MS: u128 = 10 * 60 * 1000;

impl Store {
    /// The mission context of a person ask a mission step made: its mission, and the step that
    /// waits on the answer with its goal. `None` for a standalone ask or an update, which belong
    /// to no mission of their own.
    pub(crate) fn person_ask_context(&self, subject: &str) -> Result<Option<Value>> {
        let connection = self.readers.get();
        let Some(ask) = person_work::request(&connection, subject)? else {
            return Ok(None);
        };
        let fields = &ask.body["fields"];
        let (Some(origin), Some(view)) = (
            fields["origin_step"].as_str(),
            person_work::step(&connection, subject)?,
        ) else {
            return Ok(None);
        };
        let Some(blocked) = person_work::step(&connection, origin)? else {
            return Ok(None);
        };
        // Runs are stored without their `mission-run/` prefix, and missions without `mission/`.
        let mission: Option<String> = connection
            .query_row(
                "SELECT mission_id FROM mission_runs WHERE id=?1",
                [view.run.strip_prefix("mission-run/").unwrap_or(&view.run)],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| {
                if id.starts_with("mission/") {
                    id
                } else {
                    format!("mission/{id}")
                }
            });
        Ok(Some(json!({
            "mission_id": mission,
            "blocked": {
                "step_run_id": origin,
                "step": blocked.step,
                "goal": blocked.goals.join("\n"),
                "attempt": blocked.attempt,
            },
        })))
    }

    /// One alert per seat whose harness waits on a person at a native prompt: a permission, a
    /// question or a review it shows in its terminal. It lives in the seat's conversation and
    /// clears when the harness reports the prompt gone, however it was answered, or when the
    /// seat's runtime ends. Each seat costs one indexed read of its newest harness observation,
    /// and only a blocked seat reads its runtime and owner.
    pub(super) fn harness_prompt_attention_items(
        &self,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let seats = connection
            .prepare_cached("SELECT subject FROM desired WHERE kind='agent' ORDER BY subject")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let newest = |subject: &str, kind: &str| -> Result<Option<(String, u128, Value)>> {
            let row = connection
                .prepare_cached(&format!(
                    "{} LIMIT 1",
                    newest_claims_of_kind_query(
                        "claims.id, claims.accepted_at_unix_ms, claims.body",
                        kind
                    )
                ))?
                .query_row(params![subject, i64::MAX], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .optional()?;
            row.map(|(id, at, body)| -> Result<(String, u128, Value)> {
                let body: Value = serde_json::from_str(&body)?;
                let fields = body.get("fields").cloned().unwrap_or(body);
                Ok((id, at.parse()?, fields))
            })
            .transpose()
        };
        let mut items = Vec::new();
        for seat in seats {
            let Some((claim, at, harness)) = newest(&seat, "harness.observed")? else {
                continue;
            };
            if harness["blocked_on"] != "human" {
                continue;
            }
            // The prompt was refused in the terminal, which its harness did not report: the
            // reconciler recorded that once, keyed by this observation.
            let gone: bool = connection
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_operation_index
                       WHERE json_extract(body, '$._operation.id')=?1)",
                )?
                .query_row([native_prompt_gone_operation(&claim)], |row| row.get(0))?;
            if gone {
                continue;
            }
            // Only the running incarnation's prompt waits: a restarted seat's old prompt is gone.
            let Some((_, _, runtime)) = newest(&seat, "runtime.observed")? else {
                continue;
            };
            if runtime["status"] != "running"
                || runtime["incarnation_id"] != harness["incarnation_id"]
                || !person_work::declaration_live(&connection, &seat)?
            {
                continue;
            }
            let Some(owner) = self.agent_person(&seat)? else {
                continue;
            };
            if person.is_some_and(|person| person != owner) {
                continue;
            }
            let ask = harness["ask"].as_str().unwrap_or("unknown");
            let what = match ask {
                "permission" => "permission",
                "question" => "an answer",
                "review" => "a review",
                _ => "you",
            };
            let prompt = harness["reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .unwrap_or("its terminal shows a prompt");
            items.push(AttentionItemView {
                episode: claim,
                priority: "high".into(),
                kind: "harness-prompt".into(),
                review_mode: None,
                subject: seat.clone(),
                person: owner,
                requester_id: None,
                launch_id: None,
                variant_id: None,
                message_id: None,
                conversation: Some(seat.clone()),
                title: format!("{seat} is waiting for {what}"),
                detail: format!(
                    "{prompt}\n\nAnswer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered."
                ),
                request: None,
                mission: None,
                mission_run: None,
                step: None,
                targets: vec![seat.clone()],
                requested_at_unix_ms: at,
                actions: vec![attention_action(
                    "Attach to answer",
                    &["st", "terminals", "attach", &seat],
                )],
            });
        }
        Ok(items)
    }

    pub(super) fn person_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = Vec::new();
        let mut query = connection.prepare(
            // A range on the assignee index, not `LIKE 'person/%'`: SQLite does not use an index
            // for a case-insensitive LIKE, so that read every step run in the store. Assignees
            // are written in lower case; the range also drops an upper-case `Person/` spelling
            // that LIKE would have matched.
            "SELECT subject FROM step_runs WHERE assignee>='person/' AND assignee<'person0'
            AND status IN ('ready','pending') AND (?1 IS NULL OR assignee=?1) ORDER BY subject",
        )?;
        let subjects = query
            .query_map([person], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for subject in subjects {
            if let Some(item) = person_attention_item(&connection, &subject, as_of)? {
                items.push(item);
            }
        }
        Ok(items)
    }

    /// Whether `agent` is declared and not stopped or retired: the same test that lets an agent
    /// own a fault.
    pub(crate) fn agent_is_live(&self, agent: &str) -> Result<bool> {
        let connection = self.readers.get();
        Ok(agent.starts_with("agent/")
            && current_desired_row(&connection, agent)?.is_some_and(|row| row.kind == "agent")
            && person_work::declaration_live(&connection, agent)?)
    }

    /// The fleet's fault agent: the first live agent, by subject, whose declaration carries
    /// `handles-faults`. It takes each fault that no step assignee or agent requester owns.
    pub(crate) fn fleet_fault_agent(&self) -> Result<Option<String>> {
        let connection = self.readers.get();
        let mut query = connection.prepare(
            "SELECT subject FROM desired WHERE kind='agent' AND EXISTS (
                SELECT 1 FROM json_each(desired.body, '$.children')
                WHERE json_extract(value, '$.name')='handles-faults'
            ) ORDER BY subject",
        )?;
        let agents = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for agent in agents {
            if person_work::declaration_live(&connection, &agent)? {
                return Ok(Some(agent));
            }
        }
        Ok(None)
    }

    /// The agent that owns `fault`: the agent it names, as a broken gate names the agent that
    /// published it, else the agent assigned to the failed step, else the requester of its run
    /// or of a run above it when that is an agent, else `fallback`. Only a live agent
    /// declaration owns a fault, and a faulted agent never owns its own fault.
    pub(super) fn fault_owner(
        &self,
        fault: &AttentionItemView,
        fallback: Option<&str>,
    ) -> Result<Option<String>> {
        let connection = self.readers.get();
        let owns = |agent: &str| -> Result<bool> {
            Ok(agent.starts_with("agent/")
                && agent != fault.subject
                && current_desired_row(&connection, agent)?.is_some_and(|row| row.kind == "agent")
                && person_work::declaration_live(&connection, agent)?)
        };
        if owns(&fault.person)? {
            return Ok(Some(fault.person.clone()));
        }
        let mut step = fault.step.clone().or_else(|| {
            fault
                .subject
                .starts_with("step-run/")
                .then(|| fault.subject.clone())
        });
        let mut run = fault.mission_run.clone().or_else(|| {
            fault
                .subject
                .starts_with("mission-run/")
                .then(|| fault.subject.clone())
        });
        if step.is_none() && run.is_none() {
            run = current_desired_row(&connection, &fault.subject)?.and_then(|row| row.owner_run);
        }
        let mut seen = BTreeSet::new();
        loop {
            if let Some(view) = step
                .take()
                .map(|step| person_work::step(&connection, &step))
            {
                let Some(view) = view? else { break };
                if let Some(agent) = view.assigned_to.as_deref()
                    && owns(agent)?
                {
                    return Ok(Some(agent.to_owned()));
                }
                run = Some(view.run);
            }
            let Some(current) = run.take() else { break };
            let id = current.trim_start_matches("mission-run/").to_owned();
            if !seen.insert(id.clone()) {
                break;
            }
            let Some(header) = mission_run_header_tx(&connection, &id).optional()? else {
                break;
            };
            if owns(&header.requester)? {
                return Ok(Some(header.requester));
            }
            step = header.parent_step_run;
        }
        Ok(fallback
            .filter(|agent| *agent != fault.subject)
            .map(str::to_owned))
    }

    /// Persist the observed episode on its operational subject, with declaration/incarnation
    /// fences. The compatibility view returned here is used only inside the reconciler.
    pub(crate) fn record_operational_failure(
        &self,
        episode: &str,
        input: &AttentionRequest,
    ) -> Result<AttentionRequestView, St3Error> {
        self.record_failure(episode, input, "observed")
    }

    pub(crate) fn record_runtime_failure(
        &self,
        episode: &str,
        input: &AttentionRequest,
        condition: &str,
    ) -> Result<AttentionRequestView, St3Error> {
        self.record_failure(episode, input, condition)
    }

    fn record_failure(
        &self,
        episode: &str,
        input: &AttentionRequest,
        condition: &str,
    ) -> Result<AttentionRequestView, St3Error> {
        let source = input.targets.first().ok_or_else(|| {
            St3Error::new(
                "missing-failure-source",
                "an operational failure needs its source",
            )
        })?;
        let previous = self.operational_failure(episode).map_err(internal)?;
        if let Some(existing) = previous
            .as_ref()
            .filter(|failure| failure.status == "pending")
        {
            return Ok(existing.clone());
        }
        let revision = if source.starts_with("schedule/") {
            None
        } else {
            self.selected_desired_token(source).map_err(internal)?
        };
        let runtime = self.latest_actual_value(source).map_err(internal)?;
        let incarnation = runtime
            .as_ref()
            .and_then(|value| {
                value
                    .pointer("/fields/incarnation_id")
                    .or_else(|| value.get("incarnation_id"))
            })
            .and_then(Value::as_str);
        self.append_claim(&ClaimInput {
            subject: source.clone(),
            kind: "operational.failure".into(),
            actor: Some(input.actor.clone()),
            fields: BTreeMap::from([
                ("episode".into(), json!(episode)),
                ("condition".into(), json!(condition)),
                ("reviewer".into(), json!(input.reviewer)),
                ("title".into(), json!(input.title)),
                ("reason".into(), json!(input.reason)),
                ("severity".into(), json!(input.severity)),
                ("targets".into(), json!(input.targets)),
                ("source_revision".into(), json!(revision)),
                ("incarnation".into(), json!(incarnation)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "{}:{}",
                input.idempotency_key,
                previous
                    .as_ref()
                    .map(|failure| failure.request.as_str())
                    .unwrap_or("first")
            )),
        })?;
        self.operational_failure(episode)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the failure was not recorded"))
    }

    pub(crate) fn operational_failure(
        &self,
        episode: &str,
    ) -> Result<Option<AttentionRequestView>> {
        Ok(self
            .operational_failures()?
            .into_iter()
            .find(|failure| failure.subject == episode))
    }

    pub(crate) fn pending_operational_failures(
        &self,
        actor: &str,
        origin: &str,
    ) -> Result<Vec<AttentionRequestView>> {
        let connection = self.readers.get();
        let mut failures = Vec::new();
        for failure in self.operational_failures()? {
            if failure.status == "pending"
                && failure.actor == actor
                && connection.query_row(
                    "SELECT origin=?2 FROM claims WHERE id=?1",
                    params![failure.request, origin],
                    |row| row.get::<_, bool>(0),
                )?
            {
                failures.push(failure);
            }
        }
        Ok(failures)
    }

    pub(crate) fn recover_operational_failure(
        &self,
        episode: &str,
        reason: &str,
        key: &str,
    ) -> Result<AttentionRequestView, St3Error> {
        let current = self
            .operational_failure(episode)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new("missing-operational-failure", "the episode does not exist")
            })?;
        let source = current
            .targets
            .first()
            .ok_or_else(|| St3Error::new("missing-failure-source", "the episode has no source"))?;
        self.append_claim(&ClaimInput {
            subject: source.clone(),
            kind: "operational.recovered".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("episode".into(), json!(episode)),
                ("failure".into(), json!(current.request)),
                ("reason".into(), json!(reason)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })?;
        self.operational_failure(episode)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("internal", "the recovery was not recorded"))
    }

    fn operational_failures(&self) -> Result<Vec<AttentionRequestView>> {
        let connection = self.readers.get();
        let mut query = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
            FROM claims WHERE kind IN ('operational.failure','operational.recovered') ORDER BY CANONICAL_ASC(claims)"))?;
        let claims = query
            .query_map([], claim_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut episodes: BTreeMap<String, AttentionRequestView> = BTreeMap::new();
        for claim in claims {
            let f = &claim.body["fields"];
            let episode = f["episode"].as_str().unwrap_or_default();
            if claim.kind == "operational.recovered" {
                if let Some(kept) = episodes.get_mut(episode) {
                    if f["failure"] == kept.request {
                        kept.status = "resolved".into();
                        kept.resolved_at_unix_ms = Some(claim.accepted_at_unix_ms);
                    }
                }
                continue;
            }
            episodes.insert(
                episode.into(),
                AttentionRequestView {
                    subject: episode.into(),
                    request: claim.id.clone(),
                    reviewer: f["reviewer"].as_str().unwrap_or("person/operator").into(),
                    title: f["title"].as_str().unwrap_or_default().into(),
                    reason: f["reason"].as_str().unwrap_or_default().into(),
                    severity: f["severity"].as_str().unwrap_or("error").into(),
                    targets: serde_json::from_value(f["targets"].clone())
                        .unwrap_or_else(|_| vec![claim.subject.clone()]),
                    actor: claim.actor.clone().unwrap_or_default(),
                    status: "pending".into(),
                    outcome: None,
                    resolution_reason: None,
                    requested_at_unix_ms: claim.accepted_at_unix_ms,
                    resolved_at_unix_ms: None,
                    until: None,
                    step: None,
                    step_attempt: None,
                    closed_by: None,
                },
            );
        }
        Ok(episodes.into_values().collect())
    }

    fn reconcile_fault_attention(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        if person.is_some_and(|person| person != "person/operator") {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut query = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
            FROM claims WHERE kind='reconcile.fault' ORDER BY CANONICAL_ASC(claims)"))?;
        let mut latest = BTreeMap::new();
        for claim in query.query_map([], claim_from_row)? {
            let claim = claim?;
            let scope = claim.body["fields"]["scope"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            latest.insert((claim.subject.clone(), scope, claim.origin.clone()), claim);
        }
        let mut items = Vec::new();
        for ((source, scope, _origin), claim) in latest {
            // A restart can diagnose many late admissions in one pass. Keep these visible in
            // mission views and doctor without creating an operator item for each run.
            if scope == crate::reconcile::FIRST_READINESS_FAULT_SCOPE
                || claim.body["fields"]["status"] != "faulted"
                || as_of.saturating_sub(claim.accepted_at_unix_ms) < 120_000
            {
                continue;
            }
            if source.starts_with("step-run/") {
                let Some(step) = person_work::step(&connection, &source)? else {
                    continue;
                };
                if !person_work::run_live(&connection, &step.run, Some(&step.generation), true)? {
                    continue;
                }
            } else if source.starts_with("mission-run/") {
                if !person_work::run_live(&connection, &source, None, true)? {
                    continue;
                }
            } else if !source.starts_with("daemon/")
                && !person_work::declaration_live(&connection, &source)?
            {
                continue;
            }
            items.push(AttentionItemView {
                episode: claim.id,
                priority: "high".into(),
                kind: "fault".into(),
                review_mode: None,
                subject: source.clone(),
                person: "person/operator".into(),
                requester_id: None,
                launch_id: None,
                variant_id: None,
                message_id: None, conversation: None,
                title: format!("st cannot reconcile {source}"),
                detail: format!(
                    "{scope}: {}. Inspect with `st subject {source}`.",
                    claim.body["fields"]["reason"].as_str().unwrap_or_default()
                ),
                mission: None,
                mission_run: source.starts_with("mission-run/").then(|| source.clone()),
                step: None,
                targets: vec![source.clone()],
                requested_at_unix_ms: claim.accepted_at_unix_ms,
                actions: vec![attention_action(
                    "inspect source",
                    &["st", "subject", &source],
                )],
                request: None,
            });
        }
        Ok(items)
    }

    fn stopped_loop_item(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
    ) -> Result<Option<AttentionItemView>> {
        let Some(state) = self.latest_claim(&view.subject, Some("step-run.state"))? else {
            return Ok(None);
        };
        let status = state
            .body
            .pointer("/fields/status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(status, "failed" | "cancelled") {
            return Ok(None);
        }
        let reason = state
            .body
            .pointer("/fields/reason")
            .and_then(Value::as_str)
            .unwrap_or("the loop stopped");
        let loop_subject = format!(
            "loop-run/{}/{}",
            run.generation
                .strip_prefix("run-generation/")
                .unwrap_or(&run.generation),
            loop_spec.path
        );
        let feedback = self
            .claims_for(&loop_subject, Some("loop.round-result"))?
            .into_iter()
            .rev()
            .find_map(|claim| {
                claim
                    .body
                    .pointer("/fields/feedback")
                    .and_then(Value::as_str)
                    .filter(|feedback| !feedback.is_empty())
                    .map(str::to_owned)
            });
        let mission = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
        let remedy = if status == "failed" {
            format!(
                "`st work retry {} --reason \"...\"` runs round {} after you address the cause; `st missions cancel {} --reason \"...\"` ends the run.",
                view.subject,
                view.attempt.saturating_add(1),
                run.subject
            )
        } else {
            format!(
                "st cannot retry a cancelled step. Correct the mission, then end this run with `st missions cancel {} --reason \"...\"` if it still runs, and start a new one with `st missions start {mission}`.",
                run.subject
            )
        };
        let mut detail = format!(
            "Loop `{loop_subject}` stopped in round {}: {reason}.",
            view.attempt
        );
        if let Some(feedback) = feedback {
            detail.push_str(&format!(" Latest feedback: `{feedback}`."));
        }
        detail.push(' ');
        detail.push_str(&remedy);
        detail.push_str(
            " This item closes when the loop runs again or its run is revised or cancelled.",
        );
        let (title, reviewer, severity) = match &loop_spec.exhaustion_attention {
            Some(attention) => (
                attention.title.clone(),
                attention.reviewer.clone(),
                attention.severity.clone(),
            ),
            None => (
                format!("Loop `{}` stopped", loop_spec.id),
                if run.requester.starts_with("person/") {
                    run.requester.clone()
                } else {
                    "person/operator".into()
                },
                "error".into(),
            ),
        };
        Ok(Some(AttentionItemView {
            episode: state.id,
            priority: if severity == "warning" {
                "normal"
            } else {
                "high"
            }
            .into(),
            kind: "fault".into(),
            review_mode: None,
            subject: loop_subject.clone(),
            person: reviewer,
            requester_id: None,
            launch_id: None,
            variant_id: None,
            message_id: None, conversation: None,
            title,
            detail,
            mission: Some(run.mission.clone()),
            mission_run: Some(run.subject.clone()),
            step: Some(view.subject.clone()),
            targets: vec![loop_subject, run.subject.clone()],
            requested_at_unix_ms: state.accepted_at_unix_ms,
            actions: vec![attention_action(
                "inspect source",
                &["st", "subject", &view.subject],
            )],
            request: None,
        }))
    }

    fn parked_runtime_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = Vec::new();
        for desired in self
            .desired_subjects()?
            .into_iter()
            .filter(|desired| desired.kind == "agent")
        {
            if !person_work::declaration_live(&connection, &desired.subject)? {
                continue;
            }
            let Some(token) = self.selected_desired_token(&desired.subject)? else {
                continue;
            };
            let decisions =
                self.claims_for(&desired.subject, Some("runtime.reconcile-decision"))?;
            let decision = decisions.iter().find(|decision| {
                let key = decision.body["fields"]["key"].as_str().unwrap_or_default();
                key == format!("runtime-crash-loop:{token}")
                    || key == format!("codex-crash-loop:{token}")
            });
            let Some(decision) = decision else { continue };
            if decision.accepted_at_unix_ms > as_of {
                continue;
            }
            let codex = decision.body["fields"]["key"]
                .as_str()
                .unwrap_or_default()
                .starts_with("codex-crash-loop:");
            let reviewer = if codex {
                "person/alex"
            } else {
                "person/operator"
            };
            if person.is_some_and(|person| person != reviewer) {
                continue;
            }
            let source = desired.subject;
            let diagnostic = self.last_driver_diagnostic(&source)?;
            items.push(AttentionItemView {
                episode: decision.id.clone(), priority: "high".into(), kind: "fault".into(), review_mode: None,
                subject: source.clone(), person: reviewer.into(), requester_id: None, launch_id: None, variant_id: None, message_id: None, conversation: None,
                title: if codex { "A Codex agent stopped after repeated failures" } else { "An agent stopped after repeated runtime failures" }.into(),
                detail: format!("{source}: {}.{} Inspect the seat and revise its desired declaration before restarting.", decision.body["fields"]["reason"].as_str().unwrap_or("the runtime is parked"), diagnostic.map(|diagnostic| format!(" The driver's last diagnostic: {diagnostic}.")).unwrap_or_default()),
                mission: None, mission_run: desired.owner_run, step: None, targets: vec![source.clone()], requested_at_unix_ms: decision.accepted_at_unix_ms,
                actions: vec![attention_action("inspect source", &["st", "subject", &source])],
                request: None,
            });
        }
        Ok(items)
    }

    /// The newest word a seat's driver left: its `harness.diagnostic` code and reason. The first
    /// thing anyone asks of a seat that will not start, and the reason a fault should carry it.
    fn last_driver_diagnostic(&self, subject: &str) -> Result<Option<String>> {
        Ok(self
            .latest_observation(subject, "harness.diagnostic")?
            .and_then(|claim| {
                let fields = claim.body.get("fields").unwrap_or(&claim.body);
                let reason = fields.get("reason").and_then(Value::as_str)?;
                Some(match fields.get("code").and_then(Value::as_str) {
                    Some(code) => format!("{code}: {reason}"),
                    None => reason.to_owned(),
                })
            }))
    }

    /// A seat that is declared to run and that no runtime observation has ever described, for
    /// ten minutes: `st agents ls` shows it as `desired`, and nothing else says it is not coming.
    /// Parking and failing both leave a record; this leaves none, so it is found by looking, or
    /// not at all. The fault ends when the first observation appears.
    fn unstarted_seat_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        const REVIEWER: &str = "person/operator";
        if person.is_some_and(|person| person != REVIEWER) {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut items = Vec::new();
        for desired in self
            .desired_subjects()?
            .into_iter()
            .filter(|desired| desired.kind == "agent" && desired.member.is_some())
        {
            if !person_work::declaration_live(&connection, &desired.subject)? {
                continue;
            }
            let Some(token) = self.selected_desired_token(&desired.subject)? else {
                continue;
            };
            let Some(declared) = self.claim_by_id(&token)? else {
                continue;
            };
            let due = declared
                .accepted_at_unix_ms
                .saturating_add(SEAT_NOT_STARTED_MS);
            if due > as_of
                || self
                    .latest_observation(&desired.subject, "runtime.observed")?
                    .is_some()
                || crate::suspension::current(self, &desired.subject)?.is_some()
            {
                continue;
            }
            let source = desired.subject;
            let host = desired
                .member
                .as_ref()
                .map(|member| member.host.clone())
                .unwrap_or_default();
            let diagnostic = match self.last_driver_diagnostic(&source)? {
                Some(diagnostic) => format!(" The driver's last diagnostic: {diagnostic}."),
                None => " The driver has left no diagnostic: it never ran.".into(),
            };
            items.push(AttentionItemView {
                episode: token.clone(), priority: "high".into(), kind: "fault".into(), review_mode: None,
                subject: source.clone(), person: REVIEWER.into(), requester_id: None, launch_id: None, variant_id: None, message_id: None, conversation: None,
                title: "An agent seat has not started".into(),
                detail: format!("{source} has been declared to run on {host} for over {} minutes and no runtime has ever been observed for it, so it is still `desired`.{diagnostic} Check that the host is up and its daemon can start the seat.", SEAT_NOT_STARTED_MS / 60_000),
                mission: None, mission_run: desired.owner_run, step: None, targets: vec![source.clone()], requested_at_unix_ms: due,
                actions: vec![attention_action("inspect seat", &["st", "agents", "show", &source])],
                request: None,
            });
        }
        Ok(items)
    }

    fn loop_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        fn loops<'a>(mission: &'a MissionSpec, kept: &mut Vec<&'a LoopSpec>) {
            for step in mission.steps.values() {
                if let Some(spec) = step.loop_spec.as_deref() {
                    kept.push(spec);
                }
                if let Some(nested) = step.nested_mission.as_deref() {
                    loops(nested, kept);
                }
            }
        }
        let connection = self.readers.get();
        let mut items = Vec::new();
        for run in self.mission_run_headers()? {
            if !person_work::run_live(&connection, &run.subject, Some(&run.generation), true)? {
                continue;
            }
            let Some(mission) = self.mission_spec(
                run.mission.trim_start_matches("mission/"),
                Some(&run.revision),
            )?
            else {
                continue;
            };
            let mut specs = Vec::new();
            loops(&mission, &mut specs);
            for spec in specs {
                let subject = format!(
                    "step-run/{}/{}",
                    run.generation.trim_start_matches("run-generation/"),
                    spec.path
                );
                let Some(view) = person_work::step(&connection, &subject)? else {
                    continue;
                };
                if !matches!(view.status.as_str(), "failed" | "cancelled") {
                    continue;
                }
                if let Some(item) = self.stopped_loop_item(&run, &view, spec)? {
                    if item.requested_at_unix_ms <= as_of
                        && person.is_none_or(|person| person == item.person)
                    {
                        items.push(item);
                    }
                }
            }
        }
        Ok(items)
    }

    fn observer_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        use crate::reconcile::ObserverCondition;
        let connection = self.readers.get();
        let mut items = Vec::new();
        for desired in self
            .desired_subjects()?
            .into_iter()
            .filter(|desired| desired.kind == "observer")
        {
            if !person_work::declaration_live(&connection, &desired.subject)? {
                continue;
            }
            let revision = self.selected_desired_revision(&desired.subject)?;
            let states = self.claims_for(&desired.subject, Some("observer.state"))?;
            let Some(latest) = states.last() else {
                continue;
            };
            let fields = &latest.body["fields"];
            if fields["state"] == "healthy" || fields["revision"].as_str() != revision.as_deref() {
                continue;
            }
            let reason = fields["reason"]
                .as_str()
                .unwrap_or("the observer cannot observe")
                .to_owned();
            let code = fields["error_code"].as_str().unwrap_or("unreachable");
            let condition = if fields["state"] == "degraded" {
                ObserverCondition::Rejected(reason)
            } else {
                match code {
                    "access-denied" => ObserverCondition::Access(reason),
                    "rate-limited" => ObserverCondition::RateLimited {
                        reason,
                        outlasted: fields["next_check_unix_ms"]
                            .as_str()
                            .and_then(|value| value.parse::<u128>().ok())
                            .is_some_and(|reset| as_of >= reset.saturating_add(60_000)),
                    },
                    _ => ObserverCondition::Unreachable(reason),
                }
            };
            let first = states
                .iter()
                .rev()
                .take_while(|state| {
                    state.body["fields"]["state"] == fields["state"]
                        && state.body["fields"]["error_code"]
                            .as_str()
                            .unwrap_or("unreachable")
                            == fields["error_code"].as_str().unwrap_or("unreachable")
                        && state.body["fields"]["revision"] == fields["revision"]
                })
                .last()
                .unwrap_or(latest);
            if first.accepted_at_unix_ms > as_of
                || !condition.needs_person(as_of.saturating_sub(first.accepted_at_unix_ms))
            {
                continue;
            }
            let reviewer = desired
                .owner_run
                .as_ref()
                .map(|run| self.mission_run(run))
                .transpose()?
                .flatten()
                .map(|run| run.requester)
                .filter(|recipient| recipient.starts_with("person/"))
                .unwrap_or_else(|| "person/operator".into());
            if person.is_some_and(|person| person != reviewer) {
                continue;
            }
            let source = desired.subject;
            items.push(AttentionItemView {
                episode: first.id.clone(),
                priority: "high".into(),
                kind: "fault".into(),
                review_mode: None,
                subject: source.clone(),
                person: reviewer,
                requester_id: None,
                launch_id: None,
                variant_id: None,
                message_id: None, conversation: None,
                title: condition.title().into(),
                detail: condition.reason(&source, &latest.origin),
                mission: None,
                mission_run: desired.owner_run,
                step: None,
                targets: vec![source.clone()],
                requested_at_unix_ms: first.accepted_at_unix_ms,
                actions: vec![attention_action(
                    "inspect source",
                    &["st", "subject", &source],
                )],
                request: None,
            });
        }
        Ok(items)
    }

    /// The broken exec gates of current run generations, each for the publisher it names. An
    /// item holds while its generation is current and live and its step has not ended, and
    /// closes when a revision replaces the generation.
    pub(super) fn broken_gate_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = Vec::new();
        for failure in self.operational_failures()? {
            if failure.status != "pending"
                || failure.requested_at_unix_ms > as_of
                || person.is_some_and(|person| person != failure.reviewer)
            {
                continue;
            }
            let Some(claim) = self.claim_by_id(&failure.request)? else {
                continue;
            };
            if claim.body["fields"]["condition"] != GATE_BROKEN_CONDITION {
                continue;
            }
            let [run, generation, result, rest @ ..] = failure.targets.as_slice() else {
                continue;
            };
            if !person_work::run_live(&connection, run, Some(generation), false)? {
                continue;
            }
            let step = rest.first().cloned();
            if let Some(step) = &step {
                let Some(view) = person_work::step(&connection, step)? else {
                    continue;
                };
                if matches!(view.status.as_str(), "completed" | "failed" | "cancelled") {
                    continue;
                }
            }
            let (run, result) = (run.clone(), result.clone());
            let mut item = attention_item_from_failure(failure);
            item.episode = claim.id;
            item.subject = run.clone();
            item.mission_run = Some(run.clone());
            item.step = step;
            item.actions = vec![
                attention_action("inspect gate", &["st", "subject", "show", &result]),
                attention_action("inspect run", &["st", "missions", "show", &run]),
            ];
            items.push(item);
        }
        Ok(items)
    }

    pub(super) fn operational_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = self.reconcile_fault_attention(person, as_of)?;
        items.extend(self.observer_attention_items(person, as_of)?);
        items.extend(self.loop_attention_items(person, as_of)?);
        items.extend(self.parked_runtime_attention_items(person, as_of)?);
        items.extend(self.unstarted_seat_attention_items(person, as_of)?);
        for failure in self.operational_failures()? {
            if failure.status != "pending"
                || failure.requested_at_unix_ms > as_of
                || person.is_some_and(|person| person != failure.reviewer)
            {
                continue;
            }
            let Some(claim) = self.claim_by_id(&failure.request)? else {
                continue;
            };
            let source = &claim.subject;
            if source.starts_with("loop-run/") || source.starts_with("observer/") {
                continue;
            }
            // `broken_gate_items` reads broken gates, for their publisher.
            if claim.body["fields"]["condition"] == GATE_BROKEN_CONDITION {
                continue;
            }
            if source.starts_with("mission-run/") {
                if claim.body["fields"]["condition"] == MISSING_AGENT_CONDITION {
                    let latest: Option<String> = connection.query_row(
                        &canonical_sql("SELECT id FROM claims WHERE subject=?1 AND kind='operational.failure' AND json_extract(body,'$.fields.condition')=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                        params![source, MISSING_AGENT_CONDITION], |row| row.get(0),
                    ).optional()?;
                    if latest.as_deref() != Some(claim.id.as_str())
                        || !person_work::run_live(&connection, source, None, true)?
                    {
                        continue;
                    }
                    let run =
                        self.mission_run_for_reconcile(source.trim_start_matches("mission-run/"))?;
                    let generation = failure.targets.get(1);
                    if generation != Some(&run.generation) {
                        continue;
                    }
                    let Some(step) = failure
                        .targets
                        .get(2)
                        .and_then(|subject| run.steps.iter().find(|s| &s.subject == subject))
                    else {
                        continue;
                    };
                    if !self.missing_agent_fault_is_current(&run, step)? {
                        continue;
                    }
                    if claim.body["fields"]["episode"].as_str()
                        != Some(mission_eligibility::episode(&run, step).as_str())
                    {
                        continue;
                    }
                }
                // A finished run's failed cleanup is a fault on a run that completed, so the run
                // is no longer live. It stays for a day, then the failed step is all that remains.
                let cleanup_fault = claim.body["fields"]["episode"]
                    .as_str()
                    .is_some_and(|episode| episode.starts_with("final-steps:"))
                    && as_of.saturating_sub(failure.requested_at_unix_ms) < FINAL_STEP_FAULT_MS;
                if !cleanup_fault && !person_work::run_live(&connection, source, None, true)? {
                    continue;
                }
            } else if source.starts_with("step-run/") {
                // A timeout fault holds while its attempt is still running past the same budget:
                // an extension, a completion, a failure or a release ends it.
                let Some(step) = person_work::step(&connection, source)? else {
                    continue;
                };
                let extension =
                    step_timeout_extension_at(&connection, source, step.attempt, as_of)?;
                if !matches!(step.status.as_str(), "claimed" | "working" | "ready")
                    || claim.body["fields"]["episode"].as_str()
                        != Some(step_timeout_episode(source, step.attempt, extension).as_str())
                {
                    continue;
                }
            } else if !source.starts_with("daemon/")
                && !person_work::declaration_live(&connection, source)?
            {
                continue;
            }
            let f = &claim.body["fields"];
            // Upgrades retain historical operational episodes. A current login with a concrete
            // person has one derived person item; the old agent fault must not ask again.
            if f["condition"] == "provider-auth"
                && let Some(harness) = self.current_harness(source)?
            {
                let update_menu = self.claim_by_id(&harness.claim)?.is_some_and(|claim| {
                    claim.body["fields"]["code"] == "provider-update-prompt"
                });
                // The visible update modal takes priority without restoring authentication.
                // Once it clears, an unresolved login exposes its fallback fault again.
                if update_menu
                    || (harness.state == "needs-login" && self.agent_person(source)?.is_some())
                {
                    continue;
                }
            }
            if matches!(
                f["condition"].as_str(),
                Some("readiness" | "provider-auth" | "provider-trust" | "provider-update")
            ) {
                if self
                    .current_harness(source)?
                    .is_some_and(|harness| harness.is_ready())
                {
                    continue;
                }
            }
            if let Some(revision) = f["source_revision"].as_str() {
                if self.selected_desired_token(source)?.as_deref() != Some(revision) {
                    continue;
                }
            }
            if let Some(incarnation) = f["incarnation"].as_str() {
                let actual = self.latest_actual_value(source)?;
                let current = actual
                    .as_ref()
                    .and_then(|v| {
                        v.pointer("/fields/incarnation_id")
                            .or_else(|| v.get("incarnation_id"))
                    })
                    .and_then(Value::as_str);
                if current != Some(incarnation) {
                    continue;
                }
            }
            // Diagnostic history stays immutable; current work/runtime state decides whether
            // this episode still needs an owner. This changes no canonical timing fold.
            let wake = if f["condition"] == "work-wake-exhausted" {
                let Some(context) = self.current_exhausted_work_wake(&failure, as_of)? else {
                    continue;
                };
                Some(context)
            } else {
                None
            };
            let mut item = attention_item_from_failure(failure);
            item.kind = "fault".into();
            if f["condition"] == MISSING_AGENT_CONDITION {
                item.mission_run = Some(source.clone());
                item.step = item.targets.get(2).cloned();
            }
            item.subject = source.clone();
            item.episode = claim.id;
            item.priority = if claim.body["fields"]["severity"] == "warning" {
                "normal"
            } else {
                "high"
            }
            .into();
            item.actions = vec![attention_action(
                "inspect source",
                &["st", "subject", source],
            )];
            if let Some((step, diagnostic)) = wake {
                item.step = Some(step.subject.clone());
                item.mission_run = Some(step.run);
                item.actions = vec![
                    attention_action("inspect step", &["st", "work", "show", &step.subject]),
                    attention_action("inspect seat history", &["st", "trace", "show", &diagnostic.subject, "--limit", "20"]),
                ];
            }
            items.push(item);
        }
        Ok(items)
    }

    fn current_exhausted_work_wake(
        &self,
        failure: &AttentionRequestView,
        as_of: u128,
    ) -> Result<Option<(StepRunView, ClaimRecord)>> {
        let [agent, step_subject, diagnostic_id] = failure.targets.as_slice() else {
            return Ok(None);
        };
        let Some(diagnostic) = self.claim_by_id(diagnostic_id)? else {
            return Ok(None);
        };
        let fields = &diagnostic.body["fields"];
        if diagnostic.kind != "harness.diagnostic"
            || &diagnostic.subject != agent
            || diagnostic.accepted_at_unix_ms > as_of
            || fields["code"] != "work-wake-exhausted"
            || fields["status"] != "failed"
            || fields["step_run"].as_str() != Some(step_subject)
        {
            return Ok(None);
        }
        let Some(step) = self.step_run(step_subject)? else {
            return Ok(None);
        };
        if step.status != "ready"
            || step.assigned_to.as_ref() != Some(agent)
            || fields["attempt"].as_u64() != Some(u64::from(step.attempt))
            || fields["readiness_epoch"].as_u64() != Some(u64::from(step.readiness_epoch))
            || !person_work::run_live(
                &self.readers.get(),
                &step.run,
                Some(&step.generation),
                false,
            )?
        {
            return Ok(None);
        }
        let Some(harness) = self.current_harness(agent)? else {
            return Ok(None);
        };
        if !matches!(harness.state.as_str(), "ready" | "idle")
            || fields["incarnation_id"].as_str() != Some(harness.incarnation_id.as_str())
        {
            return Ok(None);
        }
        Ok(Some((step, diagnostic)))
    }
}

/// Derive one person step using the same source fences as the full reader.
pub(super) fn person_attention_item(
    connection: &Connection,
    subject: &str,
    as_of: u128,
) -> Result<Option<AttentionItemView>> {
    let Some(view) = person_work::step(connection, subject)? else {
        return Ok(None);
    };
    if !matches!(view.status.as_str(), "pending" | "ready")
        || view
            .assigned_to
            .as_deref()
            .is_none_or(|a| {
                !a.get(..7)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("person/"))
            })
    {
        return Ok(None);
    }
    let ask = person_work::request(connection, subject)?;
    if let Some(ask) = &ask {
        if !person_work::current(connection, ask, as_of)? {
            return Ok(None);
        }
    } else if view.status != "ready"
        || !person_work::run_live(connection, &view.run, Some(&view.generation), false)?
    {
        return Ok(None);
    }
    let person = view.assigned_to.as_deref().unwrap_or_default();
    let episode = ask.as_ref().map(|ask| ask.id.clone()).unwrap_or_else(|| {
        format!(
            "{}:{}:{}",
            view.generation, view.attempt, view.readiness_epoch
        )
    });
    let activated: Option<String> = connection.query_row(
        "SELECT activated_at_unix_ms FROM step_runs WHERE subject=?1",
        [subject],
        |row| row.get(0),
    )?;
    let since = ask.as_ref().map_or(
        activated
            .and_then(|at| at.parse().ok())
            .unwrap_or(view.created_at_unix_ms),
        |ask| {
            ask.body["fields"]["waiting_since"]
                .as_str()
                .and_then(|value| value.parse().ok())
                .unwrap_or(ask.accepted_at_unix_ms)
        },
    );
    let request = ask
        .as_ref()
        .and_then(|ask| ask.body["fields"].get("request"))
        .filter(|request| request.is_object())
        .cloned();
    let (label, response): (&str, &[&str]) = match request.as_ref().and_then(|r| r["type"].as_str())
    {
        Some("decision" | "choice") => ("done", &["--answer", "ANSWER_ID"]),
        // An update clears when the person opens it or presses read.
        Some("update") => ("read", &["--answer", "read"]),
        Some(_) => ("done", &["--text", "FEEDBACK"]),
        None => ("done", &["--summary", "RESPONSE"]),
    };
    Ok(Some(AttentionItemView {
        episode,
        priority: "normal".into(),
        kind: "person-step".into(),
        review_mode: None,
        subject: subject.to_owned(),
        person: person.into(),
        conversation: match ask
            .as_ref()
            .and_then(|ask| ask.actor.as_deref())
            .and_then(agent)
        {
            Some(actor) => Some(actor),
            None => conversation_agent(connection, None, Some(&view.run))?,
        },
        requester_id: ask.and_then(|ask| ask.actor),
        launch_id: None,
        variant_id: None,
        message_id: None,
        title: view
            .title
            .unwrap_or_else(|| "A step needs your response".into()),
        detail: view.goals.join("\n"),
        mission: None,
        mission_run: Some(view.run),
        step: Some(subject.to_owned()),
        targets: vec![subject.to_owned()],
        requested_at_unix_ms: since,
        actions: vec![attention_action(
            label,
            &[
                "st",
                "work",
                "done",
                subject,
                "--as",
                person,
                response[0],
                response[1],
            ],
        )],
        request,
    }))
}

/// `subject` when it names an agent: only an agent has a conversation an item can belong to.
pub(super) fn agent(subject: &str) -> Option<String> {
    subject.starts_with("agent/").then(|| subject.to_owned())
}

/// The agent whose conversation an item about `step` or `run` belongs to: the agent assigned to
/// the step, else the agent that requested its run, else none. A person's run has no agent.
pub(super) fn conversation_agent(
    connection: &Connection,
    step: Option<&str>,
    run: Option<&str>,
) -> Result<Option<String>> {
    let mut run = run.map(str::to_owned);
    if let Some(step) = step
        && let Some(view) = person_work::step(connection, step)?
    {
        if let Some(agent) = view.assigned_to.as_deref().and_then(agent) {
            return Ok(Some(agent));
        }
        run.get_or_insert(view.run);
    }
    let Some(run) = run else { return Ok(None) };
    Ok(
        mission_run_header_tx(connection, run.strip_prefix("mission-run/").unwrap_or(&run))
            .optional()?
            .and_then(|header| agent(&header.requester)),
    )
}

/// The idempotency key of the record that a native prompt opened by `observation` is gone.
pub(crate) fn native_prompt_gone_key(observation: &str) -> String {
    format!("native-prompt-gone:{observation}")
}

fn native_prompt_gone_operation(observation: &str) -> String {
    smallclaims::store::operation_id_for_key(&native_prompt_gone_key(observation))
}
