//! Collections kept published off the request path, as the agents roster is. One refresher per
//! collection folds it after commits, at most one fold at a time, pausing at least a second and
//! as long as the fold took between folds. Each fold starts from the previous publication and
//! refolds only the rows whose claims changed; a fold from nothing reads in short snapshots. A
//! read serves the newest publication under its own cut and never folds.

use super::*;
use crate::store::mission_list::MissionRows;
use crate::store::work_list::{WorkRow, WorkRows, work_state_rank};
use crate::store::published_list::{Publication, PublishedList};
use std::collections::HashMap;
use smallclaims::store::now_ms;

/// The shortest pause between two folds of one collection. A fold also pauses as long as it
/// took, so a refresher never takes more than about half a core.
const REFRESH_PAUSE: Duration = Duration::from_secs(1);

/// The most rows one snapshot refolds. More changed rows refold in chunks, each at its own cut,
/// and a last short snapshot completes the publication at one cut.
const FOLD_CHUNK: usize = 100;

/// How many rounds of chunks a fold tries before it keeps the previous publication.
const FOLD_ROUNDS: usize = 3;

/// How many folds in a row may fail before the refresher stops serving its list.
const FAILURES_BEFORE_WITHDRAWING: usize = 3;

/// How soon a refresher tries again after a failed fold, even with no new commit.
const FAILURE_RETRY: Duration = Duration::from_secs(5);

/// The most claims one fold reads to say which rows changed. Past this many since the last
/// publication, as after a replication backlog or a heal, the list folds from nothing in chunks.
const FOLD_CLAIMS: u64 = 10_000;

/// Stop serving a list: readers fold on read and its windows follow commits again. The view is
/// withdrawn whether or not the list was being served, as after a forget.
fn withdraw<R>(store: &Store, list: fn(&Store) -> &PublishedList<R>, name: &'static str) {
    if list(store).withdraw() {
        eprintln!("st3: WARN the {name} list is no longer published; windows fold on read until it is");
    }
    store.withdraw_collection_view(name);
}

/// Withdraws its list when the refresher task ends, by any path.
struct Withdraw<R: 'static> {
    store: Arc<Store>,
    list: fn(&Store) -> &PublishedList<R>,
    name: &'static str,
}

impl<R> Drop for Withdraw<R> {
    fn drop(&mut self) {
        withdraw(&self.store, self.list, self.name);
    }
}

/// Grace periods and checkpoint waits move runs in and out of waiting on a person without a
/// claim, so the missions list rereads them at least this often.
const ATTENTION_REREAD_MS: u128 = 30_000;

/// Whether this daemon keeps its lists published. `ST3_PUBLISHED_LISTS=off` folds each window
/// on read instead, as before.
fn enabled() -> bool {
    std::env::var("ST3_PUBLISHED_LISTS").as_deref() != Ok("off")
}

/// Start the refreshers of the missions and work lists, as the daemon starts.
pub fn start_published_lists(state: &AppState) {
    if !enabled() {
        return;
    }
    spawn(
        state.store.clone(),
        |store| store.published_missions_list(),
        "missions",
        "missions/refresh",
        fold_missions,
    );
    spawn(
        state.store.clone(),
        |store| store.published_work_list(),
        "work",
        "work/refresh",
        fold_work,
    );
}

/// Fold a collection at the current cut, from the previous publication when there is one. `None`
/// when that publication is current; with each new publication, whether any row changed, so
/// windows reread only then.
type Fold<R> = fn(&Store, Option<&Publication<R>>) -> anyhow::Result<Option<(Publication<R>, bool)>>;

/// Keep one collection published: fold after commits, when a row's deadline passes, and again
/// shortly after a failed fold, never more than one fold at a time.
fn spawn<R: Send + Sync + 'static>(
    store: Arc<Store>,
    list: fn(&Store) -> &PublishedList<R>,
    name: &'static str,
    label: &'static str,
    fold: Fold<R>,
) {
    let Some(wake) = list(&store).start() else {
        return;
    };
    tokio::spawn(async move {
        // The writer's callback only wakes this task; it never reads.
        let commits = Arc::clone(&wake);
        let _observer = store.observe_commits(move |_| commits.notify_one());
        // Should this task ever stop, windows fold on read and follow commits again.
        let _withdraw = Withdraw { store: Arc::clone(&store), list, name };
        let mut failures = 0;
        loop {
            let started = tokio::time::Instant::now();
            let reader = store.clone();
            let folded = tokio::task::spawn_blocking(move || {
                crate::profile::task(label, || {
                    let list = list(&reader);
                    let (base, generation) = list.base();
                    let folded = fold(&reader, base.as_deref());
                    if base.is_none() && folded.is_err() {
                        // The next fold starts from nothing again.
                        list.fold_from_nothing_next();
                    }
                    match folded? {
                        Some((publication, changed)) => {
                            // A fold that began before projections were replaced publishes
                            // nothing; the next one folds from nothing.
                            if let Some(served) = list.publish(publication, generation) {
                                // Windows reread only when a row changed, or the list is served
                                // again; an advanced cut alone shows them nothing new.
                                if changed || !served {
                                    reader.publish_collection_view(name);
                                }
                            }
                        }
                        // Nothing new: a withdrawn list's rows are current again.
                        None => {
                            if list.serve_again(generation) {
                                reader.publish_collection_view(name);
                            }
                        }
                    }
                    anyhow::Ok(())
                })
            })
            .await;
            let failed = match folded {
                Ok(Ok(())) => false,
                Ok(Err(error)) => {
                    eprintln!("st3: {name} refresh failed: {error:#}");
                    true
                }
                Err(error) => {
                    eprintln!("st3: {name} refresh stopped: {error}");
                    // The fold may have taken a forget with it: fold from nothing next.
                    list(&store).fold_from_nothing_next();
                    true
                }
            };
            failures = if failed { failures + 1 } else { 0 };
            // A refresher that keeps failing stops serving rows it cannot keep current: its
            // windows fold on read and follow commits until a fold publishes again.
            if failures == FAILURES_BEFORE_WITHDRAWING {
                withdraw(&store, list, name);
            }
            tokio::time::sleep(started.elapsed().max(REFRESH_PAUSE)).await;
            let deadline = list(&store).newest().and_then(|newest| newest.valid_until_unix_ms);
            let mut wait = deadline.map(|deadline| {
                Duration::from_millis(deadline.saturating_sub(now_ms()).min(u64::MAX as u128) as u64)
            });
            if failed {
                wait = Some(wait.map_or(FAILURE_RETRY, |wait| wait.min(FAILURE_RETRY)));
            }
            match wait {
                Some(wait) => {
                    let _ = tokio::time::timeout(wait, wake.notified()).await;
                }
                None => wake.notified().await,
            }
        }
    });
}

enum Advance<R, P> {
    /// Nothing changed since the base, and no deadline passed.
    Current,
    /// The rows at a newer cut or past a deadline, and whether any row changed.
    Published(Publication<R>, bool),
    /// Too many rows changed by this cut to refold in one snapshot.
    TooMany(P),
    /// Too many claims since the base to read in one snapshot: fold from nothing.
    Rebuild,
}

/// Where a fold reads changed claims from: the base's cut, or, when a replication pass has
/// projected claims since the base was folded, the frontier the base saw, so claims the base
/// folded before they were projected are read again. `None` when that is too many claims.
fn claims_from(base_cut: u64, base_frontier: u64, cut: u64, frontier: u64) -> Option<u64> {
    let from = if frontier > base_frontier { base_cut.min(base_frontier) } else { base_cut };
    (cut.saturating_sub(from) <= FOLD_CLAIMS).then_some(from)
}

/// Fold the missions list at the current cut: from `base` when there is one, refolding only the
/// missions whose claims, leases, attention or recent end changed, else from nothing in chunks.
/// `None` when `base` already is current; with each publication, whether any row changed.
fn fold_missions(
    store: &Store,
    base: Option<&Publication<MissionRows>>,
) -> anyhow::Result<Option<(Publication<MissionRows>, bool)>> {
    let advance = match base {
        Some(base) => missions_since(store, base)?,
        None => Advance::Rebuild,
    };
    let mut provisional = match advance {
        Advance::Current => return Ok(None),
        Advance::Published(publication, changed) => return Ok(Some((publication, changed))),
        Advance::TooMany(plan) => {
            store.published_missions_list().note_rebuild("chunked: missions changed");
            plan.refold_in_chunks(store, base.expect("a base had too many changes"))?
        }
        Advance::Rebuild => {
            store.published_missions_list().note_rebuild(if base.is_some() { "claims" } else { "start" });
            missions_from_nothing(store)?
        }
    };
    // Every row now holds at the provisional cut or later: catch up to one cut.
    for _ in 0..FOLD_ROUNDS {
        match missions_since(store, &provisional)? {
            Advance::Current => {
                provisional.valid_until_unix_ms = Some(valid_until(&provisional.rows, now_ms()));
                return Ok(Some((provisional, true)));
            }
            Advance::Published(publication, _) => return Ok(Some((publication, true))),
            Advance::TooMany(plan) => provisional = plan.refold_in_chunks(store, &provisional)?,
            Advance::Rebuild => break,
        }
    }
    anyhow::bail!("the missions list kept changing in ways no short fold can follow; keeping the previous one")
}

/// What a fold must refold to advance its base to `cut`.
struct Plan {
    cut: u64,
    frontier: u64,
    /// When the plan was read: the earliest time any refolded card holds at, from which the
    /// next fold looks for ended leases.
    at: u128,
    leases: HashMap<String, u128>,
    missions: BTreeSet<String>,
    /// The runs waiting on a person at `cut`, when they were reread, and when.
    attention: Option<(BTreeSet<String>, u128)>,
}

impl Plan {
    /// Refold the planned missions in chunks, each in its own snapshot at or after the plan's
    /// cut. Every row then holds at the plan's cut or later; the rest were unchanged through it.
    fn refold_in_chunks(
        self,
        store: &Store,
        base: &Publication<MissionRows>,
    ) -> anyhow::Result<Publication<MissionRows>> {
        let mut rows = base.rows.clone();
        if let Some((attention, at)) = self.attention {
            rows.attention_runs = attention;
            rows.attention_read_at_unix_ms = at;
        }
        let missions = self.missions.into_iter().collect::<Vec<_>>();
        for chunk in missions.chunks(FOLD_CHUNK) {
            let chunk = chunk.iter().cloned().collect::<BTreeSet<_>>();
            store.read_snapshot(|_| refold(store, &mut rows, &chunk, now_ms()))?;
        }
        rows.folded_at_unix_ms = self.at;
        rows.next_lease_end_unix_ms = store.read_snapshot(|_| store.next_lease_end(self.at))?;
        rows.leases = self.leases;
        rows.frontier = self.frontier;
        rows.sort();
        Ok(Publication { cut: self.cut, published_at_unix_ms: now_ms(), valid_until_unix_ms: None, rows })
    }
}

/// The first rows: every mission's ID and the runs that wait on a person at one cut, then the
/// missions' keys and the shown missions' cards in chunks, each at its own later cut and time.
fn missions_from_nothing(store: &Store) -> anyhow::Result<Publication<MissionRows>> {
    let (cut, frontier, ids, mut rows) = store.read_snapshot(|cut| {
        let now = now_ms();
        let rows = MissionRows {
            attention_runs: store.human_attention_runs()?,
            attention_read_at_unix_ms: now,
            folded_at_unix_ms: now,
            next_lease_end_unix_ms: store.next_lease_end(now)?,
            leases: store.work_leases()?,
            ..MissionRows::default()
        };
        Ok((cut, store.projection_frontier()?, store.mission_list_ids()?, rows))
    })?;
    for chunk in ids.chunks(FOLD_CHUNK) {
        let chunk = chunk.iter().cloned().collect::<BTreeSet<_>>();
        store.read_snapshot(|_| refold(store, &mut rows, &chunk, now_ms()))?;
    }
    rows.frontier = frontier;
    rows.sort();
    Ok(Publication { cut, published_at_unix_ms: now_ms(), valid_until_unix_ms: None, rows })
}

fn insert_cards(rows: &mut MissionRows, cards: Vec<Value>) {
    for card in cards {
        if let Some(id) = card["id"].as_str() {
            rows.cards.insert(id.to_owned(), Arc::new(card));
        }
    }
}

/// Refold the keys and cards of `missions` in the caller's snapshot at `now`. Only shown
/// missions keep a key and a card.
fn refold(
    store: &Store,
    rows: &mut MissionRows,
    missions: &BTreeSet<String>,
    now: u128,
) -> anyhow::Result<()> {
    if missions.is_empty() {
        return Ok(());
    }
    let mut keys = store.mission_list_keys(Some(missions), now)?;
    let mut shown = Vec::new();
    for mission in missions {
        match keys.remove(mission).filter(|key| key.visible) {
            Some(key) => {
                shown.push(mission.clone());
                rows.keys.insert(mission.clone(), key);
            }
            None => {
                rows.keys.remove(mission);
                rows.cards.remove(mission);
            }
        }
    }
    let cards = client_v0::mission_list_cards_with(store, &shown, now, &rows.attention_runs)?;
    insert_cards(rows, cards);
    Ok(())
}

/// Advance `base` to the current cut in one snapshot, if few enough missions changed.
fn missions_since(
    store: &Store,
    base: &Publication<MissionRows>,
) -> anyhow::Result<Advance<MissionRows, Plan>> {
    store.read_snapshot(|cut| {
        anyhow::ensure!(cut >= base.cut, "the store's index moved back from {} to {cut}", base.cut);
        let now = now_ms();
        let rows = &base.rows;
        let frontier = store.projection_frontier()?;
        let Some(from) = claims_from(base.cut, rows.frontier, cut, frontier) else {
            return Ok(Advance::Rebuild);
        };
        let changes = store.mission_list_changes(from, cut)?;
        let mut missions = changes.missions;
        // Worker leases that ended since the last fold show their steps ready again.
        missions.extend(store.missions_with_leases_ended(rows.folded_at_unix_ms, now)?);
        // A quiet renewal moves a lease, and its step's `since`, with no claim.
        let leases = store.work_leases()?;
        let moved = rows.leases.iter()
            .filter(|(step, end)| leases.get(*step) != Some(end))
            .map(|(step, _)| step)
            .chain(leases.keys().filter(|step| !rows.leases.contains_key(*step)));
        missions.extend(store.missions_of_steps(moved)?);
        // A recently ended mission leaves the list once its grace is over.
        missions.extend(rows.keys.iter()
            .filter(|(_, key)| key.visible_until.is_some_and(|until| until < now))
            .map(|(id, _)| id.clone()));
        let reread = changes.attention
            || now.saturating_sub(rows.attention_read_at_unix_ms) >= ATTENTION_REREAD_MS;
        let attention = if reread { Some(store.human_attention_runs()?) } else { None };
        if let Some(attention) = &attention {
            let moved = attention.symmetric_difference(&rows.attention_runs).collect::<Vec<_>>();
            missions.extend(store.missions_of_runs(moved)?);
        }
        let deadline_passed = base.valid_until_unix_ms.is_some_and(|until| until <= now);
        if cut == base.cut && frontier == rows.frontier && missions.is_empty() && !deadline_passed {
            return Ok(Advance::Current);
        }
        let attention = attention.map(|attention| (attention, now));
        if missions.len() > FOLD_CHUNK {
            return Ok(Advance::TooMany(Plan { cut, frontier, at: now, leases, missions, attention }));
        }
        let changed = !missions.is_empty();
        let mut rows = rows.clone();
        if let Some((attention, at)) = attention {
            rows.attention_runs = attention;
            rows.attention_read_at_unix_ms = at;
        }
        rows.folded_at_unix_ms = now;
        rows.next_lease_end_unix_ms = store.next_lease_end(now)?;
        rows.leases = leases;
        rows.frontier = frontier;
        refold(store, &mut rows, &missions, now)?;
        rows.sort();
        let valid_until = valid_until(&rows, now);
        Ok(Advance::Published(
            Publication { cut, published_at_unix_ms: now, valid_until_unix_ms: Some(valid_until), rows },
            changed,
        ))
    })
}

/// The first time a card or the list can change with no new claim: a recently ended mission
/// leaving, the attention reread, or a worker lease ending.
fn valid_until(rows: &MissionRows, now: u128) -> u128 {
    let attention = rows.attention_read_at_unix_ms.saturating_add(ATTENTION_REREAD_MS);
    let lease = rows.next_lease_end_unix_ms.filter(|end| *end > now);
    [rows.visible_until(), Some(attention), lease].into_iter().flatten().min().unwrap_or(attention)
}

/// A current missions window from a publication: its first `limit` cards, bounded as a page
/// is, and whether more follow.
pub(super) fn mission_window(rows: &MissionRows, limit: usize) -> anyhow::Result<(Vec<Value>, bool)> {
    let mut has_more = rows.order.len() > limit;
    let mut items = rows
        .order
        .iter()
        .take(limit)
        .filter_map(|id| rows.cards.get(id).map(|card| (**card).clone()))
        .collect::<Vec<_>>();
    anyhow::ensure!(items.len() == rows.order.len().min(limit), "a shown mission has no card");
    has_more |= client_v0::bound_mission_cards(&mut items)?;
    Ok((items, has_more))
}

#[cfg(test)]
#[path = "published_lists_tests.rs"]
mod tests;

/// Fold the current work list at the current cut: from `base` when there is one, refolding only
/// the steps whose claims or leases changed and re-timing the rest, else from nothing in chunks.
/// Its rows are the direct read's at the publication's cut and time; the time is the cut's
/// projection time, never earlier than the previous publication's.
fn fold_work(
    store: &Store,
    base: Option<&Publication<WorkRows>>,
) -> anyhow::Result<Option<(Publication<WorkRows>, bool)>> {
    let advance = match base {
        Some(base) => work_since(store, base)?,
        None => Advance::Rebuild,
    };
    let mut provisional = match advance {
        Advance::Current => return Ok(None),
        Advance::Published(publication, changed) => return Ok(Some((publication, changed))),
        Advance::TooMany(plan) => {
            store.published_work_list().note_rebuild("chunked: work changed");
            plan.refold_in_chunks(store, base.expect("a base had too many changes"))?
        }
        Advance::Rebuild => {
            store.published_work_list().note_rebuild(if base.is_some() { "claims" } else { "start" });
            work_from_nothing(store)?
        }
    };
    for _ in 0..FOLD_ROUNDS {
        match work_since(store, &provisional)? {
            Advance::Current => return Ok(Some((provisional, true))),
            Advance::Published(publication, _) => return Ok(Some((publication, true))),
            Advance::TooMany(plan) => provisional = plan.refold_in_chunks(store, &provisional)?,
            Advance::Rebuild => break,
        }
    }
    anyhow::bail!("the work list kept changing in ways no short fold can follow; keeping the previous one")
}

/// What a work fold must refold to advance its base to `cut` at `time`.
struct WorkPlan {
    cut: u64,
    frontier: u64,
    time: u128,
    steps: BTreeSet<String>,
}

impl WorkPlan {
    /// Refold the planned steps in chunks, each in its own snapshot at or after the plan's cut
    /// and at its own time. Every row then holds at the plan's cut or later.
    fn refold_in_chunks(
        self,
        store: &Store,
        base: &Publication<WorkRows>,
    ) -> anyhow::Result<Publication<WorkRows>> {
        let mut rows = base.rows.clone();
        let steps = self.steps.into_iter().collect::<Vec<_>>();
        for chunk in steps.chunks(FOLD_CHUNK) {
            let chunk = chunk.iter().cloned().collect::<BTreeSet<_>>();
            store.read_snapshot(|cut| {
                let time = store.projection_time_at(cut)?.max(self.time);
                refold_work(store, &mut rows, &chunk, time, cut)
            })?;
        }
        // The rows the chunks did not refold run on to the plan's time too.
        retime_work(&mut rows, self.time);
        // Leases are rechecked from the plan's time, the earliest any row now holds at.
        rows.time_unix_ms = self.time;
        rows.frontier = self.frontier;
        rows.sort();
        Ok(Publication { cut: self.cut, published_at_unix_ms: now_ms(), valid_until_unix_ms: None, rows })
    }
}

/// The first rows: the steps the current list can show at one cut, then their rows in chunks,
/// each at its own later cut and time.
fn work_from_nothing(store: &Store) -> anyhow::Result<Publication<WorkRows>> {
    let (cut, frontier, time, steps) = store.read_snapshot(|cut| {
        Ok((cut, store.projection_frontier()?, store.projection_time_at(cut)?, store.work_list_candidates()?))
    })?;
    let rows = WorkRows { time_unix_ms: time, frontier, ..WorkRows::default() };
    let base = Publication { cut, published_at_unix_ms: now_ms(), valid_until_unix_ms: None, rows };
    WorkPlan { cut, frontier, time, steps }.refold_in_chunks(store, &base)
}

/// Refold the rows of `steps` in the caller's snapshot at `cut`, at `time`.
fn refold_work(
    store: &Store,
    rows: &mut WorkRows,
    steps: &BTreeSet<String>,
    time: u128,
    cut: u64,
) -> anyhow::Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    let views = store.work_at_snapshot_selected(None, false, time, true, true, false, Some(steps))?;
    let subjects = views.iter().map(|view| view.subject.clone()).collect::<Vec<_>>();
    let desired = store.desired_subjects_for_owner_steps(&subjects)?;
    let values = client_work_values(store, views.clone(), &desired, cut)?;
    let leases = store.work_leases()?;
    for step in steps {
        rows.rows.remove(step);
    }
    rows.seats.retain(|_, step| !steps.contains(step));
    for seat in &desired {
        if let Some(step) = &seat.owner_step {
            rows.seats.insert(seat.subject.clone(), step.clone());
        }
    }
    for (view, value) in views.into_iter().zip(values) {
        let timing_lease_unix_ms = if running(&view) {
            store.step_timing_lease(&view.subject, view.attempt, time)?
        } else {
            None
        };
        let next_claim_unix_ms = store.next_claim_after(&view.subject, time)?;
        rows.rows.insert(
            view.subject.clone(),
            Arc::new(WorkRow {
                lease_unix_ms: leases.get(&view.subject).copied(),
                timing_lease_unix_ms,
                next_claim_unix_ms,
                view: Arc::new(view),
                value: Arc::new(value),
                time_unix_ms: time,
            }),
        );
    }
    Ok(())
}

/// Whether a step's row shows execution time that grows with the list's time.
fn running(view: &crate::model::StepRunView) -> bool {
    matches!(view.status.as_str(), "claimed" | "working") && view.execution_started_at_unix_ms.is_some()
}

/// Bring every running step's execution time to `time`: with no new claim about it, a running
/// step's elapsed time grows with the list's time up to the lease its claims named, and passing
/// that lease refolds the step. Says whether any row changed.
fn retime_work(rows: &mut WorkRows, time: u128) -> bool {
    let mut changed = false;
    for row in rows.rows.values_mut() {
        if row.time_unix_ms >= time || !running(&row.view) {
            continue;
        }
        let bound = |at: u128| row.timing_lease_unix_ms.map_or(at, |lease| at.min(lease));
        let grown = bound(time).saturating_sub(bound(row.time_unix_ms));
        let mut view = (*row.view).clone();
        view.execution_elapsed_ms = view.execution_elapsed_ms.saturating_add(grown);
        let mut value = (*row.value).clone();
        value["execution_elapsed_ms"] = json!(view.execution_elapsed_ms);
        changed |= grown > 0;
        *row = Arc::new(WorkRow {
            view: Arc::new(view),
            value: Arc::new(value),
            time_unix_ms: time,
            lease_unix_ms: row.lease_unix_ms,
            timing_lease_unix_ms: row.timing_lease_unix_ms,
            next_claim_unix_ms: row.next_claim_unix_ms,
        });
    }
    changed
}

/// Advance `base` to the current cut in one snapshot, if few enough steps changed. A quiet
/// renewal moves a lease with no claim and no new cut, so leases are compared even then.
fn work_since(store: &Store, base: &Publication<WorkRows>) -> anyhow::Result<Advance<WorkRows, WorkPlan>> {
    store.read_snapshot(|cut| {
        anyhow::ensure!(cut >= base.cut, "the store's index moved back from {} to {cut}", base.cut);
        let rows = &base.rows;
        let frontier = store.projection_frontier()?;
        let Some(from) = claims_from(base.cut, rows.frontier, cut, frontier) else {
            return Ok(Advance::Rebuild);
        };
        let time = store.projection_time_at(cut)?.max(rows.time_unix_ms);
        let changes = store.work_list_changes(from, cut, &rows.seats)?;
        let mut steps = changes.steps;
        // Leases that ended by the new time show their steps ready again.
        steps.extend(store.steps_with_leases_ended(rows.time_unix_ms, time)?);
        // A quiet renewal moves a lease with no claim; a running step whose claims' lease
        // passed stops its execution time.
        let leases = store.work_leases()?;
        // A claim accepted after a row's time, as replication can deliver, enters its
        // time-fenced fields once the list's time passes it.
        let passed = |at: Option<u128>| at.is_some_and(|at| at > rows.time_unix_ms && at <= time);
        steps.extend(rows.rows.iter().filter(|(step, row)| {
            row.lease_unix_ms != leases.get(*step).copied()
                || passed(row.timing_lease_unix_ms)
                || passed(row.next_claim_unix_ms)
        }).map(|(step, _)| step.clone()));
        if cut == base.cut && frontier == rows.frontier && steps.is_empty() && !changes.reorder {
            return Ok(Advance::Current);
        }
        if steps.len() > FOLD_CHUNK {
            return Ok(Advance::TooMany(WorkPlan { cut, frontier, time, steps }));
        }
        let mut rows = rows.clone();
        refold_work(store, &mut rows, &steps, time, cut)?;
        let changed = retime_work(&mut rows, time) || !steps.is_empty() || changes.reorder;
        rows.time_unix_ms = time;
        rows.frontier = frontier;
        rows.sort();
        Ok(Advance::Published(
            Publication { cut, published_at_unix_ms: now_ms(), valid_until_unix_ms: None, rows },
            changed,
        ))
    })
}

/// A current work window from a publication: the first `limit` rows the actor, if any, sees,
/// in the list's order, and whether more follow. An actor's ready work follows its seat
/// queue's order, read at the current cut.
pub(super) fn work_window(
    store: &Store,
    rows: &WorkRows,
    actor: Option<&str>,
    limit: usize,
) -> anyhow::Result<(Vec<Value>, bool)> {
    let mut items = match actor {
        None => rows
            .order
            .iter()
            .take(limit + 1)
            .map(|step| (*rows.rows[step].value).clone())
            .collect::<Vec<_>>(),
        Some(actor) => {
            // As the direct read: rows by the normalized actor, queue order by its seat.
            let viewer = smallclaims::store::normalize_actor(actor, "agent");
            let seat = if actor.contains('/') { actor.to_owned() } else { format!("agent/{actor}") };
            let mut shown = rows
                .order
                .iter()
                .map(|step| &rows.rows[step])
                .filter(|row| {
                    let view = &row.view;
                    !view.agentless
                        && (view.assigned_to.as_deref() == Some(viewer.as_str())
                            || view.claimant.as_deref() == Some(viewer.as_str())
                            || (matches!(view.status.as_str(), "ready" | "blocked")
                                && view.available_to.contains(&viewer)))
                })
                .collect::<Vec<_>>();
            let order = store.seat_run_order(&seat)?;
            let steps = shown
                .iter()
                .map(|row| crate::seat_queue::SeatStep::from(&*row.view))
                .collect::<Vec<_>>();
            let ready = crate::seat_queue::select(&seat, &steps, &order)
                .ready
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let rank = |subject: &str| ready.iter().position(|step| step == subject).unwrap_or(usize::MAX);
            shown.sort_by(|left, right| {
                let (left, right) = (&left.view, &right.view);
                work_state_rank(&left.status)
                    .cmp(&work_state_rank(&right.status))
                    .then_with(|| rank(&left.subject).cmp(&rank(&right.subject)))
                    .then_with(|| left.readiness_epoch.cmp(&right.readiness_epoch))
                    .then_with(|| left.step.cmp(&right.step))
                    .then_with(|| left.subject.cmp(&right.subject))
            });
            shown.into_iter().take(limit + 1).map(|row| (*row.value).clone()).collect()
        }
    };
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok((items, has_more))
}
