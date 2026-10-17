//! A tile's restricted st endpoint: `RUNTIME/NAME/endpoint/st.sock`, which the tile reaches at
//! `/run/st/st.sock` and nowhere else.
//!
//! The daemon of the tile's supervisor serves it. Each connection is the tile's only when the
//! kernel says so: `SO_PEERCRED` gives the caller's Unix user and process, the process's cgroup
//! must lie in the tile's slice, and root's registry names the agent. Requests then act as that
//! agent alone. A request that names another sender or recipient is refused, never rewritten,
//! so `--as`, `--from` and `ST_AGENT` cannot widen what a tile may do. Only the routes below are
//! served; every other one, including the daemon's administrative API, is refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{
    ApiError, AppState, Body, MessageRead, MessagesQuery, blocking_api, blocking_store,
    normalize_message_party,
};
use crate::model::{MessageSendReceipt, MessageSendRequest, MessageView};
use crate::tile_registry::{DEFAULT_REGISTRY, Registry, Tile};

/// The socket's name in the tile's endpoint directory.
pub const SOCKET: &str = "st.sock";

/// The tile behind a connection, as the kernel and root's registry name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileCaller {
    pub tile: String,
    pub agent: String,
    pub person: String,
    pub pid: i32,
}

/// What the endpoint asks the kernel about a caller. Tests stand in for it; the daemon reads
/// `/proc`.
pub trait Kernel: Send + Sync + 'static {
    fn cgroup(&self, pid: i32) -> Option<String>;
}

struct Proc;

impl Kernel for Proc {
    fn cgroup(&self, pid: i32) -> Option<String> {
        crate::sekrets::identity::process_cgroup(pid)
    }
}

/// The only routes a tile reaches.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/tile/whoami", get(whoami))
        .route("/v1/messages", get(list_messages).post(send_message))
        .route("/v1/messages/read/{*subject}", get(read_message))
        .fallback(refuse)
        .with_state(state)
}

fn forbidden(message: String) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        code: "tile-forbidden".into(),
        message,
        details: Box::default(),
    }
}

async fn refuse(request: axum::extract::Request) -> ApiError {
    forbidden(format!(
        "{} {} is not available to a tile",
        request.method(),
        request.uri().path()
    ))
}

async fn whoami(Extension(caller): Extension<TileCaller>) -> Json<Value> {
    Json(json!({
        "tile": caller.tile,
        "agent": caller.agent,
        "person": caller.person,
        "pid": caller.pid,
    }))
}

/// Send as the tile's agent. A request naming another sender is refused.
async fn send_message(
    State(state): State<AppState>,
    Extension(caller): Extension<TileCaller>,
    Json(mut request): Json<MessageSendRequest>,
) -> Result<Json<MessageSendReceipt>, ApiError> {
    if !request.from.is_empty() && normalize_message_party(&request.from) != caller.agent {
        return Err(forbidden(format!(
            "this tile is {}; it cannot send as {}",
            caller.agent, request.from
        )));
    }
    // An attachment names a blob this daemon holds; a tile would reach blobs it never uploaded.
    if !request.attachments.is_empty() {
        return Err(forbidden(
            "a tile cannot attach images through its endpoint yet".into(),
        ));
    }
    request.from = caller.agent;
    blocking_api(move || super::accept_message_receipt(&state, request, None, None).map(Json)).await
}

/// The tile's own mailbox. A request for another recipient's is refused.
async fn list_messages(
    State(state): State<AppState>,
    Extension(caller): Extension<TileCaller>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Vec<MessageView>>, ApiError> {
    if let Some(to) = &query.to
        && normalize_message_party(to) != caller.agent
    {
        return Err(forbidden(format!(
            "this tile is {}; it cannot read the mailbox of {to}",
            caller.agent
        )));
    }
    let store = state.store.clone();
    blocking_store(move || store.messages(Some(&caller.agent), query.include_closed))
        .await
        .map(Json)
}

/// A message the tile's agent sent or received. Any other is not found, not forbidden, so a tile
/// learns nothing of messages that are not its own.
async fn read_message(
    State(state): State<AppState>,
    Extension(caller): Extension<TileCaller>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<MessageRead>, ApiError> {
    let subject = if subject.starts_with("message/") {
        subject
    } else {
        format!("message/{subject}")
    };
    let store = state.store.clone();
    let lookup = subject.clone();
    let message = blocking_store(move || store.message(&lookup))
        .await?
        .filter(|message| {
            normalize_message_party(&message.to) == caller.agent
                || normalize_message_party(&message.from) == caller.agent
        })
        .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
    Ok(Json(MessageRead {
        message,
        provenance: None,
    }))
}

/// Who is on the other end of `stream`, if it is the tile `name`.
fn identify(
    stream: &tokio::net::UnixStream,
    registry: &Registry,
    name: &str,
    kernel: &dyn Kernel,
) -> Option<TileCaller> {
    let credentials = stream.peer_cred().ok()?;
    let pid = credentials.pid()?;
    let cgroup = kernel.cgroup(pid)?;
    let (tile, entry) = registry.identify(credentials.uid(), &cgroup)?;
    (tile == name).then(|| TileCaller {
        tile: tile.to_owned(),
        agent: entry.agent.clone(),
        person: entry.person.clone(),
        pid,
    })
}

/// Serve tile `name` on `listener` for as long as this future lives. A connection from anyone
/// but the tile is closed before a byte is read. Every connection belongs to this future: when
/// it is dropped, as when root's registry no longer names the tile, open connections close too,
/// so none keeps acting for an identity the registry has withdrawn.
pub async fn serve_listener(
    state: AppState,
    registry: Arc<Registry>,
    name: String,
    listener: tokio::net::UnixListener,
    kernel: Arc<dyn Kernel>,
) {
    use hyper::body::Incoming;
    use hyper_util::rt::TokioIo;
    use tower::ServiceExt as _;
    let app = router(state);
    let mut connections = tokio::task::JoinSet::new();
    let mut backoff = ACCEPT_BACKOFF_START;
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _)) => {
                backoff = ACCEPT_BACKOFF_START;
                stream
            }
            // Running out of descriptors, or a peer that hung up before it was accepted, fails
            // one accept. Keep the listener and back off, as the daemon's own API does.
            Err(error) => {
                if backoff == ACCEPT_BACKOFF_START {
                    eprintln!("st3: tile {name}'s endpoint: accept a connection: {error}");
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                continue;
            }
        };
        let Some(caller) = identify(&stream, &registry, &name, kernel.as_ref()) else {
            drop(stream);
            continue;
        };
        let app = app.clone();
        connections.spawn(async move {
            let service =
                hyper::service::service_fn(move |request: axum::http::Request<Incoming>| {
                    let mut request = request.map(Body::new);
                    request.extensions_mut().insert(caller.clone());
                    app.clone().oneshot(request)
                });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

const ACCEPT_BACKOFF_START: std::time::Duration = std::time::Duration::from_millis(50);
const ACCEPT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// Serve the endpoint of every tile this daemon's Unix user supervises, following root's
/// registry and the launcher's user IDs as they change, and each tile's endpoint directory as
/// root makes it.
pub fn spawn(state: AppState) {
    spawn_from(state, PathBuf::from(DEFAULT_REGISTRY));
}

/// [`spawn`], following the registry at `path`, which root must control just the same.
pub fn spawn_from(state: AppState, path: PathBuf) {
    let load = {
        let path = path.clone();
        move || Registry::load(&path)
    };
    // SAFETY: getuid has no preconditions.
    spawn_with(state, path, load, Arc::new(Proc), unsafe { libc::getuid() });
}

/// Follow the registry at `registry_path`, read with `load`: root's file, checked, for the daemon.
fn spawn_with(
    state: AppState,
    registry_path: PathBuf,
    load: impl Fn() -> anyhow::Result<Registry> + Send + 'static,
    kernel: Arc<dyn Kernel>,
    supervisor: u32,
) {
    tokio::spawn(crate::task_cpu::account("tile.endpoints", async move {
        // Each served tile with the user ID it was served for: a tile the launcher gives a new
        // ID is served afresh, never under the ID it had.
        type Served = (Tile, Option<u32>);
        let mut serving: BTreeMap<String, (Served, tokio::task::JoinHandle<()>)> = BTreeMap::new();
        let mut reported = None;
        // The launcher's record lives under the registry's state root, known once it is read.
        let mut uids_path: Option<PathBuf> = None;
        loop {
            // Both watches start before the read, so nothing written in between is missed.
            let changed = Change::watch(&registry_path);
            let uids_changed = uids_path.as_deref().map(Change::watch);
            let registry = match load() {
                Ok(registry) => Some(Arc::new(registry)),
                Err(error) => {
                    let missing = error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    });
                    let message = format!("{error:#}");
                    if !missing && reported.as_ref() != Some(&message) {
                        eprintln!("st3: tile endpoints: {message}");
                        reported = Some(message);
                    }
                    None
                }
            };
            if let Some(registry) = &registry
                && uids_path.as_ref() != Some(&registry.uids_file())
            {
                uids_path = Some(registry.uids_file());
                continue;
            }
            let wanted = registry
                .as_ref()
                .map(|registry| {
                    registry
                        .tiles
                        .iter()
                        .filter(|(_, tile)| tile.supervisor_uid == Some(supervisor))
                        .map(|(name, tile)| (name.clone(), (tile.clone(), registry.uid(name))))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            withdraw_changed(&mut serving, &wanted, WITHDRAW_WAIT).await;
            if let Some(registry) = &registry {
                for (name, served) in wanted {
                    if serving.contains_key(&name) {
                        continue;
                    }
                    let task = tokio::spawn(crate::task_cpu::account("tile.endpoint", serve_tile(
                        state.clone(),
                        Arc::clone(registry),
                        name.clone(),
                        Arc::clone(&kernel),
                    )));
                    serving.insert(name, (served, task));
                }
            }
            match uids_changed {
                Some(uids_changed) => {
                    tokio::select! {
                        () = changed => {}
                        () = uids_changed => {}
                    }
                }
                None => changed.await,
            }
        }
    }));
}

/// How long the supervisor waits for one withdrawn tile's task to be dropped. A task only ends
/// at an await point, so this is normally immediate; the bound keeps one stuck task from
/// stalling the endpoints of every other tile.
const WITHDRAW_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Stop every served tile that `wanted` no longer names as it was served, and return once each
/// one's task is gone, or has not gone within `wait` (logged, then carried on). Its listener
/// closes when the task is dropped, which `abort` alone leaves for later: the replacement would
/// then bind while the old listener still answers, take it for another process, give up, and
/// leave a dead socket behind with nothing to retry on.
async fn withdraw_changed<S: PartialEq, K: Ord + Clone + std::fmt::Display>(
    serving: &mut BTreeMap<K, (S, tokio::task::JoinHandle<()>)>,
    wanted: &BTreeMap<K, S>,
    wait: std::time::Duration,
) {
    let changed: Vec<K> = serving
        .iter()
        .filter(|(name, (served, _))| wanted.get(*name) != Some(served))
        .map(|(name, _)| name.clone())
        .collect();
    for name in changed {
        if let Some((_, mut task)) = serving.remove(&name) {
            task.abort();
            match tokio::time::timeout(wait, &mut task).await {
                // Cancelled is the expected outcome; a task that had already ended is the same.
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => eprintln!("st3: tile {name}'s endpoint task failed: {error}"),
                Err(_) => eprintln!(
                    "st3: tile {name}'s endpoint did not stop within {} s; carrying on",
                    wait.as_secs()
                ),
            }
        }
    }
}

/// Serve one tile's endpoint once root has made its directory for this Unix user.
async fn serve_tile(
    state: AppState,
    registry: Arc<Registry>,
    name: String,
    kernel: Arc<dyn Kernel>,
) {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let directory = registry.endpoint_dir(&name);
    loop {
        Change::exists(&directory).await;
        match bind(&directory, uid) {
            Ok(listener) => {
                serve_listener(
                    state.clone(),
                    Arc::clone(&registry),
                    name.clone(),
                    listener,
                    Arc::clone(&kernel),
                )
                .await
            }
            Err(error) => {
                eprintln!("st3: tile {name}'s endpoint: {error:#}");
                // Wait for root to change the directory before trying again.
                Change::watch(&directory).await;
            }
        }
    }
}

fn bind(directory: &Path, uid: u32) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        anyhow::bail!(
            "{} is not a directory of Unix user {uid}; root makes it when the tile launches",
            directory.display()
        );
    }
    let socket = directory.join(SOCKET);
    if let Ok(existing) = std::fs::symlink_metadata(&socket) {
        // A socket this user left behind and no one listens on is ours to replace.
        if !existing.file_type().is_socket() || existing.uid() != uid {
            anyhow::bail!("{} is not this daemon's socket", socket.display());
        }
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            anyhow::bail!("another process already serves {}", socket.display());
        }
        std::fs::remove_file(&socket)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    // The tile's user connects; the kernel, not the mode, says who it is.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Waiting for a path to change without polling: a watch on it, or on its nearest existing
/// ancestor while it does not exist.
struct Change;

impl Change {
    /// Resolves at the next change to `path` or, while it is missing, to its nearest existing
    /// ancestor's entries. The watch starts before this returns, so nothing between is missed.
    fn watch(path: &Path) -> impl std::future::Future<Output = ()> + Send + 'static {
        use notify::Watcher as _;
        let wake = Arc::new(tokio::sync::Notify::new());
        let signal = Arc::clone(&wake);
        let watched = std::iter::successors(Some(path), |path| path.parent())
            .find(|path| path.exists())
            .map(Path::to_path_buf);
        // Watch the directory holding `path`, so replacing the file is seen too.
        let target = match &watched {
            Some(found) if found == path => path.parent().map(Path::to_path_buf),
            other => other.clone(),
        };
        let watcher = target.and_then(|target| {
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if event.is_ok_and(|event| !matches!(event.kind, notify::EventKind::Access(_)))
                    {
                        signal.notify_one();
                    }
                })
                .ok()?;
            watcher
                .watch(&target, notify::RecursiveMode::NonRecursive)
                .ok()?;
            Some(watcher)
        });
        async move {
            let _watcher = watcher;
            wake.notified().await;
        }
    }

    /// Resolves once `path` exists.
    async fn exists(path: &Path) {
        loop {
            let changed = Self::watch(path);
            if path.exists() {
                return;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::sync::Mutex;

    struct FakeKernel(Mutex<String>);

    impl Kernel for Arc<FakeKernel> {
        fn cgroup(&self, _pid: i32) -> Option<String> {
            Some(self.0.lock().unwrap().clone())
        }
    }

    fn caller() -> TileCaller {
        TileCaller {
            tile: "atlas".into(),
            agent: "agent/fleet/fixture-example/atlas".into(),
            person: "person/ada".into(),
            pid: 7,
        }
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        request.extensions_mut().insert(caller());
        let response = tower::ServiceExt::oneshot(app.clone(), request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn message(from: &str, to: &str, key: &str) -> Value {
        json!({"idempotency_key": key, "from": from, "to": to, "content": "hello"})
    }

    /// Sets its flag when dropped, as a listener closes when its task is.
    struct Closes(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for Closes {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_changed_tile_is_gone_before_its_replacement_may_bind() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let hold = |closed: &Arc<AtomicBool>| {
            let guard = Closes(Arc::clone(closed));
            tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
        };
        let (changed, kept) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let mut serving =
            BTreeMap::from([("atlas", (1, hold(&changed))), ("birch", (1, hold(&kept)))]);
        // Let both tasks start, so abort has a running listener to cancel.
        tokio::task::yield_now().await;
        let wait = WITHDRAW_WAIT;
        withdraw_changed(
            &mut serving,
            &BTreeMap::from([("atlas", 2), ("birch", 1)]),
            wait,
        )
        .await;
        assert!(
            changed.load(Ordering::SeqCst),
            "the changed tile's listener must be closed when the withdrawal returns"
        );
        assert!(
            !kept.load(Ordering::SeqCst),
            "an unchanged tile keeps serving"
        );
        assert_eq!(serving.keys().copied().collect::<Vec<_>>(), ["birch"]);
        // A tile the registry dropped is withdrawn the same way.
        withdraw_changed(&mut serving, &BTreeMap::new(), wait).await;
        assert!(kept.load(Ordering::SeqCst));
        assert!(serving.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_will_not_stop_delays_the_withdrawal_by_the_bound_only() {
        // A blocking task that has started ignores abort, so it stands for a serving task that
        // never ends. One that has not started yet is cancelled by abort, so wait for it.
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let (started, running) = std::sync::mpsc::channel::<()>();
        let task = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = stuck.recv();
        });
        running.recv().unwrap();
        // A running blocking task keeps the paused clock from moving by itself, so step it a
        // second at a time until the withdrawal gives up; the time it took is read on that clock.
        let withdrawal = tokio::spawn(async move {
            let mut serving = BTreeMap::from([("atlas", (1, task))]);
            let began = tokio::time::Instant::now();
            withdraw_changed(&mut serving, &BTreeMap::new(), WITHDRAW_WAIT).await;
            (began.elapsed(), serving.is_empty())
        });
        while !withdrawal.is_finished() {
            tokio::task::yield_now().await;
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        let result = withdrawal.await.unwrap();
        // Free the blocked thread before asserting, so a failure cannot hang the runtime's shutdown.
        drop(release);
        assert!(
            result.0 >= WITHDRAW_WAIT && result.0 < WITHDRAW_WAIT * 2,
            "the wait is the bound, not shorter or unbounded: {:?}",
            result.0
        );
        assert!(result.1, "the stuck tile is dropped from the supervisor");
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_panicked_is_reported_and_does_not_stop_the_withdrawal() {
        let task = tokio::spawn(async { panic!("listener task failed") });
        let mut serving = BTreeMap::from([("atlas", (1, task))]);
        tokio::task::yield_now().await;
        withdraw_changed(&mut serving, &BTreeMap::new(), WITHDRAW_WAIT).await;
        assert!(serving.is_empty());
    }

    #[tokio::test]
    async fn a_tile_acts_only_as_its_own_agent() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let app = router(state.clone());
        let me = "agent/fleet/fixture-example/atlas";
        let other = "agent/fleet/fixture-example/birch";

        let (status, seen) = call(&app, "GET", "/v1/tile/whoami", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(seen["agent"], me);
        assert_eq!(seen["tile"], "atlas");

        // Sending as itself works, with or without naming itself.
        let (status, sent) =
            call(&app, "POST", "/v1/messages", Some(message(me, other, "a"))).await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert_eq!(sent["from"], me);
        let (status, sent) =
            call(&app, "POST", "/v1/messages", Some(message("", other, "b"))).await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert_eq!(sent["from"], me);
        // Naming anyone else is refused, not rewritten.
        for from in [other, "person/ada", "fleet/fixture-example/birch"] {
            let (status, refused) =
                call(&app, "POST", "/v1/messages", Some(message(from, me, "c"))).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{from}: {refused}");
            assert_eq!(refused["code"], "tile-forbidden");
        }
        let mut attached = message(me, other, "d");
        attached["attachments"] = json!([{"blob": "blob/x", "media_type": "image/png"}]);
        let (status, _) = call(&app, "POST", "/v1/messages", Some(attached)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Its own mailbox, never another's.
        let (status, mail) = call(&app, "GET", "/v1/messages", None).await;
        assert_eq!(status, StatusCode::OK, "{mail}");
        let (status, _) = call(&app, "GET", &format!("/v1/messages?to={other}"), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A message between others is not found; its own are.
        let between_others = {
            let state = state.clone();
            let request: MessageSendRequest =
                serde_json::from_value(message(other, "agent/fleet/fixture-example/cedar", "e"))
                    .unwrap();
            super::super::accept_message_receipt(&state, request, None, None).unwrap()
        };
        let (status, _) = call(
            &app,
            "GET",
            &format!("/v1/messages/read/{}", between_others.message.subject),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, own) = call(
            &app,
            "GET",
            &format!("/v1/messages/read/{}", sent["subject"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{own}");

        // Everything else the daemon serves is refused.
        for (method, path) in [
            ("GET", "/v1/agents"),
            ("POST", "/v1/intent/apply"),
            ("POST", "/v1/work/ask"),
            ("GET", "/v1/documents"),
            ("GET", "/v1/client/terminals"),
            ("POST", "/v1/sekrets/attest"),
            ("GET", "/v1/status"),
        ] {
            let (status, _) = call(&app, method, path, Some(json!({}))).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }

    fn registry(uid: u32) -> Registry {
        Registry::parse(&format!(
            "uids = {{ first = {uid}, last = {} }}\n[tiles.atlas]\n\
             agent = \"agent/fleet/fixture-example/atlas\"\nperson = \"person/ada\"\n\n\
             [tiles.birch]\nagent = \"agent/fleet/fixture-example/birch\"\nperson = \"person/ada\"\n",
            uid + 1
        ))
        .unwrap()
        .with_uids(&format!("atlas = {uid}\nbirch = {}\n", uid + 1))
        .unwrap()
    }

    /// One raw HTTP request over the socket; `None` when the endpoint closed without a reply.
    fn get(socket: &Path, path: &str) -> Option<String> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let _ = write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: tile\r\nConnection: close\r\n\r\n"
        );
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        (!response.is_empty()).then_some(response)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_tile_the_kernel_names_is_served() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let registry = Arc::new(registry(uid));
        let kernel = Arc::new(FakeKernel(Mutex::new(String::new())));
        let socket = root.path().join(SOCKET);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(serve_listener(
            state,
            Arc::clone(&registry),
            "atlas".into(),
            listener,
            Arc::new(Arc::clone(&kernel)),
        ));
        let set = |cgroup: String| *kernel.0.lock().unwrap() = cgroup;
        let whoami =
            |socket: PathBuf| tokio::task::spawn_blocking(move || get(&socket, "/v1/tile/whoami"));

        set(crate::tile_registry::tools_cgroup("atlas"));
        let response = whoami(socket.clone()).await.unwrap().unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("agent/fleet/fixture-example/atlas"));

        // This user outside the tile's slice, in another tile's slice, or in its login session
        // is no one: the connection closes unanswered.
        for cgroup in [
            crate::tile_registry::tools_cgroup("birch"),
            format!("/user.slice/user-{uid}.slice/session-1.scope"),
            format!("/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/x.scope"),
        ] {
            set(cgroup.clone());
            assert_eq!(whoami(socket.clone()).await.unwrap(), None, "{cgroup}");
        }
    }

    /// Write one keep-alive request on `stream` and read its response; `None` once the endpoint
    /// has closed the connection.
    fn keep_alive_get(stream: &mut std::os::unix::net::UnixStream, path: &str) -> Option<String> {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: tile\r\n\r\n").ok()?;
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => response.push(byte[0]),
                _ => return None,
            }
        }
        let head = String::from_utf8_lossy(&response).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).ok()?;
        Some(format!("{head}{}", String::from_utf8_lossy(&body)))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_daemon_follows_the_registry_and_withdraws_open_connections() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        // This test process is the tile; the daemon serves as if it were another user, its
        // supervisor, so the registry can name both without a tile supervising itself.
        let supervisor = uid + 1;
        let runtime = root.path().join("run");
        let state_root = root.path().join("state");
        let registry_path = root.path().join("etc").join("tiles.toml");
        let kernel = Arc::new(FakeKernel(Mutex::new(
            crate::tile_registry::tools_cgroup("atlas"),
        )));
        let load = {
            let path = registry_path.clone();
            // The daemon requires root's files; here the same text is parsed without that check.
            move || {
                let registry = Registry::parse(&std::fs::read_to_string(&path)?)?;
                match std::fs::read_to_string(registry.uids_file()) {
                    Ok(uids) => registry.with_uids(&uids),
                    Err(_) => Ok(registry),
                }
            }
        };
        spawn_with(
            state,
            registry_path.clone(),
            load,
            Arc::new(Arc::clone(&kernel)),
            supervisor,
        );
        let socket = runtime.join("atlas").join("endpoint").join(SOCKET);
        // Served means something listens; who may talk to it is the other tests' concern.
        let served = |socket: PathBuf| std::os::unix::net::UnixStream::connect(socket).ok();
        let until = |socket: PathBuf, want: bool| async move {
            // The daemon reacts to file system events; give it a bounded moment to do so.
            for _ in 0..200 {
                if served(socket.clone()).is_some() == want {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!(
                "the endpoint was not {}served",
                if want { "" } else { "un" }
            );
        };

        // The registry appears after the daemon starts; then root makes the endpoint directory.
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        let text = format!(
            "runtime_root = {runtime:?}\nstate_root = {state_root:?}\n\
             uids = {{ first = {uid}, last = {uid} }}\n\
             [tiles.atlas]\nagent = \"agent/fleet/fixture-example/atlas\"\n\
             person = \"person/ada\"\nsupervisor_uid = {supervisor}\n"
        );
        std::fs::write(&registry_path, &text).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        until(socket.clone(), true).await;

        // Before the launcher gives the tile its user ID, nothing is the tile: this process's
        // connection closes unanswered.
        let whoami = |socket: PathBuf| {
            tokio::task::block_in_place(|| {
                let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
                keep_alive_get(&mut stream, "/v1/tile/whoami")
            })
        };
        assert_eq!(
            whoami(socket.clone()),
            None,
            "a tile with no user ID answered"
        );
        // Once the launcher records it, the daemon serves the tile under that ID.
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("uids.toml"), format!("atlas = {uid}\n")).unwrap();
        let mut answered = None;
        for _ in 0..200 {
            answered = whoami(socket.clone());
            if answered.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let answered = answered.expect("the endpoint never served the allocated tile");
        assert!(answered.starts_with("HTTP/1.1 200"), "{answered}");

        // A connection kept open across requests acts as the tile...
        let mut open = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let response =
            tokio::task::block_in_place(|| keep_alive_get(&mut open, "/v1/tile/whoami")).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("agent/fleet/fixture-example/atlas"),
            "{response}"
        );

        // ...until the registry withdraws the tile: the endpoint stops, and the open connection
        // is closed rather than left acting for the withdrawn identity.
        std::fs::write(
            &registry_path,
            text.replace(&format!("supervisor_uid = {supervisor}\n"), ""),
        )
        .unwrap();
        until(socket.clone(), false).await;
        let after = tokio::task::block_in_place(|| keep_alive_get(&mut open, "/v1/tile/whoami"));
        assert_eq!(after, None, "the open connection still answers");
    }
}
