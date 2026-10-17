//! Root's tile registry: which agent each tile is, which user ID root's launcher gave it, and
//! what its tile sees.
//!
//! The registry is the only source of a tile's identity. Root writes the tiles; the launcher
//! allocates each one's user ID from the registry's range and records it beside the tiles' state
//! (`STATE/uids.toml`), the way Nomad's dynamic workload users and systemd's `DynamicUser=` do,
//! so no tile is a hand-made account. A process is a tile's when the kernel says it runs as the
//! tile's user ID inside the tile's cgroup, which only root can place it in. Nothing a tile says
//! about itself, such as `ST_AGENT` or `--as`, counts.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// Where root keeps the registry.
pub const DEFAULT_REGISTRY: &str = "/etc/st-tiles/tiles.toml";
/// The systemd slice every tile's slice lives under.
pub const TILES_SLICE: &str = "st_tiles.slice";
/// The launcher's record of each tile's user ID, in the state root.
pub const UIDS_FILE: &str = "uids.toml";
/// User IDs systemd keeps for itself: its own dynamic users, and its containers' ranges.
const SYSTEMD_RESERVED: &[(u32, u32)] = &[(61_184, 65_519), (524_288, 1_879_048_191)];

/// Root's registry of tiles. Root owns the file; nothing reads one anyone else can change.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Each tile's private state: `STATE/NAME/home` and `STATE/NAME/work`, owned by the tile.
    #[serde(default = "default_state_root")]
    pub state_root: PathBuf,
    /// Each tile's runtime directories: its endpoint directory and the mount point of its root.
    #[serde(default = "default_runtime_root")]
    pub runtime_root: PathBuf,
    /// Immutable files and directories every tile sees read-only at the same path, such as the
    /// closures of approved tools. Each must be controlled by root. The host root is never bound.
    #[serde(default)]
    pub read_only: Vec<PathBuf>,
    /// `PATH` inside a tile.
    #[serde(default = "default_path")]
    pub path: Vec<PathBuf>,
    /// The user IDs the launcher gives tiles; each tile's group ID is its user ID. The default
    /// is Nomad's dynamic workload range. No account, group or subordinate range may use one.
    #[serde(default = "default_uids")]
    pub uids: UidRange,
    /// The sekrets gateway's socket, which every tile reaches at
    /// `/run/st-sekrets/gateway.sock`, the client's default. The gateway names a tile's caller
    /// by its Unix user and cgroup, as this registry does. Unset, a tile reaches no gateway.
    #[serde(default)]
    pub sekrets_socket: Option<PathBuf>,
    /// Tiles by name.
    #[serde(default)]
    pub tiles: BTreeMap<String, Tile>,
    /// Each tile's user ID, as the launcher allocated it: the state root's `uids.toml`, never
    /// the registry's own text.
    #[serde(skip)]
    allocated: BTreeMap<String, u32>,
}

/// The inclusive range of user IDs the launcher allocates from.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UidRange {
    pub first: u32,
    pub last: u32,
}

impl UidRange {
    pub fn contains(&self, uid: u32) -> bool {
        (self.first..=self.last).contains(&uid)
    }

    pub fn overlaps(&self, first: u32, last: u32) -> bool {
        self.first <= last && first <= self.last
    }
}

/// One tile: an agent, never a person, with a user ID the launcher allocates for as long as its
/// state lasts.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tile {
    /// The agent this tile is, such as `agent/fleet/fixture-example/atlas`.
    pub agent: String,
    /// The person the agent works for.
    pub person: String,
    /// The Unix user whose st daemon serves the tile's endpoint, normally its person's.
    #[serde(default)]
    pub supervisor_uid: Option<u32>,
    /// Resource ceilings for the tile's tools, in systemd's syntax. Limits, not reservations.
    #[serde(default = "default_tasks_max")]
    pub tasks_max: u32,
    #[serde(default = "default_memory_max")]
    pub memory_max: String,
    #[serde(default)]
    pub memory_high: Option<String>,
    #[serde(default = "default_cpu_weight")]
    pub cpu_weight: u32,
}

fn default_state_root() -> PathBuf {
    "/var/lib/st-tiles".into()
}
fn default_runtime_root() -> PathBuf {
    "/run/st-tiles".into()
}
fn default_path() -> Vec<PathBuf> {
    vec![
        "/run/current-system/sw/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
    ]
}
fn default_uids() -> UidRange {
    UidRange {
        first: 80_000,
        last: 89_999,
    }
}
fn default_tasks_max() -> u32 {
    512
}
fn default_memory_max() -> String {
    "8G".into()
}
fn default_cpu_weight() -> u32 {
    100
}

impl Registry {
    /// Read the registry and the launcher's user IDs, refusing a file anyone but root can
    /// change. Before the launcher allocates any, no tile has a user ID and none is identified.
    pub fn load(path: &Path) -> Result<Self> {
        root_controlled(path)?;
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let registry = Self::parse(&text).with_context(|| format!("parse {}", path.display()))?;
        let uids = registry.uids_file();
        match fs::symlink_metadata(&uids) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(registry),
            _ => {
                root_controlled(&uids)?;
                let text = fs::read_to_string(&uids)
                    .with_context(|| format!("read {}", uids.display()))?;
                registry
                    .with_uids(&text)
                    .with_context(|| format!("parse {}", uids.display()))
            }
        }
    }

    /// The registry's own text, with no user ID allocated yet.
    pub fn parse(text: &str) -> Result<Self> {
        let registry: Self = toml::from_str(text)?;
        registry.check()?;
        Ok(registry)
    }

    /// This registry with the launcher's user IDs, the text of `uids.toml`: `NAME = UID` lines.
    /// An entry for a tile root has since removed stays allocated, since its state may remain,
    /// but identifies no one.
    pub fn with_uids(mut self, text: &str) -> Result<Self> {
        let allocated: BTreeMap<String, u32> = toml::from_str(text)?;
        let mut owners = BTreeMap::new();
        for (name, &uid) in &allocated {
            check_name(name)?;
            if !self.uids.contains(uid) {
                bail!(
                    "tile {name} has user ID {uid}, outside the registry's range {}-{}",
                    self.uids.first,
                    self.uids.last
                );
            }
            if let Some(other) = owners.insert(uid, name) {
                bail!("tiles {other} and {name} share user ID {uid}");
            }
        }
        for (name, tile) in &self.tiles {
            if let Some(supervisor) = tile.supervisor_uid
                && let Some(other) = owners.get(&supervisor)
            {
                bail!("tile {name}: its supervisor is user ID {supervisor}, tile {other}'s");
            }
        }
        self.allocated = allocated;
        Ok(self)
    }

    /// Where the launcher records each tile's user ID.
    pub fn uids_file(&self) -> PathBuf {
        self.state_root.join(UIDS_FILE)
    }

    /// Every user ID the launcher has allocated, by tile name, including tiles root removed.
    pub fn allocated(&self) -> &BTreeMap<String, u32> {
        &self.allocated
    }

    /// The tile's user ID, once the launcher has allocated it.
    pub fn uid(&self, name: &str) -> Option<u32> {
        self.allocated.get(name).copied()
    }

    fn check(&self) -> Result<()> {
        for (label, root) in [
            ("state_root", &self.state_root),
            ("runtime_root", &self.runtime_root),
        ] {
            if !root.is_absolute() || root == Path::new("/") {
                bail!(
                    "{label}: {} must be an absolute directory, not /",
                    root.display()
                );
            }
        }
        for path in self
            .read_only
            .iter()
            .chain(&self.path)
            .chain(&self.sekrets_socket)
        {
            if !path.is_absolute() || path == Path::new("/") {
                bail!("{} must be an absolute path, not /", path.display());
            }
            if path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!("{} must not contain `..`", path.display());
            }
        }
        for path in &self.read_only {
            for private in [&self.state_root, &self.runtime_root] {
                if path.starts_with(private) || private.starts_with(path) {
                    bail!(
                        "read_only: {} overlaps {}, where tiles keep private state",
                        path.display(),
                        private.display()
                    );
                }
            }
        }
        let range = self.uids;
        if range.first == 0 || range.first > range.last || range.last >= u32::MAX >> 1 {
            bail!(
                "uids: {}-{} must be a range of user IDs above 0 and below 2^31",
                range.first,
                range.last
            );
        }
        if range.contains(65_534) || range.contains(65_535) {
            bail!("uids: the range must leave out 65534 and 65535, nobody and the invalid ID");
        }
        for &(first, last) in SYSTEMD_RESERVED {
            if range.overlaps(first, last) {
                bail!("uids: the range overlaps {first}-{last}, which systemd keeps for itself");
            }
        }
        for (name, tile) in &self.tiles {
            check_name(name)?;
            if !tile.agent.starts_with("agent/") || tile.agent.len() <= "agent/".len() {
                bail!(
                    "tile {name}: `{}` is not an agent such as agent/fleet/fixture-example/atlas",
                    tile.agent
                );
            }
            if crate::model::person_name(&tile.person).is_none() {
                bail!(
                    "tile {name}: `{}` is not a person such as person/ada",
                    tile.person
                );
            }
            if let Some(supervisor) = tile.supervisor_uid
                && range.contains(supervisor)
            {
                bail!(
                    "tile {name}: its supervisor, user ID {supervisor}, is in the tiles' range; \
                     a tile supervises no tile"
                );
            }
            if tile.tasks_max == 0 || tile.cpu_weight == 0 || tile.cpu_weight > 10_000 {
                bail!("tile {name}: tasks_max must be positive and cpu_weight 1 to 10000");
            }
            for value in std::iter::once(&tile.memory_max).chain(&tile.memory_high) {
                if !memory_value(value) {
                    bail!("tile {name}: `{value}` is not a size such as 8G or 512M");
                }
            }
        }
        Ok(())
    }

    pub fn tile(&self, name: &str) -> Result<&Tile> {
        self.tiles
            .get(name)
            .with_context(|| format!("no tile named {name} in the registry"))
    }

    /// The tile a user ID is, if any: one the launcher allocated to a tile the registry has.
    pub fn tile_for_uid(&self, uid: u32) -> Option<(&str, &Tile)> {
        let (name, _) = self.allocated.iter().find(|(_, owner)| **owner == uid)?;
        self.tiles
            .get_key_value(name)
            .map(|(name, tile)| (name.as_str(), tile))
    }

    /// The tile a process is, from the kernel's word alone: it runs as the tile's Unix user and
    /// inside the tile's slice, which only root can place it in.
    pub fn identify(&self, uid: u32, cgroup: &str) -> Option<(&str, &Tile)> {
        let (name, tile) = self.tile_for_uid(uid)?;
        cgroup
            .starts_with(&format!("{}/", slice_cgroup(name)))
            .then_some((name, tile))
    }

    pub fn tile_state(&self, name: &str) -> PathBuf {
        self.state_root.join(name)
    }

    pub fn tile_runtime(&self, name: &str) -> PathBuf {
        self.runtime_root.join(name)
    }

    /// The directory the tile's supervisor serves its endpoint in, bound into the tile.
    pub fn endpoint_dir(&self, name: &str) -> PathBuf {
        self.tile_runtime(name).join("endpoint")
    }
}

/// A tile's name: lower-case letters, digits and `_`, so it maps to systemd units unescaped.
pub fn check_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        bail!("`{name}` is not a tile name: up to 32 of a-z, 0-9 and _, starting with a letter");
    }
    Ok(())
}

fn memory_value(value: &str) -> bool {
    let digits = value.trim_end_matches(['K', 'M', 'G', 'T']);
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && value.len() - digits.len() <= 1
        && digits.parse::<u64>().is_ok_and(|n| n > 0)
}

/// The tile's own slice, which holds each of its compartments.
pub fn slice_unit(name: &str) -> String {
    format!("st_tiles-{name}.slice")
}

/// The unit the tile's tools run in.
pub fn tools_unit(name: &str) -> String {
    format!("st-tile-{name}-tools.service")
}

pub fn slice_cgroup(name: &str) -> String {
    format!("/{TILES_SLICE}/{}", slice_unit(name))
}

pub fn tools_cgroup(name: &str) -> String {
    format!("{}/{}", slice_cgroup(name), tools_unit(name))
}

/// Whether `path` and every directory above it can be changed only by root.
pub fn root_controlled(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(entry) = current {
        let metadata =
            fs::symlink_metadata(entry).with_context(|| format!("inspect {}", entry.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "{} is a symbolic link; name the file itself",
                entry.display()
            );
        }
        let mode = metadata.permissions().mode();
        if metadata.uid() != 0 || (mode & 0o022 != 0 && !(metadata.is_dir() && mode & 0o1000 != 0))
        {
            bail!(
                "{} can be changed by someone other than root (owner {}, mode {:o})",
                entry.display(),
                metadata.uid(),
                mode & 0o7777
            );
        }
        current = entry
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"
state_root = "/var/lib/st-tiles"
runtime_root = "/run/st-tiles"
read_only = ["/nix/store/example-tools"]

[tiles.atlas]
agent = "agent/fleet/fixture-example/atlas"
person = "person/ada"
supervisor_uid = 1000

[tiles.birch]
agent = "agent/fleet/fixture-example/birch"
person = "person/ada"
"#;

    const UIDS: &str = "atlas = 80001\nbirch = 80002\n";

    fn allocated(registry: &str, uids: &str) -> Result<Registry> {
        Registry::parse(registry)?.with_uids(uids)
    }

    #[test]
    fn a_registry_names_each_tile_by_uid_and_slice() {
        let registry = allocated(REGISTRY, UIDS).unwrap();
        let atlas_tools = tools_cgroup("atlas");
        assert_eq!(
            atlas_tools,
            "/st_tiles.slice/st_tiles-atlas.slice/st-tile-atlas-tools.service"
        );
        assert_eq!(registry.uid("atlas"), Some(80001));
        assert_eq!(
            registry.identify(80001, &atlas_tools).map(|(name, _)| name),
            Some("atlas")
        );
        // Another tile's user in this tile's cgroup, or this user outside it, is no one.
        assert!(registry.identify(80002, &atlas_tools).is_none());
        assert!(registry.identify(80001, &tools_cgroup("birch")).is_none());
        assert!(
            registry
                .identify(80001, "/user.slice/user-80001.slice/session-3.scope")
                .is_none()
        );
        assert!(registry.identify(80001, &slice_cgroup("atlas")).is_none());
        assert!(
            registry
                .identify(80001, "/st_tiles.slice/st_tiles-atlas.slicex/a")
                .is_none()
        );
        assert!(registry.identify(1000, &atlas_tools).is_none());
    }

    #[test]
    fn a_tile_has_a_user_id_only_once_the_launcher_allocates_one() {
        // Root's text names no user ID, and refuses one: the launcher allocates them.
        let registry = Registry::parse(REGISTRY).unwrap();
        assert_eq!(registry.uid("atlas"), None);
        assert!(registry.tile_for_uid(80001).is_none());
        let hand_made = REGISTRY.replace("[tiles.birch]\n", "[tiles.birch]\nuid = 80002\n");
        assert!(Registry::parse(&hand_made).is_err());
        // A tile root removed keeps its user ID, since its state may remain, but is no one.
        let removed = allocated(REGISTRY, "atlas = 80001\ncedar = 80003\n").unwrap();
        assert_eq!(removed.uid("cedar"), Some(80003));
        assert!(removed.tile_for_uid(80003).is_none());
    }

    #[test]
    fn allocated_user_ids_stay_in_the_range_and_apart() {
        assert!(allocated(REGISTRY, "atlas = 80001\nbirch = 80001\n").is_err());
        assert!(allocated(REGISTRY, "atlas = 79999\n").is_err());
        assert!(allocated(REGISTRY, "atlas = 0\n").is_err());
        assert!(allocated(REGISTRY, "at-las = 80001\n").is_err());
        // A tile's user ID is never its sibling's supervisor.
        let supervised = REGISTRY.replace("supervisor_uid = 1000", "supervisor_uid = 1001");
        let narrow = supervised.replace(
            "read_only",
            "uids = { first = 1001, last = 1002 }\nread_only",
        );
        assert!(
            Registry::parse(&narrow).is_err(),
            "a supervisor in the range"
        );
    }

    #[test]
    fn a_range_leaves_out_what_systemd_and_the_kernel_keep() {
        let with = |first: u32, last: u32| {
            Registry::parse(&REGISTRY.replace(
                "read_only",
                &format!("uids = {{ first = {first}, last = {last} }}\nread_only"),
            ))
        };
        assert!(with(90_000, 90_999).is_ok());
        assert!(with(0, 10).is_err(), "root");
        assert!(with(5, 4).is_err(), "empty");
        assert!(
            with(65_000, 65_600).is_err(),
            "nobody and systemd's dynamic users"
        );
        assert!(with(61_000, 61_200).is_err(), "systemd's dynamic users");
        assert!(with(500_000, 600_000).is_err(), "systemd's containers");
        assert!(with(2_147_483_640, 2_147_483_650).is_err(), "2^31 and above");
        assert!(with(1_900_000_000, 1_900_000_010).is_ok(), "above systemd's containers");
    }

    #[test]
    fn a_registry_refuses_ambiguous_or_unsafe_tiles() {
        let person = REGISTRY.replace("agent/fleet/fixture-example/birch", "person/ada");
        assert!(Registry::parse(&person).is_err());
        let supervised_by_tile =
            REGISTRY.replace("supervisor_uid = 1000", "supervisor_uid = 80002");
        assert!(Registry::parse(&supervised_by_tile).is_err());
        let private = REGISTRY.replace("/nix/store/example-tools", "/var/lib/st-tiles/birch");
        assert!(Registry::parse(&private).is_err());
        let host_root = REGISTRY.replace("/nix/store/example-tools", "/");
        assert!(Registry::parse(&host_root).is_err());
        let dashed = REGISTRY.replace("[tiles.birch]", "[tiles.bi-rch]");
        assert!(Registry::parse(&dashed).is_err());
        let size = REGISTRY.replace(
            "person = \"person/ada\"\nsupervisor_uid",
            "person = \"person/ada\"\nmemory_max = \"lots\"\nsupervisor_uid",
        );
        assert!(Registry::parse(&size).is_err());
        let unknown = format!("{REGISTRY}\nextra = 1\n");
        assert!(Registry::parse(&unknown).is_err());
    }

    #[test]
    fn tile_names_map_to_unescaped_units() {
        for name in ["atlas", "web_builder2"] {
            assert!(check_name(name).is_ok());
        }
        for name in [
            "",
            "Atlas",
            "2atlas",
            "at-las",
            "at.las",
            "at/las",
            &"a".repeat(33),
        ] {
            assert!(check_name(name).is_err(), "{name}");
        }
    }
}
