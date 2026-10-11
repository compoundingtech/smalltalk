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
    /// The earliest claim about the step accepted after the row's time, which the row's
    /// time-fenced fields (progress, timing, answers) leave out until the list's time passes it.
    pub(crate) next_claim_unix_ms: Option<u128>,
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
    /// The projection frontier at the fold, from [`Store::projection_frontier`].
    pub(crate) frontier: u64,
}

/// Every leased step's lease end, by the lease index.
pub(crate) const WORK_LEASES: &str = "SELECT subject, CAST(lease_expires_at_unix_ms AS INTEGER) FROM step_runs
 INDEXED BY step_runs_lease_index
 WHERE lease_owner IS NOT NULL AND lease_expires_at_unix_ms IS NOT NULL";
/// The earliest claim about step `?1` accepted after `?2`, by the subject index.
pub(crate) const NEXT_CLAIM_AFTER: &str = "SELECT MIN(CAST(accepted_at_unix_ms AS INTEGER)) FROM claims
 WHERE subject=?1 AND CAST(accepted_at_unix_ms AS INTEGER)>?2";

/// What claims between two cuts changed in the work list.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkChanges {
    /// Steps whose row any of those claims can change.
    pub(crate) steps: BTreeSet<String>,
    /// Whether a seat's queue moved, which reorders that seat's windows with no row changing.
    pub(crate) reorder: bool,
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
    /// it, revision proposals for that run, the person asks that block it, which their
    /// requesters' declarations and runtime decide, and the seats it owns with their usage. A
    /// claim about any other subject changes no row. `seats` names the step each seat shown on
    /// a row belonged to when that row was folded.
    pub(crate) fn work_list_changes(
        &self,
        after: u64,
        through: u64,
        seats: &HashMap<String, String>,
    ) -> Result<WorkChanges> {
        let connection = self.readers.get();
        let mut steps = BTreeSet::new();
        let mut reorder = false;
        let mut runs = BTreeSet::new();
        let mut requesters = BTreeSet::new();
        let mut every_ask = false;
        let mut tree_roots = BTreeSet::new();
        let mut statement = connection.prepare_cached(published_list::CLAIMS_SINCE)?;
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
                // A run's state ends the steps of the runs under it, and decides whether asks
                // in them are current: refold the runs under it too.
                if matches!(kind.as_str(), "mission-run.state" | "mission-run.created") {
                    tree_roots.insert(run.to_owned());
                }
            } else if let Some(generation) = subject.strip_prefix("run-generation/") {
                let mut owner = connection
                    .prepare_cached("SELECT run_id FROM run_generations WHERE id=?1")?;
                if let Some(run) = owner.query_row([generation], |row| row.get::<_, String>(0)).optional()? {
                    runs.insert(run);
                }
            } else if let Some(proposal) = subject.strip_prefix("revision-proposal/") {
                // A draining run shows only its held steps, with no claim on the run.
                runs.extend(self.revision_proposal_run(proposal)?);
            } else if subject.starts_with("step-run/") {
                // A person's answer or a cancelled ask unblocks the step that asked, and ends
                // a run the ask started; answers and cancellations name neither, the ask does.
                if kind.starts_with("work.person-") {
                    let (asked_from, started) = self.ask_origin_and_run(&subject)?;
                    steps.extend(origin.or(asked_from));
                    runs.extend(started.map(|run| run.trim_start_matches("mission-run/").to_owned()));
                }
                steps.insert(subject);
            } else if subject.starts_with("subscription/") || subject.starts_with("schedule/") {
                // An ask's currency walks the subscription or schedule that delivered its run.
                every_ask = true;
            } else if subject.starts_with("agent/") {
                // A seat a step owns shows on that step's row with its usage.
                let mut owned = connection.prepare_cached(
                    "SELECT owner_step FROM desired WHERE subject=?1 AND owner_step IS NOT NULL",
                )?;
                for step in owned.query_map([&subject], |row| row.get::<_, String>(0))? {
                    steps.insert(step?);
                }
                // A seat that left its step: that step's row no longer shows it.
                steps.extend(seats.get(&subject).cloned());
                if published_list::decides_asks(&kind) {
                    requesters.insert(subject);
                }
                // A seat's queue order moves its ready work in its own windows.
                reorder |= kind == crate::seat_queue::MOVED_CLAIM;
            }
        }
        runs.extend(self.runs_under(&tree_roots)?);
        // A step that asked decides whether its asks are current, as when it leaves waiting.
        let asked = self.asks_from_steps(&steps)?;
        steps.extend(asked);
        // A requester's declaration and runtime decide whether its asks still block steps.
        let asks = if every_ask { self.every_ask()? } else { self.asks_by_requesters(&requesters)? };
        for (ask, origin) in asks {
            steps.insert(ask);
            steps.extend(origin);
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
        Ok(WorkChanges { steps, reorder })
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
        let mut statement = connection.prepare_cached(WORK_LEASES)?;
        let leases = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as u128)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(leases)
    }

    /// When the earliest claim about `subject` accepted after `time` was accepted. Replicated
    /// claims keep their own times, so one can arrive before the list's time reaches it.
    pub(crate) fn next_claim_after(&self, subject: &str, time: u128) -> Result<Option<u128>> {
        let connection = self.readers.get();
        let next = connection
            .prepare_cached(NEXT_CLAIM_AFTER)?
            .query_row(params![subject, time.min(i64::MAX as u128) as i64], |row| row.get::<_, Option<i64>>(0))?;
        Ok(next.map(|next| next.max(0) as u128))
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
    fn the_published_lists_reads_search_their_indexes_and_scan_no_table() {
        let store = Store::open_memory("node").unwrap();
        let connection = store.readers.get();
        for (name, sql) in [
            ("claims since", published_list::CLAIMS_SINCE.to_owned()),
            ("asks by requesters", published_list::ASKS_BY_REQUESTERS.to_owned()),
            ("ask origin", published_list::ASK_ORIGIN_AND_RUN.to_owned()),
            ("asks from steps", published_list::ASKS_FROM_STEPS.to_owned()),
            ("run steps", published_list::RUN_STEPS.to_owned()),
            ("child runs", published_list::CHILD_RUNS.to_owned()),
            ("work leases", WORK_LEASES.to_owned()),
            ("next claim", NEXT_CLAIM_AFTER.to_owned()),
            ("selected work", selected_work_at_snapshot_query(false)),
            ("selected mission keys", super::super::mission_list::mission_keys_sql(true)),
        ] {
            let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let parameters = statement.parameter_count();
            let plan = statement
                .query_map(rusqlite::params_from_iter(std::iter::repeat_n("[]", parameters)), |row| {
                    row.get::<_, String>(3)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            for line in &plan {
                for table in ["claims", "step_runs", "mission_runs", "mission_definitions"] {
                    assert!(
                        !line.starts_with(&format!("SCAN {table}")),
                        "{name} scans {table}:\n{}",
                        plan.join("\n")
                    );
                }
            }
        }
    }

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
                    next_claim_unix_ms: None,
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
