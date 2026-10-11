//! A collection that one background refresher keeps published, as the agents roster is: a read
//! never folds it, it serves the newest publication under that publication's own cut, and the
//! refresher folds at most one change at a time, off the request path.
//!
//! [`PublishedList`] holds the newest publication and the refresher's wake. The refresher loop is
//! `api::published_lists::spawn`; each collection supplies how to fold its rows, from nothing or
//! from the previous publication. Until the first publication, and while the refresher keeps
//! failing or after it stops, the list is not served and readers fold on read.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Rows a refresher published, coherent at one graph cut.
pub(crate) struct Publication<R> {
    /// The graph index the rows were folded at. A reader serves them under this index's
    /// snapshot, never under its own newer one.
    pub(crate) cut: u64,
    /// When the rows were folded, in Unix ms: the collection's "as of".
    pub(crate) published_at_unix_ms: u128,
    /// The first time a row can change with no new claim, as when a lease ends or an ended run
    /// leaves the list. The refresher refolds by then.
    pub(crate) valid_until_unix_ms: Option<u128>,
    pub(crate) rows: R,
}

pub(crate) struct PublishedList<R> {
    newest: Mutex<Option<Arc<Publication<R>>>>,
    wake: Arc<tokio::sync::Notify>,
    /// Whether a refresher was started; there is at most one.
    started: AtomicBool,
    /// Whether readers may serve the newest publication.
    serving: AtomicBool,
    /// Set when the projections this list folds were replaced without a new claim, as a replay
    /// or a checkpoint trim does: the next refresh folds from nothing.
    forgotten: AtomicBool,
    /// Rises with every forget, so a fold that began before one never publishes.
    generation: AtomicU64,
    /// Folds from nothing or in chunks, by why: each refolded many rows, not a few.
    rebuilds: Mutex<BTreeMap<String, u64>>,
}

// Not derived: rows need no default of their own.
impl<R> Default for PublishedList<R> {
    fn default() -> Self {
        Self {
            newest: Mutex::new(None),
            wake: Arc::default(),
            started: AtomicBool::new(false),
            serving: AtomicBool::new(false),
            forgotten: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            rebuilds: Mutex::default(),
        }
    }
}

impl<R> PublishedList<R> {
    /// Register the one refresher, and return its wake. `None` when one is already registered.
    pub(crate) fn start(&self) -> Option<Arc<tokio::sync::Notify>> {
        (!self.started.swap(true, Ordering::AcqRel)).then(|| Arc::clone(&self.wake))
    }

    /// The newest publication, while the refresher keeps the list served. Its rows carry their
    /// own cut and publication time.
    pub(crate) fn newest(&self) -> Option<Arc<Publication<R>>> {
        if !self.serving.load(Ordering::Acquire) {
            return None;
        }
        self.newest.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The newest publication for the refresher itself to fold from, served or not, unless the
    /// projections were replaced since: then it folds from nothing. With it, the generation
    /// [`Self::publish`] must still see for the fold to publish.
    pub(crate) fn base(&self) -> (Option<Arc<Publication<R>>>, u64) {
        // Under the lock a forget takes, so the flag and the generation are read together.
        let newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
        let generation = self.generation.load(Ordering::Acquire);
        if self.forgotten.swap(false, Ordering::AcqRel) {
            return (None, generation);
        }
        (newest.clone(), generation)
    }

    /// Serve the newest publication again, as a withdrawn list whose next fold finds nothing
    /// new must, unless the projections were replaced since `generation`. Says whether it did.
    pub(crate) fn serve_again(&self, generation: u64) -> bool {
        let newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
        if newest.is_none() || self.generation.load(Ordering::Acquire) != generation {
            return false;
        }
        !self.serving.swap(true, Ordering::AcqRel)
    }

    /// Swap in a newer publication and serve it, unless the projections were replaced after its
    /// fold began (`generation` is stale): then it is dropped and `None` returned, and the next
    /// fold starts from nothing. Otherwise says whether the list was served before, so a list
    /// served again is announced to its windows.
    pub(crate) fn publish(&self, publication: Publication<R>, generation: u64) -> Option<bool> {
        let mut newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
        if self.generation.load(Ordering::Acquire) != generation {
            return None;
        }
        *newest = Some(Arc::new(publication));
        Some(self.serving.swap(true, Ordering::AcqRel))
    }

    /// Stop serving the list, as when its refresher keeps failing or stops: readers fold on
    /// read until a fold publishes again. Says whether it was served.
    pub(crate) fn withdraw(&self) -> bool {
        self.serving.swap(false, Ordering::AcqRel)
    }

    /// The projections were replaced without a new claim: fold from nothing at once, and drop
    /// any fold already under way. Readers keep the newest rows, under their own cut, until the
    /// fold from nothing publishes; replays come in bursts, and folding every window on read
    /// meanwhile would cost more than rows a fold or two old.
    pub(crate) fn forget(&self) {
        let _newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.fold_from_nothing_next();
        self.wake.notify_one();
    }

    /// Fold from nothing next time, as after a fold from nothing failed.
    pub(crate) fn fold_from_nothing_next(&self) {
        self.forgotten.store(true, Ordering::Release);
    }

    /// Count one fold of many rows, by why.
    pub(crate) fn note_rebuild(&self, why: &str) {
        *self.rebuilds.lock().unwrap_or_else(PoisonError::into_inner).entry(why.to_owned()).or_default() += 1;
    }

    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> BTreeMap<String, u64> {
        self.rebuilds.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The claims after `?1` through `?2`, by store index, with the step a person-work claim names.
pub(crate) const CLAIMS_SINCE: &str = "SELECT subject, kind, CASE WHEN kind LIKE 'work.person-%'
        THEN json_extract(body,'$.fields.origin_step') END
 FROM claims WHERE store_index>?1 AND store_index<=?2";
/// The person asks the requesters in `?1`, a JSON array, made, by the kind index.
pub(crate) const ASKS_BY_REQUESTERS: &str = "SELECT subject, json_extract(body,'$.fields.origin_step') FROM claims
 WHERE kind='work.person-asked' AND actor IN (SELECT value FROM json_each(?1))";
/// The ask on step `?1`, by the subject index.
pub(crate) const ASK_ORIGIN_AND_RUN: &str = "SELECT json_extract(body,'$.fields.origin_step'), json_extract(body,'$.fields.run')
 FROM claims WHERE subject=?1 AND kind='work.person-asked' LIMIT 1";
/// A run's steps, by the run index.
pub(crate) const RUN_STEPS: &str = "SELECT subject FROM step_runs INDEXED BY step_runs_run_index WHERE run_id=?1";
/// The runs a step started, by the parent-step index.
pub(crate) const CHILD_RUNS: &str =
    "SELECT id FROM mission_runs WHERE parent_step_run=?1 AND parent_step_run IS NOT NULL";
/// Person asks whose origin step is one of `?1`, a JSON array, by the kind index.
pub(crate) const ASKS_FROM_STEPS: &str = "SELECT subject FROM claims
 WHERE kind='work.person-asked' AND json_extract(body,'$.fields.origin_step') IN (SELECT value FROM json_each(?1))";

/// Claim kinds about an agent that decide whether its person asks still block their steps:
/// its declaration, and the runtime state and actions that tell whether it is retiring.
pub(crate) fn decides_asks(kind: &str) -> bool {
    kind == "intent.desired" || kind == "runtime.observed" || kind.starts_with("runtime.action.")
}

impl Store {
    /// How far replicated claims are projected: claims after this index that arrived by
    /// replication may still wait in the backlog, and a pass that projects them moves it on. A
    /// list that folded past them before then reads their claims again once it moves.
    pub(crate) fn projection_frontier(&self) -> Result<u64> {
        let connection = self.readers.get();
        let frontier = connection
            .prepare_cached(
                "SELECT COALESCE(MAX(last_good_store_index), 0) FROM projection_health
                 WHERE aggregate='graph'",
            )?
            .query_row([], |row| row.get::<_, u64>(0))?;
        Ok(frontier)
    }

    /// The person asks that `requesters` made, with the step each asked from, in one read for a
    /// whole fold. Person asks are few; the kind index bounds the read to them.
    pub(crate) fn asks_by_requesters(
        &self,
        requesters: &BTreeSet<String>,
    ) -> Result<Vec<(String, Option<String>)>> {
        if requesters.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let asks = connection
            .prepare_cached(ASKS_BY_REQUESTERS)?
            .query_map([serde_json::to_string(requesters)?], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(asks)
    }

    /// For a claim about a person ask's step: the step that asked and the run the ask started,
    /// as the ask itself names them. Answers and cancellations name neither.
    pub(crate) fn ask_origin_and_run(&self, ask: &str) -> Result<(Option<String>, Option<String>)> {
        let connection = self.readers.get();
        let found = connection
            .prepare_cached(ASK_ORIGIN_AND_RUN)?
            .query_row([ask], |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)))
            .optional()?;
        Ok(found.unwrap_or_default())
    }

    /// Every person ask, with the step it asked from: what a change to a subscription or
    /// schedule, whose deliveries an ask's currency walks, can affect. Person asks are few.
    pub(crate) fn every_ask(&self) -> Result<Vec<(String, Option<String>)>> {
        let connection = self.readers.get();
        let asks = connection
            .prepare_cached(
                "SELECT subject, json_extract(body,'$.fields.origin_step') FROM claims
                 WHERE kind='work.person-asked'",
            )?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(asks)
    }

    /// `runs` and every run under them, through the steps that started each child run: a
    /// run's state ends the steps of the runs under it, and decides whether asks in them are
    /// current. Walks the run and parent-step indexes, never the whole table.
    pub(crate) fn runs_under(&self, runs: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut steps_of = connection.prepare_cached(RUN_STEPS)?;
        let mut children_of = connection.prepare_cached(CHILD_RUNS)?;
        let mut found = BTreeSet::new();
        let mut queue = runs.iter().cloned().collect::<Vec<_>>();
        while let Some(run) = queue.pop() {
            if !found.insert(run.clone()) {
                continue;
            }
            let steps = steps_of
                .query_map([&run], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for step in steps {
                for child in children_of.query_map([&step], |row| row.get::<_, String>(0))? {
                    queue.push(child?);
                }
            }
        }
        Ok(found)
    }

    /// The person asks that asked from any of `steps`: a change to the step that asked, as
    /// when it leaves waiting on a person, decides whether its asks are still current.
    pub(crate) fn asks_from_steps(&self, steps: &BTreeSet<String>) -> Result<Vec<String>> {
        if steps.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let asks = connection
            .prepare_cached(ASKS_FROM_STEPS)?
            .query_map([serde_json::to_string(steps)?], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(asks)
    }

    /// The run a revision proposal is for: its phase and timestamps change with the proposal.
    pub(crate) fn revision_proposal_run(&self, proposal: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        let run = connection
            .prepare_cached("SELECT run_id FROM revision_proposals WHERE id=?1")?
            .query_row([proposal], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(cut: u64) -> Publication<Vec<u64>> {
        Publication { cut, published_at_unix_ms: 1, valid_until_unix_ms: None, rows: vec![cut] }
    }

    #[test]
    fn one_refresher_serves_its_publications_and_forgetting_folds_from_nothing_once() {
        let list = PublishedList::<Vec<u64>>::default();
        assert!(list.newest().is_none() && list.base().0.is_none());
        assert!(list.start().is_some());
        assert!(list.start().is_none(), "one refresher per list");
        assert!(list.newest().is_none(), "nothing served before the first publication");
        let (_, generation) = list.base();
        assert_eq!(list.publish(publication(4), generation), Some(false), "served for the first time");
        assert_eq!(list.newest().unwrap().cut, 4);
        let (base, generation) = list.base();
        assert_eq!(base.unwrap().rows, vec![4]);
        list.forget();
        assert_eq!(list.newest().unwrap().cut, 4, "readers keep the newest rows meanwhile");
        // A fold that began before the forget never publishes its rows.
        assert_eq!(list.publish(publication(5), generation), None);
        assert_eq!(list.newest().unwrap().cut, 4);
        let (base, generation) = list.base();
        assert!(base.is_none(), "the next fold starts from nothing");
        assert_eq!(list.base().0.unwrap().cut, 4, "only once");
        assert_eq!(list.publish(publication(6), generation), Some(true));
        assert_eq!(list.newest().unwrap().cut, 6);
    }

    #[test]
    fn a_withdrawn_list_is_not_served_until_it_publishes_again() {
        let list = PublishedList::<Vec<u64>>::default();
        list.start();
        list.publish(publication(4), 0);
        assert!(list.withdraw());
        assert!(!list.withdraw(), "already withdrawn");
        assert!(list.newest().is_none(), "readers fold on read");
        assert_eq!(list.base().0.unwrap().cut, 4, "the refresher still folds from it");
        assert_eq!(list.publish(publication(5), 0), Some(false), "served again, so announced");
        assert_eq!(list.publish(publication(6), 0), Some(true));
        assert_eq!(list.newest().unwrap().cut, 6);
        // A fold that finds nothing new serves the withdrawn rows again, once.
        list.withdraw();
        assert!(list.serve_again(0));
        assert!(!list.serve_again(0), "already served");
        assert_eq!(list.newest().unwrap().cut, 6);
        list.withdraw();
        list.forget();
        assert!(!list.serve_again(0), "not after the projections were replaced");
        list.note_rebuild("start");
        list.note_rebuild("start");
        assert_eq!(list.rebuilds(), BTreeMap::from([("start".to_owned(), 2)]));
    }
}
