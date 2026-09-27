use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use st2_resource_wasip2::{
    CapabilityContext, CapabilityModule, CapabilityPhase, InterruptionReason, InvocationControl,
    InvocationStore,
};
use wasmtime::component::{HasSelf, Linker};

use crate::github_auth::discover_authorization;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/github-issue",
        world: "github-issue-provider",
    });
}

use bindings::compoundingtech::st2_github_issue::github_issue::{
    Host, IssueError, IssueRequest, IssueResponse, SourceObject, SourceObservation, SourceSnapshot,
};

const IMPORT_NAME: &str = "compoundingtech:st2-github-issue/github-issue@0.1.0";
const API_HOST: &str = "api.github.com";
const API_PORT: u16 = 443;
const MAX_HEADERS_BYTES: usize = 16 * 1024;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ETAG_BYTES: usize = 512;
const SNAPSHOT_DIGEST_BYTES: usize = 32;
const MAX_CACHED_SNAPSHOTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubIssueConfig {
    pub auth_executable: PathBuf,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
}

impl GitHubIssueConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.auth_executable.is_absolute() {
            return Err("GitHub authentication executable must be absolute");
        }
        if self.connect_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
            || self.total_timeout > Duration::from_secs(60)
        {
            return Err("GitHub issue deadlines are invalid");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct GitHubIssueModule {
    config: GitHubIssueConfig,
    authorization: Option<HeaderValue>,
    cache: Arc<Mutex<SnapshotCache>>,
}

impl GitHubIssueModule {
    pub fn new(config: GitHubIssueConfig) -> Result<Self, &'static str> {
        config.validate()?;
        let authorization = Instant::now()
            .checked_add(config.total_timeout)
            .and_then(|deadline| discover_authorization(&config.auth_executable, deadline));
        Ok(Self {
            config,
            authorization,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
        })
    }
}

#[derive(Debug, Clone)]
struct CachedObject {
    etag: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct CachedSource {
    issue: CachedObject,
    latest_comment: Option<CachedObject>,
    observed_at: String,
}

#[derive(Default)]
struct SnapshotCache {
    sources: BTreeMap<[u8; SNAPSHOT_DIGEST_BYTES], CachedSource>,
}

impl SnapshotCache {
    fn get(&self, digest: &[u8; SNAPSHOT_DIGEST_BYTES]) -> Option<&CachedSource> {
        self.sources.get(digest)
    }

    fn insert(&mut self, digest: [u8; SNAPSHOT_DIGEST_BYTES], source: CachedSource) {
        if !self.sources.contains_key(&digest)
            && self.sources.len() >= MAX_CACHED_SNAPSHOTS
            && let Some(evicted) = self.sources.keys().next().copied()
        {
            self.sources.remove(&evicted);
        }
        self.sources.insert(digest, source);
    }
}

pub struct GitHubIssueInvocation {
    config: GitHubIssueConfig,
    authorization: Option<HeaderValue>,
    cache: Arc<Mutex<SnapshotCache>>,
    prior_digest: Option<[u8; SNAPSHOT_DIGEST_BYTES]>,
    current_source: Option<CachedSource>,
    control: InvocationControl,
}

impl CapabilityModule for GitHubIssueModule {
    type Invocation = GitHubIssueInvocation;

    fn import_names(&self) -> &'static [&'static str] {
        &[IMPORT_NAME]
    }

    fn add_to_linker(
        &self,
        linker: &mut Linker<InvocationStore<Self::Invocation>>,
    ) -> Result<(), wasmtime::Error> {
        bindings::GithubIssueProvider::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
    }

    fn begin(&self, context: CapabilityContext<'_>) -> Self::Invocation {
        let prior_digest = match context.phase() {
            CapabilityPhase::Describe => None,
            CapabilityPhase::Observe(request) => request
                .prior_digest
                .as_ref()
                .map(|digest| *digest.as_bytes()),
        };
        GitHubIssueInvocation {
            config: self.config.clone(),
            authorization: self.authorization.clone(),
            cache: Arc::clone(&self.cache),
            prior_digest,
            current_source: None,
            control: context.control().clone(),
        }
    }
}

impl Host for InvocationStore<GitHubIssueInvocation> {
    fn get(&mut self, request: IssueRequest) -> Result<IssueResponse, IssueError> {
        self.capability_mut().get(request)
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), IssueError> {
        self.capability_mut().bind_snapshot(digest)
    }
}

impl GitHubIssueInvocation {
    fn get(&mut self, request: IssueRequest) -> Result<IssueResponse, IssueError> {
        run_on_runtime(self.get_async(request))
    }

    async fn get_async(&mut self, request: IssueRequest) -> Result<IssueResponse, IssueError> {
        if !valid_request(&request) {
            return Err(IssueError::Denied);
        }
        if let Some(reason) = self.control.interruption_reason() {
            return Err(interruption_error(reason));
        }
        let prior = match self.prior_digest.as_ref() {
            Some(digest) => self
                .cache
                .lock()
                .map_err(|_| IssueError::Unavailable)?
                .get(digest)
                .cloned(),
            None => None,
        };
        let deadline = Instant::now()
            .checked_add(self.config.total_timeout)
            .ok_or(IssueError::DeadlineExceeded)?;
        let address = resolve_public_api_address(&self.control, deadline).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.config.connect_timeout)
            .gzip(true)
            .resolve(API_HOST, address)
            .build()
            .map_err(|_| IssueError::Unavailable)?;
        let issue_endpoint = format!(
            "https://{API_HOST}/repos/{}/{}/issues/{}",
            request.owner, request.repo, request.number
        );
        let issue = self
            .fetch_object(
                &client,
                issue_endpoint,
                prior.as_ref().map(|source| &source.issue),
                deadline,
            )
            .await?;
        let metadata: IssueMetadata =
            serde_json::from_slice(&issue.object.body).map_err(|_| IssueError::Unavailable)?;

        let latest_comment = if metadata.comments == 0 {
            None
        } else {
            let cached = prior.as_ref().and_then(|source| {
                let previous: IssueMetadata = serde_json::from_slice(&source.issue.body).ok()?;
                (previous.comments == metadata.comments)
                    .then_some(source.latest_comment.as_ref())
                    .flatten()
            });
            let endpoint = format!(
                "https://{API_HOST}/repos/{}/{}/issues/{}/comments?per_page=1&page={}",
                request.owner, request.repo, request.number, metadata.comments
            );
            let mut fetched = self
                .fetch_object(&client, endpoint, cached, deadline)
                .await?;
            fetched.object.body = normalize_latest_comment(&fetched.object.body)?;
            Some(fetched)
        };

        if !issue.modified
            && latest_comment
                .as_ref()
                .is_none_or(|comment| !comment.modified)
        {
            return Ok(IssueResponse::NotModified);
        }
        let latest_object = latest_comment.map(|comment| comment.object);
        let observed_at = prior
            .as_ref()
            .filter(|prior| {
                prior.issue.body == issue.object.body
                    && prior.latest_comment.as_ref().map(|object| &object.body)
                        == latest_object.as_ref().map(|object| &object.body)
            })
            .map_or_else(
                || Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                |prior| prior.observed_at.clone(),
            );
        let source = CachedSource {
            issue: issue.object,
            latest_comment: latest_object,
            observed_at,
        };
        let current = source_to_wit(&source);
        self.current_source = Some(source);
        Ok(IssueResponse::Ok(SourceObservation {
            current,
            previous: prior.as_ref().map(source_to_wit),
        }))
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), IssueError> {
        let digest: [u8; SNAPSHOT_DIGEST_BYTES] =
            digest.try_into().map_err(|_| IssueError::Denied)?;
        let source = self.current_source.take().ok_or(IssueError::Unavailable)?;
        self.cache
            .lock()
            .map_err(|_| IssueError::Unavailable)?
            .insert(digest, source);
        Ok(())
    }

    async fn fetch_object(
        &self,
        client: &reqwest::Client,
        endpoint: String,
        cached: Option<&CachedObject>,
        deadline: Instant,
    ) -> Result<FetchedObject, IssueError> {
        if let Some(reason) = self.control.interruption_reason() {
            return Err(interruption_error(reason));
        }
        let mut builder = client
            .get(endpoint)
            .timeout(remaining(deadline)?)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "st2-github-resource-profile/1");
        if let Some(authorization) = self.authorization.clone() {
            builder = builder.header(reqwest::header::AUTHORIZATION, authorization);
        }
        if let Some(etag) = cached.and_then(|object| object.etag.as_deref()) {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let mut response = tokio::select! {
            biased;
            reason = wait_for_interruption(&self.control) => {
                return Err(interruption_error(reason));
            }
            response = builder.send() => response.map_err(map_transport_error)?,
        };
        let status = response.status();
        if status.is_redirection() && status.as_u16() != 304 {
            return Err(IssueError::Denied);
        }
        validate_headers(response.headers())?;
        let response_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| valid_etag(value))
            .map(str::to_owned);
        match status.as_u16() {
            304 => replay_not_modified(cached, response_etag),
            200 => Ok(FetchedObject {
                object: CachedObject {
                    etag: response_etag,
                    body: read_body(&mut response, &self.control).await?,
                },
                modified: true,
            }),
            401 | 403 | 404 => Err(IssueError::Denied),
            429 => Err(IssueError::ResourceExhausted),
            _ => Err(IssueError::Unavailable),
        }
    }
}

#[derive(Deserialize)]
struct IssueMetadata {
    comments: u64,
}

#[derive(Deserialize, Serialize)]
struct CommentMetadata {
    updated_at: String,
}

fn normalize_latest_comment(body: &[u8]) -> Result<Vec<u8>, IssueError> {
    let comments: Vec<CommentMetadata> =
        serde_json::from_slice(body).map_err(|_| IssueError::Unavailable)?;
    let [comment] = comments.as_slice() else {
        return Err(IssueError::Unavailable);
    };
    serde_json::to_vec(&[comment]).map_err(|_| IssueError::Unavailable)
}

struct FetchedObject {
    object: CachedObject,
    modified: bool,
}

fn replay_not_modified(
    cached: Option<&CachedObject>,
    response_etag: Option<String>,
) -> Result<FetchedObject, IssueError> {
    let cached = cached.ok_or(IssueError::Unavailable)?;
    let effective_etag = response_etag.or_else(|| cached.etag.clone());
    if cached.etag != effective_etag {
        return Err(IssueError::Unavailable);
    }
    Ok(FetchedObject {
        object: cached.clone(),
        modified: false,
    })
}

fn source_to_wit(source: &CachedSource) -> SourceSnapshot {
    SourceSnapshot {
        issue: object_to_wit(&source.issue),
        latest_comment: source.latest_comment.as_ref().map(object_to_wit),
        observed_at: source.observed_at.clone(),
    }
}

fn object_to_wit(object: &CachedObject) -> SourceObject {
    SourceObject {
        etag: object.etag.clone(),
        body: object.body.clone(),
    }
}

fn run_on_runtime<T>(future: impl Future<Output = Result<T, IssueError>>) -> Result<T, IssueError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| IssueError::Unavailable)?;
    runtime.block_on(future)
}

fn valid_request(request: &IssueRequest) -> bool {
    valid_component(&request.owner, 39) && valid_component(&request.repo, 100) && request.number > 0
}

fn valid_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_etag(value: &str) -> bool {
    if value.len() > MAX_ETAG_BYTES {
        return false;
    }
    let quoted = value.strip_prefix("W/").unwrap_or(value);
    let Some(inner) = quoted
        .strip_prefix('"')
        .and_then(|quoted| quoted.strip_suffix('"'))
    else {
        return false;
    };
    !inner.is_empty()
        && inner.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'+' | b'-')
        })
}

fn remaining(deadline: Instant) -> Result<Duration, IssueError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(IssueError::DeadlineExceeded)
}

fn validate_headers(headers: &reqwest::header::HeaderMap) -> Result<(), IssueError> {
    let bytes = headers.iter().try_fold(0_usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())
            .and_then(|total| total.checked_add(value.as_bytes().len()))
            .ok_or(IssueError::ResourceExhausted)
    })?;
    if bytes > MAX_HEADERS_BYTES {
        return Err(IssueError::ResourceExhausted);
    }
    Ok(())
}

async fn read_body(
    response: &mut reqwest::Response,
    control: &InvocationControl,
) -> Result<Vec<u8>, IssueError> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            reason = wait_for_interruption(control) => return Err(interruption_error(reason)),
            chunk = response.chunk() => chunk.map_err(map_transport_error)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > MAX_SOURCE_BYTES {
            return Err(IssueError::ResourceExhausted);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn wait_for_interruption(control: &InvocationControl) -> InterruptionReason {
    loop {
        if let Some(reason) = control.interruption_reason() {
            return reason;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn interruption_error(reason: InterruptionReason) -> IssueError {
    match reason {
        InterruptionReason::Cancelled => IssueError::Unavailable,
        InterruptionReason::TimedOut => IssueError::DeadlineExceeded,
    }
}

fn map_transport_error(error: reqwest::Error) -> IssueError {
    if error.is_timeout() {
        IssueError::DeadlineExceeded
    } else {
        IssueError::Unavailable
    }
}

async fn resolve_public_api_address(
    control: &InvocationControl,
    deadline: Instant,
) -> Result<SocketAddr, IssueError> {
    let resolver = hickory_resolver::Resolver::builder_tokio()
        .and_then(hickory_resolver::ResolverBuilder::build)
        .map_err(|_| IssueError::Unavailable)?;
    let lookup = tokio::select! {
        biased;
        reason = wait_for_interruption(control) => return Err(interruption_error(reason)),
        result = await_dns_lookup(remaining(deadline)?, resolver.lookup_ip(API_HOST)) => result?,
    };
    let mut addresses = lookup.iter();
    let first = addresses.next().ok_or(IssueError::Unavailable)?;
    if !is_public(first) || addresses.any(|address| !is_public(address)) {
        return Err(IssueError::Denied);
    }
    Ok(SocketAddr::new(first, API_PORT))
}

async fn await_dns_lookup<T, E>(
    timeout: Duration,
    lookup: impl Future<Output = Result<T, E>>,
) -> Result<T, IssueError> {
    tokio::time::timeout(timeout, lookup)
        .await
        .map_err(|_| IssueError::DeadlineExceeded)?
        .map_err(|_| IssueError::Unavailable)
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_unspecified()
                || address.is_multicast()
                || octets[0] == 0
                || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240)
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            let first = segments[0];
            let mapped_private = address
                .to_ipv4_mapped()
                .is_some_and(|mapped| !is_public(IpAddr::V4(mapped)));
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || mapped_private
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_accepts_dynamic_valid_subjects_and_rejects_invalid_components() {
        for request in [
            IssueRequest {
                owner: "example".into(),
                repo: "demo".into(),
                number: 1,
            },
            IssueRequest {
                owner: "other-owner".into(),
                repo: "private.repo".into(),
                number: 42,
            },
        ] {
            assert!(valid_request(&request));
        }
        for request in [
            IssueRequest {
                owner: "..".into(),
                repo: "demo".into(),
                number: 1,
            },
            IssueRequest {
                owner: "example".into(),
                repo: "demo/path".into(),
                number: 1,
            },
            IssueRequest {
                owner: "example".into(),
                repo: "demo".into(),
                number: 0,
            },
        ] {
            assert!(!valid_request(&request));
        }
    }

    #[test]
    fn etags_are_quoted_and_header_safe() {
        assert!(valid_etag("\"issue-v1\""));
        assert!(valid_etag("W/\"comment/v1:2\""));
        assert!(!valid_etag("issue-v1"));
        assert!(!valid_etag("\"bad header\""));
        assert!(!valid_etag("\"ok\"\r\nx-injected: true"));
    }

    #[test]
    fn latest_comment_source_exposes_only_updated_at_metadata() {
        let normalized = normalize_latest_comment(
            br#"[{"updated_at":"2026-08-30T11:22:33Z","body":"private discussion"}]"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&normalized).unwrap(),
            serde_json::json!([{"updated_at": "2026-08-30T11:22:33Z"}])
        );
    }

    #[test]
    fn not_modified_replays_only_the_exact_cached_etag_and_body() {
        let cached = CachedObject {
            etag: Some("\"issue-v1\"".into()),
            body: br#"{"comments":2}"#.to_vec(),
        };
        let replayed = replay_not_modified(Some(&cached), Some("\"issue-v1\"".into())).unwrap();
        assert!(!replayed.modified);
        assert_eq!(replayed.object.etag, cached.etag);
        assert_eq!(replayed.object.body, cached.body);
        assert!(replay_not_modified(Some(&cached), Some("\"issue-v2\"".into())).is_err());
        assert!(replay_not_modified(None, Some("\"issue-v1\"".into())).is_err());
    }

    #[test]
    fn private_special_and_documentation_addresses_are_never_admitted() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "192.0.2.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "ff02::1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(is_public("2606:4700:4700::1111".parse().unwrap()));
    }
}
