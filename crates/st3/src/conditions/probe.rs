//! The host facts conditions need that the daemon does not otherwise measure: free space on each
//! local filesystem, available memory, and the CPU and memory of processes by name. Each is read
//! from the kernel by plain code, once per evaluation and only for a condition that asks for it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::disk::{DiskSpace, disk_space};

#[derive(Clone, Default)]
struct DiskRequests {
    all: bool,
    paths: std::collections::BTreeSet<String>,
}

#[derive(Default)]
struct DiskSnapshot {
    at: u128,
    all: BTreeMap<String, DiskSpace>,
    paths: BTreeMap<String, DiskSpace>,
    error: Option<String>,
}

#[derive(Default)]
struct DiskCache {
    requests: std::sync::Mutex<DiskRequests>,
    snapshot: std::sync::Mutex<DiskSnapshot>,
}

/// One native-filesystem worker per daemon. A stuck kernel call makes disk readings stale;
/// the evaluator and its other metrics continue, and no replacement workers accumulate.
#[derive(Clone)]
pub(crate) struct DiskSampler(std::sync::Arc<DiskCache>);

impl DiskSampler {
    pub fn spawn() -> Self {
        Self::spawn_with(|requests| {
            let mut snapshot = DiskSnapshot::default();
            if requests.all {
                snapshot.all = filesystems();
            }
            for path in &requests.paths {
                if let Some(space) = filesystem_of(path) {
                    snapshot.paths.insert(path.clone(), space);
                } else {
                    snapshot.error = Some(format!(
                        "unavailable native filesystem selector: {}",
                        path.chars().take(256).collect::<String>()
                    ));
                }
            }
            snapshot
        })
    }

    fn spawn_with(read: impl Fn(&DiskRequests) -> DiskSnapshot + Send + Sync + 'static) -> Self {
        let cache = std::sync::Arc::new(DiskCache::default());
        let weak = std::sync::Arc::downgrade(&cache);
        let read = std::sync::Arc::new(read);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(cache) = weak.upgrade() else { break };
                let requests = cache
                    .requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                if !requests.all && requests.paths.is_empty() {
                    continue;
                }
                let read = read.clone();
                let mut task = tokio::task::spawn_blocking(move || read(&requests));
                match tokio::time::timeout(std::time::Duration::from_secs(10), &mut task).await {
                    Ok(Ok(mut snapshot)) => {
                        snapshot.at = super::now_ms();
                        *cache
                            .snapshot
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = snapshot;
                    }
                    Ok(Err(error)) => {
                        cache
                            .snapshot
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .error = Some(format!("disk probe failed: {error}"))
                    }
                    Err(_) => {
                        cache
                            .snapshot
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .error = Some(
                            "disk probe exceeded 10 seconds; other condition metrics continue"
                                .into(),
                        );
                        // Discard late data: earlier mounts in that result may already be stale.
                        let _ = task.await;
                    }
                }
            }
        });
        Self(cache)
    }

    pub fn configure(&self, all: bool, paths: impl Iterator<Item = String>) {
        *self
            .0
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = DiskRequests {
            all,
            paths: paths.take(super::MAX_CONDITIONS).collect(),
        };
    }

    pub fn filesystems(&self) -> BTreeMap<String, DiskSpace> {
        self.0
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .all = true;
        let snapshot = self
            .0
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if super::now_ms() < snapshot.at
            || super::now_ms().saturating_sub(snapshot.at) > super::STALE_AFTER_MS
        {
            BTreeMap::new()
        } else {
            snapshot.all.clone()
        }
    }

    pub fn filesystem_of(&self, path: &str) -> Option<DiskSpace> {
        let mut requests = self
            .0
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if requests.paths.len() < super::MAX_CONDITIONS {
            requests.paths.insert(path.to_owned());
        }
        drop(requests);
        let snapshot = self
            .0
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (super::now_ms() >= snapshot.at
            && super::now_ms().saturating_sub(snapshot.at) <= super::STALE_AFTER_MS)
            .then(|| snapshot.paths.get(path).cloned())
            .flatten()
    }

    pub fn errors(&self) -> Vec<String> {
        let requests = self
            .0
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !requests.all && requests.paths.is_empty() {
            return Vec::new();
        }
        drop(requests);
        self.0
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .clone()
            .into_iter()
            .collect()
    }
}

/// Filesystem types that hold data a host can run out of room on. Everything else in the mount
/// table (proc, sysfs, cgroups, tmpfs, overlays, network filesystems) is skipped.
const LOCAL_FILESYSTEMS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "bcachefs", "jfs", "reiserfs", "vfat",
    "exfat", "ntfs", "ntfs3", "apfs", "hfs",
];

/// The mount points of local filesystems, from `/proc/self/mountinfo` text. A mount point that
/// is the same filesystem mounted again (a bind mount, a btrfs subvolume) is kept here and
/// counted once when its space is read.
pub fn mount_points(mountinfo: &str) -> Vec<String> {
    let mut mounts = Vec::new();
    for line in mountinfo.lines() {
        // `36 35 98:0 /root /mnt rw,noatime master:1 - ext3 /dev/root rw,errors=continue`
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount) = before.split(' ').nth(4) else {
            continue;
        };
        let Some(kind) = after.split(' ').next() else {
            continue;
        };
        if LOCAL_FILESYSTEMS.contains(&kind) {
            mounts.push(unescape(mount));
        }
    }
    mounts
}

/// The kernel writes a space in a path as `\040`, and likewise tab, newline and backslash.
fn unescape(path: &str) -> String {
    let mut output = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4].iter().all(u8::is_ascii_digit)
            && let Ok(code) = u8::from_str_radix(&path[index + 1..index + 4], 8)
        {
            output.push(code);
            index += 4;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| path.to_owned())
}

/// Free space on each local filesystem, keyed by its first mount point. A host without a mount
/// table produces no disk reading.
pub(crate) fn filesystems() -> BTreeMap<String, DiskSpace> {
    #[cfg(not(target_os = "macos"))]
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map(|text| mount_points(&text))
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    let mounts = apple_mount_points();
    let mut seen = std::collections::BTreeSet::new();
    let mut output = BTreeMap::new();
    for mount in mounts.into_iter().take(256) {
        if let Ok(space) = disk_space(Path::new(&mount))
            && space.total > 0
            && seen.insert(space.filesystem)
        {
            output.insert(mount, space);
        }
    }
    output
}

/// Caller-owned mount table: unlike getmntinfo, getfsstat does not return shared static storage.
#[cfg(target_os = "macos")]
fn apple_mounts() -> Vec<(String, String)> {
    let mut mounts = Vec::<libc::statfs>::with_capacity(256);
    let bytes = mounts.capacity() * std::mem::size_of::<libc::statfs>();
    // SAFETY: the allocation can hold 256 statfs records. MNT_NOWAIT uses cached kernel facts.
    let count =
        unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes as libc::c_int, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Vec::new();
    }
    // SAFETY: getfsstat initialized the returned records, bounded by the supplied buffer.
    unsafe {
        mounts.set_len((count as usize).min(mounts.capacity()));
    }
    mounts
        .iter()
        .filter_map(|mount| {
            // SAFETY: the kernel's fixed-size filesystem and mount names are NUL-terminated.
            let kind = unsafe { std::ffi::CStr::from_ptr(mount.f_fstypename.as_ptr()) }
                .to_str()
                .ok()?;
            Some((
                unsafe { std::ffi::CStr::from_ptr(mount.f_mntonname.as_ptr()) }
                    .to_str()
                    .ok()?
                    .to_owned(),
                kind.to_owned(),
            ))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn apple_mount_points() -> Vec<String> {
    apple_mounts()
        .into_iter()
        .filter(|(_, kind)| LOCAL_FILESYSTEMS.contains(&kind.as_str()))
        .map(|(mount, _)| mount)
        .collect()
}

/// Free space on the filesystem holding `path`.
/// Select the enclosing native mount from kernel facts, then stat the mount itself.
/// Never stat an authored path: it might traverse a symlink, autofs or a network mount.
pub(crate) fn filesystem_of(path: &str) -> Option<DiskSpace> {
    if !Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    #[cfg(target_os = "linux")]
    let mount = local_mount_for(path, &std::fs::read_to_string("/proc/self/mountinfo").ok()?)?;
    #[cfg(target_os = "macos")]
    let mount = apple_mounts()
        .into_iter()
        .filter(|(mount, _)| Path::new(path).starts_with(mount))
        .max_by_key(|(mount, _)| mount.len())
        .filter(|(_, kind)| LOCAL_FILESYSTEMS.contains(&kind.as_str()))?
        .0;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return None;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut prefix = PathBuf::new();
        for component in Path::new(path).components() {
            prefix.push(component.as_os_str());
            match std::fs::symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => return None,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    disk_space(Path::new(&mount))
        .ok()
        .filter(|space| space.total > 0)
}

/// An explicit selector cannot traverse a more-specific foreign mount.
#[cfg(any(target_os = "linux", test))]
fn local_mount_for(path: &str, mountinfo: &str) -> Option<String> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let (mount, kind) = mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount = unescape(before.split(' ').nth(4)?);
            let kind = after.split(' ').next()?;
            path.starts_with(&mount).then_some((mount, kind))
        })
        .max_by_key(|(mount, _)| mount.len())?;
    LOCAL_FILESYSTEMS.contains(&kind).then_some(mount)
}

pub(crate) fn free_percent(space: &DiskSpace) -> f64 {
    space.available as f64 * 100.0 / space.total as f64
}

/// `MemAvailable` as a percent of `MemTotal`, from `/proc/meminfo` text.
pub fn memory_available_percent(meminfo: &str) -> Option<f64> {
    let field = |name: &str| {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    (total > 0.0).then(|| available * 100.0 / total)
}

pub fn read_memory_available_percent() -> Option<f64> {
    memory_available_percent(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// What `/proc/PID/stat` says of one process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessStat {
    /// User and system CPU, in clock ticks.
    pub cpu_ticks: u64,
    /// When it started, in clock ticks after boot: with the PID, it names one process.
    pub start_ticks: u64,
    pub rss_pages: u64,
}

/// Parse `/proc/PID/stat`. The name in parentheses can hold spaces and parentheses, so the fields
/// are counted from the last `)`.
pub fn parse_stat(text: &str) -> Option<ProcessStat> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    // After the name: state is field 3, utime 14, stime 15, starttime 22, rss 24.
    let field = |number: usize| fields.get(number - 3)?.parse::<u64>().ok();
    Some(ProcessStat {
        cpu_ticks: field(14)?.saturating_add(field(15)?),
        start_ticks: field(22)?,
        rss_pages: field(24)?,
    })
}

/// One reading of the processes with a given name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessReading {
    /// Cores used since the previous reading, or None on the first.
    pub cpu_cores: Option<f64>,
    pub rss_bytes: f64,
}

type ProcessTicks = HashMap<(u32, u64), u64>;

/// Samples processes by name, keeping each process's CPU from the last sample so the next can
/// tell how much it used in between.
pub struct ProcessSampler {
    root: PathBuf,
    ticks_per_second: f64,
    page_size: f64,
    /// Per name: when it was last read and each process's CPU ticks then.
    last: HashMap<String, (u128, ProcessTicks)>,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        // SAFETY: sysconf reads a constant.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        // SAFETY: as above.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as f64;
        Self::new(PathBuf::from("/proc"), ticks, page)
    }
}

impl ProcessSampler {
    pub fn new(root: PathBuf, ticks_per_second: f64, page_size: f64) -> Self {
        Self {
            root,
            ticks_per_second,
            page_size,
            last: HashMap::new(),
        }
    }

    /// Read every process whose `comm` is one of `names`, in one pass over the process table.
    /// A name with no running process has no reading.
    pub fn sample(&mut self, names: &[&str], now: u128) -> BTreeMap<String, ProcessReading> {
        let names_set = names
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let mut found: HashMap<&str, HashMap<(u32, u64), ProcessStat>> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten().take(16_384) {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                let directory = entry.path();
                let Ok(comm) = std::fs::read_to_string(directory.join("comm")) else {
                    continue;
                };
                let comm = comm.trim_end_matches('\n');
                let Some(name) = names_set.get(comm) else {
                    continue;
                };
                let Some(stat) = std::fs::read_to_string(directory.join("stat"))
                    .ok()
                    .and_then(|text| parse_stat(&text))
                else {
                    continue;
                };
                found
                    .entry(name)
                    .or_default()
                    .insert((pid, stat.start_ticks), stat);
            }
        }
        let mut readings = BTreeMap::new();
        for name in names {
            let Some(processes) = found.remove(name) else {
                self.last.remove(*name);
                continue;
            };
            let previous = self.last.get(*name);
            let cpu_cores = previous.and_then(|(at, ticks)| {
                let seconds = now.saturating_sub(*at) as f64 / 1_000.0;
                (seconds > 0.0).then(|| {
                    // A process that started since the last sample counts from zero.
                    let used = processes
                        .iter()
                        .map(|(key, stat)| {
                            stat.cpu_ticks
                                .saturating_sub(ticks.get(key).copied().unwrap_or(0))
                        })
                        .sum::<u64>();
                    used as f64 / self.ticks_per_second / seconds
                })
            });
            let rss_bytes = processes
                .values()
                .map(|stat| stat.rss_pages as f64 * self.page_size)
                .sum();
            self.last.insert(
                (*name).to_owned(),
                (
                    now,
                    processes
                        .iter()
                        .map(|(key, stat)| (*key, stat.cpu_ticks))
                        .collect(),
                ),
            );
            readings.insert(
                (*name).to_owned(),
                ProcessReading {
                    cpu_cores,
                    rss_bytes,
                },
            );
        }
        readings
    }
}

/// The claim database and its write-ahead log, in bytes.
pub fn database_bytes(database: &Path) -> Option<f64> {
    let main = std::fs::metadata(database).ok()?.len();
    let mut wal = database.as_os_str().to_owned();
    wal.push("-wal");
    let wal = std::fs::metadata(PathBuf::from(wal))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Some((main + wal) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_filesystems_come_from_the_mount_table() {
        let mountinfo = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
23 22 0:21 / /proc rw,nosuid shared:12 - proc proc rw
24 22 0:5 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw,size=4096k
25 22 259:3 / /srv/data\\040disk rw,relatime shared:3 - xfs /dev/nvme1n1 rw
26 22 0:30 / /run rw,nosuid shared:5 - tmpfs tmpfs rw
27 22 0:31 /@home /home rw,relatime shared:6 - btrfs /dev/sda1 rw
";
        assert_eq!(mount_points(mountinfo), ["/", "/srv/data disk", "/home"]);
    }

    #[test]
    fn authored_paths_do_not_select_foreign_mounts_or_parent_traversals() {
        let mounts = "1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 0:2 / /net rw - nfs server:/a rw\n3 1 0:3 / /auto rw - autofs auto rw\n4 1 0:4 / /fuse rw - fuse.sshfs remote rw\n";
        assert_eq!(local_mount_for("/srv/data", mounts).as_deref(), Some("/"));
        for path in [
            "/net/data",
            "/auto/data",
            "/fuse/data",
            "/srv/../net",
            "relative",
        ] {
            assert_eq!(local_mount_for(path, mounts), None, "{path}");
        }
    }

    #[tokio::test]
    async fn a_blocked_disk_worker_does_not_block_cached_reads() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = std::sync::Mutex::new(release_rx);
        let sampler = DiskSampler::spawn_with(move |_| {
            if let Some(sender) = entered.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let _ = release.lock().unwrap().recv();
            DiskSnapshot::default()
        });
        sampler.configure(true, std::iter::empty());
        let started = tokio::time::timeout(std::time::Duration::from_secs(2), entered_rx).await;
        let reads = sampler.filesystems();
        let errors = sampler.errors();
        release_tx.send(()).unwrap();
        assert!(started.is_ok());
        assert!(reads.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn memory_is_available_over_total() {
        let meminfo =
            "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(memory_available_percent(meminfo), Some(25.0));
        assert_eq!(memory_available_percent("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn a_stat_line_is_read_past_a_name_with_spaces_and_parentheses() {
        let line = "1234 (my (odd) proc) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 4 0 9876 123456789 2048 18446744073709551615";
        assert_eq!(
            parse_stat(line),
            Some(ProcessStat {
                cpu_ticks: 300,
                start_ticks: 9876,
                rss_pages: 2048,
            })
        );
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn processes_are_sampled_by_name_and_cpu_counts_from_the_previous_sample() {
        let root = tempfile::tempdir().unwrap();
        let write = |pid: u32, comm: &str, ticks: u64, start: u64| {
            let directory = root.path().join(pid.to_string());
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(
                directory.join("stat"),
                format!(
                    "{pid} ({comm}) S 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 {start} 1000 10 0"
                ),
            )
            .unwrap();
        };
        write(10, "collector", 100, 5);
        write(11, "collector", 50, 6);
        write(12, "other", 999, 7);
        std::fs::create_dir_all(root.path().join("self")).unwrap();
        let mut sampler = ProcessSampler::new(root.path().to_path_buf(), 100.0, 4096.0);
        let first = sampler.sample(&["collector", "absent"], 0);
        assert_eq!(first["collector"].cpu_cores, None);
        assert_eq!(first["collector"].rss_bytes, 2.0 * 10.0 * 4096.0);
        assert!(!first.contains_key("absent"));
        // Ten seconds later the two used 150 and 50 more ticks: 2 seconds of CPU in 10 seconds.
        write(10, "collector", 250, 5);
        write(11, "collector", 100, 6);
        let second = sampler.sample(&["collector"], 10_000);
        assert_eq!(second["collector"].cpu_cores, Some(0.2));
        // A PID reused by a new process counts from zero, not from the old one's ticks.
        write(11, "collector", 30, 60);
        let third = sampler.sample(&["collector"], 20_000);
        assert_eq!(third["collector"].cpu_cores, Some(0.03));
    }

    #[test]
    fn the_database_counts_its_write_ahead_log() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("claims.sqlite3");
        std::fs::write(&database, vec![0; 100]).unwrap();
        assert_eq!(database_bytes(&database), Some(100.0));
        std::fs::write(root.path().join("claims.sqlite3-wal"), vec![0; 20]).unwrap();
        assert_eq!(database_bytes(&database), Some(120.0));
        assert_eq!(database_bytes(&root.path().join("missing")), None);
    }
}
