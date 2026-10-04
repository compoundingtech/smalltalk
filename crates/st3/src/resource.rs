use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};

/// What the built-in `merged` and `ci-passed` gates read from GitHub.
pub mod github_gates;
mod github_repository;
pub(crate) use github_repository::{RECENT_COMMENTS, merge_recent_comments, recent_comment_key};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug)]
pub struct ObservationRequest {
    pub provider: String,
    pub locator: String,
    pub fields: BTreeSet<String>,
    pub cursor: Option<String>,
    pub previous_facts: Option<Value>,
    pub every_ms: Option<u64>,
    /// A declared refresh asked for this observation. It revalidates with GitHub instead of
    /// reusing a cached response; a conditional request costs nothing when nothing changed.
    pub refresh: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderObservation {
    pub facts: Value,
    pub cursor: Option<String>,
    pub next_check_unix_ms: u128,
}

/// A provider that asks to be polled again within this many milliseconds continues work it could
/// not finish in one poll, so an observer's declared interval does not delay it.
pub const PROVIDER_CONTINUE_MS: u128 = 1_000;

/// GitHub refused a request until a known time: a primary or secondary rate limit. It clears
/// on its own at `retry_at_unix_ms`.
#[derive(Debug)]
pub struct ProviderRateLimit {
    pub retry_at_unix_ms: u128,
    pub status: u16,
}

impl std::fmt::Display for ProviderRateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitHub HTTP {} rate limit", self.status)
    }
}

impl std::error::Error for ProviderRateLimit {}

/// GitHub rejected the token itself. Nothing changes until a person signs in again.
#[derive(Debug)]
pub struct ProviderUnauthenticated {
    pub status: u16,
}

impl std::fmt::Display for ProviderUnauthenticated {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "GitHub HTTP {} rejected the token's credentials",
            self.status
        )
    }
}

impl std::error::Error for ProviderUnauthenticated {}

/// GitHub refused a request for a reason other than a rate limit, such as a token without access
/// to the repository or an organization's SSO policy. Nothing changes until a person grants
/// access.
#[derive(Debug)]
pub struct ProviderForbidden {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ProviderForbidden {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitHub HTTP {} forbidden", self.status)?;
        if !self.message.is_empty() {
            write!(formatter, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderForbidden {}

fn github_retry_at(headers: &reqwest::header::HeaderMap, now: u128) -> u128 {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<u128>()
                .ok()
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1_000)))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|date| date.timestamp_millis().max(0) as u128)
                })
        });
    let rate_reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u128>().ok())
        .map(|seconds| seconds.saturating_mul(1_000));
    retry_after
        .into_iter()
        .chain(rate_reset)
        .max()
        .unwrap_or_else(|| now.saturating_add(15 * 60_000))
}

/// How GitHub refused a request. A 429, or a 403 that exhausted the primary budget
/// (`x-ratelimit-remaining: 0`), names a retry time (`retry-after`) or says so in its message, is
/// a rate limit. Any other 403 is a permission failure, and a 401 is a rejected token.
enum GithubRefusal {
    RateLimit,
    Forbidden(String),
    Unauthenticated,
}

fn github_refusal(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> Option<GithubRefusal> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Some(GithubRefusal::RateLimit);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Some(GithubRefusal::Unauthenticated);
    }
    if status != reqwest::StatusCode::FORBIDDEN {
        return None;
    }
    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "0");
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    if exhausted
        || headers.contains_key(reqwest::header::RETRY_AFTER)
        || message.to_ascii_lowercase().contains("rate limit")
    {
        Some(GithubRefusal::RateLimit)
    } else {
        Some(GithubRefusal::Forbidden(message.trim().to_owned()))
    }
}

async fn github_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !matches!(
        status,
        reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::UNAUTHORIZED
    ) {
        return Ok(response.error_for_status()?);
    }
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    match github_refusal(status, &headers, &body) {
        Some(GithubRefusal::RateLimit) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            bail!(ProviderRateLimit {
                retry_at_unix_ms: github_retry_at(&headers, now),
                status: status.as_u16(),
            })
        }
        Some(GithubRefusal::Forbidden(message)) => bail!(ProviderForbidden {
            status: status.as_u16(),
            message,
        }),
        Some(GithubRefusal::Unauthenticated) => bail!(ProviderUnauthenticated {
            status: status.as_u16()
        }),
        None => bail!("GitHub HTTP {status}"),
    }
}

/// A hung connection must end, or its observer would never be polled again.
const GITHUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GITHUB_REQUEST_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(60)
};

/// Where the daemon reaches GitHub's API: `https://api.github.com`, or `ST3_GITHUB_API_URL` when
/// the daemon's environment names another, such as a test's.
pub(crate) fn github_api_base() -> String {
    std::env::var("ST3_GITHUB_API_URL")
        .ok()
        .map(|base| base.trim().trim_end_matches('/').to_owned())
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_owned())
}

/// The HTTP client every GitHub request of this daemon shares.
pub(crate) fn github_api_client() -> reqwest::Client {
    github_client()
}

fn github_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("st3-resource-observer/0.1")
                .connect_timeout(GITHUB_CONNECT_TIMEOUT)
                .timeout(GITHUB_REQUEST_TIMEOUT)
                .build()
                .expect("GitHub HTTP client configuration is valid")
        })
        .clone()
}

pub trait ResourceProvider: Send + Sync + 'static {
    fn observe(
        &self,
        request: ObservationRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderObservation>> + Send + '_>>;
}

#[derive(Clone, Default)]
pub struct RegisteredResourceProvider;

impl ResourceProvider for RegisteredResourceProvider {
    fn observe(
        &self,
        request: ObservationRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderObservation>> + Send + '_>> {
        Box::pin(async move {
            match request.provider.as_str() {
                "github.pull-request" => observe_github_pull_request(request).await,
                "github.repository" => observe_github_repository(request).await,
                "github.ref" => observe_github_ref(request).await,
                "local.file" => observe_local_file(request),
                provider => bail!("resource provider `{provider}` is not registered"),
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GithubRefLocator {
    owner: String,
    repository: String,
    name: String,
}

fn parse_github_ref_locator(locator: &str) -> Result<GithubRefLocator> {
    let (repository, name) = locator
        .rsplit_once('@')
        .context("a GitHub ref locator needs OWNER/REPO@REF")?;
    let (owner, repository) = repository
        .split_once('/')
        .context("a GitHub ref locator needs OWNER/REPO@REF")?;
    anyhow::ensure!(
        !owner.is_empty()
            && !repository.is_empty()
            && !repository.contains('/')
            && !name.is_empty(),
        "a GitHub ref locator needs OWNER/REPO@REF"
    );
    Ok(GithubRefLocator {
        owner: owner.into(),
        repository: repository.into(),
        name: name.into(),
    })
}

async fn observe_github_ref(request: ObservationRequest) -> Result<ProviderObservation> {
    let token = github_token().await?;
    observe_github_ref_at(request, &github_api_base(), &token).await
}

async fn observe_github_ref_at(request: ObservationRequest, api: &str, token: &str) -> Result<ProviderObservation> {
    let cache_for = github_cache_for(&request);
    let locator = parse_github_ref_locator(&request.locator)?;
    let client = github_client();
    let base = format!(
        "{}/repos/{}/{}",
        api.trim_end_matches('/'),
        locator.owner,
        locator.repository
    );
    let branch = github_json(
        &client,
        format!("{base}/branches/{}", urlencoding::encode(&locator.name)),
        token,
        cache_for,
    )
    .await?
    .value;
    let head = branch
        .pointer("/commit/sha")
        .and_then(Value::as_str)
        .context("the GitHub ref has no head SHA")?
        .to_owned();

    let mut ancestors = Vec::new();
    if request.fields.contains("ancestors") {
        let mut page = 1_u64;
        loop {
            let branches: Vec<Value> = serde_json::from_value(
                github_json(
                    &client,
                    format!("{base}/branches?per_page=100&page={page}"),
                    token,
                    cache_for,
                )
                .await?
                .value,
            )?;
            let count = branches.len();
            for candidate in branches {
                let Some(name) = candidate.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name == locator.name {
                    continue;
                }
                let Some(candidate_head) = candidate.pointer("/commit/sha").and_then(Value::as_str)
                else {
                    continue;
                };
                let merged = if candidate_head == head {
                    true
                } else {
                    let comparison = github_json(
                        &client,
                        format!("{base}/compare/{candidate_head}...{head}"),
                        token,
                        cache_for,
                    )
                    .await?
                    .value;
                    comparison
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(github_comparison_is_merged)
                };
                if merged {
                    ancestors.push(format!("refs/heads/{name}"));
                }
            }
            if count < 100 {
                break;
            }
            page = page
                .checked_add(1)
                .context("the GitHub branch page number overflowed")?;
        }
    }

    let facts = normalize_github_ref(&head, ancestors, &request.fields);
    let cursor = Some(hex::encode(Sha256::digest(serde_json::to_vec(&facts)?)));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok(ProviderObservation {
        facts,
        cursor,
        next_check_unix_ms: now.saturating_add(u128::from(request.every_ms.unwrap_or(300_000))),
    })
}

fn github_comparison_is_merged(status: &str) -> bool {
    matches!(status, "ahead" | "identical")
}

fn normalize_github_ref(
    head: &str,
    mut ancestors: Vec<String>,
    fields: &BTreeSet<String>,
) -> Value {
    let mut facts = serde_json::Map::new();
    if fields.contains("head") {
        facts.insert("head".into(), Value::String(head.into()));
    }
    if fields.contains("ancestors") {
        ancestors.sort();
        ancestors.dedup();
        facts.insert(
            "ancestors".into(),
            Value::Array(ancestors.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(facts)
}

async fn observe_github_repository(request: ObservationRequest) -> Result<ProviderObservation> {
    let token = github_token().await?;
    observe_github_repository_at(request, &github_api_base(), Some(&token)).await
}

async fn observe_github_repository_at(
    request: ObservationRequest,
    api_base: &str,
    token: Option<&str>,
) -> Result<ProviderObservation> {
    github_repository::observe_at(request, api_base, token).await
}

/// The most pages one GitHub listing reads. A larger listing fails the observation instead of
/// recording a partial one, because a partial listing makes older items look new later.
const GITHUB_LIST_PAGES: usize = 10;

fn response_etag(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Return the `rel="next"` target of a GitHub `Link` header.
fn github_next_page(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (target, parameters) = part.split_once(';')?;
        parameters
            .split(';')
            .any(|parameter| parameter.trim() == r#"rel="next""#)
            .then(|| {
                target
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

pub const GITHUB_REF_WATCH_MS: u64 = 30_000;

const GITHUB_CACHE_FOR: Duration = Duration::from_secs(300);

fn github_cache_for(request: &ObservationRequest) -> Duration {
    if request.refresh {
        return Duration::ZERO;
    }
    request
        .every_ms
        .map(Duration::from_millis)
        .unwrap_or(GITHUB_CACHE_FOR)
        .min(GITHUB_CACHE_FOR)
}

#[derive(Clone)]
struct GithubPayload {
    value: Value,
    etag: Option<String>,
    next: Option<String>,
    checked_at: Instant,
}

type GithubCache =
    tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<GithubPayload>>>>>;

fn github_cache() -> &'static GithubCache {
    static CACHE: OnceLock<GithubCache> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

tokio::task_local! {
    /// The observer whose observation sends the GitHub requests in this task.
    static GITHUB_SPENDER: String;
}

/// Count the GitHub requests `future` sends against `observer`.
pub(crate) async fn spend_as<F: Future>(observer: String, future: F) -> F::Output {
    GITHUB_SPENDER.scope(observer, future).await
}

/// Requests sent outside any observation.
const GITHUB_UNATTRIBUTED: &str = "unattributed";
const HOUR_MS: u128 = 3_600_000;

/// What one observer sent to GitHub since the daemon started.
#[derive(Default)]
struct GithubSpend {
    sent: u64,
    not_modified: u64,
    refused: u64,
    last_sent_at_unix_ms: u128,
    /// When each request of the last hour that GitHub counted was sent, and the budget it
    /// spent.
    counted: std::collections::VecDeque<(u128, String)>,
}

/// The token's budget for one GitHub rate limit resource, as GitHub last reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GithubBudget {
    pub resource: String,
    pub limit: u64,
    pub remaining: u64,
    pub used: u64,
    pub reset_at_unix_ms: u128,
    pub reported_at_unix_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GithubSpenderReport {
    pub spender: String,
    pub sent: u64,
    pub not_modified: u64,
    pub refused: u64,
    pub counted_last_hour: u64,
    pub last_sent_at_unix_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GithubBudgetReport {
    pub budget: GithubBudget,
    /// Requests this host's observers sent in the budget's current window that GitHub counted.
    pub counted_here: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GithubUsageReport {
    /// Most requests counted in the last hour first.
    pub spenders: Vec<GithubSpenderReport>,
    pub budgets: Vec<GithubBudgetReport>,
}

/// GitHub requests this daemon sent, by the observer that sent them, and the budget GitHub last
/// reported. Every observer on every host shares the token's hourly budget. The counts stay in
/// memory: a claim per request would cost more than it tells.
#[derive(Default)]
pub(crate) struct GithubUsage {
    spenders: HashMap<String, GithubSpend>,
    budgets: std::collections::BTreeMap<String, GithubBudget>,
}

impl GithubUsage {
    /// Record one response. A 304 answers a conditional request, which GitHub does not count.
    fn record(
        &mut self,
        spender: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        now: u128,
    ) {
        let spend = self.spenders.entry(spender.to_owned()).or_default();
        spend.sent += 1;
        spend.last_sent_at_unix_ms = spend.last_sent_at_unix_ms.max(now);
        if status == reqwest::StatusCode::NOT_MODIFIED {
            spend.not_modified += 1;
        } else {
            spend
                .counted
                .push_back((now, github_budget_resource(headers)));
        }
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::TOO_MANY_REQUESTS
        ) {
            spend.refused += 1;
        }
        while spend
            .counted
            .front()
            .is_some_and(|(sent_at, _)| now.saturating_sub(*sent_at) > HOUR_MS)
        {
            spend.counted.pop_front();
        }
        let Some(budget) = github_budget(headers, now) else {
            return;
        };
        // Concurrent responses arrive out of order. Within one window the lowest remaining
        // count is the latest; a later reset starts a new window.
        match self.budgets.get(&budget.resource) {
            Some(known)
                if known.reset_at_unix_ms > budget.reset_at_unix_ms
                    || (known.reset_at_unix_ms == budget.reset_at_unix_ms
                        && known.remaining < budget.remaining) => {}
            _ => {
                self.budgets.insert(budget.resource.clone(), budget);
            }
        }
    }

    fn report(&self, now: u128) -> GithubUsageReport {
        let counted_since = |resource: &str, since: u128| {
            self.spenders
                .values()
                .flat_map(|spend| spend.counted.iter())
                .filter(|(sent_at, spent)| {
                    *sent_at >= since && *sent_at <= now && spent == resource
                })
                .count() as u64
        };
        let mut spenders = self
            .spenders
            .iter()
            .map(|(spender, spend)| GithubSpenderReport {
                spender: spender.clone(),
                sent: spend.sent,
                not_modified: spend.not_modified,
                refused: spend.refused,
                counted_last_hour: spend
                    .counted
                    .iter()
                    .filter(|(sent_at, _)| now.saturating_sub(*sent_at) <= HOUR_MS)
                    .count() as u64,
                last_sent_at_unix_ms: spend.last_sent_at_unix_ms,
            })
            .collect::<Vec<_>>();
        spenders.sort_by(|left, right| {
            right
                .counted_last_hour
                .cmp(&left.counted_last_hour)
                .then(right.sent.cmp(&left.sent))
                .then(left.spender.cmp(&right.spender))
        });
        // Observers call the REST API, which spends the `core` budget, and GraphQL, which
        // spends its own.
        let budgets = self
            .budgets
            .values()
            .map(|budget| GithubBudgetReport {
                budget: budget.clone(),
                counted_here: counted_since(
                    &budget.resource,
                    budget.reset_at_unix_ms.saturating_sub(HOUR_MS),
                ),
            })
            .collect();
        GithubUsageReport { spenders, budgets }
    }
}

/// The budget a response says its request spent. A response without the header spent `core`.
fn github_budget_resource(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get("x-ratelimit-resource")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("core")
        .to_owned()
}

fn github_budget(headers: &reqwest::header::HeaderMap, now: u128) -> Option<GithubBudget> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let number = |name: &str| header(name).and_then(|value| value.parse::<u64>().ok());
    let limit = number("x-ratelimit-limit")?;
    let remaining = number("x-ratelimit-remaining")?;
    let reset_at_unix_ms = u128::from(number("x-ratelimit-reset")?).saturating_mul(1_000);
    Some(GithubBudget {
        resource: github_budget_resource(headers),
        limit,
        remaining,
        used: number("x-ratelimit-used").unwrap_or_else(|| limit.saturating_sub(remaining)),
        reset_at_unix_ms,
        reported_at_unix_ms: now,
    })
}

fn github_usage() -> &'static std::sync::Mutex<GithubUsage> {
    static USAGE: OnceLock<std::sync::Mutex<GithubUsage>> = OnceLock::new();
    USAGE.get_or_init(Default::default)
}

/// GitHub requests this daemon sent by observer, and the budget GitHub last reported.
pub(crate) fn github_usage_report() -> GithubUsageReport {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    github_usage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .report(now)
}

fn record_github_response(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let spender = GITHUB_SPENDER
        .try_with(Clone::clone)
        .unwrap_or_else(|_| GITHUB_UNATTRIBUTED.to_owned());
    github_usage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(&spender, status, headers, now);
}

/// A URL is fetched at most once per cache interval on this host. A stale entry is revalidated
/// with its ETag; a 304 keeps the complete prior body, including pagination links.
async fn github_json(
    client: &reqwest::Client,
    url: String,
    token: &str,
    cache_for: Duration,
) -> Result<GithubPayload> {
    let entry = {
        let mut cache = github_cache().lock().await;
        if cache.len() > 4_096 {
            cache.clear();
        }
        cache
            .entry(url.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    };
    let mut cached = entry.lock().await;
    if let Some(payload) = cached.as_ref()
        && payload.checked_at.elapsed() < cache_for
    {
        return Ok(payload.clone());
    }
    let mut request = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token);
    if let Some(etag) = cached.as_ref().and_then(|payload| payload.etag.as_deref()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await?;
    record_github_response(response.status(), response.headers());
    let response = github_response(response).await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        let payload = cached
            .as_mut()
            .context("GitHub returned 304 without a cached body")?;
        payload.checked_at = Instant::now();
        return Ok(payload.clone());
    }
    let etag = response_etag(&response);
    let next = response
        .headers()
        .get(reqwest::header::LINK)
        .and_then(|value| value.to_str().ok())
        .and_then(github_next_page);
    let payload = GithubPayload {
        value: response.json().await?,
        etag,
        next,
        checked_at: Instant::now(),
    };
    *cached = Some(payload.clone());
    Ok(payload)
}

async fn github_pages(
    client: &reqwest::Client,
    url: String,
    token: &str,
    cache_for: Duration,
) -> Result<Vec<Value>> {
    Ok(
        github_listing(client, url, token, cache_for, GITHUB_LIST_PAGES, false)
            .await?
            .values,
    )
}

/// One read of a paged GitHub listing: its items, a version for each page (its ETag, or its
/// content when GitHub named none), and whether more pages remained after the most this read
/// takes.
pub(crate) struct GithubListing {
    pub(crate) values: Vec<Value>,
    pub(crate) versions: Vec<String>,
    pub(crate) truncated: bool,
}

/// Read up to `pages` pages of a listing. A listing that continues past them fails the read,
/// unless `resumable` says the caller continues it later from where this read stopped.
async fn github_listing(
    client: &reqwest::Client,
    url: String,
    token: &str,
    cache_for: Duration,
    pages: usize,
    resumable: bool,
) -> Result<GithubListing> {
    let mut next = Some(url);
    let mut listing = GithubListing {
        values: Vec::new(),
        versions: Vec::new(),
        truncated: false,
    };
    while let Some(url) = next {
        if listing.versions.len() == pages {
            anyhow::ensure!(resumable, "the GitHub listing has more than {pages} pages");
            listing.truncated = true;
            break;
        }
        let payload = github_json(client, url, token, cache_for).await?;
        listing
            .versions
            .push(payload.etag.clone().unwrap_or_else(|| {
                hex::encode(Sha256::digest(
                    serde_json::to_vec(&payload.value).unwrap_or_default(),
                ))
            }));
        listing
            .values
            .extend(serde_json::from_value::<Vec<Value>>(payload.value)?);
        next = payload.next;
    }
    Ok(listing)
}

/// Ask GitHub's GraphQL API one query. GraphQL has its own hourly budget, reports a refusal in a
/// successful response, and has no conditional request.
async fn github_graphql(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    query: &str,
    variables: Value,
) -> Result<Value> {
    let response = client
        .post(format!("{api_base}/graphql"))
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)
        .json(&json!({"query": query, "variables": variables}))
        .send()
        .await?;
    record_github_response(response.status(), response.headers());
    let response = github_response(response).await?;
    let headers = response.headers().clone();
    let body = response.json::<Value>().await?;
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        if errors
            .iter()
            .any(|error| error.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"))
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            bail!(ProviderRateLimit {
                retry_at_unix_ms: github_retry_at(&headers, now),
                status: 200,
            });
        }
        let message = errors[0]
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("an unnamed error");
        bail!("GitHub GraphQL refused the query: {message}");
    }
    body.get("data")
        .filter(|data| !data.is_null())
        .cloned()
        .context("the GitHub GraphQL response has no data")
}

pub(crate) const GITHUB_AUTH_REMEDY: &str = "GitHub observers have no token; run `gh auth login` as the daemon account or export GH_TOKEN/GITHUB_TOKEN in that account's login-shell startup files. Check the daemon PATH with `st doctor`. No anonymous request was sent; authentication is checked again on the next poll.";

pub(crate) async fn github_token() -> Result<String> {
    static TOKEN: OnceLock<tokio::sync::Mutex<Option<(Instant, String)>>> = OnceLock::new();
    let cache = TOKEN.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cached = cache.lock().await;
    if let Some((checked_at, token)) = cached.as_ref()
        && checked_at.elapsed() < Duration::from_secs(180)
    {
        return Ok(token.clone());
    }
    let environment = tokio::task::spawn_blocking(crate::environment::snapshot).await??;
    let token = lookup_github_token(&environment)
        .await
        .context(GITHUB_AUTH_REMEDY)?;
    *cached = Some((Instant::now(), token.clone()));
    Ok(token)
}

async fn lookup_github_token(
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    if let Some(token) = ["GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .filter_map(|name| environment.get(name))
        .find(|value| !value.trim().is_empty())
    {
        return Ok(token.clone());
    }
    let mut command =
        tokio::process::Command::from(crate::environment::command_in("gh", environment)?);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        command.args(["auth", "token"]).kill_on_drop(true).output(),
    )
    .await
    .context("GitHub credential lookup timed out")??;
    // gh stderr can contain sensitive data. Only report a fixed, actionable error.
    anyhow::ensure!(output.status.success(), "gh auth token failed");
    let token = String::from_utf8(output.stdout).context("gh returned a non-UTF-8 token")?;
    let token = token.trim();
    anyhow::ensure!(!token.is_empty(), "gh returned an empty token");
    Ok(token.to_owned())
}

/// An st agent on this host and the workspace it works in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentWorkspace {
    pub(crate) agent: String,
    pub(crate) run: Option<String>,
    pub(crate) workspace: PathBuf,
}

/// Name the st agent that opened each ready pull request in a repository listing. Agents open
/// pull requests with a shared GitHub identity, so the author login cannot name one. The branch
/// can: when exactly one agent on this host has the pull request's branch checked out in its
/// workspace as the pull request appears or moves to a new head, that agent is recorded as
/// `opened_by` and its mission run as `opened_by_run`. A pull request keeps an opener once named,
/// so a later checkout of the same branch by a reviewer or a fixer does not take it over.
pub(crate) fn attach_pull_request_openers(
    facts: &mut Value,
    previous: Option<&Value>,
    agents: &[AgentWorkspace],
) {
    let previous_items = previous
        .and_then(|value| value.get("pull_requests"))
        .and_then(Value::as_array);
    let Some(items) = facts.get_mut("pull_requests").and_then(Value::as_array_mut) else {
        return;
    };
    let mut branches = None;
    for item in items {
        let prior = previous_items.and_then(|previous| {
            previous
                .iter()
                .find(|old| old.get("number").is_some() && old.get("number") == item.get("number"))
        });
        if let Some(prior) = prior.filter(|prior| prior.get("opened_by").is_some()) {
            for name in ["opened_by", "opened_by_run"] {
                if let Some(value) = prior.get(name) {
                    item[name] = value.clone();
                }
            }
            continue;
        }
        let head = item.get("head").filter(|head| !head.is_null());
        let resolvable = head.is_some()
            && prior.is_none_or(|prior| prior.get("head") != item.get("head"))
            && item.get("state").and_then(Value::as_str) == Some("open")
            && item.get("draft").and_then(Value::as_bool) != Some(true);
        let Some(branch) = item
            .get("branch")
            .and_then(Value::as_str)
            .filter(|_| resolvable)
        else {
            continue;
        };
        let owners = branches
            .get_or_insert_with(|| checked_out_branches(agents))
            .get(branch);
        if let Some([owner]) = owners.map(Vec::as_slice) {
            item["opened_by"] = Value::String(owner.agent.clone());
            if let Some(run) = &owner.run {
                item["opened_by_run"] = Value::String(run.clone());
            }
        }
    }
}

/// Each branch checked out in an agent workspace, with the agents that work there.
fn checked_out_branches(agents: &[AgentWorkspace]) -> HashMap<String, Vec<&AgentWorkspace>> {
    let mut branches = HashMap::<String, Vec<&AgentWorkspace>>::new();
    for agent in agents {
        if let Some(branch) = checked_out_branch(&agent.workspace) {
            branches.entry(branch).or_default().push(agent);
        }
    }
    branches
}

/// The branch that a Git working tree has checked out, read from its `HEAD` without starting
/// Git. A linked worktree's `.git` file names its own Git directory.
pub(crate) fn checked_out_branch(workspace: &Path) -> Option<String> {
    let dot_git = workspace.join(".git");
    let git_directory = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            workspace.join(target)
        }
    };
    let head = std::fs::read_to_string(git_directory.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

fn observe_local_file(request: ObservationRequest) -> Result<ProviderObservation> {
    let path = Path::new(&request.locator);
    anyhow::ensure!(
        path.is_absolute(),
        "a local file locator must be an absolute path"
    );
    let mut facts = serde_json::Map::new();
    facts.insert("path".into(), Value::String(request.locator.clone()));
    match std::fs::read(path) {
        Ok(bytes) => {
            facts.insert("status".into(), Value::String("ready".into()));
            facts.insert(
                "content_hash".into(),
                Value::String(hex::encode(Sha256::digest(&bytes))),
            );
            facts.insert("size".into(), Value::from(bytes.len() as u64));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
                facts.insert("mode".into(), Value::from(mode));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            facts.insert("status".into(), Value::String("missing".into()));
        }
        Err(error) => {
            facts.insert("status".into(), Value::String("unreadable".into()));
            facts.insert("reason".into(), Value::String(error.to_string()));
        }
    }
    facts.retain(|name, _| request.fields.contains(name) || name == "status" || name == "path");
    let facts = Value::Object(facts);
    let cursor = Some(hex::encode(Sha256::digest(serde_json::to_vec(&facts)?)));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok(ProviderObservation {
        facts,
        cursor,
        next_check_unix_ms: now.saturating_add(60_000),
    })
}

async fn observe_github_pull_request(request: ObservationRequest) -> Result<ProviderObservation> {
    let token = github_token().await?;
    observe_github_pull_request_at(request, &github_api_base(), Some(&token)).await
}

async fn observe_github_pull_request_at(
    request: ObservationRequest,
    api_base: &str,
    token: Option<&str>,
) -> Result<ProviderObservation> {
    let cache_for = github_cache_for(&request);
    let token = token
        .filter(|value| !value.trim().is_empty())
        .context(GITHUB_AUTH_REMEDY)?;
    let (repository, number) = request
        .locator
        .rsplit_once('#')
        .context("a GitHub pull request locator needs OWNER/REPO#NUMBER")?;
    let (owner, repository) = repository
        .split_once('/')
        .context("a GitHub pull request locator needs OWNER/REPO#NUMBER")?;
    let number = number
        .parse::<u64>()
        .context("a GitHub pull request number must be an integer")?;
    let client = github_client();
    let base = format!("{api_base}/repos/{owner}/{repository}");
    let pulls = github_pages(
        &client,
        format!("{base}/pulls?state=open&per_page=100"),
        token,
        cache_for,
    )
    .await?;
    let pull = if let Some(pull) = pulls
        .into_iter()
        .find(|pull| pull.get("number").and_then(Value::as_u64) == Some(number))
    {
        pull
    } else {
        github_json(&client, format!("{base}/pulls/{number}"), token, cache_for)
            .await?
            .value
    };
    let mut facts = serde_json::Map::new();
    if request.fields.contains("head") {
        facts.insert(
            "head".into(),
            pull.pointer("/head/sha").cloned().unwrap_or(Value::Null),
        );
    }
    if request.fields.contains("state") {
        facts.insert(
            "state".into(),
            json!({
                "state": pull.get("state").cloned().unwrap_or(Value::Null),
                "draft": pull.get("draft").cloned().unwrap_or(Value::Null),
                "merged": pull.get("merged").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    if request.fields.contains("review") {
        let reviews = github_pages(
            &client,
            format!("{base}/pulls/{number}/reviews?per_page=100"),
            token,
            cache_for,
        )
        .await?;
        let normalized = reviews
            .into_iter()
            .map(|review| {
                json!({
                    "id": review.get("id").cloned().unwrap_or(Value::Null),
                    "user": review.pointer("/user/login").cloned().unwrap_or(Value::Null),
                    "state": review.get("state").cloned().unwrap_or(Value::Null),
                    "submitted_at": review.get("submitted_at").cloned().unwrap_or(Value::Null),
                    "commit_id": review.get("commit_id").cloned().unwrap_or(Value::Null),
                })
            })
            .collect::<Vec<_>>();
        facts.insert("review".into(), Value::Array(normalized));
    }
    if request.fields.contains("checks") {
        let head = pull
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .context("the GitHub pull request has no head SHA")?;
        let checks = github_json(
            &client,
            format!("{base}/commits/{head}/check-runs?per_page=100"),
            token,
            cache_for,
        )
        .await?
        .value;
        let normalized = checks
            .get("check_runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|check| {
                json!({
                    "id": check.get("id").cloned().unwrap_or(Value::Null),
                    "name": check.get("name").cloned().unwrap_or(Value::Null),
                    "status": check.get("status").cloned().unwrap_or(Value::Null),
                    "conclusion": check.get("conclusion").cloned().unwrap_or(Value::Null),
                    "completed_at": check.get("completed_at").cloned().unwrap_or(Value::Null),
                })
            })
            .collect::<Vec<_>>();
        facts.insert("checks".into(), Value::Array(normalized));
    }
    let facts = Value::Object(facts);
    let cursor = Some(hex::encode(Sha256::digest(serde_json::to_vec(&facts)?)));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok(ProviderObservation {
        facts,
        cursor,
        next_check_unix_ms: now.saturating_add(300_000),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn credentials_recover_after_a_missing_token_and_rotate() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let environment =
            std::collections::BTreeMap::from([("PATH".into(), root.path().display().to_string())]);
        assert!(lookup_github_token(&environment).await.is_err());
        let gh = root.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(lookup_github_token(&environment).await.is_err());
        for token in ["orchid-first", "orchid-rotated"] {
            std::fs::write(&gh, format!("#!/bin/sh\nprintf '%s' '{token}'\n")).unwrap();
            assert_eq!(lookup_github_token(&environment).await.unwrap(), token);
        }
        let mut exported = environment;
        exported.insert("GH_TOKEN".into(), "orchid-exported".into());
        assert_eq!(
            lookup_github_token(&exported).await.unwrap(),
            "orchid-exported"
        );
    }

    #[tokio::test]
    async fn missing_credentials_never_send_an_anonymous_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let request = ObservationRequest {
            provider: "github.repository".into(),
            locator: "orchid/garden".into(),
            fields: BTreeSet::new(),
            cursor: None,
            previous_facts: None,
            every_ms: None,
            refresh: false,
        };
        for token in [None, Some(""), Some(" ")] {
            let error = observe_github_repository_at(request.clone(), &base, token)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("no token"));
            assert!(error.to_string().contains("gh auth login"));
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    struct FakeProvider;

    impl ResourceProvider for FakeProvider {
        fn observe(
            &self,
            request: ObservationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderObservation>> + Send + '_>> {
            Box::pin(async move {
                Ok(ProviderObservation {
                    facts: json!({"provider": request.provider, "locator": request.locator}),
                    cursor: Some("fake-cursor".into()),
                    next_check_unix_ms: 42,
                })
            })
        }
    }

    #[tokio::test]
    async fn a_second_provider_uses_the_generic_contract() {
        let observation = FakeProvider
            .observe(ObservationRequest {
                provider: "fake.issue".into(),
                locator: "project/42".into(),
                fields: BTreeSet::from(["state".into()]),
                cursor: None,
                previous_facts: None,
                every_ms: None,
                refresh: false,
            })
            .await
            .unwrap();
        assert_eq!(observation.cursor.as_deref(), Some("fake-cursor"));
        assert_eq!(observation.facts["provider"], "fake.issue");
    }

    #[test]
    fn github_ref_locators_name_one_repository_branch() {
        assert_eq!(
            parse_github_ref_locator("shareup/app-web@feature/instant-items").unwrap(),
            GithubRefLocator {
                owner: "shareup".into(),
                repository: "app-web".into(),
                name: "feature/instant-items".into(),
            }
        );
        for invalid in [
            "shareup/app-web",
            "shareup@app-web",
            "shareup/app-web@",
            "/app-web@main",
            "shareup/a/b@main",
        ] {
            assert!(
                parse_github_ref_locator(invalid).is_err(),
                "accepted invalid locator {invalid}"
            );
        }
    }

    #[test]
    fn github_ref_facts_are_selected_sorted_and_deduplicated() {
        let fields = BTreeSet::from(["head".into(), "ancestors".into()]);
        let facts = normalize_github_ref(
            "abc123",
            vec![
                "refs/heads/topic-b".into(),
                "refs/heads/topic-a".into(),
                "refs/heads/topic-b".into(),
            ],
            &fields,
        );
        assert_eq!(facts["head"], "abc123");
        assert_eq!(
            facts["ancestors"],
            json!(["refs/heads/topic-a", "refs/heads/topic-b"])
        );
        assert!(github_comparison_is_merged("ahead"));
        assert!(github_comparison_is_merged("identical"));
        assert!(!github_comparison_is_merged("behind"));
        assert!(!github_comparison_is_merged("diverged"));

        let head_only = normalize_github_ref(
            "def456",
            vec!["refs/heads/ignored".into()],
            &BTreeSet::from(["head".into()]),
        );
        assert_eq!(head_only, json!({"head": "def456"}));
    }

    #[tokio::test]
    async fn a_local_file_observer_records_metadata_without_content() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("proof.txt");
        std::fs::write(&path, "secret proof\n").unwrap();
        let observation = RegisteredResourceProvider
            .observe(ObservationRequest {
                provider: "local.file".into(),
                locator: path.display().to_string(),
                fields: BTreeSet::from(["content_hash".into(), "size".into(), "status".into()]),
                cursor: None,
                previous_facts: None,
                every_ms: None,
                refresh: false,
            })
            .await
            .unwrap();
        assert_eq!(observation.facts["status"], "ready");
        assert_eq!(observation.facts["size"], 13);
        assert!(observation.facts.get("content_hash").is_some());
        assert!(observation.facts.get("content").is_none());
        assert_eq!(observation.facts["path"], path.display().to_string());
    }

    #[tokio::test]
    async fn a_missing_local_file_is_a_distinct_observation() {
        let path = std::env::temp_dir().join("st3-file-that-does-not-exist");
        let observation = RegisteredResourceProvider
            .observe(ObservationRequest {
                provider: "local.file".into(),
                locator: path.display().to_string(),
                fields: BTreeSet::from(["status".into()]),
                cursor: None,
                previous_facts: None,
                every_ms: None,
                refresh: false,
            })
            .await
            .unwrap();
        assert_eq!(observation.facts["status"], "missing");
    }

    /// A provider that accepts the connection and never answers must not hold its observer forever.
    #[tokio::test]
    async fn a_github_request_that_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(stream);
        });
        let request = ObservationRequest {
            provider: "github.repository".into(),
            locator: "example/repo".into(),
            fields: BTreeSet::from(["issues".into()]),
            cursor: None,
            previous_facts: None,
            every_ms: None,
            refresh: false,
        };
        let observed = tokio::time::timeout(
            Duration::from_secs(10),
            observe_github_repository_at(request, &base, Some("test")),
        )
        .await
        .expect("the request did not time out");
        assert!(observed.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn watched_ref_conditional_reads_keep_the_304_facts_and_detect_the_next_head() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, etag, body) in [
                ("200 OK", "head-one", r#"{"commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#),
                ("304 Not Modified", "head-one", ""),
                ("200 OK", "head-two", r#"{"commit":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0u8; 1024]; let size = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..size]);
                    if size == 0 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n") { break; }
                }
                requests.push(String::from_utf8(request).unwrap().to_lowercase());
                stream.write_all(format!("HTTP/1.1 {status}\r\nETag: \"{etag}\"\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let request = || ObservationRequest { provider:"github.ref".into(), locator:"acme/garden@main".into(), fields:BTreeSet::from(["head".into()]),
            cursor:None, previous_facts:None, every_ms:Some(GITHUB_REF_WATCH_MS), refresh:true };
        let first = observe_github_ref_at(request(), &base, "fixture-token").await.unwrap();
        let unchanged = observe_github_ref_at(request(), &base, "fixture-token").await.unwrap();
        let changed = observe_github_ref_at(request(), &base, "fixture-token").await.unwrap();
        assert_eq!(first.facts, unchanged.facts); assert_eq!(first.cursor, unchanged.cursor);
        assert_ne!(changed.cursor, unchanged.cursor);
        assert_eq!(changed.facts["head"], "b".repeat(40));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        assert!(changed.next_check_unix_ms <= now + u128::from(GITHUB_REF_WATCH_MS));
        let requests = server.await.unwrap();
        assert!(!requests[0].contains("if-none-match"));
        assert!(requests[1].contains("if-none-match: \"head-one\""));
        assert!(requests[2].contains("if-none-match: \"head-one\""));
    }

    #[tokio::test]
    async fn pull_observers_share_one_repository_list_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let size = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..size]);
                if size == 0 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"[{"number":4,"head":{"sha":"aaaa"}},{"number":5,"head":{"sha":"bbbb"}}]"#;
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"pulls-v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        for (number, head) in [(4, "aaaa"), (5, "bbbb")] {
            let result = observe_github_pull_request_at(
                ObservationRequest {
                    provider: "github.pull-request".into(),
                    locator: format!("orchid/garden#{number}"),
                    fields: BTreeSet::from(["head".into()]),
                    cursor: None,
                    previous_facts: None,
                    every_ms: None,
                    refresh: false,
                },
                &base,
                Some("orchid-test-token"),
            )
            .await
            .unwrap();
            assert_eq!(result.facts["head"], head);
        }
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /repos/orchid/garden/pulls?state=open&per_page=100 "));
    }

    #[test]
    fn github_rate_limit_headers_set_the_later_retry_deadline() {
        let now = 1_000_000_u128;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1050".parse().unwrap());
        assert_eq!(github_retry_at(&headers, now), now + 120_000);
    }

    #[test]
    fn github_usage_counts_each_observers_requests_against_the_latest_budget() {
        use reqwest::StatusCode;
        let now = 10_000_000_000_u128;
        let reset = now + 1_800_000;
        let budget = |remaining: u64, reset: u128| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-ratelimit-limit", "5000".parse().unwrap());
            headers.insert(
                "x-ratelimit-remaining",
                remaining.to_string().parse().unwrap(),
            );
            headers.insert(
                "x-ratelimit-used",
                (5000 - remaining).to_string().parse().unwrap(),
            );
            headers.insert(
                "x-ratelimit-reset",
                (reset / 1_000).to_string().parse().unwrap(),
            );
            headers.insert("x-ratelimit-resource", "core".parse().unwrap());
            headers
        };
        let mut usage = GithubUsage::default();
        // Two hours ago, before this budget window and outside the last hour.
        usage.record(
            "observer/orchid",
            StatusCode::OK,
            &budget(4990, now - 5_400_000),
            now - 7_200_000,
        );
        usage.record(
            "observer/orchid",
            StatusCode::OK,
            &budget(3990, reset),
            now - 60_000,
        );
        usage.record(
            "observer/orchid",
            StatusCode::NOT_MODIFIED,
            &budget(3990, reset),
            now - 30_000,
        );
        // An earlier response of the same window that arrives late keeps the lower count.
        usage.record(
            "observer/lichen",
            StatusCode::OK,
            &budget(3995, reset),
            now - 10_000,
        );
        usage.record(
            "observer/lichen",
            StatusCode::FORBIDDEN,
            &reqwest::header::HeaderMap::new(),
            now,
        );

        let report = usage.report(now);
        assert_eq!(
            report.spenders,
            vec![
                GithubSpenderReport {
                    spender: "observer/lichen".into(),
                    sent: 2,
                    not_modified: 0,
                    refused: 1,
                    counted_last_hour: 2,
                    last_sent_at_unix_ms: now,
                },
                GithubSpenderReport {
                    spender: "observer/orchid".into(),
                    sent: 3,
                    not_modified: 1,
                    refused: 0,
                    counted_last_hour: 1,
                    last_sent_at_unix_ms: now - 30_000,
                },
            ]
        );
        assert_eq!(
            report.budgets,
            vec![GithubBudgetReport {
                budget: GithubBudget {
                    resource: "core".into(),
                    limit: 5000,
                    remaining: 3990,
                    used: 1010,
                    reset_at_unix_ms: reset,
                    reported_at_unix_ms: now - 30_000,
                },
                counted_here: 3,
            }]
        );

        // A later reset starts a new window.
        usage.record(
            "observer/orchid",
            StatusCode::OK,
            &budget(4999, reset + 3_600_000),
            now + 1_900_000,
        );
        let report = usage.report(now + 1_900_000);
        assert_eq!(report.budgets[0].budget.remaining, 4999);
        assert_eq!(report.budgets[0].counted_here, 1);
    }

    #[test]
    fn a_permission_403_is_not_a_rate_limit() {
        use reqwest::StatusCode;
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        };
        let refusal = |status, pairs: &[(&'static str, &str)], body: &str| {
            github_refusal(status, &headers(pairs), body)
        };
        // The primary budget is spent.
        assert!(matches!(
            refusal(
                StatusCode::FORBIDDEN,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "1050")
                ],
                r#"{"message": "API rate limit exceeded for user ID 1."}"#,
            ),
            Some(GithubRefusal::RateLimit)
        ));
        // A secondary limit names a retry time or says so.
        assert!(matches!(
            refusal(StatusCode::FORBIDDEN, &[("retry-after", "60")], "{}"),
            Some(GithubRefusal::RateLimit)
        ));
        assert!(matches!(
            refusal(
                StatusCode::FORBIDDEN,
                &[("x-ratelimit-remaining", "4000")],
                r#"{"message": "You have exceeded a secondary rate limit."}"#,
            ),
            Some(GithubRefusal::RateLimit)
        ));
        assert!(matches!(
            refusal(StatusCode::TOO_MANY_REQUESTS, &[], ""),
            Some(GithubRefusal::RateLimit)
        ));
        // A token without access, or an SSO policy, waits for a person instead.
        match refusal(
            StatusCode::FORBIDDEN,
            &[("x-ratelimit-remaining", "4999")],
            r#"{"message": "Resource protected by organization SAML enforcement."}"#,
        ) {
            Some(GithubRefusal::Forbidden(message)) => {
                assert_eq!(
                    message,
                    "Resource protected by organization SAML enforcement."
                );
            }
            _ => panic!("a permission 403 was taken for a rate limit"),
        }
        assert!(matches!(
            refusal(
                StatusCode::UNAUTHORIZED,
                &[],
                r#"{"message": "Bad credentials"}"#
            ),
            Some(GithubRefusal::Unauthenticated)
        ));
        assert!(refusal(StatusCode::NOT_FOUND, &[], "").is_none());
    }

    #[test]
    fn a_github_link_header_names_its_next_page() {
        let link = r#"<https://api.github.com/repositories/7/issues?state=open&page=2>; rel="next", <https://api.github.com/repositories/7/issues?state=open&page=4>; rel="last""#;
        assert_eq!(
            github_next_page(link).as_deref(),
            Some("https://api.github.com/repositories/7/issues?state=open&page=2")
        );
        assert_eq!(
            github_next_page(
                r#"<https://api.github.com/repositories/7/issues?page=1>; rel="prev""#
            ),
            None
        );
    }

    #[test]
    fn a_workspace_names_the_branch_it_has_checked_out() {
        use crate::checkout::test_support::{git, repository};
        let root = tempfile::tempdir().unwrap();
        let clone = repository(root.path());
        let linked = root.path().join("linked");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent/example",
                &linked.to_string_lossy(),
            ],
        );
        assert_eq!(checked_out_branch(&clone).as_deref(), Some("main"));
        assert_eq!(
            checked_out_branch(&linked).as_deref(),
            Some("agent/example")
        );
        git(&linked, &["checkout", "--quiet", "--detach"]);
        assert_eq!(checked_out_branch(&linked), None);
        assert_eq!(checked_out_branch(&root.path().join("absent")), None);
    }

    #[test]
    fn a_new_pull_request_names_the_one_agent_with_its_branch_checked_out() {
        use crate::checkout::test_support::{git, repository};
        let root = tempfile::tempdir().unwrap();
        let clone = repository(root.path());
        let worktree = |name: &str, branch: &str| {
            let path = root.path().join(name);
            git(
                &clone,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    &path.to_string_lossy(),
                ],
            );
            path
        };
        let agent = |name: &str, run: Option<&str>, workspace: PathBuf| AgentWorkspace {
            agent: format!("agent/{name}"),
            run: run.map(str::to_owned),
            workspace,
        };
        let builder_workspace = worktree("builder", "agent/feature");
        let shared_workspace = worktree("shared", "agent/shared");
        let agents = vec![
            agent("builder", Some("mission-run/feature"), builder_workspace),
            agent("steward", None, clone.clone()),
            agent("one", None, shared_workspace.clone()),
            agent("two", None, shared_workspace),
        ];
        let pull = |number: u64, head: &str, branch: &str, state: &str, draft: bool| {
            json!({
                "number": number, "head": head, "branch": branch,
                "state": state, "draft": draft, "author": "shared-login",
            })
        };
        let mut facts = json!({"pull_requests": [
            pull(1, "aaaa", "agent/feature", "open", false),
            pull(2, "bbbb", "agent/shared", "open", false),
            pull(3, "cccc", "agent/feature", "open", true),
            pull(4, "dddd", "agent/feature", "closed", false),
            pull(5, "eeee", "outside/branch", "open", false),
        ]});
        attach_pull_request_openers(&mut facts, None, &agents);
        let items = facts["pull_requests"].as_array().unwrap();
        assert_eq!(items[0]["opened_by"], "agent/builder");
        assert_eq!(items[0]["opened_by_run"], "mission-run/feature");
        for (index, why) in [
            (1, "two agents share the branch"),
            (2, "a draft is not reviewed yet"),
            (3, "a closed pull request is not reviewed"),
            (4, "no agent has the branch"),
        ] {
            assert!(items[index].get("opened_by").is_none(), "{why}");
        }

        // A named opener stays with its pull request, and an unnamed one is not named later at
        // the same head, when another agent checks the branch out to fix or review it.
        let previous = facts.clone();
        let mut next = json!({"pull_requests": [
            pull(1, "ffff", "agent/feature", "open", false),
            pull(5, "eeee", "outside/branch", "open", false),
        ]});
        let fixer = agent(
            "fixer",
            Some("mission-run/fix"),
            worktree("fixer", "outside/branch"),
        );
        let agents = vec![fixer.clone()];
        attach_pull_request_openers(&mut next, Some(&previous), &agents);
        assert_eq!(next["pull_requests"][0]["opened_by"], "agent/builder");
        assert_eq!(
            next["pull_requests"][0]["opened_by_run"],
            "mission-run/feature"
        );
        assert!(next["pull_requests"][1].get("opened_by").is_none());

        // The fixer that pushes a new head of an unnamed pull request is the agent that head
        // belongs to.
        let previous = next.clone();
        let mut pushed =
            json!({"pull_requests": [pull(5, "9999", "outside/branch", "open", false)]});
        attach_pull_request_openers(&mut pushed, Some(&previous), &agents);
        assert_eq!(pushed["pull_requests"][0]["opened_by"], "agent/fixer");
        assert_eq!(
            pushed["pull_requests"][0]["opened_by_run"],
            "mission-run/fix"
        );
    }
}
