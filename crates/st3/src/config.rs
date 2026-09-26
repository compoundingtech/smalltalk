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
                    .with_context(|| format!("read st3 config {}", selected.display()));
            }
        };
        let mut config: Self = toml::from_str(&bytes)
            .with_context(|| format!("parse st3 config {}", selected.display()))?;
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
        anyhow::ensure!(!self.node.trim().is_empty(), "the st3 node label is empty");
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
        let fleet_configured = self.fleet_id.is_some() || self.shared_secret_file.is_some();
        let peers_configured = self.peer_listen.is_some() || !self.peers.is_empty();
        anyhow::ensure!(
            fleet_configured == peers_configured,
            "fleet_id and shared_secret_file are required exactly when fleet peers are configured"
        );
        anyhow::ensure!(
            self.fleet_id.is_some() == self.shared_secret_file.is_some(),
            "fleet_id and shared_secret_file must be configured together"
        );
        anyhow::ensure!(
            self.peer_listen.is_some() == !self.peers.is_empty(),
            "a fleet node needs both a peer listener and at least one peer"
        );
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
                "peer '{}' must use plain http:// in st3 v1",
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
}
