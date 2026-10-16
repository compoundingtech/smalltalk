//! Derived person work and operational episodes. Legacy attention claims are audit data only.
use super::*;
use crate::model::LoopSpec;

/// How long a run whose cleanup failed shows that fault after it finishes.
const FINAL_STEP_FAULT_MS: u128 = 24 * 60 * 60 * 1000;
/// How long a declared seat may go without any runtime observation before that is a fault. A seat
/// that never starts parks nothing and fails nothing, so nobody finds it by looking.
const SEAT_NOT_STARTED_MS: u128 = 10 * 60 * 1000;

/// The open steps assigned to a person (`?1` NULL for any person). A range on the assignee index,
/// not `LIKE 'person/%'`: SQLite does not use an index for a case-insensitive LIKE, so that read
/// every step run in the store. Assignees are written in lower case; the range also drops an
/// upper-case `Person/` spelling that LIKE would have matched. A test pins the plan.
const PERSON_STEPS_QUERY: &str = "SELECT subject FROM step_runs
    WHERE assignee>='person/' AND assignee<'person0'
    AND status IN ('ready','pending') AND (?1 IS NULL OR assignee=?1) ORDER BY subject";

impl Store {
    /// The person-attention collection at `now`, with the first instant after `now` at which
    /// the unchanged collection changes.
    ///
    /// The collection only admits items as their waiting time arrives; it has no clock-driven
    /// removals, so one read of its eventual projection serves both: the rows already waiting,
    /// in the eventual order, and the earliest admission still ahead. This reuses the same
    /// recipient, generation, run, declaration and resolution filters, including registered
    /// custom attention. An ask's waiting time can predate its asking claim (for migrated
    /// asks), so both gates must open before its row is visible.
    ///
    /// Observer/reconcile grace, final-step fault aging, checkpoint waits and runtime faults
    /// belong to `fault_snapshot`, not this collection. Person attention reads stored step
    /// state, not effective wake, lease or execution-deadline state. Those clocks therefore
    /// cannot change this projection without another graph write.
    pub(crate) fn attention_collection_snapshot(
        &self,
        person: Option<&str>,
        now: u128,
    ) -> Result<(Vec<AttentionItemView>, Option<u128>)> {
        let eventual = self.attention_snapshot(person, u128::MAX)?;
        let connection = self.readers.get();
        let mut starts = Vec::with_capacity(eventual.len());
        for item in &eventual {
            let mut starts_at = item.requested_at_unix_ms;
            if item.kind == "person-step"
                && let Some(ask) = person_work::request(&connection, &item.subject)?
            {
                starts_at = starts_at.max(ask.accepted_at_unix_ms);
            }
            starts.push(starts_at);
        }
        let mut valid_until = None;
        let rows = eventual
            .into_iter()
            .zip(starts)
            .filter(|(_, starts_at)| {
                if *starts_at > now {
                    valid_until =
                        Some(valid_until.map_or(*starts_at, |kept: u128| kept.min(*starts_at)));
                    false
                } else {
                    true
                }
            })
            .map(|(item, _)| item)
            .collect();
        Ok((rows, valid_until))
    }

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
        as_of: u128,
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
            if let Some(item) = self.screen_prompt_item(&connection, &seat, person)? {
                items.push(item);
            }
            items.extend(self.denial_items(&connection, &seat, person)?);
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
                       WHERE json_extract(body, '$._operation.id') IS NOT NULL
                         AND json_extract(body, '$._operation.id')=?1)",
                )?
                .query_row([native_prompt_gone_operation(&claim)], |row| row.get(0))?;
            if gone || native_prompt_answer(&connection, &claim)?.is_some() {
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
            let answerable = answerable(&harness, as_of);
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
                answers: prompt_answers(&harness, as_of),
                conversation: Some(seat.clone()),
                title: format!("{seat} is waiting for {what}"),
                // Codex names what its request asks to do, not the call: that is the pending
                // request in the seat's conversation.
                detail: if answerable && harness["driver"] == "codex" {
                    format!(
                        "Codex asks {}; what it would do is the pending request in the seat's conversation. Allow or deny it here, or answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered.",
                        codex_approval_words(prompt)
                    )
                } else if harness["driver"] == "codex" && prompt == "waitingOnApproval" {
                    "Codex waits on an approval st did not see requested, so st cannot answer it: answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered.".into()
                } else if answerable && subagent_prompt(&harness) {
                    format!(
                        "A subagent of this seat asks to use {}; its call is in the subagent's own conversation, not this one. Deny it here, or answer it in the seat's terminal (Ctrl+] in stui), where Claude shows it. This alert clears when the prompt is gone, however it was answered.",
                        prompt.trim_start_matches(st_drivers::claude_prompts::SUBAGENT_PREFIX)
                    )
                } else if answerable && plan_approval(&harness) {
                    "Claude asks you to approve its plan; the plan is the pending ExitPlanMode call in the seat's conversation. Approve it here to let Claude proceed, deny it to have Claude keep planning, or answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered.".into()
                } else if answerable && ask == "question" {
                    "Claude asks you a question; its questions and their options are the pending AskUserQuestion call in the seat's conversation. Answer it here, or in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered.".into()
                } else if answerable {
                    format!(
                        "Claude asks to use {prompt}; the call it would make is the pending one in the seat's conversation. Allow or deny it here, or answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered."
                    )
                } else if answerable_kind(&harness) {
                    format!(
                        "Claude asks to use {prompt}. It has waited longer than st can answer it from here: answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered."
                    )
                } else {
                    format!(
                        "{prompt}\n\nAnswer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered."
                    )
                },
                request: None,
                mission: None,
                mission_run: None,
                step: None,
                targets: vec![seat.clone()],
                // Since the prompt opened, not since its latest restatement: a prompt that has
                // waited for hours must not read as minutes old. An opening time of zero or after
                // the restatement is not one, so the restatement's time stands.
                requested_at_unix_ms: harness["observed_since_ms"]
                    .as_u64()
                    .map(u128::from)
                    .filter(|since| (1..=at).contains(since))
                    .unwrap_or(at),
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
        let mut query = connection.prepare(PERSON_STEPS_QUERY)?;
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
                message_id: None, answers: Vec::new(), conversation: None,
                title: format!("st cannot reconcile {source}"),
                detail: format!(
                    "{scope}: {}. Inspect with `st subject show {source}`.",
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
        if status == "cancelled" {
            // Cancellation before admission changes the loop step's state without ever
            // starting a round. A stopped-loop fault needs round activity in that case;
            // structural failures still need attention even before the first round.
            smallclaims::touched::note_read(|| format!("children-of-step:{}", view.subject));
            let has_round: bool = self.readers.get().query_row(
                "SELECT EXISTS(SELECT 1 FROM mission_runs WHERE parent_step_run=?1)",
                [&view.subject],
                |row| row.get(0),
            )?;
            if !has_round {
                return Ok(None);
            }
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
            message_id: None, answers: Vec::new(), conversation: None,
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
                subject: source.clone(), person: reviewer.into(), requester_id: None, launch_id: None, variant_id: None, message_id: None, answers: Vec::new(), conversation: None,
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
                subject: source.clone(), person: REVIEWER.into(), requester_id: None, launch_id: None, variant_id: None, message_id: None, answers: Vec::new(), conversation: None,
                title: "An agent seat has not started".into(),
                detail: format!("{source} has been declared to run on {host} for over {} minutes and no runtime has ever been observed for it, so it is still `desired`.{diagnostic} Check that the host is up and its daemon can start the seat.", SEAT_NOT_STARTED_MS / 60_000),
                mission: None, mission_run: desired.owner_run, step: None, targets: vec![source.clone()], requested_at_unix_ms: due,
                actions: vec![attention_action("inspect seat", &["st", "agents", "show", &source])],
                request: None,
            });
        }
        Ok(items)
    }

    /// A completed after-step with an unarmed budget is missing admission provenance,
    /// not queued work. Keep the time unknown and make its loss an actionable fault.
    fn missing_admission_clock_faults(
        &self,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        // Begin with the small open-run index, then point-read each deadline and its
        // after-steps. Finished history is never scanned for this fault.
        let runs = connection.prepare_cached(&format!(
            "SELECT mission_runs.id FROM mission_runs INDEXED BY mission_runs_open_index
             CROSS JOIN mission_run_deadlines AS deadlines
             WHERE {OPEN_MISSION_RUN} AND deadlines.run_id=mission_runs.id
               AND deadlines.deadline_at_unix_ms IS NULL
               AND EXISTS(SELECT 1 FROM step_runs INDEXED BY step_runs_run_index
                 WHERE step_runs.run_id=mission_runs.id AND step_runs.step_path=?1
                   AND step_runs.status='completed')"
        ))?.query_map([crate::mission::AFTER_RUN_STEP], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = Vec::new();
        for id in runs {
            let run = mission_run_header_tx(&connection, &id)?;
            if !person_work::run_live(&connection, &run.subject, Some(&run.generation), true)?
                || person.is_some_and(|person| person != run.requester)
            {
                continue;
            }
            items.push(AttentionItemView {
                episode: format!("deadline-admission-missing:{id}"),
                priority: "high".into(), kind: "fault".into(), review_mode: None,
                subject: run.subject.clone(), person: run.requester.clone(),
                requester_id: None, launch_id: None, variant_id: None, message_id: None,
                answers: Vec::new(), conversation: None,
                title: "Run admission time unavailable".into(),
                detail: "This run was admitted, but its timeout cannot be reconstructed because the accepted admission history is unavailable. Its timeout is not armed. Restore the original admission history from a trusted copy and rebuild the projection, or cancel the run. A retry or duplicate completion cannot start a fresh timeout budget.".into(),
                mission: Some(run.mission.clone()), mission_run: Some(run.subject.clone()),
                step: None, targets: vec![run.subject.clone()],
                requested_at_unix_ms: run.created_at_unix_ms,
                actions: vec![attention_action("inspect run", &["st", "missions", "show", &run.subject])],
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
                message_id: None, answers: Vec::new(), conversation: None,
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
        self.broken_gate_items_from_failures(person, as_of, self.operational_failures()?)
    }

    pub(super) fn broken_gate_items_from_failures(
        &self,
        person: Option<&str>,
        as_of: u128,
        failures: Vec<AttentionRequestView>,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = Vec::new();
        for failure in failures {
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

    // The independently folding operational reader is now used only by parity controls.
    #[cfg(test)]
    pub(super) fn operational_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let (items, _) = self.operational_attention_items_with_failures(person, as_of)?;
        Ok(items)
    }

    /// Return this call's history cut for the broken-gate reducer, without caching authority.
    /// Read it at its original first-use point, after the derived fault families below.
    pub(super) fn operational_attention_items_with_failures(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<(Vec<AttentionItemView>, Vec<AttentionRequestView>)> {
        let connection = self.readers.get();
        let mut items = self.reconcile_fault_attention(person, as_of)?;
        items.extend(self.observer_attention_items(person, as_of)?);
        items.extend(self.loop_attention_items(person, as_of)?);
        items.extend(self.missing_admission_clock_faults(person)?);
        items.extend(self.parked_runtime_attention_items(person, as_of)?);
        items.extend(self.unstarted_seat_attention_items(person, as_of)?);
        let failures = self.operational_failures()?;
        for failure in &failures {
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
            } else if claim.body["fields"]["condition"] == "stop-deferred" {
                // A retained stop belongs to a terminal run, so declaration_live deliberately
                // excludes it. Its own revision, incarnation and runtime fences decide instead.
                if !self
                    .stop_deferral_faults_for(std::slice::from_ref(source), self.index()?)?
                    .contains_key(source)
                {
                    continue;
                }
            } else if !source.starts_with("daemon/")
                && !person_work::declaration_live(&connection, source)?
            {
                continue;
            }
            let f = &claim.body["fields"];
            if f["condition"] == crate::restart::WHEN_IDLE {
                let Some(request) = crate::restart::latest_request(self, source)? else {
                    continue;
                };
                if request.body["fields"]["action"] != crate::restart::WHEN_IDLE
                    || failure.subject != crate::restart::attention_episode(&request)
                    || self
                        .operation_claim(&crate::restart::release_key(&request))?
                        .is_some()
                    || self
                        .operation_claim(&format!("agent-restart-completed:{}", request.id))?
                        .is_some()
                {
                    continue;
                }
            }
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
                let Some(context) = self.current_exhausted_work_wake(failure, as_of)? else {
                    continue;
                };
                Some(context)
            } else {
                None
            };
            let mut item = attention_item_from_failure(failure.clone());
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
            if f["condition"] == "stop-deferred" {
                item.actions = vec![attention_action(
                    "inspect retained seat",
                    &["st", "agents", "show", source, "--all"],
                )];
            }
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
        Ok((items, failures))
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
        message_id: None, answers: Vec::new(),
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

/// Whether a record with this idempotency key exists.
fn recorded(connection: &Connection, key: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_operation_index
               WHERE json_extract(body, '$._operation.id') IS NOT NULL
                 AND json_extract(body, '$._operation.id')=?1)",
        )?
        .query_row([smallclaims::store::operation_id_for_key(key)], |row| row.get(0))?)
}

/// The reason recorded with the record whose idempotency key this is, when it exists.
fn recorded_reason(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .prepare_cached(
            "SELECT json_extract(body, '$.fields.reason') FROM claims INDEXED BY claims_operation_index
               WHERE json_extract(body, '$._operation.id') IS NOT NULL
                 AND json_extract(body, '$._operation.id')=?1 LIMIT 1",
        )?
        .query_row([smallclaims::store::operation_id_for_key(key)], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .map(Option::unwrap_or_default))
}

/// The idempotency key of the record that the screen prompt `claim` recorded is gone.
pub(crate) fn screen_prompt_gone_key(claim: &str) -> String {
    format!("native-screen-prompt-gone:{claim}")
}

/// The idempotency key of the record that st pressed the keys of a person's answer to the screen
/// prompt `claim` recorded. It is written before the keys, so they are never pressed twice.
pub(crate) fn screen_prompt_delivered_key(claim: &str) -> String {
    format!("native-screen-prompt-delivered:{claim}")
}

/// The idempotency key of the record that the terminal refused a key of a person's answer to the
/// screen prompt `claim` recorded, after st recorded it delivered.
pub(crate) fn screen_prompt_undelivered_key(claim: &str) -> String {
    format!("native-screen-prompt-undelivered:{claim}")
}

impl Store {
    /// The screen-only prompt (see `st_drivers::blocking_screen::screen_prompt`) the seat's
    /// incarnation shows, as the reconciler last recorded it: the newest `native-screen-prompt`
    /// of that incarnation, unless it is gone. Its id, when it was recorded, and its fields.
    pub(crate) fn open_screen_prompt(
        &self,
        seat: &str,
        incarnation: &str,
    ) -> Result<Option<(String, u128, Value)>> {
        let connection = self.readers.get();
        open_screen_prompt(&connection, seat, incarnation)
    }

    /// Whether a record with the idempotency key `key` exists.
    pub(crate) fn operation_recorded(&self, key: &str) -> Result<bool> {
        let connection = self.readers.get();
        recorded(&connection, key)
    }

    /// The person's answer to the screen prompt `claim` recorded, and whether st pressed its keys.
    pub(crate) fn screen_prompt_answer(&self, claim: &str) -> Result<(Option<String>, bool)> {
        let connection = self.readers.get();
        Ok((
            native_prompt_answer(&connection, claim)?,
            recorded(&connection, &screen_prompt_delivered_key(claim))?,
        ))
    }

    /// The alert for the seat's open screen prompt: only while its incarnation runs, and until it
    /// is answered or gone.
    fn screen_prompt_item(
        &self,
        connection: &Connection,
        seat: &str,
        person: Option<&str>,
    ) -> Result<Option<AttentionItemView>> {
        let runtime = connection
            .prepare_cached(&format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.body", "runtime.observed")
            ))?
            .query_row(params![seat, i64::MAX], |row| row.get::<_, String>(0))
            .optional()?;
        let Some(runtime) = runtime else {
            return Ok(None);
        };
        let runtime: Value = serde_json::from_str(&runtime)?;
        let runtime = runtime.get("fields").unwrap_or(&runtime);
        let Some(incarnation) = runtime["incarnation_id"].as_str() else {
            return Ok(None);
        };
        if runtime["status"] != "running" {
            return Ok(None);
        }
        let Some((claim, at, prompt)) = open_screen_prompt(connection, seat, incarnation)? else {
            return Ok(None);
        };
        // An answer whose keys the terminal refused part way leaves the dialog for the person to
        // finish in the terminal: it may now sit on a different option.
        let undelivered = recorded_reason(connection, &screen_prompt_undelivered_key(&claim))?;
        if (undelivered.is_none() && native_prompt_answer(connection, &claim)?.is_some())
            || !person_work::declaration_live(connection, seat)?
        {
            return Ok(None);
        }
        let Some(owner) = self.agent_person(seat)? else {
            return Ok(None);
        };
        if person.is_some_and(|person| person != owner) {
            return Ok(None);
        }
        let driver = prompt["driver"].as_str().unwrap_or_default();
        let kind = prompt["status"].as_str().unwrap_or_default();
        let headline = prompt["matched_line"].as_str().unwrap_or_default();
        // The folder a trust prompt asks about, as the reconciler recorded it.
        let folder = prompt["reason"]
            .as_str()
            .filter(|folder| !folder.is_empty())
            .map_or_else(|| "its workspace".to_owned(), |folder| format!("its workspace {folder}"));
        let ask = match (driver, kind) {
            ("claude", "mcp-server") => format!(
                "{headline}. Claude asks before it starts whether to use this project's MCP server. Use it, use all, or skip it. Use all also approves every MCP server this project adds later, without asking again"
            ),
            ("claude", "trust") => format!("Claude asks before it starts whether you trust {folder}, and st's own attempts to record that trust did not hold. Trust trusts that folder: Claude can then read, edit and execute files there. Trust it, or exit"),
            ("codex", "trust") => format!("Codex asks before it starts whether you trust the contents of {folder}. Trust trusts that folder: Codex then loads its project config, hooks and exec policies. Trust it, or quit"),
            _ => headline.to_owned(),
        };
        let (answers, detail) = match undelivered {
            Some(reason) => (
                Vec::new(),
                format!(
                    "{headline}: st did not finish pressing your answer into the dialog ({reason}), so it may now sit on a different option. Answer it in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone."
                ),
            ),
            None => (
                st_drivers::blocking_screen::screen_prompt_choices(driver, kind)
                    .iter()
                    .map(|(answer, _)| (*answer).to_owned())
                    .collect(),
                format!(
                    "{ask} here, or in the seat's terminal (Ctrl+] in stui). This alert clears when the prompt is gone, however it was answered."
                ),
            ),
        };
        Ok(Some(AttentionItemView {
            episode: claim,
            priority: "high".into(),
            kind: "harness-prompt".into(),
            review_mode: None,
            subject: seat.to_owned(),
            person: owner,
            requester_id: None,
            launch_id: None,
            variant_id: None,
            message_id: None,
            answers,
            conversation: Some(seat.to_owned()),
            title: format!("{seat} is waiting for you"),
            detail,
            request: None,
            mission: None,
            mission_run: None,
            step: None,
            targets: vec![seat.to_owned()],
            requested_at_unix_ms: at,
            actions: vec![attention_action(
                "Attach to answer",
                &["st", "terminals", "attach", seat],
            )],
        }))
    }
}

/// A native prompt's answer as a waiting hook reads it: the person's answer, once recorded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptAnswer {
    pub answer: Option<String>,
}

/// The answers to a call Claude's auto mode classifier denied: allow that call once, allow calls
/// of its kind for the seat, or leave it denied.
const DENIAL_ANSWERS: [&str; 3] = ["approve-once", "approve-kind", "dismiss"];

impl Store {
    /// The person's answer to the native prompt or denial `episode` recorded, if any.
    pub fn prompt_answer(&self, episode: &str) -> Result<PromptAnswer> {
        let connection = self.readers.get();
        Ok(PromptAnswer {
            answer: native_prompt_answer(&connection, episode)?,
        })
    }

    /// The calls Claude's auto mode classifier denied on `seat` since its running incarnation
    /// started, that its person has not answered: each an alert, in the seat's conversation.
    fn denial_items(
        &self,
        connection: &Connection,
        seat: &str,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        let Some((started, _)) = running_since(connection, seat)? else {
            return Ok(Vec::new());
        };
        let denials = recent_denials(connection, seat)?;
        if denials.is_empty() || !person_work::declaration_live(connection, seat)? {
            return Ok(Vec::new());
        }
        let Some(owner) = self.agent_person(seat)? else {
            return Ok(Vec::new());
        };
        if person.is_some_and(|person| person != owner) {
            return Ok(Vec::new());
        }
        let last = last_turn_or_end(connection, seat)?;
        let mut items = Vec::new();
        for (claim, index, at, fields) in denials {
            if index < started
                || moved_on(last.as_ref(), index, at)
                || native_prompt_answer(connection, &claim)?.is_some()
            {
                continue;
            }
            let tool = fields["status"].as_str().unwrap_or("tool");
            let reason = fields["reason"].as_str().filter(|reason| !reason.is_empty()).unwrap_or("no reason given");
            items.push(AttentionItemView {
                episode: claim,
                priority: "high".into(),
                kind: "harness-prompt".into(),
                review_mode: None,
                subject: seat.to_owned(),
                person: owner.clone(),
                requester_id: None,
                launch_id: None,
                variant_id: None,
                message_id: None,
                answers: DENIAL_ANSWERS.iter().map(|answer| (*answer).to_owned()).collect(),
                conversation: Some(seat.to_owned()),
                title: format!("{seat} needs you to allow a {tool} call"),
                detail: format!(
                    "Claude's auto mode denied a {tool} call ({reason}), and only you can allow it: it is the denied call in the seat's conversation. Allow it once, allow calls of its kind for this seat, or dismiss it to leave it denied. When you allow it, st tells the seat to run it again and lets it through."
                ),
                request: None,
                mission: None,
                mission_run: None,
                step: None,
                targets: vec![seat.to_owned()],
                requested_at_unix_ms: at,
                actions: vec![attention_action(
                    "Attach to answer",
                    &["st", "terminals", "attach", seat],
                )],
            });
        }
        Ok(items)
    }

    /// Record `actor`'s answer to the call Claude's classifier denied that `episode` raised, and
    /// tell the seat. `None` when `episode` is no denial of `seat`.
    fn answer_denial(
        &self,
        seat: &str,
        episode: &str,
        answer: &str,
        actor: &str,
    ) -> std::result::Result<Option<ClaimRecord>, St3Error> {
        let internal = |error: anyhow::Error| St3Error::new("internal", format!("{error:#}"));
        let Some(denial) = self.claim_by_id(episode).map_err(internal)? else {
            return Ok(None);
        };
        let fields = &denial.body["fields"];
        if denial.subject != seat
            || denial.kind != "harness.diagnostic"
            || fields["code"] != crate::driver_hook::PERMISSION_DENIED_CODE
        {
            return Ok(None);
        }
        if !DENIAL_ANSWERS.contains(&answer) {
            return Err(St3Error::new(
                "invalid-answer",
                "answer approve-once, approve-kind or dismiss",
            ));
        }
        let connection = self.readers.get();
        let started = running_since(&connection, seat).map_err(internal)?;
        let answered = native_prompt_answer(&connection, episode).map_err(internal)?;
        let last = last_turn_or_end(&connection, seat).map_err(internal)?;
        drop(connection);
        let index = denial.store_index as i64;
        let at = denial.accepted_at_unix_ms;
        if started.is_none_or(|(started, _)| index < started)
            || moved_on(last.as_ref(), index, at)
            || answered.is_some()
        {
            return Err(St3Error::new(
                "stale-fence",
                "that call is no longer waiting: it was answered, the seat moved on, or it restarted",
            ));
        }
        let tool = fields["status"].as_str().unwrap_or("tool").to_owned();
        let recorded = self.append_claim(&ClaimInput {
            subject: seat.to_owned(),
            kind: "harness.diagnostic".into(),
            actor: Some(actor.to_owned()),
            fields: BTreeMap::from([
                ("code".into(), Value::String("native-prompt-answered".into())),
                ("status".into(), Value::String(answer.into())),
                ("driver".into(), Value::String("claude".into())),
            ]),
            evidence: vec![episode.to_owned()],
            expected_subject: None,
            idempotency_key: Some(native_prompt_answer_key(episode)),
        })?;
        // The seat stopped on the denial: say what the person decided, so it acts on it.
        let content = match answer {
            "approve-once" => format!(
                "{actor} allowed the {tool} call auto mode denied, once. Run that same call again: st lets it through this time."
            ),
            "approve-kind" => format!(
                "{actor} allowed the {tool} call auto mode denied, and calls of its kind on this seat. Run it again: st lets it and calls like it through."
            ),
            _ => format!("{actor} left the {tool} call auto mode denied as denied. Do not run it, or work around it."),
        };
        let message_key = format!("st3-denial-answer:{episode}");
        let message_id = &hex::encode(sha2::Sha256::digest(message_key.as_bytes()))[..16];
        self.append_claim(&ClaimInput {
            subject: format!("message/{message_id}"),
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("daemon/runtime".into())),
                ("to".into(), Value::String(seat.into())),
                ("content".into(), Value::String(content)),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String(format!("Your person answered the denied {tool} call"))),
                ("in_reply_to".into(), Value::Null),
                ("tags".into(), Value::Array(vec![Value::String(format!("st3-denial:{episode}"))])),
            ]),
            evidence: vec![recorded.id.clone()],
            expected_subject: None,
            idempotency_key: Some(message_key),
        })?;
        Ok(Some(recorded))
    }
}

/// Where in the store `seat`'s running incarnation was observed running, and its id; `None` when
/// it is not running. Store order, not a clock: a denial after it belongs to that incarnation.
fn running_since(connection: &Connection, seat: &str) -> Result<Option<(i64, String)>> {
    let row = connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query("claims.store_index, claims.body", "runtime.observed")
        ))?
        .query_row(params![seat, i64::MAX], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    let Some((index, body)) = row else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body)?;
    let fields = body.get("fields").unwrap_or(&body);
    if fields["status"] != "running" {
        return Ok(None);
    }
    Ok(Some((index, fields["incarnation_id"].as_str().unwrap_or_default().to_owned())))
}

/// The newest observation of `seat` that began a turn or ended its session: its store position,
/// state and when that state began. Observations of the seat at rest, which repeat as it is
/// refreshed, lie after it and are passed over.
fn last_turn_or_end(connection: &Connection, seat: &str) -> Result<Option<(i64, String, Option<u64>)>> {
    connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query("claims.store_index, claims.body", "harness.observed").replace(
                " ORDER BY",
                " AND json_extract(claims.body, '$.fields.state') IN ('working', 'ended') ORDER BY"
            )
        ))?
        .query_row(params![seat, i64::MAX], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?
        .map(|(index, body)| -> Result<(i64, String, Option<u64>)> {
            let body: Value = serde_json::from_str(&body)?;
            let fields = body.get("fields").unwrap_or(&body);
            Ok((
                index,
                fields["state"].as_str().unwrap_or_default().to_owned(),
                fields["observed_since_ms"].as_u64(),
            ))
        })
        .transpose()
}

/// Whether the seat moved on after its hook raised a denial (at store position `index`, accepted
/// at `at`), given its newest turn or end (see [`last_turn_or_end`]): it began another turn, or its
/// session ended. The denied call then waits on nobody, so its alert goes. The `Stop` hook records
/// the seat's state before it raises the denial, so the turn the denial ended, though it may still
/// be observed working for a moment, began before it.
fn moved_on(last: Option<&(i64, String, Option<u64>)>, index: i64, at: u128) -> bool {
    last.is_some_and(|(observed, state, since)| {
        *observed > index
            && (state == "ended" || since.is_some_and(|since| u128::from(since) > at))
    })
}

/// The newest few denials `seat`'s hooks raised: their claims, store positions, acceptance times
/// and fields.
fn recent_denials(connection: &Connection, seat: &str) -> Result<Vec<(String, i64, u128, Value)>> {
    connection
        .prepare_cached(&format!(
            "{} LIMIT 8",
            newest_claims_of_kind_query(
                "claims.id, claims.store_index, claims.accepted_at_unix_ms, claims.body",
                "harness.diagnostic"
            )
            .replace(
                " ORDER BY",
                " AND json_extract(claims.body, '$.fields.code')='permission-denied' ORDER BY"
            )
        ))?
        .query_map(params![seat, i64::MAX], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .map(|row| -> Result<(String, i64, u128, Value)> {
            let (claim, index, at, body) = row?;
            let body: Value = serde_json::from_str(&body)?;
            Ok((claim, index, at.parse()?, body.get("fields").cloned().unwrap_or(body)))
        })
        .collect()
}

fn open_screen_prompt(
    connection: &Connection,
    seat: &str,
    incarnation: &str,
) -> Result<Option<(String, u128, Value)>> {
    let row = connection
        .prepare_cached(&format!(
            "{} LIMIT 1",
            newest_claims_of_kind_query(
                "claims.id, claims.accepted_at_unix_ms, claims.body",
                "harness.diagnostic"
            )
            .replace(
                " ORDER BY",
                " AND json_extract(claims.body, '$.fields.code')='native-screen-prompt' ORDER BY"
            )
        ))?
        .query_row(params![seat, i64::MAX], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?;
    let Some((claim, at, body)) = row else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body)?;
    let fields = body.get("fields").cloned().unwrap_or(body);
    if fields["incarnation_id"] != incarnation || recorded(connection, &screen_prompt_gone_key(&claim))? {
        return Ok(None);
    }
    Ok(Some((claim, at.parse()?, fields)))
}

/// The idempotency key of the record that a native prompt opened by `observation` is gone.
pub(crate) fn native_prompt_gone_key(observation: &str) -> String {
    format!("native-prompt-gone:{observation}")
}

fn native_prompt_gone_operation(observation: &str) -> String {
    smallclaims::store::operation_id_for_key(&native_prompt_gone_key(observation))
}

/// How long a Claude permission hook waits for a person's answer from a client. The hook is
/// Claude's only way to receive it, so a prompt that has waited longer is answered in the
/// terminal: an answer recorded later would reach nobody. Claude ends the hook at its registered
/// timeout (`hooks::claude_settings_registration`, a minute longer), which is part of every Claude
/// seat's declaration: changing it restarts every seat on upgrade.
pub(crate) const CLAUDE_PROMPT_ANSWER_WAIT_MS: u128 = 10 * 60 * 1000;

/// Whether the prompt `harness` reports is of a kind st answers from a client: a permission prompt of
/// Claude, whose hook waits for the answer, that names its tool; or of Codex, whose control
/// connection sends it, that names the request it saw (`commandExecution`, `fileChange`,
/// `permissions`). A Codex wait known only from its thread's `waitingOnApproval` flag has no
/// request st could answer. The call's input is conversation content a client reads from the
/// seat's conversation and shows beside the answers; it is never stored here. Other prompts are
/// answered in the seat's terminal.
///
/// A Claude question (`AskUserQuestion`) is answered too: its hook returns the person's choices
/// as the tool's answers (measured on Claude Code 2.1.296, multi-select included).
fn answerable_kind(harness: &Value) -> bool {
    let reason = harness["reason"].as_str().unwrap_or_default();
    harness["blocked_on"] == "human"
        && match (harness["driver"].as_str(), harness["ask"].as_str()) {
            (Some("claude"), Some("permission")) => {
                !reason.is_empty() && reason != "permissionRequest"
            }
            (Some("claude"), Some("question")) => reason == "AskUserQuestion",
            (Some("codex"), Some("permission")) => {
                matches!(reason, "commandExecution" | "fileChange" | "permissions")
            }
            _ => false,
        }
}

/// The answers a client may send to the prompt `harness` reports: `allow` or `deny` to a
/// permission; to a question, `answer` (its choices, see [`question_answers`]) or `deny`. None
/// when the prompt is answered in the terminal.
///
/// Claude's plan approval (`ExitPlanMode`) takes `approve` or `deny`: its hook can refuse a plan,
/// which Claude reads as the person's feedback, but its allow does not approve one (measured on
/// Claude Code 2.1.296), so st approves it with the dialog's own keys (see
/// `Reconciler::reconcile_plan_approval`).
fn prompt_answers(harness: &Value, now: u128) -> Vec<String> {
    if !answerable(harness, now) {
        return Vec::new();
    }
    // A subagent's call is not in the seat's conversation, where a client shows the call beside
    // an allow: st offers only deny for it, never an allow of a call the person cannot see.
    if subagent_prompt(harness) {
        return vec!["deny".into()];
    }
    let first = if harness["ask"] == "question" {
        "answer"
    } else if plan_approval(harness) {
        "approve"
    } else {
        "allow"
    };
    vec![first.into(), "deny".into()]
}

/// Whether the prompt `harness` reports is Claude asking to approve its plan.
pub(crate) fn plan_approval(harness: &Value) -> bool {
    harness["driver"] == "claude" && harness["ask"] == "permission" && harness["reason"] == "ExitPlanMode"
}

/// The most a question's choices may hold: a few indices, so a replicated record stays small.
const QUESTION_ANSWERS_MAX_BYTES: usize = 1024;
/// The most questions, and options per question, a choice may name.
const QUESTION_INDEX_LIMIT: u64 = 64;

/// A question's choices as a client sends them: a JSON object of each question's position in the
/// pending `AskUserQuestion` call to the positions of the options chosen (several for a
/// multi-select), such as `{"0":[1],"1":[0,2]}`. Positions only: the questions and labels are
/// conversation content, which stays in the conversation; the waiting hook turns positions back
/// into labels from the call it holds. A person's own words are given in the terminal.
fn question_answers(answer: &str) -> Option<serde_json::Map<String, Value>> {
    if answer.len() > QUESTION_ANSWERS_MAX_BYTES {
        return None;
    }
    let Ok(Value::Object(answers)) = serde_json::from_str::<Value>(answer) else {
        return None;
    };
    let index = |value: &Value| value.as_u64().filter(|index| *index < QUESTION_INDEX_LIMIT);
    (!answers.is_empty()
        && answers.iter().all(|(question, options)| {
            question.parse::<u64>().is_ok_and(|question| question < QUESTION_INDEX_LIMIT)
                && options.as_array().is_some_and(|options| {
                    !options.is_empty()
                        // Every position is in range and none repeats.
                        && options.iter().filter_map(index).collect::<std::collections::BTreeSet<_>>().len() == options.len()
                })
        }))
    .then_some(answers)
}

/// What a Codex approval request asks for, by the reason its observation names.
fn codex_approval_words(reason: &str) -> &'static str {
    match reason {
        "commandExecution" => "to run a command",
        "fileChange" => "to change files",
        "permissions" => "for more permissions",
        _ => "for approval",
    }
}

/// Whether the prompt `harness` reports is a Claude subagent's: its call is not in the seat's
/// conversation, so st answers it only with deny.
fn subagent_prompt(harness: &Value) -> bool {
    harness["driver"] == "claude"
        && harness["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with(st_drivers::claude_prompts::SUBAGENT_PREFIX))
}

/// Whether st can answer the prompt `harness` reports from a client at `now`: one of a kind it
/// answers ([`answerable_kind`]) whose answer can still be delivered. A Claude prompt is answered
/// through its permission hook, which waits [`CLAUDE_PROMPT_ANSWER_WAIT_MS`] from when the prompt
/// opened (`observed_since_ms`, kept while the seat stays blocked on it).
fn answerable(harness: &Value, now: u128) -> bool {
    answerable_kind(harness)
        && (harness["driver"] != "claude"
            || harness["observed_since_ms"].as_u64().is_none_or(|since| {
                now.saturating_sub(u128::from(since)) < CLAUDE_PROMPT_ANSWER_WAIT_MS
            }))
}

/// The idempotency key of the person's answer to the native prompt `observation` opened.
fn native_prompt_answer_key(observation: &str) -> String {
    format!("native-prompt-answer:{observation}")
}

/// Decode the shared diagnostic payload for both recent and durable answer lookups.
fn native_prompt_answer_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Option<String>> {
    let status = row.get::<_, Option<String>>(offset)?;
    // A question's answer is its choices, which the reason holds.
    match status.as_deref() {
        Some("answer") => row.get(offset + 1),
        _ => Ok(status),
    }
}

/// The answer recorded for the prompt `observation` opened, if any.
fn native_prompt_answer(connection: &Connection, observation: &str) -> Result<Option<String>> {
    let operation = smallclaims::store::operation_id_for_key(&native_prompt_answer_key(observation));
    Ok(connection
        .prepare_cached(
            "SELECT json_extract(body, '$.fields.status'), json_extract(body, '$.fields.reason')
               FROM claims INDEXED BY claims_operation_index
             WHERE json_extract(body, '$._operation.id') IS NOT NULL
               AND json_extract(body, '$._operation.id')=?1 LIMIT 1",
        )?
        .query_row([operation], |row| native_prompt_answer_from_row(row, 0))
        .optional()?
        .flatten())
}

/// Fold the native prompt's terminal-only release into the shared harness view.
/// Driver heartbeats can restate the old blocked file after Esc/No; their
/// unchanged episode must not resurrect a resolved hold in sender status/cards.
pub(super) fn resolve_native_harness_at(
    connection: &Connection,
    subject: &str,
    index: u64,
    view: &mut crate::model::CurrentHarnessView,
) -> Result<()> {
    if view.driver.as_deref() != Some("claude")
        || view.blocked_on.as_deref() != Some("human")
        || !matches!(view.ask.as_deref(), Some("permission" | "question" | "review"))
    {
        return Ok(());
    }
    let Some(claim) = claim_by_id_tx(connection, &view.claim)? else { return Ok(()); };
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let Some((ownership, transition)) = fields["ownership_sequence"].as_u64()
        .zip(fields["transition_sequence"].as_u64()) else { return Ok(()); };
    // A Smalltalk answer is a request for the permission hook, not proof that
    // Claude received it. The hook may already have exhausted its polling wait.
    if let Some((NativePromptState::Gone, at)) = native_prompt_sequence_resolution(connection, subject, ownership, transition, index)? {
        view.state = "idle".into();
        view.blocked_on = None;
        view.ask = None;
        view.reason = None;
        view.since_unix_ms = at;
        view.observed_at_unix_ms = view.observed_at_unix_ms.max(at);
    }
    Ok(())
}

/// An episode's release stays durable even after more than sixteen heartbeats.
/// Ownership/transition sequences identify that exact driver episode, so older
/// answers cannot release a successor prompt, even with the same tool/reason.
fn native_prompt_sequence_resolution(
    connection: &Connection,
    subject: &str,
    ownership: u64,
    transition: u64,
    index: u64,
) -> Result<Option<(NativePromptState, u128)>> {
    let mut observations = connection.prepare_cached(&canonical_sql(
        "SELECT claims.id FROM claims INDEXED BY claims_harness_episode_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='harness.observed'
           AND claims.store_index<=?2
           AND json_extract(claims.body,'$.fields.ownership_sequence')=?3
           AND json_extract(claims.body,'$.fields.transition_sequence')=?4
         ORDER BY CANONICAL_DESC(claims)",
    ))?;
    // Native closure is terminal for this episode. A delayed answer request
    // cannot supersede Gone, even when it arrives later in canonical order.
    // Operation identity distinguishes closure from an answer: question choices
    // are answer payloads too, not just the permission words "allow" and "deny".
    let mut resolutions = connection.prepare_cached(&canonical_sql(
        "SELECT json_extract(body, '$._operation.id')=?1,
           json_extract(body, '$.fields.status'), json_extract(body, '$.fields.reason'),
           accepted_at_unix_ms
         FROM claims INDEXED BY claims_operation_index
         WHERE json_extract(body, '$._operation.id') IS NOT NULL
           AND json_extract(body, '$._operation.id') IN (?1,?2)
           AND store_index<=?3
         ORDER BY (json_extract(body, '$._operation.id')=?1) DESC,
           CANONICAL_DESC(claims) LIMIT 1",
    ))?;
    let mut requested_answer = None;
    for claim in observations.query_map(params![subject, index, ownership, transition], |row| row.get::<_, String>(0))? {
        smallclaims::read_budget::check()?;
        let claim = claim?;
        let gone = native_prompt_gone_operation(&claim);
        let answer = smallclaims::store::operation_id_for_key(&native_prompt_answer_key(&claim));
        let resolved: Option<(bool, Option<String>, String)> = resolutions
            .query_row(params![gone, answer, index], |row| {
                Ok((row.get(0)?, native_prompt_answer_from_row(row, 1)?, row.get(3)?))
            })
            .optional()?;
        if let Some((gone, answer, at)) = resolved {
            let state = match (gone, answer) {
                (false, Some(answer)) => NativePromptState::Answered { answer },
                _ => NativePromptState::Gone,
            };
            let resolution = (state, at.parse()?);
            if matches!(resolution.0, NativePromptState::Gone) {
                return Ok(Some(resolution));
            }
            requested_answer.get_or_insert(resolution);
        }
    }
    Ok(requested_answer)
}

/// What a waiting prompt hook needs to know: whether the seat's prompt is still open, answered
/// (and how), or gone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum NativePromptState {
    /// Not yet published, or still waiting on a person.
    Open,
    /// The person recorded an answer for a live permission hook to apply.
    /// This is not proof the hook received it or the native dialog closed.
    Answered { answer: String },
    /// Answered in the terminal, replaced or ended: nothing will arrive.
    Gone,
}

impl Store {
    /// The newest harness observation of `seat` and when it was accepted.
    fn newest_harness_observation(&self, seat: &str) -> Result<Option<(String, u128, Value)>> {
        let connection = self.readers.get();
        let row = connection
            .prepare_cached(&format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query(
                    "claims.id, claims.accepted_at_unix_ms, claims.body",
                    "harness.observed"
                )
            ))?
            .query_row(params![seat, i64::MAX], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .optional()?;
        row.map(|(id, at, body)| -> Result<(String, u128, Value)> {
            let body: Value = serde_json::from_str(&body)?;
            Ok((id, at.parse()?, body.get("fields").cloned().unwrap_or(body)))
        })
        .transpose()
    }

    /// The prompt a hook waits on: the very observation it published, named by the state
    /// record's ownership and transition sequences. Open until that observation is published and
    /// while it is the seat's newest; gone once the seat has moved on.
    pub fn native_prompt_state(
        &self,
        seat: &str,
        ownership: u64,
        transition: u64,
    ) -> Result<NativePromptState> {
        let connection = self.readers.get();
        // The hook's observation is among the seat's last few: it is published moments after the
        // hook writes it, and a hook that waited past a few later observations has nothing left.
        let recent = connection
            .prepare_cached(&format!(
                "{} LIMIT 16",
                newest_claims_of_kind_query("claims.id, claims.body", "harness.observed")
            ))?
            .query_map(params![seat, i64::MAX], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let own = recent.iter().rposition(|(_, body)| {
            serde_json::from_str::<Value>(body).is_ok_and(|body| {
                let fields = body.get("fields").unwrap_or(&body);
                fields["ownership_sequence"] == ownership && fields["transition_sequence"] == transition
                    && fields["blocked_on"] == "human"
                    && matches!(fields["ask"].as_str(), Some("permission" | "question" | "review"))
                    && !matches!(fields["state"].as_str(), Some("indeterminate" | "unknown"))
            })
        });
        let Some(own) = own else {
            // A full history window does not prove a just-written prompt is gone:
            // the driver file can precede publication of its daemon observation.
            // Only a later ownership/transition sequence proves it was superseded.
            let superseded = recent.iter().any(|(_, body)| {
                serde_json::from_str::<Value>(body).is_ok_and(|body| {
                    let fields = body.get("fields").unwrap_or(&body);
                    matches!(fields["state"].as_str(), Some("ready" | "working" | "idle" | "ended"))
                        && fields["ownership_sequence"].as_u64()
                            .zip(fields["transition_sequence"].as_u64())
                            .is_some_and(|sequence| sequence > (ownership, transition))
                })
            });
            return Ok(if superseded {
                NativePromptState::Gone
            } else {
                NativePromptState::Open
            });
        };
        // A delivery channel also waits on questions and reviews, even though only
        // permission prompts can be answered through st. Do not confuse a later
        // prompt (or runtime) with a restatement of the waiting episode.
        let own_body: Value = serde_json::from_str(&recent[own].1)?;
        let own_fields = own_body.get("fields").unwrap_or(&own_body);
        let mut chain = Vec::new();
        let mut newer: Option<Value> = None;
        for (claim, body) in &recent[..=own] {
            let body: Value = serde_json::from_str(body)?;
            let fields = body.get("fields").unwrap_or(&body);
            // Staleness can publish an indeterminate snapshot with unchanged
            // episode counters. Neither it nor unknown axes prove native closure.
            if matches!(fields["state"].as_str(), Some("indeterminate" | "unknown"))
                || fields["blocked_on"] == "unknown"
                || fields["ask"] == "unknown"
            {
                continue;
            }
            if fields["blocked_on"] != "human"
                || !matches!(fields["ask"].as_str(), Some("permission" | "question" | "review"))
                || fields["incarnation_id"] != own_fields["incarnation_id"]
                || fields["ask"] != own_fields["ask"]
                || fields["reason"] != own_fields["reason"]
                || fields["state"] == "ended"
            {
                return Ok(NativePromptState::Gone);
            }
            if let Some(newer) = &newer
                && newer["transition_sequence"] != fields["transition_sequence"]
                && newer["provider_auth"] == fields["provider_auth"]
            {
                return Ok(NativePromptState::Gone);
            }
            newer = Some(fields.clone());
            chain.push(claim.clone());
        }
        let mut requested_answer = None;
        for claim in &chain {
            // Refused in the terminal, which only the screen showed.
            let refused: bool = connection
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_operation_index
                       WHERE json_extract(body, '$._operation.id') IS NOT NULL
                         AND json_extract(body, '$._operation.id')=?1)",
                )?
                .query_row([native_prompt_gone_operation(claim)], |row| row.get(0))?;
            if refused {
                return Ok(NativePromptState::Gone);
            }
            if let Some(answer) = native_prompt_answer(&connection, claim)? {
                requested_answer.get_or_insert(NativePromptState::Answered { answer });
            }
        }
        let durable = native_prompt_sequence_resolution(&connection, seat, ownership, transition, i64::MAX as u64)?
            .map(|(state, _)| state);
        if matches!(durable, Some(NativePromptState::Gone)) {
            return Ok(NativePromptState::Gone);
        }
        Ok(requested_answer.or(durable).unwrap_or(NativePromptState::Open))
    }

    /// Record `actor`'s answer to the seat's open screen prompt, when `episode` names it: one of
    /// that prompt's own answers, once. The reconciler presses its keys. `None` when `episode` is
    /// not the seat's open screen prompt.
    fn answer_screen_prompt(
        &self,
        seat: &str,
        episode: &str,
        answer: &str,
        actor: &str,
    ) -> std::result::Result<Option<ClaimRecord>, St3Error> {
        let internal = |error: anyhow::Error| St3Error::new("internal", format!("{error:#}"));
        let Some(incarnation) = self
            .latest_actual_value(seat)
            .map_err(internal)?
            .and_then(|runtime| {
                runtime
                    .pointer("/fields/incarnation_id")
                    .or_else(|| runtime.get("incarnation_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        else {
            return Ok(None);
        };
        let Some((claim, _, prompt)) = self
            .open_screen_prompt(seat, &incarnation)
            .map_err(internal)?
            .filter(|(claim, _, _)| claim == episode)
        else {
            return Ok(None);
        };
        let driver = prompt["driver"].as_str().unwrap_or_default();
        let choices = st_drivers::blocking_screen::screen_prompt_choices(
            driver,
            prompt["status"].as_str().unwrap_or_default(),
        );
        if !choices.iter().any(|(choice, _)| *choice == answer) {
            return Err(St3Error::new(
                "invalid-answer",
                format!(
                    "answer {}",
                    choices
                        .iter()
                        .map(|(choice, _)| *choice)
                        .collect::<Vec<_>>()
                        .join(" or ")
                ),
            ));
        }
        self.append_claim(&ClaimInput {
            subject: seat.to_owned(),
            kind: "harness.diagnostic".into(),
            actor: Some(actor.to_owned()),
            fields: BTreeMap::from([
                ("code".into(), Value::String("native-prompt-answered".into())),
                ("status".into(), Value::String(answer.into())),
                ("driver".into(), Value::String(driver.into())),
                ("incarnation_id".into(), Value::String(incarnation)),
            ]),
            evidence: vec![claim.clone()],
            expected_subject: None,
            idempotency_key: Some(native_prompt_answer_key(&claim)),
        })
        .map(Some)
    }

    /// Record `actor`'s answer to the native prompt `episode` (the observation that opened it) of
    /// `seat`. Only the seat's person answers, only the prompt still waiting, and only once.
    pub fn answer_native_prompt(
        &self,
        seat: &str,
        episode: &str,
        answer: &str,
        actor: &str,
    ) -> std::result::Result<ClaimRecord, St3Error> {
        let internal = |error: anyhow::Error| St3Error::new("internal", format!("{error:#}"));
        let now = now_ms();
        if self.agent_person(seat).map_err(internal)?.as_deref() != Some(actor) {
            return Err(St3Error::new(
                "forbidden",
                "only the seat's person answers its prompts",
            ));
        }
        if let Some(answered) = self.answer_screen_prompt(seat, episode, answer, actor)? {
            return Ok(answered);
        }
        if let Some(answered) = self.answer_denial(seat, episode, answer, actor)? {
            return Ok(answered);
        }
        if !matches!(answer, "allow" | "deny" | "approve") && question_answers(answer).is_none() {
            return Err(St3Error::new(
                "invalid-answer",
                "answer allow or deny, or a question's choices as a JSON object of question position to option positions",
            ));
        }
        let current = self.newest_harness_observation(seat).map_err(internal)?;
        let Some((claim, _, harness)) = current.filter(|(claim, _, _)| claim == episode) else {
            return Err(St3Error::new(
                "stale-fence",
                "that prompt is no longer waiting: it was answered or replaced",
            ));
        };
        if !answerable(&harness, now) {
            return Err(St3Error::new(
                "stale-fence",
                "that prompt can only be answered in the seat's terminal",
            ));
        }
        if answer == "allow" && subagent_prompt(&harness) {
            return Err(St3Error::new(
                "invalid-answer",
                "a subagent's call is not shown in the seat's conversation: deny it here, or allow it in the terminal",
            ));
        }
        // A question takes its choices or a refusal; a permission, allow or deny.
        let question = harness["ask"] == "question";
        let choices = (question && answer != "deny").then(|| question_answers(answer)).flatten();
        let valid = if question {
            answer == "deny" || choices.is_some()
        } else if plan_approval(&harness) {
            matches!(answer, "approve" | "deny")
        } else {
            matches!(answer, "allow" | "deny")
        };
        if !valid {
            return Err(St3Error::new(
                "invalid-answer",
                if question {
                    "answer this question with its choices, or deny it"
                } else {
                    "answer allow or deny"
                },
            ));
        }
        let incarnation = harness["incarnation_id"].as_str().unwrap_or_default();
        let driver = harness["driver"].as_str().unwrap_or_default();
        let mut fields = BTreeMap::from([
            ("code".into(), Value::String("native-prompt-answered".into())),
            ("status".into(), Value::String(answer.into())),
            ("driver".into(), Value::String(driver.into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ]);
        // A question's choices are positions, never the questions' or options' words: they ride
        // as the reason, and the status says only that it was answered.
        if let Some(choices) = choices {
            fields.insert("status".into(), Value::String("answer".into()));
            fields.insert("reason".into(), Value::Object(choices).to_string().into());
        }
        self.append_claim(&ClaimInput {
            subject: seat.to_owned(),
            kind: "harness.diagnostic".into(),
            actor: Some(actor.to_owned()),
            fields,
            evidence: vec![claim.clone()],
            expected_subject: None,
            idempotency_key: Some(native_prompt_answer_key(&claim)),
        })
    }
}

#[cfg(test)]
mod person_steps_plan_tests {
    use super::*;

    #[test]
    fn the_person_step_read_is_a_search_of_the_assignee_index_not_a_scan() {
        let store = Store::open_memory("alder").unwrap();
        let connection = store.readers.get();
        let plan: Vec<String> = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {PERSON_STEPS_QUERY}"))
            .unwrap()
            .query_map([None::<String>], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.starts_with("SEARCH step_runs") && line.contains("step_runs_assignee_index")),
            "the person-step read must search step_runs_assignee_index: {plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.starts_with("SCAN step_runs")),
            "the person-step read must not scan step_runs: {plan:?}"
        );
    }
}

#[cfg(test)]
mod native_prompt_tests {
    use super::*;

    #[test]
    fn choice_answers_remain_answered_and_native_closure_wins_over_delayed_answers() {
        let store = Store::open_memory("node").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
            workspace.path().display().to_string()
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let preview = store
            .mission(&intent, IntentInput { kdl: source, source_name: None })
            .unwrap();
        store
            .apply_as(&intent, &preview.subject_tokens, "answer-seat", Some("person/avery"))
            .unwrap();
        let seat = "agent/node.worker";
        let append = |kind: &str, fields: Value, key: Option<String>| {
            store
                .append_claim(&ClaimInput {
                    subject: seat.into(),
                    kind: kind.into(),
                    actor: Some(seat.into()),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: key,
                })
                .unwrap()
        };
        // Unchanged live observations are retained locally, not republished to history.
        // Seed durable historical restatements explicitly: answers fence against the
        // newest replicated claim, never a local-observation receipt, and this regression
        // must put the original answer beyond the sixteen-claim history window.
        let append_harness_history = || {
            store
                .connection
                .batched(|tx| {
                    append_claim_tx(
                        tx,
                        "node",
                        seat,
                        "harness.observed",
                        Some(seat),
                        &json!({"fields": {
                            "state":"working", "driver":"claude", "incarnation_id":"one",
                            "blocked_on":"human", "ask":"question", "reason":"AskUserQuestion",
                            "ownership_sequence":1, "transition_sequence":3
                        }}),
                        &[],
                        None,
                    )
                })
                .unwrap()
                .unwrap()
        };
        append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}), None);
        let first = append_harness_history();
        let choice = r#"{"0":[1],"1":[0,2]}"#;
        let answered = store
            .answer_native_prompt(seat, &first.id, choice, "person/avery")
            .unwrap();
        assert_eq!(answered.body["fields"]["status"], "answer");
        assert_eq!(answered.body["fields"]["reason"], choice);
        let mut restated = append_harness_history();
        for _ in 0..20 {
            restated = append_harness_history();
        }
        let restatements: u64 = store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM claims
                 WHERE subject=?1 AND kind='harness.observed'
                   AND store_index>?2 AND store_index<=?3",
                params![seat, first.store_index, restated.store_index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restatements, 21, "restatements must exceed the recent-history window");
        let resolve = |index| {
            native_prompt_sequence_resolution(&store.readers.get(), seat, 1, 3, index)
                .unwrap()
                .map(|(state, _)| state)
        };
        let expected = Some(NativePromptState::Answered { answer: choice.into() });
        assert_eq!(resolve(answered.store_index), expected);
        assert_eq!(resolve(store.index().unwrap()), expected);
        assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(),
            NativePromptState::Answered { answer: choice.into() });
        let gone = append("harness.diagnostic", json!({
            "code":"native-prompt-gone", "status":"resolved",
            "driver":"claude", "incarnation_id":"one"
        }), Some(native_prompt_gone_key(&first.id)));
        store
            .answer_native_prompt(seat, &restated.id, choice, "person/avery")
            .unwrap();
        assert_eq!(resolve(gone.store_index), Some(NativePromptState::Gone));
        assert_eq!(resolve(store.index().unwrap()), Some(NativePromptState::Gone));
        assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Gone);
        assert_eq!(resolve(answered.store_index), expected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClaimInput, PersonStepResponse};

    fn step_back_in_time(store: &Store, subject: &str, accepted_at: &str) {
        store
            .connection
            .lock().expect("fixture writer")
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE subject=?1 AND kind='work.person-asked'",
                params![subject, accepted_at],
            )
            .unwrap();
    }

    /// The collection snapshot's visible rows must equal a direct read at the same cut, in
    /// the same order, for every person and boundary.
    fn parity(store: &Store, person: Option<&str>, now: u128) {
        let (rows, _) = store.attention_collection_snapshot(person, now).unwrap();
        let direct = store.attention_snapshot(person, now).unwrap();
        let keys = |items: &[AttentionItemView]| {
            items
                .iter()
                .map(|item| {
                    (
                        item.subject.clone(),
                        item.episode.clone(),
                        item.requested_at_unix_ms,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&rows), keys(&direct), "{person:?} at {now}");
    }

    /// A person ask joins the collection at its asking claim's acceptance, never at the
    /// waiting time it names: a migrated ask can name a time before it was asked. Fault-only
    /// clocks (observer state, reconcile faults) never move this collection.
    #[test]
    fn person_ask_collection_changes_only_at_the_asks_admission() {
        let (store, origin, input) = super::super::person_work::tests::fixture();
        let ask = store.ask_person(&input).unwrap();
        let admitted = now_ms() + 60_000;
        store
            .connection
            .lock().expect("fixture writer")
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2,
                        body=json_set(body, '$.fields.waiting_since', ?3)
                 WHERE subject=?1 AND kind='work.person-asked'",
                params![
                    ask.subject,
                    admitted.to_string(),
                    (admitted - 3_600_000).to_string()
                ],
            )
            .unwrap();

        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), admitted - 1)
                .unwrap()
                .1,
            Some(admitted)
        );
        assert_eq!(
            store
                .attention_collection_snapshot(None, admitted - 1)
                .unwrap()
                .1,
            Some(admitted)
        );
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/robin"), admitted - 1)
                .unwrap()
                .1,
            None
        );
        parity(&store, Some("person/avery"), admitted - 1);
        parity(&store, None, admitted - 1);
        parity(&store, Some("person/robin"), admitted - 1);
        assert!(store
            .attention_snapshot(Some("person/avery"), admitted - 1)
            .unwrap()
            .iter()
            .all(|item| item.subject != ask.subject));
        let shown = store.attention_snapshot(Some("person/avery"), admitted).unwrap();
        let item = shown
            .iter()
            .find(|item| item.subject == ask.subject)
            .expect("the ask joins the collection at its admission");
        parity(&store, Some("person/avery"), admitted);

        // Fault-collection clocks are not this collection's inputs.
        for (subject, kind, fields) in [
            (
                "observer/issues",
                "observer.state",
                json!({"state": "degraded", "reason": "limited"}),
            ),
            (
                origin.subject.as_str(),
                "reconcile.fault",
                json!({"scope": "st3.test", "status": "faulted", "reason": "stuck"}),
            ),
        ] {
            store
                .connection
                .batched(|tx| -> Result<()> {
                    append_claim_tx(
                        tx,
                        "alder",
                        subject,
                        kind,
                        Some("agent/alder.asker"),
                        &json!({"fields": fields}),
                        &[],
                        None,
                    )?;
                    Ok(())
                })
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), admitted - 1)
                .unwrap()
                .1,
            Some(admitted)
        );
        assert_eq!(
            store.attention_collection_snapshot(None, admitted - 1).unwrap().1,
            Some(admitted)
        );
        parity(&store, Some("person/avery"), admitted - 1);
        parity(&store, None, admitted - 1);

        step_back_in_time(&store, &ask.subject, &(now_ms() - 1_000).to_string());
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), now_ms())
                .unwrap()
                .1,
            None
        );
        parity(&store, Some("person/avery"), now_ms());
        store
            .finish_person_step(
                &PersonStepResponse {
                    delegation: None,
                    subject: ask.subject.clone(),
                    actor: "person/avery".into(),
                    summary: "Friday".into(),
                    evidence: vec![],
                    episode: Some(item.episode.clone()),
                    idempotency_key: "fence-response".into(),
                    answer: None,
                },
                false,
            )
            .unwrap();
        assert!(store
            .attention_snapshot(Some("person/avery"), now_ms())
            .unwrap()
            .iter()
            .all(|item| item.subject != ask.subject));
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), now_ms())
                .unwrap()
                .1,
            None
        );
        parity(&store, Some("person/avery"), now_ms());
    }

    /// A ready person step without an ask joins at its activation, and falls back to its
    /// creation when it was never separately activated.
    #[test]
    fn ready_person_step_joins_at_its_activation_and_falls_back_to_its_creation() {
        let (store, origin, _input) = super::super::person_work::tests::fixture();
        let review = format!(
            "step-run/{}/review",
            origin.generation.trim_start_matches("run-generation/")
        );
        store.set_step_state(&review, "ready", None).unwrap();
        let activated = now_ms() + 120_000;
        store
            .connection
            .lock().expect("fixture writer")
            .execute(
                "UPDATE step_runs SET activated_at_unix_ms=?2 WHERE subject=?1",
                params![review.as_str(), activated.to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), activated - 1)
                .unwrap()
                .1,
            Some(activated)
        );
        parity(&store, Some("person/avery"), activated - 1);
        assert!(store
            .attention_snapshot(Some("person/avery"), activated - 1)
            .unwrap()
            .iter()
            .all(|item| item.subject != review));
        assert!(
            store
                .attention_snapshot(Some("person/avery"), activated)
                .unwrap()
                .iter()
                .any(|item| item.subject == review && item.kind == "person-step")
        );
        parity(&store, Some("person/avery"), activated);
        store
            .connection
            .lock().expect("fixture writer")
            .execute(
                "UPDATE step_runs SET activated_at_unix_ms=NULL WHERE subject=?1",
                params![review.as_str()],
            )
            .unwrap();
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/avery"), now_ms())
                .unwrap()
                .1,
            None
        );
        parity(&store, Some("person/avery"), now_ms());
        assert!(
            store
                .attention_snapshot(Some("person/avery"), now_ms())
                .unwrap()
                .iter()
                .any(|item| item.subject == review)
        );
    }

    /// Registered custom attention is admitted with its request, so an active custom card
    /// leaves no future change, and only its recipient ever sees it.
    #[test]
    fn registered_custom_attention_has_no_future_change_once_active() {
        let store = Store::open_memory("garden").unwrap();
        let manifest: st3_schema::custom::Manifest = serde_json::from_str(include_str!(
            "../../../../examples/st3/custom-review.json"
        ))
        .unwrap();
        store
            .register_custom_kind(&custom::RegistrationRequest {
                manifest,
                actor: "agent/garden/seed".into(),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "custom/garden/review/v1/fence".into(),
                kind: "custom.garden.review.v1.requested".into(),
                actor: Some("agent/garden/seed".into()),
                fields: serde_json::from_value(json!({
                    "title": "Retain the seed history?",
                    "detail": "Choose Keep or Discard.",
                    "recipient": "person/lichen"
                }))
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(
            store
                .attention_snapshot(Some("person/lichen"), now_ms())
                .unwrap()
                .iter()
                .any(|item| item.subject == "custom/garden/review/v1/fence")
        );
        assert_eq!(
            store
                .attention_collection_snapshot(Some("person/lichen"), now_ms())
                .unwrap()
                .1,
            None
        );
        assert_eq!(
            store.attention_collection_snapshot(None, now_ms()).unwrap().1,
            None
        );
        parity(&store, Some("person/lichen"), now_ms());
        parity(&store, None, now_ms());
        parity(&store, Some("person/avery"), now_ms());
        assert!(store
            .attention_snapshot(Some("person/avery"), now_ms())
            .unwrap()
            .is_empty());
    }
}
