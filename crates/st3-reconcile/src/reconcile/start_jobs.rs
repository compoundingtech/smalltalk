//! Launch workers never occupy the planner/control blocking pool. Their durable results wake it.
use super::*;
use crate::store::{RuntimeStartAdmission, RuntimeStartOperation};
use serde::{Deserialize, Serialize};
use std::sync::Condvar;

// One held launch still leaves a slot for an unrelated seat. A conservative cap avoids
// the simultaneous harness startup oversubscription documented in #2465.
pub const MAX_START_JOBS: usize = 2;
pub const OPERATION_TAG: &str = "st3.start-operation";
pub const OPERATION_ENV: &str = "ST3_START_OPERATION";

pub struct StartJobs {
    owner: String,
    pub live: Arc<(Mutex<HashSet<String>>, Condvar)>,
}
impl Default for StartJobs {
    fn default() -> Self {
        Self {
            owner: uuid::Uuid::now_v7().to_string(),
            live: Arc::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedStart {
    pub subject: DesiredSubject,
    pub member: MemberSpec,
    pub launch_member: MemberSpec,
    pub reason: String,
    pub request: Option<crate::model::ClaimRecord>,
    pub desired_token: String,
    pub selected_token: Option<String>,
    pub placement_evidence: Vec<String>,
    pub start_request: String,
    #[serde(default)]
    pub start_request_receipt: Option<crate::model::ClaimRecord>,
    #[serde(default)]
    pub control_request: Option<String>,
    #[serde(default)]
    pub control_request_receipt: Option<crate::model::ClaimRecord>,
    pub expected_incarnation: Option<String>,
    pub resume_operation: Option<String>,
}

impl PreparedStart {
    fn requests_retained(&self, store: &Store) -> Result<bool> {
        let Some(original) = &self.start_request_receipt else {
            return Ok(false);
        };
        if original.id != self.start_request || !store.runtime_start_request_retained(original)? {
            return Ok(false);
        }
        match (&self.control_request, &self.control_request_receipt) {
            (Some(id), Some(control))
                if control.id == *id && store.runtime_start_request_retained(control)? => {}
            (None, None) => {}
            _ => return Ok(false),
        }
        if let Some(request) = &self.request
            && !store.runtime_start_request_retained(request)?
        {
            return Ok(false);
        }
        Ok(true)
    }
}

pub enum StartCompletion {
    Superseded,
    Applied(Option<String>),
}

impl<R: RuntimeControl> Reconciler<R> {
    pub fn dispatch_start(&self, mut prepared: PreparedStart) -> Result<bool> {
        let id = uuid::Uuid::now_v7().to_string();
        prepared
            .launch_member
            .tags
            .insert(OPERATION_TAG.into(), id.clone());
        prepared
            .launch_member
            .environment
            .insert(OPERATION_ENV.into(), id.clone());
        let mut operation = RuntimeStartOperation {
            id: id.clone(),
            subject: prepared.subject.subject.clone(),
            owner: self.start_jobs.owner.clone(),
            body: serde_json::to_value(&prepared)?,
            revoked_by: None,
            finished: false,
            result: None,
            applied: false,
            cleanup_verified: false,
            dispatched: false,
            receipted: false,
        };
        match self
            .store
            .admit_runtime_start(&mut operation, MAX_START_JOBS)?
        {
            RuntimeStartAdmission::SeatBusy | RuntimeStartAdmission::Superseded => {
                return Ok(false);
            }
            RuntimeStartAdmission::Admitted | RuntimeStartAdmission::CapacityBusy => {}
        }
        self.start_changed(&operation.subject);
        self.start_queued_job(&operation)?;
        // Test-support's explicit single-pass fixtures await the same worker/result path.
        // The daemon and the real run loop always dispatch without waiting.
        #[cfg(any(test, feature = "test-support"))]
        if self.wait_for_test_starts.load(Ordering::Acquire) {
            let (live, changed) = &*self.start_jobs.live;
            let mut live = live.lock().unwrap_or_else(PoisonError::into_inner);
            while live.contains(&id) {
                live = changed.wait(live).unwrap_or_else(PoisonError::into_inner);
            }
            drop(live);
            self.reconcile_start_jobs()?;
            if let Some(error) = self.store.runtime_start_failure(&operation.subject)? {
                anyhow::bail!(error);
            }
        }
        Ok(true)
    }

    pub fn start_changed(&self, subject: &str) {
        self.incremental
            .touch(&format!("local-runtime-start:{subject}"));
        self.incremental.touch(subject);
        self.signal_changed();
    }

    pub fn start_queued_job(&self, operation: &RuntimeStartOperation) -> Result<()> {
        if operation.dispatched || operation.finished || operation.revoked_by.is_some() {
            return Ok(());
        }
        let Some(lock) = self.store.try_runtime_start_lock(&operation.subject)? else {
            return Ok(());
        };
        if !self
            .store
            .dispatch_runtime_start(&operation.id, MAX_START_JOBS)?
        {
            return Ok(());
        }
        let prepared: PreparedStart = serde_json::from_value(operation.body.clone())?;
        let store = self.store.clone();
        let runtime = self.runtime.clone();
        let notify = self.notify.clone();
        let events = self.event_notify.clone();
        let incremental = self.incremental.clone();
        let live = self.start_jobs.live.clone();
        let id = operation.id.clone();
        let subject = operation.subject.clone();
        let owner = operation.owner.clone();
        live.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone());
        let spawned = std::thread::Builder::new().name("st3-runtime-start".into()).spawn(move || crate::task_cpu::blocking("runtime.start", || {
            let _lock = lock;
            let guard = || -> Result<()> {
                anyhow::ensure!(!store.runtime_start_revoked(&id)?, "start operation was revoked");
                store.owned_desired_guard(&prepared.subject)?;
                anyhow::ensure!(store.selected_desired_token(&subject)? == prepared.selected_token,
                    "declaration changed before launch");
                anyhow::ensure!(prepared.requests_retained(&store)?,
                    "an original start request is no longer retained unchanged");
                if let Some(rollout) = prepared.member.environment.get(crate::rollout::OPERATION_ENV) {
                    anyhow::ensure!(store.rollout(&subject)?.is_some_and(|o| o.id == *rollout && o.phase == "starting"),
                        "rollout changed before launch");
                }
                Ok(())
            };
            let mut backend_entered = false;
            let result = caught(|| {
                guard()?;
                backend_entered = true;
                runtime.start_guarded(&prepared.launch_member, &guard)?;
                let (observation, observation_error) = match runtime.observe_start(&prepared.launch_member) {
                    Ok(observation) => (observation, None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                let incarnation = observation.and_then(|o| o.incarnation_id);
                let (verified, verification_error) = match incarnation.as_deref() {
                    Some(incarnation) => match runtime.start_operation(&prepared.member.runtime_id, prepared.member.terminal, incarnation) {
                        Ok(operation) => (operation.as_deref() == Some(id.as_str()), None),
                        Err(error) => (false, Some(format!("{error:#}"))),
                    },
                    None => (false, observation_error),
                };
                Ok((incarnation, verified, verification_error))
            });
            let mut result = match result {
                Ok((incarnation, verified, error)) => serde_json::json!({"incarnation": if verified {incarnation.clone()} else {None}, "candidate_incarnation": incarnation, "verification": if verified {"operation-incarnation"} else {"launch-accepted-unverified"}, "verification_error": error}),
                Err(error) => {
                    let rejected = error.downcast_ref::<st_runtime::StartRejected>()
                        .is_some_and(|proof| proof.operation.as_deref() == Some(id.as_str()));
                    let (closed, verification_error) = if !backend_entered || rejected { (true, None) }
                        else { match runtime.start_operation_closed(&prepared.launch_member) {
                            Ok(closed) => (closed, None),
                            Err(error) => (false, Some(format!("{error:#}"))),
                        }};
                    serde_json::json!({"error": format!("{error:#}"), "producer_closed": closed,
                        "closure_kind": if !backend_entered { "before-backend" } else if rejected { "native-rejection" } else { "positive-producer-proof" },
                        "verification": if closed { "producer-closed" } else { "producer-open-unverified" },
                        "verification_error": verification_error})
                },
            };
            result["operation_id"] = Value::from(id.clone());
            result["subject"] = Value::from(subject.clone());
            result["origin"] = Value::from(store.origin());
            result["owner"] = Value::from(owner);
            result["phase"] = Value::from(if result["error"].is_string() { "failed" } else if result["verification"] == "operation-incarnation" { "completed" } else { "unknown" });
            result["expected_incarnation"] = serde_json::json!(prepared.expected_incarnation);
            result["runtime_kind"] = Value::from(if prepared.member.terminal { "pty" } else { "exec" });
            result["desired_token"] = Value::from(prepared.desired_token);
            result["runtime_id"] = Value::from(prepared.member.runtime_id);
            result["terminal"] = Value::from(prepared.member.terminal);
            result["start_request"] = Value::from(prepared.start_request);
            result["control_request"] = serde_json::json!(prepared.control_request);
            if let Err(error) = store.finish_runtime_start(&id, &result) {
                eprintln!("st3: failed to persist runtime start completion {id}: {error:#}");
            }
            // Drop admission before waking the planner: a completion can admit queued work.
            drop(_lock);
            live.0.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            live.1.notify_all();
            incremental.touch(&format!("local-runtime-start:{subject}"));
            incremental.touch(&subject);
            signal_changed(&notify, &events);
        }));
        if let Err(error) = spawned {
            self.start_jobs
                .live
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&operation.id);
            self.store.finish_runtime_start(&operation.id, &serde_json::json!({"error": format!("spawn launch worker: {error}"), "producer_closed": true}))?;
            self.start_changed(&operation.subject);
        }
        Ok(())
    }

    pub fn revoke_start(&self, subject: &str) -> Result<()> {
        let Some(operation) = self.store.runtime_start_for(subject)? else {
            return Ok(());
        };
        if operation.revoked_by.is_some() {
            return Ok(());
        }
        let request = self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String("terminate".into())),
                ("operation".into(), Value::String(operation.id.clone())),
                (
                    "reason".into(),
                    Value::String("revoke the pending start before runtime control".into()),
                ),
            ]),
            evidence: self
                .store
                .selected_desired_token(subject)?
                .into_iter()
                .collect(),
            expected_subject: None,
            idempotency_key: Some(format!("revoke-start:{}", operation.id)),
        })?;
        self.store
            .revoke_runtime_start(&operation.id, &request.id)?;
        if !operation.dispatched {
            self.store
                .cancel_undispatched_runtime_start(&operation.id)?;
        }
        self.start_changed(subject);
        Ok(())
    }

    pub fn reconcile_start_jobs(&self) -> Result<()> {
        for operation in self.store.pending_runtime_starts()? {
            let subject = operation.subject.clone();
            if reconcile_decision_admitted(&subject) {
                self.isolate("start-completion", &subject, || {
                    self.reconcile_start_operation(operation)
                });
            } else {
                // Gate operations are not admitted reconciliation-fault subjects.
                // Keep a distinct daemon fault scope for each exact operation.
                self.isolate(
                    &format!("start-completion:{subject}"),
                    &format!("daemon/{}", self.host),
                    || self.reconcile_start_operation(operation),
                );
            }
        }
        Ok(())
    }

    fn reconcile_start_operation(&self, mut operation: RuntimeStartOperation) -> Result<()> {
        let prepared: PreparedStart = serde_json::from_value(operation.body.clone())?;
        let current =
            self.store.selected_desired_token(&operation.subject)? == prepared.selected_token;
        if !current || !prepared.requests_retained(&self.store)? {
            self.revoke_start(&operation.subject)?;
            operation.revoked_by = Some("superseded".into());
        }
        let live = self
            .start_jobs
            .live
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&operation.id);
        if !operation.finished
            && (operation.owner != self.start_jobs.owner || (operation.dispatched && !live))
        {
            // The kernel lock, not a PID guess, establishes that no old launch worker holds
            // admission. Late producer ownership still needs exact operation provenance.
            if let Some(_lock) = self.store.try_runtime_start_lock(&operation.subject)? {
                self.revoke_start(&operation.subject)?;
                self.store.finish_runtime_start(
                    &operation.id,
                    &serde_json::json!({"error": "launch owner ended before its result"}),
                )?;
                self.start_changed(&operation.subject);
            }
            return Ok(());
        }
        if operation.finished && !operation.applied {
            let revoked = self.store.runtime_start_revoked(&operation.id)? || !current;
            if !revoked {
                if operation.result.as_ref().is_some_and(|result| {
                    !result["error"].is_string()
                        && result["verification"] != "operation-incarnation"
                }) {
                    let observation = self.runtime.observe_start(&prepared.launch_member)?;
                    if let Some(incarnation) = observation.and_then(|item| item.incarnation_id) {
                        if self
                            .runtime
                            .start_operation(
                                &prepared.member.runtime_id,
                                prepared.member.terminal,
                                &incarnation,
                            )?
                            .as_deref()
                            != Some(operation.id.as_str())
                        {
                            return Ok(());
                        }
                        if !self
                            .store
                            .verify_runtime_start(&operation.id, &incarnation)?
                        {
                            return Ok(());
                        }
                        let result = operation
                            .result
                            .as_mut()
                            .context("start finished without result")?;
                        result["incarnation"] = Value::from(incarnation);
                        result["verification"] = Value::from("operation-incarnation");
                        result["phase"] = Value::from("completed");
                    } else {
                        if !self
                            .runtime
                            .start_operation_closed(&prepared.launch_member)?
                            || !self.store.close_unverified_runtime_start(&operation.id)?
                        {
                            return Ok(());
                        }
                        let result = operation
                            .result
                            .as_mut()
                            .context("start finished without result")?;
                        result["error"] = Value::from("producer closed without a verified launch");
                        result["producer_closed"] = Value::from(true);
                        result["verification"] = Value::from("producer-closed");
                        result["phase"] = Value::from("failed");
                    }
                }
                let result = operation
                    .result
                    .as_ref()
                    .context("start finished without result")?;
                let outcome = match result["error"].as_str() {
                    Some(error) => Err(anyhow::anyhow!(error.to_owned())),
                    None => Ok(result["incarnation"].as_str().map(str::to_owned)),
                };
                let completed = self.finish_start(&operation.id, &prepared, outcome)?;
                if let StartCompletion::Applied(Some(error)) = &completed {
                    if let Some(request) = &prepared.request
                        && self.store.runtime_start_request_retained(request)?
                    {
                        self.record_requested_restart_failure(
                            &prepared.subject,
                            &prepared.member,
                            request,
                            error,
                        )?;
                    } else if let Some(suspension) =
                        crate::suspension::current(&self.store, &operation.subject)?
                        && suspension.phase == "restoring"
                        && prepared.resume_operation.as_deref()
                            == Some(suspension.operation_id.as_str())
                    {
                        self.fail_suspension(
                            &operation.subject,
                            &suspension,
                            "start-failed",
                            error.clone(),
                            Vec::new(),
                        )?;
                    } else if let Some(rollout) = self.store.rollout(&operation.subject)?
                        && prepared
                            .member
                            .environment
                            .get(crate::rollout::OPERATION_ENV)
                            == Some(&rollout.id)
                    {
                        crate::rollout::phase(
                            &self.store,
                            &operation.subject,
                            &rollout,
                            "failed",
                            Some(error),
                            &[],
                        )?;
                    }
                }
                if matches!(&completed, StartCompletion::Applied(None))
                    && let Some(suspension) =
                        crate::suspension::current(&self.store, &operation.subject)?
                    && suspension.phase == "restoring"
                    && prepared.resume_operation.as_deref()
                        == Some(suspension.operation_id.as_str())
                {
                    self.record_suspension_phase(
                        &operation.subject,
                        &suspension,
                        "runtime.action.succeeded",
                        crate::suspension::resume_started_key(&suspension.operation_id),
                        BTreeMap::from([("operation_status".into(), Value::from("verifying"))]),
                    )?;
                }
                match completed {
                    StartCompletion::Applied(error) => {
                        if reconcile_decision_admitted(&operation.subject) {
                            self.record_member_reconcile_result(
                                &operation.subject,
                                error.map_or(Ok(()), |error| Err(anyhow::anyhow!(error))),
                            )?;
                        }
                        // The gate evaluator consumes this exact failed start and
                        // records its gate result for the owning mission/publisher.
                    }
                    StartCompletion::Superseded => {
                        self.revoke_start(&operation.subject)?;
                        if self.store.selected_desired_token(&operation.subject)?
                            != prepared.selected_token
                            && let Some(request) = &prepared.request
                            && self.store.runtime_start_request_retained(request)?
                        {
                            self.record_requested_restart_failure(
                                &prepared.subject,
                                &prepared.member,
                                request,
                                "launch operation was superseded before completion",
                            )?;
                        }
                    }
                }
            } else if !current
                && let Some(request) = &prepared.request
                && self.store.runtime_start_request_retained(request)?
            {
                // Revocation suppresses the stale physical result, but the original explicit
                // restart still needs its administrative result. The request's declaration
                // fence keeps this failure from parking the replacement declaration.
                self.record_requested_restart_failure(
                    &prepared.subject,
                    &prepared.member,
                    request,
                    "launch operation was superseded before completion",
                )?;
            }
            if operation.result.as_ref().is_some_and(|result| {
                result["error"].is_string() && result["producer_closed"] != true
            }) {
                // Ordinary failure is not closure proof. Retain admission and use the same
                // provenance-fenced retirement path if its producer publishes late.
                self.revoke_start(&operation.subject)?;
                operation.revoked_by = Some("failed-unverified".into());
            }
            self.store.runtime_start_applied(&operation.id)?;
            self.start_changed(&operation.subject);
        }
        if operation.revoked_by.is_some() {
            self.cleanup_revoked_start(&operation, &prepared)?;
        } else {
            self.start_queued_job(&operation)?;
        }
        Ok(())
    }

    fn cleanup_revoked_start(
        &self,
        operation: &RuntimeStartOperation,
        prepared: &PreparedStart,
    ) -> Result<()> {
        let member = &prepared.member;
        let observation = if member.terminal {
            self.runtime
                .snapshot_ptys()?
                .into_iter()
                .find(|o| o.runtime_id == member.runtime_id)
        } else {
            self.runtime.observe_exec(&member.runtime_id)?
        };
        let Some(observation) = observation else {
            // An error from an entered backend does not prove its detached producer closed.
            if operation.finished
                && (operation
                    .result
                    .as_ref()
                    .is_some_and(|r| r["producer_closed"] == true)
                    || self
                        .runtime
                        .start_operation_closed(&prepared.launch_member)?)
            {
                self.store
                    .retain_runtime_start_producer_closed(&operation.id)?;
                self.start_changed(&operation.subject);
            } else if operation.finished {
                self.record_once(&operation.subject, "harness.diagnostic", BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("code".into(), Value::String("start-revocation-unverified".into())),
                    ("reason".into(), Value::String("the revoked backend returned without proof that its producer cannot publish later".into())),
                ]))?;
            }
            return Ok(());
        };
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(());
        };
        let provenance =
            self.runtime
                .start_operation(&member.runtime_id, member.terminal, incarnation)?;
        if provenance.as_deref() != Some(operation.id.as_str()) {
            self.record_once(
                &operation.subject,
                "harness.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    (
                        "code".into(),
                        Value::String("start-revocation-unverified".into()),
                    ),
                    (
                        "reason".into(),
                        Value::String(
                            "a revoked launch cannot be matched to an exact runtime operation"
                                .into(),
                        ),
                    ),
                ]),
            )?;
            return Ok(());
        }
        if matches!(
            observation.status.as_str(),
            "exited" | "stopped" | "vanished"
        ) && operation.finished
        {
            if self
                .runtime
                .start_operation_closed(&prepared.launch_member)?
            {
                self.store.retain_runtime_start_ended(
                    &operation.id,
                    incarnation,
                    &observation.status,
                )?;
                self.start_changed(&operation.subject);
                return Ok(());
            }
            // An ended PTY daemon can leave a live tree or work scope. Route it
            // through exact native retirement rather than certifying absence.
            if !member.terminal {
                return Ok(());
            }
        }
        self.reconcile_runtime_stop(
            &operation.subject,
            &member.runtime_id,
            member.terminal,
            Some(incarnation),
            member.shutdown_timeout_ms,
            Some(&observation),
        )?;
        Ok(())
    }
}
