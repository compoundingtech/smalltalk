//! Current collection cursors. The source adapter owns complete keyed dependency coverage,
//! authorized bounded ranking and row reads; this consumer owns delivery acknowledgment.
//! Registration alone does not select a production adapter.
use super::*;
use crate::graph_watch_ivm::{IvmViewBridge, ViewPage};
use smallclaims::ivm::{Readiness, events};
use std::cell::RefCell;

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

pub(super) struct Adapter {
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
    let current = revalidate_session(state, session)?;
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
                let index = smallclaims::store::current_index(connection)?;
                let cut = smallclaims::ivm::source_cut(connection)?;
                anyhow::ensure!(cut.is_some_and(|cut|cut.admitted == index),
                    "collection source admission coverage is pending");
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
                previous.authority == authority(session)) && keys.len() < 256;
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
        let adapter = Arc::new(Adapter {
            view: "fixture.socket",
            rows: Arc::new(move |_, session, request, conn, _, _, _| {
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
        // Ready with no changed key refreshes; identical rows still produce no delta.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), socket.next())
                .await
                .is_err()
        );
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
        socket.close(None).await.unwrap();
        server.abort();
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
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
