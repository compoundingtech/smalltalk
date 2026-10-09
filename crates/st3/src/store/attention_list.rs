//! The attention list a daemon publishes for collection windows: every person's open attention
//! rows, folded off the request path by one refresher (`api::start_attention_list`) and served
//! under the cut they were folded at. One fold serves every person: a window keeps the rows
//! whose `person_id` it selects. A refresh folds again only when a claim admitted since the
//! newest publication can change what attention reads, the projection moved under the same
//! claims, or the clock passed the publication's period; otherwise it republishes the same rows
//! at the newer cut. Everything here is volatile cache state, cleared by `forget_views`.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

/// The views the refresher publishes, in the order it refreshes them.
pub(crate) const PUBLISHED_VIEWS: [&str; 4] = ["attention", "glasses", "arrangements", "summary"];

/// A view whose refresh has failed this long is withdrawn: its windows read for themselves, as
/// before, until it publishes again. Once published, a window rereads only on publication, so
/// a view that stopped publishing must not stay served.
const WITHDRAW_AFTER_MS: u64 = 30_000;

#[derive(Default)]
struct ViewHealth {
    /// When its refreshes began failing, in Unix ms; 0 while they succeed.
    failing_since: AtomicU64,
    withdrawn: AtomicBool,
}

/// More claims than this between two publications fold again rather than read them all.
const DELTA_LIMIT: usize = 10_000;

/// Claim kinds that no attention source reads, whatever their subject: harness timelines and
/// usage, messages (attention leaves messages to conversations), transport and workspace
/// observations, daemon diagnostics, and a person's glasses and arrangements. Work progress and
/// lease renewals move no step's status, run or ask. These are most of a busy host's claims.
const UNREAD_KINDS: &[&str] = &[
    "harness.timeline",
    "harness.usage",
    "message.sent",
    "message.staged",
    "message.delivered",
    "message.read",
    "message.closed",
    "transport.observed",
    "workspace.observed",
    "daemon.diagnostic",
    "glass.upserted",
    "glass.deleted",
    "arrangement.edited",
    "work.progress",
    "work.renewed",
];

/// The rows of one attention fold and the cut they were folded at.
pub(crate) struct AttentionPublication {
    /// The graph index the rows were folded at; windows serving them carry this snapshot.
    pub(crate) cut: u64,
    /// The graph projection's health row at the cut. Deferred replication projection catches
    /// up under the same admitted claims, so a different row means the rows may have changed.
    pub(crate) projection: Option<(String, u64)>,
    /// The clock the rows were evaluated at: eligibility and grace periods depend on it.
    pub(crate) evaluated_at_unix_ms: u128,
    /// When these rows were published, in Unix ms: the list's "as of".
    pub(crate) published_at_unix_ms: u128,
    /// Every person's rows, most urgent first, as `client_attention_resources_at` orders them.
    pub(crate) rows: Arc<Vec<Value>>,
}

/// The refresher's registration, its requests and the newest publication.
#[derive(Default)]
pub(crate) struct AttentionList {
    refresher: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
    /// When a window last read one of the refresher's views (attention, glasses, arrangements),
    /// in Unix ms. The refresher follows commits only while someone reads.
    read_at: AtomicU64,
    published: Mutex<Option<Arc<AttentionPublication>>>,
    /// Counts `forget`s. A refresh that began before one may not publish what it read.
    forgotten: AtomicU64,
    /// Counts publications, same cut or not, so streams that read an earlier one reread.
    revision: tokio::sync::watch::Sender<u64>,
    /// Set when the refresher task ends: no view is served after that.
    stopped: AtomicBool,
    health: [ViewHealth; PUBLISHED_VIEWS.len()],
    /// Folds by cause: the first claim kind that could change attention, `clock`, `projection`,
    /// `first` or `many`. Republishing the same rows at a newer cut is not a fold.
    folds: Mutex<BTreeMap<String, u64>>,
}

impl AttentionList {
    pub(super) fn forget(&self) {
        let mut published = self.published.lock().unwrap_or_else(PoisonError::into_inner);
        self.forgotten.fetch_add(1, AtomicOrdering::AcqRel);
        let forgotten = published.take().is_some();
        drop(published);
        if forgotten {
            self.revision.send_modify(|revision| *revision += 1);
        }
    }
}

/// Why the newest publication can no longer stand for the rows at a newer cut.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttentionDelta {
    /// Nothing attention reads changed: the same rows hold at the newer cut.
    Unchanged,
    /// Fold again, and why.
    Refold(String),
}

impl Store {
    /// Register the one task that keeps the attention list published, and return its wake.
    /// `None` when one is already registered.
    pub(crate) fn start_attention_list_refresher(&self) -> Option<Arc<tokio::sync::Notify>> {
        let wake = Arc::new(tokio::sync::Notify::new());
        self.smalltalk
            .attention_list
            .refresher
            .set(Arc::clone(&wake))
            .ok()?;
        Some(wake)
    }

    /// Whether a refresher was started for the attention list and its sibling views.
    pub(crate) fn attention_list_refresher_running(&self) -> bool {
        let list = &self.smalltalk.attention_list;
        list.refresher.get().is_some() && !list.stopped.load(AtomicOrdering::Acquire)
    }

    /// Whether windows of `view` serve its publication rather than reading for themselves: its
    /// refresher runs and has not withdrawn it.
    pub(crate) fn published_view_serving(&self, view: &str) -> bool {
        let Some(position) = PUBLISHED_VIEWS.iter().position(|name| *name == view) else {
            return false;
        };
        self.attention_list_refresher_running()
            && !self.smalltalk.attention_list.health[position]
                .withdrawn
                .load(AtomicOrdering::Acquire)
    }

    /// Record one refresh of `view`. A view failing for [`WITHDRAW_AFTER_MS`] is withdrawn, so
    /// its windows follow commits again; its next good refresh serves it again.
    pub(crate) fn note_view_refreshed(&self, view: &str, succeeded: bool) {
        let Some(position) = PUBLISHED_VIEWS.iter().position(|name| *name == view) else {
            return;
        };
        let health = &self.smalltalk.attention_list.health[position];
        let now = now_ms() as u64;
        if succeeded {
            health.failing_since.store(0, AtomicOrdering::Release);
            if health.withdrawn.swap(false, AtomicOrdering::AcqRel) {
                eprintln!("st3: the {view} view publishes again; its windows serve it");
                self.publish_collection_view(view);
            }
            return;
        }
        let since = match health.failing_since.compare_exchange(
            0, now, AtomicOrdering::AcqRel, AtomicOrdering::Acquire,
        ) {
            Ok(_) => now,
            Err(since) => since,
        };
        if now.saturating_sub(since) >= WITHDRAW_AFTER_MS
            && !health.withdrawn.swap(true, AtomicOrdering::AcqRel)
        {
            eprintln!("st3: WARN the {view} view has failed to refresh for {} s; its windows read for themselves until it publishes again", now.saturating_sub(since) / 1000);
            self.withdraw_collection_view(view);
        }
    }

    /// The refresher task ended: windows of every view read for themselves from now on.
    pub(crate) fn stop_attention_list_refresher(&self) {
        let list = &self.smalltalk.attention_list;
        if list.refresher.get().is_none() || list.stopped.swap(true, AtomicOrdering::AcqRel) {
            return;
        }
        eprintln!("st3: WARN the attention, glasses, arrangements and summary refresher stopped; their windows read for themselves");
        for view in PUBLISHED_VIEWS {
            self.withdraw_collection_view(view);
        }
    }

    /// Ask the refresher, if one runs, to publish the list at the newest cut. Requests made
    /// while it folds coalesce into one more refresh.
    pub(crate) fn request_attention_list_refresh(&self) {
        if let Some(wake) = self.smalltalk.attention_list.refresher.get() {
            wake.notify_one();
        }
    }

    /// The newest publication, and note that a window read it. A window serves these rows
    /// under the publication's own cut, never relabelled with its own, even when the window's
    /// snapshot began before that cut was published.
    pub(crate) fn published_attention_list(&self) -> Option<Arc<AttentionPublication>> {
        let list = &self.smalltalk.attention_list;
        list.refresher.get()?;
        self.note_published_view_read();
        list.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The newest publication, whatever its cut, for the refresher to start from, and the
    /// generation to publish under: read it before the snapshot that folds.
    pub(crate) fn newest_attention_list(&self) -> (u64, Option<Arc<AttentionPublication>>) {
        let list = &self.smalltalk.attention_list;
        let published = list.published.lock().unwrap_or_else(PoisonError::into_inner);
        (list.forgotten.load(AtomicOrdering::Acquire), published.clone())
    }

    /// Note that a window read one of the refresher's views: it follows commits only while
    /// windows read them.
    pub(crate) fn note_published_view_read(&self) {
        self.smalltalk
            .attention_list
            .read_at
            .store(now_ms() as u64, AtomicOrdering::Release);
    }

    /// Whether a window read one of the refresher's views within `within_ms`.
    pub(crate) fn attention_list_read_within(&self, within_ms: u64) -> bool {
        let read_at = self.smalltalk.attention_list.read_at.load(AtomicOrdering::Acquire);
        read_at != 0 && (now_ms() as u64).saturating_sub(read_at) <= within_ms
    }

    /// Swap in a newer publication. A publication never goes back to an older cut. Only
    /// `changed` rows raise the revision: the same rows at a newer cut give streams nothing to
    /// reread, and a window that reads later serves them under the newer cut.
    /// A refresh that began before views were forgotten (`generation` is stale) publishes
    /// nothing: what it read may no longer hold.
    pub(crate) fn publish_attention_list(
        &self,
        generation: u64,
        publication: AttentionPublication,
        changed: bool,
    ) {
        let list = &self.smalltalk.attention_list;
        {
            let mut published = list.published.lock().unwrap_or_else(PoisonError::into_inner);
            if list.forgotten.load(AtomicOrdering::Acquire) != generation {
                return;
            }
            if published
                .as_ref()
                .is_some_and(|newest| newest.cut > publication.cut)
            {
                return;
            }
            *published = Some(Arc::new(publication));
        }
        if changed {
            list.revision.send_modify(|revision| *revision += 1);
            self.publish_collection_view("attention");
        }
    }

    /// Follows the revision of attention publications, so a stream that read an earlier one
    /// rereads the newer.
    #[cfg(test)]
    pub(crate) fn subscribe_attention_list(&self) -> tokio::sync::watch::Receiver<u64> {
        self.smalltalk.attention_list.revision.subscribe()
    }

    /// Count one fold of the whole list, by why.
    pub(crate) fn note_attention_list_fold(&self, cause: &str) {
        *self
            .smalltalk
            .attention_list
            .folds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(cause.to_owned())
            .or_default() += 1;
    }

    /// How many times the attention list was folded, by why.
    pub fn attention_list_folds(&self) -> BTreeMap<String, u64> {
        self.smalltalk
            .attention_list
            .folds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The graph projection's health row, read in the snapshot that folds or compares.
    pub(crate) fn attention_projection_frontier(&self) -> Result<Option<(String, u64)>> {
        Ok(self
            .readers
            .get()
            .query_row(
                "SELECT status, last_good_store_index FROM projection_health WHERE aggregate='graph'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<u64>>(1)?.unwrap_or(0))),
            )
            .optional()?)
    }

    /// Whether the rows of `previous` still hold at `index`, read in the snapshot at `index`.
    /// Only the kinds and subjects of the claims admitted since are read, and for harness
    /// observations whether their seat was ever asked to log in.
    pub(crate) fn attention_list_delta(
        &self,
        previous: &AttentionPublication,
        index: u64,
    ) -> Result<AttentionDelta> {
        if index < previous.cut {
            return Ok(AttentionDelta::Refold("older cut".into()));
        }
        if self.attention_projection_frontier()? != previous.projection {
            return Ok(AttentionDelta::Refold("projection".into()));
        }
        if index == previous.cut {
            return Ok(AttentionDelta::Unchanged);
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind FROM claims WHERE store_index>?1 AND store_index<=?2
             ORDER BY store_index LIMIT ?3",
        )?;
        let claims = statement
            .query_map(
                params![previous.cut, index, DELTA_LIMIT as u64 + 1],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if claims.len() > DELTA_LIMIT {
            return Ok(AttentionDelta::Refold("many".into()));
        }
        let mut logins = HashMap::<String, bool>::new();
        let mut retiring = HashMap::<String, bool>::new();
        for (subject, kind) in claims {
            if UNREAD_KINDS.contains(&kind.as_str()) {
                continue;
            }
            // Harness observations and diagnostics reach attention only through a harness login
            // item, and only for a seat with a login-shaped claim: the candidates
            // `desired_harness_login_candidates` reads. A seat never asked to log in has none
            // before this claim or after it.
            let login = matches!(kind.as_str(), "harness.observed" | "harness.diagnostic" | "runtime.observed");
            if login {
                let candidate = match logins.get(&subject) {
                    Some(candidate) => *candidate,
                    None => {
                        let candidate = harness_login_candidate(&connection, &subject)?;
                        logins.insert(subject.clone(), candidate);
                        candidate
                    }
                };
                // A runtime observation is also the actual state an ask from a retiring seat
                // reads (`rollouts::retiring_ask_live`), and only while its declaration is a stop.
                let stopping = kind == "runtime.observed"
                    && match retiring.get(&subject) {
                        Some(stopping) => *stopping,
                        None => {
                            let stopping = declared_stop(&connection, &subject)?;
                            retiring.insert(subject.clone(), stopping);
                            stopping
                        }
                    };
                if !candidate && !stopping {
                    continue;
                }
            }
            return Ok(AttentionDelta::Refold(kind));
        }
        Ok(AttentionDelta::Unchanged)
    }
}

/// Whether `subject`'s current declaration is a stop: only then can a retiring seat's ask read
/// its runtime.
fn declared_stop(connection: &Connection, subject: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached("SELECT kind='stop' FROM desired WHERE subject=?1")?
        .query_row([subject], |row| row.get(0))
        .optional()?
        .unwrap_or(false))
}

/// Whether `subject` has a claim that makes it a harness login candidate, by the same indexed
/// predicate `desired_harness_login_candidates` uses.
fn harness_login_candidate(connection: &Connection, subject: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_harness_login_candidate_index
                 WHERE claims.subject=?1 AND (
                   (kind='harness.observed' AND (
                     json_type(body, '$.fields.provider_auth')='false'
                     OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                         THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
                     OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                         THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
                   OR (kind='harness.diagnostic'
                     AND json_extract(body, '$.fields.code')='provider-auth-expired')))",
        )?
        .query_row([subject], |row| row.get(0))?)
}

#[cfg(test)]
#[path = "attention_list/tests.rs"]
mod tests;
