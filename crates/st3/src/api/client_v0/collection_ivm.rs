//! Current collection cursors. The source adapter owns complete keyed dependency coverage,
//! authorized bounded ranking and row reads; this consumer owns delivery acknowledgment.
//! Registration alone does not select a production adapter.
use super::*;
use crate::graph_watch_ivm::{IvmViewBridge, ViewPage};
use anyhow::Context as _;
use smallclaims::ivm::{Readiness, events};
use std::cell::RefCell;

mod agents;

type Window = (ClientSnapshot, Vec<Value>, bool);
type RowReader = dyn Fn(
        &AppState,
        &ClientSession,
        &CollectionSubscribe,
        &rusqlite::Connection,
        &events::Boundary,
        &[events::ViewChanged],
        &BTreeMap<String, Value>,
    ) -> Result<(Vec<Value>, bool), ApiError>
    + Send
    + Sync;

type CoverageReader = dyn Fn(&rusqlite::Connection) -> anyhow::Result<bool> + Send + Sync;

pub(super) struct Adapter {
    // Mandatory independent source coverage check even when an unchanged key page skips rows.
    pub(super) coverage: Arc<CoverageReader>,
    pub(super) view: &'static str,
    // Keys are hints, never proof that other retained rows are current. The adapter
    // selects bounded authorized IDs first and verifies every reused row's generation
    // at this boundary, then fetches only changed/new rows. Full window folding is forbidden.
    pub(super) rows: Arc<RowReader>,
}

pub(super) struct Sources {
    store: Arc<Store>,
    bridge: Arc<IvmViewBridge<Store>>,
    adapters: BTreeMap<String, Arc<Adapter>>,
}

impl Sources {
    /// Called on a blocking worker after the explicit Store installer. Receipt waits and
    /// sockets obtain the same runtime-held Arc; this path never attaches a publisher.
    pub(super) fn from_store(
        store: Arc<Store>,
        adapters: BTreeMap<String, Arc<Adapter>>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        if adapters.is_empty() {
            return Ok(None);
        }
        let Some(views) = store.ivm_views() else {
            return Ok(None);
        };
        let publisher = store
            .ivm_publisher()?
            .ok_or_else(|| anyhow::anyhow!("registered collection source has no publisher"))?;
        Ok(Some(Arc::new(Self {
            store: store.clone(),
            bridge: Arc::new(IvmViewBridge::from_shared(store, views, publisher)),
            adapters,
        })))
    }

    pub(super) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<events::Notice> {
        self.bridge.subscribe()
    }

    pub(super) fn adapter(&self, collection: &str) -> Option<Arc<Adapter>> {
        self.adapters.get(collection).cloned()
    }
}

/// Only the explicitly attached complete source selects the agents provider. Other collection
/// families continue through their existing adapters until their own full source is certified.
pub(super) fn adapters(store: Arc<Store>) -> anyhow::Result<BTreeMap<String, Arc<Adapter>>> {
    if !store.has_agent_collection_source() {
        return Ok(BTreeMap::new());
    }
    let views = store.ivm_views().context("agent source registry missing")?;
    let installer = store
        .ivm_installer()
        .context("agent source Installer missing")?;
    let mut adapter = agents::factory(store.clone(), views, installer)?;
    let coverage = adapter.coverage.clone();
    let receiver = store.origin().to_string();
    adapter.coverage = Arc::new(move |connection| {
        if !crate::store::collection_ivm::agent_source::receiver_readable(connection, &receiver)? {
            return Ok(false);
        }
        coverage(connection)
    });
    Ok(BTreeMap::from([("agents".into(), Arc::new(adapter))]))
}

#[derive(Clone)]
pub(super) struct Delivered {
    boundary: events::Boundary,
    authority: String,
    continuation: Option<events::SnapshotVersion>,
}

#[derive(Default)]
pub(super) struct Held {
    pub(super) cursor: Option<Delivered>,
    pub(super) rows: Arc<BTreeMap<String, Value>>,
}

pub(super) enum Output {
    Window(Window),
    /// Keep the last delivered rows stale until Ready or a client resubscription.
    Unavailable,
    /// A delivered resync must be followed by a new authoritative snapshot.
    Resync,
    /// No visible change, or a continuation whose page version changed.
    Silent,
}

pub(super) struct Candidate {
    pub(super) output: Output,
    pub(super) delivered: Option<Delivered>,
    pub(super) again: bool,
    pub(super) replace: bool,
}

fn authority(session: &ClientSession) -> String {
    json!({"actor":session.actor,"authority":session.authority_actor,
        "grant":session.pairing_grant,"scopes":session.scopes,
        "custom_forms":session.custom_forms,"conversation_blocks":session.conversation_blocks,"transport":session.transport})
    .to_string()
}

fn authorize(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
) -> Result<ClientSession, ApiError> {
    let current = if session.transport == "unix" {
        session.clone()
    } else {
        revalidate_session(state, session)?
    };
    require_scope(&current, "read.projections")?;
    let limit = request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS);
    if !(1..=CLIENT_MAX_PAGE_ITEMS).contains(&limit) {
        return Err(validation("collection limit must be 1 through 200"));
    }
    if request.collection == "attention" {
        person_filter(&current, request.person.as_deref())?;
    }
    if request.status.is_some() && request.collection != "agents" {
        return Err(validation("status filters are supported for agents only"));
    }
    if request.subject.is_some() {
        return Err(validation(
            "subject filters are supported for arrangements only",
        ));
    }
    Ok(current)
}

/// Read a candidate without changing any delivered cursor. The socket applies Candidate
/// only after sending its snapshot/changes/resync successfully (or proving silence).
pub(super) async fn read(
    state: AppState,
    session: ClientSession,
    request: CollectionSubscribe,
    permit: tokio::sync::OwnedSemaphorePermit,
    sources: Arc<Sources>,
    adapter: Arc<Adapter>,
    held: Held,
) -> Result<Candidate, ApiError> {
    if !Arc::ptr_eq(&state.store, &sources.store) {
        return Err(ApiError::internal(anyhow::anyhow!(
            "collection source belongs to another Store"
        )));
    }
    blocking_store(move || {
        let Held { cursor: previous, rows: previous_rows } = held;
        let _permit = permit;
        let current = RefCell::new(None);
        let refusal = RefCell::new(None);
        let check = |connection: &rusqlite::Connection| match authorize(&state, &session, &request) {
            Ok(session) => {
                *current.borrow_mut() = Some(session);
                // Check even silent key pages; a missed durable source write cannot be
                // acknowledged merely because the registry still reports Ready.
                let coverage = (adapter.coverage)(connection)?;
                let index = smallclaims::store::current_index(connection)?;
                let cut = smallclaims::ivm::source_cut(connection)?;
                if !coverage || !cut.is_some_and(|cut| cut.admitted == index) {
                    let views = state.store.ivm_views().ok_or_else(|| anyhow::anyhow!("collection registry is missing"))?;
                    let boundary = events::capture(connection, &views, adapter.view)?;
                    // A fenced registry may still deliver/acknowledge its availability page.
                    // Ready cannot mask an uncaptured durable/local write, even on silence.
                    anyhow::ensure!(!matches!(boundary.availability.readiness, Readiness::Ready(_)),
                        "collection source capture or admission coverage is pending");
                }
                Ok(())
            }
            Err(error) => {
                *refusal.borrow_mut() = Some(error);
                anyhow::bail!("collection authorization refused")
            }
        };
        let rows = |connection: &rusqlite::Connection,
                    boundary: &events::Boundary,
                    keys: &[events::ViewChanged]| {
            let held = current.borrow();
            let session = held.as_ref().expect("authorization precedes row reads");
            // Registration cannot certify writes omitted from the source dispatch.
            let index = smallclaims::store::current_index(connection)?;
            anyhow::ensure!(boundary.source_cut.projected == index
                && boundary.source_cut.admitted == index, "collection source prefix is pending");
            let empty = BTreeMap::new();
            let reusable = previous.as_ref().is_some_and(|previous|
                previous.authority == authority(session)
                && matches!(previous.boundary.availability.readiness, Readiness::Ready(_))
                && previous.boundary.identity.fingerprint == boundary.identity.fingerprint
                && previous.boundary.identity.epoch == boundary.identity.epoch
                && (!keys.is_empty()
                    || (previous.boundary.snapshot.semantic_generation == boundary.snapshot.semantic_generation
                        && previous.boundary.snapshot.availability.view_sequence == boundary.snapshot.availability.view_sequence)))
                && keys.len() < 256;
            let retained = if reusable { previous_rows.as_ref() } else { &empty };
            match (adapter.rows)(&state, session, &request, connection, boundary, keys, retained) {
                Ok((items, has_more)) => Ok((
                    client_snapshot_at(&state, boundary.source_cut.projected),
                    items,
                    has_more,
                )),
                Err(error) => {
                    *refusal.borrow_mut() = Some(error);
                    anyhow::bail!("collection row read refused")
                }
            }
        };
        let result = if let Some(previous) = &previous {
            sources
                .bridge
                .page_with_rows(
                    &previous.boundary.keys,
                    &previous.boundary.status,
                    256,
                    previous.continuation.as_ref(),
                    check,
                    |connection, boundary, keys, _| {
                        let auth =
                            authority(current.borrow().as_ref().expect("authorized session"));
                        // Shared source-cut notices on unrelated writes advance cursors without
                        // reading rows. A Ready transition or authority change still refreshes.
                        let changed = !keys.is_empty()
                            || boundary.snapshot.semantic_generation != previous.boundary.snapshot.semantic_generation
                            || boundary.snapshot.availability.view_sequence != previous.boundary.snapshot.availability.view_sequence
                            || !matches!(
                                previous.boundary.availability.readiness,
                                Readiness::Ready(_)
                            )
                            || previous.authority != auth;
                        changed
                            .then(|| rows(connection, boundary, keys))
                            .transpose()
                    },
                )
                .map(|page| match page {
                    ViewPage::Changes {
                        output,
                        next_keys,
                        next_status,
                        more,
                        mut boundary,
                        has_availability_change,
                    } => {
                        let ready = matches!(boundary.availability.readiness, Readiness::Ready(_));
                        let continuation = more.then(|| boundary.snapshot.clone());
                        boundary.keys = next_keys;
                        boundary.status = next_status;
                        Candidate {
                            output: if !ready && (has_availability_change
                                || matches!(previous.boundary.availability.readiness, Readiness::Ready(_))) {
                                Output::Unavailable
                            } else if !ready {
                                Output::Silent
                            } else if let Some(window) = output.flatten() {
                                Output::Window(window)
                            } else {
                                Output::Silent
                            },
                            delivered: Some(Delivered {
                                boundary,
                                authority: authority(
                                    current.borrow().as_ref().expect("authorized session"),
                                ),
                                continuation,
                            }),
                            again: more,
                            replace: false,
                        }
                    }
                    ViewPage::Resync { reason, boundary } => {
                        tracing::debug!(?reason, identity=?boundary.identity, "collection journal gap");
                        Candidate {
                        output: Output::Resync,
                        delivered: None,
                        again: true,
                        replace: true,
                    }},
                    ViewPage::SnapshotChanged { boundary } => {
                        tracing::debug!(snapshot=?boundary.snapshot, "collection continuation changed");
                        let mut delivered = previous.clone();
                        delivered.continuation = None;
                        Candidate {
                            output: Output::Silent,
                            delivered: Some(delivered),
                            again: true,
                            replace: false,
                        }
                    }
                })
        } else {
            sources
                .bridge
                .subscribe_snapshot(adapter.view, check, |connection, boundary| {
                    rows(connection, boundary, &[])
                })
                .map(|open| {
                    // The socket owns an earlier receiver; this per-capture subscription is
                    // redundant for it but preserves the standalone bridge ordering contract.
                    drop(open.notices);
                    Candidate {
                    output: open.snapshot.map_or(Output::Unavailable, Output::Window),
                    delivered: Some(Delivered {
                        boundary: open.boundary,
                        authority: authority(
                            current.borrow().as_ref().expect("authorized session"),
                        ),
                        continuation: None,
                    }),
                    again: false,
                    replace: true,
                }})
        };
        Ok(result.map_err(|error| {
            refusal
                .into_inner()
                .unwrap_or_else(|| ApiError::internal(error))
        }))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use smallclaims::ivm::{Definition, LocalChange, View, Views};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;

    // Complete local-only transport fixture. No production source certificate is asserted.
    struct FixtureView;
    impl View for FixtureView {
        fn definition(&self) -> Definition {
            Definition {
                name: "fixture.socket",
                fingerprint: "fixture.socket.v1",
                kinds: &[],
                local_kinds: &["fixture.replace"],
                max_contributions: 1,
            }
        }
        fn contributions(
            &self,
            _: &smallclaims::ClaimRecord,
            _: &smallclaims::store::canonical::ClaimKey,
        ) -> anyhow::Result<Vec<smallclaims::ivm::Contribution>> {
            Ok(vec![])
        }
        fn create_schema(&self, connection: &rusqlite::Connection) -> anyhow::Result<()> {
            connection.execute_batch("CREATE TABLE IF NOT EXISTS fixture_rows(id TEXT PRIMARY KEY, owner TEXT NOT NULL, rank INTEGER NOT NULL, value TEXT NOT NULL); CREATE INDEX IF NOT EXISTS fixture_rows_rank ON fixture_rows(owner,rank,id); CREATE TABLE IF NOT EXISTS fixture_output(id TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
            Ok(())
        }
        fn maintain_local_key(
            &self,
            tx: &rusqlite::Transaction<'_>,
            key: &str,
            _: &LocalChange,
        ) -> anyhow::Result<bool> {
            use rusqlite::OptionalExtension;
            let old: Option<String> = tx
                .query_row(
                    "SELECT value FROM fixture_output WHERE id=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            let new: Option<String> = tx
                .query_row(
                    "SELECT json_array(owner,rank,value) FROM fixture_rows WHERE id=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(value) = &new {
                tx.execute("INSERT INTO fixture_output VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value", rusqlite::params![key,value])?;
            } else {
                tx.execute("DELETE FROM fixture_output WHERE id=?1", [key])?;
            }
            Ok(old != new)
        }
    }
    fn replace(store: &Store, views: &Views, id: &str, rank: i64, value: Option<&str>) {
        store.connection.batched(|tx| {
            if let Some(value) = value {
                tx.execute("INSERT INTO fixture_rows VALUES(?1,'person/avery',?2,?3) ON CONFLICT(id) DO UPDATE SET rank=excluded.rank,value=excluded.value", rusqlite::params![id,rank,value])?;
            } else { tx.execute("DELETE FROM fixture_rows WHERE id=?1", [id])?; }
            let mut cut = smallclaims::ivm::source_cut(tx)?.unwrap();
            cut.local_generation += 1;
            views.local_change(tx, &LocalChange {kind:"fixture.replace".into(),old_keys:BTreeSet::from([id.into()]),new_keys:BTreeSet::from([id.into()]),evaluation_time_unix_ms:0}, cut)?;
            Ok::<_, anyhow::Error>(())
        }).unwrap().unwrap();
    }
    async fn frame(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) => socket.flush().await.unwrap(),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }
    #[test]
    fn non_unix_held_scope_cannot_bypass_current_grant_revalidation() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_named(root.path(), "alder");
        let mut session = ClientSession::local(None).unwrap();
        session.transport = "fabric";
        let request: CollectionSubscribe = serde_json::from_value(json!({
            "kind":"subscribe", "id":"agents", "collection":"agents"
        })).unwrap();
        let error = authorize(&state, &session, &request).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        assert!(error.message.contains("grant is absent"));
    }

    #[tokio::test]
    async fn ivm_socket_upsert_remove_reorder_silence_resync_and_reconnect() {
        let root = tempfile::tempdir().unwrap();
        let views = Arc::new(Views::new(vec![Box::new(FixtureView)]).unwrap());
        let mut state = super::super::tests::test_state_named(root.path(), "alder");
        state.store = Arc::new(
            Store::open_with_ivm_views(&root.path().join("ivm.db"), "alder", views.clone())
                .unwrap(),
        );
        let store = state.store.clone();

        replace(&store, &views, "row/a", 1, Some("a1"));
        replace(&store, &views, "row/b", 2, Some("b1"));
        let rows = Arc::new(AtomicUsize::new(0));
        let observed = rows.clone();
        let retained_counts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let retained = retained_counts.clone();
        let acknowledged = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let committed = acknowledged.clone();
        let adapter = Arc::new(Adapter {
            view: "fixture.socket",
            coverage: Arc::new(move |_| Ok(committed.load(Ordering::SeqCst))),
            rows: Arc::new(move |_, session, request, conn, _, _, previous| {
                retained.lock().unwrap().push(previous.len());
                rows.fetch_add(1, Ordering::SeqCst);
                let mut stmt=conn.prepare("SELECT id,value FROM fixture_rows WHERE owner=?1 ORDER BY rank,id LIMIT ?2").map_err(ApiError::internal)?;
                let items=stmt.query_map(rusqlite::params![session.authority_actor,request.limit.unwrap_or(2)+1],|row| Ok(json!({"id":row.get::<_,String>(0)?,"value":row.get::<_,String>(1)?}))).map_err(ApiError::internal)?.collect::<Result<Vec<_>,_>>().map_err(ApiError::internal)?;
                let has_more = items.len() > request.limit.unwrap_or(2);
                Ok((
                    items.into_iter().take(request.limit.unwrap_or(2)).collect(),
                    has_more,
                ))
            }),
        });
        let sources =
            Sources::from_store(store.clone(), BTreeMap::from([("work".into(), adapter)]))
                .unwrap()
                .unwrap();
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, sources) = (state.clone(), sources.clone());
                async move {
                    upgrade.on_upgrade(move |socket| {
                        collection_stream_socket_with_sources(
                            socket,
                            state,
                            ClientSession::local(Some("person/avery")).unwrap(),
                            None,
                            Some(sources),
                            |_, _, _, _| async {
                                panic!("legacy fold must never run for this source")
                            },
                        )
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/stream");
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let subscribe =
            json!({"kind":"subscribe","id":"held","collection":"work","limit":2}).to_string();
        socket
            .send(Message::Text(subscribe.clone().into()))
            .await
            .unwrap();
        assert_eq!(frame(&mut socket).await["order"], json!(["row/a", "row/b"]));
        replace(&store, &views, "row/a", 3, Some("a2"));
        let change = frame(&mut socket).await;
        assert_eq!(change["kind"], "changes");
        assert_eq!(change["order"], json!(["row/b", "row/a"]));
        assert_eq!(change["upserts"], json!([{"id":"row/a","value":"a2"}]));
        replace(&store, &views, "row/b", 2, None);
        assert_eq!(frame(&mut socket).await["removes"], json!(["row/b"]));
        let before = observed.load(Ordering::SeqCst);
        replace(&store, &views, "row/a", 3, Some("a2"));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), socket.next())
                .await
                .is_err()
        );
        assert_eq!(observed.load(Ordering::SeqCst), before);
        store
            .connection
            .batched(|tx| {
                let mut cut = smallclaims::ivm::source_cut(tx)?.unwrap();
                cut.local_generation += 1;
                views.local_change(
                    tx,
                    &LocalChange {
                        kind: "fixture.unrelated".into(),
                        old_keys: BTreeSet::new(),
                        new_keys: BTreeSet::new(),
                        evaluation_time_unix_ms: 0,
                    },
                    cut,
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), socket.next())
                .await
                .is_err()
        );
        assert_eq!(observed.load(Ordering::SeqCst), before);
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE ivm_views SET ready=0 WHERE name='fixture.socket'",
                    [],
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(frame(&mut socket).await["kind"], "resync");
        let before_ready = observed.load(Ordering::SeqCst);
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE ivm_views SET ready=1 WHERE name='fixture.socket'",
                    [],
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while observed.load(Ordering::SeqCst) == before_ready {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(retained_counts.lock().unwrap().last(), Some(&0));
        // Ready with no changed key refreshes; identical rows still produce no delta.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), socket.next())
                .await
                .is_err()
        );
        // Ready-to-Ready full refresh changes this view's version/status, with no key page.
        // This is the transport vocabulary emitted by installed Changed::Refresh.
        let key_before = events::capture(&store.readers.get(), &views, "fixture.socket")
            .unwrap()
            .keys;
        store.connection.batched(|tx| {
            tx.execute("UPDATE fixture_rows SET value='refreshed' WHERE id='row/a'",[])?;
            tx.execute("UPDATE ivm_views SET generation=generation+1 WHERE name='fixture.socket'",[])?;
            tx.execute("UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1",[])?;
            tx.execute("INSERT INTO ivm_view_status(view,sequence) SELECT 'fixture.socket',sequence FROM ivm_status_frontier WHERE singleton=1 ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence",[])?;
            Ok::<_,anyhow::Error>(())
        }).unwrap().unwrap();
        let refresh = frame(&mut socket).await;
        assert_eq!(
            refresh["upserts"],
            json!([{"id":"row/a","value":"refreshed"}])
        );
        assert_eq!(
            events::capture(&store.readers.get(), &views, "fixture.socket")
                .unwrap()
                .keys,
            key_before
        );
        assert_eq!(retained_counts.lock().unwrap().last(), Some(&0));
        // A SQL wake can arrive before an external producer acknowledges its certificate.
        // Refuse the pending source, retain the cursor, then recover by bounded retry alone.
        let publisher = store.ivm_publisher().unwrap().unwrap();
        acknowledged.store(false, Ordering::SeqCst);
        let before_ack = observed.load(Ordering::SeqCst);
        replace(&store, &views, "row/a", 3, Some("acknowledged"));
        let pending = frame(&mut socket).await;
        assert_eq!(pending["kind"], "resync");
        assert_eq!(pending["retryable"], true);
        assert_eq!(observed.load(Ordering::SeqCst), before_ack);
        acknowledged.store(true, Ordering::SeqCst);
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let next = frame(&mut socket).await;
                if next["kind"] == "changes" {
                    break next;
                }
                // A retry may already have captured pending evidence before the first frame
                // reached the peer. Those refused reads do not advance the held cursor.
                assert_eq!(next["kind"], "resync");
                assert_eq!(next["retryable"], true);
            }
        })
        .await
        .unwrap();
        assert_eq!(recovered["kind"], "changes");
        assert_eq!(
            recovered["upserts"],
            json!([{"id":"row/a","value":"acknowledged"}])
        );
        assert_eq!(observed.load(Ordering::SeqCst), before_ack + 1);
        assert!(Arc::ptr_eq(
            &publisher,
            &store.ivm_publisher().unwrap().unwrap()
        ));
        store
            .connection
            .batched(|tx| {
                smallclaims::ivm::events::rotate_identity(tx)?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(frame(&mut socket).await["kind"], "resync");
        assert_eq!(frame(&mut socket).await["kind"], "snapshot");
        socket.close(None).await.unwrap();
        replace(&store, &views, "row/c", 0, Some("c1"));
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        socket.send(Message::Text(subscribe.into())).await.unwrap();
        let snapshot = frame(&mut socket).await;
        assert_eq!(snapshot["kind"], "snapshot");
        assert_eq!(snapshot["order"], json!(["row/c", "row/a"]));
        let burst_before = observed.load(Ordering::SeqCst);
        for n in 0..20 {
            replace(&store, &views, "row/a", 3, Some(&format!("burst{n}")));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        loop {
            let change = frame(&mut socket).await;
            assert_eq!(change["kind"], "changes");
            if change["upserts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["value"] == "burst19")
            {
                break;
            }
        }
        assert!(
            observed.load(Ordering::SeqCst) - burst_before <= 4,
            "committed bursts must coalesce window callbacks"
        );
        socket.close(None).await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn independent_same_index_coverage_blocks_silent_ack_but_delivers_fenced_status() {
        let root = tempfile::tempdir().unwrap();
        let views = Arc::new(Views::new(vec![Box::new(FixtureView)]).unwrap());
        let mut state = super::super::tests::test_state_named(root.path(), "alder");
        state.store = Arc::new(
            Store::open_with_ivm_views(
                &root.path().join("coverage.sqlite"),
                "alder",
                views.clone(),
            )
            .unwrap(),
        );
        let covered = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let check = covered.clone();
        let rows = Arc::new(AtomicUsize::new(0));
        let count = rows.clone();
        let adapter = Arc::new(Adapter {
            view: "fixture.socket",
            coverage: Arc::new(move |_| Ok(check.load(Ordering::SeqCst))),
            rows: Arc::new(move |_, _, _, _, _, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok((vec![], false))
            }),
        });
        let sources = Sources::from_store(
            state.store.clone(),
            BTreeMap::from([("agents".into(), adapter.clone())]),
        )
        .unwrap()
        .unwrap();
        // A read-only Unix session has no person header or pairing grant. It retains
        // the same local read authority as the legacy collection path.
        let session = ClientSession::local(None).unwrap();
        let request: CollectionSubscribe =
            serde_json::from_value(json!({"kind":"subscribe","id":"agents","collection":"agents"}))
                .unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let first = read(
            state.clone(),
            session.clone(),
            request.clone(),
            slots.clone().acquire_owned().await.unwrap(),
            sources.clone(),
            adapter.clone(),
            Held::default(),
        )
        .await
        .unwrap();
        assert!(matches!(first.output, Output::Window(_)));
        let held = first.delivered;
        assert_eq!(rows.load(Ordering::SeqCst), 1);
        covered.store(false, Ordering::SeqCst);
        let refusal = read(
            state.clone(),
            session.clone(),
            request.clone(),
            slots.clone().acquire_owned().await.unwrap(),
            sources.clone(),
            adapter.clone(),
            Held {
                cursor: held.clone(),
                ..Default::default()
            },
        )
        .await;
        assert!(refusal.is_err());
        assert_eq!(rows.load(Ordering::SeqCst), 1);
        state
            .store
            .connection
            .batched(|tx| views.fence_all(tx, "uncovered local source"))
            .unwrap()
            .unwrap();
        let unavailable = read(
            state,
            session,
            request,
            slots.acquire_owned().await.unwrap(),
            sources,
            adapter,
            Held {
                cursor: held,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(unavailable.output, Output::Unavailable));
        assert!(unavailable.delivered.is_some());
        assert_eq!(rows.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn source_admission_gap_and_person_refusal_never_read_rows() {
        let root = tempfile::tempdir().unwrap();
        let views = Arc::new(Views::new(vec![Box::new(FixtureView)]).unwrap());
        let mut state = super::super::tests::test_state_named(root.path(), "alder");
        state.store = Arc::new(
            Store::open_with_ivm_views(&root.path().join("ivm.db"), "alder", views.clone())
                .unwrap(),
        );
        let count = Arc::new(AtomicUsize::new(0));
        let rows = count.clone();
        let adapter = Arc::new(Adapter {
            view: "fixture.socket",
            coverage: Arc::new(|_| Ok(true)),
            rows: Arc::new(move |_, _, _, _, _, _, _| {
                rows.fetch_add(1, Ordering::SeqCst);
                Ok((vec![], false))
            }),
        });
        let sources = Sources::from_store(
            state.store.clone(),
            BTreeMap::from([("attention".into(), adapter.clone())]),
        )
        .unwrap()
        .unwrap();
        let request:CollectionSubscribe=serde_json::from_value(json!({"kind":"subscribe","id":"held","collection":"attention","person":"person/intruder"})).unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let result = read(
            state.clone(),
            ClientSession::local(Some("person/avery")).unwrap(),
            request,
            slots.clone().acquire_owned().await.unwrap(),
            sources.clone(),
            adapter.clone(),
            Held::default(),
        )
        .await;
        assert_eq!(result.err().unwrap().status, StatusCode::FORBIDDEN);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // This deliberate bypass leaves a Ready registry at cut0 while the Store gains
        // an undispatched durable input. Registration is insufficient for a row read.
        state
            .store
            .connection
            .batched(|tx| {
                smallclaims::store::append_claim_record_tx(
                    tx,
                    "alder",
                    "source/uncovered",
                    "fixture.uncovered",
                    None,
                    &json!({}),
                    &[],
                    None,
                )?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        let request = serde_json::from_value(
            json!({"kind":"subscribe","id":"held","collection":"attention"}),
        )
        .unwrap();
        let result = read(
            state,
            ClientSession::local(Some("person/avery")).unwrap(),
            request,
            slots.acquire_owned().await.unwrap(),
            sources,
            adapter,
            Held::default(),
        )
        .await;
        assert!(matches!(result.unwrap().output, Output::Unavailable));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
