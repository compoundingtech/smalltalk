//! Shared bounded windows, never authority. The existing socket feed still publishes frames.
//! A future graph-watch consumer can replace `revision` with view/frontier invalidations;
//! authorized row reads and socket snapshot/diff delivery remain the same.
use super::*;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};

const STORES: usize = 64;
const WINDOWS: usize = 64;
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
    values: [u64; 6],
}

struct Cached {
    revision: u64,
    period: u128,
    items: Vec<Value>,
    has_more: bool,
}

type WindowEntry = Arc<Mutex<Option<Cached>>>;
type WindowEntries = VecDeque<(String, WindowEntry)>;

pub(super) struct ReadFence {
    pub(super) index: u64,
    pub(super) now: u128,
    pub(super) commits: u64,
}

/// Held by all sockets using this Store, and by physical workers until they finish.
/// The registry and commit callback keep only weak references. No follower task is started.
pub(super) struct Windows {
    commits: AtomicU64,
    revisions: Mutex<Revisions>,
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
            entries: Mutex::new(VecDeque::new()),
            observer: Mutex::new(None),
            #[cfg(test)]
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&windows);
        // Register before any snapshot. The callback does no SQL or publication; a shared
        // snapshot reader weighs bounded kind metadata once for all equivalent sockets.
        let observer = store.observe_commits(move |_| {
            if let Some(windows) = weak.upgrade() {
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

    fn entry(&self, key: String) -> Option<WindowEntry> {
        // Inputs are untrusted; do not retain oversized query/authority keys.
        if key.len() > 4096 {
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = entries.iter().position(|(held, _)| *held == key) {
            let entry = entries.remove(position).expect("found entry");
            let result = entry.1.clone();
            entries.push_back(entry);
            return Some(result);
        }
        if entries.len() >= WINDOWS {
            // Never evict a computation another socket is using: that would duplicate it.
            let position = entries
                .iter()
                .position(|(_, entry)| Arc::strong_count(entry) == 1)?;
            entries.remove(position);
        }
        let entry = Arc::new(Mutex::new(None));
        entries.push_back((key, entry.clone()));
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
        let commits = self.commits.load(Ordering::Acquire);
        // A slower physical reader can hold an older SQLite snapshot. It cannot update the
        // shared frontier or reuse rows from a newer one.
        if index < revisions.index
            || commits != snapshot_commits
            || revisions.commits > snapshot_commits
        {
            return Ok(None);
        }
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
            let all = index == revisions.index || kinds.is_empty() || kinds.len() > KIND_LIMIT;
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

    /// Called inside the authorized SQLite snapshot. Serializes only equivalent keys;
    /// neither registry nor revision locks are held during the expensive computation.
    pub(super) fn read(
        &self,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
        fence: ReadFence,
        compute: impl FnOnce() -> anyhow::Result<(Vec<Value>, bool)>,
    ) -> anyhow::Result<(Vec<Value>, bool)> {
        let ReadFence {
            index,
            now,
            commits: snapshot_commits,
        } = fence;
        #[cfg(test)]
        let compute = || {
            self.builds.fetch_add(1, Ordering::SeqCst);
            compute()
        };
        let key = json!({
            "node":state.node, "fleet":state.fleet_id,
            "actor":session.actor, "authority":session.authority_actor,
            "grant":session.pairing_grant, "transport":session.transport,
            "scopes":session.scopes, "custom_forms":session.custom_forms,
            "conversation_blocks":session.conversation_blocks,
            "collection":request.collection, "limit":request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS),
            "person":request.person, "subject":request.subject, "filter_actor":request.actor, "status":request.status,
        }).to_string();
        let Some(entry) = self.entry(key) else {
            return compute();
        };
        let mut cached = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A commit after this reader began can update local tables without advancing the
        // claim index. Such a snapshot cannot reuse/cache a newer local-state generation.
        if self.commits() != snapshot_commits {
            return compute();
        }
        let Some(revision) =
            self.revision(&state.store, index, &request.collection, snapshot_commits)?
        else {
            return compute();
        };
        // Attention and mission previews have wall-clock grace/expiry inputs. Agents receive live local overlays
        // below the cache. The period is shared across sockets, including their initial ticks.
        let period = match request.collection.as_str() {
            "attention" | "missions" => now / ATTENTION_CLOCK_INTERVAL.as_millis(),
            // Work's deterministic selected projection time is itself an input, even if a
            // claim kind does not otherwise change its rows (elapsed budgets/lease expiry).
            "work" => state.store.projection_time_at(index)?,
            _ => 0,
        };
        if let Some(cached) = &*cached
            && cached.revision == revision
            && cached.period == period
        {
            return Ok((cached.items.clone(), cached.has_more));
        }
        let (items, has_more) = compute()?;
        // At most 64 bounded responses per Store. Oversized results remain uncached.
        if serde_json::to_vec(&items)?.len() <= CLIENT_MAX_RESPONSE_BYTES {
            *cached = Some(Cached {
                revision,
                period,
                items: items.clone(),
                has_more,
            });
        } else {
            *cached = None;
        }
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
        let commits = windows.commits();
        state.store.read_snapshot(|index| {
            windows.read(state, session, request, ReadFence { index, now, commits }, || {
                let n = count.fetch_add(1, Ordering::SeqCst);
                Ok((vec![json!({"id":format!("row/{n}"), "authority":session.authority_actor})], false))
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
                        },
                        || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            entered.send(()).unwrap();
                            held.recv().unwrap();
                            Ok((vec![json!({"id":"shared"})], false))
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
        assert_ne!(
            read(&windows, &state, &session, &attention, 30_000, &count),
            first
        );
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn shared_windows_isolate_authority_grants_scopes_and_query_keys() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let windows = Windows::attach(&state.store).unwrap();
        let count = AtomicUsize::new(0);
        let session = ClientSession::local(Some("person/ada")).unwrap();
        let query = request("attention");
        let first = read(&windows, &state, &session, &query, 0, &count);
        let mut same = query.clone();
        same.id = "another-socket-id".into();
        assert_eq!(read(&windows, &state, &session, &same, 0, &count), first);
        let mut variants = vec![ClientSession::local(Some("person/bob")).unwrap()];
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
                let rows = windows
                    .read(
                        &state,
                        &session,
                        &query,
                        ReadFence {
                            index,
                            now: 0,
                            commits: old_commits,
                        },
                        || {
                            count.fetch_add(1, Ordering::SeqCst);
                            Ok((vec![json!({"id":"old-snapshot"})], false))
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
            let mut query = request("missions");
            query.limit = Some(limit);
            read(&windows, &state, &session, &query, 0, &count);
        }
        assert_eq!(windows.entries.lock().unwrap().len(), WINDOWS);
        let query = request("attention");
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
                },
                || anyhow::bail!("fixture read failure"),
            )
        });
        assert!(result.is_err());
        read(&windows, &state, &session, &query, 0, &count);
        assert_eq!(count.load(Ordering::SeqCst), WINDOWS + 3);
    }

    #[tokio::test]
    async fn shared_windows_recheck_pairing_revocation_scope_and_expiry_before_cached_rows() {
        for change in ["revoked", "scopes", "expired"] {
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
            let query = request("missions");
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
}
