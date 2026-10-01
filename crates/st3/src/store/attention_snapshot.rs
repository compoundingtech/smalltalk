//! Derived person work and operational episodes. Legacy attention claims are audit data only.
use super::*;
use crate::model::LoopSpec;

impl Store {
    pub(super) fn person_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let mut items = Vec::new();
        let mut query = connection.prepare(
            "SELECT subject FROM step_runs WHERE assignee LIKE 'person/%'
            AND status IN ('ready','pending') AND (?1 IS NULL OR assignee=?1) ORDER BY subject",
        )?;
        let subjects = query
            .query_map([person], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for subject in subjects {
            let Some(view) = person_work::step(&connection, &subject)? else {
                continue;
            };
            let ask = person_work::request(&connection, &subject)?;
            if let Some(ask) = &ask {
                if !person_work::current(&connection, ask, as_of)? {
                    continue;
                }
            } else if view.status != "ready"
                || !person_work::run_live(&connection, &view.run, Some(&view.generation), false)?
            {
                continue;
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
                [&subject],
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
            items.push(AttentionItemView {
                episode,
                priority: "normal".into(),
                kind: "person-step".into(),
                review_mode: None,
                subject: subject.clone(),
                person: person.into(),
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
                step: Some(subject.clone()),
                targets: vec![subject.clone()],
                requested_at_unix_ms: since,
                actions: vec![attention_action(
                    "done",
                    &[
                        "st",
                        "work",
                        "done",
                        &subject,
                        "--as",
                        person,
                        "--summary",
                        "RESPONSE",
                    ],
                )],
            });
        }
        Ok(items)
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
            if claim.body["fields"]["status"] != "faulted"
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
                message_id: None,
                title: format!("st cannot reconcile {source}"),
                detail: format!(
                    "{scope}: {}. Inspect with `st subject {source}`.",
                    claim.body["fields"]["reason"].as_str().unwrap_or_default()
                ),
                mission: None,
                mission_run: None,
                step: None,
                targets: vec![source.clone()],
                requested_at_unix_ms: claim.accepted_at_unix_ms,
                actions: vec![attention_action(
                    "inspect source",
                    &["st", "subject", &source],
                )],
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
            message_id: None,
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
            items.push(AttentionItemView {
                episode: decision.id.clone(), priority: "high".into(), kind: "fault".into(), review_mode: None,
                subject: source.clone(), person: reviewer.into(), requester_id: None, launch_id: None, variant_id: None, message_id: None,
                title: if codex { "A Codex agent stopped after repeated failures" } else { "An agent stopped after repeated runtime failures" }.into(),
                detail: format!("{source}: {}. Inspect the seat and revise its desired declaration before restarting.", decision.body["fields"]["reason"].as_str().unwrap_or("the runtime is parked")),
                mission: None, mission_run: desired.owner_run, step: None, targets: vec![source.clone()], requested_at_unix_ms: decision.accepted_at_unix_ms,
                actions: vec![attention_action("inspect source", &["st", "subject", &source])],
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
                message_id: None,
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
            });
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
            if source.starts_with("mission-run/") {
                if !person_work::run_live(&connection, source, None, true)? {
                    continue;
                }
            } else if !source.starts_with("daemon/")
                && !person_work::declaration_live(&connection, source)?
            {
                continue;
            }
            let f = &claim.body["fields"];
            if matches!(
                f["condition"].as_str(),
                Some("readiness" | "provider-auth" | "provider-trust")
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
            let mut item = attention_item_from_failure(failure);
            item.kind = "fault".into();
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
            items.push(item);
        }
        Ok(items)
    }
}
