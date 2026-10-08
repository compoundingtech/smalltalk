//! Real Service/provider controls. This file requires the source owner's frozen constructor.
//! No fixture writes readiness, a Root, a source cut, or a private producer certificate.
//! Paired sessions below exercise held-grant authority, not bearer/TLS/physical-phone transport.
use super::super::{self as bridge, Candidate, Held, Output, Sources};
use crate::{api::AppState, model::ClaimInput, store::Store};
use axum::{
    body::{Body, to_bytes},
    extract::WebSocketUpgrade,
    http::{Request, StatusCode},
    routing::get,
};
use futures_util::{SinkExt as _, StreamExt as _};
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::sync::{Notify, Semaphore, oneshot, watch};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest as _},
};
use tower::ServiceExt as _;

use crate::api::client_v0::{
    COLLECTION_SUBPROTOCOL, ClientSession, CollectionSubscribe,
    collection_stream_socket_with_sources, paired_client_session,
};

// All actual Service constructors replace one process-global producer registration. Share
// the Source controls' test-only exclusion without changing the runner or test thread count.
use crate::store::collection_ivm::agent_source::service::TEST_LOCK as PRODUCER;
const AMBER: &str = "agent/ivm-profile/20261008/amber";
const BLUE: &str = "agent/ivm-profile/20261008/blue";
const CYAN: &str = "agent/ivm-profile/20261008/cyan";
const UNUSED_HOST: &str = "ivm-profile-unused-20261008";

struct Fixture {
    state: AppState,
    tokens: BTreeMap<String, Vec<String>>,
    sequence: usize,
}

impl Fixture {
    async fn new(root: &Path) -> Self {
        let fixture = Self::unready(root);
        // These positive controls exercise Ready -> write -> Pending -> pump. Input arriving
        // before first publication is tested separately, without this startup pump.
        fixture.pump().await;
        fixture
    }

    fn unready(root: &Path) -> Self {
        let store =
            Store::open_with_agent_collections(&root.join("provider.sqlite"), "provider-fixture")
                .unwrap();
        assert_ne!(store.origin(), UNUSED_HOST);
        let publisher = store.ivm_publisher().unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &publisher,
            &store.ivm_publisher().unwrap().unwrap()
        ));
        Self {
            state: AppState {
                node: store.origin().into(),
                store,
                notify: Arc::new(Notify::new()),
                event_notify: watch::channel(0).0,
                state_dir: root.into(),
                pty_root: root.join("pty"),
                pty_binary: "pty".into(),
                fleet_id: None,
                configured_peers: vec![],
                client_relay: None,
                native_session_home: None,
                planner_default: Default::default(),
            },
            tokens: BTreeMap::new(),
            sequence: 0,
        }
    }

    fn sources(&self) -> Arc<Sources> {
        // Use the production selector, never install a test map or a second Publisher.
        Sources::from_store(
            self.state.store.clone(),
            bridge::adapters(self.state.store.clone()).unwrap(),
        )
        .unwrap()
        .expect("actual constructor must register the complete agent provider")
    }

    fn covered(&self) -> bool {
        let adapter = self.sources().adapter("agents").unwrap();
        self.state
            .store
            .read_snapshot(|_| (adapter.coverage)(&self.state.store.readers.get()))
            .unwrap()
    }

    async fn pump(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        for _ in 0..256 {
            if self.covered() {
                return;
            }
            // False requests the production timer, not permanent refusal. A capture emitted
            // by this tick can still need catchup and a subsequent publication attempt.
            if !self.state.store.maintain_agent_collections().unwrap() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        assert!(
            self.covered(),
            "actual Service remained unreadable after bounded production timer/pump retries: {}",
            self.diagnostic()
        );
    }

    fn diagnostic(&self) -> String {
        self.state.store.read_snapshot(|_| {
            let c = self.state.store.readers.get();
            let cut = smallclaims::ivm::source_cut(&c)?;
            let deferred = self.state.store.replication_projection_deferred();
            let sql_index = smallclaims::store::current_index(&c)?;
            let graph_health: Option<(String,u64)> = c.query_row(
                "SELECT status,last_good_store_index FROM projection_health WHERE aggregate='graph'",
                [], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            // The Source-owned export obtains its genuine held staging Namespace. Errors
            // remain diagnostic values; this child never reconstructs an identity or cut.
            let source_diagnostic = crate::store::collection_ivm::agent_source::service::diagnostic(&self.state.store);
            let views = self.state.store.ivm_views().unwrap();
            let installer = self.state.store.ivm_installer().unwrap();
            let root = installer.root(&c, super::cards::VIEW);
            let footprint = root.as_ref().ok().map(|root| format!("{:?}",
                crate::store::agent_card_source::footprint(&c, &root.namespace,self.state.store.origin())
                    .map(|f| (f.files.len(),f.earliest_monotonic_deadline))));
            let pending: Vec<(String,String)> = c.prepare("SELECT kind,key FROM local_agent_card_source_work ORDER BY kind,key LIMIT 16")?
                .query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
            let jobs:Vec<(String,String,u64,u64,u64)> = c.prepare("SELECT id,phase,base_revision,applied_revision,queued_rows FROM ivm_install_jobs ORDER BY id LIMIT 16")?
                .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<Result<_,_>>()?;
            let clocks:Vec<(String,u64,u64,u64)> = c.prepare("SELECT namespace,phase,snapshot_index,revision FROM local_agent_card_source_clock ORDER BY namespace LIMIT 16")?
                .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
            let mut counts=Vec::new();
            for table in ["local_agent_source_dirty","local_agent_source_fanout","local_agent_queue_dirty","local_agent_card_source_work","local_agent_card_rows"] {
                counts.push((table,c.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,u64>(0))?));
            }
            Ok(format!("deferred={deferred}, sql_index={sql_index}, graph_health={graph_health:?}, source={source_diagnostic:?}, cut={cut:?}, readiness={:?}, root={root:?}, footprint={footprint:?}, work={pending:?}, capture={:?}, position={:?}, jobs={jobs:?}, clocks={clocks:?}, counts={counts:?}",
                views.readiness(&c,super::cards::VIEW,cut.map_or(0,|cut|cut.epoch)),
                crate::store::collection_ivm::status(&c), installer.position(&c,super::cards::SOURCE)))
        }).unwrap()
    }

    async fn apply(&mut self, kdl: String) {
        self.sequence += 1;
        let normalized = crate::graph::parse_intent(&kdl, self.state.store.origin()).unwrap();
        for subject in normalized.subjects.keys() {
            self.tokens.entry(subject.clone()).or_default();
        }
        let body = json!({"intent":{"kdl":kdl,"source_name":"provider-parity.kdl"},
            "expected_subjects":self.tokens,"idempotency_key":format!("provider-{}",self.sequence),
            "actor":"person/avery"});
        let response = crate::api::router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/intent/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "public apply refused: {body}");
        self.tokens = serde_json::from_value(body["value"]["subject_tokens"].clone()).unwrap();
    }

    async fn names(&mut self, amber: &str, blue: &str) {
        self.declare(&[(AMBER, amber), (BLUE, blue)]).await;
    }

    async fn three_names(&mut self, amber: &str, blue: &str, cyan: &str) {
        self.declare(&[(AMBER, amber), (BLUE, blue), (CYAN, cyan)])
            .await;
    }

    async fn declare(&mut self, names: &[(&str, &str)]) {
        let mut kdl = "version 2\n".to_string();
        for (id, name) in names {
            let tail = id.strip_prefix("agent/").unwrap();
            let workspace = id.rsplit('/').next().unwrap();
            kdl.push_str(&format!("agent \"{tail}\" {{ name \"{name}\"; host \"{UNUSED_HOST}\"; workspace \"/ivm-profile-unused/{workspace}\"; command \"true\"; restart \"never\"; }}\n"));
        }
        let normalized = crate::graph::parse_intent(&kdl, self.state.store.origin()).unwrap();
        for subject in normalized.subjects.values() {
            let member = subject.member.as_ref().unwrap();
            assert_eq!(member.host, UNUSED_HOST);
            assert_ne!(member.host, self.state.node);
        }
        // No configured peer matches this host; the production reconcile member predicate
        // excludes it before launch. This temporary API fixture runs no reconciler.
        assert!(self.state.configured_peers.is_empty());
        self.apply(kdl).await;
    }

    fn append(&self, subject: &str, kind: &str, fields: Value) -> crate::model::ClaimRecord {
        self.state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("person/avery".into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }

    fn paired(&self, expires: u64) -> ClientSession {
        let grant = self.append(
            "custom/client/provider-phone",
            "custom.client.pairing-completed",
            json!({"session_actor":"client/provider-phone","person_id":"person/avery",
                "scopes":["read.projections"],"expires_at_unix_ms":expires}),
        );
        paired_client_session(&self.state, &grant, "paired-fixture", false).unwrap()
    }

    async fn read(
        &self,
        session: ClientSession,
        cursor: Option<bridge::Delivered>,
        rows: Arc<BTreeMap<String, Value>>,
    ) -> Result<Candidate, crate::api::ApiError> {
        let sources = self.sources();
        let request: CollectionSubscribe = serde_json::from_value(
            json!({"kind":"subscribe","id":"held","collection":"agents","limit":200}),
        )
        .unwrap();
        bridge::read(
            self.state.clone(),
            session,
            request,
            Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
            sources.clone(),
            sources.adapter("agents").unwrap(),
            Held { cursor, rows },
        )
        .await
    }

    async fn oracle(&self) -> Vec<Value> {
        self.oracle_for(None).await
    }

    async fn oracle_for(&self, status: Option<&str>) -> Vec<Value> {
        let index = self.state.store.index().unwrap();
        let uri = status.map_or_else(
            || "/v1/client/agents?limit=200".to_string(),
            |status| {
                format!(
                    "/v1/client/agents?limit=200&status={}",
                    urlencoding::encode(status)
                )
            },
        );
        let response = crate::api::router(self.state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("x-st3-person", "person/avery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            self.state.store.index().unwrap(),
            index,
            "oracle source changed"
        );
        assert_eq!(body["snapshot"]["store_index"], index);
        assert!(body["value"]["next_cursor"].is_null());
        let rows = body["value"]["items"].as_array().unwrap().clone();
        for row in &rows {
            assert!(
                row.as_object().unwrap().len() >= 41,
                "incomplete public oracle: {row}"
            );
        }
        rows
    }

    async fn window(&self, session: ClientSession) -> Candidate {
        self.pump().await;
        let candidate = self
            .read(session, None, Arc::new(BTreeMap::new()))
            .await
            .unwrap();
        let Output::Window((snapshot, rows, has_more)) = &candidate.output else {
            panic!("complete provider did not deliver a Window")
        };
        assert_eq!(snapshot.store_index, self.state.store.index().unwrap());
        assert!(!has_more);
        assert_eq!(
            *rows,
            self.oracle().await,
            "complete provider/public HTTP row parity"
        );
        candidate
    }
}

#[derive(Default)]
struct ClientWindow {
    rows: BTreeMap<String, Value>,
    order: Vec<String>,
    has_more: bool,
}
impl ClientWindow {
    fn apply(&mut self, frame: &Value) {
        assert_eq!(frame["id"], "held");
        match frame["kind"].as_str().unwrap() {
            "snapshot" => {
                self.rows.clear();
                for row in frame["items"].as_array().unwrap() {
                    self.rows
                        .insert(row["id"].as_str().unwrap().into(), row.clone());
                }
            }
            "changes" => {
                for id in frame["removes"].as_array().unwrap() {
                    assert!(self.rows.remove(id.as_str().unwrap()).is_some());
                }
                for row in frame["upserts"].as_array().unwrap() {
                    self.rows
                        .insert(row["id"].as_str().unwrap().into(), row.clone());
                }
            }
            other => panic!("unexpected data frame {other}: {frame}"),
        }
        self.order = serde_json::from_value(frame["order"].clone()).unwrap();
        self.has_more = frame["has_more"].as_bool().unwrap();
        assert_eq!(self.order.len(), self.rows.len());
        for id in &self.order {
            assert!(self.rows.contains_key(id));
        }
    }
    fn ordered(&self) -> Vec<Value> {
        self.order.iter().map(|id| self.rows[id].clone()).collect()
    }
}

struct Socket {
    wire: WebSocketStream<tokio::net::UnixStream>,
    finished: Option<oneshot::Receiver<()>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Socket {
    async fn open(f: &Fixture, session: ClientSession, suffix: &str) -> Self {
        Self::open_window(f, session, suffix, 200).await
    }
    async fn open_window(f: &Fixture, session: ClientSession, suffix: &str, limit: usize) -> Self {
        let sources = f.sources();
        let state = f.state.clone();
        let (finished, done) = oneshot::channel();
        let finished = Arc::new(std::sync::Mutex::new(Some(finished)));
        let app = axum::Router::new().route("/stream",get(move |upgrade: WebSocketUpgrade| {
            let (state,sources,session,finished) = (state.clone(),sources.clone(),session.clone(),finished.clone());
            async move { upgrade.protocols([COLLECTION_SUBPROTOCOL]).on_upgrade(move |socket| async move {
                collection_stream_socket_with_sources(socket,state,session,None,Some(sources),
                    |_,_,_,_|async { panic!("registered actual agent provider fell back to a legacy reader") }).await;
                if let Some(done)=finished.lock().unwrap().take() { let _=done.send(()); }
            }) }
        }));
        Self::connect(
            f,
            app,
            "/stream",
            None,
            suffix,
            Some(done),
            json!({"kind":"subscribe","id":"held","collection":"agents","limit":limit}),
        )
        .await
    }

    async fn production(f: &Fixture, credential: &str, suffix: &str) -> Self {
        Self::connect(
            f,
            crate::api::fabric_router(f.state.clone()),
            "/v1/client/collections/stream",
            Some(credential),
            suffix,
            None,
            json!({"kind":"subscribe","id":"held","collection":"agents","limit":200}),
        )
        .await
    }

    async fn connect(
        f: &Fixture,
        app: axum::Router,
        uri: &str,
        credential: Option<&str>,
        suffix: &str,
        finished: Option<oneshot::Receiver<()>>,
        subscription: Value,
    ) -> Self {
        let path = f.state.state_dir.join(format!("provider-{suffix}.sock"));
        let (ready, bound) = oneshot::channel();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            crate::api::serve_unix_with_ready(&server_path, app, move || {
                let _ = ready.send(());
            })
            .await
            .unwrap();
        });
        bound.await.unwrap();
        let mut request = format!("ws://localhost{uri}")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            COLLECTION_SUBPROTOCOL.parse().unwrap(),
        );
        if let Some(credential) = credential {
            request.headers_mut().insert(
                "authorization",
                format!("Bearer {credential}").parse().unwrap(),
            );
        }
        let (mut wire, response) = tokio_tungstenite::client_async(
            request,
            tokio::net::UnixStream::connect(path).await.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers()["sec-websocket-protocol"],
            COLLECTION_SUBPROTOCOL
        );
        wire.send(Message::Text(subscription.to_string().into()))
            .await
            .unwrap();
        Self {
            wire,
            finished,
            server,
        }
    }
    async fn frame(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(8), self.wire.next())
                .await
                .expect("actual socket frame timed out")
                .unwrap()
                .unwrap();
            match message {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) => self.wire.flush().await.unwrap(),
                other => panic!("unexpected socket frame {other:?}"),
            }
        }
    }
    async fn close(mut self) {
        let _ = self.wire.close(None).await;
        if let Some(done) = self.finished.take() {
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .unwrap()
                .unwrap();
        } else {
            // Complete the production endpoint's close handshake before releasing its Store.
            let _ = tokio::time::timeout(Duration::from_secs(2), self.wire.next()).await;
        }
        self.server.abort();
        let _ = (&mut self.server).await;
    }
}

fn local() -> ClientSession {
    ClientSession::local(Some("person/avery")).unwrap()
}
fn expiry() -> u64 {
    super::client_now_ms() as u64 + 60_000
}

#[tokio::test]
async fn actual_service_changed_desired_body_raw_gap_reopen_full_public_parity() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(root.path()).await;
    f.names("IVM profile000", "IVM profile001").await;
    f.window(local()).await;
    let before_rows = f.oracle().await;
    let before_index = f.state.store.index().unwrap();
    let before_namespace = f
        .state
        .store
        .ivm_views()
        .unwrap()
        .installed_root(
            &f.state.store.readers.get(),
            &f.state.store.ivm_installer().unwrap(),
            super::cards::VIEW,
        )
        .unwrap()
        .namespace;
    let (claim_id, claim_index, raw): (String, u64, String) = f.state.store.readers.get()
        .query_row("SELECT id,store_index,body FROM claims WHERE subject=?1 AND kind='intent.desired' ORDER BY store_index DESC LIMIT 1",
            [AMBER], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    let mut body: Value = serde_json::from_str(&raw).unwrap();
    let mut desired: crate::model::DesiredSubject = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(
        desired.member.as_ref().unwrap().display_name.as_deref(),
        Some("IVM profile000")
    );
    desired
        .set_display_name(Some("IVM repaired declaration"))
        .unwrap();
    // Preserve other claim-body metadata and the existing identity/rank/index. This changes
    // both the authored KDL name and its normalized member projection, not a no-op field.
    body["desired"] = desired.desired;
    body["member"] = serde_json::to_value(desired.member).unwrap();
    assert_ne!(body, serde_json::from_str::<Value>(&raw).unwrap());
    assert_eq!(
        f.state
            .store
            .connection
            .write()
            .execute(
                "UPDATE claims SET body=?1 WHERE id=?2",
                rusqlite::params![body.to_string(), claim_id],
            )
            .unwrap(),
        1
    );
    assert_eq!(f.state.store.index().unwrap(), before_index);
    assert_eq!(
        f.state
            .store
            .readers
            .get()
            .query_row(
                "SELECT store_index FROM claims WHERE id=?1",
                [&claim_id],
                |r| r.get::<_, u64>(0),
            )
            .unwrap(),
        claim_index
    );
    let refused = f.read(local(), None, Arc::new(BTreeMap::new())).await;
    assert!(
        matches!(
            refused,
            Err(_)
                | Ok(Candidate {
                    output: Output::Unavailable,
                    ..
                })
        ),
        "raw same-index changed source must immediately refuse public row delivery"
    );
    drop(f);

    let recovered = Fixture::unready(root.path());
    // Native replay must repair the legacy projection before namespace catchup. Keep this
    // assertion before pump so a closure failure cannot hide a stale projection failure.
    let declarations = recovered.state.store.desired_subjects().unwrap();
    let repaired = declarations.iter().find(|s| s.subject == AMBER).unwrap();
    assert_eq!(
        repaired.member.as_ref().unwrap().display_name.as_deref(),
        Some("IVM repaired declaration"),
        "native desired projection must replay the changed canonical value"
    );
    recovered.window(local()).await;
    assert_eq!(recovered.state.store.index().unwrap(), before_index);
    assert_eq!(
        recovered
            .state
            .store
            .readers
            .get()
            .query_row(
                "SELECT store_index FROM claims WHERE id=?1",
                [&claim_id],
                |r| r.get::<_, u64>(0),
            )
            .unwrap(),
        claim_index
    );
    let after_namespace = recovered
        .state
        .store
        .ivm_views()
        .unwrap()
        .installed_root(
            &recovered.state.store.readers.get(),
            &recovered.state.store.ivm_installer().unwrap(),
            super::cards::VIEW,
        )
        .unwrap()
        .namespace;
    assert_ne!(
        after_namespace, before_namespace,
        "recovery must publish a fresh actual namespace"
    );
    assert_ne!(
        recovered.oracle().await,
        before_rows,
        "full public rows must reflect the correction"
    );
}

#[tokio::test]
async fn actual_service_declarations_before_initial_publication_settle_without_extra_readiness() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut f = Fixture::unready(root.path());
    f.names("IVM profile000", "IVM profile001").await;
    f.window(local()).await;
}

#[tokio::test]
async fn actual_service_public_apply_rename_stop_full_provider_parity() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(root.path()).await;
    f.window(local()).await;
    f.names("IVM profile000", "IVM profile001").await;
    assert!(
        !f.covered(),
        "managed writes must invalidate complete production coverage before pump"
    );
    let pending = f.read(local(), None, Arc::new(BTreeMap::new())).await;
    assert!(
        matches!(
            pending,
            Err(_)
                | Ok(Candidate {
                    output: Output::Unavailable,
                    ..
                })
        ),
        "SourcePending must refuse public row delivery"
    );
    f.window(local()).await;
    f.names("IVM profile002", "IVM profile001").await;
    f.window(local()).await;
    f.apply(format!("version 2\nstop \"{AMBER}\"\nstop \"{BLUE}\"\n"))
        .await;
    f.window(local()).await;
    let stopped = f.oracle().await;
    assert_eq!(stopped.len(), 2);
    let stopped_declarations = f.state.store.desired_subjects().unwrap();
    assert_eq!(stopped_declarations.len(), 2);
    assert!(
        stopped_declarations
            .iter()
            .all(|s| s.kind == "stop" && s.member.is_none())
    );
    assert!(
        f.state
            .store
            .desired_subjects()
            .unwrap()
            .iter()
            .all(|s| s.member.as_ref().is_none_or(|m| m.host != f.state.node)),
        "fixture must have no local launch-eligible member"
    );
}

#[tokio::test]
async fn actual_service_socket_snapshot_upsert_reorder_remove_reconnect() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(root.path()).await;
    f.three_names("IVM profile000", "IVM profile001", "IVM profile003")
        .await;
    f.pump().await;
    let mut socket = Socket::open_window(&f, local(), "initial", 2).await;
    let mut held = ClientWindow::default();
    let initial = socket.frame().await;
    assert_eq!(initial["kind"], "snapshot");
    held.apply(&initial);
    assert_eq!(
        held.ordered(),
        f.oracle().await.into_iter().take(2).collect::<Vec<_>>()
    );
    assert!(held.has_more);
    f.three_names("IVM profile002", "IVM profile001", "IVM profile003")
        .await;
    f.pump().await;
    // Pending availability may precede the committed data frame; it never updates held rows.
    let changed = loop {
        let frame = socket.frame().await;
        if frame["kind"] == "resync" {
            continue;
        }
        break frame;
    };
    assert_eq!(changed["kind"], "changes", "{changed}");
    assert!(
        changed["upserts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == AMBER)
    );
    held.apply(&changed);
    assert_eq!(held.order, vec![BLUE, AMBER]);
    assert_eq!(
        held.ordered(),
        f.oracle().await.into_iter().take(2).collect::<Vec<_>>()
    );
    f.three_names("IVM profile004", "IVM profile001", "IVM profile003")
        .await;
    f.pump().await;
    let removed = loop {
        let frame = socket.frame().await;
        if frame["kind"] == "resync" {
            continue;
        }
        break frame;
    };
    assert_eq!(removed["kind"], "changes", "{removed}");
    assert_eq!(removed["removes"], json!([AMBER]));
    held.apply(&removed);
    assert_eq!(held.order, vec![BLUE, CYAN]);
    assert!(held.has_more);
    assert_eq!(
        held.ordered(),
        f.oracle().await.into_iter().take(2).collect::<Vec<_>>()
    );
    socket.close().await;
    let mut socket = Socket::open_window(&f, local(), "reconnect", 2).await;
    let fresh = socket.frame().await;
    assert_eq!(fresh["kind"], "snapshot");
    let mut reconnect = ClientWindow::default();
    reconnect.apply(&fresh);
    assert_eq!(reconnect.ordered(), held.ordered());
    assert_eq!(
        reconnect.ordered(),
        f.oracle().await.into_iter().take(2).collect::<Vec<_>>()
    );
    socket.close().await;
}

#[tokio::test]
async fn actual_service_paired_authority_refuses_revocation_and_owner_replacement() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let f = Fixture::new(root.path()).await;
    let session = f.paired(expiry());
    let candidate = f.window(session.clone()).await;
    let Output::Window((_, rows, _)) = candidate.output else {
        unreachable!()
    };
    let rows = Arc::new(
        rows.into_iter()
            .map(|r| (r["id"].as_str().unwrap().to_owned(), r))
            .collect(),
    );
    // Grant mutations have no agent semantic key. Authorization remains mandatory on Silent.
    f.append("custom/client/provider-phone","custom.client.pairing-completed",
        json!({"session_actor":"client/provider-phone","person_id":"person/ada","scopes":["read.projections"],"expires_at_unix_ms":expiry()}));
    f.pump().await;
    assert!(
        f.read(session.clone(), candidate.delivered, rows)
            .await
            .is_err()
    );
    let session = f.paired(expiry());
    let candidate = f.window(session.clone()).await;
    f.append(
        "custom/client/provider-phone",
        "custom.client.pairing-revoked",
        json!({"reason":"fixture"}),
    );
    f.pump().await;
    assert!(
        f.read(session, candidate.delivered, Arc::new(BTreeMap::new()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn actual_service_paired_socket_expiry_refuses_without_claim_advance() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let f = Fixture::new(root.path()).await;
    f.pump().await;
    let session = f.paired(super::client_now_ms() as u64 + 1500);
    f.pump().await;
    let index = f.state.store.index().unwrap();
    let mut socket = Socket::open(&f, session, "expires").await;
    assert_eq!(socket.frame().await["kind"], "snapshot");
    let expired = socket.frame().await;
    assert_eq!(expired["kind"], "error");
    assert_eq!(expired["retryable"], false);
    assert_eq!(f.state.store.index().unwrap(), index);
    socket.close().await;
}

#[tokio::test]
async fn actual_service_paired_authority_uses_captured_wal_cut_then_refuses_revocation() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let f = Fixture::new(root.path()).await;
    let session = f.paired(expiry());
    f.window(session.clone()).await;
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe","id":"held","collection":"agents","limit":200
    }))
    .unwrap();
    f.state
        .store
        .read_snapshot(|index| {
            assert!(bridge::authorize(&f.state, &session, &request).is_ok());
            // The writer has its own thread: writing on the pinned reader thread is prohibited.
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        f.append(
                            "custom/client/provider-phone",
                            "custom.client.pairing-revoked",
                            json!({"reason":"concurrent fixture revocation"}),
                        );
                    })
                    .join()
                    .unwrap()
            });
            assert_eq!(
                smallclaims::store::current_index(&f.state.store.readers.get())?,
                index
            );
            assert!(
                bridge::authorize(&f.state, &session, &request).is_ok(),
                "nested grant getter must borrow this captured SQL snapshot"
            );
            Ok(())
        })
        .unwrap();
    assert!(
        bridge::authorize(&f.state, &session, &request).is_err(),
        "the next snapshot must refuse the committed revocation"
    );
}

#[tokio::test]
async fn actual_service_paired_socket_refuses_revocation_on_semantically_silent_advance() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let f = Fixture::new(root.path()).await;
    let session = f.paired(expiry());
    f.pump().await;
    let mut socket = Socket::open(&f, session, "revoked").await;
    assert_eq!(socket.frame().await["kind"], "snapshot");
    f.append(
        "custom/client/provider-phone",
        "custom.client.pairing-revoked",
        json!({"reason":"held grant revoked"}),
    );
    f.pump().await;
    let refused = socket.frame().await;
    assert_eq!(refused["kind"], "error", "{refused}");
    assert_eq!(refused["retryable"], false);
    socket.close().await;
}

#[tokio::test]
async fn actual_service_fabric_bearer_endpoint_full_rows_reconnect_and_revocation() {
    let _exclusive = PRODUCER.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(root.path()).await;
    f.names("IVM profile000", "IVM profile001").await;
    let credential = "fixture-provider-bearer";
    f.append(
        "custom/client/provider-phone",
        "custom.client.pairing-completed",
        json!({
            "credential_hash":crate::api::client_v0::credential_digest(credential),
            "session_actor":"client/provider-phone","person_id":"person/avery",
            "scopes":["read.projections"],"expires_at_unix_ms":expiry()
        }),
    );
    f.pump().await;
    // Actual Fabric middleware rejects a missing credential. No transport or grant bypass.
    let unauthorized = crate::api::fabric_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/client/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);
    let mut socket = Socket::production(&f, credential, "fabric-first").await;
    let mut held = ClientWindow::default();
    let first = socket.frame().await;
    assert_eq!(first["kind"], "snapshot");
    held.apply(&first);
    assert_eq!(held.ordered(), f.oracle().await);
    socket.close().await;
    let mut socket = Socket::production(&f, credential, "fabric-reconnect").await;
    let fresh = socket.frame().await;
    assert_eq!(fresh["kind"], "snapshot");
    held.apply(&fresh);
    assert_eq!(held.ordered(), f.oracle().await);
    f.append(
        "custom/client/provider-phone",
        "custom.client.pairing-revoked",
        json!({"reason":"fabric fixture"}),
    );
    f.pump().await;
    let revoked = socket.frame().await;
    assert_eq!(revoked["kind"], "error", "{revoked}");
    assert_eq!(revoked["retryable"], false);
    socket.close().await;
    let refused = crate::api::fabric_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/client/agents")
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
}
