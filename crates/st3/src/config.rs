use std::env;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::model::PlannerSpec;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

/// How a fleet member takes part in replication.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FleetMode {
    /// Accepts connections and advertises endpoints.
    #[default]
    Listening,
    /// Accepts no connections and dials listening members.
    DialOut,
}

impl FleetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Listening => "listening",
            Self::DialOut => "dial-out",
        }
    }
}

/// Written when a member learns that it was removed from the fleet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRemoval {
    /// The member whose signed refusal reported it.
    pub reported_by: String,
    /// `member-removed` or `member-left`.
    pub code: String,
}

/// `STATE/fleet/fleet.toml`: the fleet settings that `st fleet` commands write. Paths are
/// relative to `STATE/fleet` unless absolute.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FleetFile {
    pub fleet_id: String,
    pub secret_file: PathBuf,
    pub node_key_file: PathBuf,
    pub anchor_key: Option<String>,
    /// The name this machine joined under. It wins over `config.toml` and `--node`.
    pub node: Option<String>,
    /// The sponsor's name and member key from the join handshake. Until membership arrives,
    /// the new member trusts only this key and the anchor's.
    pub sponsor: Option<String>,
    pub sponsor_key: Option<String>,
    /// How this node reached its sponsor during the join: an `http://` URL, or
    /// `fabric://NODE_ID/PROTOCOL`. The worker dials it until membership arrives.
    pub sponsor_routes: Vec<String>,
    pub mode: FleetMode,
    pub port: Option<u16>,
    pub transports: Vec<String>,
    pub fabric_protocol: Option<String>,
    /// Accept HMAC-only exchanges from config peers. True only during a migration.
    pub legacy_peers: bool,
    pub advertise_loopback: bool,
    pub writer_floor: Option<u64>,
    /// Executable overrides; tests point these at shims.
    pub fabric: Option<PathBuf>,
    pub tailscale: Option<PathBuf>,
    pub removed: Option<FleetRemoval>,
}

impl FleetFile {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("fleet").join("fleet.toml")
    }

    pub fn load(state_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(state_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        let file: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&file.fleet_id).is_ok(),
            "{} needs a fleet_id UUID",
            path.display()
        );
        Ok(Some(file))
    }

    /// Write the file atomically with mode `0600` inside a `0700` directory.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let path = Self::path(state_dir);
        let directory = path.parent().expect("fleet.toml has a parent");
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let temporary = directory.join(format!(".fleet.toml.{}.tmp", std::process::id()));
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(b"# Written by st fleet. Change it with st fleet commands.\n")?;
            file.write_all(toml::to_string(self)?.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &path)?;
        Ok(())
    }

    fn resolve(state_dir: &Path, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            state_dir.join("fleet").join(path)
        }
    }

    pub fn secret_path(&self, state_dir: &Path) -> PathBuf {
        let path = if self.secret_file.as_os_str().is_empty() {
            Path::new("secret")
        } else {
            self.secret_file.as_path()
        };
        Self::resolve(state_dir, path)
    }

    pub fn node_key_path(&self, state_dir: &Path) -> PathBuf {
        let path = if self.node_key_file.as_os_str().is_empty() {
            Path::new("node.key")
        } else {
            self.node_key_file.as_path()
        };
        Self::resolve(state_dir, path)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub node: String,
    /// The concrete person represented by trusted local product commands.
    pub person: Option<String>,
    pub fleet_id: Option<String>,
    pub shared_secret_file: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub pty_root: Option<PathBuf>,
    pub socket: PathBuf,
    pub client_gateway_socket: PathBuf,
    pub peer_listen: Option<String>,
    pub peers: Vec<PeerConfig>,
    /// Default harness configuration for new planning sessions only.
    pub planner: PlannerSpec,
    /// The local observation log of this node.
    pub observations: ObservationsConfig,
    /// Checkpoints that trim replicated history. Written only when it differs from the
    /// default, so a config this build writes still loads in a build without checkpoints.
    #[serde(skip_serializing_if = "CheckpointConfig::is_default")]
    pub checkpoint: CheckpointConfig,
    /// `STATE/fleet/fleet.toml`, merged by `apply_fleet_file` after command-line overrides.
    #[serde(skip)]
    pub fleet: Option<FleetFile>,
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
    pub otlp: Option<crate::otlp::OtlpConfig>,
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
        let value = self.retention.trim();
        let split = value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len());
        let (amount, unit) = value.split_at(split);
        let amount: u64 = amount
            .parse()
            .with_context(|| format!("observations.retention `{value}` needs a number"))?;
        let unit_ms: u64 = match unit {
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => anyhow::bail!("observations.retention `{value}` needs a unit of s, m, h or d"),
        };
        amount
            .checked_mul(unit_ms)
            .with_context(|| format!("observations.retention `{value}` is too long"))
    }
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
            person: None,
            fleet_id: None,
            shared_secret_file: None,
            state_dir,
            pty_root: None,
            socket,
            client_gateway_socket,
            peer_listen: None,
            peers: Vec::new(),
            planner: PlannerSpec::default(),
            observations: ObservationsConfig::default(),
            checkpoint: CheckpointConfig::default(),
            fleet: None,
        }
    }
}

impl Config {
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

    pub fn load_unvalidated(path: Option<&Path>) -> Result<Self> {
        let selected = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        let bytes = match fs::read_to_string(&selected) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path.is_none() => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read st config {}", selected.display()));
            }
        };
        let mut config: Self = toml::from_str(&bytes)
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
        Ok(config)
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
                address.ip().is_loopback(),
                "the peer listener must bind to a loopback address"
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

/// Where `st agents new` puts an agent that names no workspace: a new directory for the agent
/// below this account's home, `~/st/agents/IDENTITY`. The host that runs the agent answers, so
/// the path is right for that host's home.
pub fn default_agent_workspace(identity: &str) -> Result<PathBuf> {
    let identity = identity.strip_prefix("agent/").unwrap_or(identity);
    anyhow::ensure!(
        !identity.is_empty()
            && identity
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "agent identity `{identity}` cannot name a workspace directory"
    );
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .context("HOME is not set to an absolute directory")?;
    Ok(home.join("st/agents").join(identity))
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
    use super::*;
    use tempfile::tempdir;

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
    fn a_peer_listener_must_use_a_loopback_address() {
        let mut config = Config {
            peer_listen: Some("0.0.0.0:31313".into()),
            fleet_id: Some("1f91ca65-7793-48cc-866e-ac15690130e1".into()),
            shared_secret_file: Some("/tmp/st3-test-secret".into()),
            peers: vec![PeerConfig {
                name: "peer".into(),
                url: "http://127.0.0.1:31314".into(),
            }],
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );

        config.peer_listen = Some("127.0.0.1:31313".into());
        config.validate().unwrap();
        config.peer_listen = Some("[::1]:31313".into());
        config.validate().unwrap();
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
    fn fleet_configuration_is_complete_and_uses_only_loopback_transport() {
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
        config.validate().unwrap();
        // An outbound-only network needs no member kind or listener to exchange both ways.
        config.peer_listen = None;
        config.validate().unwrap();
        config.peer_listen = Some("127.0.0.1:31313".into());

        config.peers.clear();
        assert!(config.validate().unwrap_err().to_string().contains("peer"));
        config.peers.push(PeerConfig {
            name: "peer".into(),
            url: "http://192.0.2.1:31314".into(),
        });
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );
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
