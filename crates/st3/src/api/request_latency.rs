//! Completed response-envelope timings. Action buckets are subsets of route totals.
//!
//! Each path also keeps rolling 1-minute, 5-minute and 1-hour windows, and each target in
//! `slo/targets.toml` keeps the same windows over all its paths.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use axum::http::Method;
use serde_json::{Value, json};

const ROUTES: usize = 256;
const RECENT: usize = 512;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum WorkAction {
    Claim,
    Renew,
    Progress,
    Complete,
    Fail,
    Release,
    Extend,
}

impl WorkAction {
    fn classify(method: &Method, route: &str, path: &str) -> Option<Self> {
        if method != Method::POST {
            return None;
        }
        if route == "/v1/work/extend/{*subject}" {
            return Some(Self::Extend);
        }
        if route != "/v1/work/{action}/{*subject}" {
            return None;
        }
        let (action, _) = path.strip_prefix("/v1/work/")?.split_once('/')?;
        // Axum decodes action path parameters. Bound decoding to the longest known
        // name (eight bytes), even when every byte is percent encoded.
        if action.len() > 24 {
            return None;
        }
        match urlencoding::decode(action).ok()?.as_ref() {
            "claim" => Some(Self::Claim),
            "renew" => Some(Self::Renew),
            "progress" => Some(Self::Progress),
            "complete" => Some(Self::Complete),
            "fail" => Some(Self::Fail),
            "release" => Some(Self::Release),
            "extend" => Some(Self::Extend),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Renew => "renew",
            Self::Progress => "progress",
            Self::Complete => "complete",
            Self::Fail => "fail",
            Self::Release => "release",
            Self::Extend => "extend",
        }
    }

    fn route(self) -> &'static str {
        match self {
            Self::Claim => "/v1/work/claim/{*subject}",
            Self::Renew => "/v1/work/renew/{*subject}",
            Self::Progress => "/v1/work/progress/{*subject}",
            Self::Complete => "/v1/work/complete/{*subject}",
            Self::Fail => "/v1/work/fail/{*subject}",
            Self::Release => "/v1/work/release/{*subject}",
            Self::Extend => "/v1/work/extend/{*subject}",
        }
    }
}

const AGENTS: &str = "/v1/client/agents";

/// What an agents roster read asks for. A fresh first page waits, by design, for the refresher
/// to publish a roster at or after its own cut; the others answer from a publication at once.
/// Each keeps its own sample so a latency target can count the designed wait separately
/// without dropping it from the route total.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum AgentsRead {
    FirstPage,
    Fresh,
    Continuation,
}

impl AgentsRead {
    fn classify(method: &Method, route: &str, query: Option<&str>) -> Option<Self> {
        if method != Method::GET || route != AGENTS {
            return None;
        }
        let mut fresh = false;
        for (name, value) in query.unwrap_or_default().split('&').filter_map(|pair| pair.split_once('=')) {
            match name {
                // A continuation answers from its first page's publication and never waits.
                "cursor" => return Some(Self::Continuation),
                "fresh" => fresh = value == "true",
                _ => {}
            }
        }
        Some(if fresh { Self::Fresh } else { Self::FirstPage })
    }

    fn name(self) -> &'static str {
        match self {
            Self::FirstPage => "first-page",
            Self::Fresh => "fresh",
            Self::Continuation => "continuation",
        }
    }
}

/// Whether the answer is a designed wait, so its time says nothing about a latency target: a
/// fresh agents read waits for the next roster publication.
pub(super) fn waits_by_design(method: &Method, route: &str, query: Option<&str>) -> bool {
    AgentsRead::classify(method, route, query) == Some(AgentsRead::Fresh)
}

#[derive(Default)]
struct Sample {
    count: u64,
    recent_ms: VecDeque<u64>,
}

impl Sample {
    fn record(&mut self, elapsed: Duration) {
        self.count = self.count.saturating_add(1);
        if self.recent_ms.len() == RECENT {
            self.recent_ms.pop_front();
        }
        self.recent_ms
            .push_back(elapsed.as_millis().min(u64::MAX as u128) as u64);
    }

    fn snapshot(&self, route: &str, scope: &str) -> Value {
        let mut sorted = self.recent_ms.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        let percentile = |percent: usize| {
            sorted
                .get(((sorted.len() * percent).div_ceil(100)).saturating_sub(1))
                .copied()
                .unwrap_or_default()
        };
        json!({
            "scope": scope,
            "route": route,
            "count": self.count,
            "recent_count": sorted.len(),
            "p50_ms": percentile(50),
            "p99_ms": percentile(99),
            "max_ms": sorted.last().copied().unwrap_or_default(),
            "duration_scope": "response-envelope",
            "population": "completed-responses",
        })
    }
}

#[derive(Default)]
pub(super) struct Meter {
    routes: BTreeMap<String, Sample>,
    // Independent of the general route cap: all seven known actions retain a
    // complete denominator even if the route table has already filled.
    work_actions: BTreeMap<WorkAction, Sample>,
    agents_reads: BTreeMap<AgentsRead, Sample>,
    /// Windows by path: `GET /route`, `stream COLLECTION`, or a long poll's route.
    paths: BTreeMap<String, smallclaims::windows::Series>,
    /// Windows by target position in `slo/targets.toml`: served here, then from another machine.
    targets: Vec<[smallclaims::windows::Series; 2]>,
    cpu: smallclaims::windows::Cpu,
}

/// How a completed sample is counted in the windows. Build it with [`Timed::resolve`] before
/// taking the meter's lock: the key and the target lookup allocate and scan.
pub(super) struct Timed {
    key: String,
    target: Option<usize>,
    remote: bool,
}

impl Timed {
    /// `path` is `GET /v1/client/now`, or `stream agents` for a socket subscription's first
    /// snapshot. `remote` says it read from another machine that owns it, through this daemon.
    /// A `long_poll` asked to wait for a change, so its time is mostly the wait it asked for:
    /// it is kept under its own key and counts toward no target.
    pub(super) fn resolve(path: &str, remote: bool, long_poll: bool) -> Self {
        if long_poll {
            return Self {
                key: format!("{path} (long poll)"),
                target: None,
                remote,
            };
        }
        Self {
            key: path.to_owned(),
            target: crate::slo::targets().index_of(path),
            remote,
        }
    }
}

impl Meter {
    pub(super) fn record(
        &mut self,
        method: &Method,
        route: &str,
        path: &str,
        query: Option<&str>,
        elapsed: Duration,
    ) {
        if self.routes.len() < ROUTES || self.routes.contains_key(route) {
            self.routes
                .entry(route.to_owned())
                .or_default()
                .record(elapsed);
        }
        if let Some(action) = WorkAction::classify(method, route, path) {
            self.work_actions.entry(action).or_default().record(elapsed);
        }
        if let Some(read) = AgentsRead::classify(method, route, query) {
            self.agents_reads.entry(read).or_default().record(elapsed);
        }
    }

    /// Count one sample in its path's windows and its target's.
    pub(super) fn time(&mut self, now: Instant, timed: Timed, elapsed: Duration) {
        self.cpu.note(now);
        let targets = crate::slo::targets();
        let Timed {
            key,
            target,
            remote,
        } = timed;
        let over = target.is_some_and(|target| elapsed > targets.latency[target].target(remote));
        if let Some(series) = self.paths.get_mut(key.as_str()) {
            series.record(now, elapsed, over);
        } else if self.paths.len() < ROUTES {
            self.paths
                .entry(key)
                .or_default()
                .record(now, elapsed, over);
        }
        if let Some(target) = target {
            if self.targets.is_empty() {
                self.targets
                    .resize_with(targets.latency.len(), Default::default);
            }
            self.targets[target][usize::from(remote)].record(now, elapsed, over);
        }
    }

    /// Every target's windows, then every path's that has a sample in its last hour.
    pub(super) fn windows(&mut self, now: Instant) -> Value {
        let targets = crate::slo::targets();
        let empty = smallclaims::windows::Series::default();
        let mut rows = targets
            .latency
            .iter()
            .enumerate()
            .map(|(index, latency)| {
                let windows = |remote: bool| {
                    self.targets
                        .get(index)
                        .map_or(&empty, |pair| &pair[usize::from(remote)])
                        .snapshot(now)
                };
                let mut row = json!({
                    "name": latency.name,
                    "about": latency.about,
                    "p99_ms": latency.p99_ms,
                    "windows": windows(false),
                });
                if let Some(remote) = latency.remote_p99_ms {
                    row["remote_p99_ms"] = json!(remote);
                    row["remote_windows"] = windows(true);
                }
                row
            })
            .collect::<Vec<_>>();
        rows.extend(crate::slo::store_report(self.cpu.snapshot(now)));
        let paths = self
            .paths
            .iter()
            .filter(|(_, series)| !series.is_empty(now))
            .map(|(path, series)| {
                json!({
                    "path": path,
                    "target": targets.for_path(path).map(|target| target.name.as_str()),
                    "windows": series.snapshot(now),
                })
            })
            .collect::<Vec<_>>();
        json!({"targets": rows, "paths": paths})
    }

    pub(super) fn snapshot(&self) -> Vec<Value> {
        self.routes
            .iter()
            .map(|(route, sample)| sample.snapshot(route, "route"))
            .chain(self.work_actions.iter().map(|(action, sample)| {
                let mut row = sample.snapshot(action.route(), "work-action");
                row["method"] = json!("POST");
                row["action"] = json!(action.name());
                row
            }))
            .chain(self.agents_reads.iter().map(|(read, sample)| {
                let mut row = sample.snapshot(AGENTS, "agents-read");
                row["method"] = json!("GET");
                row["read"] = json!(read.name());
                row
            }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: &str = "/v1/work/{action}/{*subject}";

    fn row(meter: &Meter, scope: &str, route: &str) -> Value {
        meter
            .snapshot()
            .into_iter()
            .find(|row| row["scope"] == scope && row["route"] == route)
            .unwrap()
    }

    #[test]
    fn renew_and_claim_keep_independent_full_duration_samples_and_route_total() {
        let mut meter = Meter::default();
        for (path, duration) in [
            ("/v1/work/renew/work/private-a", 11),
            ("/v1/work/claim/work/private-b", 900),
            ("/v1/work/renew/work/private-c", 27),
        ] {
            meter.record(&Method::POST, WORK, path, None, Duration::from_millis(duration));
        }
        let renew = row(&meter, "work-action", "/v1/work/renew/{*subject}");
        assert_eq!(renew["count"], 2);
        assert_eq!(renew["recent_count"], 2);
        assert_eq!(renew["p99_ms"], 27);
        assert_eq!(renew["method"], "POST");
        assert_eq!(renew["duration_scope"], "response-envelope");
        let claim = row(&meter, "work-action", "/v1/work/claim/{*subject}");
        assert_eq!(claim["count"], 1);
        assert_eq!(claim["p99_ms"], 900);
        let aggregate = row(&meter, "route", WORK);
        assert_eq!(aggregate["count"], 3);
        assert_eq!(aggregate["p99_ms"], 900);
        assert!(
            !serde_json::to_string(&meter.snapshot())
                .unwrap()
                .contains("private")
        );
    }

    #[test]
    fn only_known_post_actions_allocate_static_buckets_including_encoded_parameters() {
        let mut meter = Meter::default();
        for action in [
            "claim", "renew", "progress", "complete", "fail", "release", "extend",
        ] {
            meter.record(
                &Method::POST,
                WORK,
                &format!("/v1/work/{action}/secret"),
                None,
                Duration::ZERO,
            );
        }
        meter.record(
            &Method::POST,
            "/v1/work/extend/{*subject}",
            "/v1/work/extend/secret",
            None,
            Duration::ZERO,
        );
        meter.record(
            &Method::POST,
            WORK,
            "/v1/work/%72%65%6e%65%77/secret",
            None,
            Duration::ZERO,
        );
        for method in [Method::GET, Method::HEAD, Method::PUT] {
            meter.record(&method, WORK, "/v1/work/renew/secret", None, Duration::ZERO);
        }
        for i in 0..1_000 {
            meter.record(
                &Method::POST,
                WORK,
                &format!("/v1/work/unknown-{i}/secret"),
                None,
                Duration::ZERO,
            );
        }
        meter.record(
            &Method::POST,
            "/unmatched",
            "/v1/work/renew/secret",
            None,
            Duration::ZERO,
        );
        assert_eq!(meter.work_actions.len(), 7);
        assert_eq!(
            row(&meter, "work-action", "/v1/work/renew/{*subject}")["count"],
            2
        );
        assert_eq!(
            row(&meter, "work-action", "/v1/work/extend/{*subject}")["count"],
            2
        );
        let json = serde_json::to_string(&meter.snapshot()).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("unknown-"));
        assert!(!json.contains("%72"));
    }

    #[test]
    fn paths_count_toward_their_target_with_the_remote_target_for_remote_reads() {
        let mut meter = Meter::default();
        let now = Instant::now();
        let time = |meter: &mut Meter, path, remote, long_poll, ms| {
            meter.time(
                now,
                Timed::resolve(path, remote, long_poll),
                Duration::from_millis(ms),
            );
        };
        time(&mut meter, "GET /v1/client/now", false, false, 50);
        time(&mut meter, "GET /v1/client/now", false, false, 150);
        time(&mut meter, "stream agents", true, false, 250);
        time(&mut meter, "stream conversation", true, false, 350);
        time(&mut meter, "GET /v1/client/conversations/{id}/changes", false, true, 30_000);
        time(&mut meter, "GET /v1/health", false, false, 1);
        let report = meter.windows(now);
        let target = |name: &str| {
            report["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == name)
                .unwrap()
                .clone()
        };
        let read = target("person-read");
        assert_eq!(read["windows"]["1m"]["count"], 2);
        assert_eq!(read["windows"]["1m"]["over_target"], 1);
        assert_eq!(read["remote_windows"]["5m"]["count"], 2);
        assert_eq!(read["remote_windows"]["5m"]["over_target"], 1);
        assert_eq!(target("write-ack")["windows"]["1h"]["count"], 0);
        for name in ["sql-statement", "transaction", "cpu"] {
            target(name);
        }
        let paths = report["paths"].as_array().unwrap();
        let path = |name: &str| paths.iter().find(|row| row["path"] == name).unwrap();
        assert_eq!(path("GET /v1/client/now")["target"], "person-read");
        assert_eq!(path("GET /v1/health")["target"], Value::Null);
        let poll = path("GET /v1/client/conversations/{id}/changes (long poll)");
        assert_eq!(poll["target"], Value::Null);
        assert_eq!(poll["windows"]["1m"]["over_target"], 0);
        assert_eq!(paths.len(), 5);
    }

    #[test]
    fn a_fresh_agents_read_waits_by_design_and_counts_toward_no_target() {
        assert!(waits_by_design(&Method::GET, AGENTS, Some("limit=5&fresh=true")));
        assert!(!waits_by_design(&Method::GET, AGENTS, Some("fresh=false")));
        assert!(!waits_by_design(&Method::GET, AGENTS, None));
        assert!(!waits_by_design(&Method::GET, AGENTS, Some("fresh=true&cursor=page")));
        assert!(!waits_by_design(&Method::POST, AGENTS, Some("fresh=true")));
        assert!(!waits_by_design(&Method::GET, "/v1/client/work", Some("fresh=true")));
        let fresh = Timed::resolve("GET /v1/client/agents", false, true);
        assert!(fresh.target.is_none());
        assert_eq!(fresh.key, "GET /v1/client/agents (long poll)");
    }

    #[test]
    fn agents_reads_split_the_designed_fresh_wait_from_the_route_total() {
        let mut meter = Meter::default();
        for (query, duration) in [
            (None, 4),
            (Some("limit=50"), 6),
            (Some("limit=50&fresh=true"), 700),
            (Some("fresh=false"), 5),
            (Some("fresh=true&cursor=page%2Fsecret"), 3),
        ] {
            meter.record(&Method::GET, AGENTS, AGENTS, query, Duration::from_millis(duration));
        }
        meter.record(&Method::HEAD, AGENTS, AGENTS, Some("fresh=true"), Duration::from_millis(9));
        meter.record(&Method::GET, "/v1/client/work", "/v1/client/work", Some("fresh=true"), Duration::ZERO);
        let read = |name: &str| meter.snapshot().into_iter()
            .find(|row| row["scope"] == "agents-read" && row["read"] == name).unwrap();
        assert_eq!(read("first-page")["count"], 3);
        assert_eq!(read("first-page")["p99_ms"], 6);
        assert_eq!(read("fresh")["count"], 1);
        assert_eq!(read("fresh")["p99_ms"], 700);
        assert_eq!(read("continuation")["count"], 1);
        assert_eq!(read("fresh")["route"], AGENTS);
        // The route total keeps every read, the designed wait included.
        let route = row(&meter, "route", AGENTS);
        assert_eq!(route["count"], 6);
        assert_eq!(route["max_ms"], 700);
        assert!(!serde_json::to_string(&meter.snapshot()).unwrap().contains("secret"));
    }

    #[test]
    fn action_denominator_survives_route_capacity_and_retains_last_512_of_all_completions() {
        let mut meter = Meter::default();
        for i in 0..ROUTES {
            meter.record(
                &Method::GET,
                &format!("/known-route-{i}"),
                "",
                None,
                Duration::ZERO,
            );
        }
        for ms in 1..=600 {
            meter.record(
                &Method::POST,
                WORK,
                "/v1/work/renew/secret",
                None,
                Duration::from_millis(ms),
            );
        }
        assert_eq!(meter.routes.len(), ROUTES);
        assert!(!meter.routes.contains_key(WORK));
        let renew = row(&meter, "work-action", "/v1/work/renew/{*subject}");
        assert_eq!(renew["count"], 600);
        assert_eq!(renew["recent_count"], 512);
        assert_eq!(renew["p50_ms"], 344);
        assert_eq!(renew["p99_ms"], 595);
        assert_eq!(renew["max_ms"], 600);
        assert_eq!(meter.snapshot().len(), ROUTES + 1);
    }
}
