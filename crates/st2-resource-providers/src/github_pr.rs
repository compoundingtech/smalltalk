use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use st2_resource_wasip2::{
    CapabilityContext, CapabilityModule, CapabilityPhase, InterruptionReason, InvocationControl,
    InvocationStore,
};
use wasmtime::component::{HasSelf, Linker};

use crate::github_auth::discover_authorization;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/github-pr",
        world: "github-pr-provider",
    });
}

use bindings::compoundingtech::st2_github_pr::github_pr::{
    Host, PullRequestError, PullRequestRequest, SourceObservation, SourceSnapshot,
};

const IMPORT_NAME: &str = "compoundingtech:st2-github-pr/github-pr@0.1.0";
const API_HOST: &str = "api.github.com";
const API_PORT: u16 = 443;
const MAX_HEADERS_BYTES: usize = 16 * 1024;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const SNAPSHOT_DIGEST_BYTES: usize = 32;
const MAX_CACHED_SNAPSHOTS: usize = 16;
const PULL_REQUEST_QUERY: &str = r#"query PullRequestObservation($owner: String!, $repository: String!, $number: Int!) {
  repository(owner: $owner, name: $repository) {
    pullRequest(number: $number) {
      url
      title
      body
      state
      isDraft
      merged
      mergedAt
      closedAt
      mergeable
      author { login }
      headRefOid
      headRefName
      baseRefName
      reviewDecision
      reviewRequests(first: 100) {
        totalCount
        nodes {
          requestedReviewer {
            __typename
            ... on User { login }
            ... on Team { slug }
            ... on Bot { login }
            ... on Mannequin { login }
          }
        }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              state
              contexts(first: 100) {
                totalCount
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    status
                    conclusion
                    detailsUrl
                  }
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                    description
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPrConfig {
    pub auth_executable: PathBuf,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
}

impl GitHubPrConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.auth_executable.is_absolute() {
            return Err("GitHub authentication executable must be absolute");
        }
        if self.connect_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
            || self.total_timeout > Duration::from_secs(60)
        {
            return Err("GitHub pull request deadlines are invalid");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct GitHubPrModule {
    config: GitHubPrConfig,
    authorization: Option<HeaderValue>,
    cache: Arc<Mutex<SnapshotCache>>,
}

impl GitHubPrModule {
    pub fn new(config: GitHubPrConfig) -> Result<Self, &'static str> {
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
struct CachedSource {
    graphql_data: Vec<u8>,
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

pub struct GitHubPrInvocation {
    config: GitHubPrConfig,
    authorization: Option<HeaderValue>,
    cache: Arc<Mutex<SnapshotCache>>,
    prior_digest: Option<[u8; SNAPSHOT_DIGEST_BYTES]>,
    current_source: Option<CachedSource>,
    control: InvocationControl,
}

impl CapabilityModule for GitHubPrModule {
    type Invocation = GitHubPrInvocation;

    fn import_names(&self) -> &'static [&'static str] {
        &[IMPORT_NAME]
    }

    fn add_to_linker(
        &self,
        linker: &mut Linker<InvocationStore<Self::Invocation>>,
    ) -> Result<(), wasmtime::Error> {
        bindings::GithubPrProvider::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
    }

    fn begin(&self, context: CapabilityContext<'_>) -> Self::Invocation {
        let prior_digest = match context.phase() {
            CapabilityPhase::Describe => None,
            CapabilityPhase::Observe(request) => request
                .prior_digest
                .as_ref()
                .map(|digest| *digest.as_bytes()),
        };
        GitHubPrInvocation {
            config: self.config.clone(),
            authorization: self.authorization.clone(),
            cache: Arc::clone(&self.cache),
            prior_digest,
            current_source: None,
            control: context.control().clone(),
        }
    }
}

impl Host for InvocationStore<GitHubPrInvocation> {
    fn get(&mut self, request: PullRequestRequest) -> Result<SourceObservation, PullRequestError> {
        self.capability_mut().get(request)
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), PullRequestError> {
        self.capability_mut().bind_snapshot(digest)
    }
}

impl GitHubPrInvocation {
    fn get(&mut self, request: PullRequestRequest) -> Result<SourceObservation, PullRequestError> {
        run_on_runtime(self.get_async(request))
    }

    async fn get_async(
        &mut self,
        request: PullRequestRequest,
    ) -> Result<SourceObservation, PullRequestError> {
        if !valid_request(&request) {
            return Err(PullRequestError::Denied);
        }
        let authorization = self
            .authorization
            .clone()
            .ok_or(PullRequestError::AuthenticationRequired)?;
        if let Some(reason) = self.control.interruption_reason() {
            return Err(interruption_error(reason));
        }
        let prior = match self.prior_digest.as_ref() {
            Some(digest) => self
                .cache
                .lock()
                .map_err(|_| PullRequestError::Unavailable)?
                .get(digest)
                .cloned(),
            None => None,
        };
        let number = i32::try_from(request.number).map_err(|_| PullRequestError::Denied)?;
        let deadline = Instant::now()
            .checked_add(self.config.total_timeout)
            .ok_or(PullRequestError::DeadlineExceeded)?;
        let address = resolve_public_api_address(&self.control, deadline).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.config.connect_timeout)
            .gzip(true)
            .resolve(API_HOST, address)
            .build()
            .map_err(|_| PullRequestError::Unavailable)?;
        let remaining = remaining(deadline)?;
        let body = serde_json::to_vec(&json!({
            "query": PULL_REQUEST_QUERY,
            "variables": {
                "owner": request.owner,
                "repository": request.repo,
                "number": number,
            }
        }))
        .map_err(|_| PullRequestError::Unavailable)?;
        let builder = client
            .post(format!("https://{API_HOST}/graphql"))
            .timeout(remaining)
            .header("accept", "application/vnd.github+json")
            .header("content-type", "application/json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "st2-github-resource-profile/1")
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(body);
        let mut response = tokio::select! {
            biased;
            reason = wait_for_interruption(&self.control) => {
                return Err(interruption_error(reason));
            }
            response = builder.send() => response.map_err(map_transport_error)?,
        };
        let status = response.status();
        if status.is_redirection() {
            return Err(PullRequestError::Denied);
        }
        validate_headers(response.headers())?;
        match status.as_u16() {
            200 => {}
            401 | 403 => return Err(PullRequestError::AuthenticationRequired),
            404 => return Err(PullRequestError::Denied),
            429 => return Err(PullRequestError::ResourceExhausted),
            _ => return Err(PullRequestError::Unavailable),
        }
        let bytes = read_body(&mut response, &self.control, MAX_SOURCE_BYTES).await?;
        let mut envelope: Value =
            serde_json::from_slice(&bytes).map_err(|_| PullRequestError::Unavailable)?;
        let envelope = envelope
            .as_object_mut()
            .ok_or(PullRequestError::Unavailable)?;
        if let Some(errors) = envelope.get("errors") {
            let errors = errors.as_array().ok_or(PullRequestError::Unavailable)?;
            if !errors.is_empty() {
                return Err(PullRequestError::Unavailable);
            }
        }
        let data = envelope
            .remove("data")
            .filter(Value::is_object)
            .ok_or(PullRequestError::Unavailable)?;
        let graphql_data = serde_json::to_vec(&data).map_err(|_| PullRequestError::Unavailable)?;
        let observed_at = prior
            .as_ref()
            .filter(|prior| prior.graphql_data == graphql_data)
            .map_or_else(
                || Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                |prior| prior.observed_at.clone(),
            );
        let source = CachedSource {
            graphql_data,
            observed_at,
        };
        let current = source_to_wit(&source);
        self.current_source = Some(source);
        Ok(SourceObservation {
            current,
            previous: prior.as_ref().map(source_to_wit),
        })
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), PullRequestError> {
        let digest: [u8; SNAPSHOT_DIGEST_BYTES] =
            digest.try_into().map_err(|_| PullRequestError::Denied)?;
        let source = self
            .current_source
            .take()
            .ok_or(PullRequestError::Unavailable)?;
        self.cache
            .lock()
            .map_err(|_| PullRequestError::Unavailable)?
            .insert(digest, source);
        Ok(())
    }
}

fn source_to_wit(source: &CachedSource) -> SourceSnapshot {
    SourceSnapshot {
        graphql_data: source.graphql_data.clone(),
        observed_at: source.observed_at.clone(),
    }
}

fn run_on_runtime<T>(
    future: impl Future<Output = Result<T, PullRequestError>>,
) -> Result<T, PullRequestError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| PullRequestError::Unavailable)?;
    runtime.block_on(future)
}

fn valid_request(request: &PullRequestRequest) -> bool {
    valid_component(&request.owner, 39)
        && valid_component(&request.repo, 100)
        && request.number > 0
        && request.number <= i32::MAX as u64
}

fn valid_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn remaining(deadline: Instant) -> Result<Duration, PullRequestError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(PullRequestError::DeadlineExceeded)
}

fn validate_headers(headers: &reqwest::header::HeaderMap) -> Result<(), PullRequestError> {
    let bytes = headers.iter().try_fold(0_usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())
            .and_then(|total| total.checked_add(value.as_bytes().len()))
            .ok_or(PullRequestError::ResourceExhausted)
    })?;
    if bytes > MAX_HEADERS_BYTES {
        return Err(PullRequestError::ResourceExhausted);
    }
    Ok(())
}

async fn read_body(
    response: &mut reqwest::Response,
    control: &InvocationControl,
    maximum: usize,
) -> Result<Vec<u8>, PullRequestError> {
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
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(PullRequestError::ResourceExhausted);
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

fn interruption_error(reason: InterruptionReason) -> PullRequestError {
    match reason {
        InterruptionReason::Cancelled => PullRequestError::Unavailable,
        InterruptionReason::TimedOut => PullRequestError::DeadlineExceeded,
    }
}

fn map_transport_error(error: reqwest::Error) -> PullRequestError {
    if error.is_timeout() {
        PullRequestError::DeadlineExceeded
    } else {
        PullRequestError::Unavailable
    }
}

async fn resolve_public_api_address(
    control: &InvocationControl,
    deadline: Instant,
) -> Result<SocketAddr, PullRequestError> {
    let resolver = hickory_resolver::Resolver::builder_tokio()
        .and_then(hickory_resolver::ResolverBuilder::build)
        .map_err(|_| PullRequestError::Unavailable)?;
    let lookup = tokio::select! {
        biased;
        reason = wait_for_interruption(control) => return Err(interruption_error(reason)),
        result = await_dns_lookup(remaining(deadline)?, resolver.lookup_ip(API_HOST)) => result?,
    };
    let mut addresses = lookup.iter();
    let first = addresses.next().ok_or(PullRequestError::Unavailable)?;
    if !is_public(first) || addresses.any(|address| !is_public(address)) {
        return Err(PullRequestError::Denied);
    }
    Ok(SocketAddr::new(first, API_PORT))
}

async fn await_dns_lookup<T, E>(
    timeout: Duration,
    lookup: impl Future<Output = Result<T, E>>,
) -> Result<T, PullRequestError> {
    tokio::time::timeout(timeout, lookup)
        .await
        .map_err(|_| PullRequestError::DeadlineExceeded)?
        .map_err(|_| PullRequestError::Unavailable)
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
            PullRequestRequest {
                owner: "example".into(),
                repo: "demo".into(),
                number: 1,
            },
            PullRequestRequest {
                owner: "other-owner".into(),
                repo: "private.repo".into(),
                number: 389,
            },
        ] {
            assert!(valid_request(&request));
        }
        for request in [
            PullRequestRequest {
                owner: "..".into(),
                repo: "demo".into(),
                number: 1,
            },
            PullRequestRequest {
                owner: "example".into(),
                repo: "demo/path".into(),
                number: 1,
            },
            PullRequestRequest {
                owner: "example".into(),
                repo: "demo".into(),
                number: 0,
            },
            PullRequestRequest {
                owner: "example".into(),
                repo: "demo".into(),
                number: i32::MAX as u64 + 1,
            },
        ] {
            assert!(!valid_request(&request));
        }
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

    #[test]
    fn graphql_operation_is_fixed_and_bounded() {
        assert_eq!(
            PULL_REQUEST_QUERY
                .matches("pullRequest(number: $number)")
                .count(),
            1
        );
        assert_eq!(
            PULL_REQUEST_QUERY.matches("contexts(first: 100)").count(),
            1
        );
        assert!(!PULL_REQUEST_QUERY.contains("$cursor"));
    }
}
