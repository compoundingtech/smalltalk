//! The summary rows a daemon publishes for collection windows: one `summary/current` row per
//! selected person (or every person's, for an agent that selects none), computed off the request
//! path by the attention list's refresher and served under the cut it was computed at. A window
//! names the person it selects; the refresher computes the rows windows read recently. Volatile
//! cache state, cleared by `forget_views`.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// The rows of one summary refresh and the cut they were computed at.
pub(crate) struct SummaryPublication {
    pub(crate) cut: u64,
    pub(crate) published_at_unix_ms: u128,
    /// Each selection's row: `None` is every person's.
    pub(crate) rows: HashMap<Option<String>, Value>,
}

#[derive(Default)]
pub(crate) struct SummaryList {
    /// Each selection a window read, and when it last did, in Unix ms.
    selections: Mutex<HashMap<Option<String>, u64>>,
    published: Mutex<Option<Arc<SummaryPublication>>>,
    forgotten: AtomicU64,
}

impl SummaryList {
    pub(super) fn forget(&self) {
        let mut published = self.published.lock().unwrap_or_else(PoisonError::into_inner);
        self.forgotten.fetch_add(1, AtomicOrdering::AcqRel);
        *published = None;
    }
}

/// At most this many selections are kept; a busy fleet has a handful of people.
const SELECTIONS: usize = 64;

impl Store {
    /// The newest row published for `person`, with its cut and publication time, and note that
    /// a window reads that selection. `None` until the refresher has computed it: the caller
    /// wakes the refresher. Only while the attention list's refresher runs.
    pub(crate) fn published_summary(&self, person: Option<&str>) -> Option<(u64, u128, Value)> {
        self.attention_list_refresher_running().then_some(())?;
        self.note_published_view_read();
        let list = &self.smalltalk.summary_list;
        {
            let mut selections = list.selections.lock().unwrap_or_else(PoisonError::into_inner);
            let key = person.map(str::to_owned);
            if selections.contains_key(&key) || selections.len() < SELECTIONS {
                selections.insert(key, now_ms() as u64);
            }
        }
        let published = list.published.lock().unwrap_or_else(PoisonError::into_inner).clone()?;
        let row = published.rows.get(&person.map(str::to_owned))?.clone();
        Some((published.cut, published.published_at_unix_ms, row))
    }

    /// The selections windows read within `within_ms`, forgetting older ones, and the
    /// generation to publish under.
    pub(crate) fn summary_selections(&self, within_ms: u64) -> (u64, Vec<Option<String>>) {
        let list = &self.smalltalk.summary_list;
        let now = now_ms() as u64;
        let mut selections = list.selections.lock().unwrap_or_else(PoisonError::into_inner);
        selections.retain(|_, read_at| now.saturating_sub(*read_at) <= within_ms);
        let mut keys = selections.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        (list.forgotten.load(AtomicOrdering::Acquire), keys)
    }

    /// Swap in the rows of one refresh. Returns whether any selection's counts changed: only
    /// then do windows have anything to send.
    pub(crate) fn publish_summary(&self, generation: u64, publication: SummaryPublication) -> bool {
        let list = &self.smalltalk.summary_list;
        let mut published = list.published.lock().unwrap_or_else(PoisonError::into_inner);
        if list.forgotten.load(AtomicOrdering::Acquire) != generation
            || published.as_ref().is_some_and(|newest| newest.cut > publication.cut)
        {
            return false;
        }
        let changed = published.as_ref().is_none_or(|previous| {
            publication.rows.iter().any(|(key, row)| {
                previous.rows.get(key).is_none_or(|before| before["revision"] != row["revision"])
            })
        });
        *published = Some(Arc::new(publication));
        changed
    }
}
