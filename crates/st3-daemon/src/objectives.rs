//! Objectives and budgets as one list.
//!
//! An objective is a target with windows and an error budget: the daemon's latency targets, its
//! store and CPU targets, the database's size and the paired phone's reported targets, all from
//! `slo/targets.toml` and the same report as `GET /v1/client/request-latency`. A budget is an
//! allowance that resets: each account's weekly and 5-hour limits, from the readings `st usage`
//! shows. Nothing here measures anything. It reads those two sources and says, for each row, its
//! target, its current value, its window, what is left of its budget and what that means.
//!
//! Every row has the same fields, so a terminal, the phone and the web render one shape:
//! `id`, `kind` (`objective` or `budget`), `about`, `target`, `current`, `window`,
//! `remaining_percent` (null when nothing was measured), `status` (`met`, `missed`, `no-data` or
//! `stale`), `resets_at_unix_ms` (budgets only) and `explain`, the row in plain words.

use serde_json::{Value, json};

use crate::store::AccountLimit;

const WINDOWS: [&str; 3] = ["1m", "5m", "1h"];
/// A 99th-percentile target allows one sample in a hundred to run over.
const P99_ALLOWANCE: f64 = 0.01;
const GB: f64 = 1e9;
const MB: f64 = 1e6;

/// Every objective in `report` (the request-latency read's `targets`) and every budget in
/// `limits`, objectives first. `stale_after_ms` is how old a limits reading may be before it is
/// the last known one rather than the current one.
pub fn rows(report: &Value, limits: &[AccountLimit], now_unix_ms: u64, stale_after_ms: u64) -> Vec<Value> {
    let mut rows = Vec::new();
    for target in report["targets"].as_array().into_iter().flatten() {
        let Some(name) = target["name"].as_str() else { continue };
        let about = target["about"].as_str().unwrap_or_default();
        match name {
            "sql-statement" => rows.push(p99(name, about, "SQL statements", target["p99_ms"].as_f64(), &target["windows"])),
            "transaction" => {
                let max = target["max_ms"].as_f64();
                for (kind, label) in [
                    ("read", "read transactions"),
                    ("write", "write transactions"),
                    ("writer_hold", "holds of the store's writer"),
                ] {
                    rows.push(max_latency(&format!("{name}/{kind}"), about, label, max, &target[kind]));
                }
            }
            "cpu" => rows.push(cpu(about, target)),
            "database" => rows.push(database(about, target)),
            _ if target["min_percent"].is_number() => rows.push(share(name, about, target)),
            _ => {
                rows.push(p99(name, about, "requests", target["p99_ms"].as_f64(), &target["windows"]));
                if target["remote_windows"].is_object() {
                    rows.push(p99(
                        &format!("{name}/remote"),
                        about,
                        "requests served from another machine",
                        target["remote_p99_ms"].as_f64(),
                        &target["remote_windows"],
                    ));
                }
            }
        }
    }
    for limit in limits {
        let name = limit.account_ref.as_deref().unwrap_or(&limit.account);
        let stale = now_unix_ms.saturating_sub(limit.measured_at_unix_ms) > stale_after_ms;
        for (period, label, percent, resets) in [
            ("weekly", "weekly", limit.weekly_percent, limit.weekly_resets_at_unix_ms),
            ("5h", "5-hour", limit.five_hour_percent, limit.five_hour_resets_at_unix_ms),
        ] {
            rows.push(budget(name, &limit.account, period, label, percent, resets, limit.measured_at_unix_ms, stale, now_unix_ms));
        }
    }
    rows
}

fn count(window: &Value) -> u64 {
    window["count"].as_u64().unwrap_or_default()
}

fn share_of(window: &Value) -> f64 {
    window["over_target_share"].as_f64().unwrap_or_default()
}

fn ms(value: f64) -> String {
    if value >= 10.0 { format!("{} ms", value.round()) } else { format!("{:.1} ms", value) }
}

fn clamp_percent(value: f64) -> f64 {
    (value.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

fn plural(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// A 99th-percentile target: no more than 1% of samples over, in each window. The budget is that
/// 1%, measured over the hour, the longest window.
fn p99(id: &str, about: &str, what: &str, target_ms: Option<f64>, windows: &Value) -> Value {
    let target = target_ms.unwrap_or_default();
    let hour = &windows["1h"];
    let samples = count(hour);
    let met = WINDOWS.iter().all(|w| share_of(&windows[*w]) <= P99_ALLOWANCE);
    if samples == 0 && WINDOWS.iter().all(|w| count(&windows[*w]) == 0) {
        return json!({
            "id": id, "kind": "objective", "about": about,
            "target": format!("p99 ≤ {}", ms(target)), "current": "no samples", "window": "1h",
            "remaining_percent": null, "status": "no-data",
            "explain": format!(
                "{about}. Goal: 99 of every 100 {what} finish within {}. Nothing has been measured in the last hour, so there is no budget to spend or save yet.",
                ms(target)
            ),
        });
    }
    let remaining = clamp_percent(100.0 * (1.0 - share_of(hour) / P99_ALLOWANCE));
    let p99_now = hour["p99_ms"].as_f64().unwrap_or_default();
    let over = hour["over_target"].as_u64().unwrap_or_default();
    let verdict = if met {
        "Every window is within its allowance.".to_owned()
    } else {
        let missed = WINDOWS
            .iter()
            .filter(|w| share_of(&windows[**w]) > P99_ALLOWANCE)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        format!("The goal is missed in the {missed} window.")
    };
    json!({
        "id": id, "kind": "objective", "about": about,
        "target": format!("p99 ≤ {}", ms(target)),
        "current": format!("p99 {} over {}", ms(p99_now), plural(samples, "sample")),
        "window": "1h", "remaining_percent": remaining,
        "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: 99 of every 100 {what} finish within {}. In the last hour {} of {} were slower ({:.1}%), against an allowance of 1%, so {remaining:.0}% of the error budget is left. The slowest 1% start at {}. {verdict}",
            ms(target), over, plural(samples, "sample"), share_of(hour) * 100.0, ms(p99_now),
        ),
    })
}

/// A maximum target: not one sample over. The budget is all or nothing.
fn max_latency(id: &str, about: &str, what: &str, target_ms: Option<f64>, windows: &Value) -> Value {
    let target = target_ms.unwrap_or_default();
    let hour = &windows["1h"];
    let samples = count(hour);
    if WINDOWS.iter().all(|w| count(&windows[*w]) == 0) {
        return json!({
            "id": id, "kind": "objective", "about": about,
            "target": format!("max ≤ {}", ms(target)), "current": "no samples", "window": "1h",
            "remaining_percent": null, "status": "no-data",
            "explain": format!("{about}. Goal: no {what} take longer than {}. Nothing has been measured in the last hour.", ms(target)),
        });
    }
    let met = WINDOWS.iter().all(|w| windows[*w]["over_target"].as_u64().unwrap_or_default() == 0);
    let over = hour["over_target"].as_u64().unwrap_or_default();
    json!({
        "id": id, "kind": "objective", "about": about,
        "target": format!("max ≤ {}", ms(target)),
        "current": format!("max {} over {}", ms(hour["max_ms"].as_f64().unwrap_or_default()), plural(samples, "sample")),
        "window": "1h", "remaining_percent": if over == 0 { 100.0 } else { 0.0 },
        "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: no {what} take longer than {}; one slow sample misses it. In the last hour {} of {} ran over. {}",
            ms(target), over, plural(samples, "sample"),
            if met { "No window has a slow sample." } else { "A slow sample is still inside a window." },
        ),
    })
}

fn cpu(about: &str, target: &Value) -> Value {
    let max = target["max_cores"].as_f64().unwrap_or_default();
    let cores = |w: &str| target["windows"][w]["cores"].as_f64();
    let Some(hour) = cores("1h") else {
        return json!({
            "id": "cpu", "kind": "objective", "about": about, "target": format!("≤ {max} cores"),
            "current": "not measured", "window": "1h", "remaining_percent": null, "status": "no-data",
            "explain": format!("{about}. The daemon has not measured its CPU yet."),
        });
    };
    let met = WINDOWS.iter().all(|w| cores(w).unwrap_or_default() <= max);
    let remaining = clamp_percent(100.0 * (1.0 - hour / max));
    json!({
        "id": "cpu", "kind": "objective", "about": about,
        "target": format!("≤ {max} cores"), "current": format!("{hour:.2} cores"), "window": "1h",
        "remaining_percent": remaining, "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: the daemon uses at most {max} cores. Over the last hour it used {hour:.2} on average, so {remaining:.0}% of that allowance is left. Last minute {:.2}, last five minutes {:.2}.",
            cores("1m").unwrap_or_default(), cores("5m").unwrap_or_default(),
        ),
    })
}

fn database(about: &str, target: &Value) -> Value {
    let max_live = target["max_live_gb"].as_f64().unwrap_or_default();
    let max_growth = target["max_growth_mb_per_day"].as_f64().unwrap_or_default();
    let size = &target["size"];
    let Some(live) = size["live_bytes"].as_f64() else {
        return json!({
            "id": "database", "kind": "objective", "about": about,
            "target": format!("live ≤ {max_live} GB"), "current": "not measured yet", "window": "1d",
            "remaining_percent": null, "status": "no-data",
            "explain": format!("{about}. The store's size is measured hourly and has not been measured yet."),
        });
    };
    let growth = size["growth_bytes_per_day"].as_f64();
    let met = live / GB <= max_live && growth.is_none_or(|growth| growth / MB <= max_growth);
    let remaining = clamp_percent(100.0 * (1.0 - live / GB / max_live));
    let growth_text = growth.map_or("growth is not known yet".to_owned(), |growth| format!("it is growing {:+.0} MB a day", growth / MB));
    json!({
        "id": "database", "kind": "objective", "about": about,
        "target": format!("live ≤ {max_live} GB, growth ≤ {max_growth} MB a day"),
        "current": format!("live {:.2} GB", live / GB), "window": "1d",
        "remaining_percent": remaining, "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: no more than {max_live} GB of pages in use, growing no faster than {max_growth} MB a day. It holds {:.2} GB, so {remaining:.0}% of the size allowance is left, and {growth_text}.",
            live / GB,
        ),
    })
}

/// Open projection divergences, counted fleet-wide from their faults. Each is a P1 bug, so the
/// target is none, and one open divergence spends the whole allowance.
pub fn divergence(open: u64) -> Value {
    let targets = crate::slo::targets();
    let about = targets.divergence.about.as_str();
    let max_open = targets.divergence.max_open;
    let met = open <= max_open;
    json!({
        "id": "projection-divergence", "kind": "objective", "about": about,
        "target": format!("≤ {max_open} open"), "current": format!("{open} open"), "window": "now",
        "remaining_percent": if met { 100.0 } else { 0.0 }, "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: at most {max_open} open. {}",
            if open == 0 {
                "None is open.".to_owned()
            } else {
                format!(
                    "{} open; `st subject daemon/NODE` for the member that found one shows it with the projection, the claim range, both digests and both builds.",
                    plural(open, "divergence")
                )
            }
        ),
    })
}

/// A minimum share (a phone's live time over its foreground time). The budget is the allowed
/// remainder: with a 99% minimum, 1% of foreground time may be disconnected.
fn share(id: &str, about: &str, target: &Value) -> Value {
    let min = target["min_percent"].as_f64().unwrap_or_default();
    let hour = &target["windows"]["1h"];
    let met = WINDOWS.iter().all(|w| {
        let window = &target["windows"][*w];
        window["over_target"].as_f64().unwrap_or_default() * 100.0
            <= window["foreground_ms"].as_f64().unwrap_or_default() * (100.0 - min)
    });
    let Some(live) = hour["live_share"].as_f64() else {
        return json!({
            "id": id, "kind": "objective", "about": about, "target": format!("live ≥ {min}%"),
            "current": "no reported time", "window": "1h", "remaining_percent": null, "status": "no-data",
            "explain": format!("{about}. No paired phone has reported foreground time in the last hour."),
        });
    };
    let allowed = (100.0 - min) / 100.0;
    let remaining = clamp_percent(100.0 * (1.0 - (1.0 - live) / allowed));
    json!({
        "id": id, "kind": "objective", "about": about,
        "target": format!("live ≥ {min}%"), "current": format!("live {:.1}%", live * 100.0), "window": "1h",
        "remaining_percent": remaining, "status": if met { "met" } else { "missed" },
        "explain": format!(
            "{about}. Goal: connected for at least {min}% of the time the app is open. Over the last hour it was {:.1}%, so {remaining:.0}% of the {:.0}% that may be disconnected is left. This counts only phones that report.",
            live * 100.0, 100.0 - min,
        ),
    })
}

#[allow(clippy::too_many_arguments)]
fn budget(
    name: &str,
    label: &str,
    period: &str,
    period_label: &str,
    percent: Option<f64>,
    resets_at: Option<u64>,
    measured_at: u64,
    stale: bool,
    now: u64,
) -> Value {
    let id = format!("{name}/{period}");
    let reset = resets_at.map_or("no reset time reported".to_owned(), |at| {
        let minutes = (at.saturating_sub(now) / 60_000) as i64;
        if at <= now {
            "it has reset since".to_owned()
        } else if minutes < 120 {
            format!("it resets in {minutes} minutes")
        } else if minutes < 48 * 60 {
            format!("it resets in {} hours", minutes / 60)
        } else {
            format!("it resets in {} days", minutes / (24 * 60))
        }
    });
    let Some(used) = percent else {
        return json!({
            "id": id, "kind": "budget", "account": label, "about": format!("{name}'s {period_label} limit"),
            "target": "100% allowance", "current": "not reported", "window": period_label,
            "remaining_percent": null, "status": "no-data", "resets_at_unix_ms": resets_at,
            "measured_at_unix_ms": measured_at,
            "explain": format!("No seat on {name} has reported its {period_label} limit."),
        });
    };
    let remaining = clamp_percent(100.0 - used);
    let status = if stale { "stale" } else if used >= 100.0 { "missed" } else { "met" };
    json!({
        "id": id, "kind": "budget", "account": label, "about": format!("{name}'s {period_label} limit"),
        "target": "100% allowance", "current": format!("{used:.0}% used"), "window": period_label,
        "remaining_percent": remaining, "status": status, "resets_at_unix_ms": resets_at,
        "measured_at_unix_ms": measured_at,
        "explain": format!(
            "An allowance that resets: {name}'s {period_label} limit. {used:.0}% is used, so {remaining:.0}% is left, and {reset}.{}",
            if stale { " This is the last reading, and it is not recent." } else { "" },
        ),
    })
}

/// The row named `id`, or the single row `id` is an unambiguous prefix of.
pub fn find<'a>(rows: &'a [Value], id: &str) -> Result<&'a Value, String> {
    if let Some(row) = rows.iter().find(|row| row["id"] == id) {
        return Ok(row);
    }
    let matches = rows
        .iter()
        .filter(|row| row["id"].as_str().is_some_and(|candidate| candidate.starts_with(id)))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no objective or budget is named {id}; `st slo ls` lists them")),
        many => Err(format!(
            "{id} matches {}; name one",
            many.iter().filter_map(|row| row["id"].as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(count: u64, p99: f64, over: u64) -> Value {
        json!({"count": count, "p50_ms": 1.0, "p99_ms": p99, "max_ms": p99 + 1.0,
            "over_target": over, "over_target_share": over as f64 / count.max(1) as f64})
    }

    fn windows(count: u64, p99: f64, over: u64) -> Value {
        json!({"1m": window(count, p99, over), "5m": window(count, p99, over), "1h": window(count, p99, over)})
    }

    fn limit(account: &str, account_ref: Option<&str>, weekly: Option<f64>, five: Option<f64>) -> AccountLimit {
        AccountLimit {
            account: account.into(),
            account_ref: account_ref.map(Into::into),
            identified: true,
            driver: "claude".into(),
            plan: None,
            five_hour_percent: five,
            five_hour_resets_at_unix_ms: Some(10_000_000 + 3 * 3_600_000),
            weekly_percent: weekly,
            weekly_resets_at_unix_ms: Some(10_000_000 + 3 * 86_400_000),
            measured_at_unix_ms: 10_000_000,
            measured_by: "agent/a".into(),
            host: "host".into(),
            seats: vec![],
        }
    }

    fn by_id<'a>(rows: &'a [Value], id: &str) -> &'a Value {
        rows.iter().find(|row| row["id"] == id).unwrap_or_else(|| panic!("no row {id}"))
    }

    #[test]
    fn a_p99_target_spends_a_one_percent_budget_over_the_hour() {
        let report = json!({"targets": [
            {"name": "person-read", "about": "Reads a person waits on", "p99_ms": 100,
             "windows": windows(200, 80.0, 1), "remote_p99_ms": 300, "remote_windows": windows(0, 0.0, 0)},
        ]});
        let rows = rows(&report, &[], 0, 0);
        let read = by_id(&rows, "person-read");
        assert_eq!(read["kind"], "objective");
        assert_eq!(read["target"], "p99 ≤ 100 ms");
        assert_eq!(read["window"], "1h");
        // One in 200 over is 0.5%: half the 1% allowance is gone.
        assert_eq!(read["remaining_percent"], 50.0);
        assert_eq!(read["status"], "met");
        assert!(read["explain"].as_str().unwrap().contains("50% of the error budget is left"));
        assert_eq!(by_id(&rows, "person-read/remote")["status"], "no-data");
    }

    #[test]
    fn a_target_missed_in_any_window_is_missed_and_its_budget_is_empty() {
        let mut slow = windows(100, 80.0, 0);
        slow["5m"] = window(100, 400.0, 5);
        let report = json!({"targets": [{"name": "write-ack", "about": "Writes", "p99_ms": 100, "windows": slow}]});
        let rows = rows(&report, &[], 0, 0);
        let row = by_id(&rows, "write-ack");
        assert_eq!(row["status"], "missed");
        assert!(row["explain"].as_str().unwrap().contains("missed in the 5m window"));
        // The hour itself is clean, so the budget over the hour is whole.
        assert_eq!(row["remaining_percent"], 100.0);
    }

    #[test]
    fn a_max_target_has_all_or_nothing_budgets_for_each_store_work() {
        let report = json!({"targets": [{"name": "transaction", "about": "Any transaction", "max_ms": 100,
            "read": windows(10, 4.0, 0), "write": windows(10, 9.0, 2), "writer_hold": windows(0, 0.0, 0)}]});
        let rows = rows(&report, &[], 0, 0);
        assert_eq!(by_id(&rows, "transaction/read")["remaining_percent"], 100.0);
        assert_eq!(by_id(&rows, "transaction/write")["remaining_percent"], 0.0);
        assert_eq!(by_id(&rows, "transaction/write")["status"], "missed");
        assert_eq!(by_id(&rows, "transaction/writer_hold")["status"], "no-data");
    }

    #[test]
    fn cpu_database_and_a_phone_share_read_against_their_own_allowances() {
        let report = json!({"targets": [
            {"name": "cpu", "about": "Daemon CPU", "max_cores": 2.0, "windows": {
                "1m": {"cores": 0.5}, "5m": {"cores": 0.5}, "1h": {"cores": 0.5}}},
            {"name": "database", "about": "The store", "max_live_gb": 8.0, "max_growth_mb_per_day": 300,
             "size": {"live_bytes": 2_000_000_000u64, "growth_bytes_per_day": 60_000_000i64}},
            {"name": "ios-live-share", "about": "Phone live share", "min_percent": 99,
             "population": "client-observed-foreground-ms", "windows": {
                "1m": {"over_target": 0, "foreground_ms": 0, "live_share": null},
                "5m": {"over_target": 0, "foreground_ms": 0, "live_share": null},
                "1h": {"over_target": 6_000, "foreground_ms": 1_000_000, "live_share": 0.994}}},
        ]});
        let rows = rows(&report, &[], 0, 0);
        assert_eq!(by_id(&rows, "cpu")["remaining_percent"], 75.0);
        assert_eq!(by_id(&rows, "database")["remaining_percent"], 75.0);
        assert_eq!(by_id(&rows, "database")["status"], "met");
        // 0.6% disconnected of the 1% allowed leaves 40%.
        assert_eq!(by_id(&rows, "ios-live-share")["remaining_percent"], 40.0);
    }

    #[test]
    fn limits_become_weekly_and_five_hour_budgets_named_by_the_declared_account() {
        let now = 10_000_000 + 60_000;
        let limits = [
            limit("claude/abc", Some("ada/claude"), Some(40.0), Some(100.0)),
            limit("claude/def", None, None, None),
        ];
        let rows = rows(&json!({"targets": []}), &limits, now, 3_600_000);
        let weekly = by_id(&rows, "ada/claude/weekly");
        assert_eq!(weekly["kind"], "budget");
        assert_eq!(weekly["remaining_percent"], 60.0);
        assert_eq!(weekly["status"], "met");
        assert!(weekly["explain"].as_str().unwrap().contains("resets in 2 days"));
        let five = by_id(&rows, "ada/claude/5h");
        assert_eq!(five["remaining_percent"], 0.0);
        assert_eq!(five["status"], "missed");
        assert!(five["explain"].as_str().unwrap().contains("resets in 2 hours"));
        assert_eq!(by_id(&rows, "claude/def/weekly")["status"], "no-data");
    }

    #[test]
    fn an_old_reading_is_stale_and_says_so() {
        let limits = [limit("claude/abc", None, Some(10.0), Some(10.0))];
        let rows = rows(&json!({"targets": []}), &limits, 10_000_000 + 7_200_000, 3_600_000);
        let row = by_id(&rows, "claude/abc/weekly");
        assert_eq!(row["status"], "stale");
        assert!(row["explain"].as_str().unwrap().contains("not recent"));
    }

    #[test]
    fn find_takes_an_exact_id_or_one_unambiguous_prefix() {
        let rows = vec![json!({"id": "write-ack"}), json!({"id": "transaction/read"}), json!({"id": "transaction/write"})];
        assert_eq!(find(&rows, "write-ack").unwrap()["id"], "write-ack");
        assert_eq!(find(&rows, "write").unwrap()["id"], "write-ack");
        assert!(find(&rows, "transaction").unwrap_err().contains("transaction/read, transaction/write"));
        assert!(find(&rows, "nope").unwrap_err().contains("st slo ls"));
    }

    #[test]
    fn the_live_report_yields_a_row_for_every_target_in_the_targets_file() {
        let report = crate::api::request_latency_target_windows();
        let rows = rows(&report, &[], 0, 0);
        for latency in &crate::slo::targets().latency {
            assert!(rows.iter().any(|row| row["id"] == latency.name.as_str()), "{} is missing", latency.name);
        }
        for id in ["sql-statement", "transaction/read", "cpu", "database"] {
            assert!(rows.iter().any(|row| row["id"] == id), "{id}");
        }
        let mut ids = rows.iter().map(|row| row["id"].as_str().unwrap()).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), rows.len(), "ids are unique");
    }
}
