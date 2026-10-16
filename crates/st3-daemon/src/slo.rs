//! The daemon's targets, from `slo/targets.toml`, which is built in. The same file is what
//! `st doctor` and CI's daemon_load read. A target describes what we aim for; it gates nothing.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use smallclaims::windows::StoreWork;

/// The targets file this daemon was built with.
pub const SOURCE: &str = include_str!("../../../slo/targets.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    pub latency: Vec<Latency>,
    pub share: Vec<Share>,
    pub statement: Statement,
    pub transaction: Transaction,
    pub cpu: Cpu,
    pub divergence: Divergence,
    pub database: Database,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Latency {
    pub name: String,
    pub about: String,
    pub p99_ms: u64,
    /// The same reads served through this daemon from another machine that owns them.
    pub remote_p99_ms: Option<u64>,
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub name: String,
    pub about: String,
    pub min_percent: u64,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub about: String,
    pub p99_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub about: String,
    pub max_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cpu {
    pub about: String,
    pub max_cores: f64,
}

/// Projection divergences, counted fleet-wide from their open faults. A projection is a pure
/// function of the claims, so each one is a bug.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    pub about: String,
    pub max_open: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub about: String,
    /// The pages in use, not counting free pages SQLite reuses before the file grows.
    pub max_live_gb: f64,
    /// Net daily growth of the pages in use: what is written less what checkpoints free.
    pub max_growth_mb_per_day: f64,
}

impl Latency {
    /// The target a sample served here (`remote` false) or from another machine misses past.
    pub fn target(&self, remote: bool) -> Duration {
        let ms = if remote {
            self.remote_p99_ms.unwrap_or(self.p99_ms)
        } else {
            self.p99_ms
        };
        Duration::from_millis(ms)
    }
}

/// Read and check a targets file. A missing, unknown or nonsensical field is an error, never a
/// default.
pub fn parse(text: &str) -> Result<Targets> {
    let targets: Targets = toml::from_str(text).context("the targets file is not valid")?;
    let mut names = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for latency in &targets.latency {
        ensure!(
            names.insert(latency.name.as_str()),
            "target {} is named twice",
            latency.name
        );
        ensure!(latency.p99_ms > 0, "target {} has no latency", latency.name);
        ensure!(
            latency.remote_p99_ms.is_none_or(|remote| remote >= latency.p99_ms),
            "target {}'s remote latency is below its own",
            latency.name
        );
        ensure!(!latency.paths.is_empty(), "target {} has no paths", latency.name);
        for path in &latency.paths {
            if path.starts_with("client/ios/") {
                ensure!(paths.insert(path.as_str()), "path {path:?} has two targets");
                continue;
            }
            let (method, route) = path
                .split_once(' ')
                .with_context(|| format!("path {path:?} names no method"))?;
            ensure!(
                matches!(
                    method,
                    "GET" | "POST" | "PUT" | "DELETE" | "stream" | "publish"
                ),
                "path {path:?} has an unknown method"
            );
            ensure!(
                matches!(method, "stream" | "publish") || route.starts_with("/v1/"),
                "path {path:?} is not a daemon route"
            );
            ensure!(paths.insert(path.as_str()), "path {path:?} has two targets");
        }
    }
    for share in &targets.share {
        ensure!(names.insert(share.name.as_str()), "duplicate share target");
        ensure!(share.min_percent > 0 && share.min_percent <= 100, "invalid share target");
        ensure!(share.path.starts_with("client/ios/") && paths.insert(&share.path), "invalid share path");
    }
    ensure!(
        targets.statement.p99_ms > 0 && targets.statement.max_ms >= targets.statement.p99_ms,
        "the statement targets are inconsistent"
    );
    ensure!(targets.transaction.max_ms > 0, "the transaction target is zero");
    ensure!(
        targets.cpu.max_cores.is_finite() && targets.cpu.max_cores > 0.0,
        "the CPU target is not a positive number of cores"
    );
    ensure!(
        [targets.database.max_live_gb, targets.database.max_growth_mb_per_day]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0),
        "the database targets are not positive sizes"
    );
    Ok(targets)
}

/// The built-in targets. The file is checked by a test, so this cannot fail in a release.
pub fn targets() -> &'static Targets {
    static TARGETS: OnceLock<Targets> = OnceLock::new();
    TARGETS.get_or_init(|| parse(SOURCE).expect("the built-in targets file is valid"))
}

impl Targets {
    /// Which target `path` (`GET /v1/client/now`, `stream agents`) counts toward, by position.
    pub fn index_of(&self, path: &str) -> Option<usize> {
        self.latency
            .iter()
            .position(|latency| latency.paths.iter().any(|p| p == path))
    }

    /// The target `path` counts toward.
    pub fn for_path(&self, path: &str) -> Option<&Latency> {
        self.index_of(path).map(|index| &self.latency[index])
    }

    pub fn statement_target(&self) -> Duration {
        Duration::from_millis(self.statement.p99_ms)
    }

    pub fn transaction_target(&self) -> Duration {
        Duration::from_millis(self.transaction.max_ms)
    }
}

/// Give the store's windows their targets, once per process.
pub fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let targets = targets();
        smallclaims::windows::set_store_target(
            StoreWork::Statement,
            Some(targets.statement_target()),
        );
        for work in [
            StoreWork::ReadTransaction,
            StoreWork::WriteTransaction,
            StoreWork::WriterHold,
        ] {
            smallclaims::windows::set_store_target(work, Some(targets.transaction_target()));
        }
    });
}

/// How a target fares in its windows.
pub struct Verdict {
    pub met: bool,
    pub message: String,
}

fn share(window: &Value) -> f64 {
    window["over_target_share"].as_f64().unwrap_or_default()
}

fn describe_window(name: &str, window: &Value) -> String {
    if window["count"].as_u64().unwrap_or_default() == 0 {
        return format!("{name} none");
    }
    format!(
        "{name} p99 {} ms, max {} ms, {:.1}% over of {}",
        round(window["p99_ms"].as_f64().unwrap_or_default()),
        round(window["max_ms"].as_f64().unwrap_or_default()),
        share(window) * 100.0,
        window["count"]
    )
}

fn round(ms: f64) -> f64 {
    if ms >= 10.0 {
        ms.round()
    } else {
        (ms * 10.0).round() / 10.0
    }
}

const WINDOWS: [&str; 3] = ["1m", "5m", "1h"];

/// A p99 target is missed in a window when more than 1% of its samples are over; a max target
/// when any one is. Both counts are exact, not read from the histogram.
pub fn verdict(windows: &Value, p99: bool) -> Verdict {
    let met = WINDOWS.iter().all(|name| {
        let window = &windows[*name];
        if p99 {
            share(window) <= 0.01
        } else {
            window["over_target"].as_u64().unwrap_or_default() == 0
        }
    });
    let message = WINDOWS
        .iter()
        .map(|name| describe_window(name, &windows[*name]))
        .collect::<Vec<_>>()
        .join(" · ");
    Verdict { met, message }
}

/// This member's newest store size, which the daemon measures hourly.
static DATABASE: std::sync::Mutex<Option<crate::store::DatabaseSize>> = std::sync::Mutex::new(None);

/// Remember the newest store size for its readers.
pub fn note_database_size(size: &crate::store::DatabaseSize) {
    *DATABASE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(size.clone());
}

/// The newest store size the daemon measured: the one sample every reader of the store's size
/// and growth uses, the database target and disk-filling conditions alike.
pub fn database_size() -> Option<crate::store::DatabaseSize> {
    DATABASE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// The store's windows and the CPU's, and the store's size, with their targets.
pub fn store_report(cpu: Value) -> Vec<Value> {
    let targets = targets();
    let series = |work| smallclaims::windows::store_series(work).snapshot(std::time::Instant::now());
    vec![
        json!({
            "name": "sql-statement",
            "about": targets.statement.about,
            "p99_ms": targets.statement.p99_ms,
            "max_ms": targets.statement.max_ms,
            "windows": series(StoreWork::Statement),
        }),
        json!({
            "name": "transaction",
            "about": targets.transaction.about,
            "max_ms": targets.transaction.max_ms,
            "read": series(StoreWork::ReadTransaction),
            "write": series(StoreWork::WriteTransaction),
            "writer_hold": series(StoreWork::WriterHold),
        }),
        writer_hold_row(),
        json!({
            "name": "cpu",
            "about": targets.cpu.about,
            "max_cores": targets.cpu.max_cores,
            "windows": cpu,
        }),
        json!({
            "name": "database",
            "about": targets.database.about,
            "max_live_gb": targets.database.max_live_gb,
            "max_growth_mb_per_day": targets.database.max_growth_mb_per_day,
            "size": database_size().map_or(Value::Null, |size| json!(size)),
        }),
    ]
}

/// Open read snapshots reported beside the writer's holds.
const OPEN_READS: usize = 8;

/// The writer-hold objective: each hold of the store's single writer against the transaction
/// target, and who held it; each read transaction past that target, and who kept it open.
fn writer_hold_row() -> Value {
    let targets = targets();
    let series = |work| smallclaims::windows::store_series(work).snapshot(std::time::Instant::now());
    let mut row = json!({
        "name": "writer-hold",
        "about": "Each hold of the store's single writer, and each read transaction past the same target, by who held it: a request route or background task, its client, and the code that asked",
        "max_ms": targets.transaction.max_ms,
        "windows": series(StoreWork::WriterHold),
        "read_windows": series(StoreWork::ReadTransaction),
        "open_reads": smallclaims::sqlite::open_snapshots_over(targets.transaction_target(), OPEN_READS),
        "threads": process_threads(),
    });
    if let (Value::Object(row), Value::Object(holds)) = (&mut row, smallclaims::windows::holds::report()) {
        row.extend(holds);
    }
    row
}

/// Thread names reported beside the writer's holds, by how many threads have each.
const THREAD_NAMES: usize = 8;

/// This process's threads, in all and by name, from `/proc/self/task`; null where there is none.
/// Callers waiting for the store run on blocking threads, so their count rises with the waits.
fn process_threads() -> Value {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Value::Null;
    };
    let mut names = std::collections::BTreeMap::<String, u64>::new();
    for task in tasks.flatten() {
        let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        *names.entry(name.trim().to_owned()).or_default() += 1;
    }
    let total = names.values().sum::<u64>();
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let by_name = names
        .into_iter()
        .take(THREAD_NAMES)
        .map(|(name, count)| json!({"name": name, "count": count}))
        .collect::<Vec<_>>();
    json!({"total": total, "by_name": by_name})
}

/// `st doctor`'s line for the writer-hold objective: met when no hold or read transaction in any
/// window passed the target and none in progress has. It names the holder of the most writer
/// time in the hour and the oldest hold in progress past the target.
fn writer_hold_line(target: &Value) -> (String, &'static str, String) {
    let max = target["max_ms"].as_f64().unwrap_or_default();
    let holds = verdict(&target["windows"], false);
    let reads = verdict(&target["read_windows"], false);
    let oldest = &target["in_progress"][0];
    let held_now = oldest["held_ms"].as_f64().filter(|held| *held > max);
    let open = target["open_reads"].as_array().map_or(0, Vec::len);
    let mut message = format!("max ≤ {max} ms: {}", holds.message);
    if let Some(top) = target["holders"].get(0) {
        message.push_str(&format!(
            "; most held by {} ({} holds, {} ms in all, max {} ms in 1h)",
            top["holder"].as_str().unwrap_or_default(),
            top["count"],
            round(top["total_ms"].as_f64().unwrap_or_default()),
            round(top["max_ms"].as_f64().unwrap_or_default()),
        ));
    }
    if let Some(held) = held_now {
        message.push_str(&format!(
            "; held now by {} for {} ms",
            oldest["holder"].as_str().unwrap_or_default(),
            round(held)
        ));
    }
    if !reads.met || open > 0 {
        let longest = target["long_reads"].get(0).map_or(String::new(), |read| {
            format!(", last by {}", read["holder"].as_str().unwrap_or_default())
        });
        message.push_str(&format!("; reads past target: {}{longest}; {open} open", reads.message));
    }
    let waiting = &target["waiting"];
    if waiting["writer"].as_u64().is_some_and(|n| n > 0) || waiting["read"].as_u64().is_some_and(|n| n > 0) {
        message.push_str(&format!("; waiting now: {} for the writer, {} for a read", waiting["writer"], waiting["read"]));
    }
    if let Some(total) = target["threads"]["total"].as_u64() {
        message.push_str(&format!("; {total} threads"));
    }
    let met = holds.met && reads.met && held_now.is_none() && open == 0;
    ("slo/writer-hold".into(), if met { "pass" } else { "info" }, message)
}

/// `st doctor`'s lines for the windows the request-latency read reports: one per target, and one
/// for each of the ten paths furthest over their target in the last five minutes. A miss is
/// `info`, not `warn`: a target describes, and `st doctor --strict` gates releases.
pub fn doctor_lines(report: &Value) -> Vec<(String, &'static str, String)> {
    let status = |met: bool| if met { "pass" } else { "info" };
    let mut lines = Vec::new();
    for target in report["targets"].as_array().into_iter().flatten() {
        let name = target["name"].as_str().unwrap_or_default();
        if target["population"].as_str().is_some_and(|p| p.starts_with("client-observed")) {
            let min_percent = target["min_percent"].as_u64();
            let share = min_percent.is_some();
            let seen = WINDOWS.iter().any(|w| target["windows"][*w]["count"].as_u64().unwrap_or(0) > 0);
            let met = min_percent.map_or_else(|| verdict(&target["windows"],true).met, |min| {
                WINDOWS.iter().all(|w| {
                    let row=&target["windows"][*w];
                    u128::from(row["over_target"].as_u64().unwrap_or(0))*100
                        <= u128::from(row["foreground_ms"].as_u64().unwrap_or(0))*u128::from(100-min)
                })
            });
            let text = WINDOWS.iter().map(|w| {
                let row = &target["windows"][*w];
                if share {format!("{w} {} live / {} foreground ms",row["live_ms"],row["foreground_ms"])}
                else {describe_window(w,row)}
            }).collect::<Vec<_>>().join(" · ");
            let goal=min_percent.map_or_else(||format!("p99 ≤ {} ms",target["p99_ms"]),|min|format!("live ≥ {min}%"));
            lines.push((format!("slo/{name}"), if seen {status(met)} else {"info"},
                format!("{goal}; client-observed reported population only; closed UTC minutes, delayed/incomplete coverage: {text}")));
            continue;
        }
        match name {
            "sql-statement" => {
                let max = target["max_ms"].as_f64().unwrap_or_default();
                let p99 = verdict(&target["windows"], true);
                let under_max = WINDOWS
                    .iter()
                    .all(|w| target["windows"][*w]["max_ms"].as_f64().unwrap_or_default() <= max);
                lines.push((
                    format!("slo/{name}"),
                    status(p99.met && under_max),
                    format!("p99 ≤ {} ms, max ≤ {max} ms: {}", target["p99_ms"], p99.message),
                ));
            }
            "transaction" => {
                for (part, label) in [("read", "read"), ("write", "write"), ("writer_hold", "writer-hold")] {
                    let verdict = verdict(&target[part], false);
                    lines.push((
                        format!("slo/transaction/{label}"),
                        status(verdict.met),
                        format!("max ≤ {} ms: {}", target["max_ms"], verdict.message),
                    ));
                }
            }
            "cpu" => {
                let max = target["max_cores"].as_f64().unwrap_or_default();
                let cores = |w: &str| target["windows"][w]["cores"].as_f64().unwrap_or_default();
                lines.push((
                    format!("slo/{name}"),
                    status(WINDOWS.iter().all(|w| cores(w) <= max)),
                    format!(
                        "≤ {max} cores under load, near zero idle: {}",
                        WINDOWS
                            .iter()
                            .map(|w| format!("{w} {:.2} cores", cores(w)))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    ),
                ));
            }
            "database" => lines.push(database_line(target)),
            "writer-hold" => lines.push(writer_hold_line(target)),
            _ => {
                let own = verdict(&target["windows"], true);
                lines.push((
                    format!("slo/{name}"),
                    status(own.met),
                    format!("p99 ≤ {} ms: {}", target["p99_ms"], own.message),
                ));
                let remote = &target["remote_windows"];
                if WINDOWS.iter().any(|w| remote[*w]["count"].as_u64().unwrap_or_default() > 0) {
                    let remote_verdict = verdict(remote, true);
                    lines.push((
                        format!("slo/{name}/remote"),
                        status(remote_verdict.met),
                        format!(
                            "from another machine, p99 ≤ {} ms: {}",
                            target["remote_p99_ms"], remote_verdict.message
                        ),
                    ));
                }
            }
        }
    }
    let mut missed = report["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|path| path["target"].is_string() && share(&path["windows"]["5m"]) > 0.01)
        .collect::<Vec<_>>();
    missed.sort_by(|a, b| share(&b["windows"]["5m"]).total_cmp(&share(&a["windows"]["5m"])));
    for path in missed.into_iter().take(10) {
        lines.push((
            format!(
                "slo/{}/{}",
                path["target"].as_str().unwrap_or_default(),
                path["path"].as_str().unwrap_or_default()
            ),
            "info",
            describe_window("5m", &path["windows"]["5m"]),
        ));
    }
    lines
}

/// `st doctor`'s line for open projection divergences: met while there are no more open than the
/// target allows, which is none. A miss is `info` like every other target's: the fault itself goes
/// to the fleet's fault agent, and while a peer still differs the replication check fails.
pub fn divergence_line(open: u64) -> (String, &'static str, String) {
    let max_open = targets().divergence.max_open;
    (
        "slo/projection-divergence".into(),
        if open <= max_open { "pass" } else { "info" },
        format!(
            "open P1 projection bugs ≤ {max_open}: {open} open; `st subject daemon/NODE` for the member that found one shows it with what a reproduction needs"
        ),
    )
}

/// `st doctor`'s line for the store's size: met when the pages in use and their daily growth are
/// both within target. Growth needs an hour of samples; until then only the size is judged.
fn database_line(target: &Value) -> (String, &'static str, String) {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    let max_live = target["max_live_gb"].as_f64().unwrap_or_default();
    let max_growth = target["max_growth_mb_per_day"].as_f64().unwrap_or_default();
    let size = &target["size"];
    let Some(live) = size["live_bytes"].as_f64() else {
        return ("slo/database".into(), "info", "not measured yet".into());
    };
    let file = size["file_bytes"].as_f64().unwrap_or_default();
    let growth = size["growth_bytes_per_day"].as_f64();
    let met = live / GB <= max_live && growth.is_none_or(|growth| growth / MB <= max_growth);
    let growth = growth.map_or("growth not known yet".to_owned(), |growth| {
        let span = size["growth_span_ms"].as_f64().unwrap_or_default() / 3_600_000.0;
        format!("growth {:+.0} MB a day over {span:.0} h", growth / MB)
    });
    (
        "slo/database".into(),
        if met { "pass" } else { "info" },
        format!(
            "live ≤ {max_live} GB, growth ≤ {max_growth} MB a day: live {:.2} GB of a {:.2} GB file, {growth}",
            live / GB,
            file / GB
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_targets_file_is_valid_and_names_the_mission_targets() {
        let targets = parse(SOURCE).unwrap();
        let read = targets.for_path("GET /v1/client/agents").unwrap();
        assert_eq!((read.p99_ms, read.remote_p99_ms), (100, Some(300)));
        assert_eq!(read.target(true), Duration::from_millis(300));
        for path in [
            "stream agents",
            "stream conversation",
            "GET /v1/client/conversations/{id}/changes",
            "GET /v1/client/work",
            "GET /v1/client/now",
            "GET /v1/client/documents/content",
        ] {
            assert_eq!(targets.for_path(path).unwrap().name, "person-read", "{path}");
        }
        for path in [
            "POST /v1/messages",
            "POST /v1/work/{action}/{*subject}",
            "POST /v1/harness-events",
        ] {
            let write = targets.for_path(path).unwrap();
            assert_eq!((write.name.as_str(), write.p99_ms), ("write-ack", 100), "{path}");
        }
        let fresh = targets.for_path("GET /v1/client/agents (fresh)").unwrap();
        assert_eq!((fresh.name.as_str(), fresh.p99_ms), ("person-read-fresh", 300));
        assert_eq!(fresh.remote_p99_ms, None);
        // A fresh list counts toward its own target only, never the first page's.
        assert_eq!(targets.for_path("GET /v1/client/agents").unwrap().name, "person-read");
        for path in [
            "GET /v1/client/missions",
            "GET /v1/client/missions/{*id}",
            "stream missions",
        ] {
            assert_eq!(targets.for_path(path).unwrap().name, "person-read", "{path}");
        }
        for path in ["publish missions", "publish agents", "publish attention"] {
            let fresh = targets.for_path(path).unwrap();
            assert_eq!((fresh.name.as_str(), fresh.p99_ms), ("projection-fresh", 1500), "{path}");
        }
        let attach = targets.for_path("stream terminal").unwrap();
        assert_eq!(attach.p99_ms, 200);
        assert_eq!((targets.statement.p99_ms, targets.statement.max_ms), (10, 100));
        assert_eq!(targets.transaction.max_ms, 100);
        assert_eq!(targets.cpu.max_cores, 2.0);
        assert_eq!((targets.database.max_live_gb, targets.database.max_growth_mb_per_day), (8.0, 300.0));
        assert!(targets.for_path("GET /v1/health").is_none());
    }

    #[test]
    fn every_route_path_in_the_targets_file_is_a_route_the_daemon_serves() {
        let source = include_str!("api.rs");
        for latency in &targets().latency {
            for path in &latency.paths {
                let Some(route) = path.strip_prefix("GET ").or(path.strip_prefix("POST ")) else {
                    continue;
                };
                // `(fresh)` names the fresh form of a route the daemon serves.
                let route = route.strip_suffix(" (fresh)").unwrap_or(route);
                assert!(
                    source.contains(&format!("\"{route}\"")),
                    "{path} is not routed in api.rs"
                );
            }
        }
    }

    #[test]
    fn the_database_line_judges_size_and_daily_growth() {
        let target = |size: Value| {
            json!({"name": "database", "max_live_gb": 8.0, "max_growth_mb_per_day": 300.0, "size": size})
        };
        assert_eq!(database_line(&target(Value::Null)).1, "info");
        let (name, status, message) = database_line(&target(json!({
            "live_bytes": 6e9, "file_bytes": 14e9, "growth_bytes_per_day": null
        })));
        assert_eq!((name.as_str(), status), ("slo/database", "pass"));
        assert!(message.contains("live 6.00 GB of a 14.00 GB file, growth not known yet"), "{message}");
        let line = database_line(&target(json!({
            "live_bytes": 6e9, "file_bytes": 14e9, "growth_bytes_per_day": 450e6, "growth_span_ms": 86_400_000
        })));
        assert_eq!(line.1, "info");
        assert!(line.2.contains("growth +450 MB a day over 24 h"), "{}", line.2);
        assert_eq!(database_line(&target(json!({"live_bytes": 9e9, "file_bytes": 9e9}))).1, "info");
        let shrinking = database_line(&target(json!({
            "live_bytes": 6e9, "file_bytes": 14e9, "growth_bytes_per_day": -2e9, "growth_span_ms": 86_400_000
        })));
        assert_eq!(shrinking.1, "pass");
    }

    #[test]
    fn the_writer_hold_line_names_the_top_holder_and_a_hold_in_progress() {
        let window = |count: u64, over: u64, max: f64| {
            json!({"count": count, "over_target": over, "over_target_share": over as f64 / count.max(1) as f64,
                "p50_ms": 1.0, "p99_ms": max, "max_ms": max})
        };
        let windows = |over, max| json!({"1m": window(10, over, max), "5m": window(10, over, max), "1h": window(10, over, max)});
        let quiet = json!({"name": "writer-hold", "max_ms": 100, "windows": windows(0, 8.0),
            "read_windows": windows(0, 3.0), "open_reads": [], "in_progress": [], "long_reads": [],
            "holders": [{"holder": "/v1/messages", "count": 10, "total_ms": 40.0, "max_ms": 8.0}]});
        let (name, status, message) = writer_hold_line(&quiet);
        assert_eq!((name.as_str(), status), ("slo/writer-hold", "pass"));
        assert!(message.contains("most held by /v1/messages (10 holds, 40 ms in all, max 8 ms in 1h)"), "{message}");
        let mut busy = quiet.clone();
        busy["windows"] = windows(1, 5750.0);
        busy["holders"] = json!([{"holder": "/v1/internal/replication/export", "count": 3, "total_ms": 8000.0, "max_ms": 5750.0}]);
        busy["in_progress"] = json!([{"holder": "/v1/internal/replication/receive", "held_ms": 1200.0}]);
        busy["read_windows"] = windows(1, 2500.0);
        busy["long_reads"] = json!([{"holder": "/v1/internal/replication/export"}]);
        busy["waiting"] = json!({"writer": 12, "read": 0});
        busy["threads"] = json!({"total": 118});
        let (_, status, message) = writer_hold_line(&busy);
        assert_eq!(status, "info");
        assert!(message.contains("most held by /v1/internal/replication/export"), "{message}");
        assert!(message.contains("held now by /v1/internal/replication/receive for 1200 ms"), "{message}");
        assert!(message.contains("last by /v1/internal/replication/export; 0 open"), "{message}");
        assert!(message.contains("waiting now: 12 for the writer, 0 for a read; 118 threads"), "{message}");
        let mut open = quiet;
        open["open_reads"] = json!([{"read_id": 1}]);
        assert_eq!(writer_hold_line(&open).1, "info");
    }

    #[test]
    fn the_request_latency_report_carries_the_writer_hold_objective() {
        install();
        let rows = store_report(json!({}));
        let row = rows.iter().find(|row| row["name"] == "writer-hold").unwrap();
        assert_eq!(row["max_ms"], targets().transaction.max_ms);
        for field in ["windows", "read_windows", "open_reads", "holders", "long_holds", "long_reads", "in_progress", "waiting"] {
            assert!(row.get(field).is_some(), "{field} missing from {row}");
        }
        if cfg!(target_os = "linux") {
            let threads = &row["threads"];
            assert!(threads["total"].as_u64().unwrap() >= 1, "{threads}");
            let named = threads["by_name"].as_array().unwrap().iter().map(|n| n["count"].as_u64().unwrap()).sum::<u64>();
            assert!(named <= threads["total"].as_u64().unwrap());
        }
        assert!(row["windows"]["1h"].is_object());
    }

    #[test]
    fn a_broken_targets_file_is_refused_not_defaulted() {
        let without_cpu = SOURCE.split("[cpu]").next().unwrap();
        assert!(parse(without_cpu).is_err());
        assert!(parse(&SOURCE.replace("max_cores = 2.0", "max_cores = -1.0")).is_err());
        assert!(parse(SOURCE.split("[database]").next().unwrap()).is_err());
        assert!(parse(&SOURCE.replace("max_live_gb = 8.0", "max_live_gb = 0.0")).is_err());
        assert!(parse(&SOURCE.replace("p99_ms = 10\n", "p99_ms = 10\ntypo = 1\n")).is_err());
        let twice = format!(
            "{SOURCE}\n[[latency]]\nname = \"again\"\nabout = \"\"\np99_ms = 1\npaths = [\"GET /v1/client/now\"]\n"
        );
        assert!(parse(&twice).unwrap_err().to_string().contains("two targets"));
    }

    #[test]
    fn a_window_misses_a_p99_target_past_one_percent_and_a_max_target_at_once() {
        let window = |count: u64, over: u64| {
            json!({"count": count, "over_target": over,
                "over_target_share": over as f64 / count.max(1) as f64,
                "p50_ms": 1.0, "p99_ms": 2.0, "max_ms": 3.0})
        };
        let windows = |over| json!({"1m": window(100, over), "5m": window(100, over), "1h": window(1000, over)});
        assert!(verdict(&windows(1), true).met);
        assert!(!verdict(&windows(2), true).met);
        assert!(!verdict(&windows(1), false).met);
        assert!(verdict(&windows(0), false).met);
        assert!(verdict(&windows(0), true).message.contains("1m p99 2 ms, max 3 ms, 0.0% over of 100"));
    }
    #[test]
    fn client_share_doctor_uses_declared_minimum_and_empty_population_is_info() {
        let window=json!({"count":10000,"foreground_ms":10000,"live_ms":9500,"over_target":500,"over_target_share":0.05});
        let windows=json!({"1m":window,"5m":window,"1h":window});
        let mut target=json!({"name":"ios-live-share","population":"client-observed-foreground-ms","min_percent":90,"windows":windows});
        let report=|target:Value|json!({"targets":[target],"paths":[]});
        assert_eq!(doctor_lines(&report(target.clone()))[0].1,"pass");
        target["min_percent"]=json!(99);
        assert_eq!(doctor_lines(&report(target.clone()))[0].1,"info");
        target["windows"]=json!({"1m":{"count":0},"5m":{"count":0},"1h":{"count":0}});
        assert_eq!(doctor_lines(&report(target))[0].1,"info");
    }

}
