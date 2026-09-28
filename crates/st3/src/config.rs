use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::model::PlannerSpec;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub name: String,
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
    /// `STATE/fleet/fleet.toml`, merged by `apply_fleet_file` after command-line overrides.
    #[serde(skip)]
    pub fleet: Option<FleetFile>,
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
            config.socket = defaults.socket;
        }
        if config.client_gateway_socket.as_os_str().is_empty() {
            config.client_gateway_socket = defaults.client_gateway_socket;
        }
        Ok(config)
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
                    self.peer_listen.is_some() == !self.peers.is_empty(),
                    "a fleet node needs both a peer listener and at least one peer"
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
            self.peers
                .iter()
                .all(|peer| !peer.name.is_empty() && !peer.url.is_empty()),
            "each peer needs a name and URL"
        );
        let mut names = std::collections::HashSet::new();
        for peer in &self.peers {
            anyhow::ensure!(names.insert(&peer.name), "peer '{}' repeats", peer.name);
            anyhow::ensure!(
                peer.name != self.node,
                "peer '{}' uses the local node label",
                peer.name
            );
            let url = reqwest::Url::parse(&peer.url)
                .with_context(|| format!("parse peer URL for '{}'", peer.name))?;
            anyhow::ensure!(
                url.scheme() == "http",
                "peer '{}' must use plain http:// in st v1",
                peer.name
            );
            let host = url.host_str().unwrap_or_default();
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback());
            anyhow::ensure!(
                loopback,
                "peer '{}' must use a loopback URL exposed by Fabric",
                peer.name
            );
        }
        Ok(())
    }
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

        config.peers.clear();
        assert!(config.validate().unwrap_err().to_string().contains("both"));
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
        assert!(config.validate().unwrap_err().to_string().contains("both"));
    }
}
