//! The narrow main-push Performance failure field of the existing repository observer.
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::github_http::GithubAuth;

pub(crate) const PERFORMANCE_FAILURES_FIELD: &str = "main_performance_failures";
const WORKFLOW_PATH: &str = ".github/workflows/perf.yml";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct PerformanceFailure {
    pub repository: String,
    pub run_id: u64,
    pub run_attempt: u64,
    pub head_sha: String,
    pub workflow: String,
    pub workflow_path: String,
    pub event: String,
    pub head_branch: String,
    pub status: String,
    pub conclusion: String,
    pub url: String,
}

impl PerformanceFailure {
    fn valid(&self) -> bool {
        let Some((owner, repo)) = self.repository.split_once('/') else {
            return false;
        };
        let component = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        component(owner)
            && component(repo)
            && self.run_id > 0
            && self.run_attempt > 0
            && self.head_sha.len() == 40
            && self.head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            && self.workflow == "Performance"
            && self.workflow_path == WORKFLOW_PATH
            && self.event == "push"
            && self.head_branch == "main"
            && self.status == "completed"
            && self.conclusion == "failure"
            && self.url
                == format!(
                    "https://github.com/{}/actions/runs/{}",
                    self.repository, self.run_id
                )
    }

    pub(crate) fn from_facts(facts: &Value) -> Option<Self> {
        let failure: Self = serde_json::from_value(facts.clone()).ok()?;
        failure.valid().then_some(failure)
    }

    fn from_run(repository: &str, run: &Value) -> Option<Self> {
        if run
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.eq_ignore_ascii_case(repository))
        {
            return None;
        }
        let text = |field: &str| run.get(field).and_then(Value::as_str).map(str::to_owned);
        let run_id = run.get("id")?.as_u64()?;
        let path = text("path")?;
        // GitHub may suffix the workflow file path with its ref.
        let workflow_path = path
            .split_once('@')
            .map_or(path.as_str(), |(path, _)| path)
            .to_owned();
        let failure = Self {
            repository: repository.to_ascii_lowercase(),
            run_id,
            run_attempt: run.get("run_attempt")?.as_u64()?,
            head_sha: text("head_sha")?.to_ascii_lowercase(),
            workflow: text("name")?,
            workflow_path,
            event: text("event")?,
            head_branch: text("head_branch")?,
            status: text("status")?,
            conclusion: text("conclusion")?,
            url: format!(
                "https://github.com/{}/actions/runs/{run_id}",
                repository.to_ascii_lowercase()
            ),
        };
        failure.valid().then_some(failure)
    }
}

pub(super) async fn main_performance_failures(
    client: &reqwest::Client,
    base: &str,
    repository: &str,
    auth: &GithubAuth,
    cache_for: Duration,
) -> Result<Vec<PerformanceFailure>> {
    // These are metadata reads only, through the repository's existing conditional HTTP cache.
    // Ten pages is GitHub's 1,000-result search bound. An overflow fails rather than truncates.
    let listing = super::github_listing_at_field(
        client,
        format!("{base}/actions/workflows/perf.yml/runs?branch=main&event=push&status=failure&per_page=100"),
        auth, cache_for, 10, false, Some("workflow_runs"),
    ).await?;
    let mut failures = listing
        .values
        .iter()
        .filter_map(|run| PerformanceFailure::from_run(repository, run))
        .collect::<Vec<_>>();
    failures.sort_by(|left, right| {
        (left.run_id, left.run_attempt, &left.head_sha).cmp(&(
            right.run_id,
            right.run_attempt,
            &right.head_sha,
        ))
    });
    failures.dedup();
    Ok(failures)
}
