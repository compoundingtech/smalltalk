//! Composition happens once; clients receive panels, not PromQL or st response shapes.
#![forbid(unsafe_code)]
mod generated;
pub use generated::*;
mod terminal;
pub use terminal::run_mode;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use st3_client::{Client, Resource};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_secs_f64()
}

fn available(value: f64, observed_at: f64, generated_at: f64) -> Result<Sample> {
    ensure!(value.is_finite() && observed_at.is_finite() && observed_at >= 0.0, "non-finite sample or invalid timestamp");
    ensure!(observed_at <= generated_at + 5.0, "sample timestamp is in the future");
    let age_seconds = (generated_at - observed_at).max(0.0);
    Ok(Sample::Available {
        value, observed_at, age_seconds,
        freshness: if age_seconds <= 90.0 { Freshness::Fresh } else { Freshness::Stale },
    })
}

fn unavailable(reason: Failure, error: impl std::fmt::Display) -> Sample {
    Sample::Unavailable { reason, detail: error.to_string() }
}

#[derive(Deserialize)]
struct PrometheusResponse {
    status: String,
    data: Option<VectorData>,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VectorData {
    result_type: String,
    result: Vec<VectorSample>,
}
#[derive(Deserialize)]
struct VectorSample {
    metric: Labels,
    value: (f64, String),
}
#[derive(Deserialize)]
struct Labels {
    host: String,
    panel_sample: String,
}

/// A closed metric adapter, not an arbitrary PromQL tunnel. The host is escaped
/// as a JSON string, whose escaping is also valid for PromQL string literals.
pub fn memory_query(host: &str) -> String {
    let host = serde_json::to_string(host).expect("serialize host");
    let labels = format!("job=\"infra/node\",host={host}");
    let ratio = format!("1 - node_memory_MemAvailable_bytes{{{labels}}} / node_memory_MemTotal_bytes{{{labels}}}");
    // timestamp() drops metric names. Keep each original selector separate,
    // then distinguish the timestamp vectors before combining/aggregating them.
    // Applying timestamp after label_replace would report evaluation time.
    let timestamp = format!("min by (host) (label_replace(timestamp(node_memory_MemAvailable_bytes{{{labels}}}), \"panel_memory_part\", \"available\", \"host\", \".*\") or label_replace(timestamp(node_memory_MemTotal_bytes{{{labels}}}), \"panel_memory_part\", \"total\", \"host\", \".*\"))");
    format!("label_replace(({ratio}), \"panel_sample\", \"value\", \"host\", \".*\") or label_replace(({timestamp}), \"panel_sample\", \"observed-at\", \"host\", \".*\")")
}

fn memory_sample(bytes: &[u8], host: &str, generated_at: f64) -> Sample {
    let decode = || -> Result<Option<(f64, f64)>> {
        let response: PrometheusResponse = serde_json::from_slice(bytes)?;
        ensure!(response.status == "success", "Prometheus query failed: {}", response.error.as_deref().unwrap_or("unspecified error"));
        let data = response.data.context("missing vector data")?;
        ensure!(data.result_type == "vector", "expected instant vector");
        let mut value = None;
        let mut observed_at = None;
        for sample in data.result {
            ensure!(sample.metric.host == host, "unexpected host in query result");
            ensure!(sample.value.0.is_finite(), "invalid evaluation timestamp");
            let number: f64 = sample.value.1.parse()?;
            ensure!(number.is_finite(), "non-finite Prometheus value");
            let target = match sample.metric.panel_sample.as_str() {
                "value" => &mut value,
                "observed-at" => &mut observed_at,
                _ => bail!("unexpected sample label"),
            };
            ensure!(target.replace(number).is_none(), "duplicate host series");
        }
        match (value, observed_at) {
            (Some(value), Some(observed_at)) => {
                ensure!((0.0..=1.0).contains(&value), "memory ratio outside 0..1");
                Ok(Some((value, observed_at)))
            }
            (None, None) => Ok(None),
            _ => bail!("memory value/timestamp pair is incomplete"),
        }
    };
    match decode() {
        Ok(Some((value, observed_at))) => available(value, observed_at, generated_at)
            .unwrap_or_else(|error| unavailable(Failure::InvalidResponse, error)),
        Ok(None) => unavailable(Failure::MissingSeries, "no node exporter memory series for this host"),
        Err(error) => unavailable(Failure::InvalidResponse, error),
    }
}

async fn memory(client: &reqwest::Client, base: &str, host: &str, tenant: &str) -> Sample {
    let request = async {
        let response = client.get(format!("{}/api/v1/query", base.trim_end_matches('/')))
            .header("X-Scope-OrgID", tenant)
            .query(&[("query", memory_query(host)), ("timeout", "2s".to_owned())])
            .send().await?.error_for_status()?;
        // This closed selector yields at most two samples. Bound the body too.
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= 64 * 1024, "Prometheus response exceeds 64 KiB");
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>(bytes)
    };
    match request.await {
        Ok(bytes) => memory_sample(&bytes, host, now()),
        Err(error) => unavailable(Failure::SourceUnavailable, error),
    }
}

async fn agent_records(client: &Client, host: &str) -> Sample {
    let read = async {
        let mut cursor = None;
        let mut count = 0_u32;
        let mut snapshot_id = None;
        let mut observed_at = None;
        let subject = format!("host/{host}");
        // The existing typed current-agent collection owns what "current" means.
        // Follow every cursor rather than displaying a first-page length as total.
        for _ in 0..100 {
            let page = client.agents_list(cursor.as_deref(), Some(200), false).await?;
            ensure!(page.value.sync.is_none(), "st projection reports incomplete synchronization");
            if let Some(notice) = &page.value.replicated {
                ensure!(notice.complete && notice.state == "current", "st replica is incomplete or unverified");
            }
            if let Some(id) = &snapshot_id {
                ensure!(id == &page.snapshot.id, "pagination changed st snapshot");
            } else {
                snapshot_id = Some(page.snapshot.id);
                observed_at = Some(chrono::DateTime::parse_from_rfc3339(&page.snapshot.created_at)?.timestamp_millis() as f64 / 1000.0);
            }
            for resource in page.value.items {
                let Resource::Agent(agent) = resource else { bail!("non-agent in agent collection") };
                if agent.host_id.as_deref() == Some(subject.as_str()) {
                    count += 1;
                }
            }
            if !page.value.page.has_more {
                return Ok::<_, anyhow::Error>((f64::from(count), observed_at.context("missing snapshot timestamp")?));
            }
            let next = page.value.page.next_cursor.context("missing next cursor")?;
            ensure!(cursor.as_ref() != Some(&next), "repeated pagination cursor");
            cursor = Some(next);
        }
        bail!("agent collection exceeds 100-page budget")
    };
    match tokio::time::timeout(Duration::from_secs(5), read).await {
        Ok(Ok((value, observed_at))) => available(value, observed_at, now())
            .unwrap_or_else(|error| unavailable(Failure::InvalidResponse, error)),
        Ok(Err(error)) => unavailable(Failure::IncompleteSource, error),
        Err(error) => unavailable(Failure::SourceUnavailable, error),
    }
}

/// Both sources are independent: a Mimir outage cannot turn st agents into zero,
/// and an st outage cannot hide host telemetry. No cache or daemon is introduced.
pub async fn compose(host: &str, base: &str, tenant: &str, st: &Client) -> Result<PanelDocument> {
    ensure!(!host.is_empty() && host.len() <= 253, "host must contain 1..253 bytes");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
    let (agent, system) = tokio::join!(agent_records(st, host), memory(&http, base, host, tenant));
    Ok(PanelDocument {
        contract: Contract::MetricPanel1,
        generated_at: now(),
        panels: vec![
            Panel { id: MetricId::AgentRecords, title: "Current agent records".into(), subject: format!("host/{host}"), unit: Unit::Count, source: Source::Smalltalk, sample: agent },
            Panel { id: MetricId::MemoryUsedRatio, title: "Memory used".into(), subject: format!("host/{host}"), unit: Unit::Ratio, source: Source::Mimir, sample: system },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(value: &str, observed_at: &str) -> Vec<u8> {
        format!(r#"{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{"host":"dev3","panel_sample":"value"}},"value":[1000,"{value}"]}},{{"metric":{{"host":"dev3","panel_sample":"observed-at"}},"value":[1000,"{observed_at}"]}}]}}}}"#).into_bytes()
    }
    #[test]
    fn exporter_timestamp_not_query_time_controls_freshness() {
        assert!(matches!(memory_sample(&response("0.5", "800"), "dev3", 1000.0), Sample::Available { freshness: Freshness::Stale, age_seconds: 200.0, .. }));
    }
    #[test]
    fn fresh_zero_is_a_real_value() {
        assert!(matches!(memory_sample(&response("0", "990"), "dev3", 1000.0), Sample::Available { value: 0.0, freshness: Freshness::Fresh, .. }));
    }
    #[test]
    fn missing_series_is_not_zero() {
        assert!(matches!(memory_sample(br#"{"status":"success","data":{"resultType":"vector","result":[]}}"#, "dev3", 1000.0), Sample::Unavailable { reason: Failure::MissingSeries, .. }));
    }
    #[test]
    fn rejects_nonfinite_ratio_and_foreign_host() {
        for value in ["NaN", "Inf", "-0.1", "1.1"] {
            assert!(matches!(memory_sample(&response(value, "990"), "dev3", 1000.0), Sample::Unavailable { reason: Failure::InvalidResponse, .. }));
        }
        assert!(matches!(memory_sample(&response("0.5", "990"), "dev5", 1000.0), Sample::Unavailable { reason: Failure::InvalidResponse, .. }));
    }
    #[test]
    fn future_timestamp_is_not_fresh() {
        assert!(matches!(memory_sample(&response("0.5", "1100"), "dev3", 1000.0), Sample::Unavailable { reason: Failure::InvalidResponse, .. }));
    }
    #[test]
    fn host_literal_is_escaped() {
        assert!(memory_query("dev3\"}").contains("host=\"dev3\\\"}\""));
    }
    #[test]
    fn timestamp_selectors_are_separate_before_label_transformation() {
        let query = memory_query("dev3");
        assert!(query.contains("label_replace(timestamp(node_memory_MemAvailable_bytes"));
        assert!(query.contains("label_replace(timestamp(node_memory_MemTotal_bytes"));
        assert!(!query.contains("timestamp(label_replace"));
        assert!(!query.contains("__name__=~"));
    }
    #[test]
    fn contract_rejects_unknown_versions() {
        assert!(serde_json::from_str::<Contract>("\"metric-panel/2\"").is_err());
    }
}
