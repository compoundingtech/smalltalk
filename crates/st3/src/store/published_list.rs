//! A collection that one background refresher keeps published, as the agents roster is: a read
//! never folds it, it serves the newest publication under that publication's own cut, and the
//! refresher folds at most one change at a time, off the request path.
//!
//! [`PublishedList`] holds the newest publication and the refresher's wake. The refresher loop is
//! `api::published_lists::spawn`; each collection supplies how to fold its rows, from nothing or
//! from the previous publication.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How long a refresh request may go unanswered before reads say the publication is overdue.
const OVERDUE_MS: u64 = 30_000;

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
    /// The collection's name, as windows and logs call it, once a refresher starts.
    name: std::sync::OnceLock<&'static str>,
    newest: Mutex<Option<Arc<Publication<R>>>>,
    wake: Arc<tokio::sync::Notify>,
    running: AtomicBool,
    /// Set when the projections this list folds were replaced without a new claim, as a replay
    /// or a checkpoint trim does: the next refresh folds from nothing.
    forgotten: AtomicBool,
    /// When the oldest refresh request no publication has answered yet was made, in Unix ms;
    /// zero when none waits.
    requested_at: AtomicU64,
    overdue_warned: AtomicBool,
    /// Rises with every publication, so a window that read an earlier one rereads.
    revision: tokio::sync::watch::Sender<u64>,
    /// Folds from nothing, by why: each one refolded every row rather than the changed ones.
    rebuilds: Mutex<BTreeMap<String, u64>>,
}

// Not derived: rows need no default of their own.
impl<R> Default for PublishedList<R> {
    fn default() -> Self {
        Self {
            name: std::sync::OnceLock::new(),
            newest: Mutex::new(None),
            wake: Arc::default(),
            running: AtomicBool::new(false),
            forgotten: AtomicBool::new(false),
            requested_at: AtomicU64::new(0),
            overdue_warned: AtomicBool::new(false),
            revision: tokio::sync::watch::Sender::new(0),
            rebuilds: Mutex::default(),
        }
    }
}

impl<R> PublishedList<R> {
    pub(crate) fn name(&self) -> &'static str {
        self.name.get().copied().unwrap_or("collection")
    }

    /// Register the one refresher of the collection `name`, and return its wake. `None` when
    /// one is already registered.
    pub(crate) fn start(&self, name: &'static str) -> Option<Arc<tokio::sync::Notify>> {
        if self.running.swap(true, Ordering::AcqRel) {
            return None;
        }
        let _ = self.name.set(name);
        Some(Arc::clone(&self.wake))
    }

    /// Whether a refresher keeps this list published, so readers must never fold it.
    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The newest publication, while a refresher keeps one. Says once when a refresh has gone
    /// unanswered so long that the refresher must be slow, failing or stopped; the rows still
    /// carry their own cut and publication time.
    pub(crate) fn newest(&self) -> Option<Arc<Publication<R>>> {
        if !self.running() {
            return None;
        }
        let requested = self.requested_at.load(Ordering::Acquire);
        let waited = (now_ms() as u64).saturating_sub(requested);
        if requested != 0 && waited > OVERDUE_MS && !self.overdue_warned.swap(true, Ordering::AcqRel) {
            eprintln!(
                "st3: WARN a {} refresh asked for {} s ago is still unanswered; serving the newest published rows",
                self.name(),
                waited / 1000
            );
        }
        self.newest.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The newest publication for the refresher itself to fold from, unless the projections
    /// were replaced since: then it folds from nothing.
    pub(crate) fn base(&self) -> Option<Arc<Publication<R>>> {
        if self.forgotten.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.newest.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Swap in a newer publication and answer the requests made before its fold began.
    pub(crate) fn publish(&self, publication: Publication<R>, requested_before: u64) {
        *self.newest.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(publication));
        let _ = self.requested_at.compare_exchange(
            requested_before,
            0,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        self.overdue_warned.store(false, Ordering::Release);
        self.revision.send_modify(|revision| *revision += 1);
    }

    /// When the oldest unanswered request was made, for [`Self::publish`] to clear once the
    /// fold that began after it publishes.
    pub(crate) fn pending_request(&self) -> u64 {
        self.requested_at.load(Ordering::Acquire)
    }

    /// Ask the refresher, if one runs, for a publication at the newest cut. Requests made
    /// while it folds coalesce into one more refresh.
    pub(crate) fn request_refresh(&self) {
        if self.running() {
            let _ = self.requested_at.compare_exchange(
                0,
                now_ms() as u64,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            self.wake.notify_one();
        }
    }

    /// Wake the refresher for a commit. Unlike a read's request, a commit that changes no row
    /// never makes the publication overdue.
    pub(crate) fn notice_commit(&self) {
        self.wake.notify_one();
    }

    /// The projections were replaced without a new claim: fold from nothing, at once.
    pub(crate) fn forget(&self) {
        self.fold_from_nothing_next();
        self.wake.notify_one();
    }

    /// Fold from nothing next time, as after a fold from nothing failed.
    pub(crate) fn fold_from_nothing_next(&self) {
        self.forgotten.store(true, Ordering::Release);
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// Count one fold from nothing, by why.
    pub(crate) fn note_rebuild(&self, why: &str) {
        *self.rebuilds.lock().unwrap_or_else(PoisonError::into_inner).entry(why.to_owned()).or_default() += 1;
    }

    pub(crate) fn rebuilds(&self) -> BTreeMap<String, u64> {
        self.rebuilds.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(cut: u64) -> Publication<Vec<u64>> {
        Publication { cut, published_at_unix_ms: 1, valid_until_unix_ms: None, rows: vec![cut] }
    }

    #[test]
    fn only_a_running_refresher_publishes_and_forgetting_folds_from_nothing_once() {
        let list = PublishedList::<Vec<u64>>::default();
        assert!(list.newest().is_none() && !list.running());
        list.request_refresh();
        assert_eq!(list.pending_request(), 0, "no refresher, no request to answer");
        assert!(list.start("missions").is_some());
        assert!(list.start("missions").is_none(), "one refresher per list");
        assert_eq!(list.name(), "missions");
        let mut revisions = list.subscribe();
        list.request_refresh();
        let asked = list.pending_request();
        assert_ne!(asked, 0);
        list.publish(publication(4), asked);
        assert_eq!(list.pending_request(), 0);
        assert!(revisions.has_changed().unwrap());
        assert_eq!(list.newest().unwrap().cut, 4);
        assert_eq!(list.base().unwrap().rows, vec![4]);
        list.forget();
        assert!(list.base().is_none(), "the next fold starts from nothing");
        assert_eq!(list.base().unwrap().cut, 4, "only once");
        assert_eq!(list.newest().unwrap().cut, 4, "readers keep the newest rows meanwhile");
    }

    #[test]
    fn a_request_made_during_a_fold_stays_pending_after_it_publishes() {
        let list = PublishedList::<Vec<u64>>::default();
        list.start("work");
        let before = list.pending_request();
        list.request_refresh();
        list.publish(publication(2), before);
        assert_ne!(list.pending_request(), 0, "asked after the fold began");
        list.notice_commit();
        list.publish(publication(3), list.pending_request());
        assert_eq!(list.pending_request(), 0);
        list.note_rebuild("start");
        list.note_rebuild("start");
        assert_eq!(list.rebuilds(), BTreeMap::from([("start".to_owned(), 2)]));
    }
}
