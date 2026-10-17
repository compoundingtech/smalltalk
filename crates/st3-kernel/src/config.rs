use std::env;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::fleet::transport::is_permitted_route_address;
pub use smallclaims::fleet::file::{FleetFile, FleetMode, FleetRemoval, PeerConfig};
use crate::model::PlannerSpec;

pub mod changes;
mod client;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub node: String,
    /// Absent in legacy installs: preserve their bypass policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<st_drivers::access::AccessMode>,
    /// How seats rank against the daemon for CPU and IO. Absent means `background`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_priority: Option<st_runtime::SeatPriority>,
    /// The concrete person represented by trusted local product commands.
    pub person: Option<String>,
    pub fleet_id: Option<String>,
    pub shared_secret_file: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub pty_root: Option<PathBuf>,
    pub socket: PathBuf,
    pub client_gateway_socket: PathBuf,
    pub peer_listen: Option<String>,
    /// The one folder agents work in when nothing names another: new agents get a subfolder of
    /// it. Absent, it is `~/st` (`~/.st-instance/NAME/work` for an instance). Must be absolute
    /// and may not be the home folder or inside Documents, Desktop or Downloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_root: Option<PathBuf>,
    /// Lets `peer_listen` bind an address other than loopback or Tailscale, such as
    /// `0.0.0.0` or a LAN IP. st cannot tell whether such a path is encrypted, and the
    /// listener carries plain HTTP: replication, terminal input and attach grants.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub peer_listen_allow_plain_http: bool,
    pub peers: Vec<PeerConfig>,
    /// Default harness configuration for new planning sessions only.
    pub planner: PlannerSpec,
    /// The local observation log of this node.
    pub observations: ObservationsConfig,
    /// Daemon GitHub HTTP authentication. Omitted unless an explicit credential source is configured.
    #[serde(skip_serializing_if = "GithubConfig::is_default")]
    pub github: GithubConfig,
    /// Git change pilot: explicit repositories and one designated discovery owner.
    #[serde(skip_serializing_if = "changes::Config::is_default")]
    pub changes: changes::Config,
    /// Checkpoints that trim replicated history. Written only when it differs from the
    /// default, so a config this build writes still loads in a build without checkpoints.
    #[serde(skip_serializing_if = "CheckpointConfig::is_default")]
    pub checkpoint: CheckpointConfig,
    /// What this node does when an account nears its weekly limit. Off unless enabled.
    #[serde(skip_serializing_if = "LimitsConfig::is_default")]
    pub limits: LimitsConfig,
    /// Background reconciliation start cap, reloaded with daemon policies.
    #[serde(skip_serializing_if = "ReconcileConfig::is_default")]
    pub reconcile: ReconcileConfig,
    /// The setup screen's "Let Codex work without asking" box. Unset and `true` give Codex the
    /// install's access mode; `false` makes Codex ask. Written only by the setup screen when Codex
    /// is found. A build that does not know the key rejects a config that has it, so a config
    /// without the line still loads in older builds and one with it needs the line deleted first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_work_without_asking: Option<bool>,
    /// `STATE/fleet/fleet.toml`, merged by `apply_fleet_file` after command-line overrides.
    #[serde(skip)]
    pub fleet: Option<FleetFile>,
}

/// `[github]`: an explicit token file, or gateway-authorized requests with a sekrets profile.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// A single-token file readable only by its owner.
    pub token_file: Option<PathBuf>,
    pub sekrets_profile: Option<String>,
    /// Local observer request ceiling per resource/hour. Unset meters only until
    /// current usage and freshness are qualified; Factory's proposed cap is1000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observer_requests_per_hour: Option<u32>,
}

impl GithubConfig {
    pub fn is_default(&self) -> bool { self == &Self::default() }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.observer_requests_per_hour.is_none_or(|cap| (1..=1000).contains(&cap)),
            "github.observer_requests_per_hour must be1..1000 or omitted for meter-only");
        anyhow::ensure!(self.token_file.is_none() || self.sekrets_profile.is_none(),
            "github.token_file and github.sekrets_profile are mutually exclusive");
        if let Some(profile) = &self.sekrets_profile {
            anyhow::ensure!(!profile.trim().is_empty() && profile.trim() == profile,
                "github.sekrets_profile must name a non-empty profile without surrounding whitespace");
        }
        if let Some(path) = &self.token_file {
            open_token_file(path)?;
        }
        Ok(())
    }
}

/// Validate the opened descriptor before reading its contents, including on every reload.
/// Nonblocking open lets a configured FIFO fail validation without hanging startup.
pub fn open_token_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open GitHub token file {}", path.display()))?;
    let metadata = file.metadata().context("inspect GitHub token file")?;
    validate_token_file_metadata(&metadata, unsafe { libc::geteuid() })?;
    Ok(file)
}

pub fn validate_token_file_metadata(metadata: &fs::Metadata, effective_uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
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

/// Whether this node takes part in checkpoints. A node that does not never seals, and every
/// participant must seal, so turning it off anywhere stops trimming for the whole fleet: the
/// kill switch while checkpoints roll out.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointConfig {
    pub enabled: bool,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl CheckpointConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// `[limits]`: when an account's selected weekly reading reaches `stop_at_weekly_percent`, this
/// node stops the seats it hosts that use that account, except those in `keep`, once per weekly
/// window, and notifies the operations agent in `notify`. A seat a person starts again stays up
/// until the next window. Every node that hosts seats needs the same settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub enabled: bool,
    pub stop_at_weekly_percent: u32,
    /// Seats never stopped, such as the seat that coordinates the fleet.
    pub keep: Vec<String>,
    /// Accounts whose seats are never stopped, by the name `st usage` shows: the declared
    /// account (`ada/codex`) or the provider's label. A seat started later on one is covered too.
    pub exempt_accounts: Vec<String>,
    /// Harnesses (`claude`, `codex`) whose seats are never stopped, whatever their account.
    pub exempt_harnesses: Vec<String>,
    /// The operations agent that receives one message per account window.
    pub notify: Option<String>,
    /// Legacy setting, accepted for migration but never used for person delivery.
    pub ask: Option<String>,
    /// A reading older than this is not acted on, as a number followed by `s`, `m`, `h` or `d`.
    pub fresh: String,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            stop_at_weekly_percent: 95,
            keep: Vec::new(),
            exempt_accounts: Vec::new(),
            exempt_harnesses: Vec::new(),
            notify: None,
            ask: None,
            fresh: "1h".into(),
        }
    }
}

impl LimitsConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn fresh_ms(&self) -> Result<u64> {
        parse_duration_ms(&self.fresh, "limits.fresh")
    }

    /// Check an enabled policy. A disabled one is not read, so it may be half written.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.fresh_ms()?;
        anyhow::ensure!(
            (1..=100).contains(&self.stop_at_weekly_percent),
            "limits.stop_at_weekly_percent must be between 1 and 100"
        );
        anyhow::ensure!(
            self.keep.iter().all(|seat| seat.starts_with("agent/")),
            "limits.keep lists agent/... subjects"
        );
        anyhow::ensure!(
            self.exempt_accounts
                .iter()
                .chain(&self.exempt_harnesses)
                .all(|name| !name.is_empty() && !name.chars().any(char::is_whitespace)),
            "limits.exempt_accounts and limits.exempt_harnesses list names without spaces"
        );
        anyhow::ensure!(
            self.notify.as_deref().is_some_and(|agent| {
                agent.starts_with("agent/")
                    && agent.split('/').skip(1).all(|part| !part.is_empty())
                    && !agent.chars().any(char::is_whitespace)
            }),
            "limits needs an operations agent: set limits.notify to agent/... (limits.ask and person never receive raw limit events)"
        );
        Ok(())
    }
}

/// `[reconcile]`: maximum background pass starts per minute, default30, and when a seat that
/// holds claimed work and has nothing set to wake it is nudged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconcileConfig {
    pub max_passes_per_minute: u32,
    /// How long a seat idles holding a claimed step, with nothing set to wake it, before st nudges
    /// it: a duration such as `30m`, or `off`.
    pub idle_nudge_after: String,
    /// How long a watched thread may stay still and its watch still count as something that will
    /// wake the seat.
    pub idle_nudge_quiet_watch: String,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            max_passes_per_minute: 30,
            idle_nudge_after: "30m".into(),
            idle_nudge_quiet_watch: "2h".into(),
        }
    }
}

impl ReconcileConfig {
    pub fn is_default(&self) -> bool { self == &Self::default() }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!((1..=600).contains(&self.max_passes_per_minute),
            "reconcile.max_passes_per_minute must be between 1 and 600");
        self.idle_nudge()?;
        Ok(())
    }

    /// The idle-nudge settings these values name.
    pub fn idle_nudge(&self) -> Result<IdleNudgeSettings> {
        let at_least_a_minute = |value: &str, name: &str| -> Result<u64> {
            let ms = parse_duration_ms(value, name)?;
            anyhow::ensure!(ms >= 60_000, "{name} must be at least 1m");
            Ok(ms)
        };
        let after_ms = match self.idle_nudge_after.trim() {
            "off" => None,
            after => Some(at_least_a_minute(after, "reconcile.idle_nudge_after")?),
        };
        Ok(IdleNudgeSettings {
            after_ms,
            quiet_watch_ms: at_least_a_minute(
                &self.idle_nudge_quiet_watch,
                "reconcile.idle_nudge_quiet_watch",
            )?,
        })
    }
}

/// The default idle time before a nudge.
pub const IDLE_NUDGE_AFTER_MS: u64 = 30 * 60_000;
/// The default time a wait holds after it last woke its seat.
pub const IDLE_NUDGE_QUIET_WATCH_MS: u64 = 2 * 60 * 60_000;
/// The daemon's idle-nudge settings. `after_ms` is `None` when nudges are off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleNudgeSettings {
    pub after_ms: Option<u64>,
    pub quiet_watch_ms: u64,
}

impl Default for IdleNudgeSettings {
    fn default() -> Self {
        Self {
            after_ms: Some(IDLE_NUDGE_AFTER_MS),
            quiet_watch_ms: IDLE_NUDGE_QUIET_WATCH_MS,
        }
    }
}

/// The config file the running daemon was started with, so its `[limits]` can be read again.
static DAEMON_CONFIG: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Remember the daemon's config file. `None` is the default path.
pub fn set_daemon_config(path: Option<&Path>) {
    let _ = DAEMON_CONFIG.set(path.map(Path::to_path_buf));
}

/// The daemon's `[limits]` as the config file says now, so an edit applies without a restart.
/// `None` outside a daemon.
pub fn reload_daemon_limits() -> Option<Result<LimitsConfig>> {
    let path = DAEMON_CONFIG.get()?;
    Some(read_limits(path.as_deref()))
}

/// Read both policies once on the existing two-minute reload loop, outside database work.
pub fn reload_daemon_policies() -> Option<Result<(LimitsConfig, ReconcileConfig)>> {
    let path = DAEMON_CONFIG.get()?;
    Some(read_daemon_policies(path.as_deref()))
}

fn read_daemon_policies(path: Option<&Path>) -> Result<(LimitsConfig, ReconcileConfig)> {
    let selected = path.map(Path::to_path_buf).unwrap_or_else(Config::default_path);
    anyhow::ensure!(selected.exists(), "st config {} is missing", selected.display());
    let config = Config::load_unvalidated(Some(&selected))?;
    config.limits.validate()?;
    config.reconcile.validate()?;
    Ok((config.limits, config.reconcile))
}

/// `[limits]` from a config file that must exist: a file that is missing for a moment must not
/// read as the default, which has the policy off.
fn read_limits(path: Option<&Path>) -> Result<LimitsConfig> {
    let selected = path.map(Path::to_path_buf).unwrap_or_else(Config::default_path);
    anyhow::ensure!(
        selected.exists(),
        "st config {} is missing",
        selected.display()
    );
    let limits = Config::load_unvalidated(Some(&selected))?.limits;
    limits.validate()?;
    Ok(limits)
}

/// Observations of `local` retention stay on the node that made them. The daemon trims
/// them once an hour; the newest observation of each subject and kind always stays.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservationsConfig {
    /// How long to keep an observation, as a number followed by `s`, `m`, `h` or `d`.
    pub retention: String,
    /// The most observations to keep for one subject and kind.
    pub max_per_subject_kind: usize,
    /// Export every local observation to an OpenTelemetry collector. Off when absent.
    pub otlp: Option<OtlpConfig>,
}

/// `[observations.otlp]`: absent means no exporter runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    /// The collector's OTLP/HTTP base URL, such as `http://127.0.0.1:4318`.
    pub endpoint: String,
    /// A TOML file of extra request headers, such as an API key. It stays out of the main
    /// config so that file can be shared.
    #[serde(default)]
    pub headers_file: Option<PathBuf>,
}

impl OtlpConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.endpoint)
            .with_context(|| format!("parse observations.otlp.endpoint `{}`", self.endpoint))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "observations.otlp.endpoint must use http or https"
        );
        Ok(())
    }

    pub fn logs_url(&self) -> String {
        format!("{}/v1/logs", self.endpoint.trim_end_matches('/'))
    }

    pub fn headers(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let Some(path) = &self.headers_file else {
            return Ok(Default::default());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read observations.otlp.headers_file {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("parse observations.otlp.headers_file {}", path.display()))
    }
}

impl Default for ObservationsConfig {
    fn default() -> Self {
        Self {
            retention: "7d".into(),
            max_per_subject_kind: 20_000,
            otlp: None,
        }
    }
}

impl ObservationsConfig {
    pub const MINIMUM_RETENTION_MS: u64 = 60 * 60 * 1000;

    pub fn retention_ms(&self) -> Result<u64> {
        parse_duration_ms(&self.retention, "observations.retention")
    }
}

/// A duration such as `90s`, `30m`, `1h` or `7d`, in milliseconds.
fn parse_duration_ms(value: &str, name: &str) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(split);
    let amount: u64 = amount
        .parse()
        .with_context(|| format!("{name} `{value}` needs a number"))?;
    let unit_ms: u64 = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("{name} `{value}` needs a unit of s, m, h or d"),
    };
    amount
        .checked_mul(unit_ms)
        .with_context(|| format!("{name} `{value}` is too long"))
}

impl Default for Config {
    fn default() -> Self {
        let state_dir = xdg_dir("XDG_STATE_HOME", ".local/state").join("st3");
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("run"));
        let socket = runtime_dir.join("st3.sock");
        let client_gateway_socket = runtime_dir.join("st3-client.sock");
        Self {
            node: host_name(),
            access_mode: None,
            seat_priority: None,
            person: None,
            fleet_id: None,
            shared_secret_file: None,
            state_dir,
            pty_root: None,
            socket,
            client_gateway_socket,
            peer_listen: None,
            work_root: None,
            peer_listen_allow_plain_http: false,
            peers: Vec::new(),
            planner: PlannerSpec::default(),
            observations: ObservationsConfig::default(),
            github: GithubConfig::default(),
            changes: changes::Config::default(),
            checkpoint: CheckpointConfig::default(),
            limits: LimitsConfig::default(),
            reconcile: ReconcileConfig::default(),
            codex_work_without_asking: None,
            fleet: None,
        }
    }
}

impl Config {
    pub fn active_access_mode(&self) -> st_drivers::access::AccessMode {
        self.access_mode.unwrap_or_default()
    }

    pub fn active_seat_priority(&self) -> st_runtime::SeatPriority {
        self.seat_priority.unwrap_or_default()
    }

    /// Service installation writes a new config once, never upgrading a legacy missing mode.
    pub fn initialize_install_config(&self, path: &Path) -> Result<()> {
        let mut fresh = self.clone();
        fresh.access_mode = Some(self.access_mode.unwrap_or(st_drivers::access::AccessMode::Auto));
        let bytes = toml::to_string(&fresh)?;
        let parent = path.parent().context("the install config path has no directory")?;
        fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        use std::io::Write as _;
        staged.write_all(bytes.as_bytes())?;
        staged.as_file().sync_all()?;
        match fs::hard_link(staged.path(), path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error).with_context(|| format!("initialize st config {}", path.display())),
        }
    }

    pub fn default_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("st3")
            .join("config.toml")
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = Self::load_unvalidated(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Load the config, merge `STATE/fleet/fleet.toml`, and validate the result.
    pub fn load_with_fleet(path: Option<&Path>) -> Result<Self> {
        let mut config = Self::load_unvalidated(path)?;
        config.apply_fleet_file()?;
        config.validate()?;
        Ok(config)
    }

    /// Client configuration only: warn once on unknown names, preserving strict known types.
    /// Server, startup and policy reloads keep using the strict loaders above.
    pub fn load_client_unvalidated(path: Option<&Path>) -> Result<(Self, Vec<String>)> {
        let (config, unknown) = Self::load_selected(path, true)?;
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !unknown.is_empty() && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("st: {}", Self::unknown_client_keys_message(&unknown));
        }
        Ok((config, unknown))
    }

    /// One bounded diagnostic, including shell completion; unknown values are never shown.
    pub fn unknown_client_keys_message(unknown: &[String]) -> String {
        const MAX_NAMES: usize = 16;
        const MAX_PATH_CHARS: usize = 160;
        let mut names = unknown
            .iter()
            .take(MAX_NAMES)
            .map(|path| {
                let mut chars = path.chars();
                let mut shown: String = chars.by_ref().take(MAX_PATH_CHARS).collect();
                if chars.next().is_some() {
                    shown.push_str("...");
                }
                shown
            })
            .collect::<Vec<_>>();
        if unknown.len() > MAX_NAMES {
            names.push(format!(
                "... and {} more key paths",
                unknown.len() - MAX_NAMES
            ));
        }
        format!(
            "ignoring unknown client config keys: {}; daemon configuration remains strict",
            names.join(", ")
        )
    }

    pub fn load_unvalidated(path: Option<&Path>) -> Result<Self> {
        Self::load_selected(path, false).map(|(config, _)| config)
    }

    fn load_selected(path: Option<&Path>, lenient_client: bool) -> Result<(Self, Vec<String>)> {
        let selected = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        let bytes = match fs::read_to_string(&selected) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path.is_none() => {
                return Ok((
                    Self {
                        access_mode: Some(st_drivers::access::AccessMode::Auto),
                        ..Self::default()
                    },
                    Vec::new(),
                ));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read st config {}", selected.display()));
            }
        };
        let (mut config, unknown) = if lenient_client {
            client::parse(&bytes)
        } else {
            toml::from_str(&bytes).map(|config| (config, Vec::new()))
        }
        .with_context(|| format!("parse st config {}", selected.display()))?;
        let defaults = Self::default();
        if config.node.is_empty() {
            config.node = defaults.node;
        }
        if config.state_dir.as_os_str().is_empty() {
            config.state_dir = defaults.state_dir;
        }
        if config.socket.as_os_str().is_empty() {
            config.socket = if env::var_os("XDG_RUNTIME_DIR").is_some() {
                defaults.socket
            } else {
                state_socket(&config.state_dir)
            };
        }
        if config.client_gateway_socket.as_os_str().is_empty() {
            config.client_gateway_socket = defaults.client_gateway_socket;
        }
        Ok((config, unknown))
    }

    /// Resolve the daemon's published endpoint when this client lacks its runtime directory.
    /// Following the link itself would exceed the Unix socket address limit for long state paths.
    pub fn client_socket(&self) -> PathBuf {
        if env::var_os("XDG_RUNTIME_DIR").is_none() && self.socket == state_socket(&self.state_dir)
        {
            return fs::read_link(&self.socket)
                .ok()
                .filter(|target| target.is_absolute())
                .unwrap_or_else(|| self.socket.clone());
        }
        self.socket.clone()
    }

    /// Merge `STATE/fleet/fleet.toml` from the effective state directory. Call it after
    /// command-line overrides and before `validate`.
    pub fn apply_fleet_file(&mut self) -> Result<()> {
        let Some(file) = FleetFile::load(&self.state_dir)? else {
            return Ok(());
        };
        if let Some(configured) = &self.fleet_id {
            anyhow::ensure!(
                *configured == file.fleet_id,
                "config.toml names fleet `{configured}` but {} names fleet `{}`",
                FleetFile::path(&self.state_dir).display(),
                file.fleet_id
            );
        }
        self.fleet_id = Some(file.fleet_id.clone());
        if let Some(node) = &file.node {
            self.node = node.clone();
        }
        if self.shared_secret_file.is_none() {
            self.shared_secret_file = Some(file.secret_path(&self.state_dir));
        }
        if self.peer_listen.is_none()
            && file.mode == FleetMode::Listening
            && let Some(port) = file.port
        {
            self.peer_listen = Some(format!("127.0.0.1:{port}"));
        }
        self.fleet = Some(file);
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.reconcile.validate()?;
        self.github.validate()?;
        if self.work_root.is_some() {
            self.work_root()?;
        }
        self.changes.validate()?;
        anyhow::ensure!(
            matches!(
                self.planner.provider.as_str(),
                "codex" | "claude" | "pi" | "omp" | "opencode"
            ),
            "planner.provider must be an eligible harness driver"
        );
        anyhow::ensure!(
            self.planner
                .model
                .as_deref()
                .is_none_or(|value| !value.trim().is_empty())
                && self
                    .planner
                    .effort
                    .as_deref()
                    .is_none_or(|value| !value.trim().is_empty()),
            "planner model and effort must be nonempty when set"
        );
        anyhow::ensure!(
            self.planner.provider != "opencode" || self.planner.effort.is_none(),
            "the OpenCode planner does not accept an effort override"
        );
        anyhow::ensure!(!self.node.trim().is_empty(), "the st node label is empty");
        anyhow::ensure!(
            self.observations.retention_ms()? >= ObservationsConfig::MINIMUM_RETENTION_MS,
            "observations.retention must be at least 1h"
        );
        anyhow::ensure!(
            self.observations.max_per_subject_kind > 0,
            "observations.max_per_subject_kind must be positive"
        );
        if let Some(otlp) = &self.observations.otlp {
            otlp.validate()?;
        }
        self.limits.validate()?;
        anyhow::ensure!(
            self.person.as_deref().is_none_or(|person| {
                person.starts_with("person/")
                    && person.matches('/').count() == 1
                    && person.len() > "person/".len()
            }),
            "person must be one complete person/NAME subject"
        );
        anyhow::ensure!(
            self.socket != self.client_gateway_socket,
            "the privileged local socket and paired client gateway socket must be different"
        );
        anyhow::ensure!(
            self.client_gateway_socket != state_socket(&self.state_dir),
            "the paired client gateway socket must not replace the privileged state socket"
        );
        anyhow::ensure!(
            self.fleet_id.is_some() == self.shared_secret_file.is_some(),
            "fleet_id and shared_secret_file must be configured together"
        );
        match &self.fleet {
            // A config-peer fleet, as before membership.
            None => {
                let fleet_configured = self.fleet_id.is_some() || self.shared_secret_file.is_some();
                let peers_configured = self.peer_listen.is_some() || !self.peers.is_empty();
                anyhow::ensure!(
                    fleet_configured == peers_configured,
                    "fleet_id and shared_secret_file are required exactly when fleet peers are configured"
                );
                anyhow::ensure!(
                    !fleet_configured || !self.peers.is_empty(),
                    "a fleet node needs at least one peer; its listener is optional"
                );
            }
            // A member: peers come from membership, so config peers are optional overrides.
            Some(file) => {
                anyhow::ensure!(
                    file.mode == FleetMode::DialOut || self.peer_listen.is_some(),
                    "a listening fleet member needs a peer listener or a port in fleet.toml"
                );
            }
        }
        if let Some(fleet_id) = &self.fleet_id {
            anyhow::ensure!(
                uuid::Uuid::parse_str(fleet_id).is_ok(),
                "fleet_id must be a UUID"
            );
        }
        if let Some(address) = &self.peer_listen {
            let address = address
                .parse::<SocketAddr>()
                .with_context(|| format!("parse peer listener `{address}`"))?;
            anyhow::ensure!(
                self.peer_listen_allow_plain_http || is_permitted_route_address(&address.ip()),
                "the peer listener must bind to a loopback or Tailscale address; set \
                 peer_listen_allow_plain_http = true (or --peer-listen-allow-plain-http) to \
                 serve unencrypted HTTP on {address}"
            );
        }
        anyhow::ensure!(
            self.peers.iter().all(|peer| !peer.name.is_empty()),
            "each peer needs a name"
        );
        let mut names = std::collections::HashSet::new();
        for peer in &self.peers {
            anyhow::ensure!(
                names.insert((&peer.name, &peer.url)),
                "peer '{}' repeats",
                peer.name
            );
            anyhow::ensure!(
                peer.name != self.node,
                "peer '{}' uses the local node label",
                peer.name
            );
            if peer.url.is_empty() {
                continue;
            }
            anyhow::ensure!(
                crate::fleet::transport::parse_route(&peer.url).is_some(),
                "peer '{}' needs a loopback or tailnet http:// URL, or fabric://NODE_ID/PROTOCOL",
                peer.name
            );
        }
        Ok(())
    }
}

fn state_socket(state_dir: &Path) -> PathBuf {
    state_dir.join("run/st3.sock")
}

/// The daemon's Fabric terminal socket, which `st terminals expose-fabric` has Fabric forward
/// each terminal tunnel to.
pub fn terminal_socket(state_dir: &Path) -> PathBuf {
    state_dir.join("run/terminals.sock")
}

/// The terminating NUL also occupies a byte in sockaddr_un.sun_path.
#[cfg(target_os = "macos")]
const SUN_PATH_BYTES: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_BYTES: usize = 108;

pub fn validate_unix_socket_path(socket: &Path, flag: &str) -> Result<()> {
    let length = socket.as_os_str().as_bytes().len();
    anyhow::ensure!(
        length < SUN_PATH_BYTES,
        "Unix socket path {} is {length} bytes; maximum is {} bytes (sun_path limit: {SUN_PATH_BYTES} bytes including NUL). Use {flag} or set XDG_RUNTIME_DIR to a shorter directory",
        socket.display(),
        SUN_PATH_BYTES - 1
    );
    Ok(())
}

/// Folders that make macOS ask for permission the first time something lists them.
const PROTECTED_HOME_FOLDERS: [&str; 3] = ["Documents", "Desktop", "Downloads"];

/// The work root when nothing configures one: `~/st`, which is also where st has always put
/// the workspaces of new agents, or the instance's own `work` folder.
pub fn default_work_root() -> Result<PathBuf> {
    if let Some(instance) = crate::instance::current() {
        return Ok(instance.work_dir());
    }
    Ok(home_dir()?.join("st"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .context("HOME is not set to an absolute directory")
}

/// Refuse a work root that would make agents start in the home folder itself or in a folder
/// macOS guards with a permission prompt. Links are followed as far as the path exists, so a
/// linked `~/Documents` is caught, and a folder that does not exist yet below a link is judged
/// where it will be made; the path as written is refused as well.
fn check_work_root(root: &Path) -> Result<()> {
    let Ok(home) = home_dir() else {
        anyhow::ensure!(
            root.is_absolute(),
            "work_root must be an absolute path, not {}",
            root.display()
        );
        return Ok(());
    };
    check_work_root_in(root, &home)
}

/// `path` with links followed as far as it exists: the deepest existing ancestor is resolved and
/// the part that does not exist yet is appended, so a new folder below a link is seen where it
/// will be made.
fn resolve_existing(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut base = path;
    loop {
        if let Ok(real) = fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |resolved, name| resolved.join(name));
        }
        match (base.file_name(), base.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                base = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn check_work_root_in(root: &Path, home: &Path) -> Result<()> {
    anyhow::ensure!(
        root.is_absolute(),
        "work_root must be an absolute path, not {}",
        root.display()
    );
    // The folder is refused when it is guarded as written or once links are followed. A guarded
    // folder that is itself a link is resolved too, so the folder it points to is refused.
    let (resolved_root, resolved_home) = (resolve_existing(root), resolve_existing(home));
    for (root, home, resolve_guarded) in [
        (root, home, false),
        (resolved_root.as_path(), resolved_home.as_path(), true),
    ] {
        anyhow::ensure!(
            root != home,
            "work_root cannot be the home folder; name a folder inside it"
        );
        for protected in PROTECTED_HOME_FOLDERS {
            let guarded = home.join(protected);
            let guarded = if resolve_guarded {
                resolve_existing(&guarded)
            } else {
                guarded
            };
            anyhow::ensure!(
                !root.starts_with(&guarded),
                "work_root cannot be inside {protected}: macOS asks permission before a program lists it"
            );
        }
    }
    Ok(())
}

impl Config {
    /// The folder agents work in: the configured one, else the default.
    pub fn work_root(&self) -> Result<PathBuf> {
        match &self.work_root {
            Some(root) => {
                check_work_root(root)?;
                Ok(root.clone())
            }
            None => default_work_root(),
        }
    }
}

/// The work root of the running daemon, read from its config file again each time so an edit
/// applies to the next agent without a restart. Outside a daemon it is the default.
pub fn work_root() -> Result<PathBuf> {
    match DAEMON_CONFIG.get() {
        Some(path) => Config::load_unvalidated(path.as_deref())?.work_root(),
        None => default_work_root(),
    }
}

/// Where `st agents new` puts an agent that names no workspace: a new directory for the agent
/// inside the work root, `~/st/agents/IDENTITY` by default. The host that runs the agent
/// answers, so the path is right for that host's home.
pub fn default_agent_workspace(identity: &str) -> Result<PathBuf> {
    let identity = identity.strip_prefix("agent/").unwrap_or(identity);
    anyhow::ensure!(
        !identity.is_empty()
            && identity
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "agent identity `{identity}` cannot name a workspace directory"
    );
    Ok(work_root()?.join("agents").join(identity))
}

fn xdg_dir(variable: &str, home_suffix: &str) -> PathBuf {
    env::var_os(variable).map(PathBuf::from).unwrap_or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(home_suffix)
    })
}

fn host_name() -> String {
    let host = machine_host_name();
    match crate::instance::current() {
        Some(instance) => instance.node_name(&host),
        None => host,
    }
}

fn machine_host_name() -> String {
    env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_work_root_defaults_to_a_folder_inside_home_that_macos_does_not_guard() {
        let Ok(home) = super::home_dir() else { return };
        if crate::instance::current().is_some() {
            return;
        }
        let root = Config::default().work_root().unwrap();
        assert_eq!(root, home.join("st"));
        assert_eq!(
            super::default_agent_workspace("agent/site").unwrap(),
            root.join("agents/site")
        );
        for guarded in ["Documents", "Desktop", "Downloads"] {
            assert!(!root.starts_with(home.join(guarded)));
        }
    }

    #[test]
    fn a_configured_work_root_must_be_absolute_and_not_a_guarded_folder() {
        let config = |root: &str| -> Config {
            toml::from_str(&format!("work_root=\"{root}\"\n")).unwrap()
        };
        assert_eq!(
            config("/var/work").work_root().unwrap(),
            PathBuf::from("/var/work")
        );
        assert!(config("work").work_root().unwrap_err().to_string().contains("absolute"));
        let Ok(home) = super::home_dir() else { return };
        let home = home.display();
        assert!(config(&home.to_string()).work_root().is_err());
        for guarded in ["Documents", "Desktop", "Downloads"] {
            let error = config(&format!("{home}/{guarded}/projects")).validate().unwrap_err();
            assert!(error.to_string().contains(guarded), "{error}");
        }
        assert!(config(&format!("{home}/Smalltalk")).validate().is_ok());
    }

    #[test]
    fn a_linked_guarded_folder_is_still_refused() {
        let home = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        fs::create_dir(elsewhere.path().join("projects")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join("Documents")).unwrap();
        // Through the link, and straight at the folder the link points to.
        let through = home.path().join("Documents/projects");
        assert!(super::check_work_root_in(&through, home.path()).is_err());
        let direct = elsewhere.path().join("projects");
        assert!(super::check_work_root_in(&direct, home.path()).is_err());
        // A folder that does not exist yet, below the link, is refused too.
        let new_through = home.path().join("Documents/newwork");
        assert!(super::check_work_root_in(&new_through, home.path()).is_err());
        let new_direct = elsewhere.path().join("newwork/deeper");
        assert!(super::check_work_root_in(&new_direct, home.path()).is_err());
        assert!(super::check_work_root_in(&home.path().join("Smalltalk"), home.path()).is_ok());
        assert!(super::check_work_root_in(home.path(), home.path()).is_err());
    }

    #[test]
    fn a_config_without_a_work_root_stays_without_one() {
        let config: Config = toml::from_str("node=\"example\"\n").unwrap();
        assert!(config.work_root.is_none());
        assert!(!toml::to_string(&config).unwrap().contains("work_root"));
    }

    #[test]
    fn seat_priority_defaults_to_background_and_accepts_normal() {
        use st_runtime::SeatPriority;
        assert_eq!(Config::default().active_seat_priority(), SeatPriority::Background);
        let background: Config = toml::from_str("seat_priority=\"background\"").unwrap();
        assert_eq!(background.active_seat_priority(), SeatPriority::Background);
        let normal: Config = toml::from_str("seat_priority=\"normal\"").unwrap();
        assert_eq!(normal.active_seat_priority(), SeatPriority::Normal);
        assert!(toml::from_str::<Config>("seat_priority=\"high\"").is_err());
        assert!(!toml::to_string(&Config::default()).unwrap().contains("seat_priority"));
    }

    #[test]
    fn install_access_mode_preserves_legacy_config_and_writes_auto_once() {
        use st_drivers::access::AccessMode;
        let root = tempdir().unwrap();
        let path = root.path().join("config.toml");
        let legacy: Config = toml::from_str("node=\"example\"\n").unwrap();
        assert_eq!(legacy.active_access_mode(), AccessMode::Bypass);
        legacy.initialize_install_config(&path).unwrap();
        assert_eq!(Config::load(Some(&path)).unwrap().active_access_mode(), AccessMode::Auto);
        fs::write(&path, "node=\"example\"\n").unwrap();
        legacy.initialize_install_config(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "node=\"example\"\n");
        assert_eq!(Config::load(Some(&path)).unwrap().active_access_mode(), AccessMode::Bypass);
        assert!(toml::from_str::<Config>("access_mode=\"guess\"").is_err());
        let bypass: Config = toml::from_str("access_mode=\"bypass\"").unwrap();
        assert_eq!(bypass.active_access_mode(), AccessMode::Bypass);
    }

    #[test]
    fn reconcile_rate_defaults_and_bad_reload_preserves_the_callers_effective_value() {
        use super::*;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let (_, mut effective) = read_daemon_policies(Some(&path)).unwrap();
        assert_eq!(effective.max_passes_per_minute, 30);
        std::fs::write(&path, "[reconcile]\nmax_passes_per_minute=60\n").unwrap();
        effective = read_daemon_policies(Some(&path)).unwrap().1;
        for bad in ["0", "-1", "601", "4294967296", "\"bad\""] {
            std::fs::write(&path, format!("[reconcile]\nmax_passes_per_minute={bad}\n")).unwrap();
            assert!(read_daemon_policies(Some(&path)).is_err());
            assert_eq!(effective.max_passes_per_minute, 60);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(read_daemon_policies(Some(&path)).is_err());
        assert_eq!(effective.max_passes_per_minute, 60);
        std::fs::write(&path, "[reconcile]\nmax_passes_per_minute=15\n").unwrap();
        effective = read_daemon_policies(Some(&path)).unwrap().1;
        assert_eq!(effective.max_passes_per_minute, 15);
    }
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn github_observer_admission_is_meter_only_until_explicitly_configured() {
        let mut config=GithubConfig::default();
        assert_eq!(config.observer_requests_per_hour,None);
        assert!(config.validate().is_ok());
        config.observer_requests_per_hour=Some(1000);
        assert!(config.validate().is_ok());
        config.observer_requests_per_hour=Some(0);
        assert!(config.validate().is_err());
        config.observer_requests_per_hour=Some(1001);
        assert!(config.validate().is_err());
    }

    #[test]
    fn github_profile_is_opt_in_and_rejects_empty_configuration() {
        let config = Config::default();
        assert!(config.github.sekrets_profile.is_none());
        assert!(!toml::to_string(&config).unwrap().contains("[github]"));
        let configured: Config = toml::from_str(r#"[github]
sekrets_profile = "nathan/daemon-gh"
"#).unwrap();
        configured.validate().unwrap();
        assert_eq!(configured.github.sekrets_profile.as_deref(), Some("nathan/daemon-gh"));
        for profile in ["", " ", " nathan/daemon-gh", "nathan/daemon-gh "] {
            let mut invalid = Config::default();
            invalid.github.sekrets_profile = Some(profile.into());
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn github_token_file_permissions_are_checked_at_startup() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempdir().unwrap();
        let path = root.path().join("token");
        fs::write(&path, b"fixture-token\n").unwrap();
        let mut config = Config::default();
        config.github.token_file = Some(path.clone());
        for mode in [0o640, 0o604, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(config.validate().unwrap_err().to_string().contains("group or others"));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        config.validate().unwrap();
        config.github.sekrets_profile = Some("owner/daemon-gh".into());
        assert!(config.validate().unwrap_err().to_string().contains("mutually exclusive"));
        config.github.sekrets_profile = None;
        fs::remove_file(&path).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn unix_socket_path_reports_byte_limit_and_override() {
        let limit = SUN_PATH_BYTES - 1;
        let prefix = "/tmp/";
        let accepted = PathBuf::from(format!("{prefix}{}", "x".repeat(limit - prefix.len())));
        validate_unix_socket_path(&accepted, "--socket").unwrap();
        let one_byte_over = PathBuf::from(format!("{}x", accepted.display()));
        assert!(validate_unix_socket_path(&one_byte_over, "--socket").is_err());

        let rejected = PathBuf::from(format!("{}é", accepted.display()));
        let error = validate_unix_socket_path(&rejected, "--client-gateway-socket")
            .unwrap_err()
            .to_string();
        assert!(error.contains(&rejected.display().to_string()), "{error}");
        assert!(error.contains(&format!("{} bytes", limit + 2)), "{error}");
        assert!(
            error.contains(&format!("sun_path limit: {SUN_PATH_BYTES}")),
            "{error}"
        );
        assert!(error.contains("--client-gateway-socket"), "{error}");
        assert!(error.contains("XDG_RUNTIME_DIR"), "{error}");
    }

    #[test]
    fn a_missing_config_file_is_an_error_not_a_disabled_policy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let error = read_limits(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("is missing"), "{error}");
        fs::write(
            &path,
            "person = \"person/avery\"\n[limits]\nenabled = true\nnotify = \"agent/example/operations\"\n",
        )
        .unwrap();
        assert!(read_limits(Some(&path)).unwrap().enabled);
        fs::remove_file(&path).unwrap();
        assert!(read_limits(Some(&path)).is_err());
    }

    #[test]
    fn the_limits_policy_names_exempt_accounts_and_harnesses() {
        let config: Config = toml::from_str(
            "person = \"person/avery\"\n[limits]\nenabled = true\nnotify = \"agent/example/operations\"\n\
             exempt_accounts = [\"ada/codex\"]\nexempt_harnesses = [\"codex\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.limits.exempt_accounts, ["ada/codex"]);
        assert_eq!(config.limits.exempt_harnesses, ["codex"]);
        assert!(Config::default().limits.exempt_accounts.is_empty());
        let mut invalid = config.clone();
        invalid.limits.exempt_accounts = vec!["ada codex".into()];
        assert!(invalid.validate().is_err());
        invalid.limits.exempt_accounts = vec![String::new()];
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn the_limits_policy_is_off_until_enabled_and_needs_an_operations_agent() {
        let config = Config::default();
        assert!(!config.limits.enabled);
        assert_eq!(config.limits.stop_at_weekly_percent, 95);
        assert!(!toml::to_string(&config).unwrap().contains("[limits]"));

        let parsed: Config = toml::from_str(
            "[limits]\nenabled = true\nkeep = [\"agent/example/coordinator\"]\nfresh = \"30m\"\n",
        )
        .unwrap();
        let error = parsed.validate().unwrap_err().to_string();
        assert!(error.contains("an operations agent"), "{error}");
        let mut config = parsed.clone();
        config.person = Some("person/avery".into());
        config.limits.ask = Some("person/avery".into());
        assert!(
            config.validate().is_err(),
            "a legacy person target cannot receive events"
        );
        config.limits.notify = Some("agent/example/operations".into());
        config.validate().unwrap();
        for recipient in ["person/avery", "agent/", "agent//ops", "agent/ops seat"] {
            let mut invalid = config.clone();
            invalid.limits.notify = Some(recipient.into());
            assert!(invalid.validate().is_err(), "{recipient}");
        }
        assert_eq!(config.limits.fresh_ms().unwrap(), 30 * 60_000);
        for (field, value) in [("stop_at_weekly_percent", "0"), ("keep", "[\"seat\"]")] {
            let parsed: Config = toml::from_str(&format!(
                "person = \"person/avery\"\n[limits]\nenabled = true\n{field} = {value}\n"
            ))
            .unwrap();
            assert!(parsed.validate().is_err(), "{field}");
        }
    }

    #[test]
    fn idle_nudges_default_to_thirty_minutes_and_a_two_hour_quiet_watch() {
        let config = Config::default();
        let settings = config.reconcile.idle_nudge().unwrap();
        assert_eq!(settings.after_ms, Some(30 * 60_000));
        assert_eq!(settings.quiet_watch_ms, 2 * 3_600_000);
        assert!(!toml::to_string(&config).unwrap().contains("[reconcile]"));

        let parsed: Config = toml::from_str(
            "[reconcile]\nidle_nudge_after = \"45m\"\nidle_nudge_quiet_watch = \"3h\"\n",
        )
        .unwrap();
        parsed.validate().unwrap();
        let settings = parsed.reconcile.idle_nudge().unwrap();
        assert_eq!(settings.after_ms, Some(45 * 60_000));
        assert_eq!(settings.quiet_watch_ms, 3 * 3_600_000);

        let off: Config = toml::from_str("[reconcile]\nidle_nudge_after = \"off\"\n").unwrap();
        assert_eq!(off.reconcile.idle_nudge().unwrap().after_ms, None);

        for (field, value) in [
            ("idle_nudge_after", "30s"),
            ("idle_nudge_after", "soon"),
            ("idle_nudge_quiet_watch", "off"),
        ] {
            let parsed: Config =
                toml::from_str(&format!("[reconcile]\n{field} = \"{value}\"\n")).unwrap();
            assert!(parsed.validate().is_err(), "{field} = {value}");
        }
    }

    #[test]
    fn observation_retention_defaults_to_a_week_and_rejects_short_windows() {
        let config = Config::default();
        assert_eq!(config.observations.retention_ms().unwrap(), 7 * 86_400_000);
        config.validate().unwrap();

        let parsed: Config =
            toml::from_str("[observations]\nretention = \"36h\"\nmax_per_subject_kind = 50\n")
                .unwrap();
        assert_eq!(parsed.observations.retention_ms().unwrap(), 36 * 3_600_000);
        assert_eq!(parsed.observations.max_per_subject_kind, 50);

        for (retention, message) in [
            ("59m", "at least 1h"),
            ("7w", "unit of s, m, h or d"),
            ("d", "needs a number"),
        ] {
            let mut config = Config::default();
            config.observations.retention = retention.into();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(message), "{retention}: {error}");
        }
        let mut config = Config::default();
        config.observations.max_per_subject_kind = 0;
        assert!(config.validate().is_err());
        assert_eq!(Config::default().observations.otlp, None);
        let exported: Config =
            toml::from_str("[observations.otlp]\nendpoint = \"http://127.0.0.1:4318\"\n").unwrap();
        exported.validate().unwrap();
        let mut config = exported.clone();
        config.observations.otlp.as_mut().unwrap().endpoint = "ftp://collector".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn checkpoints_are_on_unless_disabled_and_absent_from_a_written_default_config() {
        let config = Config::default();
        assert!(config.checkpoint.enabled);
        // An older build refuses unknown sections, so a default config never names it.
        assert!(!toml::to_string(&config).unwrap().contains("checkpoint"));
        let config: Config = toml::from_str("").unwrap();
        assert!(config.checkpoint.enabled);
        let config: Config = toml::from_str("[checkpoint]\nenabled = false\n").unwrap();
        assert!(!config.checkpoint.enabled);
        assert!(toml::to_string(&config).unwrap().contains("[checkpoint]"));
        assert!(toml::from_str::<Config>("[checkpoint]\nenabled = true\nlag = 3\n").is_err());
    }

    #[test]
    fn defaults_to_a_local_only_daemon() {
        let config = Config::default();
        assert_eq!(config.planner, PlannerSpec::default());
        assert!(config.peer_listen.is_none());
        assert!(config.peers.is_empty());
        assert!(config.socket.ends_with("st3.sock"));
        assert!(config.client_gateway_socket.ends_with("st3-client.sock"));
        assert_ne!(config.socket, config.client_gateway_socket);
    }

    #[test]
    fn privileged_and_paired_client_sockets_must_be_distinct() {
        let mut config = Config::default();
        config.client_gateway_socket = config.socket.clone();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("different")
        );
        config.socket = "/tmp/st3-private.sock".into();
        config.client_gateway_socket = state_socket(&config.state_dir);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("privileged state socket")
        );
    }

    #[test]
    fn a_peer_listener_needs_the_plain_http_opt_in_beyond_loopback_or_tailnet() {
        let mut config = Config {
            peer_listen: Some("127.0.0.1:31313".into()),
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/tmp/st3-test-secret".into()),
            peers: vec![PeerConfig {
                name: "peer".into(),
                url: "http://127.0.0.1:31314".into(),
            }],
            ..Config::default()
        };
        for address in [
            "127.0.0.1:31313",
            "[::1]:31313",
            "100.64.0.1:31313",
            "100.127.255.254:31313",
            "[fd7a:115c:a1e0::1]:31313",
        ] {
            config.peer_listen = Some(address.into());
            config.validate().unwrap();
        }
        for address in [
            "0.0.0.0:31313",
            "[::]:31313",
            "100.63.255.255:31313",
            "100.128.0.1:31313",
            "192.168.1.10:31313",
            "[fd7a:115c:a1e1::1]:31313",
        ] {
            config.peer_listen = Some(address.into());
            config.peer_listen_allow_plain_http = false;
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("peer_listen_allow_plain_http"),
                "{address}"
            );
            config.peer_listen_allow_plain_http = true;
            config.validate().unwrap();
        }
    }

    #[test]
    fn a_named_peer_without_a_url_is_valid() {
        let peer: PeerConfig = toml::from_str("name = 'inbound'").unwrap();
        assert!(peer.url.is_empty());
        let config = Config {
            peer_listen: Some("127.0.0.1:31313".into()),
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/tmp/st3-test-secret".into()),
            peers: vec![peer],
            ..Config::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn fleet_configuration_accepts_only_loopback_or_tailnet_peer_urls() {
        let mut config = Config {
            peer_listen: Some("127.0.0.1:31313".into()),
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/tmp/st3-test-secret".into()),
            peers: vec![PeerConfig {
                name: "peer".into(),
                url: String::new(),
            }],
            ..Config::default()
        };
        config.validate().unwrap();
        // An outbound-only network needs no listener to exchange both ways.
        config.peer_listen = None;
        config.validate().unwrap();
        config.peer_listen = Some("127.0.0.1:31313".into());
        for url in [
            "http://localhost:31314",
            "http://127.0.0.1:31314",
            "http://[::1]:31314",
            "http://100.64.0.1:31314",
            "http://100.127.255.254:31314",
            "http://[fd7a:115c:a1e0::1]:31314",
        ] {
            config.peers[0].url = url.into();
            config.validate().unwrap();
        }
        for url in [
            "http://100.63.255.255:31314",
            "http://100.128.0.1:31314",
            "http://192.0.2.1:31314",
            "http://[fd7a:115c:a1e1::1]:31314",
            "http://example.com:31314",
        ] {
            config.peers[0].url = url.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("loopback or tailnet"),
                "{url}"
            );
        }
        config.peers.clear();
        assert!(config.validate().unwrap_err().to_string().contains("peer"));
    }

    #[test]
    fn command_overrides_can_complete_an_old_partial_fleet_config() {
        let root = tempdir().unwrap();
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            r#"
node = "replica-a"
peer_listen = "127.0.0.1:31313"

[[peers]]
name = "replica-b"
url = "http://127.0.0.1:31314"
"#,
        )
        .unwrap();

        assert!(Config::load(Some(&path)).is_err());
        let mut config = Config::load_unvalidated(Some(&path)).unwrap();
        config.fleet_id = Some("1f91ca65-7793-48cc-866e-ac15690130e1".into());
        config.shared_secret_file = Some(root.path().join("fleet.secret"));
        config.validate().unwrap();
    }

    #[test]
    fn fleet_toml_merges_with_config_and_rejects_different_fleet_ids() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        FleetFile {
            fleet_id: "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12".into(),
            port: Some(31999),
            anchor_key: Some("anchor".into()),
            ..FleetFile::default()
        }
        .save(&state_dir)
        .unwrap();
        let path = FleetFile::path(&state_dir);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let mut config = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        config.apply_fleet_file().unwrap();
        assert_eq!(
            config.fleet_id.as_deref(),
            Some("5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12")
        );
        assert_eq!(
            config.shared_secret_file.as_deref(),
            Some(state_dir.join("fleet/secret").as_path())
        );
        assert_eq!(config.peer_listen.as_deref(), Some("127.0.0.1:31999"));
        assert!(config.peers.is_empty());
        config.validate().unwrap();

        let mut conflicting = Config {
            state_dir,
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            ..Config::default()
        };
        assert!(
            conflicting
                .apply_fleet_file()
                .unwrap_err()
                .to_string()
                .contains("names fleet")
        );
    }

    #[test]
    fn a_dial_out_member_needs_no_listener_and_a_listening_member_does() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let mut file = FleetFile {
            fleet_id: "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12".into(),
            mode: FleetMode::DialOut,
            ..FleetFile::default()
        };
        file.save(&state_dir).unwrap();
        let mut config = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        config.apply_fleet_file().unwrap();
        assert!(config.peer_listen.is_none());
        config.validate().unwrap();

        file.mode = FleetMode::Listening;
        file.save(&state_dir).unwrap();
        let mut config = Config {
            state_dir,
            ..Config::default()
        };
        config.apply_fleet_file().unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("listening fleet member")
        );
    }

    #[test]
    fn a_node_without_fleet_toml_keeps_the_config_peer_rules() {
        let root = tempdir().unwrap();
        let mut config = Config {
            state_dir: root.path().join("state"),
            ..Config::default()
        };
        config.apply_fleet_file().unwrap();
        assert!(config.fleet.is_none());
        config.peer_listen = Some("127.0.0.1:31313".into());
        config.fleet_id = Some("1f91ca65-7793-48cc-866e-ac15690130e1".into());
        config.shared_secret_file = Some(root.path().join("secret"));
        assert!(config.validate().unwrap_err().to_string().contains("peer"));
    }
    #[test]
    fn native_routes_allow_alternatives_without_a_machine_kind() {
        let mut config = Config {
            fleet_id: Some("60407391-a0b8-4c5b-aa62-8137a3b3a2bf".into()),
            shared_secret_file: Some("/tmp/isolated-secret".into()),
            peers: vec![
                PeerConfig {
                    name: "beacon".into(),
                    url: "http://100.64.0.10:31313".into(),
                },
                PeerConfig {
                    name: "beacon".into(),
                    url: "fabric://invented-node-id/sync".into(),
                },
            ],
            ..Config::default()
        };
        config.validate().unwrap();
        config.peers.push(config.peers[0].clone());
        assert!(
            config.validate().is_err(),
            "a duplicate route must not create another dialer"
        );
        config.peers.pop();
        for url in [
            "http://192.0.2.1:31313",
            "https://100.64.0.10",
            "fabric://invented-node-id/",
            "fabric:///sync",
        ] {
            config.peers[0].url = url.into();
            assert!(config.validate().is_err(), "{url}");
        }
    }
}
