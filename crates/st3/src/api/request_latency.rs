//! Completed response-envelope timings. Action buckets are subsets of route totals.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

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
}

impl Meter {
    pub(super) fn record(&mut self, method: &Method, route: &str, path: &str, elapsed: Duration) {
        if self.routes.len() < ROUTES || self.routes.contains_key(route) {
            self.routes
                .entry(route.to_owned())
                .or_default()
                .record(elapsed);
        }
        if let Some(action) = WorkAction::classify(method, route, path) {
            self.work_actions.entry(action).or_default().record(elapsed);
        }
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
            meter.record(&Method::POST, WORK, path, Duration::from_millis(duration));
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
                Duration::ZERO,
            );
        }
        meter.record(
            &Method::POST,
            "/v1/work/extend/{*subject}",
            "/v1/work/extend/secret",
            Duration::ZERO,
        );
        meter.record(
            &Method::POST,
            WORK,
            "/v1/work/%72%65%6e%65%77/secret",
            Duration::ZERO,
        );
        for method in [Method::GET, Method::HEAD, Method::PUT] {
            meter.record(&method, WORK, "/v1/work/renew/secret", Duration::ZERO);
        }
        for i in 0..1_000 {
            meter.record(
                &Method::POST,
                WORK,
                &format!("/v1/work/unknown-{i}/secret"),
                Duration::ZERO,
            );
        }
        meter.record(
            &Method::POST,
            "/unmatched",
            "/v1/work/renew/secret",
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
    fn action_denominator_survives_route_capacity_and_retains_last_512_of_all_completions() {
        let mut meter = Meter::default();
        for i in 0..ROUTES {
            meter.record(
                &Method::GET,
                &format!("/known-route-{i}"),
                "",
                Duration::ZERO,
            );
        }
        for ms in 1..=600 {
            meter.record(
                &Method::POST,
                WORK,
                "/v1/work/renew/secret",
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
