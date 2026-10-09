//! The summary rows a daemon publishes for collection windows: one `summary/current` row per
//! selected person (or every person's, for an agent that selects none), computed off the request
//! path by the attention list's refresher and served under the cut it was computed at. A window
//! names the person it selects; the refresher computes the rows windows read recently, each in
//! its own short snapshot. A selection the refresher cannot serve (more selections than it
//! keeps, or one whose computation fails) is not published, and its windows read for
//! themselves. Volatile cache state, cleared by `forget_views`.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// At most this many selections are published; a busy fleet has a handful of people. Windows of
/// any further selection read for themselves.
const SELECTIONS: usize = 64;

/// One selection's published row and what it was computed from.
#[derive(Clone)]
pub(crate) struct SummaryRow {
    pub(crate) cut: u64,
    pub(crate) published_at_unix_ms: u128,
    pub(crate) row: Value,
    /// The inputs the row was computed from. Equal inputs compute nothing.
    pub(crate) inputs: SummaryInputs,
}

/// What a summary row was computed from: the graph cut, the attention rows' revision, the
/// roster revision, and the clock period it was evaluated in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SummaryInputs {
    pub(crate) cut: u64,
    pub(crate) attention: u64,
    pub(crate) roster: u64,
    pub(crate) period: u128,
}

#[derive(Default)]
struct Selection {
    /// When a window last read it, in Unix ms.
    read_at: u64,
    row: Option<SummaryRow>,
    /// Its last computation failed: its windows read for themselves until one succeeds.
    failed: bool,
}

#[derive(Default)]
pub(crate) struct SummaryList {
    selections: Mutex<HashMap<Option<String>, Selection>>,
    forgotten: AtomicU64,
}

impl SummaryList {
    pub(super) fn forget(&self) {
        let mut selections = self.selections.lock().unwrap_or_else(PoisonError::into_inner);
        self.forgotten.fetch_add(1, AtomicOrdering::AcqRel);
        for selection in selections.values_mut() {
            selection.row = None;
        }
    }
}

/// What a summary window gets for its selection.
pub(crate) enum PublishedSummary {
    /// The published row, with its cut and publication time.
    Row(u64, u128, Value),
    /// Not computed yet: retry shortly; the refresher has been asked.
    Pending,
    /// The refresher does not serve this selection: the window reads it for itself.
    Unserved,
}

impl Store {
    /// The row published for `person`, and note that a window reads that selection. Only while
    /// the attention list's refresher runs.
    pub(crate) fn published_summary(&self, person: Option<&str>) -> PublishedSummary {
        if !self.attention_list_refresher_running() {
            return PublishedSummary::Unserved;
        }
        self.note_published_view_read();
        let list = &self.smalltalk.summary_list;
        let mut selections = list.selections.lock().unwrap_or_else(PoisonError::into_inner);
        let key = person.map(str::to_owned);
        if !selections.contains_key(&key) && selections.len() >= SELECTIONS {
            return PublishedSummary::Unserved;
        }
        let selection = selections.entry(key).or_default();
        selection.read_at = now_ms() as u64;
        match (&selection.row, selection.failed) {
            (_, true) => PublishedSummary::Unserved,
            (Some(row), false) => {
                PublishedSummary::Row(row.cut, row.published_at_unix_ms, row.row.clone())
            }
            (None, false) => PublishedSummary::Pending,
        }
    }

    /// The selections windows read within `within_ms`, forgetting older ones, each with the
    /// inputs its row was computed from, and the generation to publish under.
    pub(crate) fn summary_selections(
        &self,
        within_ms: u64,
    ) -> (u64, Vec<(Option<String>, Option<SummaryInputs>)>) {
        let list = &self.smalltalk.summary_list;
        let now = now_ms() as u64;
        let mut selections = list.selections.lock().unwrap_or_else(PoisonError::into_inner);
        selections.retain(|_, selection| now.saturating_sub(selection.read_at) <= within_ms);
        let mut keys = selections
            .iter()
            .map(|(key, selection)| {
                (key.clone(), selection.row.as_ref().map(|row| row.inputs.clone()))
            })
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| left.0.cmp(&right.0));
        (list.forgotten.load(AtomicOrdering::Acquire), keys)
    }

    /// Whether a window waits for a selection the refresher has not computed yet.
    pub(crate) fn summary_selection_waiting(&self) -> bool {
        self.smalltalk
            .summary_list
            .selections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|selection| selection.row.is_none() && !selection.failed)
    }

    /// Store one selection's computation: its row, or `None` for a failure. Returns whether
    /// what its windows show changed: only then do they have anything to reread. A computation
    /// that began before views were forgotten (`generation` is stale) stores nothing.
    pub(crate) fn publish_summary_row(
        &self,
        generation: u64,
        person: &Option<String>,
        row: Option<SummaryRow>,
    ) -> bool {
        let list = &self.smalltalk.summary_list;
        let mut selections = list.selections.lock().unwrap_or_else(PoisonError::into_inner);
        if list.forgotten.load(AtomicOrdering::Acquire) != generation {
            return false;
        }
        let Some(selection) = selections.get_mut(person) else {
            return false;
        };
        let Some(row) = row else {
            let changed = !selection.failed;
            selection.failed = true;
            selection.row = None;
            return changed;
        };
        if selection.row.as_ref().is_some_and(|newest| newest.cut > row.cut) {
            return false;
        }
        let changed = selection.failed
            || selection
                .row
                .as_ref()
                .is_none_or(|before| before.row["revision"] != row.row["revision"]);
        selection.failed = false;
        selection.row = Some(row);
        changed
    }
}
