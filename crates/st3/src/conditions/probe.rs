//! The host facts conditions need that the daemon does not otherwise measure: free space on each
//! local filesystem, available memory, and the CPU and memory of processes by name. Each is read
//! from the kernel by plain code, once per evaluation and only for a condition that asks for it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::disk::{DiskSpace, disk_space};

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
/// table reads `/` alone.
pub(crate) fn filesystems() -> BTreeMap<String, DiskSpace> {
    #[cfg(not(target_os = "macos"))]
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map(|text| mount_points(&text))
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    let mounts = apple_mount_points();
    let mounts = if mounts.is_empty() {
        vec!["/".to_owned()]
    } else {
        mounts
    };
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
fn apple_mount_points() -> Vec<String> {
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
            if !LOCAL_FILESYSTEMS.contains(&kind) {
                return None;
            }
            Some(
                unsafe { std::ffi::CStr::from_ptr(mount.f_mntonname.as_ptr()) }
                    .to_str()
                    .ok()?
                    .to_owned(),
            )
        })
        .collect()
}

/// Free space on the filesystem holding `path`.
pub(crate) fn filesystem_of(path: &str) -> Option<DiskSpace> {
    disk_space(Path::new(path))
        .ok()
        .filter(|space| space.total > 0)
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

/// Samples processes by name, keeping each process's CPU from the last sample so the next can
/// tell how much it used in between.
pub struct ProcessSampler {
    root: PathBuf,
    ticks_per_second: f64,
    page_size: f64,
    /// Per name: when it was last read and each process's CPU ticks then.
    last: HashMap<String, (u128, HashMap<(u32, u64), u64>)>,
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
                let Some(name) = names.iter().find(|name| **name == comm) else {
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
