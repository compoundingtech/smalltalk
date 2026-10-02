//! `STATE/fleet/fleet.toml` and the peers a node is configured with: what a member reads to
//! take part in a fleet. `st fleet` commands write the file; the sync worker and
//! [`super::activate`] read it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

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
