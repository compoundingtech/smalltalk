//! The daemon's live SLO windows as OpenTelemetry gauges, for the fleet collector.
//!
//! The windows are in memory and say how the daemon is doing now, so they are sampled on a timer
//! and sent as gauges: a minute that cannot be delivered is dropped, never replayed, and the
//! collector holds the history. Nothing here runs on a request path; once a minute a task reads
//! the same report as `GET /v1/client/request-latency` and sends it to `[observations.otlp]`.
//!
//! Every point carries bounded labels, all from `slo/targets.toml` or fixed words: `target`, the
//! `series` inside it (`owner` or `remote` for a read target, `reported` for a latency a paired
//! phone reported, `statement`, `read`, `write` or `writer_hold` for the store), the `window`
//! (`1m`, `5m`, `1h`) and, for the latency gauge, the `stat` (`p50`, `p99`, `max`). The target
//! figures go out beside them as `st_slo_target_ms` so one query can set what a host did against
//! what it should. A phone's live share is a minimum, not a latency, and has its own gauges
//! (`st_slo_live_share`, `st_slo_foreground_ms`, `st_slo_min_share_percent`).

use std::time::Duration;

use serde_json::{Value, json};

use super::{DAEMON_SERVICE, OtlpExporter, attribute, resource};

const INTERVAL: Duration = Duration::from_secs(60);
const WINDOWS: [&str; 3] = ["1m", "5m", "1h"];

/// Gauges for the live windows in `report` (the request-latency read's `targets`), or `None`
/// when it holds nothing to send.
pub fn metrics(node: &str, report: &Value, now_unix_ms: u64) -> Option<Value> {
    let time = (now_unix_ms * 1_000_000).to_string();
    let mut latency = Vec::new();
    let mut over = Vec::new();
    let mut samples = Vec::new();
    let mut targets = Vec::new();
    let mut cpu = Vec::new();
    let mut cpu_max = Vec::new();
    let mut live_share = Vec::new();
    let mut foreground = Vec::new();
    let mut min_share = Vec::new();
    let point = |value: f64, labels: &[(&str, &str)]| {
        json!({
            "timeUnixNano": time, "asDouble": value,
            "attributes": labels
                .iter()
                .map(|(key, value)| attribute(key, json!({"stringValue": value})))
                .collect::<Vec<_>>(),
        })
    };
    for row in report["targets"].as_array().into_iter().flatten() {
        let Some(name) = row["name"].as_str() else { continue };
        // A phone's reported minutes are another population from the daemon's own timings.
        let population = row["population"].as_str();
        if population == Some("client-observed-foreground-ms") {
            // Its count is foreground milliseconds and its target a minimum share, not a latency.
            for window in WINDOWS {
                let labels = [("target", name), ("window", window)];
                let row = &row["windows"][window];
                if let Some(ms) = row["foreground_ms"].as_f64() {
                    foreground.push(point(ms, &labels));
                }
                if let Some(share) = row["live_share"].as_f64() {
                    live_share.push(point(share, &labels));
                }
            }
            if let Some(percent) = row["min_percent"].as_f64() {
                min_share.push(point(percent, &[("target", name)]));
            }
            continue;
        }
        let mut series = |series: &str, windows: &Value| {
            for window in WINDOWS {
                let row = &windows[window];
                let Some(count) = row["count"].as_u64() else { continue };
                let labels = [("target", name), ("series", series), ("window", window)];
                samples.push(point(count as f64, &labels));
                if count == 0 {
                    continue;
                }
                if let Some(share) = row["over_target_share"].as_f64() {
                    over.push(point(share, &labels));
                }
                for (stat, field) in [("p50", "p50_ms"), ("p99", "p99_ms"), ("max", "max_ms")] {
                    if let Some(ms) = row[field].as_f64() {
                        latency.push(point(
                            ms,
                            &[("target", name), ("series", series), ("window", window), ("stat", stat)],
                        ));
                    }
                }
            }
        };
        let mut target = |series: &str, stat: &str, ms: &Value| {
            if let Some(ms) = ms.as_f64() {
                targets.push(point(ms, &[("target", name), ("series", series), ("stat", stat)]));
            }
        };
        match name {
            "cpu" => {
                for window in WINDOWS {
                    if let Some(cores) = row["windows"][window]["cores"].as_f64() {
                        cpu.push(point(cores, &[("window", window)]));
                    }
                }
                if let Some(max) = row["max_cores"].as_f64() {
                    cpu_max.push(point(max, &[]));
                }
            }
            "sql-statement" => {
                series("statement", &row["windows"]);
                target("statement", "p99", &row["p99_ms"]);
                target("statement", "max", &row["max_ms"]);
            }
            "transaction" => {
                for kind in ["read", "write", "writer_hold"] {
                    series(kind, &row[kind]);
                    target(kind, "max", &row["max_ms"]);
                }
            }
            // The writer-hold row's windows go out under `transaction`; its holders are not gauges.
            "database" | "writer-hold" => {}
            _ => {
                let owner = if population.is_some() { "reported" } else { "owner" };
                series(owner, &row["windows"]);
                target(owner, "p99", &row["p99_ms"]);
                if row["remote_windows"].is_object() {
                    series("remote", &row["remote_windows"]);
                    target("remote", "p99", &row["remote_p99_ms"]);
                }
            }
        }
    }
    let gauge = |name: &str, unit: &str, description: &str, points: Vec<Value>| {
        (!points.is_empty()).then(|| {
            json!({"name": name, "unit": unit, "description": description, "gauge": {"dataPoints": points}})
        })
    };
    let metrics = [
        gauge(
            "st_slo_latency_ms",
            "ms",
            "Latency of an SLO target over a rolling window, by target, series, window and stat",
            latency,
        ),
        gauge(
            "st_slo_over_target_ratio",
            "1",
            "Share of an SLO target's samples over its target in a rolling window",
            over,
        ),
        gauge(
            "st_slo_samples",
            "{sample}",
            "Samples an SLO target holds in a rolling window",
            samples,
        ),
        gauge(
            "st_slo_target_ms",
            "ms",
            "The target in slo/targets.toml for a series: its 99th percentile, or its maximum",
            targets,
        ),
        gauge(
            "st_slo_live_share",
            "1",
            "Share of a phone's reported foreground time that was live, over a rolling window of whole reported minutes",
            live_share,
        ),
        gauge(
            "st_slo_foreground_ms",
            "ms",
            "Foreground milliseconds the paired phones reported in a rolling window of whole reported minutes",
            foreground,
        ),
        gauge(
            "st_slo_min_share_percent",
            "%",
            "The minimum live share, in percent, a phone's foreground time should reach",
            min_share,
        ),
        gauge(
            "st_slo_cpu_cores",
            "{core}",
            "Cores the daemon process used over a rolling window",
            cpu,
        ),
        gauge(
            "st_slo_cpu_max_cores",
            "{core}",
            "The daemon CPU target in cores",
            cpu_max,
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!metrics.is_empty()).then(|| {
        json!({"resourceMetrics": [{"resource": resource(node, DAEMON_SERVICE), "scopeMetrics": [{
            "scope": {"name": "st.slo"}, "metrics": metrics,
        }]}]})
    })
}

/// Send `report` once; `false` when it held nothing to send.
async fn send_once(exporter: &OtlpExporter, report: &Value, now_unix_ms: u64) -> anyhow::Result<bool> {
    let Some(body) = metrics(&exporter.node, report, now_unix_ms) else { return Ok(false) };
    exporter.send(&exporter.metrics_url, &body).await?;
    Ok(true)
}

/// Send the windows once a minute until the process stops. A failed send is reported once and
/// dropped: the next minute's windows replace it.
///
/// `windows` reads the live request-latency windows against the targets, as the daemon's API holds them.
pub async fn run(exporter: OtlpExporter, windows: fn() -> Value) {
    let mut failing = false;
    loop {
        tokio::time::sleep(INTERVAL).await;
        let report = match tokio::task::spawn_blocking(move || smallclaims::task_cpu::blocking("slo.export", windows)).await {
            Ok(report) => report,
            Err(error) => {
                if !failing {
                    failing = true;
                    eprintln!("st3: reading the SLO windows for OpenTelemetry export failed: {error}");
                }
                continue;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        match send_once(&exporter, &report, now).await {
            Ok(_) => failing = false,
            Err(error) if !failing => {
                failing = true;
                eprintln!("st3: OpenTelemetry export of the SLO windows failed: {error:#}");
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(count: u64, p99: f64, over: u64) -> Value {
        json!({"count": count, "p50_ms": 1.5, "p99_ms": p99, "max_ms": p99 + 1.0,
            "over_target": over, "over_target_share": over as f64 / count.max(1) as f64})
    }

    fn windows(count: u64, p99: f64, over: u64) -> Value {
        json!({"1m": window(count, p99, over), "5m": window(count, p99, over), "1h": window(count, p99, over)})
    }

    fn points<'a>(body: &'a Value, name: &str) -> &'a Vec<Value> {
        body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["name"] == name)
            .unwrap_or_else(|| panic!("no {name}"))["gauge"]["dataPoints"]
            .as_array()
            .unwrap()
    }

    fn label<'a>(point: &'a Value, key: &str) -> Option<&'a str> {
        point["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attribute| attribute["key"] == key)
            .and_then(|attribute| attribute["value"]["stringValue"].as_str())
    }

    fn find<'a>(points: &'a [Value], labels: &[(&str, &str)]) -> &'a Value {
        points
            .iter()
            .find(|point| labels.iter().all(|(key, value)| label(point, key) == Some(value)))
            .unwrap_or_else(|| panic!("no point {labels:?}"))
    }

    fn report() -> Value {
        json!({"targets": [
            {"name": "person-read", "p99_ms": 100, "windows": windows(40, 120.0, 3),
             "remote_p99_ms": 300, "remote_windows": windows(0, 0.0, 0)},
            {"name": "sql-statement", "p99_ms": 10, "max_ms": 100, "windows": windows(900, 2.0, 1)},
            {"name": "transaction", "max_ms": 100, "read": windows(10, 4.0, 0),
             "write": windows(5, 9.0, 0), "writer_hold": windows(5, 8.0, 1)},
            {"name": "cpu", "max_cores": 2.0, "windows": {
                "1m": {"cores": 0.4, "measured_seconds": 60}, "5m": {"cores": 0.3, "measured_seconds": 300},
                "1h": {"cores": 0.2, "measured_seconds": 3600}}},
            {"name": "database", "max_live_gb": 5, "size": {"live_gb": 1.0}},
            {"name": "ios-connect", "p99_ms": 1500, "population": "client-observed-latencies",
             "windows": windows(20, 900.0, 0)},
            {"name": "ios-live-share", "min_percent": 99, "population": "client-observed-foreground-ms",
             "windows": {"1m": {"count": 60000, "over_target": 600, "over_target_share": 0.01,
                 "foreground_ms": 60000, "live_ms": 59400, "live_share": 0.99},
              "5m": {"count": 0, "over_target": 0, "over_target_share": 0.0, "foreground_ms": 0, "live_ms": 0, "live_share": null},
              "1h": {"count": 0, "over_target": 0, "over_target_share": 0.0, "foreground_ms": 0, "live_ms": 0, "live_share": null}}},
        ]})
    }

    #[test]
    fn every_target_goes_out_as_gauges_with_bounded_labels() {
        let body = metrics("node-a", &report(), 1_700_000_000_000).unwrap();
        let resource = &body["resourceMetrics"][0]["resource"]["attributes"];
        assert!(resource.to_string().contains("node-a"));
        let latency = points(&body, "st_slo_latency_ms");
        let read = find(latency, &[("target", "person-read"), ("series", "owner"), ("window", "1h"), ("stat", "p99")]);
        assert_eq!(read["asDouble"], 120.0);
        assert_eq!(read["timeUnixNano"], "1700000000000000000");
        let hold = find(latency, &[("target", "transaction"), ("series", "writer_hold"), ("window", "5m"), ("stat", "max")]);
        assert_eq!(hold["asDouble"], 9.0);
        let statement = find(latency, &[("target", "sql-statement"), ("series", "statement"), ("window", "1m"), ("stat", "p50")]);
        assert_eq!(statement["asDouble"], 1.5);
        let share = points(&body, "st_slo_over_target_ratio");
        assert_eq!(find(share, &[("target", "person-read"), ("series", "owner"), ("window", "1m")])["asDouble"], 0.075);
        let target = points(&body, "st_slo_target_ms");
        assert_eq!(find(target, &[("target", "person-read"), ("series", "remote"), ("stat", "p99")])["asDouble"], 300.0);
        assert_eq!(find(target, &[("target", "sql-statement"), ("stat", "max")])["asDouble"], 100.0);
        assert_eq!(find(target, &[("target", "transaction"), ("series", "read"), ("stat", "max")])["asDouble"], 100.0);
        assert_eq!(find(points(&body, "st_slo_cpu_cores"), &[("window", "1m")])["asDouble"], 0.4);
        assert_eq!(points(&body, "st_slo_cpu_max_cores")[0]["asDouble"], 2.0);
        // The database row is not a window and goes out through no gauge here.
        for metric in body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"].as_array().unwrap() {
            for point in metric["gauge"]["dataPoints"].as_array().unwrap() {
                assert_ne!(label(point, "target"), Some("database"));
            }
        }
    }

    #[test]
    fn a_phone_latency_is_a_reported_series_and_its_live_share_has_gauges_of_its_own() {
        let body = metrics("node-a", &report(), 1).unwrap();
        let latency = points(&body, "st_slo_latency_ms");
        assert_eq!(
            find(latency, &[("target", "ios-connect"), ("series", "reported"), ("window", "1m"), ("stat", "p99")])["asDouble"],
            900.0
        );
        assert_eq!(
            find(points(&body, "st_slo_target_ms"), &[("target", "ios-connect"), ("series", "reported")])["asDouble"],
            1500.0
        );
        // Foreground milliseconds are not sample counts, and a minimum is not a latency target.
        for gauge in ["st_slo_samples", "st_slo_over_target_ratio", "st_slo_latency_ms", "st_slo_target_ms"] {
            assert!(
                !points(&body, gauge).iter().any(|point| label(point, "target") == Some("ios-live-share")),
                "{gauge}"
            );
        }
        let share = points(&body, "st_slo_live_share");
        assert_eq!(share.len(), 1, "an empty window has no share");
        assert_eq!(find(share, &[("target", "ios-live-share"), ("window", "1m")])["asDouble"], 0.99);
        assert_eq!(
            find(points(&body, "st_slo_foreground_ms"), &[("target", "ios-live-share"), ("window", "1m")])["asDouble"],
            60000.0
        );
        assert_eq!(points(&body, "st_slo_min_share_percent")[0]["asDouble"], 99.0);
    }

    #[test]
    fn an_empty_window_reports_its_count_and_no_latency() {
        let body = metrics("node-a", &report(), 1).unwrap();
        let samples = points(&body, "st_slo_samples");
        assert_eq!(find(samples, &[("target", "person-read"), ("series", "remote"), ("window", "1h")])["asDouble"], 0.0);
        let latency = points(&body, "st_slo_latency_ms");
        assert!(!latency.iter().any(|point| label(point, "series") == Some("remote")));
        let share = points(&body, "st_slo_over_target_ratio");
        assert!(!share.iter().any(|point| label(point, "series") == Some("remote")));
    }

    #[test]
    fn a_report_with_nothing_in_it_sends_nothing() {
        assert!(metrics("node-a", &Value::Null, 1).is_none());
        assert!(metrics("node-a", &json!({"targets": []}), 1).is_none());
    }

    #[tokio::test]
    async fn the_gauges_reach_the_collector_with_its_headers_and_a_failure_is_dropped() {
        use std::sync::{Arc, Mutex};
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
        type Shared = (Arc<Mutex<Vec<Value>>>, Arc<std::sync::atomic::AtomicU16>);
        async fn collect(
            axum::extract::State((received, status)): axum::extract::State<Shared>,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::http::StatusCode {
            let code = axum::http::StatusCode::from_u16(status.load(std::sync::atomic::Ordering::SeqCst)).unwrap();
            if code.is_success() {
                received.lock().unwrap().push(body);
            }
            code
        }
        let app = axum::Router::new()
            .route("/v1/metrics", axum::routing::post(collect))
            .with_state((received.clone(), status.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let exporter = OtlpExporter::new(
            &super::super::OtlpConfig { endpoint, headers_file: None },
            "node-a",
        )
        .unwrap();
        assert!(send_once(&exporter, &report(), 1_700_000_000_000).await.unwrap());
        assert!(!send_once(&exporter, &Value::Null, 1).await.unwrap());
        status.store(503, std::sync::atomic::Ordering::SeqCst);
        assert!(send_once(&exporter, &report(), 2).await.is_err());
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1, "a refused send is dropped, not queued");
        assert!(!points(&received[0], "st_slo_latency_ms").is_empty());
    }
}
