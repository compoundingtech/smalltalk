//! The live SLO report, which only the API can build, against the OTLP gauges.

use serde_json::Value;

use crate::otlp::slo::metrics;

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

#[test]
fn the_live_report_names_every_target_in_the_targets_file() {
    let report = crate::api::request_latency_target_windows();
    assert!(report.get("paths").is_none(), "the collector's read skips every path");
    let body = metrics("node-a", &report, 1).unwrap();
    let samples = points(&body, "st_slo_samples");
    for latency in &crate::slo::targets().latency {
        assert!(
            samples.iter().any(|point| label(point, "target") == Some(latency.name.as_str())),
            "{} is missing",
            latency.name
        );
    }
    // A share target is a minimum on its own gauge, never a sample count.
    let minimums = points(&body, "st_slo_min_share_percent");
    for share in &crate::slo::targets().share {
        assert!(minimums.iter().any(|point| label(point, "target") == Some(share.name.as_str())));
        assert!(!samples.iter().any(|point| label(point, "target") == Some(share.name.as_str())));
    }
    for store in ["sql-statement", "transaction"] {
        assert!(samples.iter().any(|point| label(point, "target") == Some(store)), "{store}");
    }
    assert!(!points(&body, "st_slo_cpu_cores").is_empty());
}
