//! Which collection views a background refresher keeps published, and how many times each has
//! published. This is the agents roster's contract for every other collection: once a view is
//! published, a collection socket rereads its windows when a newer one is published, never on a
//! raw commit, so a commit that changes nothing in the view folds no window.

use super::*;

/// The collections a refresher may publish. Agents keep their own roster signal.
pub(crate) const PUBLISHED_COLLECTIONS: [&str; 6] =
    ["missions", "work", "attention", "summary", "glasses", "arrangements"];

/// Publication revisions, by position in [`PUBLISHED_COLLECTIONS`]. Zero means no refresher has
/// published that view, so its windows still follow commits.
pub(crate) type Revisions = [u64; PUBLISHED_COLLECTIONS.len()];

pub(crate) struct PublishedViews(tokio::sync::watch::Sender<Revisions>);

impl Default for PublishedViews {
    fn default() -> Self {
        Self(tokio::sync::watch::Sender::new([0; PUBLISHED_COLLECTIONS.len()]))
    }
}

fn position(collection: &str) -> Option<usize> {
    PUBLISHED_COLLECTIONS.iter().position(|name| *name == collection)
}

/// The publication revision of `collection` in `revisions`; zero when none is published.
pub(crate) fn revision(revisions: &Revisions, collection: &str) -> u64 {
    position(collection).map_or(0, |position| revisions[position])
}

impl PublishedViews {
    /// Count one more publication of `collection`'s view, after the refresher has swapped it in.
    pub(crate) fn publish(&self, collection: &str) {
        let Some(position) = position(collection) else {
            debug_assert!(false, "unknown published collection {collection}");
            return;
        };
        self.0.send_modify(|revisions| revisions[position] += 1);
    }

    /// Every published view was discarded, as on rollback or reopen: each window that reads one
    /// rereads it. A view nobody published stays unpublished.
    pub(crate) fn invalidate(&self) {
        self.0.send_if_modified(|revisions| {
            let published = revisions.iter().any(|revision| *revision > 0);
            for revision in revisions.iter_mut().filter(|revision| **revision > 0) {
                *revision += 1;
            }
            published
        });
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<Revisions> {
        self.0.subscribe()
    }

    pub(crate) fn published(&self, collection: &str) -> bool {
        revision(&self.0.borrow(), collection) > 0
    }
}

impl Store {
    /// Say that `collection`'s refresher has swapped in a newer published view. Call it after
    /// every publication, including one at the same graph index after a deadline passed.
    pub(crate) fn publish_collection_view(&self, collection: &str) {
        self.smalltalk.published_views.publish(collection);
    }

    /// Whether a refresher has published `collection`'s view: its windows then reread only when
    /// it publishes again.
    pub(crate) fn collection_view_published(&self, collection: &str) -> bool {
        self.smalltalk.published_views.published(collection)
    }

    /// Follows every collection's publication revision.
    pub(crate) fn subscribe_collection_views(&self) -> tokio::sync::watch::Receiver<Revisions> {
        self.smalltalk.published_views.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publications_count_per_collection_and_invalidation_spares_unpublished_views() {
        let views = PublishedViews::default();
        let mut receiver = views.subscribe();
        assert!(!views.published("work"));
        views.invalidate();
        assert!(!receiver.has_changed().unwrap(), "nothing published, nothing to reread");
        views.publish("work");
        views.publish("work");
        assert!(views.published("work") && !views.published("missions"));
        let revisions = *receiver.borrow_and_update();
        assert_eq!((revision(&revisions, "work"), revision(&revisions, "missions")), (2, 0));
        views.invalidate();
        let revisions = *receiver.borrow_and_update();
        assert_eq!((revision(&revisions, "work"), revision(&revisions, "missions")), (3, 0));
        assert_eq!(revision(&revisions, "agents"), 0, "agents keep the roster signal");
    }
}
