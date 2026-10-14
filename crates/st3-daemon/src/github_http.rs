//! Authentication and the one send path for daemon GitHub HTTP requests.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::SystemTime;
use tokio::time::Instant;

use anyhow::{Context as _, Result, bail};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

/// An acquired credential retains its identity so a late 401 cannot evict a newer acquisition.
/// Deliberately has no Debug implementation: credentials never belong in diagnostics.
#[derive(Clone)]
pub(crate) enum GithubAuth {
    Local {
        token: Arc<String>,
        cache: Option<Arc<TokenCache>>,
    },
    Authorized {
        config: Arc<crate::config::Config>,
        profile: String,
    },
}

impl GithubAuth {
    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Local { token, .. } => !token.trim().is_empty(),
            Self::Authorized { profile, .. } => !profile.trim().is_empty(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test(token: &str) -> Self {
        Self::Local {
            token: Arc::new(token.into()),
            cache: None,
        }
    }

    #[cfg(test)]
    fn local_token(&self) -> &Arc<String> {
        match self {
            Self::Local { token, .. } => token,
            Self::Authorized { .. } => panic!("profile auth has no local token"),
        }
    }
}

#[derive(Default)]
struct TokenState {
    token: Option<Arc<String>>,
    retry_at: Option<Instant>,
    file_version: Option<FileVersion>,
    default_version: Option<DefaultVersion>,
}

#[derive(PartialEq, Eq)]
struct FileVersion {
    modified: SystemTime,
    device: u64,
    inode: u64,
    length: u64,
}

#[derive(PartialEq, Eq)]
enum DefaultVersion {
    Exported([u8; 32]),
    Gh {
        path: Option<std::path::PathBuf>,
        version: Option<FileVersion>,
        host: Option<String>,
    },
}

fn default_version(environment: &BTreeMap<String, String>) -> Result<DefaultVersion> {
    if let Some(token) = ["GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .filter_map(|key| environment.get(key))
        .find(|token| !token.trim().is_empty())
    {
        return Ok(DefaultVersion::Exported(
            Sha256::digest(token.as_bytes()).into(),
        ));
    }
    let value = |key: &str| environment.get(key).filter(|value| !value.is_empty());
    let path = value("GH_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| value("XDG_CONFIG_HOME").map(|home| Path::new(home).join("gh")))
        .or_else(|| value("HOME").map(|home| Path::new(home).join(".config/gh")))
        .map(|dir| dir.join("hosts.yml"));
    let version = match path.as_ref().map(std::fs::metadata) {
        Some(Ok(metadata)) => Some(FileVersion {
            modified: metadata
                .modified()
                .context("inspect gh credential modification time")?,
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
        }),
        Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("inspect gh credential file metadata");
        }
        _ => None,
    };
    Ok(DefaultVersion::Gh {
        path,
        version,
        host: value("GH_HOST").cloned(),
    })
}

/// Validate the opened descriptor before reading its contents, including on every reload.
/// Nonblocking open lets a configured FIFO fail validation without hanging startup.
pub(crate) fn open_token_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open GitHub token file {}", path.display()))?;
    let metadata = file.metadata().context("inspect GitHub token file")?;
    validate_token_file_metadata(&metadata, unsafe { libc::geteuid() })?;
    Ok(file)
}

fn validate_token_file_metadata(metadata: &std::fs::Metadata, effective_uid: u32) -> Result<()> {
    anyhow::ensure!(
        metadata.is_file(),
        "GitHub token file must be a regular file"
    );
    anyhow::ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "GitHub token file must grant no permissions to group or others; use chmod 600"
    );
    anyhow::ensure!(
        metadata.uid() == effective_uid,
        "GitHub token file must be owned by the daemon account"
    );
    Ok(())
}

fn read_token_file(file: File) -> Result<String> {
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .context("read GitHub token file")?;
    anyhow::ensure!(bytes.len() <= 65_536, "GitHub token file is too large");
    let contents = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("GitHub token file must contain UTF-8"))?;
    let token = contents.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(
        !token.is_empty() && !token.chars().any(char::is_whitespace),
        "GitHub token file must contain only one nonempty token"
    );
    Ok(token.to_owned())
}

#[derive(Default)]
pub(crate) struct TokenCache(Mutex<TokenState>);

// Failed logins or a permanently rejected exported token must not spawn one process per caller.
const AUTH_REFRESH_BACKOFF: Duration = Duration::from_secs(30);

impl TokenCache {
    #[cfg(test)]
    async fn acquire<F, Fut>(self: &Arc<Self>, lookup: F) -> Result<GithubAuth>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let mut state = self.0.lock().await;
        self.resolve(&mut state, lookup).await
    }

    async fn acquire_default<F, Fut>(self: &Arc<Self>, environment: F) -> Result<GithubAuth>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<BTreeMap<String, String>>>,
    {
        let mut state = self.0.lock().await;
        // Snapshot under the acquisition lock so an older environment snapshot cannot
        // overwrite a newer credential. The environment itself is cached by its provider.
        let environment = environment().await?;
        let for_metadata = environment.clone();
        let version = tokio::task::spawn_blocking(move || default_version(&for_metadata)).await??;
        if state.default_version.as_ref() != Some(&version) {
            state.token = None;
            state.retry_at = None;
            state.default_version = Some(version);
        }
        self.resolve(&mut state, || lookup_github_token(&environment))
            .await
    }

    async fn resolve<F, Fut>(
        self: &Arc<Self>,
        state: &mut TokenState,
        lookup: F,
    ) -> Result<GithubAuth>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        if let Some(token) = &state.token {
            return Ok(GithubAuth::Local {
                token: token.clone(),
                cache: Some(self.clone()),
            });
        }
        if state.retry_at.is_some_and(|at| at > Instant::now()) {
            bail!("GitHub credential refresh is backing off; try the next poll");
        }
        // Cancellation releases the acquisition lock without suppressing the next lookup.
        // Only a completed failure starts the backoff.
        let token = match lookup().await {
            Ok(token) => Arc::new(token),
            Err(error) => {
                state.retry_at = Some(Instant::now() + AUTH_REFRESH_BACKOFF);
                return Err(error);
            }
        };
        state.token = Some(token.clone());
        state.retry_at = None;
        Ok(GithubAuth::Local {
            token,
            cache: Some(self.clone()),
        })
    }

    async fn acquire_file(self: &Arc<Self>, path: &Path) -> Result<GithubAuth> {
        let mut state = self.0.lock().await;
        let path = path.to_owned();
        let (file, version) = tokio::task::spawn_blocking(move || -> Result<_> {
            let file = open_token_file(&path)?;
            let metadata = file.metadata().context("inspect GitHub token file")?;
            let version = FileVersion {
                modified: metadata
                    .modified()
                    .context("read GitHub token file mtime")?,
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.len(),
            };
            Ok((file, version))
        })
        .await??;
        if state.file_version.as_ref() != Some(&version) {
            state.token = None;
            state.retry_at = None;
            state.file_version = Some(version);
        }
        self.resolve(&mut state, || async {
            tokio::task::spawn_blocking(move || read_token_file(file)).await?
        })
        .await
    }

    async fn invalidate(&self, auth: &GithubAuth) {
        let GithubAuth::Local { token, .. } = auth else {
            return;
        };
        let mut state = self.0.lock().await;
        if state
            .token
            .as_ref()
            .is_some_and(|known| Arc::ptr_eq(known, token))
        {
            state.token = None;
            state.retry_at = Some(Instant::now() + AUTH_REFRESH_BACKOFF);
        }
    }
}

fn token_cache() -> &'static Arc<TokenCache> {
    static CACHE: OnceLock<Arc<TokenCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

static SOURCE: OnceLock<Arc<crate::config::Config>> = OnceLock::new();

pub(crate) fn configure(config: &crate::config::Config) -> Result<()> {
    config.github.validate()?;
    if let Some(known) = SOURCE.get() {
        anyhow::ensure!(
            known.as_ref() == config,
            "GitHub source was already configured for this process"
        );
        return Ok(());
    }
    SOURCE
        .set(Arc::new(config.clone()))
        .map_err(|_| anyhow::anyhow!("GitHub source was already configured"))
}

async fn auth_for_config<F, Fut>(
    config: Arc<crate::config::Config>,
    cache: &Arc<TokenCache>,
    lookup: F,
) -> Result<GithubAuth>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<BTreeMap<String, String>>>,
{
    anyhow::ensure!(
        config.github.token_file.is_none() || config.github.sekrets_profile.is_none(),
        "github.token_file and github.sekrets_profile are mutually exclusive"
    );
    if let Some(profile) = &config.github.sekrets_profile {
        anyhow::ensure!(
            !profile.trim().is_empty() && profile.trim() == profile,
            "github.sekrets_profile must name a non-empty profile without surrounding whitespace"
        );
        return Ok(GithubAuth::Authorized {
            profile: profile.clone(),
            config,
        });
    }
    auth_for(&config.github, cache, lookup).await
}

async fn auth_for<F, Fut>(
    config: &crate::config::GithubConfig,
    cache: &Arc<TokenCache>,
    lookup: F,
) -> Result<GithubAuth>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<BTreeMap<String, String>>>,
{
    anyhow::ensure!(
        config.token_file.is_none() || config.sekrets_profile.is_none(),
        "github.token_file and github.sekrets_profile are mutually exclusive"
    );
    anyhow::ensure!(
        config.sekrets_profile.is_none(),
        "profile authentication must use the authorized-request path; no fallback credentials were used"
    );
    if let Some(path) = &config.token_file {
        return cache.acquire_file(path).await;
    }
    cache
        .acquire_default(lookup)
        .await
        .context(crate::resource::GITHUB_AUTH_REMEDY)
}

pub(crate) async fn github_auth() -> Result<GithubAuth> {
    let config = configured_source(&SOURCE)?;
    auth_for_config(config.clone(), token_cache(), || async {
        tokio::task::spawn_blocking(crate::environment::snapshot).await?
    })
    .await
}

fn configured_source(
    source: &OnceLock<Arc<crate::config::Config>>,
) -> Result<&Arc<crate::config::Config>> {
    source
        .get()
        .context("GitHub credential source was not configured for this process")
}

pub(crate) async fn lookup_github_token(environment: &BTreeMap<String, String>) -> Result<String> {
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
        Duration::from_secs(10),
        command.args(["auth", "token"]).kill_on_drop(true).output(),
    )
    .await
    .context("GitHub credential lookup timed out")??;
    anyhow::ensure!(output.status.success(), "gh auth token failed");
    let token = String::from_utf8(output.stdout).context("gh returned a non-UTF-8 token")?;
    let token = token.trim();
    anyhow::ensure!(!token.is_empty(), "gh returned an empty token");
    Ok(token.to_owned())
}

/// Send exactly once, including mutations. A 401 invalidates only the credential used here;
/// another poll can acquire again after backoff. Never retry the original HTTP request.
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    auth: &GithubAuth,
) -> Result<reqwest::Response> {
    match auth {
        GithubAuth::Authorized { config, profile } => {
            send_authorized_with(
                config.clone(),
                profile,
                request,
                crate::sekrets::authorized::authorized_request,
            )
            .await
        }
        GithubAuth::Local { token, cache } => {
            let response = request.bearer_auth(token.as_str()).send().await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && let Some(cache) = cache
            {
                cache.invalidate(auth).await;
            }
            Ok(response)
        }
    }
}

async fn send_authorized_with<F>(
    config: Arc<crate::config::Config>,
    profile: &str,
    builder: reqwest::RequestBuilder,
    transport: F,
) -> Result<reqwest::Response>
where
    F: FnOnce(
            &crate::config::Config,
            &str,
            &crate::sekrets::authorized::AuthorizedRequest,
        ) -> std::result::Result<
            crate::sekrets::authorized::AuthorizedResponse,
            crate::sekrets::authorized::AuthorizedError,
        > + Send
        + 'static,
{
    use crate::sekrets::authorized::{AuthorizedRequest, MAX_REQUEST_BODY, MAX_RESPONSE_BODY};
    use reqwest::ResponseBuilderExt as _;
    // Client defaults are applied by reqwest only when sending; this path builds for the RPC.
    let mut request = builder.build()?;
    request
        .headers_mut()
        .entry(reqwest::header::USER_AGENT)
        .or_insert(reqwest::header::HeaderValue::from_static(
            "st3-resource-observer/0.1",
        ));
    let url = request.url().clone();
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443),
        "GitHub authorized requests require https://api.github.com with no userinfo or non-default port"
    );
    anyhow::ensure!(
        !request
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION),
        "GitHub authorized requests must not carry their own Authorization header"
    );
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| Ok((name.as_str().to_owned(), value.to_str()?.to_owned())))
        .collect::<Result<Vec<_>>>()?;
    let body = request
        .body()
        .map(|body| {
            let bytes = body.as_bytes().context(
                "GitHub authorized request requires a buffered body; streamed bodies are refused",
            )?;
            anyhow::ensure!(
                bytes.len() <= MAX_REQUEST_BODY,
                "GitHub authorized request body exceeds 16 MiB"
            );
            Ok::<_, anyhow::Error>(bytes.to_vec())
        })
        .transpose()?
        .unwrap_or_default();
    let wire = AuthorizedRequest {
        method: request.method().as_str().to_owned(),
        url: url.as_str().to_owned(),
        headers,
        body,
    };
    let profile = profile.to_owned();
    let response =
        tokio::task::spawn_blocking(move || transport(&config, &profile, &wire)).await??;
    anyhow::ensure!(
        response.body.len() <= MAX_RESPONSE_BODY,
        "GitHub authorized response body exceeds 64 MiB"
    );
    let mut raw = axum::http::Response::builder()
        .status(response.status)
        .url(url);
    for (name, value) in response.headers {
        raw.headers_mut()
            .context("invalid authorized response status")?
            .append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                reqwest::header::HeaderValue::from_str(&value)?,
            );
    }
    Ok(reqwest::Response::from(raw.body(response.body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn authorized_destination_body_and_header_validation_refuses_before_the_rpc() {
        let client = reqwest::Client::new();
        for url in [
            "http://api.github.com/repos",
            "https://github.com/repos",
            "https://api.github.com.evil.example/repos",
            "https://api.github.com:8443/repos",
            "https://user@api.github.com/repos",
            "https://user:password@api.github.com/repos",
            "https://api.github.com./repos",
            "https://127.0.0.1/repos",
        ] {
            let error = send_authorized_with(
                Arc::new(crate::config::Config::default()),
                "owner/daemon-gh",
                client.get(url),
                |_, _, _| panic!("foreign destination reached RPC"),
            )
            .await
            .err()
            .unwrap();
            let message = error.to_string();
            // reqwest moves URL userinfo into an Authorization header before build.
            assert!(
                message.contains("require https://api.github.com")
                    || message.contains("must not carry"),
                "{url}: {message}"
            );
        }
        let error = send_authorized_with(
            Arc::new(crate::config::Config::default()),
            "owner/daemon-gh",
            client
                .get("https://api.github.com/user")
                .header("authorization", "fixture-local"),
            |_, _, _| panic!("own authorization reached RPC"),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("must not carry"));
        let stream =
            futures_util::stream::iter([Ok::<_, std::io::Error>(b"fixture-stream".to_vec())]);
        let error = send_authorized_with(
            Arc::new(crate::config::Config::default()),
            "owner/daemon-gh",
            client
                .post("https://api.github.com/graphql")
                .body(reqwest::Body::wrap_stream(stream)),
            |_, _, _| panic!("streamed request reached RPC"),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("streamed bodies are refused"));
        let error = send_authorized_with(
            Arc::new(crate::config::Config::default()),
            "owner/daemon-gh",
            client.post("https://api.github.com/graphql").body(vec![
                0;
                crate::sekrets::authorized::MAX_REQUEST_BODY
                    + 1
            ]),
            |_, _, _| panic!("oversized request reached RPC"),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("exceeds 16 MiB"));
    }

    #[tokio::test]
    async fn authorized_response_body_is_capped_before_becoming_a_response() {
        use crate::sekrets::authorized::{AuthorizedResponse, MAX_RESPONSE_BODY};
        for length in [MAX_RESPONSE_BODY, MAX_RESPONSE_BODY + 1] {
            let result = send_authorized_with(
                Arc::new(crate::config::Config::default()),
                "owner/daemon-gh",
                reqwest::Client::new().get("https://api.github.com:443/repos/acme/garden"),
                move |_, _, _| {
                    Ok(AuthorizedResponse {
                        status: 200,
                        headers: vec![],
                        body: vec![0; length],
                    })
                },
            )
            .await;
            if length == MAX_RESPONSE_BODY {
                assert_eq!(
                    result.unwrap().bytes().await.unwrap().len(),
                    MAX_RESPONSE_BODY
                );
            } else {
                assert!(result.err().unwrap().to_string().contains("exceeds 64 MiB"));
            }
        }
    }

    #[tokio::test]
    async fn authorized_requests_preserve_method_body_status_and_all_response_headers() {
        use crate::sekrets::authorized::AuthorizedResponse;
        let config = Arc::new(crate::config::Config {
            node: "fixture-node".into(),
            ..Default::default()
        });
        for (method, path, status) in [
            ("GET", "/repos/acme/garden/issues?page=2", 304),
            ("POST", "/graphql", 200),
            ("POST", "/repos/acme/garden/issues/1/comments", 401),
            ("POST", "/repos/acme/garden/pulls/1/reviews", 403),
            ("GET", "/redirect", 302),
            ("GET", "/error", 503),
        ] {
            let url = format!("https://api.github.com{path}");
            let expected_url = url.clone();
            let payload = br#"{"body":"fixture-comment"}"#;
            let response = send_authorized_with(config.clone(), "owner/daemon-gh",
                reqwest::Client::new().request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), &url)
                    .header("if-none-match", "\"fixture-etag\"")
                    .header("accept", "application/vnd.github+json")
                    .header("x-github-api-version", "2022-11-28")
                    .body(payload.to_vec()), move |config, profile, request| {
                assert_eq!(config.node, "fixture-node");
                assert_eq!(profile, "owner/daemon-gh");
                assert_eq!(request.method, method);
                assert_eq!(request.url, expected_url);
                assert_eq!(request.body, payload);
                let headers = request.headers.iter().cloned().collect::<BTreeMap<_,_>>();
                assert_eq!(headers["if-none-match"], "\"fixture-etag\"");
                assert_eq!(headers["accept"], "application/vnd.github+json");
                assert_eq!(headers["x-github-api-version"], "2022-11-28");
                assert!(headers.contains_key("user-agent"));
                assert!(!headers.contains_key("authorization"));
                Ok(AuthorizedResponse { status, headers: vec![
                    ("etag".into(), "\"next-etag\"".into()),
                    ("link".into(), "<https://api.github.com/repos/acme/garden/issues?page=3>; rel=\"next\"".into()),
                    ("x-ratelimit-remaining".into(), "37".into()),
                    ("x-ratelimit-reset".into(), "1234567890".into()),
                    ("retry-after".into(), "30".into()),
                    ("location".into(), "https://github.com/login".into()),
                    ("x-fixture".into(), "first".into()), ("x-fixture".into(), "second".into()),
                ], body: br#"{"message":"fixture response"}"#.to_vec() })
            }).await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.url().as_str(), url);
            assert_eq!(response.headers()["etag"], "\"next-etag\"");
            assert!(
                response.headers()["link"]
                    .to_str()
                    .unwrap()
                    .contains("page=3")
            );
            assert_eq!(response.headers()["x-ratelimit-remaining"], "37");
            assert_eq!(response.headers()["x-ratelimit-reset"], "1234567890");
            assert_eq!(response.headers()["retry-after"], "30");
            assert_eq!(response.headers().get_all("x-fixture").iter().count(), 2);
            assert_eq!(
                response.bytes().await.unwrap().as_ref(),
                br#"{"message":"fixture response"}"#
            );
        }
    }

    #[tokio::test]
    async fn gateway_errors_and_unavailable_socket_never_fall_back_or_retry() {
        use crate::sekrets::authorized::{AuthorizedError, authorized_request_at};
        for error in [
            AuthorizedError::Unavailable("fixture absent".into()),
            AuthorizedError::Refused("fixture grant".into()),
            AuthorizedError::Transport("fixture TLS".into()),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let expected = error.to_string();
            let result = send_authorized_with(
                Arc::new(crate::config::Config::default()),
                "owner/daemon-gh",
                reqwest::Client::new().post("https://api.github.com/graphql"),
                move |_, _, _| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Err(error)
                },
            )
            .await
            .err()
            .unwrap();
            assert!(format!("{result:#}").contains(&expected));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("missing.sock");
        let result = send_authorized_with(
            Arc::new(crate::config::Config::default()),
            "owner/daemon-gh",
            reqwest::Client::new().get("https://api.github.com/user"),
            move |config, profile, request| {
                authorized_request_at(&socket, config, profile, request)
            },
        )
        .await
        .err()
        .unwrap();
        assert!(
            result
                .downcast_ref::<AuthorizedError>()
                .is_some_and(|e| matches!(e, AuthorizedError::Unavailable(_)))
        );
    }

    #[tokio::test]
    async fn authorized_adapter_uses_the_real_client_wire_once_for_a_rejected_mutation() {
        use crate::sekrets::{
            authorized::authorized_request_at,
            protocol::{self, CallerView, Reply, Request},
        };
        use base64::Engine as _;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("gateway.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let gateway = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (hello, _) = protocol::recv::<Request>(&stream).unwrap().unwrap();
            assert!(matches!(hello, Request::Hello { attestation: None }));
            protocol::send(
                &stream,
                &Reply::Hello {
                    caller: CallerView::Person {
                        person: "person/fixture".into(),
                    },
                    nonce: "fixture nonce".into(),
                    gateway: "fixture".into(),
                },
                &[],
            )
            .unwrap();
            let (request, _) = protocol::recv::<Request>(&stream).unwrap().unwrap();
            let Request::Authorized(call) = request else {
                panic!("wrong gateway request");
            };
            assert_eq!(call.profile, "owner/daemon-gh");
            assert_eq!(call.method, "POST");
            assert_eq!(
                call.url,
                "https://api.github.com/repos/acme/garden/issues/1/comments"
            );
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(call.body)
                    .unwrap(),
                br#"{"body":"fixture"}"#
            );
            assert!(
                !call
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            );
            protocol::send(
                &stream,
                &Reply::Response {
                    status: 401,
                    headers: vec![("x-ratelimit-remaining".into(), "37".into())],
                    body: base64::engine::general_purpose::STANDARD.encode(b"original 401"),
                },
                &[],
            )
            .unwrap();
            assert!(
                protocol::recv::<Request>(&stream).unwrap().is_none(),
                "mutation was replayed"
            );
        });
        let response = send_authorized_with(
            Arc::new(crate::config::Config::default()),
            "owner/daemon-gh",
            reqwest::Client::new()
                .post("https://api.github.com/repos/acme/garden/issues/1/comments")
                .json(&serde_json::json!({"body":"fixture"})),
            move |config, profile, request| {
                authorized_request_at(&socket, config, profile, request)
            },
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-ratelimit-remaining"], "37");
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"original 401");
        gateway.join().unwrap();
    }

    fn gh_fixture() -> (tempfile::TempDir, BTreeMap<String, String>) {
        let root = tempfile::tempdir().unwrap();
        let gh = root.path().join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\nprintf x >> \"$CALL_COUNT\"\nprintf fixture-credential\n",
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let environment = BTreeMap::from([
            ("PATH".into(), root.path().display().to_string()),
            (
                "CALL_COUNT".into(),
                root.path().join("calls").display().to_string(),
            ),
        ]);
        (root, environment)
    }

    #[tokio::test]
    async fn concurrent_acquisition_runs_gh_once_and_reuses_without_expiry() {
        let (root, environment) = gh_fixture();
        let cache = Arc::new(TokenCache::default());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let environment = environment.clone();
            tasks.push(tokio::spawn(async move {
                auth_for(&crate::config::GithubConfig::default(), &cache, || async {
                    Ok(environment)
                })
                .await
                .unwrap()
            }));
        }
        let first = tasks.remove(0).await.unwrap();
        for task in tasks {
            let auth = task.await.unwrap();
            assert!(Arc::ptr_eq(first.local_token(), auth.local_token()));
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3_600)).await;
        auth_for(&crate::config::GithubConfig::default(), &cache, || async {
            Ok(environment.clone())
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(root.path().join("calls")).unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_credentials_back_off_and_late_401_cannot_evict_refresh() {
        let cache = Arc::new(TokenCache::default());
        let first = cache
            .acquire(|| async { Ok("fixture-first".into()) })
            .await
            .unwrap();
        cache.invalidate(&first).await;
        assert!(
            cache
                .acquire(|| async { panic!("must back off") })
                .await
                .is_err()
        );
        tokio::time::advance(AUTH_REFRESH_BACKOFF).await;
        let second = cache
            .acquire(|| async { Ok("fixture-second".into()) })
            .await
            .unwrap();
        cache.invalidate(&first).await;
        let still_second = cache
            .acquire(|| async { panic!("late 401 evicted the refresh") })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            second.local_token(),
            still_second.local_token()
        ));
    }

    #[tokio::test]
    async fn failed_acquisition_is_single_flight_and_bounded() {
        let cache = Arc::new(TokenCache::default());
        let attempts = AtomicUsize::new(0);
        let lookups = (0..16).map(|_| {
            cache.acquire(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                bail!("fixture login unavailable")
            })
        });
        assert!(
            futures_util::future::join_all(lookups)
                .await
                .iter()
                .all(Result::is_err)
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_acquisition_does_not_suppress_the_next_lookup() {
        let cache = Arc::new(TokenCache::default());
        let acquiring = cache.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            acquiring
                .acquire(|| async {
                    started.send(()).unwrap();
                    std::future::pending::<Result<String>>().await
                })
                .await
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.err().unwrap().is_cancelled());
        let auth = cache
            .acquire(|| async { Ok("fixture-next".into()) })
            .await
            .unwrap();
        assert!(auth.local_token().as_str() == "fixture-next");
    }

    #[test]
    fn missing_configuration_fails_closed() {
        let source = OnceLock::new();
        assert!(
            configured_source(&source)
                .unwrap_err()
                .to_string()
                .contains("not configured")
        );
        source
            .set(Arc::new(crate::config::Config::default()))
            .unwrap();
        assert!(configured_source(&source).is_ok());
    }

    #[tokio::test]
    async fn environment_precedence_never_spawns_gh() {
        let (root, mut environment) = gh_fixture();
        environment.insert("GH_TOKEN".into(), "fixture-primary".into());
        environment.insert("GITHUB_TOKEN".into(), "fixture-secondary".into());
        assert!(lookup_github_token(&environment).await.unwrap() == "fixture-primary");
        environment.insert("GH_TOKEN".into(), " ".into());
        assert!(lookup_github_token(&environment).await.unwrap() == "fixture-secondary");
        assert!(!root.path().join("calls").exists());
    }

    #[tokio::test]
    async fn unauthorized_mutation_is_sent_once_and_invalidates_its_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                assert_eq!(request.method(), reqwest::Method::POST);
                assert_eq!(request.headers()["if-none-match"], "fixture-etag");
                assert!(request.headers().contains_key("authorization"));
                let body = axum::body::to_bytes(request.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(&body[..], br#"{"body":"fixture-comment"}"#);
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [("x-ratelimit-remaining", "37"), ("etag", "fixture-etag")],
                    "{\"message\":\"Bad credentials\"}",
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cache = Arc::new(TokenCache::default());
        let auth = cache
            .acquire(|| async { Ok("fixture-auth".into()) })
            .await
            .unwrap();
        let response = send(
            reqwest::Client::new()
                .post(&base)
                .header("if-none-match", "fixture-etag")
                .json(&serde_json::json!({"body":"fixture-comment"})),
            &auth,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-ratelimit-remaining"], "37");
        assert_eq!(response.headers()["etag"], "fixture-etag");
        assert!(cache.0.lock().await.token.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            cache
                .acquire(|| async { panic!("401 refresh must back off") })
                .await
                .is_err()
        );
        server.abort();
    }
    fn token_file_fixture(contents: &[u8]) -> (tempfile::TempDir, crate::config::GithubConfig) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = crate::config::GithubConfig {
            token_file: Some(path),
            ..Default::default()
        };
        (root, config)
    }

    #[tokio::test]
    async fn explicit_file_precedes_default_sources_and_reloads_changed_mtime() {
        let (_root, config) = token_file_fixture(b"fixture-first\n");
        let cache = Arc::new(TokenCache::default());
        let first = auth_for(&config, &cache, || async {
            panic!("file source touched default credentials")
        })
        .await
        .unwrap();
        assert!(first.local_token().as_str() == "fixture-first");
        let unchanged = auth_for(&config, &cache, || async {
            panic!("file source touched default credentials")
        })
        .await
        .unwrap();
        assert!(Arc::ptr_eq(first.local_token(), unchanged.local_token()));
        let path = config.token_file.as_ref().unwrap();
        let prior_mtime = std::fs::metadata(path).unwrap().modified().unwrap();
        std::fs::write(path, b"fixture-next!\r\n").unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(prior_mtime + Duration::from_secs(2)))
            .unwrap();
        let rotated = auth_for(&config, &cache, || async {
            panic!("rotation touched default credentials")
        })
        .await
        .unwrap();
        assert!(rotated.local_token().as_str() == "fixture-next!");
        assert!(!Arc::ptr_eq(first.local_token(), rotated.local_token()));
        cache.invalidate(&first).await;
        let still_rotated = cache.acquire_file(path).await.unwrap();
        assert!(Arc::ptr_eq(
            rotated.local_token(),
            still_rotated.local_token()
        ));
    }

    #[tokio::test]
    async fn file_rotation_by_atomic_replacement_is_seen_even_with_same_mtime() {
        let (root, config) = token_file_fixture(b"fixture-first");
        let cache = Arc::new(TokenCache::default());
        let path = config.token_file.as_ref().unwrap();
        let first = cache.acquire_file(path).await.unwrap();
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"fixture-next!").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(replacement, path).unwrap();
        let rotated = cache.acquire_file(path).await.unwrap();
        assert!(rotated.local_token().as_str() == "fixture-next!");
        assert!(!Arc::ptr_eq(first.local_token(), rotated.local_token()));
    }

    #[tokio::test]
    async fn unsafe_permissions_on_reload_refuse_even_an_unchanged_cached_file() {
        let (_root, config) = token_file_fixture(b"fixture-first");
        let cache = Arc::new(TokenCache::default());
        let path = config.token_file.as_ref().unwrap();
        cache.acquire_file(path).await.unwrap();
        for mode in [0o640, 0o604, 0o644, 0o620, 0o602, 0o610, 0o601] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = auth_for(&config, &cache, || async {
                panic!("unsafe file fell back")
            })
            .await
            .err()
            .unwrap();
            assert!(error.to_string().contains("group or others"));
        }
    }

    #[test]
    fn token_file_rejects_symlinks_and_a_different_owner() {
        let (root, config) = token_file_fixture(b"fixture-credential");
        let path = config.token_file.as_ref().unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(path, &alias).unwrap();
        assert!(open_token_file(&alias).is_err());
        let metadata = std::fs::metadata(path).unwrap();
        let other_uid = metadata.uid().wrapping_add(1);
        assert!(
            validate_token_file_metadata(&metadata, other_uid)
                .unwrap_err()
                .to_string()
                .contains("owned by")
        );
        assert!(open_token_file(path).is_ok());
    }

    #[tokio::test]
    async fn invalid_file_contents_never_fall_back_or_include_contents_in_errors() {
        for contents in [
            &b""[..],
            &b"fixture-first\nfixture-next"[..],
            &b"fixture-secret \n"[..],
            &b"\xfffixture-secret"[..],
        ] {
            let (_root, config) = token_file_fixture(contents);
            let cache = Arc::new(TokenCache::default());
            let error = auth_for(&config, &cache, || async {
                panic!("invalid file fell back")
            })
            .await
            .err()
            .unwrap();
            assert!(!format!("{error:#}").contains("fixture-secret"));
        }
    }

    #[tokio::test]
    async fn profile_selection_never_reads_local_credentials_or_cached_token() {
        let cache = Arc::new(TokenCache::default());
        cache
            .acquire(|| async { Ok("fixture-cached".into()) })
            .await
            .unwrap();
        let mut config = crate::config::Config::default();
        config.github.sekrets_profile = Some("owner/daemon-gh".into());
        config.state_dir = "/definitely/missing/node-state".into();
        let auth = auth_for_config(Arc::new(config.clone()), &cache, || async {
            panic!("profile touched environment or gh credentials")
        })
        .await
        .unwrap();
        assert!(auth.is_valid());
        assert!(matches!(auth, GithubAuth::Authorized { .. }));
        config.github.token_file = Some("/definitely/missing/credential".into());
        let error = auth_for_config(Arc::new(config.clone()), &cache, || async {
            panic!("conflicting profile touched local credentials")
        })
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("mutually exclusive"));
        assert!(config.github.validate().is_err());
    }
    #[tokio::test]
    async fn default_environment_changes_rotate_with_existing_precedence() {
        let (root, mut environment) = gh_fixture();
        let config = crate::config::GithubConfig::default();
        let cache = Arc::new(TokenCache::default());
        environment.insert("GH_TOKEN".into(), "fixture-primary".into());
        environment.insert("GITHUB_TOKEN".into(), "fixture-secondary".into());
        let primary = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(primary.local_token().as_str() == "fixture-primary");
        environment.insert("GITHUB_TOKEN".into(), "fixture-rotated-secondary".into());
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(primary.local_token(), unchanged.local_token()));
        environment.remove("GH_TOKEN");
        let secondary = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(secondary.local_token().as_str() == "fixture-rotated-secondary");
        cache.invalidate(&primary).await;
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            secondary.local_token(),
            unchanged.local_token()
        ));
        assert!(!root.path().join("calls").exists());
        environment.remove("GITHUB_TOKEN");
        auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("calls")).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gh_config_change_refreshes_once_without_reading_its_contents() {
        let (root, mut environment) = gh_fixture();
        environment.insert("GH_CONFIG_DIR".into(), root.path().display().to_string());
        let hosts = root.path().join("hosts.yml");
        std::fs::write(&hosts, b"not valid credential content").unwrap();
        let config = crate::config::GithubConfig::default();
        let cache = Arc::new(TokenCache::default());
        let first = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(first.local_token(), unchanged.local_token()));
        let modified = std::fs::metadata(&hosts).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(&hosts)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified + Duration::from_secs(2)))
            .unwrap();
        let refreshes =
            (0..8).map(|_| auth_for(&config, &cache, || async { Ok(environment.clone()) }));
        let refreshed = futures_util::future::join_all(refreshes).await;
        let newest = refreshed[0].as_ref().unwrap();
        assert!(!Arc::ptr_eq(first.local_token(), newest.local_token()));
        assert!(
            refreshed.iter().all(|auth| Arc::ptr_eq(
                auth.as_ref().unwrap().local_token(),
                newest.local_token()
            ))
        );
        cache.invalidate(&first).await;
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(newest.local_token(), unchanged.local_token()));
        assert_eq!(std::fs::read(root.path().join("calls")).unwrap().len(), 2);
    }
}
