//! The current work list as a published view: every open step of a current generation, with its
//! effective state and its row. The refresher in `api::published_lists` folds it; this module
//! holds the rows and the reads that say which steps changed between two cuts.

use super::*;

/// One step of the current work list.
pub(crate) struct WorkRow {
    /// The step with its effective state at the fold, which orders it and decides which actors
    /// see it.
    pub(crate) view: Arc<StepRunView>,
    pub(crate) value: Arc<Value>,
    /// The list time the row was folded or re-timed at.
    pub(crate) time_unix_ms: u128,
    /// The step's worker lease when folded. A quiet renewal moves it without a claim.
    pub(crate) lease_unix_ms: Option<u128>,
    /// For a running step, the lease its claims last named, which bounds its execution time.
    pub(crate) timing_lease_unix_ms: Option<u128>,
}

/// One publication of the current work list.
#[derive(Clone, Default)]
pub(crate) struct WorkRows {
    pub(crate) rows: HashMap<String, Arc<WorkRow>>,
    /// The steps in the unfiltered list's order.
    pub(crate) order: Vec<String>,
    /// The list's time: the projection time of its cut, never earlier than the previous
    /// publication's. A worker lease that ends after it changes a row.
    pub(crate) time_unix_ms: u128,
    /// The step that owned each seat a row shows, when that row was folded.
    pub(crate) seats: HashMap<String, String>,
}

/// How the work list ranks a step's state: ready work first, ended work last.
pub(crate) fn work_state_rank(status: &str) -> u8 {
    match status {
        "ready" => 0,
        "claimed" | "working" => 1,
        "verifying" => 2,
        "blocked" => 3,
        "pending" | "waiting" => 4,
        _ => 5,
    }
}

impl WorkRows {
    /// Put the steps in the unfiltered list's order: by state, then readiness, step path and
    /// subject, as the direct read sorts them.
    pub(crate) fn sort(&mut self) {
        let mut order = self.rows.values().map(|row| &row.view).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            work_state_rank(&left.status)
                .cmp(&work_state_rank(&right.status))
                .then_with(|| left.readiness_epoch.cmp(&right.readiness_epoch))
                .then_with(|| left.step.cmp(&right.step))
                .then_with(|| left.subject.cmp(&right.subject))
        });
        self.order = order.into_iter().map(|view| view.subject.clone()).collect();
    }
}

impl Store {
    /// Which steps' work rows the claims after `after` through `through` can change, read in
    /// the caller's snapshot. A row reads its step, the run, generation and root run that own
    /// it, the person asks that block it, and the seats it owns with their usage. A claim about
    /// any other subject changes no row.
    /// `seats` names the step each seat shown on a row belonged to when that row was folded.
    pub(crate) fn work_list_changes(
        &self,
        after: u64,
        through: u64,
        seats: &HashMap<String, String>,
    ) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut steps = BTreeSet::new();
        let mut runs = BTreeSet::new();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, CASE WHEN kind LIKE 'work.person-%'
                    THEN json_extract(body,'$.fields.origin_step') END
             FROM claims WHERE store_index>?1 AND store_index<=?2",
        )?;
        let claims = statement
            .query_map(params![after, through], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (subject, kind, origin) in claims {
            if let Some(run) = subject.strip_prefix("mission-run/") {
                runs.insert(run.to_owned());
                // A root run's state ends the steps of every run under it.
                if matches!(kind.as_str(), "mission-run.state" | "mission-run.created") {
                    let mut children = connection.prepare_cached(
                        "SELECT id FROM mission_runs WHERE root_run_id=?1 AND id<>?1",
                    )?;
                    for child in children.query_map([run], |row| row.get::<_, String>(0))? {
                        runs.insert(child?);
                    }
                }
            } else if let Some(generation) = subject.strip_prefix("run-generation/") {
                let mut owner = connection
                    .prepare_cached("SELECT run_id FROM run_generations WHERE id=?1")?;
                if let Some(run) = owner.query_row([generation], |row| row.get::<_, String>(0)).optional()? {
                    runs.insert(run);
                }
            } else if subject.starts_with("step-run/") {
                steps.insert(subject);
                // A person's answer to an ask unblocks the step that asked.
                steps.extend(origin);
            } else if subject.starts_with("agent/") {
                // A seat a step owns shows on that step's row with its usage.
                let mut owned = connection.prepare_cached(
                    "SELECT owner_step FROM desired WHERE subject=?1 AND owner_step IS NOT NULL",
                )?;
                for step in owned.query_map([&subject], |row| row.get::<_, String>(0))? {
                    steps.insert(step?);
                }
                if kind == "intent.desired" {
                    // A requester's declaration decides whether its asks still block steps.
                    let mut asks = connection.prepare_cached(
                        "SELECT subject, json_extract(body,'$.fields.origin_step') FROM claims
                         WHERE kind='work.person-asked' AND actor=?1",
                    )?;
                    for ask in asks.query_map([&subject], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })? {
                        let (ask, origin) = ask?;
                        steps.insert(ask);
                        steps.extend(origin);
                    }
                }
                // A seat that left its step: that step's row no longer shows it.
                steps.extend(seats.get(&subject).cloned());
            }
        }
        if !runs.is_empty() {
            let mut owned = connection.prepare_cached(
                "SELECT subject FROM step_runs INDEXED BY step_runs_run_index WHERE run_id=?1",
            )?;
            for run in &runs {
                for step in owned.query_map([run], |row| row.get::<_, String>(0))? {
                    steps.insert(step?);
                }
            }
        }
        Ok(steps)
    }

    /// The steps whose worker lease ended after `after` and by `through`: their rows show them
    /// ready again with no new claim.
    pub(crate) fn steps_with_leases_ended(&self, after: u128, through: u128) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject FROM step_runs INDEXED BY step_runs_lease_index
             WHERE lease_owner IS NOT NULL
               AND CAST(lease_expires_at_unix_ms AS INTEGER)>?1
               AND CAST(lease_expires_at_unix_ms AS INTEGER)<=?2",
        )?;
        let steps = statement
            .query_map(
                params![after.min(i64::MAX as u128) as i64, through.min(i64::MAX as u128) as i64],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        Ok(steps)
    }

    /// The steps the current work list can show: those whose state is not waiting or ended.
    /// Their effective state can hide more of them, never show others.
    pub(crate) fn work_list_candidates(&self) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject FROM step_runs INDEXED BY step_runs_open_index
             WHERE status NOT IN ('completed','failed','cancelled') AND status<>'pending'",
        )?;
        let steps = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        Ok(steps)
    }

    /// Every step that holds a worker lease, with the lease's end: few, by the lease index.
    pub(crate) fn work_leases(&self) -> Result<HashMap<String, u128>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, CAST(lease_expires_at_unix_ms AS INTEGER) FROM step_runs
             INDEXED BY step_runs_lease_index
             WHERE lease_owner IS NOT NULL AND lease_expires_at_unix_ms IS NOT NULL",
        )?;
        let leases = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as u128)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(leases)
    }

    /// The lease `subject`'s claims last named for the open execution interval of `attempt` at
    /// `at`, when one is open.
    pub(crate) fn step_timing_lease(&self, subject: &str, attempt: u32, at: u128) -> Result<Option<u128>> {
        Ok(step_timing_lease_at(&self.readers.get(), subject, attempt, at)?)
    }

    /// The published work list, while a refresher keeps one.
    pub(crate) fn published_work(&self) -> Option<Arc<published_list::Publication<WorkRows>>> {
        self.smalltalk.published_work.newest()
    }

    pub(crate) fn published_work_list(&self) -> &published_list::PublishedList<WorkRows> {
        &self.smalltalk.published_work
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_sorts_by_state_then_readiness_path_and_subject() {
        let mut rows = WorkRows::default();
        for (subject, status, epoch, step) in [
            ("step-run/g/b", "claimed", 1, "b"),
            ("step-run/g/a", "ready", 3, "a"),
            ("step-run/g/c", "ready", 1, "c"),
            ("step-run/h/c", "ready", 1, "c"),
            ("step-run/g/d", "blocked", 0, "d"),
        ] {
            let mut view: StepRunView = serde_json::from_value(json!({
                "subject": subject, "run": "mission-run/r", "generation": "run-generation/g",
                "definition_hash": "d", "step": step, "status": status, "attempt": 1,
                "agentless": false, "worker_reported": false, "readiness_epoch": 0,
                "created_at_unix_ms": 0, "updated_at_unix_ms": 0,
            }))
            .unwrap();
            view.readiness_epoch = epoch;
            rows.rows.insert(
                subject.into(),
                Arc::new(WorkRow {
                    view: Arc::new(view),
                    value: Arc::new(Value::Null),
                    time_unix_ms: 0,
                    lease_unix_ms: None,
                    timing_lease_unix_ms: None,
                }),
            );
        }
        rows.sort();
        assert_eq!(
            rows.order,
            ["step-run/g/c", "step-run/h/c", "step-run/g/a", "step-run/g/b", "step-run/g/d"]
        );
    }
}
