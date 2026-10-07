//! Opt-in benchmark against a standalone, copied real store; never a live daemon or database.
//! ST3_AGENT_RESOURCES_BENCH_STORE is required. The input must have no outstanding WAL.
//! All writes and the Unix listener live in a temporary directory removed after the run.
//! Timings include an actual limit=1 HTTP request, body transfer, and JSON decoding;
//! "cold" means a cold agent projection, not a flushed operating-system page cache.
//! No extra full-card reads: each scenario measures only its selected HTTP route.

use super::*;
use std::time::Instant;

fn positive_env(name: &str, default: usize) -> usize {
    let value = std::env::var(name)
        .map(|value| value.parse::<usize>().expect("benchmark setting must be an integer"))
        .unwrap_or(default);
    assert!(value > 0, "{name} must be positive");
    value
}

async fn first_page(client: &reqwest::Client, _store: &Store) -> (Value, [f64; 3]) {
    let url = std::env::var("ST3_AGENT_RESOURCES_BENCH_URL")
        .unwrap_or_else(|_| "http://localhost/v1/client/agents?limit=1".into());
    let started = Instant::now();
    let response = client
        .get(&url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: Value = response.json().await.unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(envelope["value"]["collection"], "agents");
    assert!(envelope["value"]["items"].as_array().unwrap().len() <= 1);
    assert!(envelope["snapshot"]["store_index"].as_u64().is_some());
    (envelope, [elapsed_ms, 0.0, 0.0])
}

fn latency(samples: &mut [[f64; 3]], column: usize) -> Value {
    samples.sort_by(|left, right| left[column].total_cmp(&right[column]));
    let percentile = |percent: usize| {
        samples[(samples.len() * percent).div_ceil(100) - 1][column]
    };
    json!({
        "mean": samples.iter().map(|sample| sample[column]).sum::<f64>() / samples.len() as f64,
        "min": samples[0][column],
        "p50": percentile(50),
        "p95": percentile(95),
        "max": samples[samples.len() - 1][column],
    })
}

fn report(phase: &str, samples: &mut [[f64; 3]], commits: usize, commit_ms: f64, store: &Store) {
    println!(
        "{}",
        json!({
            "benchmark": "agent-resources-copied-store",
            "phase": phase,
            "limit": 1,
            "requests": samples.len(),
            "commits": commits,
            "commit_ms_total": commit_ms,
            "http_ms": latency(samples, 0),
            "store_index": store.index().unwrap(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ST3_AGENT_RESOURCES_BENCH_STORE pointing to a standalone copied real store"]
async fn copied_store_agent_resources_benchmark() {
    let source = std::path::PathBuf::from(
        std::env::var_os("ST3_AGENT_RESOURCES_BENCH_STORE")
            .expect("set ST3_AGENT_RESOURCES_BENCH_STORE to a copied, non-live SQLite store"),
    );
    let source = source.canonicalize().unwrap();
    let mut wal = source.as_os_str().to_os_string();
    wal.push("-wal");
    match std::fs::metadata(std::path::PathBuf::from(wal)) {
        Ok(metadata) => assert_eq!(metadata.len(), 0, "input must be a standalone SQLite backup"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot inspect copied-store WAL: {error}"),
    }
    let commits = positive_env("ST3_AGENT_RESOURCES_BENCH_COMMITS", 100);
    let warm_requests = positive_env("ST3_AGENT_RESOURCES_BENCH_WARM_REQUESTS", 13);
    let node = std::env::var("ST3_AGENT_RESOURCES_BENCH_NODE")
        .unwrap_or_else(|_| "benchmark-node".into());
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("claims.sqlite3");
    let copied_bytes = std::fs::copy(&source, &database).unwrap();
    let opened = Instant::now();
    let store = Arc::new(Store::open(&database, &node).unwrap());
    let open_ms = opened.elapsed().as_secs_f64() * 1_000.0;
    // Reuse the API fixture, replacing its empty store before any request or measurement.
    let mut state = tests::state(root.path());
    state.store = store.clone();
    state.node = node.clone();
    println!(
        "{}",
        json!({
            "benchmark": "agent-resources-copied-store",
            "phase": "setup",
            "source": source,
            "source_bytes": copied_bytes,
            "node": node,
            "store_open_ms": open_ms,
            "store_index": store.index().unwrap(),
            "commits_per_phase": commits,
        })
    );

    let socket = root.path().join("api.sock");
    let server_socket = socket.clone();
    let server_state = state.clone();
    let server = tokio::spawn(async move { serve_unix(&server_socket, router(server_state)).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(!server.is_finished(), "isolated API exited before binding its socket");
        assert!(tokio::time::Instant::now() < deadline, "isolated API did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let (cold_page, cold_ms) = first_page(&client, &store).await;
    report("cold_first_page", &mut [cold_ms], 0, 0.0, &store);
    let cold_page = if cold_page["value"]["items"].as_array().unwrap().is_empty() {
        client.get("http://localhost/v1/client/agents?limit=1")
            .send().await.unwrap().json::<Value>().await.unwrap()
    } else {
        cold_page
    };
    let first = &cold_page["value"]["items"][0];
    let subject = first["id"].as_str().unwrap().to_owned();
    let incarnation = first["incarnation_id"]
        .as_str()
        .unwrap_or("agent-resources-bench")
        .to_owned();

    let mut samples = Vec::with_capacity(warm_requests);
    for _ in 0..warm_requests {
        samples.push(first_page(&client, &store).await.1);
    }
    report("warm_same_index", &mut samples, 0, 0.0, &store);

    // Each independent commit is followed by a fresh first-page request, not a cursor read.
    // This catches repeated full fills across changing claim and local-observation positions.
    for local in [false, true] {
        let mut samples = Vec::with_capacity(commits);
        let mut commit_ms = 0.0;
        for sequence in 0..commits {
            let entry = format!("agent-resources-bench-{sequence}");
            let input = if local {
                ClaimInput {
                    subject: subject.clone(),
                    kind: "harness.timeline".into(),
                    actor: Some(subject.clone()),
                    fields: BTreeMap::from([
                        ("operation".into(), json!("append")),
                        ("entry_id".into(), json!(entry)),
                        ("revision".into(), json!(1)),
                        ("role".into(), json!("assistant")),
                        ("entry_type".into(), json!("content")),
                        ("final".into(), json!(true)),
                        ("body".into(), json!({"media_type": "text/plain", "text": entry})),
                        ("driver".into(), json!("codex")),
                        ("incarnation_id".into(), json!(incarnation)),
                        ("sequence".into(), json!(sequence)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("timeline:{subject}:{incarnation}:{entry}")),
                }
            } else {
                ClaimInput {
                    subject: format!("daemon/{node}"),
                    kind: "daemon.diagnostic".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("severity".into(), json!("warning")),
                        ("code".into(), json!("agent-resources-bench")),
                        ("reason".into(), json!(entry)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(entry),
                }
            };
            let previous_index = store.index().unwrap();
            let started = Instant::now();
            let record = store.append_claim(&input).unwrap();
            commit_ms += started.elapsed().as_secs_f64() * 1_000.0;
            if local {
                assert!(record.id.starts_with("local-observation/"));
                assert_eq!(store.index().unwrap(), previous_index);
            } else {
                assert!(store.index().unwrap() > previous_index);
            }
            let (page, elapsed_ms) = first_page(&client, &store).await;
            assert_eq!(page["snapshot"]["store_index"], json!(store.index().unwrap()));
            samples.push(elapsed_ms);
        }
        report(
            if local { "warm_after_local_observations" } else { "warm_after_diagnostic_commits" },
            &mut samples,
            commits,
            commit_ms,
            &store,
        );
    }
    let mut samples = Vec::with_capacity(commits);
    let mut commit_ms = 0.0;
    for sequence in 0..commits {
        let previous_index = store.index().unwrap();
        let started = Instant::now();
        // Use the production message API helper: validation, normalization, durable append,
        // and activity invalidation all run against the disposable real-store copy.
        let _response = send_message(
            State(state.clone()),
            Json(MessageSendRequest {
                idempotency_key: format!("agent-resources-bench-message-{sequence}"),
                from: subject.clone(),
                to: subject.clone(),
                content: format!("Copied-store benchmark message {sequence}"),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            }),
        )
        .await
        .unwrap();
        commit_ms += started.elapsed().as_secs_f64() * 1_000.0;
        assert!(store.index().unwrap() > previous_index);
        let (page, elapsed_ms) = first_page(&client, &store).await;
        assert_eq!(page["snapshot"]["store_index"], json!(store.index().unwrap()));
        samples.push(elapsed_ms);
    }
    report("warm_after_message_commits", &mut samples, commits, commit_ms, &store);
    server.abort();
    let _ = server.await;
}
