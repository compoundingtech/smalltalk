//! Free space on the filesystems a daemon writes to: its state directory and its members'
//! workspaces. A full filesystem fails builds and claim writes alike.

use std::path::Path;

const GIB: u64 = 1 << 30;

/// The space an unprivileged process can still use on one filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiskSpace {
    /// Identifies the filesystem, so paths on one filesystem are counted once.
    pub filesystem: u64,
    pub available: u64,
    pub total: u64,
}

impl DiskSpace {
    /// Under 2 GiB or 2% left. A cargo build or a claim store write can fail from here on.
    pub(crate) fn is_low(&self) -> bool {
        self.available < 2 * GIB || self.available.saturating_mul(50) < self.total
    }

    /// At least 4 GiB and 4% left. Between low and recovered, a raised item stays open, so a
    /// filesystem near the line does not raise and close an item on every build.
    pub(crate) fn has_recovered(&self) -> bool {
        self.available >= 4 * GIB && self.available.saturating_mul(25) >= self.total
    }

    pub(crate) fn describe(&self) -> String {
        let percent = if self.total == 0 {
            0.0
        } else {
            self.available as f64 * 100.0 / self.total as f64
        };
        format!(
            "{:.1} GiB of {:.1} GiB free ({percent:.1}%)",
            self.available as f64 / GIB as f64,
            self.total as f64 / GIB as f64,
        )
    }
}

/// Read the space left on the filesystem that holds `path`.
// The statvfs field types differ by platform, so a cast that is a no-op on one is needed on
// another.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn disk_space(path: &Path) -> std::io::Result<DiskSpace> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `stat` is valid for one `statvfs` write.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize as u64;
    Ok(DiskSpace {
        filesystem: stat.f_fsid as u64,
        available: (stat.f_bavail as u64).saturating_mul(block),
        total: (stat.f_blocks as u64).saturating_mul(block),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_space_has_a_margin_before_it_counts_as_recovered() {
        let space = |available_gib: u64, total_gib: u64| DiskSpace {
            filesystem: 1,
            available: available_gib * GIB,
            total: total_gib * GIB,
        };
        // A large disk is low under 2%, a small one under 2 GiB.
        assert!(space(15, 1000).is_low());
        assert!(!space(25, 1000).is_low());
        assert!(space(1, 32).is_low());
        assert!(!space(3, 32).is_low());
        // Recovery needs twice the room.
        assert!(!space(30, 1000).has_recovered());
        assert!(space(45, 1000).has_recovered());
        assert!(!space(3, 32).has_recovered());
        assert!(space(5, 32).has_recovered());
        assert_eq!(
            space(15, 1000).describe(),
            "15.0 GiB of 1000.0 GiB free (1.5%)"
        );
    }

    #[test]
    fn the_state_directory_filesystem_is_readable() {
        let root = tempfile::tempdir().unwrap();
        let space = disk_space(root.path()).unwrap();
        assert!(space.total > 0);
        assert!(space.available <= space.total);
        assert!(disk_space(&root.path().join("missing")).is_err());
    }
}
