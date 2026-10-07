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
pub(crate) struct GithubAuth {
    token: Arc<String>,
    cache: Option<Arc<TokenCache>>,
}

impl GithubAuth {
    pub(crate) fn is_valid(&self) -> bool {
        !self.token.trim().is_empty()
    }

    #[cfg(test)]
    pub(crate) fn test(token: &str) -> Self {
        Self {
            token: Arc::new(token.into()),
            cache: None,
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
struct TokenCache(Mutex<TokenState>);

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
            return Ok(GithubAuth {
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
        Ok(GithubAuth {
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
        let mut state = self.0.lock().await;
        if state
            .token
            .as_ref()
            .is_some_and(|known| Arc::ptr_eq(known, &auth.token))
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

static SOURCE: OnceLock<crate::config::GithubConfig> = OnceLock::new();

pub(crate) fn configure(config: &crate::config::GithubConfig) -> Result<()> {
    config.validate()?;
    anyhow::ensure!(
        config.sekrets_profile.is_none(),
        "unsupported GitHub configuration: github.sekrets_profile requires the pending authorized-request gateway client; no fallback credentials were used"
    );
    if let Some(known) = SOURCE.get() {
        anyhow::ensure!(
            known == config,
            "GitHub source was already configured for this process"
        );
        return Ok(());
    }
    SOURCE
        .set(config.clone())
        .map_err(|_| anyhow::anyhow!("GitHub source was already configured"))
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
        "unsupported GitHub configuration: github.sekrets_profile requires the pending authorized-request gateway client; no fallback credentials were used"
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
    auth_for(config, token_cache(), || async {
        tokio::task::spawn_blocking(crate::environment::snapshot).await?
    })
    .await
}

fn configured_source(
    source: &OnceLock<crate::config::GithubConfig>,
) -> Result<&crate::config::GithubConfig> {
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
    let response = request.bearer_auth(auth.token.as_str()).send().await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        && let Some(cache) = &auth.cache
    {
        cache.invalidate(auth).await;
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            assert!(Arc::ptr_eq(&first.token, &auth.token));
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
        assert!(Arc::ptr_eq(&second.token, &still_second.token));
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
        assert!(auth.token.as_str() == "fixture-next");
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
        source.set(crate::config::GithubConfig::default()).unwrap();
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
        assert!(first.token.as_str() == "fixture-first");
        let unchanged = auth_for(&config, &cache, || async {
            panic!("file source touched default credentials")
        })
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&first.token, &unchanged.token));
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
        assert!(rotated.token.as_str() == "fixture-next!");
        assert!(!Arc::ptr_eq(&first.token, &rotated.token));
        cache.invalidate(&first).await;
        let still_rotated = cache.acquire_file(path).await.unwrap();
        assert!(Arc::ptr_eq(&rotated.token, &still_rotated.token));
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
        assert!(rotated.token.as_str() == "fixture-next!");
        assert!(!Arc::ptr_eq(&first.token, &rotated.token));
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
    async fn reserved_profile_fails_closed_without_file_env_gh_or_cached_token() {
        let cache = Arc::new(TokenCache::default());
        cache
            .acquire(|| async { Ok("fixture-cached".into()) })
            .await
            .unwrap();
        let mut config = crate::config::GithubConfig {
            sekrets_profile: Some("owner/daemon-gh".into()),
            ..Default::default()
        };
        let error = auth_for(&config, &cache, || async {
            panic!("profile touched default credentials")
        })
        .await
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("unsupported GitHub configuration")
        );
        assert!(configure(&config).is_err());
        config.token_file = Some("/definitely/missing/credential".into());
        let error = auth_for(&config, &cache, || async {
            panic!("conflicting profile touched credentials")
        })
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("mutually exclusive"));
        assert!(config.validate().is_err());
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
        assert!(primary.token.as_str() == "fixture-primary");
        environment.insert("GITHUB_TOKEN".into(), "fixture-rotated-secondary".into());
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&primary.token, &unchanged.token));
        environment.remove("GH_TOKEN");
        let secondary = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(secondary.token.as_str() == "fixture-rotated-secondary");
        cache.invalidate(&primary).await;
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&secondary.token, &unchanged.token));
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
        assert!(Arc::ptr_eq(&first.token, &unchanged.token));
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
        assert!(!Arc::ptr_eq(&first.token, &newest.token));
        assert!(
            refreshed
                .iter()
                .all(|auth| Arc::ptr_eq(&auth.as_ref().unwrap().token, &newest.token))
        );
        cache.invalidate(&first).await;
        let unchanged = auth_for(&config, &cache, || async { Ok(environment.clone()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&newest.token, &unchanged.token));
        assert_eq!(std::fs::read(root.path().join("calls")).unwrap().len(), 2);
    }
}
