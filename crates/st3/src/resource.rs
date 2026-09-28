use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug)]
pub struct ObservationRequest {
    pub provider: String,
    pub locator: String,
    pub fields: BTreeSet<String>,
    pub cursor: Option<String>,
    pub previous_facts: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderObservation {
    pub facts: Value,
    pub cursor: Option<String>,
    pub next_check_unix_ms: u128,
}

#[derive(Debug)]
pub struct ProviderRateLimit {
    pub retry_at_unix_ms: u128,
    pub unauthenticated: bool,
    pub status: u16,
}

impl std::fmt::Display for ProviderRateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitHub HTTP {} rate limit", self.status)
    }
}

impl std::error::Error for ProviderRateLimit {}

#[derive(Debug)]
pub struct ProviderUnauthenticated {
    pub status: u16,
}

impl std::fmt::Display for ProviderUnauthenticated {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "GitHub HTTP {} without authentication",
            self.status
        )
    }
}

impl std::error::Error for ProviderUnauthenticated {}

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

fn github_response(
    response: reqwest::Response,
    unauthenticated: bool,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        bail!(ProviderRateLimit {
            retry_at_unix_ms: github_retry_at(response.headers(), now),
            unauthenticated,
            status: status.as_u16(),
        });
    }
    if unauthenticated && status == reqwest::StatusCode::NOT_FOUND {
        bail!(ProviderUnauthenticated {
            status: status.as_u16()
        });
    }
    Ok(response.error_for_status()?)
}

fn github_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("st3-resource-observer/0.1")
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
    let locator = parse_github_ref_locator(&request.locator)?;
    let client = github_client();
    let token = github_token().await?;
    let base = format!(
        "https://api.github.com/repos/{}/{}",
        locator.owner, locator.repository
    );
    let branch = github_json(
        &client,
        format!("{base}/branches/{}", urlencoding::encode(&locator.name)),
        &token,
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
                    &token,
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
                        &token,
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
        next_check_unix_ms: now.saturating_add(300_000),
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
    observe_github_repository_at(request, "https://api.github.com", Some(&token)).await
}

async fn observe_github_repository_at(
    request: ObservationRequest,
    api_base: &str,
    token: Option<&str>,
) -> Result<ProviderObservation> {
    let token = token
        .filter(|value| !value.trim().is_empty())
        .context(GITHUB_AUTH_REMEDY)?;
    let (owner, repository) = request
        .locator
        .split_once('/')
        .context("a GitHub repository locator needs OWNER/REPO")?;
    anyhow::ensure!(
        !owner.is_empty() && !repository.is_empty() && !repository.contains('/'),
        "a GitHub repository locator needs OWNER/REPO"
    );
    let client = github_client();
    let base = format!("{api_base}/repos/{owner}/{repository}");
    // A renamed repository answers through a redirect. Its numeric ID proves that the locator
    // still names the repository whose items were observed before.
    let repository_id = github_json(&client, base.clone(), token)
        .await?
        .value
        .get("id")
        .and_then(Value::as_u64)
        .context("the GitHub repository response has no numeric ID")?;
    let pulls = if request.fields.contains("pull_requests") {
        github_pages(
            &client,
            format!("{base}/pulls?state=open&per_page=100"),
            token,
        )
        .await?
    } else {
        Vec::new()
    };
    let issues = if request.fields.contains("issues") {
        github_pages(
            &client,
            format!("{base}/issues?state=open&per_page=100"),
            token,
        )
        .await?
    } else {
        Vec::new()
    };
    let mut facts = normalize_github_repository(
        request.previous_facts.as_ref(),
        repository_id,
        &pulls,
        &issues,
        &request.fields,
    )?;
    facts["github_http_requests_since_start"] = Value::from(github_request_count(&request.locator));
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

const GITHUB_CACHE_FOR: Duration = Duration::from_secs(300);

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

fn github_request_counts() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static COUNTS: OnceLock<std::sync::Mutex<HashMap<String, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn github_repository_key(url: &str) -> Option<String> {
    let path = url.split("/repos/").nth(1)?;
    let mut parts = path.split('/');
    Some(format!(
        "{}/{}",
        parts.next()?,
        parts.next()?.split('?').next()?
    ))
}

fn github_request_count(repository: &str) -> u64 {
    github_request_counts()
        .lock()
        .expect("GitHub count mutex poisoned")
        .get(repository)
        .copied()
        .unwrap_or(0)
}

/// A URL is fetched at most once per cache interval on this host. A stale entry is revalidated
/// with its ETag; a 304 keeps the complete prior body, including pagination links.
async fn github_json(client: &reqwest::Client, url: String, token: &str) -> Result<GithubPayload> {
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
        && payload.checked_at.elapsed() < GITHUB_CACHE_FOR
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
    if let Some(repository) = github_repository_key(&url) {
        let mut counts = github_request_counts()
            .lock()
            .expect("GitHub count mutex poisoned");
        *counts.entry(repository).or_default() += 1;
    }
    let response = github_response(response, false)?;
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

async fn github_pages(client: &reqwest::Client, url: String, token: &str) -> Result<Vec<Value>> {
    let mut next = Some(url);
    let mut pages = 0;
    let mut values = Vec::new();
    while let Some(url) = next {
        anyhow::ensure!(
            pages < GITHUB_LIST_PAGES,
            "the GitHub listing has more than {GITHUB_LIST_PAGES} pages"
        );
        let payload = github_json(client, url, token).await?;
        values.extend(serde_json::from_value::<Vec<Value>>(payload.value)?);
        next = payload.next;
        pages += 1;
    }
    Ok(values)
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

/// Merge one complete repository listing into the previous facts. Items are identified by
/// their number within the observed repository, so a rename keeps every identity. A listing
/// never removes a previous item or a field that this observation did not request.
pub(crate) fn normalize_github_repository(
    previous: Option<&Value>,
    repository_id: u64,
    pulls: &[Value],
    issues: &[Value],
    fields: &BTreeSet<String>,
) -> Result<Value> {
    if let Some(previous_id) = previous
        .and_then(|value| value.get("repository_id"))
        .and_then(Value::as_u64)
    {
        anyhow::ensure!(
            previous_id == repository_id,
            "the locator now names GitHub repository {repository_id}, not the observed repository {previous_id}"
        );
    }
    let mut facts = previous
        .and_then(Value::as_object)
        .map(|previous| {
            previous
                .iter()
                .filter(|(field, _)| matches!(field.as_str(), "pull_requests" | "issues"))
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    facts.insert("repository_id".into(), Value::from(repository_id));
    if fields.contains("pull_requests") {
        let mut values = previous
            .and_then(|value| value.get("pull_requests"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for pull in pulls {
            let value = json!({
                "number": pull.get("number").cloned().unwrap_or(Value::Null),
                "url": pull.get("html_url").cloned().unwrap_or(Value::Null),
                "title": pull.get("title").cloned().unwrap_or(Value::Null),
                "head": pull.pointer("/head/sha").cloned().unwrap_or(Value::Null),
                "state": "open",
                "draft": pull.get("draft").cloned().unwrap_or(Value::Bool(false)),
            });
            let number = value.get("number");
            if let Some(old) = values.iter_mut().find(|old| old.get("number") == number) {
                *old = value;
            } else if value.get("draft").and_then(Value::as_bool) == Some(false) {
                values.push(value);
            }
        }
        for value in &mut values {
            if !pulls
                .iter()
                .any(|pull| pull.get("number") == value.get("number"))
            {
                value["state"] = Value::String("closed".into());
            }
        }
        values.sort_by_key(|value| value.get("number").and_then(Value::as_u64));
        facts.insert("pull_requests".into(), Value::Array(values));
    }
    if fields.contains("issues") {
        let mut values = previous
            .and_then(|value| value.get("issues"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for issue in issues
            .iter()
            .filter(|issue| issue.get("pull_request").is_none())
        {
            let value = json!({
                "number": issue.get("number").cloned().unwrap_or(Value::Null),
                "url": issue.get("html_url").cloned().unwrap_or(Value::Null),
                "title": issue.get("title").cloned().unwrap_or(Value::Null),
            });
            let number = value.get("number");
            if !values.iter().any(|old| old.get("number") == number) {
                values.push(value);
            }
        }
        values.sort_by_key(|value| value.get("number").and_then(Value::as_u64));
        facts.insert("issues".into(), Value::Array(values));
    }
    Ok(Value::Object(facts))
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
    observe_github_pull_request_at(request, "https://api.github.com", Some(&token)).await
}

async fn observe_github_pull_request_at(
    request: ObservationRequest,
    api_base: &str,
    token: Option<&str>,
) -> Result<ProviderObservation> {
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
    )
    .await?;
    let pull = if let Some(pull) = pulls
        .into_iter()
        .find(|pull| pull.get("number").and_then(Value::as_u64) == Some(number))
    {
        pull
    } else {
        github_json(&client, format!("{base}/pulls/{number}"), token)
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
            })
            .await
            .unwrap();
        assert_eq!(observation.facts["status"], "missing");
    }

    #[test]
    fn repository_discovery_filters_drafts_and_pull_requests_from_issues() {
        let fields = BTreeSet::from(["pull_requests".into(), "issues".into()]);
        let facts = normalize_github_repository(
            None,
            7,
            &[
                json!({"number": 1, "draft": true, "title": "draft"}),
                json!({"number": 2, "draft": false, "title": "ready", "head": {"sha": "abc"}}),
            ],
            &[
                json!({"number": 2, "title": "PR", "pull_request": {}}),
                json!({"number": 3, "title": "Issue"}),
            ],
            &fields,
        )
        .unwrap();
        assert_eq!(facts["pull_requests"].as_array().unwrap().len(), 1);
        assert_eq!(facts["pull_requests"][0]["number"], 2);
        assert_eq!(facts["issues"].as_array().unwrap().len(), 1);
        assert_eq!(facts["issues"][0]["number"], 3);
    }

    #[test]
    fn repository_discovery_retains_old_items_and_adds_a_ready_draft_once() {
        let fields = BTreeSet::from(["pull_requests".into()]);
        let previous = json!({"pull_requests": [{"number": 1, "title": "old"}]});
        let facts = normalize_github_repository(
            Some(&previous),
            7,
            &[json!({"number": 2, "draft": false, "title": "now ready"})],
            &[],
            &fields,
        )
        .unwrap();
        assert_eq!(facts["pull_requests"].as_array().unwrap().len(), 2);
        let repeated = normalize_github_repository(Some(&facts), 7, &[], &[], &fields).unwrap();
        assert_eq!(repeated["pull_requests"][1]["state"], "closed");
    }

    #[tokio::test]
    async fn repository_observer_reuses_facts_after_an_etag_not_modified_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            // Two observers share a poll; a later poll revalidates both URLs.
            for index in 0..4 {
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
                requests.push(String::from_utf8(request).unwrap());
                let (etag, body) = if index % 2 == 0 {
                    ("\"repo-v1\"", r#"{"id":7}"#)
                } else {
                    (
                        "\"issues-v1\"",
                        r#"[{"number":7,"title":"An invented issue"}]"#,
                    )
                };
                let response = if index < 2 {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: {etag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .into()
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let request = ObservationRequest {
            provider: "github.repository".into(),
            locator: "example/repo".into(),
            fields: BTreeSet::from(["issues".into()]),
            cursor: None,
            previous_facts: None,
        };
        let first = observe_github_repository_at(request.clone(), &base, Some("orchid-test-token"))
            .await
            .unwrap();
        let second = observe_github_repository_at(
            ObservationRequest {
                cursor: first.cursor.clone(),
                previous_facts: Some(first.facts.clone()),
                ..request
            },
            &base,
            Some("orchid-test-token"),
        )
        .await
        .unwrap();
        assert_eq!(second.facts, first.facts);
        assert_eq!(second.facts["repository_id"], 7);
        let entries = github_cache()
            .lock()
            .await
            .iter()
            .filter(|(url, _)| url.starts_with(&base))
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();
        for entry in entries {
            let mut cached = entry.lock().await;
            if let Some(payload) = cached.as_mut() {
                payload.checked_at = Instant::now() - GITHUB_CACHE_FOR;
            }
        }
        let third = observe_github_repository_at(
            ObservationRequest {
                cursor: second.cursor.clone(),
                previous_facts: Some(second.facts.clone()),
                provider: "github.repository".into(),
                locator: "example/repo".into(),
                fields: BTreeSet::from(["issues".into()]),
            },
            &base,
            Some("orchid-test-token"),
        )
        .await
        .unwrap();
        assert_eq!(third.facts["issues"], second.facts["issues"]);
        let requests = server
            .await
            .unwrap()
            .into_iter()
            .map(|request| request.to_ascii_lowercase())
            .collect::<Vec<_>>();
        assert!(
            requests
                .iter()
                .all(|request| request.contains("authorization: bearer orchid-test-token"))
        );
        assert!(requests[0].starts_with("get /repos/example/repo "));
        assert!(requests[1].starts_with("get /repos/example/repo/issues?"));
        assert!(!requests[0].contains("if-none-match"));
        assert!(!requests[1].contains("if-none-match"));
        assert!(requests[2].contains("if-none-match: \"repo-v1\""));
        assert!(requests[3].contains("if-none-match: \"issues-v1\""));
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
    fn a_renamed_repository_keeps_its_items_and_a_narrower_read_keeps_other_fields() {
        let fields = BTreeSet::from(["pull_requests".into(), "issues".into()]);
        let before = normalize_github_repository(
            None,
            7,
            &[json!({"number": 4, "draft": false, "title": "PR", "html_url": "https://github.com/acme/old/pull/4"})],
            &[json!({"number": 5, "title": "Issue", "html_url": "https://github.com/acme/old/issues/5"})],
            &fields,
        )
        .unwrap();

        let renamed = normalize_github_repository(
            Some(&before),
            7,
            &[json!({"number": 4, "draft": false, "title": "PR", "html_url": "https://github.com/acme/new/pull/4"})],
            &[json!({"number": 5, "title": "Issue", "html_url": "https://github.com/acme/new/issues/5"})],
            &fields,
        )
        .unwrap();
        assert_eq!(renamed["repository_id"], before["repository_id"]);
        assert_eq!(
            renamed["pull_requests"][0]["number"],
            before["pull_requests"][0]["number"]
        );

        let narrower = normalize_github_repository(
            Some(&before),
            7,
            &[],
            &[],
            &BTreeSet::from(["pull_requests".into()]),
        )
        .unwrap();
        assert_eq!(narrower["issues"], before["issues"]);
        assert_eq!(narrower["pull_requests"][0]["state"], "closed");

        let error = normalize_github_repository(Some(&before), 8, &[], &[], &fields).unwrap_err();
        assert!(error.to_string().contains("repository 8"), "{error}");
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
}
