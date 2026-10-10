//! A seat that holds claimed work, has finished its turn, and has nothing set to wake it is
//! idle-holding. After `idle_nudge_after` (30 minutes by default) in that state, st sends it one
//! message naming each such step, its goal, its last progress, how long the seat has been idle and
//! what it waits on, and asks it to continue, set a watch, or release the step. The nudge is
//! recorded on each step it names.
//!
//! Something will wake a seat when its harness is not quiet (a turn, an ask, unsent input,
//! background jobs or subagents), a message to it is not delivered yet, or one of its waits holds
//! (see `waits`): it woke the seat within `idle_nudge_quiet_watch` (2 hours by default), or it
//! names a time it is expected by that has not passed.
//!
//! A wait can also carry an action: a movement its seat was not woken for that needs it to act,
//! such as a watched pull request leaving the merge queue without merging. An idle seat is
//! nudged for an action as soon as the action settles, without waiting out the idle threshold.
//!
//! Back-off: one nudge per step per idle period, the next only after the seat has taken a turn
//! and gone idle again, and, except for an action, never more than one every two hours per step.
//! Each action nudges a step at most once.
//!
//! The check runs inside the seat's wake evaluation, which already reads its work and harness.
//! Each further read happens only once the cheaper conditions hold, the quiet check stops at the
//! first wait that holds, and a seat that is not due yet records when it will be, so nothing
//! polls.
use super::waits::{Wait, WaitAction, describe_age, step_waits};
use super::*;
use crate::store::WORK_NUDGED_KIND;
use serde_json::json;

/// The default idle time before a nudge.
pub const IDLE_NUDGE_AFTER_MS: u64 = 30 * 60_000;
/// The default time a wait holds after it last woke its seat.
pub const IDLE_NUDGE_QUIET_WATCH_MS: u64 = 2 * 60 * 60_000;
/// The least time between two quiet nudges about one step.
pub const IDLE_NUDGE_MIN_INTERVAL_MS: u128 = 2 * 60 * 60_000;

/// The daemon's idle-nudge settings. `after_ms` is `None` when nudges are off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleNudgeSettings {
    pub after_ms: Option<u64>,
    pub quiet_watch_ms: u64,
}

impl Default for IdleNudgeSettings {
    fn default() -> Self {
        Self {
            after_ms: Some(IDLE_NUDGE_AFTER_MS),
            quiet_watch_ms: IDLE_NUDGE_QUIET_WATCH_MS,
        }
    }
}

/// The steps a seat holds: claimed by it and not yet submitted.
pub(super) fn held_steps<'a>(agent: &str, work: &'a [StepRunView]) -> Vec<&'a StepRunView> {
    work.iter()
        .filter(|step| {
            step.claimant.as_deref() == Some(agent)
                && matches!(step.status.as_str(), "claimed" | "working")
        })
        .collect()
}

/// When the harness last finished a turn, if it is idle now.
pub(super) fn idle_since(harness: &CurrentHarnessView) -> Option<u128> {
    (harness.is_ready() && matches!(harness.state.as_str(), "idle" | "ready"))
        .then_some(harness.since_unix_ms)
}

/// Whether the driver reports a clean boundary: no turn, ask, unsent input or background job.
pub(super) fn harness_quiet(report: Option<&Value>) -> bool {
    report.is_some_and(|fields| {
        fields.get("quiescent").and_then(Value::as_bool) == Some(true)
            && fields
                .get("blocking")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
    })
}

/// One held step's back-off, from its last nudge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StepBackoff {
    /// When the step may get a quiet nudge; `None` when it was nudged in this idle period.
    pub quiet_due: Option<u128>,
    /// The step's last nudge, which an action must come after.
    pub last_nudge: Option<u128>,
}

/// A step held through an idle period that began at `idle_since` may get a quiet nudge after the
/// idle threshold and two hours after its last nudge; after a nudge in this idle period, only a
/// new turn makes it eligible again.
pub(super) fn step_backoff(idle_since: u128, after_ms: u64, last_nudge: Option<u128>) -> StepBackoff {
    let nudged_this_period = last_nudge.is_some_and(|last| idle_since <= last);
    let idle_due = idle_since.saturating_add(u128::from(after_ms));
    StepBackoff {
        quiet_due: (!nudged_this_period).then(|| {
            last_nudge.map_or(idle_due, |last| {
                idle_due.max(last.saturating_add(IDLE_NUDGE_MIN_INTERVAL_MS))
            })
        }),
        last_nudge,
    }
}

/// Whether an action may nudge a step: the step was not nudged in this idle period or since the
/// action, and the action has settled.
pub(super) fn action_nudges(backoff: &StepBackoff, action: &WaitAction, now: u128) -> bool {
    backoff.quiet_due.is_some()
        && backoff.last_nudge.is_none_or(|last| action.at_unix_ms > last)
        && action.due_unix_ms <= now
}

/// Why a nudge is sent.
enum NudgeReason<'a> {
    /// Nothing will wake the seat; these are its quiet waits.
    Quiet(Vec<Wait>),
    /// A wait needs the seat to act.
    Action(&'a Wait, &'a WaitAction),
}

impl<R: RuntimeControl> Reconciler<R> {
    pub fn set_idle_nudge(&self, settings: IdleNudgeSettings) {
        *self
            .idle_nudge
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    fn idle_nudge_settings(&self) -> IdleNudgeSettings {
        *self
            .idle_nudge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Nudge `agent` if it is idle-holding and due, or note when it will be.
    pub(super) fn nudge_idle_holder(
        &self,
        agent: &str,
        work: &[StepRunView],
        harness: Option<&CurrentHarnessView>,
        now: u128,
    ) -> Result<()> {
        let settings = self.idle_nudge_settings();
        let Some(after_ms) = settings.after_ms else {
            return Ok(());
        };
        let held = held_steps(agent, work);
        if held.is_empty() {
            return Ok(());
        }
        let Some(since) = harness.and_then(idle_since) else {
            return Ok(());
        };
        let mut steps = Vec::new();
        for step in held {
            let last = self
                .store
                .latest_nudge(&step.subject)?
                .filter(|claim| {
                    claim.body.pointer("/fields/attempt").and_then(Value::as_u64)
                        == Some(u64::from(step.attempt))
                })
                .map(|claim| claim.accepted_at_unix_ms);
            let backoff = step_backoff(since, after_ms, last);
            if backoff.quiet_due.is_some() {
                steps.push((step, backoff));
            }
        }
        if steps.is_empty() {
            return Ok(());
        }
        if !harness_quiet(self.store.latest_harness_report(agent)?.as_ref())
            || !self.store.open_subagents(agent)?.is_empty()
            || self.store.has_undelivered_message(agent)?
        {
            return Ok(());
        }

        // Actions first: they do not wait out the idle threshold, and any watch can raise one.
        let github = self.github_waits(agent)?;
        for wait in &github {
            let Some(action) = &wait.action else {
                continue;
            };
            let due = steps
                .iter()
                .filter(|(_, backoff)| action_nudges(backoff, action, now))
                .map(|(step, _)| *step)
                .collect::<Vec<_>>();
            if !due.is_empty() {
                return self.send_idle_nudge(agent, since, &due, NudgeReason::Action(wait, action), now);
            }
            if action.due_unix_ms > now {
                smallclaims::touched::note_due(action.due_unix_ms);
            }
        }

        let due = steps
            .iter()
            .filter(|(_, backoff)| backoff.quiet_due.is_some_and(|due| due <= now))
            .map(|(step, _)| *step)
            .collect::<Vec<_>>();
        if due.is_empty() {
            if let Some(next) = steps.iter().filter_map(|(_, backoff)| backoff.quiet_due).min() {
                smallclaims::touched::note_due(next);
            }
            return Ok(());
        }
        // The quiet check: cheapest source first, and the first wait that holds ends it.
        let mut quiet = Vec::new();
        let mut consider = |waits: Vec<Wait>| -> bool {
            for wait in waits {
                if let Some(until) = wait.holds_until(settings.quiet_watch_ms, now) {
                    smallclaims::touched::note_due(until);
                    return true;
                }
                quiet.push(wait);
            }
            false
        };
        if consider(step_waits(agent, work, now))
            || consider(self.depends_on_waits(agent, work)?)
            || consider(github)
            || consider(self.report_to_waits(agent)?)
        {
            return Ok(());
        }
        self.send_idle_nudge(agent, since, &due, NudgeReason::Quiet(quiet), now)
    }

    fn send_idle_nudge(
        &self,
        agent: &str,
        idle_since: u128,
        steps: &[&StepRunView],
        reason: NudgeReason<'_>,
        now: u128,
    ) -> Result<()> {
        let event = match &reason {
            NudgeReason::Quiet(_) => "quiet".to_owned(),
            NudgeReason::Action(_, action) => action.key.clone(),
        };
        let key = format!("st3-idle-nudge:{agent}:{idle_since}:{event}");
        let message_id = &hex::encode(sha2::Sha256::digest(key.as_bytes()))[..16];
        let message = format!("message/{message_id}");
        let idle_for = describe_age(now.saturating_sub(idle_since));
        let held = if steps.len() == 1 {
            "this step".to_owned()
        } else {
            format!("{} steps", steps.len())
        };
        let mut content = match &reason {
            NudgeReason::Quiet(_) => format!(
                "You hold {held} and have been idle for {idle_for} with nothing set to wake you.\n"
            ),
            NudgeReason::Action(_, action) => format!(
                "{} {} ago, and nothing has woken you for it. You hold {held} and have been idle for {idle_for}.\n",
                action.what,
                describe_age(now.saturating_sub(action.at_unix_ms)),
            ),
        };
        for step in steps {
            content.push_str(&format!(
                "\nStep: {}\nTitle: {}\n",
                step.subject,
                step.title.as_deref().unwrap_or(&step.step)
            ));
            for goal in &step.goals {
                content.push_str(&format!("Goal: {goal}\n"));
            }
            match (&step.progress_summary, step.progress_at_unix_ms) {
                (Some(summary), Some(at)) => content.push_str(&format!(
                    "Last progress ({} ago): {summary}\n",
                    describe_age(now.saturating_sub(at))
                )),
                (Some(summary), None) => {
                    content.push_str(&format!("Last progress: {summary}\n"));
                }
                (None, _) => content.push_str("Last progress: none recorded\n"),
            }
        }
        let waits = match &reason {
            NudgeReason::Quiet(waits) => waits.iter().collect::<Vec<_>>(),
            NudgeReason::Action(wait, _) => vec![*wait],
        };
        if !waits.is_empty() {
            content.push_str("\nWhat you wait on has not woken you:\n");
            for wait in &waits {
                content.push_str(&format!("- {}\n", wait.describe(now)));
            }
        }
        if matches!(reason, NudgeReason::Action(..)) {
            content.push_str(
                "Check it: whether it is still queued, was ejected, or is stuck. Then act, keep waiting, or release the step.\n",
            );
        }
        let release = steps
            .iter()
            .map(|step| format!("`st work release {} --as {agent}`", step.subject))
            .collect::<Vec<_>>()
            .join(", ");
        content.push_str(&format!(
            "\nContinue the work, set `st gh watch` on what you are waiting for, or release the step ({release}).\n\
             st sends this once per idle period, and at most once every 2 hours per step unless something needs you to act."
        ));
        let title = match steps {
            [step] => format!(
                "Idle holding: {}",
                step.title.as_deref().unwrap_or(&step.step)
            ),
            _ => format!("Idle holding {} steps", steps.len()),
        };
        let mut tags = vec![Value::String("st3-idle-nudge".into())];
        tags.extend(
            steps
                .iter()
                .map(|step| Value::String(format!("mission-run:{}", step.run))),
        );
        self.store.append_claim(&ClaimInput {
            subject: message.clone(),
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("daemon/runtime".into())),
                ("to".into(), Value::String(agent.into())),
                ("content".into(), Value::String(content)),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String(title)),
                ("in_reply_to".into(), Value::Null),
                ("tags".into(), Value::Array(tags)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.clone()),
        })?;
        let recorded = waits
            .iter()
            .map(|wait| {
                json!({
                    "source": wait.source.name(),
                    "subject": wait.subject,
                    "last_moved_unix_ms": wait.last_moved_unix_ms.map(|at| at.to_string()),
                    "expected_by_unix_ms": wait.expected_by_unix_ms.map(|at| at.to_string()),
                })
            })
            .collect::<Vec<_>>();
        for step in steps {
            self.store.append_claim(&ClaimInput {
                subject: step.subject.clone(),
                kind: WORK_NUDGED_KIND.into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("attempt".into(), Value::from(step.attempt)),
                    ("agent".into(), Value::String(agent.into())),
                    ("message".into(), Value::String(message.clone())),
                    (
                        "idle_since_unix_ms".into(),
                        Value::String(idle_since.to_string()),
                    ),
                    ("reason".into(), Value::String(event.clone())),
                    ("waits".into(), Value::Array(recorded.clone())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{key}:{}", step.subject)),
            })?;
        }
        self.signal_changed();
        Ok(())
    }
}
