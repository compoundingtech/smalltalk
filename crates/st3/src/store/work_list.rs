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
    /// Each seat's queue order at `orders_cut`, for the seats rows show, read in the fold's own
    /// snapshot: an actor's ready work is ordered by it, never by a newer cut's order.
    pub(crate) seat_orders: HashMap<String, Arc<Vec<String>>>,
    /// The cut `seat_orders` was read at. A publication is complete only when it equals the
    /// publication's cut.
    pub(crate) orders_cut: u64,
    /// The actors' orders over these rows, built on an actor's first read and dropped with the
    /// publication. A clone for the next fold starts empty.
    pub(crate) actors: ActorOrders,
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

/// How many actors' orders one publication keeps. Each holds at most one `u32` per row, so a
/// publication's memo retains at most this many times four bytes per row.
pub(crate) const ACTOR_ORDERS: usize = 64;

/// The rows each actor sees of one publication, as indexes into its `order`, in the order the
/// direct read gives that actor. An actor's order is built once per publication, by its first
/// read; concurrent reads of the same actor wait for that one build, and no lock is held across
/// it. The least recently read actor is dropped past [`ACTOR_ORDERS`].
#[derive(Default)]
pub(crate) struct ActorOrders {
    entries: Mutex<std::collections::VecDeque<(String, Arc<std::sync::OnceLock<Arc<Vec<u32>>>>)>>,
    /// Rows scanned building actors' orders, and the builds and reuses, for tests.
    #[cfg(test)]
    pub(crate) scanned: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) builds: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) hits: std::sync::atomic::AtomicUsize,
}

// A clone for the next fold's rows starts with no actor's order: they are of these rows only.
impl Clone for ActorOrders {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl ActorOrders {
    /// `viewer`'s order, built by `build` unless an earlier read built it. Says whether this
    /// read built it.
    fn get_or_build(&self, viewer: &str, build: impl FnOnce() -> Vec<u32>) -> (Arc<Vec<u32>>, bool) {
        let cell = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            match entries.iter().position(|(actor, _)| actor == viewer) {
                Some(position) => {
                    let entry = entries.remove(position).expect("the entry just found");
                    let cell = Arc::clone(&entry.1);
                    entries.push_back(entry);
                    cell
                }
                None => {
                    if entries.len() >= ACTOR_ORDERS {
                        entries.pop_front();
                    }
                    let cell = Arc::new(std::sync::OnceLock::new());
                    entries.push_back((viewer.to_owned(), Arc::clone(&cell)));
                    cell
                }
            }
        };
        let mut built = false;
        let order = Arc::clone(cell.get_or_init(|| {
            built = true;
            Arc::new(build())
        }));
        #[cfg(test)]
        {
            let counter = if built { &self.builds } else { &self.hits };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        (order, built)
    }

    /// The bytes the kept orders hold: four per row index.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.iter().filter_map(|(_, cell)| cell.get()).map(|order| order.len() * 4).sum()
    }
}

/// The seat whose queue orders `viewer`'s ready work, as the direct read names it.
fn viewer_seat(viewer: &str) -> String {
    normalize_seat(viewer)
}

/// Whether `viewer` sees `view` in its own work list, as the direct read's actor filter does.
fn visible_to(view: &StepRunView, viewer: &str) -> bool {
    !view.agentless
        && (view.assigned_to.as_deref() == Some(viewer)
            || view.claimant.as_deref() == Some(viewer)
            || (matches!(view.status.as_str(), "ready" | "blocked")
                && view.available_to.iter().any(|seat| seat == viewer)))
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

    /// The agent seats the rows name, each of which may read its own list: whose queue order
    /// the publication keeps.
    pub(crate) fn seats_shown(&self) -> BTreeSet<String> {
        let mut seats = BTreeSet::new();
        for row in self.rows.values().filter(|row| !row.view.agentless) {
            let view = &row.view;
            let named = view.assigned_to.iter().chain(view.claimant.iter()).chain(view.available_to.iter());
            seats.extend(named.filter(|seat| seat.starts_with("agent/")).map(|seat| viewer_seat(seat)));
        }
        seats
    }

    /// The rows `actor` sees, as indexes into `order`, in the order the direct read gives it:
    /// its ready work by its seat queue at this publication's own cut. Built on the actor's
    /// first read of this publication; says whether this read built it.
    pub(crate) fn actor_rows(&self, actor: &str) -> (Arc<Vec<u32>>, bool) {
        let viewer = smallclaims::store::normalize_actor(actor, "agent");
        self.actors.get_or_build(&viewer, || self.actor_order(&viewer))
    }

    fn actor_order(&self, viewer: &str) -> Vec<u32> {
        #[cfg(test)]
        self.actors.scanned.fetch_add(self.order.len(), std::sync::atomic::Ordering::Relaxed);
        let mut shown = self
            .order
            .iter()
            .enumerate()
            .map(|(index, step)| (index as u32, &self.rows[step].view))
            .filter(|(_, view)| visible_to(view, viewer))
            .collect::<Vec<_>>();
        let seat = viewer_seat(viewer);
        let queue = self.seat_orders.get(&seat).map_or(&[][..], |order| order.as_slice());
        let steps = shown
            .iter()
            .map(|(_, view)| crate::seat_queue::SeatStep::from(&***view))
            .collect::<Vec<_>>();
        let ready = crate::seat_queue::select(&seat, &steps, queue)
            .ready
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let rank = |subject: &str| ready.iter().position(|step| step == subject).unwrap_or(usize::MAX);
        shown.sort_by(|(_, left), (_, right)| {
            work_state_rank(&left.status)
                .cmp(&work_state_rank(&right.status))
                .then_with(|| rank(&left.subject).cmp(&rank(&right.subject)))
                .then_with(|| left.readiness_epoch.cmp(&right.readiness_epoch))
                .then_with(|| left.step.cmp(&right.step))
                .then_with(|| left.subject.cmp(&right.subject))
        });
        shown.into_iter().map(|(index, _)| index).collect()
    }

    /// The rendered rows at `indexes[offset..]`, at most `limit` of them, and whether more
    /// follow. Clones only the rows it returns.
    pub(crate) fn page_of(&self, indexes: Option<&[u32]>, offset: usize, limit: usize) -> (Vec<Value>, bool) {
        let len = indexes.map_or(self.order.len(), <[u32]>::len);
        let end = offset.saturating_add(limit).min(len);
        let items = (offset.min(end)..end)
            .map(|position| {
                let index = indexes.map_or(position, |indexes| indexes[position] as usize);
                (*self.rows[&self.order[index]].value).clone()
            })
            .collect();
        (items, end < len)
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

    /// The published work list, while a refresher keeps one. Readers use
    /// [`published_list::PublishedList::current`], which also refuses a forgotten generation.
    #[cfg(test)]
    pub(crate) fn published_work(&self) -> Option<Arc<published_list::Publication<WorkRows>>> {
        self.smalltalk.published_work.newest()
    }

    /// Each of `seats`' queue order, read in the caller's snapshot. A seat with no queued run
    /// is left out, as reading it gives no order.
    pub(crate) fn seat_orders_of(&self, seats: &BTreeSet<String>) -> Result<HashMap<String, Arc<Vec<String>>>> {
        let mut orders = HashMap::new();
        for seat in seats {
            let order = self.seat_run_order(seat)?;
            if !order.is_empty() {
                orders.insert(seat.clone(), Arc::new(order));
            }
        }
        Ok(orders)
    }

    /// Count one direct current-work read, which an enabled work list never makes. Counted in
    /// tests only.
    pub(crate) fn count_direct_work_read(&self) {
        #[cfg(test)]
        self.smalltalk.direct_work_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn direct_work_reads(&self) -> usize {
        self.smalltalk.direct_work_reads.load(std::sync::atomic::Ordering::Relaxed)
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
