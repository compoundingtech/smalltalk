//! E2E proof that the real `st2` binary exports OTLP/HTTP JSON spans into an otelite receiver
//! when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (and stays silent without one). The receiver is the
//! `otelite` binary from effect-utils (`packages.x86_64-linux.otelite`); the flake check wires it
//! in via `ST2_OTELITE_BIN`.
//!
//! Needs `ST2_OTELITE_BIN` on a gate run — HARD failure if absent unless `ST2_ALLOW_OTEL_SKIP`
//! is set (a gate must not silently skip).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `st2 up --once` under otelite's managed receiver lifecycle.
///
/// `otelite run` owns the receiver and shuts it down after the command exits, so the test does
/// not depend on the surrounding process's stdin or on detached pipe-reader threads. For the
/// no-export case, `env -u` removes the endpoint that otelite injects into its child while leaving
/// the receiver live to catch any unintended traffic.
fn run_with_capture(otelite: &Path, out_dir: &Path, catalog: &Path, export: bool) -> Output {
    let bin = env!("CARGO_BIN_EXE_st2");
    let bin_dir = Path::new(bin).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut command = Command::new(otelite);
    command
        .args(["run", "--out"])
        .arg(out_dir)
        .args(["--protocol", "http/json", "--"]);
    if !export {
        command.args(["env", "-u", "OTEL_EXPORTER_OTLP_ENDPOINT"]);
    }
    command
        .arg(bin)
        .args(["up", "--catalog"])
        .arg(catalog)
        .arg("--once")
        .env("PATH", path)
        .output()
        .expect("run st2 up --once under otelite")
}

/// Flatten an OTLP/HTTP-JSON `ExportMetricsServiceRequest` into its metric records.
fn metric_records(line: &str) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return records;
    };
    let Some(resource_metrics) = value.get("resourceMetrics").and_then(|v| v.as_array()) else {
        return records;
    };
    for resource_metric in resource_metrics {
        let Some(scope_metrics) = resource_metric
            .get("scopeMetrics")
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for scope_metric in scope_metrics {
            if let Some(batch) = scope_metric.get("metrics").and_then(|v| v.as_array()) {
                records.extend(batch.iter().cloned());
            }
        }
    }
    records
}
/// Flatten an OTLP/HTTP-JSON `ExportTraceServiceRequest` into its spans.
fn span_records(line: &str) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return records;
    };
    let Some(resource_spans) = value.get("resourceSpans").and_then(|v| v.as_array()) else {
        return records;
    };
    for resource_span in resource_spans {
        let Some(scope_spans) = resource_span.get("scopeSpans").and_then(|v| v.as_array()) else {
            continue;
        };
        for scope_span in scope_spans {
            if let Some(batch) = scope_span.get("spans").and_then(|v| v.as_array()) {
                records.extend(batch.iter().cloned());
            }
        }
    }
    records
}

/// A data point's string-valued attribute by key, if present.
fn string_attr<'a>(point: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    point["attributes"]
        .as_array()?
        .iter()
        .find(|attr| attr["key"].as_str() == Some(key))?["value"]
        .get("stringValue")
        .and_then(|v| v.as_str())
}

/// Flatten an OTLP/HTTP-JSON `ExportLogsServiceRequest` into its log records.
fn log_records(line: &str) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return records;
    };
    let Some(resource_logs) = value.get("resourceLogs").and_then(|v| v.as_array()) else {
        return records;
    };
    for resource_log in resource_logs {
        let Some(scope_logs) = resource_log.get("scopeLogs").and_then(|v| v.as_array()) else {
            continue;
        };
        for scope_log in scope_logs {
            if let Some(batch) = scope_log.get("logRecords").and_then(|v| v.as_array()) {
                records.extend(batch.iter().cloned());
            }
        }
    }
    records
}

#[test]
fn st2_exports_spans_to_otelite_when_endpoint_is_set() {
    let Some(otelite) = std::env::var_os("ST2_OTELITE_BIN").map(PathBuf::from) else {
        assert!(
            std::env::var_os("ST2_ALLOW_OTEL_SKIP").is_some(),
            "`ST2_OTELITE_BIN` not set — can't prove OTLP export. Set ST2_ALLOW_OTEL_SKIP=1 to skip."
        );
        eprintln!("SKIP st2_exports_spans_to_otelite: ST2_OTELITE_BIN not set");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let cap_dir = tmp.path().join("cap");
    let empty_catalog = tmp.path().join("catalog");
    std::fs::create_dir_all(&empty_catalog).unwrap();

    let out = run_with_capture(&otelite, &cap_dir, &empty_catalog, true);
    assert!(
        out.status.success(),
        "otelite run failed while exporting st2 telemetry.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("BatchLogProcessor.Emit.AfterShutdown"),
        "logger provider must remain usable for the lifetime of the global tracing bridge:\n{stderr}"
    );

    let traces =
        std::fs::read_to_string(cap_dir.join("traces.ndjson")).expect("traces.ndjson written");
    let all_spans: Vec<serde_json::Value> = traces.lines().flat_map(span_records).collect();
    let reconcile_spans: Vec<&serde_json::Value> = all_spans
        .iter()
        .filter(|span| span["name"].as_str() == Some("st2.reconcile_pass"))
        .collect();
    assert_eq!(
        reconcile_spans.len(),
        1,
        "one invocation must export exactly one reconcile pass span:\n{traces}"
    );
    assert!(
        traces.contains("st2-cli"),
        "service.name st2-cli missing from capture:\n{traces}"
    );
    let reconcile_span = reconcile_spans[0];

    // The empty catalog exercises the bounded aggregate hierarchy: no hook consumer exists,
    // so hook verification is correctly absent rather than represented by a fake no-op span.
    assert_eq!(
        all_spans.len(),
        6,
        "one empty-catalog pass must export one root plus exactly five semantic children:\n{traces}"
    );
    let root_span_id = reconcile_span["spanId"]
        .as_str()
        .expect("reconcile root spanId");
    let root_trace_id = reconcile_span["traceId"]
        .as_str()
        .expect("reconcile root traceId");
    let expected_children = [
        ("st2.catalog.lock", "shared"),
        ("st2.catalog.discover", "catalog"),
        ("st2.catalog.materialize", "catalog"),
        ("st2.runtime.observe", "all sessions"),
        ("st2.reconcile.execute", "apply plan"),
    ];
    let children: Vec<&serde_json::Value> = all_spans
        .iter()
        .filter(|span| span["parentSpanId"].as_str() == Some(root_span_id))
        .collect();
    assert_eq!(
        children.len(),
        expected_children.len(),
        "every expected semantic span must be a direct root child:\n{traces}"
    );
    for (name, label) in expected_children {
        let matching: Vec<&&serde_json::Value> = children
            .iter()
            .filter(|span| span["name"].as_str() == Some(name))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "{name} must occur exactly once as a direct child:\n{traces}"
        );
        let child = matching[0];
        assert_eq!(
            child["parentSpanId"].as_str(),
            Some(root_span_id),
            "{name} must be parented directly to st2.reconcile_pass:\n{child}"
        );
        assert_eq!(
            child["traceId"].as_str(),
            Some(root_trace_id),
            "{name} must share the reconcile root trace:\n{child}"
        );
        assert_eq!(
            string_attr(child, "span.label"),
            Some(label),
            "{name} must carry its bounded span.label:\n{child}"
        );
    }
    for span in &all_spans {
        assert!(
            string_attr(span, "span.label").is_some_and(|label| !label.is_empty()),
            "every first-party root and child span must carry a non-empty span.label:\n{span}"
        );
    }

    // PR2: the same run must deliver metric points over the shared OTLP/HTTP endpoint.
    // An empty-catalog `up --once` records exactly one reconcile pass (pass) plus its
    // duration histogram sample.
    let metrics =
        std::fs::read_to_string(cap_dir.join("metrics.ndjson")).expect("metrics.ndjson written");
    let all_metrics: Vec<serde_json::Value> = metrics.lines().flat_map(metric_records).collect();

    let passes: Vec<&serde_json::Value> = all_metrics
        .iter()
        .filter(|m| m["name"].as_str() == Some("reconcile_passes_total"))
        .collect();
    assert_eq!(
        passes.len(),
        1,
        "one invocation must export one cumulative reconcile counter record:\n{metrics}"
    );
    let pass_points = passes[0]["sum"]["dataPoints"]
        .as_array()
        .expect("reconcile counter data points");
    assert_eq!(
        pass_points.len(),
        1,
        "reconcile counter export must contain one logical data point:\n{}",
        passes[0]
    );
    let pass_point = &pass_points[0];
    assert_eq!(
        pass_point["asInt"].as_i64(),
        Some(1),
        "one invocation must contribute exactly one reconcile pass:\n{}",
        passes[0]
    );
    assert_eq!(
        string_attr(pass_point, "result"),
        Some("pass"),
        "reconcile passes counter must carry result=pass:\n{}",
        passes[0]
    );

    // The view must replace the SDK's millisecond-tuned default boundaries with seconds-scale
    // buckets, or every sub-second pass sample collapses into the lowest bucket (P2).
    let durations: Vec<&serde_json::Value> = all_metrics
        .iter()
        .filter(|m| m["name"].as_str() == Some("reconcile_pass_duration_seconds"))
        .collect();
    assert_eq!(
        durations.len(),
        1,
        "one invocation must export one cumulative reconcile histogram record:\n{metrics}"
    );
    let duration_points = durations[0]["histogram"]["dataPoints"]
        .as_array()
        .expect("reconcile duration data points");
    assert_eq!(
        duration_points.len(),
        1,
        "reconcile histogram export must contain one logical data point:\n{}",
        durations[0]
    );
    let duration_point = &duration_points[0];
    assert_eq!(
        duration_point["count"].as_u64(),
        Some(1),
        "one invocation must contribute exactly one duration sample:\n{}",
        durations[0]
    );
    let bucket_sample_count: u64 = duration_point["bucketCounts"]
        .as_array()
        .expect("duration bucket counts")
        .iter()
        .map(|count| count.as_u64().expect("integer bucket count"))
        .sum();
    assert_eq!(
        bucket_sample_count, 1,
        "duration buckets must contain exactly one sample:\n{}",
        durations[0]
    );
    let bounds = &duration_point["explicitBounds"];
    assert_eq!(
        bounds.as_array().map(Vec::len),
        Some(12),
        "duration histogram must carry the 12 seconds-scale boundaries:\n{}",
        durations[0]
    );
    assert_eq!(
        bounds[0].as_f64(),
        Some(0.001),
        "lowest duration bucket must be 1ms, not the SDK default:\n{}",
        durations[0]
    );

    // PR3: the same run must deliver log records through the tracing facade, and the
    // deterministic per-pass completion INFO record must carry the reconcile-pass span
    // context (trace + span ids), proving the tracing→OTel log path end to end.
    let logs = std::fs::read_to_string(cap_dir.join("logs.ndjson")).expect("logs.ndjson written");
    let completions: Vec<serde_json::Value> = logs
        .lines()
        .flat_map(log_records)
        .filter(|record| {
            record.pointer("/body/stringValue").and_then(|b| b.as_str())
                == Some("reconcile pass complete")
        })
        .collect();
    assert_eq!(
        completions.len(),
        1,
        "one invocation must export exactly one reconcile completion log:\n{logs}"
    );
    let completion = &completions[0];
    assert_eq!(
        completion["severityText"].as_str(),
        Some("INFO"),
        "completion record must be INFO:\n{completion}"
    );
    assert_eq!(
        completion["severityNumber"].as_i64(),
        Some(9),
        "INFO maps to severity number 9:\n{completion}"
    );
    let trace_id = completion["traceId"].as_str().expect("traceId present");
    let span_id = completion["spanId"].as_str().expect("spanId present");
    assert_eq!(
        trace_id.len(),
        32,
        "log must carry a trace id:\n{completion}"
    );
    assert_eq!(span_id.len(), 16, "log must carry a span id:\n{completion}");
    assert_eq!(
        reconcile_span["traceId"].as_str(),
        Some(trace_id),
        "completion log must correlate with the sole reconcile pass span:\n{completion}"
    );
    assert_eq!(
        reconcile_span["spanId"].as_str(),
        Some(span_id),
        "completion log must carry the sole reconcile pass span id:\n{completion}"
    );
}

#[test]
fn st2_without_endpoint_does_not_error() {
    let Some(otelite) = std::env::var_os("ST2_OTELITE_BIN").map(PathBuf::from) else {
        assert!(
            std::env::var_os("ST2_ALLOW_OTEL_SKIP").is_some(),
            "`ST2_OTELITE_BIN` not set — can't prove OTLP silence. Set ST2_ALLOW_OTEL_SKIP=1 to skip."
        );
        eprintln!("SKIP st2_without_endpoint_does_not_error: ST2_OTELITE_BIN not set");
        return;
    };

    // No-op guarantee: an unset OTEL_EXPORTER_OTLP_ENDPOINT must keep every command working
    // AND emit no telemetry at all. Run a real `st2 up --once` against a scratch EMPTY
    // catalog with a live otelite receiver attached; if an always-on-export regression ever
    // lands, the receiver flushes non-empty ndjson files here and the test fails — a bare
    // `--version` smoke could never catch that.
    let tmp = tempfile::tempdir().unwrap();
    let cap_dir = tmp.path().join("cap");
    let empty_catalog = tmp.path().join("catalog");
    std::fs::create_dir_all(&empty_catalog).unwrap();

    let out = run_with_capture(&otelite, &cap_dir, &empty_catalog, false);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "st2 up --once must succeed without an OTLP endpoint\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    for name in ["traces.ndjson", "metrics.ndjson", "logs.ndjson"] {
        let path = cap_dir.join(name);
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            bytes, 0,
            "{name} must be absent or empty when OTEL_EXPORTER_OTLP_ENDPOINT is unset, got {bytes} bytes"
        );
    }
}
