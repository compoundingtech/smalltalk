use super::*;
use crate::rollout::{Operation, Policy, Selection};

/// Both frontiers fence a safe-point read against shared claims and driver-local reports.
fn restart_frontier(connection: &Connection) -> Result<(u64, u64)> {
    connection.query_row(
        "SELECT (SELECT COALESCE(MAX(store_index),0) FROM claims),
                (SELECT COALESCE(MAX(id),0) FROM local_observations)",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(Into::into)
}

fn restart_cutover_request(connection: &Connection, subject: &str) -> Result<Option<ClaimRecord>> {
    let Some(desired) = current_desired_row(connection, subject)? else {
        return Ok(None);
    };
    let (_, owner, conflict) = selected_actual_source_at(connection, subject, None, None)?;
    let Some(actual) = latest_actual(connection, subject)?.filter(|actual| actual["status"] == "running") else {
        return Ok(None);
    };
    if conflict { return Ok(None); }
    let incarnation = actual["incarnation_id"].as_str();
    let request = connection.query_row(&canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
         FROM claims WHERE subject=?1 AND kind='runtime.action.requested'
            AND origin=?2 AND json_extract(body,'$.fields.action')='apply-restart-cutover'
            AND json_extract(body,'$.fields.rollout.desired_token')=?3
            AND json_extract(body,'$.fields.incarnation_id')=?4
            AND NOT EXISTS (SELECT 1 FROM claims ended WHERE ended.subject=claims.subject
                AND ended.origin=claims.origin AND ended.kind='runtime.action.failed'
                AND json_extract(ended.body,'$.fields.operation')=claims.id
                AND json_extract(ended.body,'$.fields.operation_status')='aborted')
         ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
        params![subject, owner, desired.claim_id, incarnation], claim_from_row,
    ).optional()?;
    if request.is_none() { return Ok(None); }
    let evaluated = connection.query_row(
        "SELECT subject,kind,body,member,owner_run,owner_generation,owner_step FROM desired WHERE subject=?1",
        [subject], desired_from_row,
    )?;
    // Old or externally produced barriers cannot hold a seat whose launch is already current.
    if running_restart_at(connection, &evaluated, None).map_err(internal)?.is_none() {
        return Ok(None);
    }
    Ok(request)
}

fn restart_cutover(connection: &Connection, subject: &str) -> Result<bool> {
    Ok(restart_cutover_request(connection, subject)?.is_some())
}

fn selection(connection: &Connection, subject: &str) -> Result<Option<Selection>, St3Error> {
    owned_sets::guard_member(connection, subject)?;
    let Some(row) = current_desired_row(connection, subject).map_err(internal)? else {
        return Ok(None);
    };
    let desired = connection.query_row("SELECT subject,kind,body,member,owner_run,owner_generation,owner_step FROM desired WHERE subject=?1",
        [subject],desired_from_row).map_err(internal)?;
    for view in owned_sets::selected(connection, None)? {
        let members = owned_sets::effective_members(connection, &view, None)?;
        let Some((member, _, _)) = members.get(subject) else {
            continue;
        };
        // Retirement keeps the last authored seat policy even though its desired node is stop.
        let manual = if desired.kind == "stop" {
            owned_sets::manual_member(connection, member)?
        } else {
            crate::rollout::manual(&desired)
        };
        let policy = match view.receipt.rollout.clone() {
            Some(policy) => policy,
            None if manual => Policy::when_idle(30 * 60 * 1000, false),
            None => return Ok(None),
        };
        let actor = claim_by_id_tx(connection, &view.claim)
            .map_err(internal)?
            .and_then(|claim| claim.actor);
        let target = crate::rollout::target(&desired).map_err(internal)?;
        return Ok(Some(Selection {
            set: view.id,
            receipt: view.claim,
            source: view.receipt.source,
            policy,
            manual,
            desired_token: row.claim_id,
            target,
            desired,
            actor,
        }));
    }
    Ok(None)
}

fn operation(connection: &Connection, subject: &str) -> Result<Option<Operation>, St3Error> {
    let Some(selected) = selection(connection, subject)? else {
        return Ok(None);
    };
    operation_for_selection(connection, subject, &selected)
}

fn operation_for_selection(
    connection: &Connection,
    subject: &str,
    selected: &Selection,
) -> Result<Option<Operation>, St3Error> {
    smallclaims::touched::note_read(|| subject.to_owned());
    let mut query = connection.prepare_cached(&canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
         FROM claims WHERE subject=?1 AND kind IN ('runtime.action.requested','runtime.action.succeeded','runtime.action.failed','runtime.action.deadline-reached')
         ORDER BY CANONICAL_ASC(claims)" )).map_err(internal)?;
    let claims = query
        .query_map([subject], claim_from_row)
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let owner = selected
        .desired
        .member
        .as_ref()
        .map(|m| m.host.clone())
        .or_else(|| {
            selected_actual_source_at(connection, subject, None, None)
                .ok()
                .and_then(|v| v.1)
        });
    let request = claims.iter().rev().find(|claim| {
        claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("rollout")
            && claim.kind == "runtime.action.requested"
            && claim.actor.is_some()
            && owner.as_deref() == Some(claim.origin.as_str())
    });
    let Some(request) = request else {
        return Ok(None);
    };
    let Some(data) = request.body.pointer("/fields/rollout") else {
        return Ok(None);
    };
    let Ok(mut operation) = serde_json::from_value::<Operation>(data.clone()) else {
        return Ok(None);
    };
    operation.id = request.id.clone();
    operation.requested_by = request.actor.clone();
    // The initial request can carry a positively stopped predecessor after supersession.
    for claim in claims.iter().filter(|c| {
        c.origin == request.origin
            && c.actor == request.actor
            && c.body.pointer("/fields/operation").and_then(Value::as_str)
                == Some(request.id.as_str())
    }) {
        if let Some(phase) = claim
            .body
            .pointer("/fields/operation_status")
            .and_then(Value::as_str)
        {
            if phase == "drain-ack" {
                operation.drain_ack = Some(claim.accepted_at_unix_ms);
                continue;
            }
            if phase == "start-attempted" {
                if let Some(token) = claim
                    .body
                    .pointer("/fields/rollout/desired_token")
                    .and_then(Value::as_str)
                {
                    operation.desired_token = token.into();
                }
                operation.start_attempted = true;
                continue;
            }
            operation.phase = if phase == "failed-replacement" {
                "failed"
            } else {
                phase
            }
            .into();
            operation.phase_at_unix_ms = claim.accepted_at_unix_ms;
            if let Some(state) = claim.body.pointer("/fields/rollout") {
                if let Some(token) = state["desired_token"].as_str() {
                    operation.desired_token = token.into();
                }
                operation.native_session_id = state["native_session_id"]
                    .as_str()
                    .map(str::to_owned)
                    .or(operation.native_session_id);
                operation.native_account = state["native_account"]
                    .as_str()
                    .map(str::to_owned)
                    .or(operation.native_account);
                operation.native_path = state["native_path"]
                    .as_str()
                    .map(str::to_owned)
                    .or(operation.native_path);
                operation.replacement_incarnation = state["replacement_incarnation"]
                    .as_str()
                    .map(str::to_owned)
                    .or(operation.replacement_incarnation);
                operation.forced |= state["forced"].as_bool().unwrap_or(false);
                operation.blocking = state["blocking"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                operation.reason = state["reason"].as_str().map(str::to_owned);
            }
        }
    }
    if selected.set == operation.set
        && selected.target == operation.target
        && (selected.policy != operation.publication_policy
            || selected.manual != operation.publication_manual)
        && matches!(operation.phase.as_str(), "running" | "retired")
    {
        return Ok(None);
    }
    if selected.set != operation.set
        || selected.target != operation.target
        || (selected.policy != operation.publication_policy
            || selected.manual != operation.publication_manual)
    {
        operation.phase = "superseded".into();
    }
    Ok(Some(operation))
}

/// A pending person ask remains attached to the still-running retiring seat until
/// its existing work finishes or the original incarnation actually ends.
pub(super) fn retiring_ask_live(
    connection: &Connection,
    ask: &ClaimRecord,
) -> Result<bool, St3Error> {
    let Some(subject) = ask.actor.as_deref() else {
        return Ok(false);
    };
    let retiring: bool = connection.query_row(
        "SELECT kind='stop' FROM desired WHERE subject=?1", [subject], |row| row.get(0),
    ).optional().map_err(internal)?.unwrap_or(false);
    if !retiring { return Ok(false); }
    let Some(selected) = selection(connection, subject)? else {
        return Ok(false);
    };
    if selected.desired.kind != "stop" {
        return Ok(false);
    }
    let fields = &ask.body["fields"];
    let Some(step) = fields["origin_step"].as_str() else {
        return Ok(false);
    };
    let Some(attempt) = fields["origin_attempt"].as_u64() else {
        return Ok(false);
    };
    let actual = latest_actual(connection, subject)
        .map_err(internal)?
        .unwrap_or(Value::Null);
    let Some(incarnation) = actual["incarnation_id"]
        .as_str()
        .filter(|_| actual["status"] == "running")
    else {
        return Ok(false);
    };
    if let Some(operation) = operation(connection, subject)? {
        return Ok(operation.holds_seat()
            && operation.old_incarnation == incarnation
            && operation.allowed_work.get(step).copied().map(u64::from) == Some(attempt));
    }
    // Publication can precede the owner's first reconciliation pass.
    connection
        .query_row(
            "SELECT status='waiting-person' AND attempt=?2 FROM step_runs WHERE subject=?1",
            params![step, attempt],
            |row| row.get(0),
        )
        .optional()
        .map(|result| result.unwrap_or(false))
        .map_err(internal)
}

pub(super) fn intake_held(
    connection: &Connection,
    subject: &str,
    step: &str,
) -> Result<bool, St3Error> {
    if restart_cutover(connection, subject).map_err(internal)? {
        return Ok(true);
    }
    let Some(operation) = operation(connection, subject)?.filter(|o| o.holds_intake()) else {
        return Ok(false);
    };
    let attempt: Option<u32> = connection
        .query_row(
            "SELECT attempt FROM step_runs WHERE subject=?1",
            [step],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?;
    Ok(operation.allowed_work.get(step).copied() != attempt || attempt.is_none())
}

pub(super) fn message_allowed(
    connection: &Connection,
    message: &MessageView,
) -> Result<bool, St3Error> {
    if restart_cutover(connection, &message.to).map_err(internal)? {
        return Ok(false);
    }
    let Some(operation) = operation(connection, &message.to)?.filter(|o| o.holds_intake()) else {
        return Ok(true);
    };
    if message.status != "sent" {
        return Ok(true);
    }
    if message.from == "daemon/runtime"
        && message.tags.iter().any(|tag| {
            let Some(value) = tag.strip_prefix("st3-work:") else {
                return false;
            };
            let mut parts = value.rsplitn(4, '@');
            let _incarnation = parts.next();
            let _epoch = parts.next();
            let attempt = parts.next().and_then(|s| s.parse::<u32>().ok());
            parts
                .next()
                .is_some_and(|step| operation.allowed_work.get(step).copied() == attempt)
        })
    {
        return Ok(true);
    }
    let mut parent = message.in_reply_to.clone();
    let mut visited = BTreeSet::new();
    while let Some(subject) = parent {
        if !visited.insert(subject.clone()) || visited.len() > 64 {
            break;
        }
        let index: Option<u64> = connection
            .query_row(
                "SELECT MIN(store_index) FROM claims WHERE subject=?1",
                [&subject],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let Some(index) = index else {
            break;
        };
        let prior = message_view_tx(connection, &subject, index).map_err(internal)?;
        let request_index: u64 = connection
            .query_row(
                "SELECT store_index FROM claims WHERE id=?1",
                [&operation.id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if index < request_index
            && (prior.from == message.to || (prior.to == message.to && prior.status != "sent"))
        {
            return Ok(true);
        }
        parent = prior.in_reply_to;
    }
    Ok(false)
}

impl Store {
    /// Bind the policy/token to the declaration the caller actually evaluated, before proof reads.
    pub(crate) fn restart_policy_for(
        &self, evaluated: &DesiredSubject,
    ) -> Result<Option<(String, Option<crate::rollout::DeferredRestart>)>> {
        let connection = self.readers.get();
        let Some(row) = current_desired_row(&connection, &evaluated.subject)? else {
            return Ok(None);
        };
        let current = connection.query_row(
            "SELECT subject,kind,body,member,owner_run,owner_generation,owner_step FROM desired WHERE subject=?1",
            [&evaluated.subject], desired_from_row,
        )?;
        if current != *evaluated { return Ok(None); }
        let claim = claim_by_id_tx(&connection, &row.claim_id)?;
        let pending = claim.and_then(|claim| claim.body.get("deferred_restart").cloned())
            .map(serde_json::from_value).transpose()?;
        Ok(Some((row.claim_id, pending)))
    }

    pub(crate) fn abort_restart_cutover(&self, subject: &str, token: &str, reason: &str) -> Result<()> {
        self.connection.batched(|tx| -> Result<()> {
            let Some(request) = restart_cutover_request(tx, subject)? else { return Ok(()); };
            if request.body.pointer("/fields/rollout/desired_token").and_then(Value::as_str) != Some(token) {
                return Ok(());
            }
            append_claim_tx(tx, self.origin(), subject, "runtime.action.failed", Some(subject),
                &json!({"fields":{"action":"apply-restart-cutover","operation":request.id,
                    "operation_status":"aborted","reason":reason},"evidence":[request.id]}),
                &[], None)?;
            Ok(())
        }).map_err(anyhow::Error::msg)?
    }

    pub(crate) fn restart_cutover(&self, subject: &str) -> Result<bool> {
        restart_cutover(&self.readers.get(), subject)
    }

    pub(crate) fn restart_frontier(&self) -> Result<(u64, u64)> {
        restart_frontier(&self.readers.get())
    }

    /// Keep publication-era asks attached to their running incumbent. The same
    /// origin-step/attempt fence is used by owned rollouts to allow retiring work.
    pub(crate) fn restart_person_work_pending(&self, subject: &str) -> Result<bool> {
        Ok(self.readers.get().query_row(
            "SELECT EXISTS(SELECT 1 FROM step_runs step JOIN claims ask
                ON json_extract(ask.body,'$.fields.origin_step')=step.subject
                WHERE step.status IN ('waiting-person','ready') AND ask.kind='work.person-asked'
                AND ask.actor=?1 AND json_extract(ask.body,'$.fields.origin_attempt')=step.attempt)",
            [subject], |row| row.get(0),
        )?)
    }

    /// A proof taken outside the writer is accepted only if neither frontier changed.
    /// Claim intake uses this same writer, so it either precedes the proof or sees the fence.
    pub(crate) fn commit_restart_cutover(
        &self, evaluated: &DesiredSubject, desired_token: &str, incarnation: &str,
        frontier: (u64, u64), deadline: u128,
    ) -> Result<bool> {
        let subject = &evaluated.subject;
        self.connection.batched(|tx| -> Result<bool> {
            if restart_frontier(tx)? != frontier || now_ms() >= deadline {
                return Ok(false);
            }
            let Some(desired) = current_desired_row(tx, subject)? else { return Ok(false); };
            let (_, owner, conflict) = selected_actual_source_at(tx, subject, None, None)?;
            let actual = latest_actual(tx, subject)?;
            let current = tx.query_row(
                "SELECT subject,kind,body,member,owner_run,owner_generation,owner_step FROM desired WHERE subject=?1",
                [subject], desired_from_row,
            )?;
            if desired.claim_id != desired_token || current != *evaluated || conflict
                || owner.as_deref() != Some(self.origin())
                || actual.as_ref().is_none_or(|a| a["status"] != "running" || a["incarnation_id"] != incarnation)
                || running_restart_at(tx, &current, None).map_err(internal)?.is_none()
            {
                return Ok(false);
            }
            append_claim_tx(tx, self.origin(), subject, "runtime.action.requested", Some(subject),
                &json!({"fields":{"action":"apply-restart-cutover","incarnation_id":incarnation,
                    "rollout":{"desired_token":desired_token}},"evidence":[desired_token]}),
                &[], None)?;
            Ok(true)
        }).map_err(anyhow::Error::msg)?
    }

    pub fn rollout_message_allowed(&self, message: &MessageView) -> Result<bool, St3Error> {
        message_allowed(&self.readers.get(), message)
    }
    pub fn rollout_selection(&self, subject: &str) -> Result<Option<Selection>, St3Error> {
        selection(&self.readers.get(), subject)
    }
    pub fn rollout(&self, subject: &str) -> Result<Option<Operation>, St3Error> {
        self.rollout_and_selection(subject)
            .map(|(operation, _)| operation)
    }
    /// A status read uses the publication that selected its operation, including when that
    /// operation is absent or superseded, rather than selecting the declaration a second time.
    pub(crate) fn rollout_and_selection(
        &self,
        subject: &str,
    ) -> Result<(Option<Operation>, Option<Selection>), St3Error> {
        let connection = self.readers.get();
        let selected = selection(&connection, subject)?;
        let mut operation = selected
            .as_ref()
            .map(|selected| operation_for_selection(&connection, subject, selected))
            .transpose()?
            .flatten();
        if let Some(operation) = operation.as_mut().filter(|o| o.phase == "draining") {
            operation.blocking =
                crate::rollout::blockers(self, subject, operation).map_err(internal)?;
        }
        Ok((operation, selected))
    }
    /// Capture publication and incarnation fences atomically with the durable request.
    #[allow(clippy::too_many_arguments)]
    pub fn request_rollout(
        &self,
        subject: &str,
        expected_token: &str,
        old: &crate::model::MemberSpec,
        incarnation: &str,
        actor: &str,
        policy: &Policy,
        key: &str,
    ) -> Result<ClaimRecord, St3Error> {
        policy
            .validate()
            .map_err(|e| St3Error::new("invalid-rollout-policy", e.to_string()))?;
        self.connection.batched(|tx| -> Result<ClaimRecord, St3Error> {
            if let Some(prior) = tx.query_row(
                "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims
                 WHERE subject=?1 AND kind='runtime.action.requested' AND origin=?2 AND json_extract(body,'$.fields.operation')=?3",
                params![subject,self.origin(),key], claim_from_row).optional().map_err(internal)? {
                let fields = &prior.body["fields"]["rollout"];
                if prior.actor.as_deref() != Some(actor) || fields["desired_token"] != expected_token
                    || fields["old_incarnation"] != incarnation || fields["policy"] != json!(policy) {
                    return Err(St3Error::new("idempotency-key-mismatch", "rollout request key already names another request"));
                }
                return Ok(prior);
            }
            let selected = selection(tx, subject)?.ok_or_else(|| St3Error::new("rollout-not-enabled", "publish an owned set with --rollout when-idle or a rollout manual seat first"))?;
            if selected.desired_token != expected_token {
                return Err(St3Error::new("stale-rollout-target", "selected declaration changed; read it again"));
            }
            let actual = latest_actual(tx, subject).map_err(internal)?.unwrap_or(Value::Null);
            let prior = operation(tx,subject)?;
            let ended = matches!(actual["status"].as_str(),Some("stopped"|"exited"|"vanished"));
            let carry = prior.as_ref().filter(|o| o.native_session_id.is_some()
                && (o.old_incarnation == incarnation || o.replacement_incarnation.as_deref() == Some(incarnation)));
            // A pending manual publication can outlive its incumbent. Capture its exact
            // native binding in this transaction; an explicit request still requires positive
            // runtime exit before replacement and never falls back to a fresh conversation.
            let ended_binding = if selected.manual && ended && carry.is_none() {
                tx.query_row(&canonical_sql("SELECT body FROM claims WHERE subject=?1 AND kind='harness.session-file'
                    AND json_extract(body,'$.fields.incarnation_id')=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                    params![subject,incarnation], |row| row.get::<_,String>(0)).optional().map_err(internal)?
                    .map(|body| serde_json::from_str::<Value>(&body)).transpose().map_err(internal)?
            } else { None };
            let ended_binding = ended_binding.as_ref().map(|body| &body["fields"])
                .filter(|fields| fields["harness"].as_str() == old.driver.as_deref());
            let ended_session = ended_binding.and_then(|fields| fields["session_id"].as_str())
                .filter(|session| !session.trim().is_empty());
            if actual["incarnation_id"].as_str() != Some(incarnation) || (actual["status"] != "running" && !(ended && (carry.is_some() || ended_session.is_some()))) {
                return Err(St3Error::new("stale-rollout-incarnation", "rollout needs the exact running incarnation"));
            }
            if old.host != self.origin() || selected.desired.member.as_ref().is_some_and(|new| new.host != old.host || new.driver != old.driver)
                || !matches!(old.driver.as_deref(), Some("claude" | "codex" | "pi" | "omp" | "opencode")) {
                return Err(St3Error::new("unsupported-rollout", "rollout requires a supported native harness on its current host"));
            }
            let now = now_ms();
            let mut asks = tx.prepare_cached("SELECT step.subject,step.attempt FROM step_runs step
                JOIN claims ask ON json_extract(ask.body,'$.fields.origin_step')=step.subject
                WHERE step.status IN ('waiting-person','ready') AND ask.kind='work.person-asked' AND ask.actor=?1
                    AND json_extract(ask.body,'$.fields.origin_attempt')=step.attempt").map_err(internal)?;
            let allowed_work = asks.query_map([subject],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u32>(1)?))).map_err(internal)?
                .collect::<Result<BTreeMap<_,_>,_>>().map_err(internal)?;
            let operation = Operation { id: String::new(), set: selected.set, receipt: selected.receipt,
                source: selected.source, desired_token: selected.desired_token, target: selected.target,
                policy: policy.clone(), publication_policy: selected.policy, publication_manual: selected.manual, old_incarnation: incarnation.into(),
                old_member: old.clone(), deadline_unix_ms: now.saturating_add(policy.deadline_ms.into()),
                requested_by: Some(actor.into()), requested_at_unix_ms: now, phase: if ended { "stopping" } else { "draining" }.into(),
                phase_at_unix_ms: now, drain_ack: None, start_attempted: false, allowed_work, native_session_id: carry.and_then(|o|o.native_session_id.clone()).or_else(|| ended_session.map(str::to_owned)),
                native_path: carry.and_then(|o|o.native_path.clone()).or_else(|| ended_binding.and_then(|f|f["path"].as_str()).map(str::to_owned)),
                native_account: carry.and_then(|o|o.native_account.clone()).or_else(|| ended_binding.and_then(|f|f["account_ref"].as_str()).map(str::to_owned)), replacement_incarnation: None,
                forced: false, blocking: Vec::new(), reason: None };
            let claim = append_claim_tx(tx, self.origin(), subject, "runtime.action.requested", Some(actor),
                &json!({"fields":{"action":"rollout","operation":key,"rollout":operation},"evidence":[expected_token]}),
                &[], None).map_err(internal)?;
            Ok(claim)
        }).map_err(|e| St3Error::new("internal", e))?
    }
}
