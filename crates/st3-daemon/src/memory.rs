//! Read the daemon's cgroup, including ancestor limits. Never inspects another service.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
#[cfg(any(target_os = "linux", test))]
use anyhow::{Context, anyhow, ensure};

#[derive(Debug)]
pub(crate) struct GroupMemory {
    pub path: PathBuf,
    pub max_bytes: Option<u64>,
    pub max_events: u64,
}

#[cfg(target_os = "linux")]
pub(crate) fn service_memory() -> Result<Option<Vec<GroupMemory>>> {
    groups(
        &std::fs::read_to_string("/proc/self/cgroup")?,
        &std::fs::read_to_string("/proc/self/mountinfo")?,
    )
    .map(Some)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn service_memory() -> Result<Option<Vec<GroupMemory>>> {
    Ok(None)
}

#[cfg(any(target_os = "linux", test))]
fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(any(target_os = "linux", test))]
fn groups(cgroup: &str, mounts: &str) -> Result<Vec<GroupMemory>> {
    let name = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| anyhow!("unified cgroup v2 membership unavailable"))?;
    ensure!(
        Path::new(name).is_absolute()
            && !Path::new(name)
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "invalid cgroup membership path"
    );
    let (mount, leaf) = mounts
        .lines()
        .find_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            if after.split_whitespace().next()? != "cgroup2" {
                return None;
            }
            let fields: Vec<_> = before.split_whitespace().collect();
            let root = PathBuf::from(unescape_mount(fields.get(3)?));
            let mount = PathBuf::from(unescape_mount(fields.get(4)?));
            let relative = Path::new(name).strip_prefix(root).ok()?;
            let leaf = mount.join(relative);
            Some((mount, leaf))
        })
        .ok_or_else(|| anyhow!("cgroup v2 mount for the daemon unavailable"))?;
    let mut result = Vec::new();
    let mut path = leaf.as_path();
    loop {
        let max_path = path.join("memory.max");
        match std::fs::read_to_string(&max_path) {
            // The hierarchy root has no memory controller files.
            Err(error) if path == mount && error.kind() == std::io::ErrorKind::NotFound => {}
            value => {
                let max = value.with_context(|| format!("read {}", max_path.display()))?;
                let max_bytes = if max.trim() == "max" {
                    None
                } else {
                    Some(
                        max.trim()
                            .parse()
                            .with_context(|| format!("parse {}", max_path.display()))?,
                    )
                };
                let events_path = path.join("memory.events");
                let events = std::fs::read_to_string(&events_path)
                    .with_context(|| format!("read {}", events_path.display()))?;
                let max_events = events
                    .lines()
                    .find_map(|line| {
                        let mut fields = line.split_whitespace();
                        (fields.next()? == "max").then(|| fields.next()).flatten()
                    })
                    .ok_or_else(|| anyhow!("{} has no max counter", events_path.display()))?
                    .parse()
                    .with_context(|| format!("parse {}", events_path.display()))?;
                result.push(GroupMemory {
                    path: path.to_path_buf(),
                    max_bytes,
                    max_events,
                });
            }
        }
        if path == mount {
            break;
        }
        path = path
            .parent()
            .ok_or_else(|| anyhow!("cgroup path escaped its mount"))?;
    }
    ensure!(!result.is_empty(), "cgroup memory controller unavailable");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(path: &Path, max: &str, hits: u64) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("memory.max"), max).unwrap();
        std::fs::write(
            path.join("memory.events"),
            format!("low 0\nhigh 0\nmax {hits}\noom 0\noom_kill 0\n"),
        )
        .unwrap();
    }

    #[test]
    fn finds_service_and_ancestor_limits_even_at_a_subtree_mount() {
        let directory = tempfile::tempdir().unwrap();
        let mount = directory.path().join("cgroup mount");
        group(&mount, "1073741824", 9);
        group(&mount.join("service"), "max", 0);
        let mounts = format!(
            "42 29 0:28 /tenant {} rw - cgroup2 cgroup rw",
            mount.display().to_string().replace(' ', "\\040")
        );
        let result = groups("0::/tenant/service\n", &mounts).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].max_bytes, None);
        assert_eq!(result[1].max_bytes, Some(1 << 30));
        assert_eq!(result[1].max_events, 9);
        assert_eq!(result[0].path, mount.join("service"));
    }

    #[test]
    fn skips_only_the_hierarchy_root_without_controller_files() {
        let directory = tempfile::tempdir().unwrap();
        group(&directory.path().join("service"), "536870912", 1225);
        let mounts = format!(
            "42 29 0:28 / {} rw - cgroup2 cgroup rw",
            directory.path().display()
        );
        let result = groups("0::/service", &mounts).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].max_bytes, Some(1 << 29));
        assert_eq!(result[0].max_events, 1225);
        std::fs::write(directory.path().join("service/memory.max"), "bad").unwrap();
        assert!(groups("0::/service", &mounts).is_err());
        assert!(groups("0::/missing", &mounts).is_err());
        assert!(groups("0::/../service", &mounts).is_err());
        assert!(groups("1:memory:/service", &mounts).is_err());
    }
}
