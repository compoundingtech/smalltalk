//! SQL audit: how many statements, how much SQLite work and how long each daemon read takes on a
//! copy of a real store. It counts every statement (`smallclaims::sqlite::work` and
//! `smallclaims::sqlite::histogram`) and times each request from the client side.
//!
//! ```sh
//! SQL_AUDIT_STORE=/path/to/copy-of-claims.sqlite3 SQL_AUDIT_OUT=/path/to/out \
//!   SQL_AUDIT_NODE=node-a SQL_AUDIT_FLEET=<fleet id> SQL_AUDIT_PERSON=person/ada \
//!   cargo test --release -p st3 --test integration sql_audit:: -- --nocapture --test-threads 1
//! ```
//!
//! The store is opened in place and may be written (an agent-card invalidation writes one
//! observation), so point it at a scratch copy made with the SQLite backup API, never the live
//! database. `SQL_AUDIT_ONLY` limits the run to routes containing one of its comma-separated
//! words. Nothing here asserts; it reports to `SQL_AUDIT_OUT/reads.json`.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smallclaims::sqlite::{histogram, work};
use st3::api::AppState;
use st3::client::Client;
use st3::store::Store;
use tokio::sync::{Notify, watch};

use crate::daemon_bench::stub_pty;

/// Every local GET route with a sample request. `{name}` is filled from what the lists return.
const READS: &[(&str, &str)] = &[
    ("GET /v1/health", "/v1/health"),
    ("GET /v1/schema", "/v1/schema"),
    ("GET /v1/client/capabilities", "/v1/client/capabilities"),
    ("GET /v1/client/sets", "/v1/client/sets"),
    ("GET /v1/client/glasses", "/v1/client/glasses"),
    ("GET /v1/client/request-latency", "/v1/client/request-latency"),
    ("GET /v1/client/usage", "/v1/client/usage"),
    ("GET /v1/client/clients", "/v1/client/clients"),
    ("GET /v1/client/now", "/v1/client/now"),
    ("GET /v1/client/machines", "/v1/client/machines"),
    ("GET /v1/client/devices", "/v1/client/devices"),
    ("GET /v1/client/attention", "/v1/client/attention"),
    ("GET /v1/client/attention/{*id}", "/v1/client/attention/{attention}"),
    ("GET /v1/client/mail-backlog", "/v1/client/mail-backlog"),
    ("GET /v1/client/messages", "/v1/client/messages"),
    ("GET /v1/client/messages/{*id}", "/v1/client/messages/{message}"),
    ("GET /v1/client/launches", "/v1/client/launches"),
    ("GET /v1/client/launches/{id}", "/v1/client/launches/{launch}"),
    ("GET /v1/client/work", "/v1/client/work"),
    ("GET /v1/client/work/{*id}", "/v1/client/work/{step}"),
    ("GET /v1/client/agents", "/v1/client/agents"),
    ("GET /v1/client/agents?limit=1", "/v1/client/agents?limit=1"),
    ("GET /v1/client/agents?history=true&limit=1", "/v1/client/agents?history=true&limit=1"),
    ("GET /v1/client/agents/{*id}", "/v1/client/agents/{agent}"),
    ("GET /v1/client/agent-workspaces/{*id}", "/v1/client/agent-workspaces/{agent}"),
    ("GET /v1/client/status-history/{*id}", "/v1/client/status-history/{agent}"),
    ("GET /v1/client/agent-declarations/{*id}", "/v1/client/agent-declarations/{agent}"),
    ("GET /v1/client/agent-queues/{*id}", "/v1/client/agent-queues/{agent}"),
    ("GET /v1/client/lanes", "/v1/client/lanes"),
    ("GET /v1/client/history", "/v1/client/history"),
    ("GET /v1/client/sessions", "/v1/client/sessions"),
    ("GET /v1/client/sessions/{*id}", "/v1/client/sessions/{session}"),
    ("GET /v1/client/conversations/search", "/v1/client/conversations/search?text=invented&limit=20"),
    ("GET /v1/client/missions", "/v1/client/missions"),
    ("GET /v1/client/missions-tree", "/v1/client/missions-tree"),
    ("GET /v1/client/missions/{*id}", "/v1/client/missions/{mission}"),
    ("GET /v1/client/resources", "/v1/client/resources"),
    ("GET /v1/client/runtimes", "/v1/client/runtimes"),
    ("GET /v1/client/runtimes/{*id}", "/v1/client/runtimes/{runtime}"),
    ("GET /v1/client/observers", "/v1/client/observers"),
    ("GET /v1/client/observers/{*id}", "/v1/client/observers/{observer}"),
    ("GET /v1/client/subscriptions", "/v1/client/subscriptions"),
    ("GET /v1/client/subscriptions/{*id}", "/v1/client/subscriptions/{subscription}"),
    ("GET /v1/client/terminals", "/v1/client/terminals"),
    ("GET /v1/client/operations", "/v1/client/operations"),
    ("GET /v1/client/events", "/v1/client/events"),
    ("GET /v1/missions/{id}", "/v1/missions/{mission_name}"),
    ("GET /v1/launches/{id}", "/v1/launches/{launch}"),
    ("GET /v1/documents", "/v1/documents"),
    ("GET /v1/rules", "/v1/rules"),
    ("GET /v1/rules/audit", "/v1/rules/audit"),
    ("GET /v1/delivery/hold", "/v1/delivery/hold?subject={seat}"),
    ("GET /v1/claims", "/v1/claims?limit=100"),
    ("GET /v1/usage", "/v1/usage"),
    ("GET /v1/reviews", "/v1/reviews"),
    ("GET /v1/attention", "/v1/attention"),
    ("GET /v1/messages", "/v1/messages?to={seat}"),
    ("GET /v1/messages/page", "/v1/messages/page?include_closed=false&limit=100&to={seat}"),
    ("GET /v1/messages/read/{*subject}", "/v1/messages/read/{message}"),
    ("GET /v1/status", "/v1/status?subject={seat}"),
    ("GET /v1/status (every subject)", "/v1/status"),
    ("GET /v1/desired/{*subject}", "/v1/desired/{seat}"),
    ("GET /v1/events", "/v1/events?limit=100"),
    ("GET /v1/events (recent cursor)", "/v1/events?after={recent_index}"),
    ("GET /v1/doctor", "/v1/doctor"),
    ("GET /v1/repair", "/v1/repair"),
    ("GET /v1/replication/status", "/v1/replication/status"),
    ("GET /v1/replication/records", "/v1/replication/records"),
    ("GET /v1/replication/records/{*record}", "/v1/replication/records/{record}"),
    ("GET /v1/checkpoint/status", "/v1/checkpoint/status"),
    ("GET /v1/internal/fleet/membership", "/v1/internal/fleet/membership"),
    ("GET /v1/internal/fleet/status", "/v1/internal/fleet/status"),
    ("GET /v1/mission-runs", "/v1/mission-runs?mission={mission_name}"),
    ("GET /v1/mission-overview", "/v1/mission-overview?mission={mission_name}"),
    ("GET /v1/outcome-history", "/v1/outcome-history?collection=work&limit=50"),
    ("GET /v1/performance", "/v1/performance"),
    ("GET /v1/mission-runs/{run}", "/v1/mission-runs/{run}"),
    ("GET /v1/work", "/v1/work?actor={seat}"),
    ("GET /v1/work (every seat)", "/v1/work"),
    ("GET /v1/work-items/{*subject}", "/v1/work-items/{step}"),
    ("GET /v1/lanes", "/v1/lanes"),
    ("GET /v1/sessions", "/v1/sessions"),
];

/// Seconds a single request may take before the audit records a timeout and goes on.
static ROUND_WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const LIMIT: Duration = Duration::from_secs(180);

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

#[derive(Clone, Default)]
struct Sample {
    ms: f64,
    statements: u64,
    vm_steps: u64,
    fullscan_steps: u64,
    sorts: u64,
    autoindex_rows: u64,
    bytes: usize,
    error: Option<String>,
    shapes: Vec<(String, u64, f64)>,
}

impl Sample {
    fn json(&self) -> Value {
        json!({
            "ms": (self.ms * 1000.0).round() / 1000.0, "statements": self.statements,
            "vm_steps": self.vm_steps, "fullscan_steps": self.fullscan_steps,
            "sorts": self.sorts, "autoindex_rows": self.autoindex_rows, "bytes": self.bytes,
            "error": self.error,
            "top_shapes": self.shapes.iter().map(|(sql, count, ms)| json!({"count": count, "total_ms": ms, "sql": sql})).collect::<Vec<_>>(),
        })
    }
}

async fn once(client: &Client, path: &str) -> Sample {
    // Let work the previous request left to the daemon finish, so it is not counted here.
    settle().await;
    histogram::take();
    let before = work::total();
    let started = Instant::now();
    let answer = tokio::time::timeout(LIMIT, client.get::<Value>(path)).await;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    settle().await;
    let spent = work::total() - before;
    let mut shapes = histogram::take()
        .into_iter()
        .map(|(sql, shape)| (sql, shape.count, shape.total_ns as f64 / 1e6))
        .collect::<Vec<_>>();
    shapes.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)));
    shapes.truncate(12);
    let (bytes, error) = match answer {
        Ok(Ok(value)) => (serde_json::to_vec(&value).map_or(0, |bytes| bytes.len()), None),
        Ok(Err(error)) => (0, Some(error.to_string().chars().take(160).collect())),
        Err(_) => (0, Some("timed out".to_owned())),
    };
    Sample {
        ms,
        statements: spent.statements,
        vm_steps: spent.vm_steps,
        fullscan_steps: spent.fullscan_steps,
        sorts: spent.sorts,
        autoindex_rows: spent.autoindex_rows,
        bytes,
        error,
        shapes,
    }
}

/// A GET without the client's fifteen-second limit, for routes the CLI cannot finish: returns the
/// status line's code and the response size. `person` is sent as the client authority.
async fn raw_get(socket: &Path, path: &str, person: Option<&str>) -> Result<(u16, usize), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| e.to_string())?;
    let header = person.map(|p| format!("x-st3-person: {p}\r\n")).unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{header}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut bytes = 0usize;
    let mut first = Vec::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = stream.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if first.len() < 32 {
            first.extend_from_slice(&buffer[..n.min(32)]);
        }
        bytes += n;
    }
    let status = String::from_utf8_lossy(&first).split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((status, bytes))
}

/// The same measurement as [`once`], through [`raw_get`], with a ten-minute limit.
async fn once_raw(socket: &Path, path: &str, person: Option<&str>) -> Sample {
    settle().await;
    histogram::take();
    let before = work::total();
    let started = Instant::now();
    let answer = tokio::time::timeout(Duration::from_secs(600), raw_get(socket, path, person)).await;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    settle().await;
    let spent = work::total() - before;
    let mut shapes = histogram::take()
        .into_iter()
        .map(|(sql, shape)| (sql, shape.count, shape.total_ns as f64 / 1e6))
        .collect::<Vec<_>>();
    shapes.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)));
    shapes.truncate(12);
    let (bytes, error) = match answer {
        Ok(Ok((status, bytes))) if (200..300).contains(&status) => (bytes, None),
        Ok(Ok((status, bytes))) => (bytes, Some(format!("HTTP {status}"))),
        Ok(Err(error)) => (0, Some(error)),
        Err(_) => (0, Some("timed out after 600 s".to_owned())),
    };
    Sample {
        ms,
        statements: spent.statements,
        vm_steps: spent.vm_steps,
        fullscan_steps: spent.fullscan_steps,
        sorts: spent.sorts,
        autoindex_rows: spent.autoindex_rows,
        bytes,
        error,
        shapes,
    }
}

async fn settle() {
    let mut last = work::total();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = work::total();
        if now.statements == last.statements {
            return;
        }
        last = now;
    }
}

async fn ids(client: &Client, path: &str, fields: &[&str]) -> Vec<String> {
    let page = client.get::<Value>(path).await.unwrap_or(Value::Null);
    let page = page.get("value").unwrap_or(&page);
    let items = page
        .get("items")
        .or_else(|| page.get("records"))
        .or_else(|| page.get("documents"))
        .or_else(|| page.get("claims"))
        .or(Some(page))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            fields
                .iter()
                .find_map(|field| item.get(*field).and_then(Value::as_str))
        })
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql_audit_reads() {
    if std::env::var_os("SQL_AUDIT_STORE").is_none() {
        println!("skipped: set SQL_AUDIT_STORE to a scratch copy of a store");
        return;
    }
    let database = PathBuf::from(env("SQL_AUDIT_STORE"));
    let out = PathBuf::from(env("SQL_AUDIT_OUT"));
    let node = env("SQL_AUDIT_NODE");
    let fleet = env("SQL_AUDIT_FLEET");
    let person = env("SQL_AUDIT_PERSON");
    let seat = env("SQL_AUDIT_SEAT");
    let only = std::env::var("SQL_AUDIT_ONLY").ok();
    std::fs::create_dir_all(&out).unwrap();
    // A Unix socket path must fit in about a hundred bytes, so the run directory is short.
    let root = PathBuf::from(format!("/var/tmp/sqa-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    histogram::take();
    let before = work::total();
    let opened = Instant::now();
    let store = Arc::new(Store::open(&database, node.clone()).unwrap());
    let open_ms = opened.elapsed().as_secs_f64() * 1000.0;
    let open_work = work::total() - before;
    let open_shapes = histogram::take();
    println!(
        "open: {open_ms:.0} ms, {} statements, {} vm steps",
        open_work.statements, open_work.vm_steps
    );
    store.bind_fleet(&fleet).ok();
    let claims = store.index().unwrap();

    let socket = root.join("st3.sock");
    let pty = stub_pty(&root);
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.clone(),
        state_dir: root.join("state"),
        pty_root: root.join("pty"),
        pty_binary: pty,
        fleet_id: Some(fleet.clone()),
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(state)).await
    });
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix(&socket);
    let human = Client::unix_as(&socket, person.clone()).unwrap();

    // Items the detail routes need.
    let first = |items: Vec<String>| items.into_iter().next().unwrap_or_default();
    let mut items = BTreeMap::<&str, String>::new();
    {
        let page = human.get::<Value>("/v1/client/agents?limit=1").await.unwrap_or(Value::Null);
        println!("agents page: {}", page.to_string().chars().take(500).collect::<String>());
    }
    items.insert("agent", first(ids(&human, "/v1/client/agents?limit=1", &["id", "subject", "name"]).await));
    items.insert("mission", first(ids(&human, "/v1/client/missions?limit=1", &["id"]).await));
    items.insert("step", first(ids(&human, "/v1/client/work?limit=1", &["id"]).await));
    items.insert("message", first(ids(&human, "/v1/client/messages?limit=1", &["id"]).await));
    items.insert("launch", first(ids(&human, "/v1/client/launches?limit=1", &["id"]).await));
    items.insert("session", first(ids(&human, "/v1/client/sessions?limit=1", &["id"]).await));
    items.insert("runtime", first(ids(&human, "/v1/client/runtimes?limit=1", &["id"]).await));
    items.insert("observer", first(ids(&human, "/v1/client/observers?limit=1", &["id"]).await));
    items.insert("subscription", first(ids(&human, "/v1/client/subscriptions?limit=1", &["id"]).await));
    items.insert("attention", first(ids(&human, "/v1/client/attention?limit=1", &["id"]).await));
    items.insert("seat", seat.clone());
    items.insert(
        "record",
        first(ids(&client, "/v1/replication/records?limit=1", &["id", "record", "claim_id"]).await),
    );
    let mission_name = items["mission"].trim_start_matches("mission/").to_owned();
    items.insert("mission_name", mission_name);
    items.insert("recent_index", claims.saturating_sub(100).to_string());
    let runs = client
        .get::<Value>(&format!("/v1/mission-runs?mission={}", items["mission_name"]))
        .await
        .unwrap_or(Value::Null);
    println!("runs page: {}", runs.to_string().chars().take(300).collect::<String>());
    let run = runs
        .as_array()
        .or_else(|| runs.get("runs").and_then(Value::as_array))
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    items.insert("run", run);
    println!("items: {items:?}");

    let mut results = Vec::new();
    for (route, template) in READS {
        if let Some(only) = &only
            && !only.split(',').any(|word| route.contains(word))
        {
            continue;
        }
        let mut path = (*template).to_owned();
        for (name, value) in &items {
            path = path.replace(&format!("{{{name}}}"), &urlencoding::encode(value).replace("%2F", "/"));
        }
        if path.contains('{') || path.contains("//") && !path.contains("://") && path.ends_with('/') {
            println!("{route}: unresolved placeholder in {path}");
            results.push(json!({"route": route, "path": path, "skipped": "no sample item in this store"}));
            continue;
        }
        let heavy = ["doctor", "repair", "client/operations", "every subject", "checkpoint/status"]
            .iter()
            .any(|word| route.contains(word))
            || *route == "GET /v1/events";
        let (cold, warm1, warm2) = if heavy {
            let person = path.starts_with("/v1/client/").then_some(person.as_str());
            let cold = once_raw(&socket, &path, person).await;
            let warm = once_raw(&socket, &path, person).await;
            (cold, warm.clone(), warm)
        } else {
            let client = if path.starts_with("/v1/client/") { &human } else { &client };
            let cold = once(client, &path).await;
            let warm1 = once(client, &path).await;
            let warm2 = once(client, &path).await;
            (cold, warm1, warm2)
        };
        let warm = if warm1.statements <= warm2.statements { &warm1 } else { &warm2 };
        println!(
            "{route}: cold {:.1} ms / {} stmts; warm {:.1} ms / {} stmts / {} vm / {} scan{}",
            cold.ms,
            cold.statements,
            warm.ms,
            warm.statements,
            warm.vm_steps,
            warm.fullscan_steps,
            cold.error.as_deref().map(|e| format!("  ERROR {e}")).unwrap_or_default()
        );
        results.push(json!({"route": route, "path": path, "cold": cold.json(), "warm": warm.json(), "warm_ms_runs": [warm1.ms, warm2.ms]}));
        std::fs::write(
            out.join("reads.json"),
            serde_json::to_vec_pretty(&json!({
                "claims": claims, "open_ms": open_ms, "open_statements": open_work.statements,
                "open_vm_steps": open_work.vm_steps,
                "open_shapes": open_shapes.iter().map(|(sql, shape)| json!({"count": shape.count, "ms": shape.total_ns as f64 / 1e6, "sql": sql})).collect::<Vec<_>>(),
                "results": results,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    // An agent-card read after a write that invalidates the cards, as every seat's heartbeat does.
    if only.as_ref().is_none_or(|only| only.contains("agents")) {
        let seat = items["seat"].clone();
        for (label, path) in [
            ("cards after a seat observation (list, limit 1)", "/v1/client/agents?limit=1".to_owned()),
            ("cards after a seat observation (detail)", format!("/v1/client/agents/{}", items["agent"])),
            ("cards after an unrelated document (list, limit 1)", "/v1/client/agents?limit=1".to_owned()),
        ] {
            let unrelated = label.contains("unrelated");
            static ROUND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let round = ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if unrelated {
                let _: Value = client
                    .post(
                        "/v1/documents",
                        &json!({
                            "name": format!("doc/audit/unrelated-{round}"),
                            "bytes": b"An invented finding.".to_vec(),
                            "idempotency_key": format!("audit-document-{round}"),
                        }),
                    )
                    .await
                    .unwrap_or(Value::Null);
            } else {
                let store = store.clone();
                let seat = seat.clone();
                tokio::task::spawn_blocking(move || {
                    let mut observation = crate::daemon_bench::claim_input(
                        "harness.observed",
                        &format!("audit-observed-{round}"),
                        round,
                        "",
                    );
                    observation.subject = seat.clone();
                    observation.actor = Some(seat);
                    observation.fields.insert("state".into(), json!(if round % 2 == 0 { "idle" } else { "working" }));
                    store.append_claim(&observation).unwrap();
                })
                .await
                .unwrap();
            }
            let cold = once(&human, &path).await;
            let warm = once(&human, &path).await;
            println!(
                "{label}: {:.1} ms / {} stmts / {} vm; then warm {:.1} ms / {} stmts{}",
                cold.ms, cold.statements, cold.vm_steps, warm.ms, warm.statements,
                cold.error.as_deref().map(|e| format!("  ERROR {e}")).unwrap_or_default()
            );
            results.push(json!({"route": label, "path": path, "cold": cold.json(), "warm": warm.json()}));
        }
        std::fs::write(
            out.join("reads.json"),
            serde_json::to_vec_pretty(&json!({
                "claims": claims, "open_ms": open_ms, "open_statements": open_work.statements,
                "open_vm_steps": open_work.vm_steps,
                "open_shapes": open_shapes.iter().map(|(sql, shape)| json!({"count": shape.count, "ms": shape.total_ns as f64 / 1e6, "sql": sql})).collect::<Vec<_>>(),
                "results": results,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    // Writes: the claims seats and the daemon append most, on a seat with a long history, each
    // with fresh values so nothing is a duplicate. Counted from the store call, as a request's
    // handler does it; the writer's own transaction is inside.
    let mut writes = Vec::new();
    if only.as_ref().is_none_or(|only| only.contains("write")) {
        for kind in [
            "harness.observed",
            "harness.usage",
            "harness.limits",
            "harness.timeline",
            "runtime.observed",
            "work.progress",
        ] {
            let mut samples = Vec::new();
            for attempt in 0..4usize {
                let store = store.clone();
                let seat = seat.clone();
                settle().await;
                histogram::take();
                let before = work::total();
                let started = Instant::now();
                let outcome = tokio::task::spawn_blocking(move || {
                    let round = ROUND_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut input = crate::daemon_bench::claim_input(
                        kind,
                        &format!("audit-write-{kind}-{round}"),
                        round + 17,
                        "",
                    );
                    input.subject = seat.clone();
                    input.actor = Some(seat);
                    store.append_claim(&input).map(|_| ()).map_err(|e| e.message)
                })
                .await
                .unwrap();
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                settle().await;
                let spent = work::total() - before;
                let mut shapes = histogram::take()
                    .into_iter()
                    .map(|(sql, shape)| (sql, shape.count, shape.total_ns as f64 / 1e6))
                    .collect::<Vec<_>>();
                shapes.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)));
                shapes.truncate(12);
                samples.push(Sample {
                    ms,
                    statements: spent.statements,
                    vm_steps: spent.vm_steps,
                    fullscan_steps: spent.fullscan_steps,
                    sorts: spent.sorts,
                    autoindex_rows: spent.autoindex_rows,
                    bytes: 0,
                    error: outcome.err(),
                    shapes,
                });
            }
            println!(
                "write {kind}: first {:.1} ms / {} stmts; later {:.1} ms / {} stmts / {} vm{}",
                samples[0].ms,
                samples[0].statements,
                samples[3].ms,
                samples[3].statements,
                samples[3].vm_steps,
                samples[0].error.as_deref().map(|e| format!("  ERROR {e}")).unwrap_or_default()
            );
            writes.push(json!({"kind": kind, "first": samples[0].json(), "later": samples[3].json()}));
        }
    }
    std::fs::write(
        out.join("writes.json"),
        serde_json::to_vec_pretty(&json!({"writes": writes})).unwrap(),
    )
    .unwrap();
    server.abort();
    let _ = Path::new(&root);
}

fn process_cpu() -> Duration {
    // SAFETY: getrusage fills a plain struct for this process.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let sum = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    sum(usage.ru_utime) + sum(usage.ru_stime)
}

/// What the reconciler costs while claims arrive at the rate a fleet of seat drivers writes them.
/// It runs as a host that owns none of the store's members (`SQL_AUDIT_RECONCILER_NODE`), so it
/// starts, renders and signals nothing: its numbers are a lower bound for a host that owns seats.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql_audit_reconciler() {
    if std::env::var_os("SQL_AUDIT_STORE").is_none() || std::env::var_os("SQL_AUDIT_RECONCILER").is_none() {
        println!("skipped: set SQL_AUDIT_STORE and SQL_AUDIT_RECONCILER");
        return;
    }
    let database = PathBuf::from(env("SQL_AUDIT_STORE"));
    let out = PathBuf::from(env("SQL_AUDIT_OUT"));
    let node = env("SQL_AUDIT_RECONCILER_NODE");
    let fleet = env("SQL_AUDIT_FLEET");
    let seat = env("SQL_AUDIT_SEAT");
    let rate_ms: u64 = std::env::var("SQL_AUDIT_WRITE_EVERY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
    let root = PathBuf::from(format!("/var/tmp/sqa-rec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&database, node.clone()).unwrap());
    store.bind_fleet(&fleet).ok();
    let notify = Arc::new(Notify::new());
    let (event_notify, _events) = watch::channel(0_u64);
    let socket = root.join("st3.sock");
    let pty = stub_pty(&root);
    let state = AppState {
        store: store.clone(),
        notify: notify.clone(),
        event_notify: event_notify.clone(),
        node: node.clone(),
        state_dir: root.join("state"),
        pty_root: root.join("pty"),
        pty_binary: pty.clone(),
        fleet_id: Some(fleet.clone()),
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await });
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    smallclaims::performance::reset_for_test();
    let reconciler = Arc::new(
        st3::reconcile::Reconciler::native(
            store.clone(),
            &root.join("state"),
            Some(&root.join("pty")),
            &pty,
            node.clone(),
            socket.display().to_string(),
            notify.clone(),
            event_notify.clone(),
            None,
        )
        .unwrap(),
    );
    let task = tokio::spawn(reconciler.supervise());
    let mut report = Vec::new();
    let mut phase = |label: &str, seconds: u64, writing: bool| {
        let label = label.to_owned();
        let store = store.clone();
        let seat = seat.clone();
        async move {
            histogram::take();
            let before = work::total();
            let cpu = process_cpu();
            let started = Instant::now();
            let mut written = 0u64;
            while started.elapsed() < Duration::from_secs(seconds) {
                if writing {
                    let store = store.clone();
                    let seat = seat.clone();
                    let round = ROUND_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::task::spawn_blocking(move || {
                        let mut input = crate::daemon_bench::claim_input(
                            "harness.limits",
                            &format!("audit-rec-{round}"),
                            round + 5,
                            "",
                        );
                        input.subject = seat.clone();
                        input.actor = Some(seat);
                        let _ = store.append_claim(&input);
                    })
                    .await
                    .unwrap();
                    written += 1;
                    tokio::time::sleep(Duration::from_millis(rate_ms)).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            let spent = work::total() - before;
            let cpu = (process_cpu() - cpu).as_secs_f64();
            let secs = started.elapsed().as_secs_f64();
            let mut shapes = histogram::take()
                .into_iter()
                .map(|(sql, shape)| (sql, shape.count, shape.total_ns as f64 / 1e6))
                .collect::<Vec<_>>();
            shapes.sort_by(|a, b| b.1.cmp(&a.1));
            shapes.truncate(25);
            println!(
                "{label}: {secs:.0} s, {written} claims written, {} statements ({:.0}/s), {} vm steps, process CPU {cpu:.1} s ({:.0}% of a core)",
                spent.statements,
                spent.statements as f64 / secs,
                spent.vm_steps,
                cpu / secs * 100.0
            );
            json!({"phase": label, "seconds": secs, "claims_written": written, "statements": spent.statements,
                "statements_per_s": spent.statements as f64 / secs, "vm_steps": spent.vm_steps,
                "cpu_s": cpu, "cpu_core_fraction": cpu / secs,
                "top_shapes": shapes.iter().map(|(sql, count, ms)| json!({"count": count, "total_ms": ms, "sql": sql})).collect::<Vec<_>>()})
        }
    };
    report.push(phase("startup (first 60 s, no writes)", 60, false).await);
    report.push(phase("idle (60 s, no writes)", 60, false).await);
    report.push(phase("claims arriving (120 s)", 120, true).await);
    report.push(phase("idle again (30 s)", 30, false).await);
    let snapshot = smallclaims::performance::snapshot();
    println!("performance: {}", snapshot.to_string().chars().take(3000).collect::<String>());
    std::fs::write(
        out.join("reconciler.json"),
        serde_json::to_vec_pretty(&json!({"phases": report, "performance": snapshot})).unwrap(),
    )
    .unwrap();
    task.abort();
    server.abort();
}
