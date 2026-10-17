//! The OTLP exporter's tests that read the local observation log from a store.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::otlp::*;
use crate::store::Store;
use crate::model::ClaimInput;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

#[derive(Clone, Default)]
struct Collector {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    status: Arc<AtomicU16>,
    metrics_status: Arc<AtomicU16>,
    traces_status: Arc<AtomicU16>,
}

async fn collect(
    State(collector): State<Collector>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> StatusCode {
    let signal_status = match uri.path() {
        "/v1/metrics" => collector.metrics_status.load(Ordering::SeqCst),
        "/v1/traces" => collector.traces_status.load(Ordering::SeqCst),
        _ => 0,
    };
    let status = StatusCode::from_u16(if signal_status == 0 {
        collector.status.load(Ordering::SeqCst)
    } else {
        signal_status
    })
    .unwrap_or(StatusCode::OK);
    if status.is_success() {
        collector.requests.lock().unwrap().push((headers, body));
    }
    status
}

async fn start_collector() -> (Collector, String) {
    let collector = Collector::default();
    collector.status.store(200, Ordering::SeqCst);
    let app = Router::new()
        .route("/v1/logs", post(collect))
        .route("/v1/metrics", post(collect))
        .route("/v1/traces", post(collect))
        .with_state(collector.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (collector, format!("http://{address}/"))
}

fn observe(store: &Store, entry: u64) {
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.timeline".into(),
            actor: Some("agent/node.worker".into()),
            fields: BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                ("entry_id".into(), Value::String(format!("entry-{entry}"))),
                ("sequence".into(), Value::from(entry)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("assistant".into())),
                ("entry_type".into(), Value::String("content".into())),
                ("final".into(), Value::Bool(true)),
                (
                    "body".into(),
                    json!({"media_type": "text/plain", "text": format!("entry {entry}")}),
                ),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("otlp-entry-{entry}")),
        })
        .unwrap();
}

fn records(request: &Value) -> Vec<Value> {
    request["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .unwrap()
        .clone()
}

fn resource_service_name(request: &Value) -> &str {
    request["resourceLogs"]
        .get(0)
        .or_else(|| request["resourceMetrics"].get(0))
        .or_else(|| request["resourceSpans"].get(0))
        .and_then(|resource| {
            resource["resource"]["attributes"]
                .as_array()
                .and_then(|attributes| {
                    attributes
                        .iter()
                        .find(|attribute| attribute["key"] == "service.name")
                        .and_then(|attribute| attribute["value"]["stringValue"].as_str())
                })
        })
        .unwrap()
}

#[test]
fn a_local_observation_becomes_an_otlp_log_record() {
    let store = Store::open_memory("node-a").unwrap();
    observe(&store, 7);
    let batch = store.local_observations_after(0, 10).unwrap();
    let request = otlp_logs("node-a", &batch);
    let resource = &request["resourceLogs"][0]["resource"]["attributes"];
    assert!(resource.as_array().unwrap().contains(&json!({
        "key": "service.name", "value": {"stringValue": "st-daemon"}
    })));
    assert!(resource.as_array().unwrap().contains(&json!({
        "key": "st3.node", "value": {"stringValue": "node-a"}
    })));
    let record = &records(&request)[0];
    assert_eq!(record["eventName"], "harness.timeline");
    assert_eq!(
        record["timeUnixNano"],
        (batch[0].accepted_at_unix_ms * 1_000_000).to_string()
    );
    let attributes = record["attributes"].as_array().unwrap();
    for (key, value) in [
        ("st3.subject", json!({"stringValue": "agent/node.worker"})),
        ("st3.kind", json!({"stringValue": "harness.timeline"})),
        ("st3.actor", json!({"stringValue": "agent/node.worker"})),
        ("st3.incarnation_id", json!({"stringValue": "inc-1"})),
        ("st3.local_id", json!({"intValue": "1"})),
    ] {
        assert!(
            attributes.contains(&json!({"key": key, "value": value})),
            "{key}"
        );
    }
    let body = record["body"]["kvlistValue"]["values"].as_array().unwrap();
    assert!(body.contains(&json!({"key": "sequence", "value": {"intValue": "7"}})));
    assert!(body.contains(&json!({"key": "final", "value": {"boolValue": true}})));
    assert!(body.contains(&json!({
        "key": "body",
        "value": {"kvlistValue": {"values": [
            {"key": "media_type", "value": {"stringValue": "text/plain"}},
            {"key": "text", "value": {"stringValue": "entry 7"}}
        ]}}
    })));
}

#[tokio::test]
async fn each_model_response_exports_its_spend_as_a_log_and_bounded_counters() {
    let (collector, endpoint) = start_collector().await;
    let store = Arc::new(Store::open_memory("node-a").unwrap());
    let respond = |entry: u64, account: Option<&str>| {
        let mut body = json!({"semantics": "response", "model": "claude-opus-5-5",
            "input_tokens": 1000, "output_tokens": 100, "cached_tokens": 10000,
            "cache_write_tokens": 0, "total_tokens": 11100});
        if let Some(account) = account {
            body["account"] = json!(account);
        }
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "harness.timeline".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("operation".into(), json!("append")),
                    ("entry_id".into(), json!(format!("usage-{entry}"))),
                    ("source_id".into(), json!(format!("source-{entry}"))),
                    ("sequence".into(), json!(entry)),
                    ("revision".into(), json!(1)),
                    ("role".into(), json!("system")),
                    ("entry_type".into(), json!("usage")),
                    ("final".into(), json!(true)),
                    ("body".into(), body),
                    ("driver".into(), json!("claude")),
                    ("incarnation_id".into(), json!("inc-1")),
                    (
                        "observed_at_unix_ms".into(),
                        json!(1_700_000_000_000_u64 + entry),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("otlp-usage-{entry}")),
            })
            .unwrap();
    };
    respond(1, Some("claude/aaaaaaaaaaaaaaaa"));
    respond(2, Some("claude/aaaaaaaaaaaaaaaa"));
    respond(3, None);
    let exporter = OtlpExporter::new(
        &OtlpConfig {
            endpoint,
            headers_file: None,
        },
        "node-a",
    )
    .unwrap();
    assert_eq!(exporter.export_once(&store).await.unwrap().exported, 3);
    let requests = collector.requests.lock().unwrap();
    let usage = records(&requests[0].1)
        .into_iter()
        .filter(|record| record["eventName"] == "st.usage.response")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 3);
    assert_eq!(usage[0]["timeUnixNano"], "1700000000001000000");
    let attributes = usage[0]["attributes"].as_array().unwrap();
    // Opus 5.5: $4 input, $20 output and $0.20 cache reads per million tokens.
    for (key, value) in [
        ("st.agent", json!({"stringValue": "agent/node.worker"})),
        ("st.model", json!({"stringValue": "claude-opus-5-5"})),
        (
            "st.account",
            json!({"stringValue": "claude/aaaaaaaaaaaaaaaa"}),
        ),
        ("st.basis", json!({"stringValue": "estimated"})),
        ("st.step", json!({"stringValue": "unknown"})),
        ("st.tokens.cache_read", json!({"intValue": "10000"})),
        ("st.cost.microusd", json!({"intValue": "8000"})),
    ] {
        assert!(
            attributes.contains(&json!({"key": key, "value": value})),
            "{key}: {attributes:#?}"
        );
    }
    let usage_body = requests
        .iter()
        .find(|(_, body)| body.get("resourceMetrics").is_some())
        .map(|(_, body)| body)
        .unwrap();
    assert_eq!(
        resource_service_name(usage_body),
        "st-daemon",
        "usage counters are daemon bookkeeping, not per-hook signals"
    );
    let metrics = &usage_body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
    let cost = metrics
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["name"] == "st_usage_cost_microusd_total")
        .unwrap();
    let points = cost["sum"]["dataPoints"].as_array().unwrap();
    assert_eq!(points.len(), 2, "one point per account: {points:#?}");
    assert!(points.iter().any(|point| point["asInt"] == "16000"));
    let labels = points[0]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attribute| attribute["key"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["driver", "model", "account", "basis"]);
}

#[tokio::test]
async fn the_exporter_delivers_each_observation_once_the_collector_accepts_it() {
    let (collector, endpoint) = start_collector().await;
    let directory = tempfile::tempdir().unwrap();
    let headers_file = directory.path().join("headers.toml");
    std::fs::write(&headers_file, "x-api-key = \"example-key\"\n").unwrap();
    let store = Arc::new(Store::open_memory("node-a").unwrap());
    let exporter = OtlpExporter::new(
        &OtlpConfig {
            endpoint,
            headers_file: Some(headers_file),
        },
        "node-a",
    )
    .unwrap();
    assert_eq!(
        exporter.export_once(&store).await.unwrap(),
        ExportOutcome::default()
    );
    for entry in 1..=3 {
        observe(&store, entry);
    }
    assert_eq!(
        exporter.export_once(&store).await.unwrap(),
        ExportOutcome {
            exported: 3,
            skipped: 0
        }
    );
    assert_eq!(store.otlp_export_cursor().unwrap(), 3);
    {
        let requests = collector.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0["x-api-key"], "example-key");
        assert_eq!(records(&requests[0].1).len(), 3);
    }
    assert_eq!(
        exporter.export_once(&store).await.unwrap().exported,
        0,
        "nothing is sent twice"
    );

    collector.status.store(503, Ordering::SeqCst);
    observe(&store, 4);
    assert!(exporter.export_once(&store).await.is_err());
    assert_eq!(
        store.otlp_export_cursor().unwrap(),
        3,
        "a refused batch is sent again"
    );
    collector.status.store(200, Ordering::SeqCst);
    assert_eq!(exporter.export_once(&store).await.unwrap().exported, 1);
    assert_eq!(store.otlp_export_cursor().unwrap(), 4);

    observe(&store, 5);
    observe(&store, 6);
    // Keep only the newest observation, as a trim past the export cursor would.
    store.trim_local_observations(0, 1, 100).unwrap();
    assert_eq!(
        exporter.export_once(&store).await.unwrap(),
        ExportOutcome {
            exported: 1,
            skipped: 1
        }
    );
    assert_eq!(store.otlp_export_cursor().unwrap(), 6);
    assert_eq!(collector.requests.lock().unwrap().len(), 3);
}

fn observe_hook(store: &Store, event: &str) {
    store.append_claim(&ClaimInput {
        subject: "agent/example/seat".into(), kind: crate::telemetry::KIND.into(),
        actor: Some("agent/example/seat".into()),
        fields: BTreeMap::from([
            ("driver".into(), json!("claude")), ("unit".into(), json!("hook")),
            ("incarnation_id".into(), json!("incarnation-1")),
            ("signals".into(), json!({
                "started_at_unix_nano": "1000000000", "ended_at_unix_nano": "1025000000",
                "exit_code": 0,
                "hook_invocations": [{"hook": "claude-observe", "event": event}],
                "logs": [{"severity": "WARN", "fields": {"message": "example warning"}}],
            })),
        ]),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

#[tokio::test]
async fn hook_signals_carry_the_hook_service_and_do_not_advance_until_every_signal_is_accepted()
{
    let (collector, endpoint) = start_collector().await;
    let store = Arc::new(Store::open_memory("node-a").unwrap());
    observe_hook(&store, "PreToolUse");
    let batch = store.local_observations_after(0, 10).unwrap();
    assert_eq!(
        store.index().unwrap(),
        0,
        "telemetry never enters replicated history"
    );
    let exporter = OtlpExporter::new(
        &OtlpConfig {
            endpoint,
            headers_file: None,
        },
        "node-a",
    )
    .unwrap();
    collector.metrics_status.store(503, Ordering::SeqCst);
    assert!(exporter.export_once(&store).await.is_err());
    assert_eq!(store.otlp_export_cursor().unwrap(), 0);
    collector.metrics_status.store(200, Ordering::SeqCst);
    collector.traces_status.store(503, Ordering::SeqCst);
    assert!(exporter.export_once(&store).await.is_err());
    assert_eq!(store.otlp_export_cursor().unwrap(), 0);
    collector.traces_status.store(200, Ordering::SeqCst);
    assert_eq!(exporter.export_once(&store).await.unwrap().exported, 1);
    assert_eq!(store.otlp_export_cursor().unwrap(), 1);
    assert_eq!(exporter.export_once(&store).await.unwrap().exported, 0);
    let requests = collector.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        6,
        "a partial acceptance can repeat, without dropping a signal"
    );
    for (_, request) in requests.iter() {
        let expected = if request["resourceLogs"].get(0).is_some() {
            "st-daemon"
        } else {
            "st-hook"
        };
        assert_eq!(
            resource_service_name(request),
            expected,
            "hook spans and invocation counters carry st-hook; logs stay st-daemon"
        );
    }
    let logs = records(&requests[0].1);
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1]["severityText"], "WARN");
    let expected_span = hook_span(&batch[0]).unwrap();
    assert_eq!(logs[1]["traceId"], expected_span["traceId"]);
    assert_eq!(logs[1]["spanId"], expected_span["spanId"]);
    let metrics = &requests[2].1["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    assert_eq!(metrics["name"], "hook_invocations_total");
    assert_eq!(metrics["sum"]["aggregationTemporality"], 1);
    assert_eq!(metrics["sum"]["dataPoints"][0]["asInt"], "1");
    assert_eq!(
        metrics["sum"]["dataPoints"][0]["attributes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let spans = &requests[5].1["resourceSpans"][0]["scopeSpans"][0]["spans"];
    assert_eq!(spans, &json!([expected_span]));
}

#[test]
fn hook_metric_labels_are_bounded_and_a_non_application_has_no_counter_or_span() {
    let store = Store::open_memory("node-a").unwrap();
    observe_hook(&store, "arbitrary-new-event-with-an-identity");
    let mut batch = store.local_observations_after(0, 10).unwrap();
    let metric = otlp_hook_metrics("node-a", &batch).unwrap();
    assert_eq!(
        metric["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0]["attributes"]
            [1]["value"]["stringValue"],
        "other"
    );
    batch[0].body["fields"]["signals"]["hook_invocations"] = json!([]);
    assert!(otlp_hook_metrics("node-a", &batch).is_none());
    assert!(otlp_hook_traces("node-a", &batch).is_none());
    assert_eq!(
        records(&otlp_logs("node-a", &batch))[1]["severityText"],
        "WARN"
    );
}
