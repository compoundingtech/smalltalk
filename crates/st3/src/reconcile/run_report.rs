//! A run that names a reporter tells that agent when it fails, is cancelled or stalls, and when
//! it completes if asked. The reconciler decides this while it evaluates the run, so a run that
//! does not change costs nothing, and a stall is a deadline on the run's item rather than a scan.
//!
//! One message per distinct event: its identity is the run, the event and, for a stall, the
//! last progress the run made, so an evaluation that repeats, a restart and a replayed pass all
//! find the message already sent. The message names the run and its steps and links the commands
//! that show them. It never copies fault or cancellation text, which can carry anything a step
//! printed. A reporter that is not a live agent gets a fault on the run instead, once per
//! reason, and the next evaluation looks again without sending anything twice.
use super::*;
use serde_json::json;

/// The scope of the fault a run records when its reporter cannot be told.
pub(super) const REPORT_FAULT_SCOPE: &str = "report-to";

/// Who a run reports to and which events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RunReport {
    pub to: String,
    pub stalled_after_ms: u64,
    pub completed: bool,
}

/// What happened to a run that its reporter is told.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunEvent {
    Failed,
    Cancelled,
    Completed,
    Stalled { since: u128 },
}

impl RunEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Stalled { .. } => "stalled",
        }
    }
}

/// The last sign of life in a run: its own last change, any step's last change or progress, and
/// the end of any step's lease.
pub(crate) fn run_activity(run: &MissionRunView) -> u128 {
    run.steps
        .iter()
        .flat_map(|step| {
            [
                Some(step.updated_at_unix_ms),
                step.progress_at_unix_ms,
                step.claim_expires_at_unix_ms,
            ]
        })
        .flatten()
        .chain([run.created_at_unix_ms, run.updated_at_unix_ms])
        .max()
        .unwrap_or_default()
}

/// Whether a run is still working, so that silence means something. A run in cleanup is ending,
/// and a run that waits for the run before it has not started.
fn can_stall(run: &MissionRunView) -> bool {
    matches!(run.phase.as_str(), "normal" | "revision-draining" | "final")
        && !run.steps.iter().any(|step| {
            step.step == crate::mission::AFTER_RUN_STEP && step.status != "completed"
        })
}

impl<R: RuntimeControl> Reconciler<R> {
    /// The report a run asked for when it was created, if any.
    pub(super) fn run_report(&self, run: &MissionRunView) -> Result<Option<RunReport>> {
        let Some(created) = self
            .store
            .latest_claim(&run.subject, Some("mission-run.created"))?
        else {
            return Ok(None);
        };
        let fields = created.body.get("fields").unwrap_or(&created.body);
        let Some(to) = fields.get("report_to").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(RunReport {
            to: to.to_owned(),
            stalled_after_ms: fields
                .get("stalled_after_ms")
                .and_then(Value::as_u64)
                .unwrap_or(crate::mission::DEFAULT_STALLED_AFTER_MS),
            completed: fields
                .get("report_completed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }))
    }

    /// Tell the reporter what this run's phase calls for, and return when a run that has not
    /// stalled yet will have: the time to evaluate it again.
    pub(super) fn report_run(
        &self,
        run: &MissionRunView,
        report: &RunReport,
        now: u128,
    ) -> Result<Option<u128>> {
        let event = match run.phase.as_str() {
            "cleanup-failed" => Some(RunEvent::Failed),
            "cleanup-cancelled" | "final-cancelled" => Some(RunEvent::Cancelled),
            "cleanup-completed" if report.completed => Some(RunEvent::Completed),
            _ => None,
        };
        if let Some(event) = event {
            self.tell_reporter(run, report, event)?;
            return Ok(None);
        }
        if !can_stall(run) {
            return Ok(None);
        }
        let since = run_activity(run);
        let stalls_at = since.saturating_add(u128::from(report.stalled_after_ms));
        if now < stalls_at {
            return Ok(Some(stalls_at));
        }
        self.tell_reporter(run, report, RunEvent::Stalled { since })?;
        Ok(None)
    }

    fn tell_reporter(
        &self,
        run: &MissionRunView,
        report: &RunReport,
        event: RunEvent,
    ) -> Result<()> {
        let key = match event {
            RunEvent::Stalled { since } => {
                format!("st3-run-report:{}:stalled:{since}", run.subject)
            }
            _ => format!("st3-run-report:{}:{}", run.subject, event.name()),
        };
        let message_id = &hex::encode(sha2::Sha256::digest(key.as_bytes()))[..16];
        let subject = format!("message/{message_id}");
        if self
            .store
            .latest_claim(&subject, Some("message.sent"))?
            .is_some()
        {
            return self.close_fault(
                &run.subject,
                REPORT_FAULT_SCOPE,
                "the reporter was told",
            );
        }
        if !self.store.agent_is_live(&report.to)? {
            return self.record_fault(
                &run.subject,
                REPORT_FAULT_SCOPE,
                Err(anyhow::anyhow!(
                    "{} is not a running agent, so it was not told that the run {}",
                    report.to,
                    event.name()
                )),
            );
        }
        let steps = |keep: &dyn Fn(&StepRunView) -> bool| {
            let listed = run
                .steps
                .iter()
                .filter(|step| keep(step))
                .map(|step| format!("- {} ({})", step.step, step.status))
                .collect::<Vec<_>>();
            if listed.is_empty() {
                String::new()
            } else {
                format!("\n\nSteps:\n{}", listed.join("\n"))
            }
        };
        let (title, detail) = match event {
            RunEvent::Failed => (
                "failed",
                format!(
                    "The run failed.{}",
                    steps(&|step| matches!(step.status.as_str(), "failed" | "orphaned"))
                ),
            ),
            RunEvent::Cancelled => ("was cancelled", "The run was cancelled.".to_owned()),
            RunEvent::Completed => ("completed", "The run completed.".to_owned()),
            RunEvent::Stalled { since } => (
                "has stalled",
                format!(
                    "The run has made no progress for {} (its limit is {}). It is not failed and may still go on.{}",
                    describe_duration(now_ms().saturating_sub(since)),
                    describe_duration(u128::from(report.stalled_after_ms)),
                    steps(&|step| {
                        !matches!(step.status.as_str(), "completed" | "cancelled")
                    })
                ),
            ),
        };
        let content = format!(
            "{detail}\n\nRun: {run_subject}\nMission: {mission}\n\nInspect: `st missions show {run_subject}`, and `st work show STEP_SUBJECT` for a step. The reason is on the run, not in this message.\n\nYou are told because the run names you in `report-to`. st sends one message for each event of a run.",
            run_subject = run.subject,
            mission = run.mission,
        );
        self.store.append_claim(&ClaimInput {
            subject,
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("daemon/runtime".into())),
                ("to".into(), Value::String(report.to.clone())),
                ("content".into(), Value::String(content)),
                ("status".into(), Value::String("sent".into())),
                (
                    "title".into(),
                    Value::String(format!("Run {title}: {}", run.subject)),
                ),
                ("in_reply_to".into(), Value::Null),
                (
                    "tags".into(),
                    json!([
                        format!("st3-run-report:{}", event.name()),
                        format!("mission-run:{}", run.subject),
                    ]),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key),
        })?;
        self.close_fault(&run.subject, REPORT_FAULT_SCOPE, "the reporter was told")
    }
}

fn describe_duration(ms: u128) -> String {
    let minutes = ms / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, 0) => format!("{}s", ms / 1_000),
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes}m"),
    }
}
