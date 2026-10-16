//! Which collection views a background refresher keeps published, and how many times each has
//! published. This is the agents roster's contract for every other collection: once a view is
//! published, a collection socket rereads its windows when a newer one is published, never on a
//! raw commit, so a commit that changes nothing in the view folds no window.

use super::*;

/// The collections a refresher may publish. Agents keep their own roster signal.
pub(crate) const PUBLISHED_COLLECTIONS: [&str; 6] =
    ["missions", "work", "attention", "summary", "glasses", "arrangements"];

/// One collection's publication state.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct View {
    /// Rises with every publication, withdrawal and invalidation, and never goes back, so a
    /// window can tell any later change from the one it read.
    revision: u64,
    /// Whether a refresher serves or governs this view now. Until it does, and after it
    /// withdraws, the collection's windows follow commits. A held view is governed even before
    /// its first publication and while it has nothing current: its windows wait for its next
    /// change instead (see [`PublishedViews::hold`]).
    published: bool,
}

/// By position in [`PUBLISHED_COLLECTIONS`].
pub(crate) type Revisions = [View; PUBLISHED_COLLECTIONS.len()];

pub(crate) struct PublishedViews(tokio::sync::watch::Sender<Revisions>);

impl Default for PublishedViews {
    fn default() -> Self {
        Self(tokio::sync::watch::Sender::new([View::default(); PUBLISHED_COLLECTIONS.len()]))
    }
}

fn position(collection: &str) -> Option<usize> {
    PUBLISHED_COLLECTIONS.iter().position(|name| *name == collection)
}

/// The revision of `collection` in `revisions`; zero when it was never published.
pub(crate) fn revision(revisions: &Revisions, collection: &str) -> u64 {
    position(collection).map_or(0, |position| revisions[position].revision)
}

impl PublishedViews {
    fn change(&self, collection: &str, published: bool) {
        let Some(position) = position(collection) else {
            debug_assert!(false, "unknown published collection {collection}");
            return;
        };
        self.0.send_modify(|revisions| {
            revisions[position].revision += 1;
            revisions[position].published = published;
        });
    }

    /// Count one more publication of `collection`'s view, after the refresher has swapped it in.
    #[cfg_attr(not(test), allow(dead_code, reason = "the missions, work and attention refreshers call it"))]
    pub(crate) fn publish(&self, collection: &str) {
        self.change(collection, true);
    }

    /// `collection`'s refresher no longer serves its view: its windows read it once more the
    /// unpublished way, then follow commits again until the next publication. A refresher that
    /// governs its view holds it instead while it runs (see [`PublishedViews::hold`]); it is
    /// withdrawn only when that refresher ends.
    #[cfg_attr(not(test), allow(dead_code, reason = "the missions, work and attention refreshers call it"))]
    pub(crate) fn withdraw(&self, collection: &str) {
        self.change(collection, false);
    }

    /// `collection`'s running refresher governs its view though it has nothing to serve yet, or
    /// no longer: before its first publication, or after it withdrew rows it could not keep
    /// current. Its windows read once more, then wait for the view's next change instead of
    /// following commits, which would read nothing new.
    #[cfg_attr(not(test), allow(dead_code, reason = "the work refresher calls it"))]
    pub(crate) fn hold(&self, collection: &str) {
        self.change(collection, true);
    }

    /// Every published view was discarded, as on rollback or reopen: each window that reads one
    /// rereads it. A view nobody publishes stays as it is.
    pub(crate) fn invalidate(&self) {
        self.0.send_if_modified(|revisions| {
            let published = revisions.iter().any(|view| view.published);
            for view in revisions.iter_mut().filter(|view| view.published) {
                view.revision += 1;
            }
            published
        });
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<Revisions> {
        self.0.subscribe()
    }

    pub(crate) fn published(&self, collection: &str) -> bool {
        position(collection).is_some_and(|position| self.0.borrow()[position].published)
    }
}

impl Store {
    /// Say that `collection`'s refresher has swapped in a newer published view. Call it after
    /// every publication, including one at the same graph index after a deadline passed.
    #[cfg_attr(not(test), allow(dead_code, reason = "the missions, work and attention refreshers call it"))]
    pub(crate) fn publish_collection_view(&self, collection: &str) {
        self.smalltalk.published_views.publish(collection);
    }

    /// Say that `collection`'s refresher stopped serving its view, because it exited or keeps
    /// failing: its windows go back to following commits until it publishes again.
    #[cfg_attr(not(test), allow(dead_code, reason = "the missions, work and attention refreshers call it"))]
    pub(crate) fn withdraw_collection_view(&self, collection: &str) {
        self.smalltalk.published_views.withdraw(collection);
    }

    /// Say that `collection`'s running refresher governs its view while it has nothing to serve:
    /// see [`PublishedViews::hold`].
    pub(crate) fn hold_collection_view(&self, collection: &str) {
        self.smalltalk.published_views.hold(collection);
    }

    /// Whether a refresher serves or governs `collection`'s view: its windows then reread only
    /// when the view changes again.
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
        // A withdrawal is a change every window sees, and the view follows commits again.
        views.withdraw("work");
        assert!(!views.published("work"));
        assert_eq!(revision(&receiver.borrow_and_update(), "work"), 4);
        views.invalidate();
        assert!(!receiver.has_changed().unwrap(), "a withdrawn view has nothing to reread");
        views.publish("work");
        assert!(views.published("work"));
        assert_eq!(revision(&receiver.borrow_and_update(), "work"), 5, "revisions never go back");
        // A held view is a change every window sees once; it then follows the view, not commits.
        views.hold("work");
        assert!(views.published("work"));
        assert_eq!(revision(&receiver.borrow_and_update(), "work"), 6);
    }
}
