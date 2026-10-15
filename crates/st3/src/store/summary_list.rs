//! The summary rows a daemon publishes for collection windows: one `summary/current` row per
//! selected person (or every person's, for an agent that selects none), assembled off the request
//! path by the attention list's refresher and served under the cut it was computed at. What no
//! person changes (missions, working agents, machines) is computed once, in one short snapshot,
//! for every selection; each selection then counts what needs its person from the published
//! attention rows, the same rows its attention window shows. A window names the person it
//! selects. A selection the refresher cannot serve (more selections than it keeps, or one whose
//! shared counts cannot be computed) is not published, and its windows read for themselves.
//! The machines the shared counts count are the costliest part to read and few claims change
//! them, so they are read again only when one may have (`summary_machines_hold`).
//! Volatile cache state, cleared by `forget_views`.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

/// At most this many selections are published; a busy fleet has a handful of people. Windows of
/// any further selection read for themselves.
const SELECTIONS: usize = 64;

/// More claims than this since the machines were read reads them again rather than every claim.
const MACHINES_DELTA_LIMIT: usize = 10_000;

/// More runtimes than this with claims since the machines were read reads them again rather
/// than reducing each of them.
const MACHINES_RUNTIMES_LIMIT: usize = 64;

/// One selection's published row.
#[derive(Clone)]
pub(crate) struct SummaryRow {
    pub(crate) cut: u64,
    pub(crate) published_at_unix_ms: u128,
    pub(crate) row: Value,
}

/// What every selection's summary shares: each mission (`id`, `system`, `active`), the working
/// agents and the machine counts, computed at `cut` and the clock `evaluated_at_unix_ms`, with
/// the roster publication the agents were counted from.
#[derive(Clone, Debug)]
pub(crate) struct SummaryShared {
    pub(crate) cut: u64,
    pub(crate) evaluated_at_unix_ms: u128,
    pub(crate) published_at_unix_ms: u128,
    pub(crate) missions: Vec<(String, bool, bool)>,
    pub(crate) working_agents: usize,
    pub(crate) machines: Value,
    pub(crate) agents_as_of: Option<Value>,
}

/// What the shared counts were computed from: the graph cut, the roster revision and the clock
/// period they were evaluated in. Equal inputs compute nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SummaryInputs {
    pub(crate) cut: u64,
    pub(crate) roster: u64,
    pub(crate) period: u128,
}

/// The machines the shared counts were counted from and what they were read under: the graph
/// cut, the clock period, the count of conflicting operations and the fleet's membership. Kept
/// past their cut, the machines' hosts, states and transports still hold; their runtimes, work
/// and update times may not, and the counts read none of those.
#[derive(Clone)]
pub(crate) struct SummaryMachines {
    pub(crate) cut: u64,
    pub(crate) period: u128,
    pub(crate) conflicts: u64,
    pub(crate) fleet: smallclaims::fleet::FleetView,
    pub(crate) machines: Arc<Vec<Value>>,
    /// The host each current runtime runs on, by runtime subject.
    pub(crate) runtimes: Arc<BTreeMap<String, String>>,
}

#[derive(Default)]
struct Selection {
    /// When a window last read it, in Unix ms.
    read_at: u64,
    row: Option<SummaryRow>,
    /// The shared counts could not be computed: its windows read for themselves until they are.
    failed: bool,
}

#[derive(Default)]
struct Lists {
    selections: HashMap<Option<String>, Selection>,
    shared: Option<(Arc<SummaryShared>, SummaryInputs)>,
    machines: Option<SummaryMachines>,
}

#[derive(Default)]
pub(crate) struct SummaryList {
    lists: Mutex<Lists>,
    forgotten: AtomicU64,
    /// The shared counts were computed once since the store opened.
    started: AtomicBool,
    /// The newest attempt to compute the shared counts failed.
    failing: AtomicBool,
}

impl SummaryList {
    pub(super) fn forget(&self) {
        let mut lists = self.lists.lock().unwrap_or_else(PoisonError::into_inner);
        self.forgotten.fetch_add(1, AtomicOrdering::AcqRel);
        lists.shared = None;
        lists.machines = None;
        for selection in lists.selections.values_mut() {
            selection.row = None;
        }
    }
}

/// What a summary window gets for its selection.
pub(crate) enum PublishedSummary {
    /// The published row, with its cut and publication time.
    Row(u64, u128, Value),
    /// No row published yet: the window counts its own from the published shared counts and
    /// attention rows, else retries shortly; the refresher has been asked.
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
        let mut lists = list.lists.lock().unwrap_or_else(PoisonError::into_inner);
        let selections = &mut lists.selections;
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

    /// The selections windows read within `within_ms`, forgetting older ones, and the generation
    /// to publish under. With `waiting_only`, only the selections a window waits for (see
    /// `summary_selection_waiting`).
    pub(crate) fn summary_selections(
        &self,
        within_ms: u64,
        waiting_only: bool,
    ) -> (u64, Vec<Option<String>>) {
        let list = &self.smalltalk.summary_list;
        let now = now_ms() as u64;
        let mut lists = list.lists.lock().unwrap_or_else(PoisonError::into_inner);
        lists.selections.retain(|_, selection| now.saturating_sub(selection.read_at) <= within_ms);
        let mut keys = lists
            .selections
            .iter()
            .filter(|(_, selection)| {
                !waiting_only || selection.row.is_none() && !selection.failed
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        keys.sort();
        (list.forgotten.load(AtomicOrdering::Acquire), keys)
    }

    /// The newest shared counts and the inputs they were computed from, if any.
    pub(crate) fn summary_shared(&self) -> Option<(Arc<SummaryShared>, SummaryInputs)> {
        self.smalltalk
            .summary_list
            .lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shared
            .clone()
    }

    /// Mark the shared counts computed once since the store opened. Until they are (the
    /// complete roster they count agents from may not be published yet), the refresher keeps
    /// computing them with no window asking, so the first window is served at once.
    pub(crate) fn start_summary_shared(&self) {
        self.smalltalk.summary_list.started.store(true, AtomicOrdering::Release);
    }

    /// Note whether an attempt to compute the shared counts failed. Returns whether this begins
    /// a run of failures: the refresher retries every pass, and says so once per run.
    pub(crate) fn note_summary_shared_failed(&self, failed: bool) -> bool {
        self.smalltalk.summary_list.failing.swap(failed, AtomicOrdering::AcqRel) != failed && failed
    }

    /// Whether the shared counts are still to be computed as the daemon starts.
    pub(crate) fn summary_shared_starting(&self) -> bool {
        !self.smalltalk.summary_list.started.load(AtomicOrdering::Acquire)
    }

    /// Store newly computed shared counts. A computation that began before views were forgotten
    /// (`generation` is stale), or one older than the newest, stores nothing.
    pub(crate) fn publish_summary_shared(
        &self,
        generation: u64,
        shared: Arc<SummaryShared>,
        inputs: SummaryInputs,
    ) {
        let list = &self.smalltalk.summary_list;
        let mut lists = list.lists.lock().unwrap_or_else(PoisonError::into_inner);
        if list.forgotten.load(AtomicOrdering::Acquire) != generation
            || lists.shared.as_ref().is_some_and(|(newest, _)| newest.cut > shared.cut)
        {
            return;
        }
        lists.shared = Some((shared, inputs));
    }

    /// Whether a window waits for a selection the refresher has not computed yet.
    pub(crate) fn summary_selection_waiting(&self) -> bool {
        self.smalltalk
            .summary_list
            .lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .selections
            .values()
            .any(|selection| selection.row.is_none() && !selection.failed)
    }

    /// Store one selection's row, or `None` when its shared counts could not be computed. Returns whether
    /// what its windows show changed: only then do they have anything to reread. A computation
    /// that began before views were forgotten (`generation` is stale) stores nothing.
    pub(crate) fn publish_summary_row(
        &self,
        generation: u64,
        person: &Option<String>,
        row: Option<SummaryRow>,
    ) -> bool {
        let list = &self.smalltalk.summary_list;
        let mut lists = list.lists.lock().unwrap_or_else(PoisonError::into_inner);
        if list.forgotten.load(AtomicOrdering::Acquire) != generation {
            return false;
        }
        let Some(selection) = lists.selections.get_mut(person) else {
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

    /// The machines the shared counts were last counted from, if any.
    pub(crate) fn summary_machines(&self) -> Option<SummaryMachines> {
        self.smalltalk
            .summary_list
            .lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .machines
            .clone()
    }

    /// Keep the machines read, or found to still hold, at `machines.cut`. A read that began
    /// before views were forgotten (`generation` is stale), or one older than the newest, keeps
    /// nothing.
    pub(crate) fn keep_summary_machines(&self, generation: u64, machines: SummaryMachines) {
        let list = &self.smalltalk.summary_list;
        let mut lists = list.lists.lock().unwrap_or_else(PoisonError::into_inner);
        if list.forgotten.load(AtomicOrdering::Acquire) != generation
            || lists.machines.as_ref().is_some_and(|newest| newest.cut > machines.cut)
        {
            return;
        }
        lists.machines = Some(machines);
    }

    /// How many operations conflict. A conflict changes the status of the subjects its claims
    /// are about without a claim of their own.
    pub(crate) fn conflicting_operations(&self) -> Result<u64> {
        Ok(self
            .readers
            .get()
            .prepare_cached("SELECT COUNT(*) FROM operations WHERE state='conflict'")?
            .query_row(/* sqlite-large-value: allow-for-now lv-h58da23dc1734 owner=st3::store::summary_list TODO=trace-statement-and-value-representation */ [], |row| row.get(0))?)
    }

    /// Whether the machines read at `after` may still hold at `through`, read in the snapshot at
    /// `through`: `None` when a claim admitted between them may change one, else the runtimes
    /// whose claims did, which may have started, stopped or moved to another host. A machine is
    /// a configured, fleet or current runtime's host with that host's own status. A runtime is a
    /// subject with a runtime observation; whether it is current, and where it runs, come from
    /// the claims about it or that it acted in, other than its harness reports without a live
    /// status (`CURRENT_VIEW_CLAIM`), and from the claims about the run and generation that own
    /// it. So only claims about hosts, runs and generations change machines, and claims about
    /// runtimes change only those runtimes; the rest, most of a busy host's claims (harness
    /// reports, messages, documents), change none. Conflicting operations, fleet membership and
    /// replication successes change machines without a claim: the caller compares those.
    pub(crate) fn summary_machines_hold(
        &self,
        after: u64,
        through: u64,
    ) -> Result<Option<BTreeSet<String>>> {
        if through < after {
            return Ok(None);
        }
        let connection = self.readers.get();
        let runtime = |of: &str| {
            format!(
                "EXISTS(SELECT 1 FROM claims AS runtime INDEXED BY claims_runtime_subject_index
                        WHERE runtime.kind='runtime.observed' AND runtime.subject=claims.{of}
                          AND runtime.store_index<=?2)"
            )
        };
        let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-h8550417f01db owner=st3::store::summary_list TODO=trace-statement-and-value-representation */ &format!(
            "SELECT subject, actor, kind, {CURRENT_VIEW_CLAIM}, {}, {}
             FROM claims WHERE store_index>?1 AND store_index<=?2 ORDER BY store_index LIMIT ?3",
            runtime("subject"),
            runtime("actor"),
        ))?;
        let claims = statement
            .query_map(params![after, through, MACHINES_DELTA_LIMIT as u64 + 1], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<bool>>(5)?.unwrap_or(false),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if claims.len() > MACHINES_DELTA_LIMIT {
            return Ok(None);
        }
        let mut runtimes = BTreeSet::new();
        for (subject, actor, kind, live, subject_runtime, actor_runtime) in claims {
            if subject.starts_with("host/")
                || subject.starts_with("mission-run/")
                || subject.starts_with("run-generation/")
            {
                return Ok(None);
            }
            if kind.starts_with("harness.") && !live {
                continue;
            }
            if subject_runtime {
                runtimes.insert(subject);
            }
            if actor_runtime && let Some(actor) = actor {
                runtimes.insert(actor);
            }
        }
        Ok((runtimes.len() <= MACHINES_RUNTIMES_LIMIT).then_some(runtimes))
    }
}
