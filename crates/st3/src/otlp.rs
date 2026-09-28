//! Optional OpenTelemetry export of this node's local observation log.
//!
//! Each local observation becomes one OTLP log record, sent as OTLP/HTTP JSON to
//! `{endpoint}/v1/logs`. The export cursor advances only after the collector accepts a
//! batch, so delivery is at least once. Export never blocks a write: a collector that is
//! down only delays export, and observations trimmed before export are counted as a gap.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .json(&otlp_logs(&self.node, &batch))
            .send()
            .await
            .with_context(|| format!("send observations to {}", self.url))?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "the collector at {} answered {status}",
            self.url
        );
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.set_otlp_export_cursor(last)).await??;
        Ok(ExportOutcome {
            exported: batch.len(),
            skipped,
        })
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
        .map(|observation| {
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
            json!({
                "timeUnixNano": time,
                "observedTimeUnixNano": time,
                "severityNumber": 9,
                "severityText": "INFO",
                "eventName": observation.kind,
                "body": any_value(&fields),
                "attributes": attributes,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [
                    attribute("service.name", json!({ "stringValue": "st3" })),
                    attribute("host.name", json!({ "stringValue": node })),
                    attribute("st3.node", json!({ "stringValue": node })),
                ]
            },
            "scopeLogs": [{
                "scope": { "name": "st3.observations" },
                "logRecords": records,
            }]
        }]
    })
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
    }

    async fn collect(
        State(collector): State<Collector>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> StatusCode {
        let status =
            StatusCode::from_u16(collector.status.load(Ordering::SeqCst)).unwrap_or(StatusCode::OK);
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

    #[test]
    fn a_local_observation_becomes_an_otlp_log_record() {
        let store = Store::open_memory("node-a").unwrap();
        observe(&store, 7);
        let batch = store.local_observations_after(0, 10).unwrap();
        let request = otlp_logs("node-a", &batch);
        let resource = &request["resourceLogs"][0]["resource"]["attributes"];
        assert!(resource.as_array().unwrap().contains(&json!({
            "key": "service.name", "value": {"stringValue": "st3"}
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
