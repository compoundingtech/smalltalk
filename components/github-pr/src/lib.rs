use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

wit_bindgen::generate!({
    path: "../../wit/github-pr",
    world: "github-pr-provider",
    with: {
        "compoundingtech:st2-github-pr/github-pr@0.1.0": generate,
    },
});

use compoundingtech::st2_github_pr::github_pr;
use exports::st2::resource_provider::provider_api;

const SNAPSHOT_SCHEMA: &str = "dev.schickling.github-pr.snapshot.v1";
const TOPICS: [&str; 4] = [
    "ci.failure",
    "mergeability.conflict",
    "review.requested",
    "terminal",
];
const SELECTOR_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "topics": {
      "type": "array",
      "items": { "type": "string" },
      "uniqueItems": true
    }
  },
  "additionalProperties": false
}"#;

struct Component;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    #[serde(default)]
    topics: Vec<String>,
}

#[derive(Clone)]
struct PullRequestRef {
    owner: String,
    repo: String,
    number: u64,
}

impl PullRequestRef {
    fn parse_uri(uri: &str) -> Option<Self> {
        Self::parse_subject(uri.strip_prefix("github-pr://github.com/")?, "pull")
    }

    fn parse_html_url(url: &str) -> Option<Self> {
        Self::parse_subject(url.strip_prefix("https://github.com/")?, "pull")
    }

    fn parse_subject(subject: &str, collection: &str) -> Option<Self> {
        let mut parts = subject.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if parts.next()? != collection {
            return None;
        }
        let number = parts.next()?;
        if parts.next().is_some()
            || !valid_component(owner, 39)
            || !valid_component(repo, 100)
            || number.is_empty()
            || number.len() > 10
            || number.starts_with('0')
            || !number.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: number.parse().ok()?,
        })
    }
}

#[derive(Deserialize)]
struct GraphqlData {
    repository: Option<RepositoryResponse>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryResponse {
    pull_request: Option<PullRequestResponse>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestResponse {
    url: String,
    title: String,
    body: String,
    state: String,
    is_draft: bool,
    merged: bool,
    merged_at: Option<String>,
    closed_at: Option<String>,
    mergeable: String,
    author: Option<ActorResponse>,
    head_ref_oid: String,
    head_ref_name: String,
    base_ref_name: String,
    review_decision: Option<String>,
    review_requests: ReviewRequestsResponse,
    commits: CommitsResponse,
}

#[derive(Deserialize)]
struct ActorResponse {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestsResponse {
    total_count: u64,
    nodes: Vec<ReviewRequestResponse>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestResponse {
    requested_reviewer: Option<RequestedReviewerResponse>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum RequestedReviewerResponse {
    User { login: String },
    Team { slug: String },
    Bot { login: String },
    Mannequin { login: String },
}

#[derive(Deserialize)]
struct CommitsResponse {
    nodes: Vec<CommitNodeResponse>,
}

#[derive(Deserialize)]
struct CommitNodeResponse {
    commit: CommitResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitResponse {
    status_check_rollup: Option<CheckRollupResponse>,
}

#[derive(Deserialize)]
struct CheckRollupResponse {
    state: String,
    contexts: CheckContextsResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckContextsResponse {
    total_count: u64,
    nodes: Vec<CheckContextResponse>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum CheckContextResponse {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
        description: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot<'a> {
    schema: &'static str,
    uri: &'a str,
    observed_at: &'a str,
    repository: RepositorySnapshot<'a>,
    number: u64,
    pull_request: PullRequestSnapshot,
    ci: CiSnapshot,
    facets: Facets,
}

#[derive(Serialize)]
struct RepositorySnapshot<'a> {
    owner: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestSnapshot {
    api_url: String,
    html_url: String,
    title: String,
    body: String,
    state: String,
    author: Option<String>,
    draft: bool,
    merged: bool,
    merged_at: Option<String>,
    closed_at: Option<String>,
    mergeable: Option<bool>,
    mergeable_state: String,
    head: HeadSnapshot,
    base: BaseSnapshot,
    review_decision: Option<String>,
    requested_reviewers: Vec<String>,
    requested_teams: Vec<String>,
    review_request_total_count: u64,
    review_requests_truncated: bool,
}

#[derive(Serialize)]
struct HeadSnapshot {
    sha: String,
    r#ref: String,
}

#[derive(Serialize)]
struct BaseSnapshot {
    r#ref: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CiSnapshot {
    state: String,
    total_count: u64,
    truncated: bool,
    check_runs: Vec<CheckRunSnapshot>,
    statuses: Vec<StatusSnapshot>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckRunSnapshot {
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusSnapshot {
    context: String,
    state: String,
    target_url: Option<String>,
    description: Option<String>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Facets {
    review_requested: bool,
    ci_failure: bool,
    merge_conflict: bool,
    terminal: bool,
}

impl provider_api::Guest for Component {
    fn describe() -> Result<provider_api::ProviderDescriptor, provider_api::DescriptorError> {
        Ok(provider_api::ProviderDescriptor {
            capabilities: vec![provider_api::SchedulingCapability::Demand],
            selector_schema_json: SELECTOR_SCHEMA.into(),
            default_selector_json: "{}".into(),
            topics: TOPICS.iter().map(|topic| (*topic).to_owned()).collect(),
            snapshot_media_type: "application/json".into(),
            snapshot_schema_id: SNAPSHOT_SCHEMA.into(),
        })
    }

    fn observe(request: provider_api::ObserveRequest) -> provider_api::ObservationResult {
        observe(request)
            .unwrap_or_else(|diagnostic| provider_api::ObservationResult::Failed(Some(diagnostic)))
    }
}

fn observe(
    request: provider_api::ObserveRequest,
) -> Result<provider_api::ObservationResult, String> {
    let selector: Selector = serde_json::from_str(&request.selector_json)
        .map_err(|_| "invalid GitHub pull request selector".to_owned())?;
    let reference = PullRequestRef::parse_uri(&request.uri)
        .ok_or_else(|| "invalid canonical GitHub pull request URI".to_owned())?;
    let observation = github_pr::get(&github_pr::PullRequestRequest {
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
    })
    .map_err(map_source_error)?;

    let current = build_snapshot(&request.uri, &reference, &observation.current)?;
    let previous = observation
        .previous
        .as_ref()
        .map(|source| build_snapshot(&request.uri, &reference, source))
        .transpose()?;
    if previous
        .as_ref()
        .is_some_and(|previous| same_semantics(previous, &current))
    {
        let prior_digest = request
            .prior_digest
            .as_deref()
            .ok_or_else(|| "GitHub prior source lacked a snapshot digest".to_owned())?;
        github_pr::bind_snapshot(prior_digest).map_err(map_source_error)?;
        return Ok(provider_api::ObservationResult::Unchanged);
    }
    let bytes = serde_json::to_vec(&current)
        .map_err(|_| "GitHub pull request snapshot normalization failed".to_owned())?;
    let digest = Sha256::digest(&bytes);
    github_pr::bind_snapshot(digest.as_slice()).map_err(map_source_error)?;
    if request.prior_digest.as_deref() == Some(digest.as_slice()) {
        return Ok(provider_api::ObservationResult::Unchanged);
    }
    let publication_topics = topics(previous.as_ref(), &current);
    let facts = facts(previous.as_ref(), &current, reference.number);
    let _ = (request.demand_watermark, selector.topics);
    Ok(provider_api::ObservationResult::Published(
        provider_api::Publication {
            schema_id: SNAPSHOT_SCHEMA.into(),
            media_type: "application/json".into(),
            bytes,
            topics: publication_topics,
            facts: Some(facts),
        },
    ))
}

fn build_snapshot(
    uri: &str,
    reference: &PullRequestRef,
    source: &github_pr::SourceSnapshot,
) -> Result<Value, String> {
    let response: GraphqlData = serde_json::from_slice(&source.graphql_data)
        .map_err(|_| "GitHub pull request response was invalid".to_owned())?;
    let pull = response
        .repository
        .and_then(|repository| repository.pull_request)
        .ok_or_else(|| "GitHub pull request was missing".to_owned())?;
    if !valid_head_sha(&pull.head_ref_oid) {
        return Err("GitHub pull request head SHA was invalid".into());
    }
    let resolved = PullRequestRef::parse_html_url(&pull.url)
        .filter(|resolved| resolved.number == reference.number)
        .ok_or_else(|| "GitHub pull request URL was invalid".to_owned())?;
    let api_url = format!(
        "https://api.github.com/repos/{}/{}/pulls/{}",
        resolved.owner, resolved.repo, resolved.number
    );

    let review_request_total_count = pull.review_requests.total_count;
    let connection_count = pull.review_requests.nodes.len();
    if connection_count > 100 || review_request_total_count < connection_count as u64 {
        return Err("GitHub pull request bounded connection was invalid".into());
    }
    let mut requested_reviewers = Vec::new();
    let mut requested_teams = Vec::new();
    for request in pull.review_requests.nodes {
        let Some(reviewer) = request.requested_reviewer else {
            continue;
        };
        match reviewer {
            RequestedReviewerResponse::User { login }
            | RequestedReviewerResponse::Bot { login }
            | RequestedReviewerResponse::Mannequin { login } => requested_reviewers.push(login),
            RequestedReviewerResponse::Team { slug } => requested_teams.push(slug),
        }
    }
    requested_reviewers.sort();
    requested_teams.sort();
    let review_request_observed_count = requested_reviewers.len() + requested_teams.len();

    let mut commits = pull.commits.nodes.into_iter();
    let commit = commits
        .next()
        .ok_or_else(|| "GitHub pull request commit was missing".to_owned())?;
    if commits.next().is_some() {
        return Err("GitHub pull request bounded connection was invalid".into());
    }
    let (combined_state, total_count, truncated, mut check_runs, mut statuses) =
        if let Some(rollup) = commit.commit.status_check_rollup {
            let observed_count = rollup.contexts.nodes.len();
            if observed_count > 100 || rollup.contexts.total_count < observed_count as u64 {
                return Err("GitHub pull request bounded connection was invalid".into());
            }
            let mut check_runs = Vec::new();
            let mut statuses = Vec::new();
            for context in rollup.contexts.nodes {
                match context {
                    CheckContextResponse::CheckRun {
                        name,
                        status,
                        conclusion,
                        details_url,
                    } => check_runs.push(CheckRunSnapshot {
                        name,
                        status: normalized_enum(
                            status,
                            &[
                                "completed",
                                "in_progress",
                                "pending",
                                "queued",
                                "requested",
                                "waiting",
                            ],
                        )?,
                        conclusion: conclusion
                            .map(|value| {
                                normalized_enum(
                                    value,
                                    &[
                                        "action_required",
                                        "cancelled",
                                        "failure",
                                        "neutral",
                                        "skipped",
                                        "stale",
                                        "startup_failure",
                                        "success",
                                        "timed_out",
                                    ],
                                )
                            })
                            .transpose()?,
                        details_url,
                    }),
                    CheckContextResponse::StatusContext {
                        context,
                        state,
                        target_url,
                        description,
                    } => statuses.push(StatusSnapshot {
                        context,
                        state: normalized_enum(
                            state,
                            &["error", "expected", "failure", "pending", "success"],
                        )?,
                        target_url,
                        description,
                    }),
                }
            }
            (
                normalized_enum(
                    rollup.state,
                    &["error", "expected", "failure", "pending", "success"],
                )?,
                rollup.contexts.total_count,
                rollup.contexts.total_count > observed_count as u64,
                check_runs,
                statuses,
            )
        } else {
            ("pending".to_owned(), 0, false, Vec::new(), Vec::new())
        };
    check_runs.sort_by(|left, right| left.name.cmp(&right.name));
    statuses.sort_by(|left, right| left.context.cmp(&right.context));

    let state = normalized_enum(pull.state, &["open", "closed", "merged"])?;
    let review_decision = pull
        .review_decision
        .map(|value| normalized_enum(value, &["approved", "changes_requested", "review_required"]))
        .transpose()?
        .map(|value| value.to_ascii_uppercase());
    let (mergeable, mergeable_state) = match pull.mergeable.as_str() {
        "MERGEABLE" => (Some(true), "clean"),
        "CONFLICTING" => (Some(false), "dirty"),
        "UNKNOWN" => (None, "unknown"),
        _ => return Err("GitHub pull request enum was invalid".into()),
    };
    let check_failure = check_runs.iter().any(|check| {
        check.status == "completed"
            && matches!(
                check.conclusion.as_deref(),
                Some(
                    "failure"
                        | "timed_out"
                        | "cancelled"
                        | "action_required"
                        | "startup_failure"
                        | "stale"
                )
            )
    });
    let status_failure = statuses
        .iter()
        .any(|status| matches!(status.state.as_str(), "failure" | "error"));
    let facets = Facets {
        review_requested: review_request_total_count > 0,
        ci_failure: check_failure
            || status_failure
            || matches!(combined_state.as_str(), "failure" | "error"),
        merge_conflict: mergeable == Some(false),
        terminal: pull.merged || matches!(state.as_str(), "closed" | "merged"),
    };

    serde_json::to_value(Snapshot {
        schema: SNAPSHOT_SCHEMA,
        uri,
        observed_at: &source.observed_at,
        repository: RepositorySnapshot {
            owner: &reference.owner,
            name: &reference.repo,
        },
        number: reference.number,
        pull_request: PullRequestSnapshot {
            api_url,
            html_url: pull.url,
            title: pull.title,
            body: pull.body,
            state,
            author: pull.author.map(|author| author.login),
            draft: pull.is_draft,
            merged: pull.merged,
            merged_at: pull.merged_at,
            closed_at: pull.closed_at,
            mergeable,
            mergeable_state: mergeable_state.to_owned(),
            head: HeadSnapshot {
                sha: pull.head_ref_oid,
                r#ref: pull.head_ref_name,
            },
            base: BaseSnapshot {
                r#ref: pull.base_ref_name,
            },
            review_decision,
            requested_reviewers,
            requested_teams,
            review_request_total_count,
            review_requests_truncated: review_request_total_count
                > review_request_observed_count as u64,
        },
        ci: CiSnapshot {
            state: combined_state,
            total_count,
            truncated,
            check_runs,
            statuses,
        },
        facets,
    })
    .map_err(|_| "GitHub pull request snapshot normalization failed".to_owned())
}

fn normalized_enum(value: String, allowed: &[&str]) -> Result<String, String> {
    let normalized = value.to_ascii_lowercase();
    allowed
        .contains(&normalized.as_str())
        .then_some(normalized)
        .ok_or_else(|| "GitHub pull request enum was invalid".to_owned())
}

fn same_semantics(before: &Value, after: &Value) -> bool {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return false;
    };
    let before_len = before
        .keys()
        .filter(|key| key.as_str() != "observedAt")
        .count();
    let after_len = after
        .keys()
        .filter(|key| key.as_str() != "observedAt")
        .count();
    before_len == after_len
        && before
            .iter()
            .filter(|(key, _)| key.as_str() != "observedAt")
            .all(|(key, value)| after.get(key) == Some(value))
}

fn topics(previous: Option<&Value>, current: &Value) -> Vec<String> {
    let Some(previous) = previous else {
        return TOPICS.iter().map(|topic| (*topic).to_owned()).collect();
    };
    [
        ("ciFailure", "ci.failure"),
        ("mergeConflict", "mergeability.conflict"),
        ("reviewRequested", "review.requested"),
        ("terminal", "terminal"),
    ]
    .into_iter()
    .filter(|&(facet, _topic)| facet_value(previous, facet) != facet_value(current, facet))
    .map(|(_facet, topic)| topic.to_owned())
    .collect()
}

fn facts(previous: Option<&Value>, current: &Value, number: u64) -> Vec<provider_api::Fact> {
    fn state(snapshot: &Value) -> Option<&str> {
        let pull_request = snapshot.get("pullRequest")?;
        if pull_request.get("merged").and_then(Value::as_bool) == Some(true) {
            Some("merged")
        } else if pull_request.get("state").and_then(Value::as_str) == Some("closed") {
            Some("closed")
        } else if pull_request.get("draft").and_then(Value::as_bool) == Some(true) {
            Some("draft")
        } else {
            pull_request.get("state").and_then(Value::as_str)
        }
    }
    fn ci(snapshot: &Value) -> Option<&str> {
        if snapshot
            .get("facets")
            .and_then(|facets| facets.get("ciFailure"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            Some("failure")
        } else {
            snapshot
                .get("ci")
                .and_then(|ci| ci.get("state"))
                .and_then(Value::as_str)
        }
    }
    let current_state = state(current).unwrap_or("unknown");
    let current_ci = ci(current).unwrap_or("unknown");
    let mut facts = vec![current_fact("pr", format!("#{number}"))];
    match previous {
        Some(previous) => {
            facts.push(transition_fact("state", state(previous), current_state));
            facts.push(transition_fact("ci", ci(previous), current_ci));
        }
        None => {
            facts.push(current_fact("state", current_state));
            facts.push(current_fact("ci", current_ci));
        }
    }
    facts
}

fn current_fact(key: &str, value: impl Into<String>) -> provider_api::Fact {
    provider_api::Fact {
        key: key.into(),
        before: provider_api::FactValue::Omitted,
        after: provider_api::FactValue::Value(value.into()),
    }
}

fn transition_fact(key: &str, before: Option<&str>, after: &str) -> provider_api::Fact {
    provider_api::Fact {
        key: key.into(),
        before: before.map_or(provider_api::FactValue::Null, |value| {
            provider_api::FactValue::Value(value.into())
        }),
        after: provider_api::FactValue::Value(after.into()),
    }
}

fn facet_value(snapshot: &Value, facet: &str) -> Option<bool> {
    snapshot
        .get("facets")
        .and_then(|facets| facets.get(facet))
        .and_then(Value::as_bool)
}

fn valid_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_head_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn map_source_error(error: github_pr::PullRequestError) -> String {
    match error {
        github_pr::PullRequestError::Denied => "GitHub pull request scope denied",
        github_pr::PullRequestError::AuthenticationRequired => {
            "GitHub authentication is unavailable"
        }
        github_pr::PullRequestError::Unavailable => "GitHub is unavailable",
        github_pr::PullRequestError::ResourceExhausted => "GitHub response exceeded limits",
        github_pr::PullRequestError::DeadlineExceeded => "GitHub request deadline exceeded",
    }
    .into()
}

export!(Component);
