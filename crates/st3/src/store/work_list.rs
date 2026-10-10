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
    /// Each registered seat's queue order at `orders_cut`, read in the fold's own snapshot: an
    /// actor's ready work is ordered by it, never by a newer cut's order. Only nonempty orders.
    pub(crate) seat_orders: Arc<HashMap<String, Arc<Vec<String>>>>,
    /// The runs each registered seat's queue order read (see [`SeatQueue::runs`]), and the
    /// reverse: the seats that read each run. A seat is registered while it has an order or any
    /// such run; a change to any run it read rereads it. Shared between publications until a
    /// fold changes them.
    pub(crate) seat_runs: Arc<HashMap<String, Arc<BTreeSet<String>>>>,
    pub(crate) run_seats: Arc<HashMap<String, Arc<BTreeSet<String>>>>,
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
/// The seat each of steps `?1`, a JSON array, is assigned to, if any, and its run, whatever its
/// state or generation, by the primary key: a seat's queue order follows the runs it holds
/// steps in.
pub(crate) const STEP_ASSIGNEES: &str = "SELECT assignee, run_id FROM step_runs
 WHERE subject IN (SELECT value FROM json_each(?1))";

/// One run key for every source: `mission-run/ID`, whether a source names the run bare.
pub(crate) fn run_key(run: &str) -> String {
    normalize_mission_run(run)
}

/// One seat's queue order and every run its read depends on: the runs it joined (an open run it
/// holds a current step in), the runs its moves name or anchor on (whether or not it holds a
/// step there), and those named runs' own joins. A change to any of them can change the order.
pub(crate) struct SeatQueue {
    pub(crate) order: Vec<String>,
    pub(crate) runs: BTreeSet<String>,
}
/// Whether a replica record was repaired by a claim after `?1` through `?2`, by the kind index.
pub(crate) const REPAIRED_SINCE: &str = "SELECT 1 FROM claims
 WHERE kind='record.repaired' AND store_index>?1 AND store_index<=?2 LIMIT 1";
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
    /// The seats whose queue moved.
    pub(crate) moved_seats: BTreeSet<String>,
    /// The runs those claims name or reach, as run keys: a seat that read one rereads it.
    pub(crate) runs: BTreeSet<String>,
}

/// How many actors' orders one publication keeps. Each holds at most one `u32` per row, so a
/// publication's memo retains at most this many times four bytes per row.
pub(crate) const ACTOR_ORDERS: usize = 64;

/// The rows each actor sees of one publication, as indexes into its `order`, in the order the
/// direct read gives that actor. Every in-flight build has one cell per actor: concurrent reads
/// of the same actor wait on it, so an actor's order is built once, and no lock is held across a
/// build. Up to [`ACTOR_ORDERS`] actors are kept, the least recently read built one dropped
/// first; one still being built is never dropped. When every kept actor is still being built, a
/// new actor's cell is registered as overflow: reads of that actor share it, and it is removed
/// once built. A dropped order lives on only while a read or a page cursor still holds it.
#[derive(Default)]
pub(crate) struct ActorOrders {
    entries: Mutex<std::collections::VecDeque<ActorEntry>>,
    /// Rows scanned building actors' orders, builds, reuses and overflow registrations, for
    /// tests.
    #[cfg(test)]
    pub(crate) scanned: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) builds: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) hits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) overflow: std::sync::atomic::AtomicUsize,
    /// Seats a fold reread and found unchanged, so copied nothing, for tests. Kept here, on a
    /// field every clone resets, so each publication counts its own fold's.
    #[cfg(test)]
    pub(crate) unchanged_seats: std::sync::atomic::AtomicUsize,
}

type OrderCell = Arc<std::sync::OnceLock<Arc<Vec<u32>>>>;

/// One actor's registered cell: kept, or overflow until it is built, and how many reads hold
/// it, counted under the registry's lock so the last one to leave decides its removal.
struct ActorEntry {
    actor: String,
    cell: OrderCell,
    kept: bool,
    readers: usize,
}

// A clone for the next fold's rows starts with no actor's order: they are of these rows only.
impl Clone for ActorOrders {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// A read's registration of its cell, released however the read ends, its build's panic
/// included. Under the registry's lock it counts itself out and decides: a built overflow cell
/// is removed, a built kept one stays, and an unbuilt one (every build of it panicked) is
/// removed by the last read to leave it, so none is stranded and none removed while another
/// read can still build it. It removes only the very cell it registered, never a later one.
struct Registration<'a> {
    orders: &'a ActorOrders,
    actor: &'a str,
    cell: &'a OrderCell,
    kept: bool,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut entries = self.orders.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(position) =
            entries.iter().position(|entry| entry.actor == self.actor && Arc::ptr_eq(&entry.cell, self.cell))
        else {
            // Evicted once built: nothing to release.
            return;
        };
        let entry = &mut entries[position];
        entry.readers -= 1;
        let remove = if self.cell.get().is_some() {
            // Built: an overflow cell has served the reads that shared it; a kept one stays.
            !self.kept
        } else {
            // Its build panicked. A read still holding the cell builds it next, and new reads
            // find it; when none is left, a new read registers a new cell.
            entry.readers == 0
        };
        if remove {
            entries.remove(position);
        }
    }
}

impl ActorOrders {
    /// `viewer`'s order, built by `build` unless another read of the same cell built it. Says
    /// whether this read built it.
    fn get_or_build(&self, viewer: &str, build: impl FnOnce() -> Vec<u32>) -> (Arc<Vec<u32>>, bool) {
        let (cell, kept) = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            match entries.iter().position(|entry| entry.actor == viewer) {
                Some(position) => {
                    entries[position].readers += 1;
                    let (cell, kept) = (Arc::clone(&entries[position].cell), entries[position].kept);
                    if kept {
                        let entry = entries.remove(position).expect("the entry just found");
                        entries.push_back(entry);
                    }
                    (cell, kept)
                }
                None => {
                    let kept = entries.iter().filter(|entry| entry.kept).count() < ACTOR_ORDERS
                        || match entries.iter().position(|entry| entry.kept && entry.cell.get().is_some()) {
                            Some(built) => {
                                entries.remove(built);
                                true
                            }
                            // Every kept actor is mid-build: this one is overflow.
                            None => false,
                        };
                    #[cfg(test)]
                    {
                        if !kept {
                            self.overflow.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    let cell = OrderCell::default();
                    entries.push_back(ActorEntry { actor: viewer.to_owned(), cell: Arc::clone(&cell), kept, readers: 1 });
                    (cell, kept)
                }
            }
        };
        let registration = Registration { orders: self, actor: viewer, cell: &cell, kept };
        let mut built = false;
        let order = Arc::clone(cell.get_or_init(|| {
            built = true;
            Arc::new(build())
        }));
        drop(registration);
        #[cfg(test)]
        {
            let counter = if built { &self.builds } else { &self.hits };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        (order, built)
    }

    /// The bytes the kept and overflow actors hold: each order's allocated row indexes, its
    /// actor's name, and its entry and cell. Orders dropped while a read or cursor still holds
    /// them are not here.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .iter()
            .map(|entry| {
                let order = entry.cell.get().map_or(0, |order| order.capacity() * 4 + std::mem::size_of::<Vec<u32>>());
                entry.actor.capacity()
                    + std::mem::size_of::<ActorEntry>()
                    + std::mem::size_of::<std::sync::OnceLock<Arc<Vec<u32>>>>()
                    + order
            })
            .sum()
    }

    /// How many reads hold `actor`'s registered cell, if one is registered.
    #[cfg(test)]
    pub(crate) fn readers(&self, actor: &str) -> Option<usize> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.iter().find(|entry| entry.actor == actor).map(|entry| entry.readers)
    }

    /// How many actors are kept, how many of those are still being built, and how many overflow
    /// cells are registered.
    #[cfg(test)]
    pub(crate) fn kept(&self) -> (usize, usize, usize) {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let kept = entries.iter().filter(|entry| entry.kept);
        (
            kept.clone().count(),
            kept.filter(|entry| entry.cell.get().is_none()).count(),
            entries.iter().filter(|entry| !entry.kept).count(),
        )
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
    /// the publication keeps. Every row's, for a fold from nothing or in chunks.
    pub(crate) fn seats_shown(&self) -> BTreeSet<String> {
        self.seats_of(self.rows.keys())
    }

    /// The agent seats the rows of `steps` name, of the steps this list holds.
    pub(crate) fn seats_of<'a>(&self, steps: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
        let mut seats = BTreeSet::new();
        for row in steps.into_iter().filter_map(|step| self.rows.get(step)).filter(|row| !row.view.agentless) {
            let view = &row.view;
            let named = view.assigned_to.iter().chain(view.claimant.iter()).chain(view.available_to.iter());
            seats.extend(named.filter(|seat| seat.starts_with("agent/")).map(|seat| viewer_seat(seat)));
        }
        seats
    }

    /// Register `seat`'s queue as just read, `None` when it has neither queued run nor move:
    /// its order, its runs, and the reverse index, each link of its earlier runs removed and
    /// emptied buckets dropped. A seat stays registered while it has an order or any run. Says
    /// whether its order changed.
    pub(crate) fn set_seat_queue(&mut self, seat: &str, queue: Option<SeatQueue>) -> bool {
        // Unchanged, order and every run it read: nothing to copy or relink.
        let registered_order = self.seat_orders.get(seat).map(|order| order.as_slice());
        let registered_runs = self.seat_runs.get(seat).map(|runs| &**runs);
        let unchanged = match &queue {
            Some(queue) if !queue.runs.is_empty() || !queue.order.is_empty() => {
                registered_runs == Some(&queue.runs)
                    && registered_order.unwrap_or_default() == queue.order.as_slice()
            }
            _ => registered_runs.is_none() && registered_order.is_none(),
        };
        if unchanged {
            #[cfg(test)]
            self.actors.unchanged_seats.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        let seat_runs = Arc::make_mut(&mut self.seat_runs);
        let run_seats = Arc::make_mut(&mut self.run_seats);
        if let Some(runs) = seat_runs.remove(seat) {
            for run in runs.iter() {
                if let Some(seats) = run_seats.get_mut(run) {
                    Arc::make_mut(seats).remove(seat);
                    if seats.is_empty() {
                        run_seats.remove(run);
                    }
                }
            }
        }
        let orders = Arc::make_mut(&mut self.seat_orders);
        let old = orders.remove(seat);
        let mut new = None;
        if let Some(queue) = queue.filter(|queue| !queue.runs.is_empty() || !queue.order.is_empty()) {
            for run in &queue.runs {
                Arc::make_mut(run_seats.entry(run.clone()).or_default()).insert(seat.to_owned());
            }
            seat_runs.insert(seat.to_owned(), Arc::new(queue.runs));
            if !queue.order.is_empty() {
                new = Some(Arc::new(queue.order));
            }
        }
        let changed = old.as_deref() != new.as_deref();
        if let Some(order) = new {
            orders.insert(seat.to_owned(), order);
        }
        changed
    }

    /// Clear every registered seat, as a fold that reads every shown seat's queue again does.
    pub(crate) fn clear_seat_queues(&mut self) {
        self.seat_orders = Arc::default();
        self.seat_runs = Arc::default();
        self.run_seats = Arc::default();
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
        // The direct read hands `select` its own seat string (the actor if it names a kind, else
        // `agent/ACTOR`, which is `viewer`) and looks the queue up by the normalized seat.
        let queue = self.seat_orders.get(&viewer_seat(viewer)).map_or(&[][..], |order| order.as_slice());
        let steps = shown
            .iter()
            .map(|(_, view)| crate::seat_queue::SeatStep::from(&***view))
            .collect::<Vec<_>>();
        let ready = crate::seat_queue::select(viewer, &steps, queue)
            .ready
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        // Ranks looked up once each, not searched for in every comparison.
        let ranks = ready.iter().enumerate().map(|(rank, step)| (step.as_str(), rank)).collect::<HashMap<_, _>>();
        let rank = |subject: &str| ranks.get(subject).copied().unwrap_or(usize::MAX);
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

    /// The rendered rows at `indexes`, or every row, in order, sharing this publication's values:
    /// what a page cursor keeps of it.
    pub(crate) fn selected_rows(&self, indexes: Option<&[u32]>) -> Vec<Arc<Value>> {
        match indexes {
            Some(indexes) => indexes
                .iter()
                .map(|&index| Arc::clone(&self.rows[&self.order[index as usize]].value))
                .collect(),
            None => self.order.iter().map(|step| Arc::clone(&self.rows[step].value)).collect(),
        }
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
        let mut moved_seats = BTreeSet::new();
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
                if kind == crate::seat_queue::MOVED_CLAIM {
                    reorder = true;
                    moved_seats.insert(viewer_seat(&subject));
                }
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
        let runs = runs.iter().map(|run| run_key(run)).collect();
        Ok(WorkChanges { steps, reorder, moved_seats, runs })
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

    /// The seats `steps` are assigned to and the runs they are in, whatever their state or
    /// generation, read in the caller's snapshot. Every step's run, with or without a seat.
    pub(crate) fn step_assignees(&self, steps: &BTreeSet<String>) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        if steps.is_empty() {
            return Ok(Default::default());
        }
        let connection = self.readers.get();
        let rows = connection
            .prepare_cached(STEP_ASSIGNEES)?
            .query_map([serde_json::to_string(steps)?], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let seats = rows
            .iter()
            .filter_map(|(seat, _)| seat.as_deref())
            .filter(|seat| seat.starts_with("agent/"))
            .map(viewer_seat)
            .collect();
        let runs = rows.iter().map(|(_, run)| run_key(run)).collect();
        Ok((seats, runs))
    }

    /// Each of `seats`' queue order and the runs it read, in the caller's snapshot, from the same
    /// inputs [`Store::seat_run_order`] reads. A seat with no queued run and no move is left
    /// out. One read of every seat's inputs serves them all: per seat, the moves query scans
    /// every move claim by kind and the joins every open run anyway.
    pub(crate) fn seat_queues_of(&self, seats: &BTreeSet<String>) -> Result<HashMap<String, SeatQueue>> {
        #[cfg(test)]
        {
            self.smalltalk.seat_order_reads.fetch_add(seats.len(), std::sync::atomic::Ordering::Relaxed);
            *self.smalltalk.last_seats_read.lock().unwrap() = seats.clone();
        }
        if seats.is_empty() {
            return Ok(HashMap::new());
        }
        smallclaims::touched::note_read(|| format!("kind:{}", seat_queue::MOVED_CLAIM));
        let connection = self.readers.get();
        // Every move and join in one pass, but only these seats' named runs probed.
        let mut every = seat_queue_inputs_of_tx(&connection, None, Some(seats))?;
        let mut queues = HashMap::new();
        for seat in seats {
            let Some(inputs) = every.remove(seat) else {
                continue;
            };
            let order = inputs.live_order();
            let mut runs = inputs.joins.iter().map(|join| run_key(&join.run)).collect::<BTreeSet<_>>();
            for recorded in &inputs.moves {
                runs.insert(run_key(&recorded.movement.run));
                runs.extend(recorded.movement.anchor.as_deref().map(run_key));
            }
            queues.insert(seat.clone(), SeatQueue { order, runs });
        }
        Ok(queues)
    }

    /// Whether a replica record was repaired by a claim after `after` through `through`, read in
    /// the caller's snapshot. A repair applied with its claim, as `apply` does, drops a seat's
    /// move with no claim about the seat and no forget; the other routes forget when the record
    /// flips. Only claims newer than `after` count, so one repair folds the list from nothing
    /// once, however far a lagging frontier makes a fold read back.
    pub(crate) fn repaired_since(&self, after: u64, through: u64) -> Result<bool> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(REPAIRED_SINCE)?
            .query_row(params![after, through], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Set how far replicated claims are projected, as a replication pass would, for tests.
    #[cfg(test)]
    pub(crate) fn set_projection_frontier_for_test(&self, index: u64) {
        let connection = self.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
                 VALUES ('graph', 'healthy', ?1, '0')
                 ON CONFLICT(aggregate) DO UPDATE SET last_good_store_index=excluded.last_good_store_index",
                [index],
            )
            .unwrap();
    }

    /// Count `count` runs a warm fold looked up in its reverse index, for tests.
    pub(crate) fn count_seat_lookups(&self, count: usize) {
        #[cfg(test)]
        self.smalltalk.seat_lookups.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = count;
    }

    /// Record `claim` as a replica record of `state`, as replication would, for tests.
    #[cfg(test)]
    pub(crate) fn record_replica_for_test(&self, record_ref: &str, claim: &str, state: &str) {
        let connection = self.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO replica_records(record_ref, writer, sequence, envelope_hash, position,
                    raw, state, claim_id, updated_at_unix_ms)
                 VALUES (?1, 'peer', 1, 'envelope', 0, x'00', ?3, ?2, '0')",
                params![record_ref, claim, state],
            )
            .unwrap();
    }

    #[cfg(test)]
    pub(crate) fn seat_lookups(&self) -> usize {
        self.smalltalk.seat_lookups.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn last_seats_read(&self) -> BTreeSet<String> {
        self.smalltalk.last_seats_read.lock().unwrap().clone()
    }

    /// Count one direct current-work read, which an enabled work list never makes. Counted in
    /// tests only.
    pub(crate) fn count_direct_work_read(&self) {
        #[cfg(test)]
        self.smalltalk.direct_work_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many seats' queue orders folds have read, for tests.
    #[cfg(test)]
    pub(crate) fn seat_order_reads(&self) -> usize {
        self.smalltalk.seat_order_reads.load(std::sync::atomic::Ordering::Relaxed)
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
            ("step assignees", STEP_ASSIGNEES.to_owned()),
            ("repaired since", REPAIRED_SINCE.to_owned()),
            ("seat queue moves", seat_queue_moves_query()),
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
    fn an_actor_mid_build_is_never_evicted_so_a_second_read_waits_for_it() {
        use std::sync::atomic::Ordering::Relaxed;
        let orders = ActorOrders::default();
        let orders = &orders;
        let (started, building) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Owned in the scope: a failing assertion drops it, which unblocks every worker.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let first = scope.spawn(move || {
                orders.get_or_build("agent/a", move || {
                    started.send(()).unwrap();
                    held.recv().unwrap();
                    vec![3, 1, 2]
                })
            });
            building.recv_timeout(WAIT).expect("a worker started");
            // Churn every other slot while a's build is under way: a stays kept.
            for n in 0..ACTOR_ORDERS {
                orders.get_or_build(&format!("agent/other-{n}"), || vec![n as u32]);
            }
            assert_eq!(orders.kept(), (ACTOR_ORDERS, 1, 0), "a, mid-build, was not evicted");
            // A second read of a waits for the one build rather than building again.
            let second = scope.spawn(|| orders.get_or_build("agent/a", || panic!("a second build of agent/a")));
            release.send(()).unwrap();
            assert_eq!((*first.join().unwrap().0).clone(), vec![3, 1, 2]);
            let (order, built) = second.join().unwrap();
            assert!(!built);
            assert_eq!(*order, vec![3, 1, 2]);
        });
        assert_eq!(orders.builds.load(Relaxed), ACTOR_ORDERS + 1);
        assert_eq!(orders.overflow.load(Relaxed), 0);
    }

    /// How long a test waits on another thread before failing instead of hanging.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Wait until `count` reads hold `actor`'s registered cell: proof each one registered.
    fn wait_for_readers(orders: &ActorOrders, actor: &str, count: usize) {
        let deadline = std::time::Instant::now() + WAIT;
        while orders.readers(actor) != Some(count) {
            assert!(std::time::Instant::now() < deadline, "{actor} never had {count} readers");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Block every kept slot's build, run `then` while they are all mid-build, then release them.
    fn while_saturated(orders: &ActorOrders, then: impl FnOnce()) {
        let (started, building) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Owned in the scope: a failing assertion drops it, which unblocks every worker.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let held = std::sync::Arc::new(std::sync::Mutex::new(held));
            let builders = (0..ACTOR_ORDERS)
                .map(|n| {
                    let (started, held) = (started.clone(), std::sync::Arc::clone(&held));
                    scope.spawn(move || {
                        orders.get_or_build(&format!("agent/slow-{n}"), move || {
                            started.send(()).unwrap();
                            held.lock().unwrap().recv().unwrap();
                            vec![n as u32]
                        })
                    })
                })
                .collect::<Vec<_>>();
            for _ in 0..ACTOR_ORDERS {
                building.recv_timeout(WAIT).expect("a worker started");
            }
            assert_eq!(orders.kept(), (ACTOR_ORDERS, ACTOR_ORDERS, 0), "every kept slot mid-build");
            then();
            for _ in 0..ACTOR_ORDERS {
                release.send(()).unwrap();
            }
            for builder in builders {
                assert!(builder.join().unwrap().1);
            }
        });
    }

    #[test]
    fn with_every_kept_slot_mid_build_one_overflow_cell_serves_every_read_of_an_actor() {
        use std::sync::atomic::Ordering::Relaxed;
        let orders = ActorOrders::default();
        while_saturated(&orders, || {
            let (started, building) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                // Owned in the scope: a failing assertion drops it, which unblocks every worker.
                let (release, held) = std::sync::mpsc::channel::<()>();
                let first = scope.spawn(|| {
                    orders.get_or_build("agent/late", move || {
                        started.send(()).unwrap();
                        held.recv().unwrap();
                        vec![7]
                    })
                });
                building.recv_timeout(WAIT).expect("a worker started");
                assert_eq!(orders.kept().2, 1, "one overflow cell, registered");
                // A second read of the same actor shares that cell: one build.
                let second = scope.spawn(|| orders.get_or_build("agent/late", || panic!("a second build of agent/late")));
                wait_for_readers(&orders, "agent/late", 2);
                release.send(()).unwrap();
                assert_eq!(first.join().unwrap(), (std::sync::Arc::new(vec![7]), true));
                assert_eq!(second.join().unwrap(), (std::sync::Arc::new(vec![7]), false));
            });
            // Built, it served its reads and is gone; still saturated, a later read is overflow
            // again, built once more and removed.
            assert_eq!(orders.kept(), (ACTOR_ORDERS, ACTOR_ORDERS, 0));
            assert_eq!(orders.get_or_build("agent/late", || vec![8]), (std::sync::Arc::new(vec![8]), true));
            assert_eq!(orders.kept().2, 0);
            assert_eq!(orders.overflow.load(Relaxed), 2);
        });
        // Slots built again: the next read of it is kept.
        orders.get_or_build("agent/late", || vec![9]);
        assert_eq!(orders.kept(), (ACTOR_ORDERS, 0, 0));
        assert_eq!(orders.get_or_build("agent/late", || panic!("kept, so not built again")).1, false);
    }

    #[test]
    fn a_build_that_panics_releases_its_cell_unless_another_read_waits_on_it() {
        let orders = ActorOrders::default();
        // Alone: its registration is removed, and the next read registers and builds afresh.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            orders.get_or_build("agent/a", || panic!("the build failed"))
        }));
        assert!(panicked.is_err());
        assert_eq!(orders.kept(), (0, 0, 0), "nothing stranded");
        assert_eq!(orders.get_or_build("agent/a", || vec![1]), (std::sync::Arc::new(vec![1]), true));
        // With another read waiting on its cell, that read builds it once, in the same cell.
        let (started, building) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Owned in the scope: a failing assertion drops it, which unblocks every worker.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let failing = scope.spawn(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    orders.get_or_build("agent/b", move || {
                        started.send(()).unwrap();
                        held.recv().unwrap();
                        panic!("the first build failed")
                    })
                }))
            });
            building.recv_timeout(WAIT).expect("a worker started");
            let waiting = scope.spawn(|| orders.get_or_build("agent/b", || vec![2]));
            // The waiting read holds the same cell before the first build fails.
            wait_for_readers(&orders, "agent/b", 2);
            release.send(()).unwrap();
            assert!(failing.join().unwrap().is_err());
            assert_eq!(waiting.join().unwrap(), (std::sync::Arc::new(vec![2]), true));
        });
        assert_eq!(orders.kept(), (2, 0, 0), "a and b kept, built, nothing orphaned");
        assert_eq!(orders.get_or_build("agent/b", || panic!("kept, so not built again")).1, false);
    }

    /// Two reads of `actor`'s one cell, both of whose builds panic: the first blocks until the
    /// second has registered, so both hold the same cell.
    fn two_failing_reads(orders: &ActorOrders, actor: &str) {
        let (started, building) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Owned in the scope: a failing assertion drops it, which unblocks every worker.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let first = scope.spawn(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    orders.get_or_build(actor, move || {
                        started.send(()).unwrap();
                        held.recv().unwrap();
                        panic!("the first build failed")
                    })
                }))
            });
            building.recv_timeout(WAIT).expect("a worker started");
            let second = scope.spawn(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    orders.get_or_build(actor, || panic!("the second build failed too"))
                }))
            });
            wait_for_readers(orders, actor, 2);
            release.send(()).unwrap();
            assert!(first.join().unwrap().is_err());
            assert!(second.join().unwrap().is_err());
        });
    }

    #[test]
    fn two_reads_whose_builds_both_panic_strand_no_cell() {
        // Kept: the last of the two reads to leave removes the unbuilt cell.
        let orders = ActorOrders::default();
        two_failing_reads(&orders, "agent/a");
        assert_eq!(orders.readers("agent/a"), None);
        assert_eq!(orders.kept(), (0, 0, 0), "no unbuilt cell left registered");
        assert_eq!(orders.get_or_build("agent/a", || vec![1]), (std::sync::Arc::new(vec![1]), true));
        // Overflow, with every kept slot mid-build: the same.
        let full = ActorOrders::default();
        while_saturated(&full, || {
            two_failing_reads(&full, "agent/late");
            assert_eq!(full.readers("agent/late"), None);
            assert_eq!(full.kept(), (ACTOR_ORDERS, ACTOR_ORDERS, 0), "no overflow cell left registered");
        });
    }

    #[test]
    fn the_seat_queue_joins_walk_open_runs_by_index_and_scan_no_table() {
        // One read per fold that reads any seat: it walks the open runs, by their partial index.
        let store = Store::open_memory("node").unwrap();
        let connection = store.readers.get();
        let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {}", seat_queue_joins_query())).unwrap();
        let parameters = statement.parameter_count();
        let plan = statement
            .query_map(rusqlite::params_from_iter(std::iter::repeat_n("[]", parameters)), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for line in &plan {
            if line.starts_with("SCAN ") && !line.starts_with("SCAN CONSTANT") {
                assert!(line.contains("USING "), "a bare table scan:\n{}", plan.join("\n"));
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
