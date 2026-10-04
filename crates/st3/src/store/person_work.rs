//! Person asks are runtime steps. The immutable asking claim supplies their ownership fence;
//! step_runs supplies their state. No separate attention record is created.
use super::*;
use crate::model::{PersonAskRequest, PersonStepResponse};
use crate::person_request::{StructuredRequest, answer_summary};

/// The ask's structured request in canonical form, or null for a free-text ask.
fn canonical_request(input: &PersonAskRequest) -> Result<Value, St3Error> {
    input
        .request
        .as_ref()
        .map(|request| StructuredRequest::parse(request).map(|(_, canonical)| canonical))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Adds the structured request to an ask claim body. A free-text ask's body stays exactly as
/// before, so members that do not know the field still admit it.
fn with_request(mut body: Value, request: &Value) -> Value {
    if !request.is_null() {
        body["fields"]["request"] = request.clone();
    }
    body
}

/// The structured request an ask claim carries. An ask from a newer build whose request this
/// build cannot read is refused rather than answered as free text.
fn structured_request(ask: &ClaimRecord) -> Result<Option<StructuredRequest>, St3Error> {
    let Some(request) = ask.body["fields"].get("request").filter(|r| !r.is_null()) else {
        return Ok(None);
    };
    serde_json::from_value::<StructuredRequest>(request.clone())
        .ok()
        .filter(|request| request.version == crate::person_request::REQUEST_VERSION)
        .map(Some)
        .ok_or_else(|| {
            St3Error::new(
                "unsupported-person-request",
                "this ask's request needs a newer st to answer",
            )
        })
}

/// The answer the person gives and the summary history keeps. A free-text ask takes text
/// only; a structured ask resolves the named answer against its request.
fn resolve_answer(
    ask: Option<&ClaimRecord>,
    input: &PersonStepResponse,
    cancel: bool,
) -> Result<(Option<Value>, String), St3Error> {
    let request = if cancel {
        None
    } else {
        ask.map(structured_request).transpose()?.flatten()
    };
    if cancel || request.is_none() {
        let Some(answer) = &input.answer else {
            return Ok((None, input.summary.clone()));
        };
        if cancel || answer.id.is_some() {
            return Err(St3Error::new(
                "invalid-person-answer",
                if cancel {
                    "a cancelled ask has no answer"
                } else {
                    "this ask is free text; it has no named answers"
                },
            ));
        }
        let summary = if input.summary.trim().is_empty() {
            answer.text.clone().unwrap_or_default()
        } else {
            input.summary.clone()
        };
        return Ok((None, summary));
    }
    let answer = request
        .expect("checked above")
        .answer(input.answer.as_ref(), &input.summary)?;
    let summary = if input.summary.trim().is_empty() {
        answer_summary(&answer)
    } else {
        input.summary.clone()
    };
    Ok((Some(answer), summary))
}

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
    Ok(run_liveness(connection, run, generation, failure)?.is_ok())
}

/// Whether `run` still matters, or why not: it and each run above it exist, are open and on
/// the generation their parent step belongs to, each parent step is still open, and a
/// subscription or schedule that delivered it still runs. `failure` keeps a failed run or
/// parent step live, for the fault that names it.
pub(super) fn run_liveness(
    connection: &Connection,
    run: &str,
    generation: Option<&str>,
    failure: bool,
) -> Result<std::result::Result<(), String>> {
    let mut run = run.strip_prefix("mission-run/").unwrap_or(run).to_owned();
    let mut expected_generation = generation.map(str::to_owned);
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(run.clone()) {
            return Ok(Err(format!(
                "mission run `mission-run/{run}` is its own ancestor"
            )));
        }
        let header = mission_run_header_tx(connection, &run).optional()?;
        let Some(header) = header else {
            return Ok(Err(format!("mission run `mission-run/{run}` is gone")));
        };
        if let Some(expected) = expected_generation
            .as_ref()
            .filter(|expected| *expected != &header.generation)
        {
            return Ok(Err(format!(
                "mission run `{}` moved on from {expected} to {}",
                header.subject, header.generation
            )));
        }
        if is_terminal_run_state(&header.status) && !(failure && header.status == "failed") {
            return Ok(Err(format!(
                "mission run `{}` is {}",
                header.subject, header.status
            )));
        }
        if let Some(parent) = header.parent_step_run.as_deref() {
            let Some(parent) = step(connection, parent)? else {
                // Subscription and schedule deliveries use their declaration as the parent,
                // normalized with a step-run prefix. They do not create a synthetic work step.
                let subject = parent.strip_prefix("step-run/").unwrap_or(parent);
                let Some(owner) = current_desired_row(connection, subject)? else {
                    return Ok(Err(format!("`{subject}`, which started it, is gone")));
                };
                let body: Value = serde_json::from_str(&owner.body)?;
                if !matches!(owner.kind.as_str(), "subscription" | "schedule") {
                    return Ok(Err(format!(
                        "`{subject}`, which started it, is not a step, subscription or schedule"
                    )));
                }
                if body
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| children.len() == 1 && children[0]["name"] == "stop")
                {
                    return Ok(Err(format!("`{subject}`, which started it, is stopped")));
                }
                let Some(owner_run) = owner.owner_run else {
                    return Ok(Err(format!(
                        "`{subject}`, which started it, has no owning run"
                    )));
                };
                let owner_header =
                    mission_run_header_tx(connection, owner_run.trim_start_matches("mission-run/"))
                        .optional()?;
                if owner_header
                    .is_none_or(|owner| owner.root_mission_run != header.root_mission_run)
                {
                    return Ok(Err(format!(
                        "`{subject}`, which started it, now belongs to another run"
                    )));
                }
                run = owner_run.trim_start_matches("mission-run/").into();
                expected_generation = owner.owner_generation;
                continue;
            };
            if matches!(parent.status.as_str(), "completed" | "cancelled")
                || (parent.status == "failed" && !failure)
            {
                return Ok(Err(format!(
                    "its parent step `{}` is {}",
                    parent.subject, parent.status
                )));
            }
            run = parent.run.trim_start_matches("mission-run/").into();
            expected_generation = Some(parent.generation);
        } else {
            let root = mission_run_header_tx(
                connection,
                header.root_mission_run.trim_start_matches("mission-run/"),
            )
            .optional()?;
            return Ok(match root {
                None => Err(format!(
                    "its root mission run `{}` is gone",
                    header.root_mission_run
                )),
                Some(root)
                    if is_terminal_run_state(&root.status)
                        && !(failure && root.status == "failed") =>
                {
                    Err(format!(
                        "its root mission run `{}` is {}",
                        root.subject, root.status
                    ))
                }
                Some(_) => Ok(()),
            });
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
    let requester = ask.actor.as_deref().unwrap_or_default();
    // A daemon asks for its own policy, not for a seat, so no declaration fences it. An update
    // waits for nobody, so it stays until the person reads it, even after its poster stops.
    let unfenced = requester.starts_with("daemon/") || is_update(ask);
    if !matches!(view.status.as_str(), "pending" | "ready")
        || !run_live(connection, &view.run, Some(&view.generation), false)?
    {
        return Ok(false);
    }
    let requester_live = unfenced || declaration_live(connection, requester)?;
    let retiring = !requester_live && super::rollouts::retiring_ask_live(connection, ask)?;
    if !requester_live && !retiring {
        return Ok(false);
    }
    if unfenced {
        return Ok(true);
    }
    let mut declarations = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='intent.desired' ORDER BY CANONICAL_ASC(claims)"))?;
    let ask_key = canonical::claim_key(connection, &ask.id)?;
    for declaration in declarations.query_map([requester], claim_from_row)? {
        let declaration = declaration?;
        if canonical::claim_key(connection, &declaration.id)? <= ask_key {
            continue;
        }
        if !retiring
            && (declaration.body["kind"] == "stop"
                || declaration.body["desired"]
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| children.len() == 1 && children[0]["name"] == "stop"))
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

fn is_update(ask: &ClaimRecord) -> bool {
    ask.body["fields"]["request"]["type"] == "update"
}

/// Whether `person` asked for `about`: a mission run or step run that the person requested, or
/// whose root run they requested, or a message the person sent to `agent`.
fn person_asked(connection: &Connection, person: &str, agent: &str, about: &str) -> Result<bool> {
    if about.starts_with("message/") {
        let mut query = connection
            .prepare_cached("SELECT body FROM claims WHERE subject=?1 AND kind='message.sent'")?;
        for body in query.query_map([about], |row| row.get::<_, String>(0))? {
            let body: Value = serde_json::from_str(&body?)?;
            if body["fields"]["from"] == person && body["fields"]["to"] == agent {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let run = if about.starts_with("step-run/") {
        match step(connection, about)? {
            Some(view) => view.run,
            None => return Ok(false),
        }
    } else {
        about.to_owned()
    };
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM mission_runs run
          LEFT JOIN mission_runs root ON root.id=run.root_run_id
          WHERE run.id=?1 AND (run.requester=?2 OR root.requester=?2))",
        params![run.trim_start_matches("mission-run/"), person],
        |row| row.get(0),
    )?)
}

/// Adds person responses to a step view: the responses to asks this attempt made, as
/// constraints and as data, or a person ask's own response as data.
pub(super) fn enrich_responses(
    connection: &Connection,
    view: &mut StepRunView,
) -> rusqlite::Result<()> {
    let mut query = connection.prepare_cached(&canonical_sql(
        "SELECT resolution.subject, resolution.actor, resolution.body, resolution.accepted_at_unix_ms,
          json_extract(request.body,'$.fields.origin_step')=?1 FROM claims resolution JOIN claims request
        ON request.subject=resolution.subject AND request.kind='work.person-asked'
        WHERE resolution.kind IN ('work.person-done','work.person-cancelled')
          -- Only asks this step made, or the step's own ask: an index walk, not every answer.
          AND resolution.subject IN (
            SELECT subject FROM claims WHERE kind='work.person-asked'
              AND json_extract(body,'$.fields.origin_step')=?1
            UNION SELECT ?1)
          AND ((json_extract(request.body,'$.fields.origin_step')=?1
                AND json_extract(request.body,'$.fields.origin_attempt')=?2)
            OR (resolution.subject=?1 AND json_extract(resolution.body,'$.fields.attempt')=?2))
        ORDER BY CANONICAL_ASC(resolution)",
    ))?;
    let responses = query
        .query_map(params![view.subject, view.attempt], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<bool>>(4)?.unwrap_or(false),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (ask, actor, body, accepted, from_origin) in responses {
        let Ok(body) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let fields = &body["fields"];
        let Some(summary) = fields["summary"].as_str() else {
            continue;
        };
        if from_origin {
            view.constraints.push(format!("Person response: {summary}"));
        }
        view.person_answers.push(crate::model::PersonAnswerView {
            ask,
            status: fields["status"].as_str().unwrap_or("completed").into(),
            summary: summary.into(),
            respondent: actor.unwrap_or_default(),
            answered_at_unix_ms: accepted.parse().unwrap_or(0),
            answer: fields
                .get("answer")
                .filter(|answer| answer.is_object())
                .cloned(),
            evidence: serde_json::from_value(body["evidence"].clone()).unwrap_or_default(),
        });
    }
    Ok(())
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
        if input
            .request
            .as_ref()
            .is_some_and(|request| request["type"] == "update")
        {
            return self.post_update(input);
        }
        if input.step.is_none() || input.new_run.is_some() {
            return self.ask_person_in_new_run(input);
        }
        self.connection.batched(|tx| {
            let structured = canonical_request(input)?;
            let named = step(tx, &normalize_step_run(input.step.as_deref().unwrap())).map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-step-run", "the asking step does not exist"))?;
            // A revision moves a step into the run's new generation, and its worker may still
            // name it by the predecessor subject, as the seat's environment does. Work actions
            // accept that name; so does an ask, and only the claimant may ask from either.
            let origin = current_generation_successor_tx(tx, &named).map_err(internal)?.unwrap_or(named);
            let origin_subject = origin.subject.clone();
            let identity = serde_json::to_string(&(&origin.generation, &origin_subject, origin.attempt, &input.idempotency_key)).map_err(internal)?;
            let hash = hex::encode(Sha256::digest(identity.as_bytes()));
            let subject = format!("step-run/{}/ask-{}", generation_id_from_subject(&origin.generation), &hash[..32]);
            if let Some(existing) = request(tx, &subject).map_err(internal)? {
                if existing.actor.as_deref() != Some(input.actor.as_str())
                    || existing.body["fields"]["person"] != input.person
                    || existing.body["fields"]["title"] != input.title
                    || existing.body["fields"]["reason"] != input.reason
                    || existing.body["fields"]["request"] != structured {
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
                &with_request(json!({"fields": {"run": origin.run, "generation": origin.generation,
                    "origin_step": origin_subject, "origin_attempt": origin.attempt,
                    "person": input.person, "title": input.title, "reason": input.reason,
                    "key": input.idempotency_key, "attempt": 1, "status": "ready", "waiting_since": waiting_since, "legacy_request": input.legacy_request}}), &structured), &evidence, None).map_err(claim_append_error)?;
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
            // Checked after authority and fences below; a replay compares against it first.
            let resolved = resolve_answer(ask.as_ref(), input, cancel);
            let kind = if cancel { "work.person-cancelled" } else { "work.person-done" };
            if let Some(existing) = tx.query_row(&canonical_sql("SELECT id, store_index, batch_id, subject, kind, origin,
                actor, body, predecessors, accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind=?2
                ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![subject, kind], claim_from_row).optional().map_err(internal)? {
                if existing.body["fields"]["key"] == input.idempotency_key && existing.actor.as_deref() == Some(input.actor.as_str()) {
                    let Ok((answer, summary)) = &resolved else {
                        return Err(St3Error::new("idempotency-conflict", "this response key already has another answer"));
                    };
                    if existing.body["fields"]["summary"] != *summary || existing.body["evidence"] != json!(input.evidence)
                        || existing.body["fields"]["answer"] != answer.clone().unwrap_or_default()
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
            let (answer, summary) = resolved?;
            if summary.trim().is_empty() {
                return Err(St3Error::new("invalid-person-response", "a response needs a summary"));
            }
            let evidence = ask.as_ref().map(|a| vec![a.id.clone()]).unwrap_or_default();
            let mut body = json!({"fields": {"attempt": view.attempt, "status": if cancel { "cancelled" } else { "completed" },
                "summary": summary, "key": input.idempotency_key, "episode": ask.as_ref().map(|a| a.id.clone()).unwrap_or_else(|| format!("{}:{}:{}", view.generation, view.attempt, view.readiness_epoch))}, "evidence": input.evidence});
            if let Some(answer) = &answer {
                body["fields"]["answer"] = answer.clone();
            }
            let claim = append_claim_tx(tx, &self.origin, &subject, kind, Some(&input.actor), &body, &evidence, None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            if let Some(ask) = ask.as_ref().filter(|a| !cancel && a.body["fields"]["origin_step"].is_null() && !is_update(a)) {
                send_answer_tx(tx, &self.origin, ask, &claim, &summary, answer.as_ref())?;
            }
            if let Some(origin) = ask.as_ref().and_then(|a| a.body["fields"]["origin_step"].as_str()) {
                if let Some(origin_view) = step(tx, origin).map_err(internal)? {
                    if origin_view.status == "ready" {
                        let response = append_claim_tx(tx, &self.origin, origin, "step-run.state", Some("daemon/runtime"),
                            &json!({"fields": {"status": "ready", "attempt": origin_view.attempt, "readiness_epoch": origin_view.readiness_epoch,
                                "reason": format!("Person response: {summary}")}}), &[claim.id], None).map_err(claim_append_error)?;
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
                request: None,
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

    /// Brings a person information they asked for. Nothing waits on an update: it stays on the
    /// person's home until they open or read it, and it is refused unless `about` names the
    /// person's own run or step, or their message to the poster.
    fn post_update(&self, input: &PersonAskRequest) -> Result<StepRunView, St3Error> {
        if input.step.is_some() || input.new_run.is_some() {
            return Err(St3Error::new(
                "ambiguous-ask-owner",
                "an update names the work it is about in its request, not with a step or new run",
            ));
        }
        let structured = canonical_request(input)?;
        let about = structured["about"].as_str().unwrap_or_default().to_owned();
        self.connection.batched(|tx| {
            if !declaration_live(tx, &input.actor).map_err(internal)? {
                return Err(St3Error::new("missing-ask-owner", "the agent posting an update must have a live declaration"));
            }
            if !person_asked(tx, &input.person, &input.actor, &about).map_err(internal)? {
                return Err(St3Error::new("update-not-asked", format!(
                    "{} did not ask for `{about}`: an update is about the person's own run or step, or their message to you",
                    input.person)));
            }
            let identity = serde_json::to_string(&(&input.actor, &input.person, &about, &input.idempotency_key)).map_err(internal)?;
            let hash = hex::encode(Sha256::digest(identity.as_bytes()));
            let generation = format!("update-{}", &hash[..32]);
            let subject = format!("step-run/{generation}/update");
            if let Some(existing) = request(tx, &subject).map_err(internal)? {
                if existing.body["fields"]["title"] != input.title || existing.body["fields"]["reason"] != input.reason
                    || existing.body["fields"]["request"] != structured {
                    return Err(St3Error::new("idempotency-conflict", "this update key already names another update"));
                }
                return step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the update is no longer retained"));
            }
            let mission_id = format!("person-update/{}", &hash[..32]);
            let kdl = format!("version 2\nmission {mission_id:?} state=\"ready\" {{ goal {:?}; step \"update\" {{ assigned-to {:?}; goal {:?}; }} }}", input.title, input.person, input.reason);
            let mut intent = crate::graph::parse_internal_intent(&kdl, &self.origin)?;
            let mission = intent.missions.remove(&mission_id).ok_or_else(|| St3Error::new("internal", "the update mission could not be parsed"))?;
            let claim = append_claim_tx(tx, &self.origin, &subject, "work.person-asked", Some(&input.actor),
                &with_request(json!({"fields": {"run": format!("mission-run/person-update/{}", &hash[..32]),
                    "generation": format!("run-generation/{generation}"), "person": input.person, "title": input.title,
                    "reason": input.reason, "key": input.idempotency_key, "attempt": 1, "status": "ready", "mission_spec": mission}}), &structured),
                &[], None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the update could not be projected"))
        }).map_err(internal)?
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
        let structured = canonical_request(input)?;
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
                if existing.body["fields"]["person"] != input.person || existing.body["fields"]["title"] != input.title || existing.body["fields"]["reason"] != input.reason
                    || existing.body["fields"]["request"] != structured {
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
                &with_request(json!({"fields": {"run": run, "generation": format!("run-generation/{generation}"),
                    "person": input.person, "title": input.title, "reason": input.reason, "key": input.idempotency_key,
                    "attempt": 1, "status": "ready", "mission_spec": mission, "owner_run": desired.owner_run,
                    "owner_generation": desired.owner_generation, "requester_declaration": desired.claim_id, "waiting_since": waiting_since, "legacy_request": input.legacy_request}}), &structured), &evidence, None).map_err(claim_append_error)?;
            project(tx, &claim)?;
            step(tx, &subject).map_err(internal)?.ok_or_else(|| St3Error::new("missing-step-run", "the ask could not be projected"))
        }).map_err(internal)?
    }
}

/// Tell the requester of a new-run ask what the person answered. A step-owned ask needs no
/// message: the response readies the asking step again, and its wake reaches the seat.
fn send_answer_tx(
    tx: &Transaction<'_>,
    origin: &str,
    ask: &ClaimRecord,
    response: &ClaimRecord,
    summary: &str,
    answer: Option<&Value>,
) -> Result<(), St3Error> {
    let Some(requester) = ask.actor.as_deref() else {
        return Ok(());
    };
    let fields = &ask.body["fields"];
    let title = fields["title"].as_str().unwrap_or_default();
    let person = response.actor.as_deref().unwrap_or("the person");
    let mut content = format!("{person} answered `{}`: {title}\n\n", ask.subject);
    match answer.filter(|answer| answer["id"].is_string()) {
        Some(answer) => {
            content.push_str(&format!(
                "Answer: {} ({})",
                answer["label"].as_str().unwrap_or_default(),
                answer["id"].as_str().unwrap_or_default()
            ));
            if let Some(text) = answer["text"].as_str() {
                content.push_str(&format!("\n{text}"));
            }
        }
        None => content.push_str(summary),
    }
    content.push_str(&format!(
        "\n\nThe answer as data: `st work show {} --json` (person_answers).",
        ask.subject
    ));
    let key = format!("person-answer:{}:{}", ask.subject, response.id);
    let subject = format!(
        "message/{}",
        &hex::encode(Sha256::digest(key.as_bytes()))[..16]
    );
    let mut tags = vec![Value::String(format!("st3-person-answer:{}", ask.subject))];
    if let Some(run) = fields["owner_run"].as_str() {
        tags.push(Value::String(format!("mission-run:{run}")));
    }
    append_claim_tx(tx, origin, &subject, "message.sent", Some("daemon/runtime"),
        &json!({"fields": {"from": "daemon/runtime", "to": requester, "content": content, "status": "sent",
            "title": format!("Answered: {title}"), "in_reply_to": null, "tags": tags}, "evidence": [response.id]}),
        &[], None).map_err(claim_append_error)?;
    Ok(())
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
            request: None,
        };
        (store, origin, input)
    }

    fn delivery_fixture(kind: &str) -> (Store, StepRunView, PersonAskRequest, MissionRunView) {
        let (store, origin, mut input) = fixture();
        let root = store.mission_run(&origin.run).unwrap().unwrap();
        let source = r#"version 2
mission "child-person-work" state="ready" {
  goal "Gather one person decision."
  step "prepare" { assigned-to "agent/alder.asker"; goal "Prepare the question." }
}
resource "issues" { kind "vcs.repository" }
observer "issues" { resource "resource/issues"; provider "github.repository"; locator "example/repo"; field "issues" }
subscription "intake" {
  observer "observer/issues"; on "issues"
  delivery "mission" { mission "child-person-work"; resource "source"; workspace "/tmp" }
}
schedule "intake" {
  calendar { at "08:00"; timezone "UTC" }
  work { mission "person-work@REVISION"; workspace "/tmp" }
}"#.replace("REVISION", &root.revision);
        let mut intent = crate::graph::parse_internal_intent(&source, "alder").unwrap();
        let parent = format!("{kind}/intake");
        let subscription = intent.subjects.get_mut(&parent).unwrap();
        subscription.owner_run = Some(root.subject.clone());
        subscription.owner_generation = Some(root.generation.clone());
        store.apply_internal(&intent, "owned-subscription").unwrap();
        let child = store
            .create_child_mission_run(
                &MissionRunRequest {
                    mission: "child-person-work".into(),
                    revision: None,
                    workspace: "/tmp/child".into(),
                    requester: Some(input.actor.clone()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "subscription-child".into(),
                },
                &root,
                &parent,
                None,
            )
            .unwrap();
        let origin = child
            .steps
            .iter()
            .find(|step| step.step == "prepare")
            .unwrap()
            .clone();
        store.connection.batched(|tx| -> Result<()> {
            let claim = append_claim_tx(tx, "alder", &origin.subject, "work.claimed", Some(&input.actor),
                &json!({"fields": {"attempt": 1, "status": "claimed", "claimant": input.actor,
                    "claim_incarnation": "asker-one", "claim_expires_at_unix_ms": (now_ms()+600_000) as u64}}), &[], None)?;
            project_mission_run_update(tx, &claim).unwrap();
            Ok(())
        }).unwrap().unwrap();
        input.step = Some(origin.subject.clone());
        (store, origin, input, root)
    }

    #[test]
    fn delivery_child_person_ask_survives_reconciliation_and_replay() {
        for kind in ["subscription", "schedule"] {
            let (store, origin, input, _) = delivery_fixture(kind);
            let ask = store.ask_person(&input).unwrap();
            for replay in [false, true] {
                if replay {
                    store.replay_replication_graph().unwrap();
                }
                store.reconcile_person_asks().unwrap();
                assert_eq!(
                    store.step_run(&ask.subject).unwrap().unwrap().status,
                    "ready"
                );
                assert!(
                    store
                        .attention_items(Some("person/avery"))
                        .unwrap()
                        .iter()
                        .any(|item| item.subject == ask.subject)
                );
            }
            let response = PersonStepResponse {
                subject: ask.subject,
                actor: "person/avery".into(),
                summary: "Friday".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: "subscription-answer".into(),
                answer: None,
            };
            store.finish_person_step(&response, false).unwrap();
            assert_eq!(
                store.step_run(&origin.subject).unwrap().unwrap().status,
                "ready"
            );
        }
    }

    #[test]
    fn subscription_child_person_asks_keep_owner_fences() {
        for invalidation in [
            "stop",
            "root-ended",
            "stale-generation",
            "missing-parent",
            "missing-step",
        ] {
            let (store, origin, input, root) = delivery_fixture("subscription");
            let ask = store.ask_person(&input).unwrap();
            match invalidation {
                "stop" => {
                    let mut intent = crate::graph::parse_internal_intent(
                        "version 2\nsubscription \"intake\" { stop }",
                        "alder",
                    )
                    .unwrap();
                    let stopped = intent.subjects.get_mut("subscription/intake").unwrap();
                    stopped.owner_run = Some(root.subject.clone());
                    stopped.owner_generation = Some(root.generation.clone());
                    store.apply_internal(&intent, "stop-subscription").unwrap();
                }
                "root-ended" => {
                    store
                        .set_mission_run_state(
                            &root.subject,
                            "cancelled",
                            "terminal",
                            Some("intake ended"),
                        )
                        .unwrap();
                }
                "stale-generation" => {
                    store.connection.batched(|tx| -> Result<()> {
                        tx.execute("UPDATE desired SET owner_generation='run-generation/old' WHERE subject='subscription/intake'", [])?;
                        Ok(())
                    }).unwrap().unwrap();
                }
                missing => {
                    let parent = if missing == "missing-parent" {
                        "step-run/subscription/missing"
                    } else {
                        "step-run/missing/prepare"
                    };
                    store
                        .connection
                        .batched(|tx| -> Result<()> {
                            tx.execute(
                                "UPDATE mission_runs SET parent_step_run=?1 WHERE id=?2",
                                params![parent, origin.run.trim_start_matches("mission-run/")],
                            )?;
                            Ok(())
                        })
                        .unwrap()
                        .unwrap();
                }
            }
            assert!(
                store
                    .attention_items(Some("person/avery"))
                    .unwrap()
                    .iter()
                    .all(|item| item.subject != ask.subject),
                "{invalidation}"
            );
            store.reconcile_person_asks().unwrap();
            assert_eq!(
                store.step_run(&ask.subject).unwrap().unwrap().status,
                "cancelled",
                "{invalidation}"
            );
        }
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
            answer: None,
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

    /// Revise the fixture's run so `prepare` moves into a new generation: carried with its
    /// claim when only `review` changes, or issued again and claimed anew when `prepare` changes.
    fn revise_fixture(store: &Store, origin: &StepRunView, prepare_changes: bool) -> StepRunView {
        let (prepare, review) = if prepare_changes {
            ("Prepare the release notes too.", "Review the release.")
        } else {
            ("Prepare the release.", "Review the release notes.")
        };
        let source = format!(
            r#"version 2
mission "person-work" state="ready" {{
  goal "Review the release.";
  step "prepare" {{ assigned-to "agent/alder.asker"; goal "{prepare}"; }}
  step "review" {{ assigned-to "person/avery"; goal "{review}"; }}
}}
"#
        );
        let intent = crate::graph::parse_internal_intent(&source, "alder").unwrap();
        store.apply_internal(&intent, "person-revision").unwrap();
        let revised = store
            .adopt_mission_revision(
                &origin.run,
                &intent.missions["person-work"],
                "person/avery",
                "the release needs notes",
                "person-revision",
            )
            .unwrap();
        let moved = revised
            .steps
            .iter()
            .find(|step| step.step == "prepare")
            .unwrap()
            .clone();
        assert_ne!(moved.subject, origin.subject);
        if prepare_changes {
            store.set_step_state(&moved.subject, "ready", None).unwrap();
            store
                .work_action(
                    &moved.subject,
                    "claim",
                    &crate::model::WorkRequest {
                        actor: Some("agent/alder.asker".into()),
                        incarnation: Some("asker-one".into()),
                        summary: None,
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: "reclaim-prepare".into(),
                    },
                )
                .unwrap();
        }
        let moved = store.step_run(&moved.subject).unwrap().unwrap();
        assert_eq!(moved.claimant.as_deref(), Some("agent/alder.asker"));
        moved
    }

    #[test]
    fn a_worker_asks_from_the_step_a_revision_moved_by_its_old_name() {
        for prepare_changes in [false, true] {
            let (store, origin, input) = fixture();
            let moved = revise_fixture(&store, &origin, prepare_changes);
            // The worker still names the step it claimed before the revision.
            assert_eq!(input.step.as_deref(), Some(origin.subject.as_str()));
            let ask = store
                .ask_person(&input)
                .unwrap_or_else(|error| panic!("prepare changes: {prepare_changes}: {error:?}"));
            let paused = store.step_run(&moved.subject).unwrap().unwrap();
            assert_eq!(paused.status, "waiting-person");
            assert_eq!(
                store.ask_person(&input).unwrap().subject,
                ask.subject,
                "a retry is the same ask"
            );
            assert!(
                ask.subject.starts_with(&format!(
                    "step-run/{}/",
                    moved.generation.trim_start_matches("run-generation/")
                )),
                "{}",
                ask.subject
            );
            // Another seat cannot ask through a step it never held.
            let mut stranger = input.clone();
            stranger.actor = "agent/alder.other".into();
            stranger.idempotency_key = "stranger".into();
            assert_eq!(
                store.ask_person(&stranger).unwrap_err().code,
                "stale-work-ask"
            );
        }
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
            answer: None,
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
            answer: None,
        };
        assert_eq!(
            store.finish_person_step(&response, true).unwrap().status,
            "cancelled"
        );
        assert_eq!(
            store.mission_run(&ask.run).unwrap().unwrap().status,
            "cancelled"
        );
        assert!(
            store
                .messages(Some("agent/alder.asker"), true)
                .unwrap()
                .is_empty(),
            "the requester's own cancel tells nobody"
        );
    }

    fn gateway_decision() -> Value {
        json!({
            "version": 1,
            "type": "decision",
            "question": "Change the browser sign-in, then review the gateway again?",
            "why_person": "Only the owner decides how browsers authenticate.",
            "recommendation": {"answer": "revise-auth", "reason": "Native clients already pair this way."},
            "subjects": [{"kind": "pull_request", "label": "#41", "url": "https://example.com/pull/41", "revision": "abc123"}],
            "answers": [
                {"id": "land", "label": "Land the gateway", "outcome": "accept", "consequence": "The gateway merges as it is."},
                {"id": "keep-open", "label": "Keep it open", "outcome": "decline", "consequence": "Nothing merges."},
                {"id": "revise-auth", "label": "Revise auth and review", "outcome": "request_changes", "consequence": "The author changes sign-in and asks again."}
            ]
        })
    }

    /// A structured ask shows its request to the person, takes only a named answer, and gives
    /// the asker that answer as data when its step resumes.
    #[test]
    fn structured_ask_returns_the_named_answer_to_the_asker_as_data() {
        let (store, origin, mut input) = fixture();
        input.title = "Land the browser gateway?".into();
        input.request = Some(gateway_decision());
        let mut malformed = input.clone();
        malformed.request.as_mut().unwrap()["answers"][1]["outcome"] = json!("accept");
        assert_eq!(
            store.ask_person(&malformed).unwrap_err().code,
            "invalid-person-request"
        );
        let ask = store.ask_person(&input).unwrap();
        let mut conflict = input.clone();
        conflict.request.as_mut().unwrap()["question"] = json!("Land it now?");
        assert_eq!(
            store.ask_person(&conflict).unwrap_err().code,
            "idempotency-conflict"
        );
        let item = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.subject == ask.subject)
            .unwrap();
        let request = item.request.clone().unwrap();
        assert_eq!(request["answers"][2]["id"], "revise-auth");
        assert_eq!(request["subjects"][0]["revision"], "abc123");
        assert!(item.actions[0].argv.iter().any(|arg| arg == "--answer"));
        let mut response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: "person/avery".into(),
            summary: "yes".into(),
            evidence: vec![],
            episode: Some(item.episode.clone()),
            idempotency_key: "gateway-answer".into(),
            answer: None,
        };
        // Authority comes before the answer's shape.
        response.actor = "person/robin".into();
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "forbidden"
        );
        response.actor = "person/avery".into();
        // A reply in words never selects an answer.
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "answer-required"
        );
        response.summary = String::new();
        response.answer = Some(crate::person_request::AnswerInput {
            id: Some("revise-auth".into()),
            text: Some("Pair browsers like the native clients.".into()),
        });
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        let mut other = response.clone();
        other.answer.as_mut().unwrap().id = Some("land".into());
        other.answer.as_mut().unwrap().text = None;
        assert_eq!(
            store.finish_person_step(&other, false).unwrap_err().code,
            "idempotency-conflict"
        );
        let expected = json!({
            "type": "decision", "outcome": "request_changes", "id": "revise-auth",
            "label": "Revise auth and review", "text": "Pair browsers like the native clients."
        });
        let check = |store: &Store| {
            let resumed = store.step_run(&origin.subject).unwrap().unwrap();
            assert_eq!(resumed.status, "ready");
            assert_eq!(resumed.person_answers.len(), 1);
            let answer = &resumed.person_answers[0];
            assert_eq!(answer.ask, ask.subject);
            assert_eq!(answer.respondent, "person/avery");
            assert_eq!(answer.answer.as_ref(), Some(&expected));
            assert_eq!(
                answer.summary,
                "Revise auth and review: Pair browsers like the native clients."
            );
            assert!(resumed.constraints.iter().any(|constraint| constraint
                == "Person response: Revise auth and review: Pair browsers like the native clients."));
            let own = store.step_run(&ask.subject).unwrap().unwrap();
            assert_eq!(own.person_answers.len(), 1);
            assert_eq!(own.person_answers[0].answer.as_ref(), Some(&expected));
        };
        check(&store);
        store.replay_replication_graph().unwrap();
        check(&store);
        assert!(
            store
                .messages(Some("agent/alder.asker"), true)
                .unwrap()
                .is_empty(),
            "a step-owned ask resumes its step instead of sending a message"
        );
    }

    #[test]
    fn new_run_decision_pushes_the_named_answer_to_the_requester() {
        let (store, origin, mut input) = fixture();
        store
            .set_step_state(&origin.subject, "completed", None)
            .unwrap();
        input.step = None;
        input.new_run = Some("gateway".into());
        input.title = "Land the browser gateway?".into();
        input.request = Some(gateway_decision());
        let ask = store.ask_person(&input).unwrap();
        store
            .finish_person_step(
                &PersonStepResponse {
                    subject: ask.subject.clone(),
                    actor: "person/avery".into(),
                    summary: String::new(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: "gateway-answer".into(),
                    answer: Some(crate::person_request::AnswerInput {
                        id: Some("revise-auth".into()),
                        text: Some("Pair browsers like the native clients.".into()),
                    }),
                },
                false,
            )
            .unwrap();
        let messages = store.messages(Some("agent/alder.asker"), true).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains(
            "Answer: Revise auth and review (revise-auth)\nPair browsers like the native clients."
        ));
    }

    /// An update brings the person what they asked for: the poster keeps working, the update
    /// stays on the person's home after the poster stops, and reading it clears it without
    /// telling anyone. Work the person did not ask for takes no update.
    #[test]
    fn an_update_waits_for_nobody_stays_until_read_and_needs_the_persons_own_work() {
        let (store, origin, mut input) = fixture();
        input.step = None;
        input.incarnation = None;
        input.title = "Release notes are ready".into();
        input.reason = "The notes cover all three fixes.".into();
        input.request = Some(json!({"version": 1, "type": "update", "about": origin.run}));
        let update = store.ask_person(&input).unwrap();
        assert!(update.subject.ends_with("/update"));
        assert_eq!(store.ask_person(&input).unwrap().subject, update.subject);
        // Nothing waits on an update: the poster's step keeps its claim.
        assert_eq!(
            store.step_run(&origin.subject).unwrap().unwrap().status,
            "claimed"
        );

        // Not the person's work, not the person's message: refused.
        let mut stranger = input.clone();
        stranger.person = "person/someone-else".into();
        assert_eq!(
            store.ask_person(&stranger).unwrap_err().code,
            "update-not-asked"
        );
        let message = |subject: &str, from: &str, to: &str| {
            store.connection.batched(|tx| -> Result<()> {
                append_claim_tx(tx, "alder", subject, "message.sent", Some(from),
                    &json!({"fields": {"from": from, "to": to, "content": "How did the release go?", "status": "sent"}}), &[], None)?;
                Ok(())
            }).unwrap().unwrap();
        };
        message(
            "message/0000000000000001",
            "person/avery",
            "agent/alder.asker",
        );
        message(
            "message/0000000000000002",
            "person/avery",
            "agent/alder.other",
        );
        let mut asked = input.clone();
        asked.idempotency_key = "asked-in-message".into();
        asked.request =
            Some(json!({"version": 1, "type": "update", "about": "message/0000000000000001"}));
        let answered = store.ask_person(&asked).unwrap();
        asked.request =
            Some(json!({"version": 1, "type": "update", "about": "message/0000000000000002"}));
        assert_eq!(
            store.ask_person(&asked).unwrap_err().code,
            "update-not-asked"
        );
        asked.request = Some(json!({"version": 1, "type": "update", "about": origin.subject}));
        asked.idempotency_key = "asked-in-step".into();
        store.ask_person(&asked).unwrap();
        let mut with_step = input.clone();
        with_step.step = Some(origin.subject.clone());
        assert_eq!(
            store.ask_person(&with_step).unwrap_err().code,
            "ambiguous-ask-owner"
        );

        let stop =
            crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
                .unwrap();
        store.apply_internal(&stop, "retire-poster").unwrap();
        store.reconcile_person_asks().unwrap();
        let items = store.attention_items(Some("person/avery")).unwrap();
        let item = items
            .iter()
            .find(|item| item.subject == update.subject)
            .expect("the update outlives its poster");
        assert_eq!(item.request.as_ref().unwrap()["type"], "update");
        assert_eq!(item.actions[0].label, "read");
        assert_eq!(item.actions[0].argv[6..], ["--answer", "read"]);

        let read = |subject: &str, answer: Option<&str>, text: Option<&str>| {
            store.finish_person_step(
                &PersonStepResponse {
                    subject: subject.into(),
                    actor: "person/avery".into(),
                    summary: String::new(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: format!("read:{subject}:{answer:?}:{text:?}"),
                    answer: (answer.is_some() || text.is_some()).then(|| {
                        crate::person_request::AnswerInput {
                            id: answer.map(str::to_owned),
                            text: text.map(str::to_owned),
                        }
                    }),
                },
                false,
            )
        };
        assert_eq!(
            read(&update.subject, None, Some("Thanks, now ship it"))
                .unwrap_err()
                .code,
            "invalid-person-answer"
        );
        read(&update.subject, Some("read"), None).unwrap();
        read(&answered.subject, None, None).unwrap();
        let done = store.step_run(&update.subject).unwrap().unwrap();
        assert_eq!(done.status, "completed");
        assert_eq!(done.person_answers[0].summary, "Read");
        assert_eq!(
            done.person_answers[0].answer,
            Some(json!({"type": "update", "outcome": "read"}))
        );
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.subject != update.subject && item.subject != answered.subject)
        );
        // Reading tells nobody: an update is not a question. The one message is the person's.
        let messages = store.messages(Some("agent/alder.asker"), true).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "How did the release go?");
    }

    /// A free-text ask keeps working, takes no named answer, and keeps its claim body unchanged
    /// so older members still admit it.
    #[test]
    fn free_text_ask_takes_words_and_refuses_a_named_answer() {
        let (store, origin, input) = fixture();
        let ask = store.ask_person(&input).unwrap();
        let claim = request(&store.readers.get(), &ask.subject)
            .unwrap()
            .unwrap();
        assert!(claim.body["fields"].get("request").is_none());
        let item = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.subject == ask.subject)
            .unwrap();
        assert!(item.request.is_none());
        let mut response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: "person/avery".into(),
            summary: String::new(),
            evidence: vec![],
            episode: Some(item.episode),
            idempotency_key: "free-text".into(),
            answer: Some(crate::person_request::AnswerInput {
                id: Some("friday".into()),
                text: None,
            }),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "invalid-person-answer"
        );
        response.answer = None;
        response.summary = "Friday".into();
        store.finish_person_step(&response, false).unwrap();
        let resumed = store.step_run(&origin.subject).unwrap().unwrap();
        assert_eq!(resumed.person_answers[0].summary, "Friday");
        assert!(resumed.person_answers[0].answer.is_none());
        let done = store
            .readers
            .get()
            .query_row(
                "SELECT body FROM claims WHERE subject=?1 AND kind='work.person-done'",
                [&ask.subject],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(!done.contains("\"answer\""));
    }

    /// A feedback ask in a run of its own: the person's text is the answer, and the asker reads
    /// it from the ask step.
    #[test]
    fn new_run_feedback_ask_keeps_the_answer_on_the_ask() {
        let (store, origin, mut input) = fixture();
        store
            .set_step_state(&origin.subject, "completed", None)
            .unwrap();
        input.step = None;
        input.new_run = Some("page-feedback".into());
        input.request = Some(json!({
            "version": 1, "type": "feedback", "question": "What should the landing page say?",
            "why_person": "It speaks in the owner's voice.",
            "subjects": [{"kind": "document", "label": "Draft", "ref": "doc/example/landing"}]
        }));
        let ask = store.ask_person(&input).unwrap();
        // An older client sends only words; feedback takes them as its text.
        store
            .finish_person_step(
                &PersonStepResponse {
                    subject: ask.subject.clone(),
                    actor: "person/avery".into(),
                    summary: "Lead with the phone.".into(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: "page-feedback".into(),
                    answer: None,
                },
                false,
            )
            .unwrap();
        let own = store.step_run(&ask.subject).unwrap().unwrap();
        assert_eq!(
            own.person_answers[0].answer,
            Some(
                json!({"type": "feedback", "outcome": "feedback", "text": "Lead with the phone."})
            )
        );
        // No step waits on a new-run ask, so the answer is pushed to its requester.
        let messages = store.messages(Some("agent/alder.asker"), true).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon/runtime");
        assert_eq!(
            messages[0].title.as_deref(),
            Some("Answered: Choose a release date")
        );
        assert!(messages[0].content.contains("Lead with the phone."));
        assert!(
            messages[0]
                .content
                .contains(&format!("st work show {} --json", ask.subject))
        );
        // A replayed response does not send it again.
        store
            .finish_person_step(
                &PersonStepResponse {
                    subject: ask.subject.clone(),
                    actor: "person/avery".into(),
                    summary: "Lead with the phone.".into(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: "page-feedback".into(),
                    answer: None,
                },
                false,
            )
            .unwrap();
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store
                .messages(Some("agent/alder.asker"), true)
                .unwrap()
                .len(),
            1
        );
    }

    const TEST_FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

    fn receive(source: &Store, target: &Store) {
        let exchange = source
            .export_replication_exchange_answering(
                TEST_FLEET,
                &target.replication_inventory().unwrap(),
                &[],
            )
            .unwrap();
        receive_exchange(target, &exchange);
    }

    fn receive_exchange(target: &Store, exchange: &ReplicationExchange) {
        let before = FULL_REPLAYS.with(|count| count.get());
        target
            .receive_replication_exchange(&exchange.peer, TEST_FLEET, exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        assert!(target.project_replication_backlog().unwrap());
        assert_eq!(
            FULL_REPLAYS.with(|count| count.get()),
            before,
            "person work replayed the whole graph"
        );
        let incremental = projection_digest::tables(&target.readers.get()).unwrap();
        target.replay_replication_graph().unwrap();
        assert_eq!(
            incremental,
            projection_digest::tables(&target.readers.get()).unwrap(),
            "person work differs from full replay"
        );
    }

    #[test]
    fn person_lifecycle_replication_rebuilds_only_its_tree() {
        for mode in ["step", "standalone", "owned"] {
            let standalone = mode != "step";
            for cancel in [false, true] {
                let (source, origin, mut input) = fixture();
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("claims.sqlite3");
                let target = Store::open(&path, "birch").unwrap();
                // The first graph has no healthy frontier; its initial replay is intentional.
                target.project_replication_backlog().unwrap();
                receive(&source, &target);
                if standalone {
                    source
                        .set_step_state(&origin.subject, "completed", None)
                        .unwrap();
                    input.step = None;
                    input.new_run = Some("release-question".into());
                }
                if mode == "owned" {
                    let mut owned = crate::graph::parse_internal_intent(
                        r#"version 2
agent "alder.asker" { workspace "/tmp"; command "true"; restart always; }
"#,
                        "alder",
                    )
                    .unwrap();
                    let declaration = owned.subjects.get_mut("agent/alder.asker").unwrap();
                    declaration.owner_run = Some(origin.run.clone());
                    declaration.owner_generation = Some(origin.generation.clone());
                    source.apply_internal(&owned, "own-the-requester").unwrap();
                }
                let ask = source.ask_person(&input).unwrap();
                receive(&source, &target);
                assert_eq!(
                    target.step_run(&ask.subject).unwrap().unwrap().status,
                    "ready"
                );
                if !standalone {
                    assert_eq!(
                        target.step_run(&origin.subject).unwrap().unwrap().status,
                        "waiting-person"
                    );
                }
                source
                    .finish_person_step(
                        &PersonStepResponse {
                            subject: ask.subject.clone(),
                            actor: if cancel { input.actor } else { input.person },
                            summary: "Release reviewed".into(),
                            evidence: vec![],
                            episode: None,
                            idempotency_key: "review-release".into(),
                            answer: None,
                        },
                        cancel,
                    )
                    .unwrap();
                receive(&source, &target);
                assert_eq!(
                    target.step_run(&ask.subject).unwrap().unwrap().status,
                    if cancel { "cancelled" } else { "completed" }
                );
                if !standalone {
                    assert_eq!(
                        target.step_run(&origin.subject).unwrap().unwrap().status,
                        "ready"
                    );
                }
                target
                    .replication_status(true, Some(TEST_FLEET), &[])
                    .unwrap();
                let before = projection_digest::tables(&target.readers.get()).unwrap();
                drop(target);
                let reopened = Store::open(&path, "birch").unwrap();
                assert_eq!(
                    before,
                    projection_digest::tables(&reopened.readers.get()).unwrap()
                );
                assert_eq!(
                    before,
                    projection_digest::oracle(&reopened.readers.get()).unwrap()
                );
            }
        }
    }

    #[test]
    fn person_response_before_standalone_ask_matches_full_replay() {
        let (source, origin, mut input) = fixture();
        source
            .set_step_state(&origin.subject, "completed", None)
            .unwrap();
        let target = Store::open_memory("birch").unwrap();
        target.project_replication_backlog().unwrap();
        receive(&source, &target);
        input.step = None;
        input.new_run = Some("release-question".into());
        let ask = source.ask_person(&input).unwrap();
        let asking = source
            .export_replication_exchange_answering(
                TEST_FLEET,
                &target.replication_inventory().unwrap(),
                &[],
            )
            .unwrap();
        source
            .finish_person_step(
                &PersonStepResponse {
                    subject: ask.subject.clone(),
                    actor: input.actor,
                    summary: "No longer needed".into(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: "cancel-release".into(),
                    answer: None,
                },
                true,
            )
            .unwrap();
        let mut response = source
            .export_replication_exchange_answering(
                TEST_FLEET,
                &target.replication_inventory().unwrap(),
                &[],
            )
            .unwrap();
        response
            .envelopes
            .retain(|envelope| !asking.envelopes.iter().any(|old| old.hash == envelope.hash));
        receive_exchange(&target, &response);
        assert!(target.step_run(&ask.subject).unwrap().is_none());
        receive_exchange(&target, &asking);
        assert_eq!(
            target.step_run(&ask.subject).unwrap().unwrap().status,
            "cancelled"
        );
    }

    #[test]
    fn populated_person_receive_does_not_block_claims_and_renewals() {
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};
        let (source, origin, mut input) = fixture();
        source
            .set_step_state(&origin.subject, "completed", None)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = Store::open(&directory.path().join("claims.sqlite3"), "birch").unwrap();
        target.project_replication_backlog().unwrap();
        receive(&source, &target);
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
agent "birch.worker" { workspace "/tmp"; command "true"; restart always; }
mission "writer-load" state="ready" {
 goal "Write while a peer receives.";
 step "write" { assigned-to "agent/birch.worker"; goal "Keep working."; }
}"#,
            "birch",
        )
        .unwrap();
        target.apply_internal(&intent, "writer-load").unwrap();
        let run = target
            .create_mission_run(&MissionRunRequest {
                mission: "writer-load".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/avery".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "writer-load-run".into(),
            })
            .unwrap();
        let worker = &run.steps[0];
        target.connection.batched(|tx| -> Result<()> {
            let claim = append_claim_tx(tx, "birch", &worker.subject, "work.claimed", Some("agent/birch.worker"),
                &json!({"fields":{"attempt":1,"status":"claimed","claimant":"agent/birch.worker",
                    "claim_incarnation":"worker-one","claim_expires_at_unix_ms":(now_ms()+600_000) as u64}}), &[], None)?;
            project_mission_run_update(tx, &claim).unwrap();
            // These are valid retained claims on unrelated subjects, not the run being updated.
            for n in 0..20_000 {
                append_claim_tx(tx, "birch", "daemon/birch", "daemon.diagnostic", None,
                    &json!({"fields":{"severity":"warning","code":"retained","reason":format!("history {n}")}}), &[], None)?;
            }
            Ok(())
        }).unwrap().unwrap();
        target.project_replication_backlog().unwrap();
        input.step = None;
        input.new_run = Some("release-question".into());
        let ask = source.ask_person(&input).unwrap();
        source
            .finish_person_step(
                &PersonStepResponse {
                    subject: ask.subject.clone(),
                    actor: input.actor,
                    summary: "No longer needed".into(),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: "cancel-release".into(),
                    answer: None,
                },
                true,
            )
            .unwrap();
        let exchange = source
            .export_replication_exchange_answering(
                TEST_FLEET,
                &target.replication_inventory().unwrap(),
                &[],
            )
            .unwrap();
        let gate = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let receiving = scope.spawn(|| {
                gate.wait();

                let before = FULL_REPLAYS.with(|count| count.get());
                let start = Instant::now();
                target
                    .receive_replication_exchange(&exchange.peer, TEST_FLEET, &exchange)
                    .unwrap();
                target.validate_replication_backlog().unwrap();
                target.apply_replication_repairs().unwrap();
                target.project_replication_backlog().unwrap();
                assert_eq!(FULL_REPLAYS.with(|count| count.get()), before);
                start.elapsed()
            });
            let writing = scope.spawn(|| {
                gate.wait();

                let start = Instant::now();
                for n in 0..10 {
                    target.append_claim(&ClaimInput {
                        subject: "daemon/birch".into(), kind: "daemon.diagnostic".into(), actor: None,
                        fields: serde_json::from_value(json!({"severity":"warning","code":"live","reason":format!("claim {n}")})).unwrap(),
                        evidence: vec![], expected_subject: None, idempotency_key: None,
                    }).unwrap();
                }
                start.elapsed()
            });
            gate.wait();

            let start = Instant::now();
            for n in 0..10 {
                target
                    .work_action(
                        &worker.subject,
                        "renew",
                        &WorkRequest {
                            actor: Some("agent/birch.worker".into()),
                            incarnation: Some("worker-one".into()),
                            summary: None,
                            reason: None,
                            evidence: vec![],
                            idempotency_key: format!("load-renewal-{n}"),
                        },
                    )
                    .unwrap();
            }
            let renewing = start.elapsed();

            for (label, elapsed) in [
                ("receive/apply", receiving.join().unwrap()),
                ("claims", writing.join().unwrap()),
                ("renewals", renewing),
            ] {
                assert!(
                    elapsed < Duration::from_secs(2),
                    "{label} queued for {elapsed:?}"
                );
            }
        });
        assert_eq!(
            target.step_run(&ask.subject).unwrap().unwrap().status,
            "cancelled"
        );
        target.project_replication_backlog().unwrap();
        let incremental = projection_digest::tables(&target.readers.get()).unwrap();
        target.replay_replication_graph().unwrap();
        assert_eq!(
            incremental,
            projection_digest::tables(&target.readers.get()).unwrap()
        );
        assert_eq!(
            incremental,
            projection_digest::oracle(&target.readers.get()).unwrap()
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
            answer: None,
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
                answer: None,
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
        store
            .set_step_state(&origin.subject, "ready", None)
            .unwrap();
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
        assert_eq!(
            store.step_run(&origin.subject).unwrap().unwrap().status,
            "waiting-person"
        );
        assert_eq!(
            store
                .step_run(&origin.subject)
                .unwrap()
                .unwrap()
                .blocked_reason,
            Some(fresh.subject)
        );
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
            answer: None,
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "stale-fence"
        );
    }
}
