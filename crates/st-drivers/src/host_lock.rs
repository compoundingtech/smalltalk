//! The single-owner guard (post-v0 item b) — one supervising st2 per **(folder, host)**.
//!
//! Two st2 loops reconciling the same folder for the same host would double-spawn every agent and
//! fight over liveness. A pid-file lock prevents it and is scoped by host
//! (`<root>/.st2.<host>.lock`), because the folder is a *synced* catalog: host A's lock file
//! syncs to host B, and B must ignore it (B only reads `.st2.<B>.lock`). The lock is dot-prefixed so
//! discovery skips it. A crashed owner leaves a stale lock (dead pid) which the next start reclaims.

use std::fs;
use std::path::{Path, PathBuf};

/// A pid-file lock for one host's supervision of one folder.
pub struct HostLock {
    path: PathBuf,
}

impl HostLock {
    pub fn new(root: &Path, host: &str) -> Self {
        Self {
            path: root.join(format!(".st2.{host}.lock")),
        }
    }

    pub fn pid_path(&self) -> &Path {
        &self.path
    }

    /// The pid of a *live, foreign* st2 already supervising this (folder, host), or `None` (no lock,
    /// a stale lock from a dead pid, or the lock is ours).
    pub fn live_owner(&self) -> Option<i32> {
        let raw = fs::read_to_string(&self.path).ok()?;
        let pid: i32 = raw.trim().parse().ok()?;
        if pid == std::process::id() as i32 {
            return None; // us
        }
        process_alive(pid).then_some(pid)
    }

    /// True if a lock file exists but its owner is dead (a crashed st2 left it behind).
    pub fn has_stale_lock(&self) -> bool {
        self.path.exists() && self.live_owner().is_none()
    }

    /// Write our pid as the owner. Call only after [`live_owner`](Self::live_owner) is `None`.
    pub fn acquire(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, std::process::id().to_string())
    }

    /// Remove the lock — but only if it is still ours (never clobber a live successor's lock).
    pub fn release(&self) {
        if let Ok(raw) = fs::read_to_string(&self.path)
            && raw.trim().parse::<u32>().ok() == Some(std::process::id())
        {
            let _ = fs::remove_file(&self.path);
        }
    }

    /// The one clear, actionable message shown when another st2 owns this (folder, host).
    pub fn busy_warning(&self, owner_pid: i32) -> String {
        format!(
            "another st2 is already supervising this folder for this host (pid {owner_pid}) — refusing to start.\n\
             Two supervisors on one (folder, host) double-spawn every agent. Stop the other one first.\n\
             If it is already gone, clear the stale lock:  rm {}",
            self.path.display()
        )
    }
}

/// Whether `pid` is a live process. `kill(pid, 0)` succeeds for a live process we may signal, and
/// fails with `EPERM` for a live process we may NOT signal (still alive) — both count as alive.
pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = HostLock::new(tmp.path(), "example-linux");
        assert!(lock.live_owner().is_none());
        assert!(!lock.has_stale_lock());

        lock.acquire().unwrap();
        assert!(lock.pid_path().exists());
        assert!(
            lock.live_owner().is_none(),
            "our own lock is not a foreign owner"
        );

        lock.release();
        assert!(!lock.pid_path().exists());
    }

    #[test]
    fn lock_path_is_host_scoped_and_dot_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let a = HostLock::new(tmp.path(), "example-linux");
        let b = HostLock::new(tmp.path(), "example-mac");
        assert_ne!(a.pid_path(), b.pid_path(), "per-host lock files");
        assert!(
            a.pid_path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with('.')
        );
    }

    #[test]
    fn stale_lock_from_dead_pid_is_detected_and_not_an_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = HostLock::new(tmp.path(), "example-linux");
        fs::write(lock.pid_path(), "2000000000").unwrap(); // almost certainly dead
        assert!(lock.live_owner().is_none());
        assert!(lock.has_stale_lock());
    }

    #[test]
    fn a_live_foreign_pid_is_reported_as_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = HostLock::new(tmp.path(), "example-linux");
        fs::write(lock.pid_path(), "1").unwrap(); // init — always alive, not us
        assert_eq!(lock.live_owner(), Some(1));
    }

    #[test]
    fn release_does_not_clobber_a_foreign_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = HostLock::new(tmp.path(), "example-linux");
        fs::write(lock.pid_path(), "1").unwrap(); // not ours
        lock.release();
        assert!(lock.pid_path().exists(), "must not remove a foreign lock");
    }

    #[test]
    fn process_alive_basics() {
        assert!(process_alive(std::process::id() as i32)); // ourselves
        assert!(!process_alive(0));
        assert!(!process_alive(-1));
        assert!(!process_alive(2_000_000_000)); // dead
    }
}
