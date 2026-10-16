//! Tiles: each agent as its own Unix user, in its own namespaces and root-owned cgroup.
//!
//! Linux first. Root's registry names each tile's agent, and the launcher allocates its user ID
//! from the registry's range. `st tiles run` starts the
//! tile's tools in a systemd unit under the tile's slice; the unit's process makes mount, PID,
//! IPC, network, UTS and cgroup namespaces, builds a file system from an empty root and the
//! registry's read-only paths, and drops to the tile's user with no capabilities, no new
//! privileges and a filter against new namespaces and mounts. See `docs/st3/tiles.md`.

pub mod cli;
#[cfg(target_os = "linux")]
pub mod launch;
#[cfg(target_os = "linux")]
pub mod seccomp;
#[cfg(target_os = "linux")]
pub mod uids;
