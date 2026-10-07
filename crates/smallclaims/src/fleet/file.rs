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

/// What a removed node learned from the signed refusal that ended its membership: the member
/// that reported it, the refusal's code and message (which names who removed it and why when
/// the member knows), and when. It lives in `STATE/fleet/removal.json`, beside `fleet.toml`,
/// whose strict format older builds still read.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemovalNotice {
    pub reported_by: String,
    /// `member-removed` or `member-left`.
    pub code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// When this node learned it; zero when only `fleet.toml` recorded the removal.
    #[serde(default)]
    pub learned_at_unix_ms: u128,
}

impl RemovalNotice {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("fleet").join("removal.json")
    }

    /// The removal `fleet.toml` records, with the details beside it when they were kept.
    pub fn load(state_dir: &Path) -> Option<Self> {
        let removed = FleetFile::load(state_dir).ok()??.removed?;
        let notice = fs::read(Self::path(state_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|notice| notice.code == removed.code);
        Some(notice.unwrap_or(Self {
            reported_by: removed.reported_by,
            code: removed.code,
            ..Self::default()
        }))
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let path = Self::path(state_dir);
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// One line for a person: who removed this node and what to do next.
    pub fn describe(&self, fleet_id: Option<&str>) -> String {
        let fleet = fleet_id.map(|id| format!(" {id}")).unwrap_or_default();
        let what = if self.code == "member-left" {
            format!("this node left fleet{fleet}")
        } else {
            format!("this node was removed from fleet{fleet}")
        };
        let detail = if self.message.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.message)
        };
        format!(
            "{what}{detail}, as {} reported{}; it no longer syncs, and writes here stay here. \
             Run st uninstall, or st fleet leave --offline to keep the local store as a \
             local-only node that a member can invite again",
            self.reported_by,
            if self.learned_at_unix_ms == 0 {
                String::new()
            } else {
                format!(" at {}", rfc3339(self.learned_at_unix_ms))
            }
        )
    }
}

fn rfc3339(unix_ms: u128) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms as i64)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| unix_ms.to_string())
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
    /// Serialize refusal persistence and leave cleanup, including the read before a write.
    /// Keep the lock outside `fleet/`, and never unlink it: waiters must share one inode even
    /// after leave removes that directory. Closing the returned file releases the lock.
    pub fn lock_settings(state_dir: &Path) -> Result<fs::File> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let path = state_dir.join("fleet-settings.lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        // A bounded isolated control observes actual contention, rather than sleeping to
        // guess whether the CLI reached the lock while the refusal writer was paused.
        #[cfg(feature = "test-support")]
        if let Some(directory) = std::env::var_os("ST3_TEST_REMOVAL_WRITE_BARRIER") {
            // SAFETY: lock owns a valid descriptor for the duration of this call.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
            {
                fs::write(
                    Path::new(&directory).join("contended"),
                    std::process::id().to_string(),
                )?;
            }
        }
        loop {
            // SAFETY: lock owns a valid descriptor for the duration of this call.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(lock);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).with_context(|| format!("lock {}", path.display()));
            }
        }
    }

    /// Drain any refusal settings write before removing the fleet directory. A later writer
    /// must reload under the same lock, so it cannot recreate settings after successful leave.
    pub fn remove(state_dir: &Path) -> Result<()> {
        let _lock = Self::lock_settings(state_dir)?;
        let directory = state_dir.join("fleet");
        fs::remove_dir_all(&directory)
            .with_context(|| format!("remove fleet settings {}", directory.display()))
    }

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
