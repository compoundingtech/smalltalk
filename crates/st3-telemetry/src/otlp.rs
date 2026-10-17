//! Optional OpenTelemetry export of this node's local observation log.
//!
//! Local observations become OTLP/HTTP JSON logs; hook telemetry also supplies delta counters
//! and operation spans. The cursor advances only after every signal in a batch is accepted,
//! so delivery is at least once (a partially accepted batch can repeat). Export never blocks a write:
//! a collector that is
//! down only delays export, and observations trimmed before export are counted as a gap.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub mod slo;
pub use slo::run as run_slo;

use st3_kernel::model::{ClaimRecord, local_observation_position};

pub use st3_kernel::config::OtlpConfig;

/// The local observation log this exporter reads and the cursor it keeps. The store implements
/// it, so this module names no store code.
pub trait ObservationSource: Send + Sync + 'static {
    /// The last local observation the exporter delivered.
    fn export_cursor(&self) -> Result<u64>;
    /// Local observations written after `cursor`, oldest first.
    fn observations_after(&self, cursor: u64, limit: usize) -> Result<Vec<ClaimRecord>>;
    /// Record that every observation up to `cursor` was delivered, on the source's writer.
    fn save_export_cursor(
        self: Arc<Self>,
        cursor: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
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
    pub async fn export_once<S: ObservationSource>(&self, store: &Arc<S>) -> Result<ExportOutcome> {
        let cursor = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || smallclaims::task_cpu::blocking("observations.export", || store.export_cursor())).await??
        };
        let batch = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || smallclaims::task_cpu::blocking("observations.export", || store.observations_after(cursor, OTLP_BATCH)))
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
        store.clone().save_export_cursor(last).await?;
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
pub async fn run<S: ObservationSource>(store: Arc<S>, exporter: OtlpExporter) {
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
            let fields = observation
                .body
                .get("fields")
                .cloned()
                .unwrap_or(Value::Null);
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
            "resource": resource(node, DAEMON_SERVICE),
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
        json!({"resourceMetrics": [{"resource": resource(node, DAEMON_SERVICE), "scopeMetrics": [{
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

/// The exporter runs inside the daemon process, so observation logs and usage
/// counters carry its identity.
const DAEMON_SERVICE: &str = "st-daemon";

/// Per-invocation hook signals are their own service so hook timing and counts
/// stay separable from the daemon's observation bookkeeping.
const HOOK_SERVICE: &str = "st-hook";

fn resource(node: &str, service_name: &str) -> Value {
    json!({"attributes": [
        attribute("service.name", json!({"stringValue": service_name})),
        attribute("service.version", json!({"stringValue": st_drivers::version::machine_version()})),
        attribute("service.instance.id", json!({"stringValue": crate::otel::service_instance_id()})),
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
pub fn otlp_hook_metrics(node: &str, batch: &[ClaimRecord]) -> Option<Value> {
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
        json!({"resourceMetrics": [{"resource": resource(node, HOOK_SERVICE), "scopeMetrics": [{
            "scope": {"name": "st"}, "metrics": [{
                "name": "hook_invocations_total", "unit": "1",
                "description": "Lifecycle hook invocations applied in-process, by hook and event",
                "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": points},
            }],
        }]}]}),
    )
}

pub fn hook_span(observation: &ClaimRecord) -> Option<Value> {
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

pub fn otlp_hook_traces(node: &str, batch: &[ClaimRecord]) -> Option<Value> {
    let spans = batch.iter().filter_map(hook_span).collect::<Vec<_>>();
    (!spans.is_empty()).then(|| json!({"resourceSpans": [{
        "resource": resource(node, HOOK_SERVICE), "scopeSpans": [{"scope": {"name": "st.hooks"}, "spans": spans}],
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
