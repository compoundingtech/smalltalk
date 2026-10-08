//! Optional OpenTelemetry export of this node's local observation log.
//!
//! Local observations become OTLP/HTTP JSON logs; hook telemetry also supplies delta counters
//! and operation spans. The cursor advances only after every signal in a batch is accepted,
//! so delivery is at least once (a partially accepted batch can repeat). Export never blocks a write:
//! a collector that is
//! down only delays export, and observations trimmed before export are counted as a gap.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::model::ClaimRecord;
use crate::store::{Store, local_observation_position};

/// `[observations.otlp]`: absent means no exporter runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    /// The collector's OTLP/HTTP base URL, such as `http://127.0.0.1:4318`.
    pub endpoint: String,
    /// A TOML file of extra request headers, such as an API key. It stays out of the main
    /// config so that file can be shared.
    #[serde(default)]
    pub headers_file: Option<PathBuf>,
}

impl OtlpConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.endpoint)
            .with_context(|| format!("parse observations.otlp.endpoint `{}`", self.endpoint))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "observations.otlp.endpoint must use http or https"
        );
        Ok(())
    }

    fn logs_url(&self) -> String {
        format!("{}/v1/logs", self.endpoint.trim_end_matches('/'))
    }

    fn headers(&self) -> Result<BTreeMap<String, String>> {
        let Some(path) = &self.headers_file else {
            return Ok(BTreeMap::new());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read observations.otlp.headers_file {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("parse observations.otlp.headers_file {}", path.display()))
    }
}

/// The most observations sent in one request.
pub const OTLP_BATCH: usize = 512;
const IDLE_POLL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExportOutcome {
    /// Observations the collector accepted.
    pub exported: usize,
    /// Observations trimmed before they could be exported.
    pub skipped: u64,
}

pub struct OtlpExporter {
    client: reqwest::Client,
    url: String,
    metrics_url: String,
    traces_url: String,
    headers: reqwest::header::HeaderMap,
    node: String,
}

impl OtlpExporter {
    pub fn new(config: &OtlpConfig, node: &str) -> Result<Self> {
        config.validate()?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in config.headers()? {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("observations.otlp header name `{name}`"))?,
                reqwest::header::HeaderValue::from_str(&value)
                    .with_context(|| format!("observations.otlp header `{name}`"))?,
            );
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: config.logs_url(),
            metrics_url: format!("{}/v1/metrics", config.endpoint.trim_end_matches('/')),
            traces_url: format!("{}/v1/traces", config.endpoint.trim_end_matches('/')),
            headers,
            node: node.to_owned(),
        })
    }

    /// Export the next batch after the stored cursor. The cursor moves only when the
    /// collector accepts the batch.
    pub async fn export_once(&self, store: &Arc<Store>) -> Result<ExportOutcome> {
        let cursor = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || store.otlp_export_cursor()).await??
        };
        let batch = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || store.local_observations_after(cursor, OTLP_BATCH))
                .await??
        };
        let Some(last) = batch.last().and_then(local_observation_position) else {
            return Ok(ExportOutcome::default());
        };
        let first = batch
            .first()
            .and_then(local_observation_position)
            .unwrap_or(last);
        let skipped = first.saturating_sub(cursor).saturating_sub(1);
        self.send(&self.url, &otlp_logs(&self.node, &batch)).await?;
        if let Some(metrics) = otlp_hook_metrics(&self.node, &batch) {
            self.send(&self.metrics_url, &metrics).await?;
        }
        if let Some(metrics) = otlp_usage_metrics(&self.node, &batch) {
            self.send(&self.metrics_url, &metrics).await?;
        }
        if let Some(traces) = otlp_hook_traces(&self.node, &batch) {
            self.send(&self.traces_url, &traces).await?;
        }
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.set_otlp_export_cursor(last)).await??;
        Ok(ExportOutcome {
            exported: batch.len(),
            skipped,
        })
    }

    async fn send(&self, url: &str, body: &Value) -> Result<()> {
        let response = self
            .client
            .post(url)
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .with_context(|| format!("send observations to {url}"))?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "the collector at {} answered {status}",
            url
        );
        Ok(())
    }
}

/// Export until the process stops. A failure backs off up to five minutes.
pub async fn run(store: Arc<Store>, exporter: OtlpExporter) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match exporter.export_once(&store).await {
            Ok(outcome) => {
                backoff = Duration::from_secs(1);
                if outcome.skipped > 0 {
                    eprintln!(
                        "st3: {} local observations were trimmed before OpenTelemetry export",
                        outcome.skipped
                    );
                }
                if outcome.exported < OTLP_BATCH {
                    tokio::time::sleep(IDLE_POLL).await;
                }
            }
            Err(error) => {
                eprintln!("st3: OpenTelemetry export failed: {error:#}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// One OTLP/HTTP JSON logs request for a batch of local observations.
pub fn otlp_logs(node: &str, batch: &[ClaimRecord]) -> Value {
    let records = batch
        .iter()
        .flat_map(|observation| {
            let mut fields = observation
                .body
                .get("fields")
                .cloned()
                .unwrap_or(Value::Null);
            if observation.kind == "runtime.action.failed"
                && observation.actor.is_none()
                && matches!(fields.get("action").and_then(Value::as_str), Some("start" | "startup-output"))
            {
                // Keep startup output on its runtime host, including when OTLP is configured.
                // Preserve structured action and incarnation provenance for collector queries.
                fields["reason"] = json!("startup detail is available from the runtime host");
            }
            let time = (observation.accepted_at_unix_ms * 1_000_000).to_string();
            let mut attributes = vec![
                attribute("st3.subject", json!({ "stringValue": observation.subject })),
                attribute("st3.kind", json!({ "stringValue": observation.kind })),
                attribute(
                    "st3.local_id",
                    json!({ "intValue": local_observation_position(observation).unwrap_or(0).to_string() }),
                ),
            ];
            if let Some(actor) = &observation.actor {
                attributes.push(attribute("st3.actor", json!({ "stringValue": actor })));
            }
            if let Some(incarnation) = fields.get("incarnation_id").and_then(Value::as_str) {
                attributes.push(attribute(
                    "st3.incarnation_id",
                    json!({ "stringValue": incarnation }),
                ));
            }
            let mut record = json!({
                "timeUnixNano": time,
                "observedTimeUnixNano": time,
                "severityNumber": 9,
                "severityText": "INFO",
                "eventName": observation.kind,
                "body": any_value(&fields),
                "attributes": attributes,
            });
            let span = hook_span(observation);
            if let Some(span) = &span {
                record["traceId"] = span["traceId"].clone();
                record["spanId"] = span["spanId"].clone();
            }
            let mut records = vec![record];
            if let Some(usage) = UsageResponse::of(observation) {
                records.push(usage.log_record());
            }
            if observation.kind == crate::telemetry::KIND
                && let Some(logs) = fields.pointer("/signals/logs").and_then(Value::as_array)
            {
                    for log in logs.iter().take(16) {
                        let (severity, number) = match log["severity"].as_str() {
                            Some("WARN") => ("WARN", 13),
                            Some("ERROR") => ("ERROR", 17),
                            _ => continue,
                        };
                        let mut warning = json!({
                            "timeUnixNano": time, "observedTimeUnixNano": time,
                            "severityNumber": number, "severityText": severity,
                            "eventName": "st.hook.warning",
                            "body": any_value(&log["fields"]),
                            "attributes": attributes,
                        });
                        if let Some(span) = &span {
                            warning["traceId"] = span["traceId"].clone();
                            warning["spanId"] = span["spanId"].clone();
                        }
                        records.push(warning);
                    }
            }
            records
        })
        .collect::<Vec<_>>();
    json!({
        "resourceLogs": [{
            "resource": resource(node),
            "scopeLogs": [{
                "scope": { "name": "st.observations" },
                "logRecords": records,
            }]
        }]
    })
}

/// One model response's spend, from the harness timeline entry st recorded for it.
struct UsageResponse<'a> {
    observation: &'a ClaimRecord,
    fields: &'a Value,
}

impl<'a> UsageResponse<'a> {
    fn of(observation: &'a ClaimRecord) -> Option<Self> {
        let fields = observation.body.get("fields")?;
        (observation.kind == "harness.timeline"
            && fields["entry_type"] == "usage"
            && fields["body"]["semantics"] == "response")
            .then_some(Self {
                observation,
                fields,
            })
    }

    fn text(&self, pointer: &str) -> &'a str {
        self.fields
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
    }

    fn tokens(&self, name: &str) -> u64 {
        self.fields["body"][name].as_u64().unwrap_or(0)
    }

    fn cost_microusd(&self) -> u64 {
        self.fields["spend"]["cost_microusd"].as_u64().unwrap_or(0)
    }

    /// Bounded metric labels: no agent, step or run identity, which stay in the log record.
    fn labels(&self) -> [(&'static str, &'a str); 4] {
        [
            ("driver", self.text("/driver")),
            ("model", self.text("/body/model")),
            ("account", self.text("/body/account")),
            ("basis", self.text("/spend/basis")),
        ]
    }

    const TOKENS: [(&'static str, &'static str); 4] = [
        ("input", "input_tokens"),
        ("output", "output_tokens"),
        ("cache_read", "cached_tokens"),
        ("cache_write", "cache_write_tokens"),
    ];

    /// `st.usage.response`: the response's attribution, tokens and cost as flat attributes, so a
    /// log store can sum spend by agent, mission run, step, model and account.
    fn log_record(&self) -> Value {
        let at = self.fields["observed_at_unix_ms"]
            .as_u64()
            .map_or(self.observation.accepted_at_unix_ms, u128::from);
        let time = (at * 1_000_000).to_string();
        let string = |key: &str, value: &str| attribute(key, json!({ "stringValue": value }));
        let int = |key: &str, value: u64| attribute(key, json!({ "intValue": value.to_string() }));
        let mut attributes = vec![
            string("st.agent", &self.observation.subject),
            string("st.mission_run", self.text("/attribution/mission_run_id")),
            string("st.step", self.text("/attribution/step_id")),
            string("st.incarnation_id", self.text("/incarnation_id")),
            string("st.host", self.text("/host")),
            string("st.pricing", self.text("/spend/pricing")),
        ];
        for (name, value) in self.labels() {
            attributes.push(string(&format!("st.{name}"), value));
        }
        for (name, field) in Self::TOKENS {
            attributes.push(int(&format!("st.tokens.{name}"), self.tokens(field)));
        }
        attributes.push(int("st.tokens.total", self.tokens("total_tokens")));
        attributes.push(int("st.cost.microusd", self.cost_microusd()));
        json!({
            "timeUnixNano": time,
            "observedTimeUnixNano": (self.observation.accepted_at_unix_ms * 1_000_000).to_string(),
            "severityNumber": 9,
            "severityText": "INFO",
            "eventName": "st.usage.response",
            "body": { "stringValue": "model response" },
            "attributes": attributes,
        })
    }
}

/// Token and cost counters for the batch's model responses, as delta sums.
fn otlp_usage_metrics(node: &str, batch: &[ClaimRecord]) -> Option<Value> {
    let responses = batch
        .iter()
        .filter_map(UsageResponse::of)
        .collect::<Vec<_>>();
    if responses.is_empty() {
        return None;
    }
    let mut tokens = BTreeMap::<(Vec<(&str, &str)>, &str), u64>::new();
    let mut cost = BTreeMap::<Vec<(&str, &str)>, u64>::new();
    for response in &responses {
        let labels = response.labels().to_vec();
        for (name, field) in UsageResponse::TOKENS {
            *tokens.entry((labels.clone(), name)).or_default() += response.tokens(field);
        }
        *cost.entry(labels).or_default() += response.cost_microusd();
    }
    let started = (batch.first()?.accepted_at_unix_ms * 1_000_000)
        .saturating_sub(1)
        .to_string();
    let ended = (batch.last()?.accepted_at_unix_ms * 1_000_000).to_string();
    let point = |labels: &[(&str, &str)], extra: Option<(&str, &str)>, value: u64| {
        let attributes = labels
            .iter()
            .copied()
            .chain(extra)
            .map(|(key, value)| attribute(key, json!({ "stringValue": value })))
            .collect::<Vec<_>>();
        json!({
            "startTimeUnixNano": started, "timeUnixNano": ended,
            "asInt": value.to_string(), "attributes": attributes,
        })
    };
    let token_points = tokens
        .iter()
        .map(|((labels, kind), value)| point(labels, Some(("type", kind)), *value))
        .collect::<Vec<_>>();
    let cost_points = cost
        .iter()
        .map(|(labels, value)| point(labels, None, *value))
        .collect::<Vec<_>>();
    Some(
        json!({"resourceMetrics": [{"resource": resource(node), "scopeMetrics": [{
            "scope": {"name": "st"}, "metrics": [
                {
                    "name": "st_usage_tokens_total", "unit": "{token}",
                    "description": "Model tokens by driver, model, paying account, cost basis and token type",
                    "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": token_points},
                },
                {
                    "name": "st_usage_cost_microusd_total", "unit": "{microUSD}",
                    "description": "API-equivalent cost in millionths of a US dollar, by driver, model, paying account and cost basis",
                    "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": cost_points},
                },
            ],
        }]}]}),
    )
}

fn resource(node: &str) -> Value {
    json!({"attributes": [
        attribute("service.name", json!({"stringValue": "st"})),
        attribute("host.name", json!({"stringValue": node})),
        attribute("st3.node", json!({"stringValue": node})),
    ]})
}

fn hook_invocations(observation: &ClaimRecord) -> Vec<&str> {
    if observation.kind != crate::telemetry::KIND {
        return Vec::new();
    }
    observation
        .body
        .pointer("/fields/signals/hook_invocations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(16)
        .filter(|invocation| invocation["hook"] == "claude-observe")
        .filter_map(|invocation| invocation["event"].as_str())
        .map(st_drivers::metrics::normalize_hook_event)
        .collect()
}

/// Delta points retain the bounded hook/event labels; identities only appear in logs/spans.
fn otlp_hook_metrics(node: &str, batch: &[ClaimRecord]) -> Option<Value> {
    let mut counts = BTreeMap::<&str, u64>::new();
    for observation in batch {
        for event in hook_invocations(observation) {
            *counts.entry(event).or_default() += 1;
        }
    }
    if counts.is_empty() {
        return None;
    }
    let first = (batch.first()?.accepted_at_unix_ms * 1_000_000) as u64;
    let started = batch
        .iter()
        .filter(|observation| !hook_invocations(observation).is_empty())
        .filter_map(|observation| {
            observation
                .body
                .pointer("/fields/signals/started_at_unix_nano")?
                .as_str()?
                .parse::<u64>()
                .ok()
        })
        .min()
        .unwrap_or(first.saturating_sub(1))
        .min(first.saturating_sub(1))
        .to_string();
    let ended = (batch.last()?.accepted_at_unix_ms * 1_000_000).to_string();
    let points = counts
        .into_iter()
        .map(|(event, count)| {
            json!({
                "startTimeUnixNano": started, "timeUnixNano": ended, "asInt": count.to_string(),
                "attributes": [
                    attribute("hook", json!({"stringValue": "claude-observe"})),
                    attribute("event", json!({"stringValue": event})),
                ],
            })
        })
        .collect::<Vec<_>>();
    Some(
        json!({"resourceMetrics": [{"resource": resource(node), "scopeMetrics": [{
            "scope": {"name": "st"}, "metrics": [{
                "name": "hook_invocations_total", "unit": "1",
                "description": "Lifecycle hook invocations applied in-process, by hook and event",
                "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": points},
            }],
        }]}]}),
    )
}

fn hook_span(observation: &ClaimRecord) -> Option<Value> {
    let events = hook_invocations(observation);
    let event = events.first()?;
    let signals = observation.body.pointer("/fields/signals")?;
    let start = signals["started_at_unix_nano"]
        .as_str()?
        .parse::<u64>()
        .ok()?;
    let end = signals["ended_at_unix_nano"]
        .as_str()?
        .parse::<u64>()
        .ok()?;
    if end < start {
        return None;
    }
    let digest = Sha256::digest(observation.id.as_bytes());
    Some(json!({
        "traceId": hex::encode(&digest[..16]), "spanId": hex::encode(&digest[16..24]),
        "name": "st.hook.claude-observe", "kind": 1,
        "startTimeUnixNano": start.to_string(), "endTimeUnixNano": end.to_string(),
        "attributes": [
            attribute("st.subject", json!({"stringValue": observation.subject})),
            attribute("st.incarnation_id", json!({"stringValue": observation.body["fields"]["incarnation_id"]})),
            attribute("st.hook.event", json!({"stringValue": event})),
        ],
        "status": {"code": if signals["exit_code"] == 0 {1} else {2}},
    }))
}

fn otlp_hook_traces(node: &str, batch: &[ClaimRecord]) -> Option<Value> {
    let spans = batch.iter().filter_map(hook_span).collect::<Vec<_>>();
    (!spans.is_empty()).then(|| json!({"resourceSpans": [{
        "resource": resource(node), "scopeSpans": [{"scope": {"name": "st.hooks"}, "spans": spans}],
    }]}))
}

fn attribute(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

/// Convert JSON to an OTLP `AnyValue`.
fn any_value(value: &Value) -> Value {
    match value {
        Value::Null => json!({}),
        Value::Bool(value) => json!({ "boolValue": value }),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => json!({ "intValue": integer.to_string() }),
            None => json!({ "doubleValue": number.as_f64().unwrap_or_default() }),
        },
        Value::String(value) => json!({ "stringValue": value }),
        Value::Array(values) => {
            json!({ "arrayValue": { "values": values.iter().map(any_value).collect::<Vec<_>>() } })
        }
        Value::Object(fields) => json!({
            "kvlistValue": {
                "values": fields
                    .iter()
                    .map(|(key, value)| attribute(key, any_value(value)))
                    .collect::<Vec<_>>()
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn local_startup_detail_is_not_sent_to_the_collector() {
        let (collector, endpoint) = start_collector().await;
        let store = Arc::new(Store::open_memory("node-a").unwrap());
        let marker = "local-fixture-launch-detail-must-stay-on-node";
        for action in ["start", "startup-output"] {
            store
                .append_claim(&ClaimInput {
                    subject: "agent/node.worker".into(),
                    kind: "runtime.action.failed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), json!(action)),
                        ("reason".into(), json!(marker)),
                        ("incarnation_id".into(), json!("inc-1")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("local-startup:{action}")),
                })
                .unwrap();
        }
        let exporter = OtlpExporter::new(
            &OtlpConfig {
                endpoint,
                headers_file: None,
            },
            "node-a",
        )
        .unwrap();
        assert_eq!(exporter.export_once(&store).await.unwrap().exported, 2);
        assert_eq!(exporter.export_once(&store).await.unwrap().exported, 0);
        let requests = collector.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].1.to_string().contains(marker));
        let logs = records(&requests[0].1);
        assert_eq!(logs.len(), 2);
        for (log, action) in logs.iter().zip(["start", "startup-output"]) {
            assert!(log.to_string().contains(action));
            assert!(log.to_string().contains("inc-1"));
            assert!(
                log.to_string()
                    .contains("startup detail is available from the runtime host")
            );
        }
        let local = store.local_observations_after(0, 10).unwrap();
        assert_eq!(local.len(), 2);
        assert!(
            local
                .iter()
                .all(|claim| claim.body["fields"]["reason"] == marker)
        );
    }

    #[test]
    fn a_local_observation_becomes_an_otlp_log_record() {
        let store = Store::open_memory("node-a").unwrap();
        observe(&store, 7);
        let batch = store.local_observations_after(0, 10).unwrap();
        let request = otlp_logs("node-a", &batch);
        let resource = &request["resourceLogs"][0]["resource"]["attributes"];
        assert!(resource.as_array().unwrap().contains(&json!({
            "key": "service.name", "value": {"stringValue": "st"}
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
        let metrics = requests
            .iter()
            .find(|(_, body)| body.get("resourceMetrics").is_some())
            .map(|(_, body)| &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"])
            .unwrap();
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
    async fn hook_signals_share_one_service_and_do_not_advance_until_every_signal_is_accepted() {
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
            let resource = request["resourceLogs"]
                .get(0)
                .or_else(|| request["resourceMetrics"].get(0))
                .or_else(|| request["resourceSpans"].get(0))
                .unwrap();
            assert!(
                resource["resource"]["attributes"]
                    .as_array()
                    .unwrap()
                    .contains(&json!({
                        "key": "service.name", "value": {"stringValue": "st"},
                    }))
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

    #[test]
    fn a_collector_endpoint_must_be_an_http_url() {
        for endpoint in ["collector:4318", "ftp://collector"] {
            assert!(
                OtlpConfig {
                    endpoint: endpoint.into(),
                    headers_file: None,
                }
                .validate()
                .is_err(),
                "{endpoint}"
            );
        }
    }
}
