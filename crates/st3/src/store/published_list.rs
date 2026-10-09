//! A collection that one background refresher keeps published, as the agents roster is: a read
//! never folds it, it serves the newest publication under that publication's own cut, and the
//! refresher folds at most one change at a time, off the request path.
//!
//! [`PublishedList`] holds the newest publication and the refresher's wake. The refresher loop is
//! `api::published_lists::spawn`; each collection supplies how to fold its rows, from nothing or
//! from the previous publication. Until the first publication, and while the refresher keeps
//! failing or after it stops, the list is not served and readers fold on read.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// projections were replaced since: then it folds from nothing.
    pub(crate) fn base(&self) -> Option<Arc<Publication<R>>> {
        if self.forgotten.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.newest.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Swap in a newer publication and serve it. Says whether the list was served before, so a
    /// list served again is announced to its windows.
    pub(crate) fn publish(&self, publication: Publication<R>) -> bool {
        *self.newest.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(publication));
        self.serving.swap(true, Ordering::AcqRel)
    }

    /// Stop serving the list, as when its refresher keeps failing or stops: readers fold on
    /// read until a fold publishes again. Says whether it was served.
    pub(crate) fn withdraw(&self) -> bool {
        self.serving.swap(false, Ordering::AcqRel)
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

    /// Count one fold of many rows, by why.
    pub(crate) fn note_rebuild(&self, why: &str) {
        *self.rebuilds.lock().unwrap_or_else(PoisonError::into_inner).entry(why.to_owned()).or_default() += 1;
    }

    #[cfg(test)]
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
    fn one_refresher_serves_its_publications_and_forgetting_folds_from_nothing_once() {
        let list = PublishedList::<Vec<u64>>::default();
        assert!(list.newest().is_none() && list.base().is_none());
        assert!(list.start().is_some());
        assert!(list.start().is_none(), "one refresher per list");
        assert!(list.newest().is_none(), "nothing served before the first publication");
        assert!(!list.publish(publication(4)), "served for the first time");
        assert_eq!(list.newest().unwrap().cut, 4);
        assert_eq!(list.base().unwrap().rows, vec![4]);
        list.forget();
        assert!(list.base().is_none(), "the next fold starts from nothing");
        assert_eq!(list.base().unwrap().cut, 4, "only once");
        assert_eq!(list.newest().unwrap().cut, 4, "readers keep the newest rows meanwhile");
    }

    #[test]
    fn a_withdrawn_list_is_not_served_until_it_publishes_again() {
        let list = PublishedList::<Vec<u64>>::default();
        list.start();
        list.publish(publication(4));
        assert!(list.withdraw());
        assert!(!list.withdraw(), "already withdrawn");
        assert!(list.newest().is_none(), "readers fold on read");
        assert_eq!(list.base().unwrap().cut, 4, "the refresher still folds from it");
        assert!(!list.publish(publication(5)), "served again, so announced");
        assert!(list.publish(publication(6)));
        assert_eq!(list.newest().unwrap().cut, 6);
        list.note_rebuild("start");
        list.note_rebuild("start");
        assert_eq!(list.rebuilds(), BTreeMap::from([("start".to_owned(), 2)]));
    }
}
