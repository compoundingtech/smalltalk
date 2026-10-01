//! Person asks are runtime steps. The immutable asking claim supplies their ownership fence;
//! step_runs supplies their state. No separate attention record is created.
use super::*;
use crate::model::{PersonAskRequest, PersonStepResponse};

const STEP_QUERY: &str = "SELECT subject, run_id, step_path, definition_hash, status, attempt,
 assignee, available_to, agentless, title, goals, worker_reported, lease_owner,
 lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms,
 created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
 FROM step_runs WHERE subject=?1";

pub(super) fn step(connection: &Connection, subject: &str) -> Result<Option<StepRunView>> {
    Ok(connection
        .query_row(STEP_QUERY, [subject], step_run_from_row)
        .optional()?)
}

pub(super) fn request(connection: &Connection, subject: &str) -> Result<Option<ClaimRecord>> {
    Ok(connection
        .query_row(
            &canonical_sql(
                "SELECT id, store_index, batch_id, subject, kind,
        origin, actor, body, predecessors, accepted_at_unix_ms FROM claims
        WHERE subject=?1 AND kind='work.person-asked' ORDER BY CANONICAL_ASC(claims) LIMIT 1",
            ),
            [subject],
            claim_from_row,
        )
        .optional()?)
}

pub(super) fn declaration_live(connection: &Connection, subject: &str) -> Result<bool> {
    let row = current_desired_row(connection, subject)?;
    let Some(row) = row else { return Ok(false) };
    let body: Value = serde_json::from_str(&row.body)?;
    if row.kind == "stop"
        || body
            .get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| children.len() == 1 && children[0]["name"] == "stop")
    {
        return Ok(false);
    }
    if let Some(run) = row.owner_run.as_deref() {
        if !run_live(connection, run, row.owner_generation.as_deref(), false)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn run_live(
    connection: &Connection,
    run: &str,
    generation: Option<&str>,
    failure: bool,
) -> Result<bool> {
    let mut run = run.strip_prefix("mission-run/").unwrap_or(run).to_owned();
    let mut expected_generation = generation.map(str::to_owned);
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(run.clone()) {
            return Ok(false);
        }
        let header = mission_run_header_tx(connection, &run).optional()?;
        let Some(header) = header else {
            return Ok(false);
        };
        if expected_generation
            .as_ref()
            .is_some_and(|g| g != &header.generation)
            || (is_terminal_run_state(&header.status) && !(failure && header.status == "failed"))
        {
            return Ok(false);
        }
        if let Some(parent) = header.parent_step_run.as_deref() {
            let Some(parent) = step(connection, parent)? else {
                return Ok(false);
            };
            if matches!(parent.status.as_str(), "completed" | "cancelled")
                || (parent.status == "failed" && !failure)
            {
                return Ok(false);
            }
            run = parent.run.trim_start_matches("mission-run/").into();
            expected_generation = Some(parent.generation);
        } else {
            let root = mission_run_header_tx(
                connection,
                header.root_mission_run.trim_start_matches("mission-run/"),
            )
            .optional()?;
            return Ok(root.is_some_and(|root| {
                !is_terminal_run_state(&root.status) || (failure && root.status == "failed")
            }));
        }
    }
}

pub(super) fn current(connection: &Connection, ask: &ClaimRecord, as_of: u128) -> Result<bool> {
    if ask.accepted_at_unix_ms > as_of {
        return Ok(false);
    }
    let fields = &ask.body["fields"];
    let Some(view) = step(connection, &ask.subject)? else {
        return Ok(false);
    };
    if !matches!(view.status.as_str(), "pending" | "ready")
        || !run_live(connection, &view.run, Some(&view.generation), false)?
        || !declaration_live(connection, ask.actor.as_deref().unwrap_or_default())?
    {
        return Ok(false);
    }
    let requester = ask.actor.as_deref().unwrap_or_default();
    let mut declarations = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='intent.desired' ORDER BY CANONICAL_ASC(claims)"))?;
    let ask_key = canonical::claim_key(connection, &ask.id)?;
    for declaration in declarations.query_map([requester], claim_from_row)? {
        let declaration = declaration?;
        if canonical::claim_key(connection, &declaration.id)? <= ask_key {
            continue;
        }
        if declaration.body["kind"] == "stop"
            || declaration.body["desired"]
                .get("children")
                .and_then(Value::as_array)
                .is_some_and(|children| children.len() == 1 && children[0]["name"] == "stop")
        {
            return Ok(false);
        }
    }
    if let Some(declaration) = fields["requester_declaration"].as_str() {
        if current_desired_row(connection, requester)?.is_none_or(|row| row.claim_id != declaration)
        {
            return Ok(false);
        }
    }
    if let Some(owner) = fields["owner_run"].as_str() {
        if !run_live(
            connection,
            owner,
            fields["owner_generation"].as_str(),
            false,
        )? {
            return Ok(false);
        }
    }
    if let Some(origin) = fields["origin_step"].as_str() {
        let Some(origin) = step(connection, origin)? else {
            return Ok(false);
        };
        if origin.attempt as u64 != fields["origin_attempt"].as_u64().unwrap_or(0)
            || origin.status != "waiting-person"
            || origin.blocked_reason.as_deref() != Some(ask.subject.as_str())
            || origin.generation != view.generation
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Store {
    pub fn ask_person(&self, input: &PersonAskRequest) -> Result<StepRunView, St3Error> {
        if !input.person.starts_with("person/")
            || input.person.matches('/').count() != 1
            || input.person == "person/"
            || !input.actor.starts_with("agent/")
            || input.title.trim().is_empty()
            || input.reason.trim().is_empty()
            || input.idempotency_key.is_empty()
        {
            return Err(St3Error::new(
                "invalid-person-ask",
                "a person ask needs a person, title, reason and idempotency key",
            ));
        }
        if input.step.is_none() || input.new_run.is_some() {
            return self.ask_person_in_new_run(input);
        }
        self.connection.batched(|tx| {
            let origin_subject = normalize_step_run(input.step.as_deref().unwrap());
            let origin = step(tx, &origin_subject).map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-step-run", "the asking step does not exist"))?;
            let identity = serde_json::to_string(&(&origin.generation, &origin_subject, origin.attempt, &input.idempotency_key)).map_err(internal)?;
            let hash = hex::encode(Sha256::digest(identity.as_bytes()));
            let subject = format!("step-run/{}/ask-{}", generation_id_from_subject(&origin.generation), &hash[..32]);
            if let Some(existing) = request(tx, &subject).map_err(internal)? {
                if existing.actor.as_deref() != Some(input.actor.as_str())
                    || existing.body["fields"]["person"] != input.person
                    || existing.body["fields"]["title"] != input.title
                    || existing.body["fields"]["reason"] != input.reason {
                    return Err(St3Error::new("idempotency-conflict", "this ask key already names a different question"));
                }
                return step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the ask is no longer retained"));
            }
            let legacy_origin = input.legacy_request.as_ref().map(|id| -> Result<bool> {
                let legacy = tx.query_row("SELECT body FROM claims WHERE id=?1 AND kind='attention.requested' AND actor=?2", params![id, input.actor], |row| row.get::<_, String>(0)).optional()?;
                let Some(legacy) = legacy else { return Ok(false) };
                let legacy: Value = serde_json::from_str(&legacy)?;
                let accepted = canonical::claim_key(tx, id)?;
                let mut claimed = tx.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='work.claimed' ORDER BY CANONICAL_DESC(claims)"))?;
                let mut proven = false;
                for claim in claimed.query_map([&origin_subject], claim_from_row)? {
                    let claim = claim?;
                    if canonical::claim_key(tx, &claim.id)? > accepted { continue; }
                    proven = claim.actor.as_deref() == Some(input.actor.as_str()) && claim.body["fields"]["attempt"].as_u64() == Some(origin.attempt as u64);
                    break;
                }
                Ok(proven && legacy["fields"]["step"] == origin_subject && legacy["fields"]["step_attempt"].as_u64() == Some(origin.attempt as u64)
                    && matches!(origin.status.as_str(), "ready" | "blocked" | "claimed" | "working"))
            }).transpose().map_err(internal)?.unwrap_or(false);
            if step_owner_is_terminal_tx(tx, &origin_subject)?
                || !declaration_live(tx, &input.actor).map_err(internal)?
                || (!legacy_origin && (!matches!(origin.status.as_str(), "claimed" | "working")
                || origin.claimant.as_deref() != Some(input.actor.as_str())
                || origin.claim_incarnation != input.incarnation
                || origin.claim_expires_at_unix_ms.is_none_or(|expiry| expiry <= now_ms()))) {
                return Err(St3Error::new("stale-work-ask", "only the current claimant and incarnation of live work can ask a person"));
            }
            let waiting_since = input.legacy_request.as_ref().map(|id| tx.query_row("SELECT accepted_at_unix_ms FROM claims WHERE id=?1 AND kind='attention.requested'", [id], |row| row.get::<_, String>(0))).transpose().map_err(internal)?;
            let evidence = input.legacy_request.iter().cloned().collect::<Vec<_>>();
            let claim = append_claim_tx(tx, &self.origin, &subject, "work.person-asked", Some(&input.actor),
                &json!({"fields": {"run": origin.run, "generation": origin.generation,
                    "origin_step": origin_subject, "origin_attempt": origin.attempt,
                    "person": input.person, "title": input.title, "reason": input.reason,
                    "key": input.idempotency_key, "attempt": 1, "status": "ready", "waiting_since": waiting_since, "legacy_request": input.legacy_request}}), &evidence, None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            let pause = append_claim_tx(tx, &self.origin, &origin_subject, "step-run.state", Some("daemon/runtime"),
                &json!({"fields": {"status": "waiting-person", "reason": subject, "attempt": origin.attempt}}), &[claim.id], None).map_err(claim_append_error)?;
            project_mission_run_update(tx, &pause)?;
            step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the ask could not be projected"))
        }).map_err(internal)?
    }

    pub fn finish_person_step(
        &self,
        input: &PersonStepResponse,
        cancel: bool,
    ) -> Result<StepRunView, St3Error> {
        self.connection.batched(|tx| {
            let subject = normalize_step_run(&input.subject);
            let mut view = step(tx, &subject).map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-step-run", "the person step does not exist"))?;
            let ask = request(tx, &subject).map_err(internal)?;
            let kind = if cancel { "work.person-cancelled" } else { "work.person-done" };
            if let Some(existing) = tx.query_row(&canonical_sql("SELECT id, store_index, batch_id, subject, kind, origin,
                actor, body, predecessors, accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind=?2
                ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![subject, kind], claim_from_row).optional().map_err(internal)? {
                if existing.body["fields"]["key"] == input.idempotency_key && existing.actor.as_deref() == Some(input.actor.as_str()) {
                    if existing.body["fields"]["summary"] != input.summary || existing.body["evidence"] != json!(input.evidence)
                        || existing.body["fields"]["attempt"].as_u64() != Some(view.attempt as u64)
                        || input.episode.as_ref().is_some_and(|episode| existing.body["fields"]["episode"] != *episode) {
                        return Err(St3Error::new("idempotency-conflict", "this response key already has another summary"));
                    }
                    return Ok(view);
                }
            }
            if cancel {
                if ask.as_ref().and_then(|a| a.actor.as_deref()) != Some(input.actor.as_str()) {
                    return Err(St3Error::new("forbidden", "only the requester can cancel its ask"));
                }
            } else if !input.actor.starts_with("person/") || view.assigned_to.as_deref() != Some(input.actor.as_str()) {
                return Err(St3Error::new("forbidden", "only the assigned person can complete this step"));
            }
            apply_effective_step_state(tx, &mut view, now_ms()).map_err(internal)?;
            if view.status != "ready"
                || ask.as_ref().is_some_and(|a| input.episode.as_ref().is_some_and(|e| e != &a.id))
                || ask.as_ref().map(|a| current(tx, a, now_ms())).transpose().map_err(internal)?.is_some_and(|live| !live) {
                return Err(St3Error::new("stale-fence", "this person step is no longer waiting in that episode"));
            }
            if ask.is_none() && input.episode.as_ref().is_some_and(|episode| episode != &format!("{}:{}:{}", view.generation, view.attempt, view.readiness_epoch)) {
                return Err(St3Error::new("stale-fence", "this authored person step has moved to another episode"));
            }
            if input.summary.trim().is_empty() {
                return Err(St3Error::new("invalid-person-response", "a response needs a summary"));
            }
            let evidence = ask.as_ref().map(|a| vec![a.id.clone()]).unwrap_or_default();
            let claim = append_claim_tx(tx, &self.origin, &subject, kind, Some(&input.actor),
                &json!({"fields": {"attempt": view.attempt, "status": if cancel { "cancelled" } else { "completed" },
                    "summary": input.summary, "key": input.idempotency_key, "episode": ask.as_ref().map(|a| a.id.clone()).unwrap_or_else(|| format!("{}:{}:{}", view.generation, view.attempt, view.readiness_epoch))}, "evidence": input.evidence}), &evidence, None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            if let Some(origin) = ask.as_ref().and_then(|a| a.body["fields"]["origin_step"].as_str()) {
                if let Some(origin_view) = step(tx, origin).map_err(internal)? {
                    if origin_view.status == "ready" {
                        let response = append_claim_tx(tx, &self.origin, origin, "step-run.state", Some("daemon/runtime"),
                            &json!({"fields": {"status": "ready", "attempt": origin_view.attempt, "readiness_epoch": origin_view.readiness_epoch,
                                "reason": format!("Person response: {}", input.summary)}}), &[claim.id], None).map_err(claim_append_error)?;
                        project_mission_run_update(tx, &response)?;
                    }
                }
            }
            step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the person step disappeared"))
        }).map_err(internal)?
    }

    fn migrate_legacy_person_asks(&self) -> Result<bool> {
        let connection = self.readers.get();
        let requests = pending_attention_requests_tx(&connection, None)?;
        let mut changed = false;
        for legacy in requests {
            if !agent_attention_requester(&legacy.actor)
                || !declaration_live(&connection, &legacy.actor)?
            {
                continue;
            }
            let imported: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM claims WHERE kind='work.person-asked' AND json_extract(body,'$.fields.legacy_request')=?1)", [&legacy.request], |row| row.get(0))?;
            if imported {
                continue;
            }
            let origin = if let Some(subject) = &legacy.step {
                let Some(origin) = step(&connection, subject)? else {
                    continue;
                };
                if legacy.step_attempt != Some(u64::from(origin.attempt))
                    || !matches!(
                        origin.status.as_str(),
                        "claimed" | "working" | "ready" | "blocked"
                    )
                {
                    continue;
                }
                Some(origin)
            } else {
                let Some(owner) = current_desired_row(&connection, &legacy.actor)? else {
                    continue;
                };
                if owner.owner_run.is_none() || owner.owner_generation.is_none() {
                    continue;
                }
                None
            };
            let input = PersonAskRequest {
                legacy_request: Some(legacy.request.clone()),
                person: legacy.reviewer,
                title: legacy.title,
                reason: legacy.reason,
                actor: legacy.actor,
                step: origin.as_ref().map(|origin| origin.subject.clone()),
                new_run: origin
                    .is_none()
                    .then(|| format!("legacy-{}", &legacy.request[..16])),
                incarnation: origin.and_then(|origin| origin.claim_incarnation),
                idempotency_key: format!("legacy-person-ask:{}", legacy.request),
            };
            match self.ask_person(&input) {
                Ok(_) => changed = true,
                Err(error)
                    if matches!(
                        error.code,
                        "stale-work-ask" | "missing-ask-owner" | "ambiguous-ask-owner"
                    ) => {}
                Err(error) => return Err(anyhow::Error::new(error)),
            }
        }
        Ok(changed)
    }

    pub(crate) fn reconcile_person_asks(&self) -> Result<bool> {
        let migrated = self.migrate_legacy_person_asks()?;
        self.connection.batched(|tx| -> Result<bool> {
            let mut query = tx.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
                FROM claims WHERE kind='work.person-asked' ORDER BY CANONICAL_ASC(claims)"))?;
            let asks = query.query_map([], claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut changed = migrated;
            for ask in asks {
                let Some(view) = step(tx, &ask.subject)? else { continue };
                if matches!(view.status.as_str(), "ready" | "pending") && !current(tx, &ask, now_ms())? {
                    let claim = append_claim_tx(tx, &self.origin, &ask.subject, "work.person-cancelled", Some("daemon/runtime"),
                        &json!({"fields": {"attempt": view.attempt, "status": "cancelled", "summary": "the requester, origin or owning run ended", "key": format!("person-owner-ended:{}", ask.id)}}), &[ask.id], None)?;
                    project(tx, &claim).map_err(anyhow::Error::new)?;
                    changed = true;
                }
            }
            Ok(changed)
        }).map_err(anyhow::Error::msg)?
    }

    fn ask_person_in_new_run(&self, input: &PersonAskRequest) -> Result<StepRunView, St3Error> {
        let name = input
            .new_run
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| St3Error::new("missing-ask-owner", "specify --step or --new-run"))?;
        if input.step.is_some() {
            return Err(St3Error::new(
                "ambiguous-ask-owner",
                "choose either --step or --new-run",
            ));
        }
        self.connection.batched(|tx| {
            if !declaration_live(tx, &input.actor).map_err(internal)? {
                return Err(St3Error::new("missing-ask-owner", "the requester must have a live declaration"));
            }
            let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM step_runs WHERE lease_owner=?1 AND status IN ('claimed','working'))",
                [&input.actor], |row| row.get(0)).map_err(internal)?;
            if active { return Err(St3Error::new("ambiguous-ask-owner", "use --step while you hold claimed work")); }
            let desired = current_desired_row(tx, &input.actor).map_err(internal)?.unwrap();
            let key = serde_json::to_string(&(&input.actor, name, &desired.owner_run, &desired.owner_generation, &desired.claim_id, &input.idempotency_key)).map_err(internal)?;
            let hash = hex::encode(Sha256::digest(key.as_bytes()));
            let generation = format!("ask-{}", &hash[..32]);
            let subject = format!("step-run/{generation}/ask");
            if let Some(existing) = request(tx, &subject).map_err(internal)? {
                if existing.body["fields"]["person"] != input.person || existing.body["fields"]["title"] != input.title || existing.body["fields"]["reason"] != input.reason {
                    return Err(St3Error::new("idempotency-conflict", "this ask key already names another question"));
                }
                return step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the ask is no longer retained"));
            }
            let mission_id = format!("person-ask/{}", &hash[..32]);
            let kdl = format!("version 2\nmission {mission_id:?} state=\"ready\" {{ goal {:?}; step \"ask\" {{ assigned-to {:?}; goal {:?}; }} }}", input.title, input.person, input.reason);
            let mut intent = crate::graph::parse_internal_intent(&kdl, &self.origin)?;
            let mission = intent.missions.remove(&mission_id).ok_or_else(|| St3Error::new("internal", "the person mission could not be parsed"))?;
            let run = format!("mission-run/person-ask/{}", &hash[..32]);
            let waiting_since = input.legacy_request.as_ref().map(|id| tx.query_row("SELECT accepted_at_unix_ms FROM claims WHERE id=?1 AND kind='attention.requested'", [id], |row| row.get::<_, String>(0))).transpose().map_err(internal)?;
            let evidence = input.legacy_request.iter().cloned().collect::<Vec<_>>();
            let claim = append_claim_tx(tx, &self.origin, &subject, "work.person-asked", Some(&input.actor),
                &json!({"fields": {"run": run, "generation": format!("run-generation/{generation}"),
                    "person": input.person, "title": input.title, "reason": input.reason, "key": input.idempotency_key,
                    "attempt": 1, "status": "ready", "mission_spec": mission, "owner_run": desired.owner_run,
                    "owner_generation": desired.owner_generation, "requester_declaration": desired.claim_id, "waiting_since": waiting_since, "legacy_request": input.legacy_request}}), &evidence, None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the ask could not be projected"))
        }).map_err(internal)?
    }
}

pub(super) fn project(tx: &Transaction<'_>, claim: &ClaimRecord) -> Result<bool, St3Error> {
    let fields = &claim.body["fields"];
    if claim.kind == "work.person-asked" {
        let run = fields["run"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("mission-run/");
        let generation = fields["generation"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("run-generation/");
        if let Some(spec) = fields.get("mission_spec") {
            let mission: MissionSpec = serde_json::from_value(spec.clone()).map_err(internal)?;
            let root = fields["owner_run"]
                .as_str()
                .unwrap_or(run)
                .trim_start_matches("mission-run/");
            let at = claim.accepted_at_unix_ms.to_string();
            tx.execute("INSERT OR IGNORE INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index)
                VALUES(?1,?2,'ready',?3,?4,?5)", params![mission.id, mission.revision, serde_json::to_string(&mission).map_err(internal)?, claim.id, claim.store_index]).map_err(internal)?;
            tx.execute("INSERT OR IGNORE INTO mission_definitions(mission_id,revision,state,claim_id) VALUES(?1,?2,'ready',?3)", params![mission.id, mission.revision, claim.id]).map_err(internal)?;
            tx.execute("INSERT OR IGNORE INTO mission_runs(id,mission_id,initial_revision,current_generation_id,root_revision,root_run_id,
                workspace,requester,inputs,mode,status,phase,created_at_unix_ms,updated_at_unix_ms)
                VALUES(?1,?2,?3,?4,?3,?5,'.',?6,'{}','run','running','normal',?7,?7)",
                params![run, mission.id, mission.revision, generation, root, claim.actor, at]).map_err(internal)?;
            tx.execute("INSERT OR IGNORE INTO run_generations(id,run_id,revision,status,actor,reason,created_at_unix_ms,updated_at_unix_ms)
                VALUES(?1,?2,?3,'running',?4,'person ask',?5,?5)", params![generation, run, mission.revision, claim.actor, at]).map_err(internal)?;
        }
        tx.execute("INSERT OR IGNORE INTO step_runs(subject,run_id,generation_id,step_path,definition_hash,status,
            attempt,assignee,available_to,agentless,title,goals,created_at_unix_ms,updated_at_unix_ms,constraints)
            VALUES(?1,?2,?3,?4,?5,'ready',1,?6,'[]',0,?7,?8,?9,?9,'[]')",
            params![claim.subject, run, generation, claim.subject.rsplit('/').next(), claim.id,
                fields["person"].as_str(), fields["title"].as_str(), serde_json::to_string(&vec![fields["reason"].as_str().unwrap_or_default()]).map_err(internal)?, claim.accepted_at_unix_ms.to_string()]).map_err(internal)?;
        if step_owner_is_terminal_tx(tx, &claim.subject)? {
            tx.execute("UPDATE step_runs SET status='cancelled',blocked_reason='the owning run ended' WHERE subject=?1", [&claim.subject]).map_err(internal)?;
            return Ok(true);
        }
        if let Some(origin) = fields["origin_step"].as_str() {
            tx.execute("UPDATE step_runs SET status='waiting-person',lease_owner=NULL,lease_incarnation=NULL,
                lease_expires_at_unix_ms=NULL,blocked_reason=?3,updated_at_unix_ms=?4
                WHERE subject=?1 AND attempt=?2 AND status IN ('claimed','working','ready','blocked')",
                params![origin, fields["origin_attempt"].as_u64(), claim.subject, claim.accepted_at_unix_ms.to_string()]).map_err(internal)?;
            tx.execute(
                "DELETE FROM local_work_lease_renewals WHERE subject=?1",
                [origin],
            )
            .map_err(internal)?;
        }
        return Ok(true);
    }
    if !matches!(
        claim.kind.as_str(),
        "work.person-done" | "work.person-cancelled"
    ) {
        return Ok(false);
    }
    tx.execute(
        "UPDATE step_runs SET status=?2,worker_reported=1,updated_at_unix_ms=?3
        WHERE subject=?1 AND attempt=?4 AND status IN ('ready','pending')",
        params![
            claim.subject,
            fields["status"].as_str(),
            claim.accepted_at_unix_ms.to_string(),
            fields["attempt"].as_u64()
        ],
    )
    .map_err(internal)?;
    if let Some(ask) = request(tx, &claim.subject).map_err(internal)? {
        if ask.body["fields"].get("mission_spec").is_some() {
            tx.execute("UPDATE mission_runs SET status=?2,phase='terminal',updated_at_unix_ms=?3 WHERE id=?1",
                params![ask.body["fields"]["run"].as_str().unwrap_or_default().trim_start_matches("mission-run/"), fields["status"].as_str(), claim.accepted_at_unix_ms.to_string()]).map_err(internal)?;
        }
        if let Some(origin) = ask.body["fields"]["origin_step"].as_str() {
            if !step_owner_is_terminal_tx(tx, origin)? {
                tx.execute("UPDATE step_runs SET status='ready',blocked_reason=NULL,readiness_epoch=readiness_epoch+1,
                    activated_at_unix_ms=?3,updated_at_unix_ms=?3 WHERE subject=?1 AND attempt=?2 AND status='waiting-person' AND blocked_reason=?4",
                    params![origin, ask.body["fields"]["origin_attempt"].as_u64(), claim.accepted_at_unix_ms.to_string(), ask.subject]).map_err(internal)?;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Store, StepRunView, PersonAskRequest) {
        let store = Store::open_memory("alder").unwrap();
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
agent "alder.asker" { workspace "/tmp"; command "true"; restart always; }
mission "person-work" state="ready" {
  goal "Review the release.";
  step "prepare" { assigned-to "agent/alder.asker"; goal "Prepare the release."; }
  step "review" { assigned-to "person/avery"; goal "Review the release."; }
}
"#,
            "alder",
        )
        .unwrap();
        store.apply_internal(&intent, "person-fixture").unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "person-work".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/avery".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "person-run".into(),
            })
            .unwrap();
        let origin = run
            .steps
            .iter()
            .find(|step| step.step == "prepare")
            .unwrap()
            .clone();
        store.connection.batched(|tx| -> Result<()> {
            let claim = append_claim_tx(tx, "alder", &origin.subject, "work.claimed", Some("agent/alder.asker"),
                &json!({"fields": {"attempt": 1, "status": "claimed", "claimant": "agent/alder.asker",
                    "claim_incarnation": "asker-one", "claim_expires_at_unix_ms": (now_ms()+600_000) as u64}}), &[], None)?;
            project_mission_run_update(tx, &claim).unwrap();
            Ok(())
        }).unwrap().unwrap();
        let input = PersonAskRequest {
            legacy_request: None,
            person: "person/avery".into(),
            title: "Choose a release date".into(),
            reason: "Reply with the release date.".into(),
            actor: "agent/alder.asker".into(),
            step: Some(origin.subject.clone()),
            new_run: None,
            incarnation: Some("asker-one".into()),
            idempotency_key: "release-date".into(),
        };
        (store, origin, input)
    }

    #[test]
    fn person_ask_suspends_without_lease_and_response_resumes_same_attempt() {
        let (store, origin, input) = fixture();
        let ask = store.ask_person(&input).unwrap();
        let paused = store.step_run(&origin.subject).unwrap().unwrap();
        assert_eq!(paused.status, "waiting-person");
        assert!(paused.claimant.is_none() && paused.claim_expires_at_unix_ms.is_none());
        assert_eq!(store.ask_person(&input).unwrap().subject, ask.subject);
        let mut conflict = input.clone();
        conflict.reason = "Another question".into();
        assert_eq!(
            store.ask_person(&conflict).unwrap_err().code,
            "idempotency-conflict"
        );
        let items = store
            .attention_snapshot(Some("person/avery"), now_ms() + 7 * 86_400_000)
            .unwrap();
        let item = items
            .iter()
            .find(|item| item.subject == ask.subject)
            .unwrap();
        assert!(
            store
                .attention_snapshot(Some("person/robin"), now_ms())
                .unwrap()
                .is_empty()
        );
        let mut response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: "person/robin".into(),
            summary: "Friday".into(),
            evidence: Vec::new(),
            episode: Some(item.episode.clone()),
            idempotency_key: "release-response".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "forbidden"
        );
        response.actor = "person/avery".into();
        response.episode = Some("old-episode".into());
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "stale-fence"
        );
        response.episode = Some(item.episode.clone());
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        let resumed = store.step_run(&origin.subject).unwrap().unwrap();
        assert_eq!(resumed.status, "ready");
        assert_eq!(resumed.attempt, origin.attempt);
        assert!(
            resumed
                .constraints
                .iter()
                .any(|constraint| constraint == "Person response: Friday")
        );
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != ask.subject)
        );
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store.step_run(&origin.subject).unwrap().unwrap().status,
            "ready"
        );
        assert_eq!(
            store.step_run(&ask.subject).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn person_ask_disappears_on_requester_retirement_without_cleanup() {
        let (store, _origin, input) = fixture();
        let ask = store.ask_person(&input).unwrap();
        let stop =
            crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
                .unwrap();
        store.apply_internal(&stop, "retire-asker").unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != ask.subject)
        );
        let response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: "person/avery".into(),
            summary: "Friday".into(),
            evidence: vec![],
            episode: None,
            idempotency_key: "late-response".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "stale-fence"
        );
        store.reconcile_person_asks().unwrap();
        assert_eq!(
            store.step_run(&ask.subject).unwrap().unwrap().status,
            "cancelled"
        );
    }

    #[test]
    fn new_run_person_ask_is_durable_and_bound_to_requester() {
        let (store, origin, mut input) = fixture();
        store
            .set_step_state(&origin.subject, "completed", None)
            .unwrap();
        input.step = None;
        input.new_run = Some("release-question".into());
        let ask = store.ask_person(&input).unwrap();
        assert_eq!(store.ask_person(&input).unwrap().subject, ask.subject);
        store.replay_replication_graph().unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .any(|item| item.subject == ask.subject)
        );
        let response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: input.actor,
            summary: "No longer needed".into(),
            evidence: vec![],
            episode: None,
            idempotency_key: "cancel-question".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, true).unwrap().status,
            "cancelled"
        );
        assert_eq!(
            store.mission_run(&ask.run).unwrap().unwrap().status,
            "cancelled"
        );
    }
    #[test]
    fn authored_person_work_waits_for_readiness_and_only_assignee_can_finish() {
        let (store, origin, _input) = fixture();
        let run = store.mission_run(&origin.run).unwrap().unwrap();
        let review = run.steps.iter().find(|step| step.step == "review").unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
        store
            .set_step_state(&review.subject, "ready", None)
            .unwrap();
        let item = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(item.subject, review.subject);
        let mut response = PersonStepResponse {
            subject: review.subject.clone(),
            actor: "person/robin".into(),
            summary: "Reviewed the release".into(),
            evidence: vec![],
            episode: Some(item.episode),
            idempotency_key: "authored-response".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "forbidden"
        );
        response.actor = "person/avery".into();
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
        response.evidence.push("doc/release-review".into());
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "idempotency-conflict"
        );
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store.step_run(&review.subject).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn cancelled_completed_and_failed_owners_hide_person_asks_before_cleanup() {
        for status in ["cancelled", "completed", "failed"] {
            let (store, origin, input) = fixture();
            let ask = store.ask_person(&input).unwrap();
            store
                .set_mission_run_state(&origin.run, status, "terminal", Some("the owner ended"))
                .unwrap();
            assert!(
                store
                    .attention_items(Some("person/avery"))
                    .unwrap()
                    .iter()
                    .all(|item| item.subject != ask.subject)
            );
            let response = PersonStepResponse {
                subject: ask.subject.clone(),
                actor: "person/avery".into(),
                summary: "Friday".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: format!("ended-{status}"),
            };
            assert_eq!(
                store.finish_person_step(&response, false).unwrap_err().code,
                "stale-fence"
            );
            store.replay_replication_graph().unwrap();
            assert!(
                store
                    .attention_items(Some("person/avery"))
                    .unwrap()
                    .iter()
                    .all(|item| item.subject != ask.subject)
            );
        }
    }

    #[test]
    fn a_redeclared_requester_does_not_revive_its_old_person_episode() {
        let (store, _origin, input) = fixture();
        let ask = store.ask_person(&input).unwrap();
        let stop =
            crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
                .unwrap();
        store.apply_internal(&stop, "retire-old-episode").unwrap();
        let renewed = crate::graph::parse_internal_intent("version 2\nagent \"alder.asker\" { workspace \"/tmp\"; command \"true\"; restart always; }", "alder").unwrap();
        store
            .apply_internal(&renewed, "new-requester-declaration")
            .unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != ask.subject)
        );
        store.replay_replication_graph().unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != ask.subject)
        );
    }

    #[test]
    fn a_later_question_in_the_same_attempt_does_not_revive_the_previous_ask() {
        let (store, origin, input) = fixture();
        let old = store.ask_person(&input).unwrap();
        store.set_step_state(&origin.subject, "ready", None).unwrap();
        store.connection.batched(|tx| -> Result<()> {
            let claim = append_claim_tx(tx, "alder", &origin.subject, "work.claimed", Some(&input.actor),
                &json!({"fields":{"attempt":1,"status":"claimed","claimant":input.actor,
                    "claim_incarnation":"asker-one","claim_expires_at_unix_ms":(now_ms()+600_000) as u64}}), &[], None)?;
            project_mission_run_update(tx, &claim).unwrap();
            Ok(())
        }).unwrap().unwrap();
        let mut next = input;
        next.idempotency_key = "a-later-question".into();
        let fresh = store.ask_person(&next).unwrap();
        let items = store.attention_items(Some("person/avery")).unwrap();
        assert!(items.iter().any(|item| item.subject == fresh.subject));
        assert!(items.iter().all(|item| item.subject != old.subject));
        store.reconcile_person_asks().unwrap();
        assert_eq!(store.step_run(&origin.subject).unwrap().unwrap().status, "waiting-person");
        assert_eq!(store.step_run(&origin.subject).unwrap().unwrap().blocked_reason, Some(fresh.subject));
    }

    #[test]
    fn live_legacy_asks_import_once_with_age_and_origin_after_release() {
        let (store, origin, input) = fixture();
        let legacy = store
            .request_attention_closing(
                "attention/legacy-date",
                &AttentionRequest {
                    reviewer: input.person.clone(),
                    title: input.title.clone(),
                    reason: input.reason.clone(),
                    severity: "warning".into(),
                    targets: vec![origin.subject.clone()],
                    actor: input.actor.clone(),
                    idempotency_key: "legacy-date".into(),
                },
                &AttentionClosing {
                    step: Some(origin.subject.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(legacy.step.as_deref(), Some(origin.subject.as_str()));
        store
            .work_action(
                &origin.subject,
                "release",
                &WorkRequest {
                    actor: Some(input.actor.clone()),
                    incarnation: input.incarnation.clone(),
                    summary: None,
                    reason: Some("Waiting for a person".into()),
                    evidence: vec![],
                    idempotency_key: "release-legacy-ask".into(),
                },
            )
            .unwrap();
        assert!(store.reconcile_person_asks().unwrap());
        let asks = store.attention_items(Some("person/avery")).unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].title, legacy.title);
        assert_eq!(asks[0].detail, legacy.reason);
        assert_eq!(asks[0].requested_at_unix_ms, legacy.requested_at_unix_ms);
        assert_eq!(
            store.step_run(&origin.subject).unwrap().unwrap().status,
            "waiting-person"
        );
        assert!(!store.reconcile_person_asks().unwrap());
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store.attention_items(Some("person/avery")).unwrap()[0].episode,
            asks[0].episode
        );
        let connection = store.readers.get();
        let at = now_ms();
        let (_, elapsed) =
            step_execution_timing_at(&connection, &origin.subject, 1, at, false).unwrap();
        assert_eq!(
            step_execution_timing_at(&connection, &origin.subject, 1, at + 6 * 86_400_000, false)
                .unwrap(),
            (None, elapsed)
        );
        let record = request(&connection, &asks[0].subject).unwrap().unwrap();
        assert!(record.predecessors.contains(&legacy.request));
    }

    #[test]
    fn origin_retry_invalidates_old_ask_and_response_without_cleanup() {
        let (store, origin, input) = fixture();
        let ask = store.ask_person(&input).unwrap();
        store
            .set_step_state(&origin.subject, "failed", Some("try again"))
            .unwrap();
        store.retry_step(&origin.subject, "new attempt", 0).unwrap();
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != ask.subject)
        );
        let response = PersonStepResponse {
            subject: ask.subject,
            actor: "person/avery".into(),
            summary: "Friday".into(),
            evidence: vec![],
            episode: None,
            idempotency_key: "late-old-attempt".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "stale-fence"
        );
    }
}
