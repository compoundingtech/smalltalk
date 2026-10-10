//! What a seat waits on: anything it has subscribed to that will wake it. Each wait is a subject,
//! when its seat was last woken by it (or when the wait began), and, when the source knows one,
//! the time it is expected by. A wait holds (its seat is not stuck on it) until its expected-by
//! time when it has one, and otherwise for the quiet window after it last woke the seat.
//!
//! Only an event delivered to the seat restarts a wait's quiet clock: a delivered event wakes the
//! seat, which ends an idle period anyway. A movement the source observed but did not deliver,
//! such as a reordered merge queue or another seat's progress, never extends a hold. When a
//! source can tell that such a movement needs the seat to act, the wait carries it as an action,
//! and the seat is nudged then.
//!
//! The sources are read from facts st already records: a GitHub watch, a person ask, a gate, a
//! scheduled retry, a step another seat holds that one of this seat's steps depends on, and a run
//! that reports to this seat. A new source, such as a watch on any subject, adds one more reader.
//! Expected-by times are only what a source states today (a watch's deadline, a retry's time);
//! forecasts from a stage's history can fill it in later.
use super::*;

/// How long a pull request must stay out of the merge queue before leaving it counts. GitHub
/// reports a queued pull request out of the queue for a minute or two while the queue rebuilds.
pub(crate) const MERGE_QUEUE_EXIT_SETTLE_MS: u128 = 5 * 60_000;

/// Where a wait comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitSource {
    GithubWatch,
    PersonAsk,
    Gate,
    Retry,
    DependsOn,
    ReportTo,
}

impl WaitSource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::GithubWatch => "gh watch",
            Self::PersonAsk => "person ask",
            Self::Gate => "gate",
            Self::Retry => "scheduled retry",
            Self::DependsOn => "depends-on",
            Self::ReportTo => "report-to",
        }
    }
}

/// One thing a seat waits on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Wait {
    pub source: WaitSource,
    /// What is waited on: a thread, a step or a run.
    pub subject: String,
    /// When something was last observed on it.
    pub last_moved_unix_ms: Option<u128>,
    /// What that was, when the source can say.
    pub last_move: Option<String>,
    /// When the source says it will wake the seat. It wins over the quiet window.
    pub expected_by_unix_ms: Option<u128>,
    /// A movement the seat was not woken for that needs it to act.
    pub action: Option<WaitAction>,
}

/// A movement on a wait that its seat was not woken for and needs to act on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WaitAction {
    /// When the source saw it.
    pub at_unix_ms: u128,
    /// When it counts: after it has settled.
    pub due_unix_ms: u128,
    pub what: String,
    /// What tells this movement from every other one, so it nudges at most once.
    pub key: String,
}

impl Wait {
    /// Until when this wait counts as something that will wake its seat; `None` once it does not.
    pub(crate) fn holds_until(&self, quiet_ms: u64, now: u128) -> Option<u128> {
        self.expected_by_unix_ms
            .or_else(|| {
                self.last_moved_unix_ms
                    .map(|moved| moved.saturating_add(u128::from(quiet_ms)))
            })
            .filter(|until| *until > now)
    }

    /// The wait as a nudge names it.
    pub(crate) fn describe(&self, now: u128) -> String {
        let moved = match (self.last_moved_unix_ms, &self.last_move) {
            (Some(at), Some(what)) => format!(
                "last moved {} ago ({what})",
                describe_age(now.saturating_sub(at))
            ),
            (Some(at), None) => format!("last moved {} ago", describe_age(now.saturating_sub(at))),
            (None, _) => "nothing has moved on it".to_owned(),
        };
        let expected = self.expected_by_unix_ms.map_or(String::new(), |at| {
            if at > now {
                format!(", expected in {}", describe_age(at - now))
            } else {
                format!(", expected {} ago", describe_age(now - at))
            }
        });
        format!("{} {}: {moved}{expected}", self.source.name(), self.subject)
    }
}

/// The waits that come from the seat's own steps, which the wake evaluation has already read:
/// asks, gates and scheduled retries. Steps that depend on others are read separately.
pub(crate) fn step_waits(agent: &str, work: &[StepRunView], now: u128) -> Vec<Wait> {
    work.iter()
        .filter(|step| {
            step.claimant.as_deref() == Some(agent) || step.assigned_to.as_deref() == Some(agent)
        })
        .filter_map(|step| {
            let (source, subject, expected_by) = match step.status.as_str() {
                "waiting-person" => (
                    WaitSource::PersonAsk,
                    step.blocked_reason
                        .clone()
                        .filter(|reason| reason.starts_with("step-run/"))
                        .unwrap_or_else(|| step.subject.clone()),
                    None,
                ),
                "verifying" => (WaitSource::Gate, step.subject.clone(), None),
                "completed" | "failed" | "cancelled" => return None,
                _ => {
                    let not_before = step.not_before_unix_ms.filter(|at| *at > now)?;
                    (WaitSource::Retry, step.subject.clone(), Some(not_before))
                }
            };
            Some(Wait {
                source,
                subject,
                last_moved_unix_ms: Some(step.updated_at_unix_ms),
                last_move: Some("the wait began".into()),
                expected_by_unix_ms: expected_by,
                action: None,
            })
        })
        .collect()
}

impl<R: RuntimeControl> Reconciler<R> {
    /// The seat's GitHub watches. A watch last moved when it last woke the seat, or when it
    /// began. A pull request that left the merge queue without merging, and stayed out, since then
    /// is an action.
    pub(crate) fn github_waits(&self, agent: &str) -> Result<Vec<Wait>> {
        let mut waits = Vec::new();
        for watch in self.store.live_watches_of(agent)? {
            let woke = self
                .store
                .last_message_to(agent, &[&watch.subject])?
                .filter(|woke| *woke > watch.since_unix_ms);
            let last = woke.unwrap_or(watch.since_unix_ms);
            let thread = watch.thread.to_string();
            let action = self
                .store
                .thread_left_merge_queue(&watch.thread, last)?
                .map(|at| WaitAction {
                    at_unix_ms: at,
                    due_unix_ms: at.saturating_add(MERGE_QUEUE_EXIT_SETTLE_MS),
                    what: format!("{thread} left the merge queue without merging"),
                    key: format!("left-merge-queue:{thread}:{at}"),
                });
            waits.push(Wait {
                source: WaitSource::GithubWatch,
                subject: thread,
                last_moved_unix_ms: Some(last),
                last_move: Some(if woke.is_some() { "it woke you" } else { "the watch began" }.into()),
                expected_by_unix_ms: watch.until_unix_ms,
                action,
            });
        }
        Ok(waits)
    }

    /// The open runs that report to the seat. A run last moved when it last reported to the seat,
    /// or when it began.
    pub(crate) fn report_to_waits(&self, agent: &str) -> Result<Vec<Wait>> {
        let mut waits = Vec::new();
        for run in self.store.open_mission_runs()? {
            if self
                .run_report(&run)?
                .is_none_or(|report| report.to != agent)
            {
                continue;
            }
            let Some(view) = self.store.mission_run(&run)? else {
                continue;
            };
            let reported = self
                .store
                .last_message_to(agent, &["st3-run-report:", &format!("mission-run:{run}\"")])?;
            let last = reported.map_or(view.created_at_unix_ms, |at| at.max(view.created_at_unix_ms));
            waits.push(Wait {
                source: WaitSource::ReportTo,
                subject: run,
                last_moved_unix_ms: Some(last),
                last_move: Some(if reported.is_some() { "it reported to you" } else { "the run began" }.into()),
                expected_by_unix_ms: None,
                action: None,
            });
        }
        Ok(waits)
    }

    /// Steps another seat holds that one of this seat's pending steps depends on, and gates such a
    /// step depends on.
    pub(crate) fn depends_on_waits(&self, agent: &str, work: &[StepRunView]) -> Result<Vec<Wait>> {
        let pending = work
            .iter()
            .filter(|step| step.assigned_to.as_deref() == Some(agent) && step.status == "pending")
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let runs = pending
            .iter()
            .map(|step| step.run.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let specs = self.store.mission_specs_for_runs(&runs)?;
        let mut waits = Vec::new();
        for step in pending {
            let Some(spec) = specs.get(&step.run).or_else(|| {
                specs.get(step.run.strip_prefix("mission-run/").unwrap_or(&step.run))
            }) else {
                continue;
            };
            let flat = flatten_mission_steps(spec);
            let Some(runtime) = flat.iter().find(|runtime| runtime.spec.path == step.step) else {
                continue;
            };
            let generation = step
                .generation
                .strip_prefix("run-generation/")
                .unwrap_or(&step.generation);
            for dependency in &runtime.spec.dependencies {
                match dependency {
                    DependencySpec::Step { step: target, .. } => {
                        let path = if runtime.dependency_prefix.is_empty() {
                            target.clone()
                        } else {
                            format!("{}/{target}", runtime.dependency_prefix)
                        };
                        let subject = format!("step-run/{generation}/{path}");
                        let Some(target) = self.store.step_run(&subject)? else {
                            continue;
                        };
                        // A step this seat holds or will run itself wakes nobody.
                        if matches!(target.status.as_str(), "completed" | "failed" | "cancelled")
                            || target.claimant.as_deref() == Some(agent)
                            || target.assigned_to.as_deref() == Some(agent)
                        {
                            continue;
                        }
                        // Another seat's progress is not delivered here, so the wait runs from
                        // when this step began waiting.
                        waits.push(Wait {
                            source: WaitSource::DependsOn,
                            subject,
                            last_moved_unix_ms: Some(step.created_at_unix_ms),
                            last_move: Some("the wait began".into()),
                            expected_by_unix_ms: None,
                            action: None,
                        });
                    }
                    DependencySpec::Predicate { .. } => waits.push(Wait {
                        source: WaitSource::Gate,
                        subject: step.subject.clone(),
                        last_moved_unix_ms: Some(step.updated_at_unix_ms),
                        last_move: Some("the wait began".into()),
                        expected_by_unix_ms: None,
                        action: None,
                    }),
                }
            }
        }
        Ok(waits)
    }
}

pub(crate) fn describe_age(ms: u128) -> String {
    let minutes = ms / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, 0) => format!("{}s", ms / 1_000),
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes}m"),
    }
}
