//! Shared bounded windows, never authority. The existing socket feed still publishes frames.
//! A future graph-watch consumer can replace `revision` with view/frontier invalidations;
//! authorized row reads and socket snapshot/diff delivery remain the same.
use super::*;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};

const STORES: usize = 64;
const WINDOWS: usize = 64;
const SESSION_WINDOWS: usize = 16;
const KIND_LIMIT: usize = 10_000;
const COLLECTIONS: [&str; 6] = [
    "missions",
    "attention",
    "agents",
    "work",
    "glasses",
    "arrangements",
];

#[derive(Default)]
struct Revisions {
    index: u64,
    commits: u64,
    local: u64,
    values: [u64; 6],
}

struct Cached {
    revision: u64,
    period: u128,
    valid_from: u128,
    valid_until: Option<u128>,
    items: Vec<Value>,
    has_more: bool,
}

#[derive(Default)]
struct Observed {
    index: u64,
    local: u64,
}

#[derive(Default)]
struct Entry {
    admission: Arc<tokio::sync::Mutex<()>>,
    cached: Mutex<Option<Arc<Cached>>>,
}

type WindowEntry = Arc<Entry>;
type WindowEntries = VecDeque<(String, String, WindowEntry)>;

#[derive(Clone)]
pub(super) struct Prepared {
    key: String,
    entry: WindowEntry,
}

impl Prepared {
    pub(super) async fn admit(&self) -> tokio::sync::OwnedMutexGuard<()> {
        let started = Instant::now();
        let guard = self.entry.admission.clone().lock_owned().await;
        crate::performance::record_request("collection/admission-wait", None, started.elapsed());
        guard
    }
}

pub(super) struct ReadFence {
    pub(super) index: u64,
    pub(super) now: u128,
    pub(super) commits: u64,
    pub(super) prepared: Option<Prepared>,
}

/// Held by all sockets using this Store, and by physical workers until they finish.
/// The registry and commit callback keep only weak references. No follower task is started.
pub(super) struct Windows {
    commits: AtomicU64,
    revisions: Mutex<Revisions>,
    observed: Mutex<Observed>,
    entries: Mutex<WindowEntries>,
    observer: Mutex<Option<smallclaims::sqlite::CommitObserver>>,
    #[cfg(test)]
    builds: std::sync::atomic::AtomicUsize,
}

type Registry = Vec<(Weak<Store>, Weak<Windows>)>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

impl Windows {
    pub(super) fn commits(&self) -> u64 {
        self.commits.load(Ordering::Acquire)
    }
    pub(super) fn attach(store: &Arc<Store>) -> Option<Arc<Self>> {
        let mut registry = REGISTRY
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|(store, windows)| store.strong_count() > 0 && windows.strong_count() > 0);
        let store_key = Arc::downgrade(store);
        if let Some(windows) = registry
            .iter()
            .find_map(|(key, windows)| key.ptr_eq(&store_key).then(|| windows.upgrade()).flatten())
        {
            return Some(windows);
        }
        if registry.len() >= STORES {
            return None;
        }
        let windows = Arc::new(Self {
            commits: AtomicU64::new(0),
            revisions: Mutex::new(Revisions::default()),
            observed: Mutex::new(Observed {
                index: store.index().unwrap_or(0),
                local: 0,
            }),
            entries: Mutex::new(VecDeque::new()),
            observer: Mutex::new(None),
            #[cfg(test)]
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&windows);
        let observed_store = store_key.clone();
        // Register before any snapshot. The callback does no SQL or publication; a shared
        // snapshot reader weighs bounded kind metadata once for all equivalent sockets.
        let observer = store.observe_commits(move |_| {
            if let Some(windows) = weak.upgrade() {
                let mut observed = windows
                    .observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // The writer updates its atomic committed index before this callback. Retain
                // a separate epoch for commits with no claim advance: a later ignored claim
                // cannot erase their invalidation. No SQL runs on the writer callback.
                match observed_store
                    .upgrade()
                    .and_then(|store| store.index().ok())
                {
                    Some(index) if index > observed.index => observed.index = index,
                    _ => observed.local = observed.local.wrapping_add(1),
                }
                windows.commits.fetch_add(1, Ordering::Release);
            }
        });
        *windows
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
        registry.push((store_key, Arc::downgrade(&windows)));
        Some(windows)
    }

    fn entry(&self, key: String, session: String) -> Option<WindowEntry> {
        // Inputs are untrusted; do not retain oversized query/authority keys.
        if key.len() > 4096 {
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = entries.iter().position(|(held, _, _)| *held == key) {
            let entry = entries.remove(position).expect("found entry");
            let result = entry.2.clone();
            entries.push_back(entry);
            return Some(result);
        }
        let session_full = entries
            .iter()
            .filter(|(_, owner, _)| *owner == session)
            .count()
            >= SESSION_WINDOWS;
        if session_full || entries.len() >= WINDOWS {
            // Query churn beyond a session's quota evicts only its own inactive entries.
            // Reservations protect both queued async readers and physical computations.
            let position = entries.iter().position(|(_, owner, entry)| {
                (!session_full || *owner == session) && Arc::strong_count(entry) == 1
            })?;
            entries.remove(position);
        }
        let entry = Arc::new(Entry::default());
        entries.push_back((key, session, entry.clone()));
        Some(entry)
    }

    fn revision(
        &self,
        store: &Store,
        index: u64,
        collection: &str,
        snapshot_commits: u64,
    ) -> anyhow::Result<Option<u64>> {
        let mut revisions = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let commits = self.commits.load(Ordering::Acquire);
        // A slower physical reader can hold an older SQLite snapshot. It cannot update the
        // shared frontier or reuse rows from a newer one.
        if index < revisions.index
            || commits != snapshot_commits
            || revisions.commits > snapshot_commits
        {
            return Ok(None);
        }
        // Seed from the first authorized snapshot in case registration raced a commit.
        observed.index = observed.index.max(index);
        let local = observed.local;
        drop(observed);
        if index != revisions.index || commits != revisions.commits {
            let connection = store.readers.get();
            let mut kinds = connection.prepare_cached("SELECT kind FROM claims WHERE store_index>?1 AND store_index<=?2 ORDER BY store_index LIMIT ?3")?;
            let kinds = kinds
                .query_map(
                    rusqlite::params![revisions.index, index, KIND_LIMIT + 1],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A non-claim commit can change local availability, ordering or lease state.
            // Overflow, replay/trim and metadata errors must never certify an unchanged view.
            let all = local != revisions.local
                || index == revisions.index
                || kinds.is_empty()
                || kinds.len() > KIND_LIMIT;
            let arrangements = store.arrangements_changed(revisions.index, index)?;
            for (position, name) in COLLECTIONS.iter().enumerate() {
                if all
                    || (*name == "arrangements" && arrangements)
                    || kinds.iter().any(|kind| !collection_ignores(name, kind))
                {
                    revisions.values[position] = revisions.values[position].wrapping_add(1);
                }
            }
            revisions.index = index;
            revisions.commits = commits;
            revisions.local = local;
        }
        Ok(COLLECTIONS
            .iter()
            .position(|name| *name == collection)
            .map(|position| revisions.values[position]))
    }

    pub(super) fn changes(&self, store: &Store) -> anyhow::Result<[u64; 6]> {
        let commits = self.commits();
        store.read_snapshot(|index| {
            let mut values = [0; 6];
            for (position, collection) in COLLECTIONS.iter().enumerate() {
                values[position] = self
                    .revision(store, index, collection, commits)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("collection frontier changed during metadata read")
                    })?;
            }
            Ok(values)
        })
    }

    pub(super) fn changed(collection: &str, before: &[u64; 6], after: &[u64; 6]) -> bool {
        COLLECTIONS
            .iter()
            .position(|name| *name == collection)
            .is_none_or(|position| before[position] != after[position])
    }

    fn key(state: &AppState, session: &ClientSession, request: &CollectionSubscribe) -> String {
        // These projections have no per-grant data once authority has been revalidated.
        // Limits are window slicing, not projection inputs. Attention's effective person
        // and compatibility view are its visibility boundary; missions are fleet-public.
        if request.collection == "missions" {
            return json!({"node":state.node, "fleet":state.fleet_id,
                "collection":"missions"}).to_string();
        }
        if request.collection == "attention"
            && let Ok(person) = person_filter(session, request.person.as_deref())
        {
            return json!({"node":state.node, "fleet":state.fleet_id,
                "collection":"attention", "person":person,
                "custom_forms":session.custom_forms}).to_string();
        }
        json!({
            "node":state.node, "fleet":state.fleet_id,
            "actor":session.actor, "authority":session.authority_actor,
            "grant":session.pairing_grant, "transport":session.transport,
            "scopes":session.scopes, "custom_forms":session.custom_forms,
            "conversation_blocks":session.conversation_blocks,
            "collection":request.collection, "limit":request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS),
            "person":request.person, "subject":request.subject, "filter_actor":request.actor, "status":request.status,
        }).to_string()
    }

    /// Reserve and await admission before opening SQLite or scheduling a blocking worker.
    pub(super) fn prepare(
        &self,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
    ) -> Option<Prepared> {
        let key = Self::key(state, session, request);
        let owner = json!({"node":state.node, "fleet":state.fleet_id,
            "actor":session.actor, "authority":session.authority_actor,
            "grant":session.pairing_grant, "transport":session.transport})
        .to_string();
        let entry = self.entry(key.clone(), owner)?;
        Some(Prepared { key, entry })
    }

    /// Called inside the authorized SQLite snapshot after async admission. Cache mutexes
    /// hold only Arc loads/stores; computation, serialization and deep clones run outside them.
    pub(super) fn read(
        &self,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
        fence: ReadFence,
        compute: impl FnOnce() -> anyhow::Result<(Vec<Value>, bool, Option<u128>)>,
    ) -> anyhow::Result<(Vec<Value>, bool)> {
        let ReadFence {
            index,
            now,
            commits: snapshot_commits,
            prepared,
        } = fence;
        #[cfg(test)]
        let compute = || {
            self.builds.fetch_add(1, Ordering::SeqCst);
            compute()
        };
        let Some(prepared) = prepared else {
            return compute().map(|(items, more, _)| (items, more));
        };
        // Authority can change while async admission waits. The current snapshot's key
        // must still match; otherwise this read computes without publishing a cache entry.
        if prepared.key != Self::key(state, session, request) {
            return compute().map(|(items, more, _)| (items, more));
        }
        // A commit after this reader began can update local tables without advancing the
        // claim index. Such a snapshot cannot reuse/cache a newer local-state generation.
        if self.commits() != snapshot_commits {
            return compute().map(|(items, more, _)| (items, more));
        }
        let Some(revision) =
            self.revision(&state.store, index, &request.collection, snapshot_commits)?
        else {
            return compute().map(|(items, more, _)| (items, more));
        };
        // Work uses its selected projection time. Missions and attention use exact
        // transition fences, not a coarse shared wall-clock bucket.
        let period = match request.collection.as_str() {
            // Work's deterministic selected projection time is itself an input, even if a
            // claim kind does not otherwise change its rows (elapsed budgets/lease expiry).
            "work" => state.store.projection_time_at(index)?,
            _ => 0,
        };
        let cached = prepared
            .entry
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(cached) = cached
            && cached.revision == revision
            && cached.period == period
            && now >= cached.valid_from
            && cached.valid_until.is_none_or(|until| now < until)
        {
            return Ok((cached.items.clone(), cached.has_more));
        }
        let (items, has_more, valid_until) = compute()?;
        // At most 64 bounded responses per Store. Oversized results remain uncached.
        let cached = (serde_json::to_vec(&items)?.len() <= CLIENT_MAX_RESPONSE_BYTES).then(|| {
            Arc::new(Cached {
                revision,
                period,
                valid_from: now,
                valid_until,
                items: items.clone(),
                has_more,
            })
        });
        *prepared
            .entry
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cached;
        Ok((items, has_more))
    }

    #[cfg(test)]
    pub(super) fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn request(collection: &str) -> CollectionSubscribe {
        serde_json::from_value(
            json!({"kind":"subscribe", "id":"one", "collection":collection, "limit":2}),
        )
        .unwrap()
    }

    fn state(root: &std::path::Path) -> AppState {
        super::super::tests::test_state_named(root, "shared-window-test")
    }

    fn read(
        windows: &Windows,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
        now: u128,
        count: &AtomicUsize,
    ) -> Vec<Value> {
        let prepared = windows.prepare(state, session, request);
        let _admission = prepared
            .as_ref()
            .map(|p| p.entry.admission.clone().blocking_lock_owned());
        let commits = windows.commits();
        state.store.read_snapshot(|index| {
            windows.read(state, session, request, ReadFence { index, now, commits, prepared }, || {
                let n = count.fetch_add(1, Ordering::SeqCst);
                Ok((vec![json!({"id":format!("row/{n}"), "authority":session.authority_actor})], false, None))
            }).map(|(rows, _)| rows)
        }).unwrap()
    }

    fn diagnostic(state: &AppState) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: "daemon/shared-window".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: serde_json::from_value(
                    json!({"code":"fixture", "severity":"error", "reason":"unrelated"}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[test]
    fn shared_windows_compute_once_for_concurrent_sockets_and_cleanup_after_last_reader() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let first = Windows::attach(&state.store).unwrap();
        let second = Windows::attach(&state.store).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let weak = Arc::downgrade(&first);
        let count = Arc::new(AtomicUsize::new(0));
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let writer = state.clone();
        let builds = count.clone();
        let thread = std::thread::spawn(move || {
            let session = ClientSession::local(None).unwrap();
            let query = request("missions");
            let prepared = first.prepare(&writer, &session, &query);
            let _admission = prepared
                .as_ref()
                .map(|p| p.entry.admission.clone().blocking_lock_owned());
            let commits = first.commits();
            writer
                .store
                .read_snapshot(|index| {
                    first.read(
                        &writer,
                        &ClientSession::local(None).unwrap(),
                        &request("missions"),
                        ReadFence {
                            index,
                            now: 0,
                            commits,
                            prepared,
                        },
                        || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            entered.send(()).unwrap();
                            held.recv().unwrap();
                            Ok((vec![json!({"id":"shared"})], false, None))
                        },
                    )
                })
                .unwrap()
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let other = state.clone();
        let builds = count.clone();
        let (starting, started) = std::sync::mpsc::channel();
        let other_thread = std::thread::spawn(move || {
            starting.send(()).unwrap();
            read(
                &second,
                &other,
                &ClientSession::local(None).unwrap(),
                &request("missions"),
                0,
                &builds,
            )
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        let first_rows = thread.join().unwrap().0;
        let second_rows = other_thread.join().unwrap();
        assert_eq!(first_rows, second_rows);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(
            weak.upgrade().is_none(),
            "no registry/observer/worker keeps the cache alive"
        );
        let new = Windows::attach(&state.store).unwrap();
        read(
            &new,
            &state,
            &ClientSession::local(None).unwrap(),
            &request("missions"),
            0,
            &count,
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "a new lifetime starts from authority"
        );
    }

    #[test]
    fn shared_windows_skip_irrelevant_claims_but_refresh_relevant_local_and_clock_inputs() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let session = ClientSession::local(None).unwrap();
        let count = AtomicUsize::new(0);
        let mission = request("missions");
        let first = read(&windows, &state, &session, &mission, 0, &count);
        diagnostic(&state);
        assert_eq!(read(&windows, &state, &session, &mission, 0, &count), first);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        state
            .store
            .apply_internal(
                &crate::graph::parse_intent(
                    "version 2\nagent \"shared-window\" { harness \"omp\" {} }\n",
                    state.store.origin(),
                )
                .unwrap(),
                "fixture",
            )
            .unwrap();
        assert_ne!(read(&windows, &state, &session, &mission, 0, &count), first);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // A lent writer can change local tables without advancing claim index.
        let index = state.store.index().unwrap();
        drop(state.store.connection.lock().unwrap());
        assert_eq!(state.store.index().unwrap(), index);
        read(&windows, &state, &session, &mission, 0, &count);
        assert_eq!(count.load(Ordering::SeqCst), 3);
        let attention = request("attention");
        let first = read(&windows, &state, &session, &attention, 29_999, &count);
        assert_eq!(
            read(&windows, &state, &session, &attention, 29_999, &count),
            first
        );
        assert_eq!(
            read(&windows, &state, &session, &attention, 30_000, &count),
            first,
            "no graph transition means crossing a timer bucket does not rebuild"
        );
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn shared_windows_local_commit_is_not_hidden_by_a_later_ignored_claim() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let session = ClientSession::local(None).unwrap();
        let count = AtomicUsize::new(0);
        let query = request("missions");
        let before = read(&windows, &state, &session, &query, 0, &count);
        let index = state.store.index().unwrap();
        {
            let connection = state.store.connection.lock().unwrap();
            connection
                .execute(
                    "INSERT OR REPLACE INTO meta VALUES('fixture-local-window', 'changed')",
                    [],
                )
                .unwrap();
        }
        assert_eq!(state.store.index().unwrap(), index);
        diagnostic(&state);
        assert!(state.store.index().unwrap() > index);
        let after = read(&windows, &state, &session, &query, 0, &count);
        assert_ne!(
            after, before,
            "ignored claims must not erase a local invalidation"
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let mut cold = query;
        cold.id = "cold-subscriber".into();
        assert_eq!(read(&windows, &state, &session, &cold, 0, &count), after);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shared_windows_collection_scope_loss_under_the_same_grant_denies_cached_and_cold_reads()
     {
        for (collection, scope) in [
            ("glasses", "read.glasses"),
            ("arrangements", "read.arrangements"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let state = state(root.path());
            let grant = "custom/client/scoped-window-reader";
            let pair = |scopes: Value| {
                state.store.append_claim(&ClaimInput {
                subject:grant.into(), kind:"custom.client.pairing-completed".into(), actor:Some("person/ada".into()),
                fields:serde_json::from_value(json!({"session_actor":"client/scoped-window-reader", "person_id":"person/ada", "expires_at_unix_ms":client_now_ms() as u64 + 60_000, "scopes":scopes})).unwrap(),
                evidence:vec![], expected_subject:None, idempotency_key:None,
            }).unwrap()
            };
            let paired = pair(json!(["read.projections", scope]));
            let original =
                paired_client_session(&state, &paired, "fabric-loopback", false).unwrap();
            let windows = Windows::attach(&state.store).unwrap();
            let slots = Arc::new(tokio::sync::Semaphore::new(1));
            let mut query = request(collection);
            if collection == "arrangements" {
                query.person = Some("person/ada".into());
            }
            collection_items_with_windows(
                &state,
                &original,
                &query,
                slots.clone().acquire_owned().await.unwrap(),
                Some(windows.clone()),
            )
            .await
            .unwrap();
            pair(json!(["read.projections"]));
            for cache in [Some(windows.clone()), None] {
                let error = collection_items_with_windows(
                    &state,
                    &original,
                    &query,
                    slots.clone().acquire_owned().await.unwrap(),
                    cache,
                )
                .await
                .unwrap_err();
                assert_eq!(error.status, StatusCode::FORBIDDEN);
                assert!(error.message.contains(scope), "{collection}: {error:?}");
            }
        }
    }

    #[test]
    fn shared_windows_isolate_authority_grants_scopes_and_query_keys() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let count = AtomicUsize::new(0);
        let session = ClientSession::local(Some("person/ada")).unwrap();
        let query = request("work");
        let first = read(&windows, &state, &session, &query, 0, &count);
        let mut same = query.clone();
        same.id = "another-socket-id".into();
        assert_eq!(read(&windows, &state, &session, &same, 0, &count), first);
        let mut variants = vec![ClientSession::local(Some("person/avery")).unwrap()];
        let mut grant = session.clone();
        grant.pairing_grant = Some("custom/client/another-grant".into());
        variants.push(grant);
        let mut scope = session.clone();
        scope.scopes.remove("read.arrangements");
        variants.push(scope);
        let mut compatibility = session.clone();
        compatibility.custom_forms = !compatibility.custom_forms;
        variants.push(compatibility);
        for other in variants {
            assert_ne!(read(&windows, &state, &other, &query, 0, &count), first);
        }
        for field in ["limit", "person", "actor", "subject", "status"] {
            let mut other = query.clone();
            match field {
                "limit" => other.limit = Some(1),
                "person" => other.person = Some("person/ada".into()),
                "actor" => other.actor = Some("agent/selected".into()),
                "subject" => other.subject = Some("arrangement/person/ada/one".into()),
                "status" => other.status = Some("running".into()),
                _ => unreachable!(),
            }
            assert_ne!(read(&windows, &state, &session, &other, 0, &count), first);
        }
        assert_eq!(count.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn shared_windows_never_reuse_newer_rows_in_an_old_snapshot_or_after_mid_read_commit() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let query = request("missions");
        let session = ClientSession::local(None).unwrap();
        let count = AtomicUsize::new(0);
        let prepared = windows.prepare(&state, &session, &query);
        let old_commits = windows.commits();
        state
            .store
            .read_snapshot(|index| {
                let writer = state.clone();
                let held = windows.clone();
                std::thread::spawn(move || {
                    diagnostic(&writer);
                    read(
                        &held,
                        &writer,
                        &ClientSession::local(None).unwrap(),
                        &request("missions"),
                        0,
                        &AtomicUsize::new(0),
                    );
                })
                .join()
                .unwrap();
                let _admission = prepared
                    .as_ref()
                    .map(|p| p.entry.admission.clone().try_lock_owned().unwrap());
                let rows = windows
                    .read(
                        &state,
                        &session,
                        &query,
                        ReadFence {
                            index,
                            now: 0,
                            commits: old_commits,
                            prepared,
                        },
                        || {
                            count.fetch_add(1, Ordering::SeqCst);
                            Ok((vec![json!({"id":"old-snapshot"})], false, None))
                        },
                    )?
                    .0;
                assert_eq!(rows[0]["id"], "old-snapshot");
                Ok(())
            })
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let rows = read(&windows, &state, &session, &query, 0, &count);
        assert_ne!(rows[0]["id"], "old-snapshot");
    }

    #[test]
    fn shared_windows_bound_cache_and_retry_failed_computations() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let session = ClientSession::local(None).unwrap();
        let count = AtomicUsize::new(0);
        for limit in 1..=WINDOWS + 2 {
            let mut query = request("work");
            query.limit = Some(limit);
            let mut owner = session.clone();
            owner.actor = format!("client/owner/{}", limit / SESSION_WINDOWS);
            read(&windows, &state, &owner, &query, 0, &count);
        }
        assert_eq!(windows.entries.lock().unwrap().len(), WINDOWS);
        let query = request("attention");
        let prepared = windows.prepare(&state, &session, &query);
        let _admission = prepared
            .as_ref()
            .map(|p| p.entry.admission.clone().blocking_lock_owned());
        let commits = windows.commits();
        let result = state.store.read_snapshot(|index| {
            windows.read(
                &state,
                &session,
                &query,
                ReadFence {
                    index,
                    now: 0,
                    commits,
                    prepared: prepared.clone(),
                },
                || anyhow::bail!("fixture read failure"),
            )
        });
        assert!(result.is_err());
        drop(_admission);
        drop(prepared);
        read(&windows, &state, &session, &query, 0, &count);
        assert_eq!(count.load(Ordering::SeqCst), WINDOWS + 3);
    }

    #[test]
    fn shared_windows_query_churn_cannot_evict_another_sessions_window() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let victim = ClientSession::local(None).unwrap();
        let mut attacker = victim.clone();
        attacker.actor = "client/churn".into();
        let count = AtomicUsize::new(0);
        let query = request("agents");
        let rows = read(&windows, &state, &victim, &query, 0, &count);
        for n in 0..WINDOWS * 2 {
            let mut query = request("agents");
            query.status = Some(format!("fixture-status/{n}"));
            read(&windows, &state, &attacker, &query, 0, &count);
        }
        assert_eq!(windows.entries.lock().unwrap().len(), SESSION_WINDOWS + 1);
        let before = count.load(Ordering::SeqCst);
        assert_eq!(read(&windows, &state, &victim, &query, 0, &count), rows);
        assert_eq!(count.load(Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn shared_windows_waiters_do_not_start_physical_reads_and_cancel_without_dropping_worker_admission()
     {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let session = ClientSession::local(None).unwrap();
        let query = request("missions");
        let first = windows.prepare(&state, &session, &query).unwrap();
        let guard = first.admit().await;
        let (release, held) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let physical = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            entered.send(()).unwrap();
            held.recv().unwrap();
        });
        started.await.unwrap();
        physical.abort(); // a started physical worker still owns admission
        let second = windows.prepare(&state, &session, &query).unwrap();
        let mut waiter = Box::pin(second.admit());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        // Drop the canceled async waiter: it has never opened a snapshot or a worker.
        drop(waiter);
        assert!(second.entry.admission.clone().try_lock_owned().is_err());
        release.send(()).unwrap();
        physical.await.unwrap();
        let _guard = second.admit().await;
    }

    #[test]
    fn shared_windows_attention_and_mission_helpers_use_the_captured_clock() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let intent = crate::graph::parse_internal_intent(
            "version 2\nmission \"clock\" state=\"ready\" { goal \"Clock\"; step \"review\" { assigned-to \"person/ada\"; goal \"Review\"; } }\n",
            state.store.origin(),
        ).unwrap();
        state
            .store
            .apply_internal(&intent, "clock-fixture")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "clock".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/ada".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "clock-run".into(),
            })
            .unwrap();
        {
            let connection = state.store.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE step_runs SET status='ready' WHERE subject=?1",
                    [&run.steps[0].subject],
                )
                .unwrap();
        }
        let current =
            client_attention_resources_at(&state.store, Some("person/ada"), false, client_now_ms())
                .unwrap();
        assert!(!current.is_empty());
        let requested =
            chrono::DateTime::parse_from_rfc3339(current[0]["requested_at"].as_str().unwrap())
                .unwrap()
                .timestamp_millis() as u128;
        assert!(
            client_attention_resources_at(&state.store, Some("person/ada"), false, requested - 1)
                .unwrap()
                .is_empty()
        );
        assert!(
            !client_attention_resources_at(&state.store, Some("person/ada"), false, requested)
                .unwrap()
                .is_empty()
        );
        let step = &run.steps[0];
        {
            let connection = state.store.connection.lock().unwrap();
            connection.execute("UPDATE step_runs SET status='claimed',lease_owner='agent/clock',lease_incarnation='clock',lease_expires_at_unix_ms=30000 WHERE subject=?1", [&step.subject]).unwrap();
        }
        let before = state
            .store
            .mission_step_preview_at(&run.subject, 29_999)
            .unwrap()
            .2;
        let after = state
            .store
            .mission_step_preview_at(&run.subject, 30_000)
            .unwrap()
            .2;
        assert_eq!(before[0].status, "claimed");
        assert_eq!(after[0].status, "ready");
        assert!(after[0].claimant.is_none());
        assert!(after[0].claim_expires_at_unix_ms.is_none());
        assert_eq!(after[0].readiness_epoch, before[0].readiness_epoch + 1);
    }

    /// A disposable local cost comparison, explicitly invoked by the author. This does
    /// not run against the daemon or measure production paired-client CPU or tail latency.
    #[tokio::test]
    #[ignore = "local single-client cold-window cost receipt"]
    async fn shared_windows_local_single_client_cost() {
        fn cpu_ms() -> f64 {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
                0
            );
            time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0
        }
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for n in 0..100 {
            state.store.append_claim(&ClaimInput {
                subject: format!("glass/person/ada/019a0000-0000-7000-8000-{n:012x}"),
                kind: "glass.upserted".into(), actor: Some("person/ada".into()),
                fields: serde_json::from_value(json!({"body":{"name":format!("glass/{n}"),"layout":{"tabs":[{"pane":format!("opaque:{}", "x".repeat(1024))}]}},"base_revision":null})).unwrap(),
                evidence: vec![], expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
        let grant = state.store.append_claim(&ClaimInput {
            subject: "custom/client/cost-window-reader".into(), kind: "custom.client.pairing-completed".into(), actor: Some("person/ada".into()),
            fields: serde_json::from_value(json!({"session_actor":"client/cost-window-reader", "person_id":"person/ada", "expires_at_unix_ms":(client_now_ms()+600_000) as u64,"scopes":["read.projections","read.glasses"]})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let paired = paired_client_session(&state, &grant, "fabric-loopback", false).unwrap();
        let local = ClientSession::local(Some("person/ada")).unwrap();
        let windows = Windows::attach(&state.store).unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let mut query = request("glasses");
        query.limit = Some(200);
        let index = state.store.index().unwrap();
        for (transport, session) in [("unix", local), ("paired", paired)] {
            let mut receipt = serde_json::Map::new();
            let mut expected = None;
            for _ in 0..10 {
                collection_items_with_windows(
                    &state,
                    &session,
                    &query,
                    slots.clone().acquire_owned().await.unwrap(),
                    None,
                )
                .await
                .unwrap();
            }
            // Alternate order across blocks to reduce simple warmup/order bias.
            for block in 0..6 {
                let modes = if block % 2 == 0 {
                    ["uncached", "cold", "warm"]
                } else {
                    ["warm", "cold", "uncached"]
                };
                for mode in modes {
                    let mut wall = 0.0;
                    let mut cpu = 0.0;
                    for _ in 0..20 {
                        if mode == "cold" {
                            windows.entries.lock().unwrap().clear();
                        }
                        let start_cpu = cpu_ms();
                        let start = std::time::Instant::now();
                        let result = collection_items_with_windows(
                            &state,
                            &session,
                            &query,
                            slots.clone().acquire_owned().await.unwrap(),
                            (mode != "uncached").then(|| windows.clone()),
                        )
                        .await
                        .unwrap();
                        wall += start.elapsed().as_secs_f64() * 1000.0;
                        cpu += cpu_ms() - start_cpu;
                        assert_eq!(result.0.store_index, index);
                        assert_eq!(result.1.len(), 100);
                        if let Some(expected) = &expected {
                            assert_eq!(&result.1, expected);
                        } else {
                            expected = Some(result.1);
                        }
                    }
                    let totals = receipt
                        .entry(mode.to_string())
                        .or_insert(json!({"samples":0,"wall_ms":0.0,"process_cpu_ms":0.0}));
                    totals["samples"] = json!(totals["samples"].as_u64().unwrap() + 20);
                    totals["wall_ms"] = json!(totals["wall_ms"].as_f64().unwrap() + wall);
                    totals["process_cpu_ms"] =
                        json!(totals["process_cpu_ms"].as_f64().unwrap() + cpu);
                }
            }
            println!(
                "LOCAL_WINDOW_COST {}",
                json!({"transport":transport,"rows":100,"response_bytes":serde_json::to_vec(&expected).unwrap().len(),"profile":"debug","cases":receipt})
            );
        }
    }

    #[tokio::test]
    async fn shared_windows_recheck_pairing_revocation_scope_and_expiry_before_cached_rows() {
        for (collection, change) in [
            ("missions", "revoked"), ("missions", "scopes"), ("missions", "expired"),
            ("attention", "revoked"), ("attention", "scopes"), ("attention", "expired"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let state = state(root.path());
            let grant = "custom/client/window-reader";
            let fields = |expires: u128, scopes: Value| {
                serde_json::from_value(json!({
                    "session_actor":"client/window-reader", "person_id":"person/ada",
                    "expires_at_unix_ms":expires as u64, "scopes":scopes,
                }))
                .unwrap()
            };
            let paired = state
                .store
                .append_claim(&ClaimInput {
                    subject: grant.into(),
                    kind: "custom.client.pairing-completed".into(),
                    actor: Some("person/ada".into()),
                    fields: fields(client_now_ms() + 60_000, json!(["read.projections"])),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let session = paired_client_session(&state, &paired, "fabric-loopback", false).unwrap();
            let windows = Windows::attach(&state.store).unwrap();
            let slots = Arc::new(tokio::sync::Semaphore::new(1));
            let query = request(collection);
            let first = collection_items_with_windows(
                &state,
                &session,
                &query,
                slots.clone().acquire_owned().await.unwrap(),
                Some(windows.clone()),
            )
            .await
            .unwrap();
            assert!(first.1.is_empty());
            assert_eq!(windows.entries.lock().unwrap().len(), 1);
            if change == "revoked" {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: grant.into(),
                        kind: "custom.client.pairing-revoked".into(),
                        actor: Some("person/ada".into()),
                        fields: BTreeMap::new(),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
            } else {
                // Deterministic expiry/scope change at the existing grant. This also exercises
                // a local mutation with no new claim index, unlike a second pairing identity.
                let connection = state.store.connection.lock().unwrap();
                let value = if change == "expired" {
                    json!(0)
                } else {
                    json!([])
                };
                let path = if change == "expired" {
                    "$.fields.expires_at_unix_ms"
                } else {
                    "$.fields.scopes"
                };
                connection
                    .execute(
                        "UPDATE claims SET body=json_set(body,?1,json(?2)) WHERE id=?3",
                        rusqlite::params![path, value.to_string(), paired.id],
                    )
                    .unwrap();
            }
            let error = collection_items_with_windows(
                &state,
                &session,
                &query,
                slots.clone().acquire_owned().await.unwrap(),
                Some(windows),
            )
            .await
            .unwrap_err();
            assert_eq!(error.status, StatusCode::FORBIDDEN, "{change}: {error:?}");
            assert_eq!(error.code, "forbidden");
        }
    }

    #[tokio::test]
    async fn shared_windows_socket_snapshots_share_rows_but_reconnect_uses_current_fence() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let weak = Arc::downgrade(&windows);
        let route_state = state.clone();
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let state = route_state.clone();
                async move {
                    upgrade.on_upgrade(move |socket| {
                        let windows = Windows::attach(&state.store);
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            ClientSession::local(Some("person/ada")).unwrap(),
                            None,
                            move |state, session, request, permit| {
                                let windows = windows.clone();
                                async move {
                                    collection_items_with_windows(
                                        &state, &session, &request, permit, windows,
                                    )
                                    .await
                                }
                            },
                        )
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        let (mut second, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        for (socket, id) in [(&mut first, "first"), (&mut second, "second")] {
            socket
                .send(Message::Text(
                    json!({"kind":"subscribe", "id":id, "collection":"glasses", "limit":2})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(frame["kind"], "snapshot");
            assert_eq!(frame["id"], id);
        }
        assert_eq!(windows.builds(), 1);
        diagnostic(&state);
        let index = state.store.index().unwrap();
        signal_visible_change(&state);
        // Reconnect sends its own authoritative snapshot even while rows remain reusable.
        second.close(None).await.unwrap();
        let (mut reconnect, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        reconnect
            .send(Message::Text(
                json!({"kind":"subscribe", "id":"reconnect", "collection":"glasses", "limit":2})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), reconnect.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(frame["kind"], "snapshot");
        assert_eq!(frame["snapshot"]["store_index"], index);
        assert_eq!(windows.builds(), 1);
        first
            .send(Message::Text(
                json!({"kind":"unsubscribe", "id":"first"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        first.close(None).await.unwrap();
        reconnect.close(None).await.unwrap();
        server.abort();
        drop(windows);
        tokio::time::timeout(Duration::from_secs(5), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed sockets release shared windows and the observer");
    }
    fn mission_attention_fixture(state: &AppState, count: usize) {
        let source = format!("version 2\n{}", (0..count).map(|n| format!(
            "mission \"shared-{n:03}\" state=\"ready\" {{ goal \"Review fixture\"; step \"review\" {{ assigned-to \"person/ada\"; goal \"Review\"; }} }}\n"
        )).collect::<String>());
        let intent = crate::graph::parse_internal_intent(&source, state.store.origin()).unwrap();
        state.store.apply_internal(&intent, "shared-projection-fixture").unwrap();
        for n in 0..count {
            let run = state.store.create_mission_run(&crate::model::MissionRunRequest {
                mission:format!("shared-{n:03}"), revision:None, workspace:"/tmp".into(),
                requester:Some("person/ada".into()), mode:Some("run".into()),
                inputs:BTreeMap::new(), idempotency_key:format!("shared-run-{n}"),
            }).unwrap();
            state.store.set_step_state(&run.steps[0].subject, "ready", None).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mission_attention_subscribers_share_projection_across_grants_and_limits_with_parity() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        mission_attention_fixture(&state, 8);
        let windows = Windows::attach(&state.store).unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(32));
        for collection in ["missions", "attention"] {
            let before = windows.builds();
            let mut readers = tokio::task::JoinSet::new();
            let barrier = Arc::new(tokio::sync::Barrier::new(22));
            for n in 0..22 {
                let state = state.clone();
                let windows = windows.clone();
                let slots = slots.clone();
                let barrier = barrier.clone();
                readers.spawn(async move {
                    let mut session = ClientSession::local(Some("person/ada")).unwrap();
                    // Independently authenticated subscribers with identical visible data.
                    session.actor = format!("client/{n}");
                    session.pairing_grant = Some(format!("custom/client/{n}"));
                    let mut query = request(collection);
                    query.limit = Some(n % 8 + 1);
                    barrier.wait().await;
                    let (_, items, more) = collection_items_with_windows(&state, &session, &query,
                        slots.clone().acquire_owned().await.unwrap(), Some(windows)).await.unwrap();
                    let (_, oracle, oracle_more) = collection_items_with_windows(&state, &session, &query,
                        slots.acquire_owned().await.unwrap(), None).await.unwrap();
                    assert_eq!(items, oracle, "{collection} subscriber {n}");
                    assert_eq!(more, oracle_more);
                });
            }
            while let Some(result) = readers.join_next().await { result.unwrap(); }
            assert_eq!(windows.builds() - before, 1, "{collection}");
        }
        let mut query = request("attention");
        let ada = ClientSession::local(Some("person/ada")).unwrap();
        let avery = ClientSession::local(Some("person/avery")).unwrap();
        let (_, items, _) = collection_items_with_windows(&state, &avery, &query,
            slots.clone().acquire_owned().await.unwrap(), Some(windows.clone())).await.unwrap();
        assert!(items.is_empty(), "another person must not receive Ada's rows");
        query.person = Some("person/ada".into());
        assert!(collection_items_with_windows(&state, &avery, &query,
            slots.clone().acquire_owned().await.unwrap(), Some(windows.clone())).await.is_err());
        query.person = None;
        let before = windows.builds();
        let mut compatibility = ada.clone();
        compatibility.custom_forms = !ada.custom_forms;
        collection_items_with_windows(&state, &compatibility, &query,
            slots.acquire_owned().await.unwrap(), Some(windows.clone())).await.unwrap();
        assert_eq!(windows.builds(), before + 1, "visibility/compatibility keys are separate");
    }

    #[test]
    #[ignore = "focused collection timing fixture; run explicitly with --ignored --nocapture"]
    fn mission_attention_projection_fixture_timing() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        mission_attention_fixture(&state, 70);
        let windows = Windows::attach(&state.store).unwrap();
        let session = ClientSession::local(Some("person/ada")).unwrap();
        let now = client_now_ms();
        for collection in ["missions", "attention"] {
            let query = request(collection);
            let projection = || -> anyhow::Result<(Vec<Value>, bool)> {
                let mut items = if collection == "missions" {
                    let ids = state.store.mission_collection_ids(false, 0, CLIENT_MAX_PAGE_ITEMS + 1)?;
                    mission_list_cards_at(&state.store, &ids, now)?
                } else {
                    client_attention_resources_at(&state.store, Some("person/ada"), false, now)?
                };
                client_attention_compatibility(&mut items, session.custom_forms);
                Ok((items, false))
            };
            let oracle = state.store.read_snapshot(|_| projection()).unwrap();
            let measure = |label:&str, f:&dyn Fn()| {
                let work = smallclaims::sqlite::work::total();
                let started = Instant::now();
                f();
                println!("{collection} fixture {label}: {:.3} ms; sqlite_work={:?}",
                    started.elapsed().as_secs_f64()*1000.0, smallclaims::sqlite::work::total()-work);
            };
            measure("uncached x22", &|| {
                for _ in 0..22 { assert_eq!(state.store.read_snapshot(|_| projection()).unwrap(), oracle); }
            });
            let cached = || {
                let prepared = windows.prepare(&state, &session, &query);
                let _admission = prepared.as_ref().map(|p| p.entry.admission.clone().blocking_lock_owned());
                let commits = windows.commits();
                state.store.read_snapshot(|index| windows.read(&state, &session, &query,
                    ReadFence { index, now, commits, prepared }, || {
                        let (mut items, until) = if collection == "missions" {
                            let ids = state.store.mission_collection_ids(false, 0, CLIENT_MAX_PAGE_ITEMS + 1)?;
                            (mission_list_cards_at(&state.store, &ids, now)?,
                                state.store.mission_collection_valid_until(&ids, now)?)
                        } else {
                            client_attention_resources_fenced_at(&state.store, Some("person/ada"), now)?
                        };
                        client_attention_compatibility(&mut items, session.custom_forms);
                        Ok((items, false, until))
                    })).unwrap()
            };
            measure("shared cold", &|| { assert_eq!(cached(), oracle); });
            measure("shared warm x22", &|| {
                for _ in 0..22 { assert_eq!(cached(), oracle); }
            });
        }
        println!("fixture stage profile: {}", crate::performance::snapshot());
    }
    #[test]
    fn mission_attention_invalidation_covers_projection_claim_families() {
        for collection in ["missions", "attention"] {
            for kind in st3_schema::registry().claims.keys() {
                let telemetry = matches!(kind.as_str(),
                    "daemon.diagnostic" | "transport.observed" | "workspace.observed")
                    || (collection == "missions" && matches!(kind.as_str(),
                        "harness.observed" | "harness.diagnostic" | "harness.timeline" | "harness.usage"
                            | "harness.todo.observed" | "harness.session-file"))
                    || (collection == "attention" && matches!(kind.as_str(),
                        "harness.timeline" | "harness.usage" | "harness.todo.observed" | "harness.session-file"));
                assert_eq!(collection_ignores(collection, kind), telemetry, "{collection}: {kind}");
            }
            assert!(!collection_ignores(collection, "unknown.future-projection-input"));
            assert!(!collection_ignores(collection, "harness.future-projection-input"));
        }
        for kind in ["harness.observed", "harness.diagnostic", "harness.timeline",
            "harness.usage", "harness.todo.observed", "harness.session-file"] {
            assert!(collection_ignores("missions", kind), "{kind}");
        }
        for kind in ["harness.observed", "harness.diagnostic"] {
            assert!(!collection_ignores("attention", kind), "login input {kind}");
        }
        for kind in ["harness.timeline", "harness.usage", "harness.todo.observed", "harness.session-file"] {
            assert!(collection_ignores("attention", kind), "{kind}");
        }
    }
    #[test]
    fn mission_attention_cache_expires_at_exact_transition_and_rejects_backward_clock() {
        for collection in ["missions", "attention"] {
            let root = tempfile::tempdir().unwrap();
            let state = state(root.path());
            mission_attention_fixture(&state, 1);
            let overview = state.store.mission_overview("shared-000", 3).unwrap();
            let run = overview["newest"][0]["id"].as_str().unwrap();
            let step = state.store.mission_step_preview(run).unwrap().2[0].subject.clone();
            {
                let writer = state.store.connection.lock().unwrap();
                if collection == "missions" {
                    writer.execute("UPDATE step_runs SET status='claimed',lease_owner='agent/clock',lease_incarnation='clock',lease_expires_at_unix_ms='30000' WHERE subject=?1", [&step]).unwrap();
                } else {
                    writer.execute("UPDATE step_runs SET activated_at_unix_ms='30000' WHERE subject=?1", [&step]).unwrap();
                }
            }
            let windows = Windows::attach(&state.store).unwrap();
            let session = ClientSession::local(Some("person/ada")).unwrap();
            let query = request(collection);
            let projection = |now| {
                if collection == "missions" {
                    mission_list_cards_at(&state.store, &["mission/shared-000".into()], now)
                } else {
                    client_attention_resources_at(&state.store, Some("person/ada"), false, now)
                }
            };
            let cached = |now| {
                let prepared = windows.prepare(&state, &session, &query);
                let _admission = prepared.as_ref().map(|p| p.entry.admission.clone().blocking_lock_owned());
                let commits = windows.commits();
                state.store.read_snapshot(|index| windows.read(&state, &session, &query,
                    ReadFence { index, now, commits, prepared }, || {
                        let (items, until) = if collection == "missions" {
                            (projection(now)?, state.store.mission_collection_valid_until(
                                &["mission/shared-000".into()], now)?)
                        } else {
                            client_attention_resources_fenced_at(&state.store, Some("person/ada"), now)?
                        };
                        Ok((items, false, until))
                    })).unwrap().0
            };
            let before = cached(29_999);
            assert_eq!(before, projection(29_999).unwrap());
            assert_eq!(cached(29_999), before);
            assert_eq!(windows.builds(), 1, "{collection}");
            let after = cached(30_000);
            assert_eq!(after, projection(30_000).unwrap());
            assert_ne!(after, before, "{collection} changes at the fence, not the next timer bucket");
            assert_eq!(windows.builds(), 2, "{collection}");
            assert_eq!(cached(29_999), before, "backward clock must not reuse future rows");
            assert_eq!(windows.builds(), 3, "{collection}");
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mission_attention_twenty_two_websockets_build_once_per_visible_cut() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        mission_attention_fixture(&state, 8);
        let windows = Windows::attach(&state.store).unwrap();
        let app = axum::Router::new()
            .route("/stream", axum::routing::get(collection_stream))
            .with_state(state.clone())
            .layer(Extension(ClientSession::local(Some("person/ada")).unwrap()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        for collection in ["missions", "attention"] {
            let before = windows.builds();
            let barrier = Arc::new(tokio::sync::Barrier::new(22));
            let mut readers = tokio::task::JoinSet::new();
            for n in 0..22 {
                let barrier = barrier.clone();
                readers.spawn(async move {
                    let mut handshake = format!("ws://{address}/stream").into_client_request().unwrap();
                    handshake.headers_mut().insert("sec-websocket-protocol",
                        COLLECTION_SUBPROTOCOL.parse().unwrap());
                    let (mut socket, _) = tokio_tungstenite::connect_async(handshake).await.unwrap();
                    barrier.wait().await;
                    socket.send(Message::Text(json!({"kind":"subscribe", "id":"visible",
                        "collection":collection, "limit":n%8+1}).to_string().into())).await.unwrap();
                    let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
                        .await.unwrap().unwrap().unwrap();
                    let frame: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                    assert_eq!(frame["kind"], "snapshot", "{frame}");
                    assert_eq!(frame["items"].as_array().unwrap().len(), n%8+1);
                    socket
                });
            }
            let mut sockets = Vec::new();
            while let Some(result) = readers.join_next().await { sockets.push(result.unwrap()); }
            assert_eq!(windows.builds()-before, 1, "{collection}");
            for mut socket in sockets { socket.close(None).await.unwrap(); }
        }
        server.abort();
    }
}
