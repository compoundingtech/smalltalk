//! Authorized requests: the st daemon (or an st command such as a gate) makes a GitHub API
//! request through the gateway, which adds the profile's token and returns the response. The
//! caller never holds the token. This is one request and one response over the gateway socket,
//! not a proxy: only the profile's API base (GitHub's API) is reachable, redirects are never
//! followed, and nothing is retried.
//!
//! The caller is `host/NODE`: it signs a statement for its own process with the node key the
//! person registered with `st sekrets enable`, and the person grants the profile to that node.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::identity::{self, STATEMENT_VERSION, Statement};
use super::protocol::{Attestation, CallerView, Reply, Request};
use crate::config::Config;

/// The largest request body the gateway forwards.
pub const MAX_REQUEST_BODY: usize = 16 << 20;
/// The largest response body the gateway returns.
pub const MAX_RESPONSE_BODY: usize = 64 << 20;
/// The name a policy rule uses for GitHub's API: `http METHOD github`.
pub const GITHUB_TARGET: &str = "github";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorizedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorizedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorizedError {
    /// No gateway answers here, or this node has no identity to call it with.
    Unavailable(String),
    /// The gateway refused: identity, grant, policy, URL or a header.
    Refused(String),
    /// The gateway tried and the request failed: DNS, TLS, a timeout or a body too large.
    Transport(String),
}

impl std::fmt::Display for AuthorizedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "sekrets is unavailable: {reason}"),
            Self::Refused(reason) => write!(f, "sekrets refused the request: {reason}"),
            Self::Transport(reason) => write!(f, "the request failed: {reason}"),
        }
    }
}

impl std::error::Error for AuthorizedError {}

/// One request as it crosses the gateway socket; bodies are base64.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedCall {
    pub profile: String,
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
}

/// Request headers the gateway never forwards: its own credential, and hop-by-hop headers.
const DROPPED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// The headers to forward, or why the request carries a header it must not.
pub fn forwarded_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    let mut forwarded = Vec::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower == "authorization" || lower == "cookie" {
            return Err(format!(
                "the request carries its own `{name}` header; the gateway adds the credential"
            ));
        }
        if DROPPED_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        forwarded.push((name.clone(), value.clone()));
    }
    Ok(forwarded)
}

/// Whether `url` lies under the API base `base`: same scheme, host and port, no user name or
/// password, and a path under the base's.
pub fn check_url(url: &str, base: &str) -> Result<reqwest::Url, String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("`{url}` is not a URL: {error}"))?;
    let base =
        reqwest::Url::parse(base).map_err(|error| format!("the API base is not a URL: {error}"))?;
    let same_origin = parsed.scheme() == base.scheme()
        && parsed.host_str() == base.host_str()
        && parsed.port_or_known_default() == base.port_or_known_default();
    let base_path = base.path().trim_end_matches('/');
    let under = parsed.path() == base_path
        || base_path.is_empty()
        || parsed.path().starts_with(&format!("{base_path}/"));
    if !same_origin || !under || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "{url} is not under {base}; the profile authorizes requests to its API only"
        ));
    }
    Ok(parsed)
}

/// This node's name and the key it signs with: the fleet member key, or the standalone node
/// key, as the daemon loads them. Never creates one.
fn node_identity(config: &Config) -> Result<(String, smallclaims::fleet::MemberKey), String> {
    let mut config = config.clone();
    if config.fleet.is_none() {
        config
            .apply_fleet_file()
            .map_err(|error| format!("read the fleet file: {error:#}"))?;
    }
    let path = match &config.fleet {
        Some(file) => file.node_key_path(&config.state_dir),
        None => crate::fleet::join::key_directory(&config.state_dir).join("node.key"),
    };
    let key = smallclaims::fleet::MemberKey::load(&path)
        .map_err(|error| format!("this node has no key at {}: {error:#}", path.display()))?;
    Ok((config.node.clone(), key))
}

/// Make `request` with `profile`'s credential, through this host's gateway. Blocking: call it
/// from `spawn_blocking`. One request, one response; nothing is retried. A 4xx or 5xx is a
/// response, not an error.
pub fn authorized_request(
    config: &Config,
    profile: &str,
    request: &AuthorizedRequest,
) -> Result<AuthorizedResponse, AuthorizedError> {
    authorized_request_at(&super::client::socket_path(), config, profile, request)
}

pub fn authorized_request_at(
    socket: &Path,
    config: &Config,
    profile: &str,
    request: &AuthorizedRequest,
) -> Result<AuthorizedResponse, AuthorizedError> {
    let pid = std::process::id() as i32;
    let cgroup = identity::process_cgroup(pid).unwrap_or_default();
    request_from_cgroup(socket, config, profile, request, cgroup)
}

/// The request, naming `cgroup` as this process's in its statement; tests stand in for /proc.
pub(crate) fn request_from_cgroup(
    socket: &Path,
    config: &Config,
    profile: &str,
    request: &AuthorizedRequest,
    cgroup: String,
) -> Result<AuthorizedResponse, AuthorizedError> {
    if request.body.len() > MAX_REQUEST_BODY {
        return Err(AuthorizedError::Refused(format!(
            "the request body is {} bytes, over {MAX_REQUEST_BODY}",
            request.body.len()
        )));
    }
    let mut connection = super::client::Connection::open(socket)
        .map_err(|error| AuthorizedError::Unavailable(format!("{error:#}")))?;
    if !matches!(connection.caller, CallerView::Person { .. }) {
        let person = config.person.clone().ok_or_else(|| {
            AuthorizedError::Unavailable("set `person` in st's configuration".into())
        })?;
        let (node, key) = node_identity(config).map_err(AuthorizedError::Unavailable)?;
        let pid = std::process::id() as i32;
        let statement = Statement {
            version: STATEMENT_VERSION,
            node: format!("host/{node}"),
            person,
            agent: format!("host/{node}"),
            revision: None,
            cgroup,
            pid,
            pid_start: identity::process_start(pid).unwrap_or_default(),
            nonce: connection.nonce.clone(),
            issued_at_unix_ms: super::store::now_unix_ms(),
        };
        let text = serde_json::to_string(&statement)
            .map_err(|error| AuthorizedError::Unavailable(error.to_string()))?;
        let signature = key.sign(&identity::signing_message(&text));
        connection
            .hello(Some(Attestation {
                statement: text,
                signature,
            }))
            .map_err(|error| AuthorizedError::Unavailable(format!("{error:#}")))?;
    }
    let call = AuthorizedCall {
        profile: profile.to_owned(),
        method: request.method.clone(),
        url: request.url.clone(),
        headers: request.headers.clone(),
        body: base64::engine::general_purpose::STANDARD.encode(&request.body),
    };
    match connection.call(&Request::Authorized(call)) {
        Ok(Reply::Response {
            status,
            headers,
            body,
        }) => Ok(AuthorizedResponse {
            status,
            headers,
            body: base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|error| AuthorizedError::Transport(error.to_string()))?,
        }),
        Ok(Reply::Refused { reason, .. }) => Err(AuthorizedError::Refused(reason)),
        Ok(Reply::Error { message }) => Err(AuthorizedError::Transport(message)),
        Ok(other) => Err(AuthorizedError::Transport(format!(
            "unexpected reply from the gateway: {other:?}"
        ))),
        Err(error) => Err(AuthorizedError::Unavailable(format!("{error:#}"))),
    }
}

/// The gateway's HTTP client: no redirects, a 60 second limit, and its own runtime.
pub struct Http {
    pub runtime: tokio::runtime::Runtime,
    pub client: reqwest::Client,
}

impl Http {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sekrets-http")
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(60))
            .user_agent("st-sekrets")
            .build()?;
        Ok(Arc::new(Self { runtime, client }))
    }

    /// Send one request; the body is read whole, up to the limit.
    pub fn send(
        &self,
        method: &str,
        url: reqwest::Url,
        headers: &[(String, String)],
        token: &str,
        body: Vec<u8>,
    ) -> Result<AuthorizedResponse, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("`{method}` is not an HTTP method"))?;
        self.runtime.block_on(async {
            let mut request = self.client.request(method, url).bearer_auth(token);
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if !body.is_empty() {
                request = request.body(body);
            }
            let mut response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                if body.len() + chunk.len() > MAX_RESPONSE_BODY {
                    return Err(format!(
                        "the response body is over {MAX_RESPONSE_BODY} bytes"
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(AuthorizedResponse {
                status,
                headers,
                body,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_api_base_is_reachable() {
        let base = "https://api.github.com";
        assert!(check_url("https://api.github.com/repos/example/web", base).is_ok());
        assert!(check_url("https://api.github.com/graphql", base).is_ok());
        for url in [
            "https://github.com/login",
            "http://api.github.com/repos",
            "https://api.github.com:8443/repos",
            "https://user@api.github.com/repos",
            "https://api.github.com.example.com/repos",
            "not a url",
        ] {
            assert!(check_url(url, base).is_err(), "{url}");
        }
        assert!(check_url("http://127.0.0.1:9/api/v3/x", "http://127.0.0.1:9/api/v3").is_ok());
        assert!(check_url("http://127.0.0.1:9/api/v30", "http://127.0.0.1:9/api/v3").is_err());
    }

    #[test]
    fn the_caller_brings_no_credential_and_hop_by_hop_headers_go() {
        let given = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect::<Vec<_>>()
        };
        let forwarded = forwarded_headers(&given(&[
            ("If-None-Match", "\"etag\""),
            ("Accept", "application/vnd.github+json"),
            ("Connection", "close"),
            ("Host", "elsewhere"),
        ]))
        .unwrap();
        assert_eq!(
            forwarded,
            given(&[
                ("If-None-Match", "\"etag\""),
                ("Accept", "application/vnd.github+json")
            ])
        );
        assert!(forwarded_headers(&given(&[("authorization", "Bearer x")])).is_err());
        assert!(forwarded_headers(&given(&[("Cookie", "x")])).is_err());
    }
}
