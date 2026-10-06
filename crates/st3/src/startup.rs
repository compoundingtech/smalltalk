//! Local startup observation independent of SQLite. A held file lock establishes the writer's
//! lifetime; a leftover JSON file after SIGKILL never means a live daemon is ready.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use smallclaims::store::ProjectionProgress;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Readiness {
    pub status: String,
    pub phase: String,
    pub pid: u32,
    pub frontier: Option<u64>,
    pub target: Option<u64>,
    pub processed: Option<u64>,
    pub total: Option<u64>,
}

impl Readiness {
    pub fn full_replay(&self) -> bool {
        self.phase == "full-replay" || self.phase.starts_with("full-replay/")
    }

    pub fn summary(&self) -> String {
        let mut message = format!("daemon {} · {}", self.status, self.phase);
        if let (Some(frontier), Some(target)) = (self.frontier, self.target) {
            message.push_str(&format!(" · committed frontier {frontier}/{target}"));
        }
        if let (Some(processed), Some(total)) = (self.processed, self.total) {
            message.push_str(&format!(
                " · replay claims {processed}/{total} (uncommitted)"
            ));
        }
        if self.status == "starting" {
            message.push_str("; the API is not ready");
        }
        message
    }
}

fn sibling(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Private observations owned by the daemon at this socket, also removed by reset/uninstall.
pub fn paths(socket: &Path) -> [PathBuf; 2] {
    [
        sibling(socket, ".readiness.json"),
        sibling(socket, ".readiness.lock"),
    ]
}

fn lock_query() -> libc::flock {
    // OFD locks belong to this open file description, so a reader closing its own descriptor
    // cannot release the daemon's lock. GETLK only queries; it never holds a competing lock.
    let mut query: libc::flock = unsafe { std::mem::zeroed() };
    query.l_type = libc::F_WRLCK as _;
    query.l_whence = libc::SEEK_SET as _;
    query
}

fn lock(file: &File) -> bool {
    let mut query = lock_query();
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut query) == 0 }
}

fn writer_is_live(file: &File) -> bool {
    let mut query = lock_query();
    (unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut query) == 0 })
        && query.l_type == libc::F_WRLCK as libc::c_short
}

fn process_is_live(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied)
}

/// A local reader uses the effective API socket, following its discovery link if present.
/// Unlocked files belong to an exited process and are ignored, including after PID reuse.
pub fn read(socket: &Path) -> Option<Readiness> {
    let socket = fs::read_link(socket).unwrap_or_else(|_| socket.to_path_buf());
    let file = OpenOptions::new()
        .read(true)
        .open(sibling(&socket, ".readiness.lock"))
        .ok()?;
    if !writer_is_live(&file) {
        return None;
    }
    let bytes = fs::read(sibling(&socket, ".readiness.json")).ok()?;
    let report: Readiness = serde_json::from_slice(&bytes).ok()?;
    // Between the new writer acquiring its lock and its first publication, JSON can still
    // name a crashed predecessor. It is not this daemon's readiness.
    (process_is_live(report.pid) && writer_is_live(&file)).then_some(report)
}

pub struct Startup {
    path: PathBuf,
    _lock: File,
    status: Arc<RwLock<Readiness>>,
    warned: AtomicBool,
}

impl Startup {
    pub fn begin(socket: &Path) -> Result<Self> {
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(sibling(socket, ".readiness.lock"))?;
        anyhow::ensure!(
            lock(&file),
            "another daemon owns startup readiness at {}",
            socket.display()
        );
        // Discard the previous publication before initializing this incarnation.
        let path = sibling(socket, ".readiness.json");
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let status = Arc::new(RwLock::new(Readiness {
            status: "starting".into(),
            phase: "open-store".into(),
            pid: std::process::id(),
            frontier: None,
            target: None,
            processed: None,
            total: None,
        }));
        let startup = Self {
            path,
            _lock: file,
            status,
            warned: AtomicBool::new(false),
        };
        startup.publish()?;
        Ok(startup)
    }

    fn publish(&self) -> Result<()> {
        let report = self
            .status
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut temporary =
            tempfile::NamedTempFile::new_in(self.path.parent().unwrap_or(Path::new(".")))?;
        serde_json::to_writer(&mut temporary, &report)?;
        temporary.flush()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("publish startup readiness at {}", self.path.display()))?;
        Ok(())
    }

    fn update(&self, change: impl FnOnce(&mut Readiness)) {
        change(&mut self.status.write().unwrap_or_else(PoisonError::into_inner));
        if let Err(error) = self.publish()
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!("st: cannot publish startup readiness: {error:#}");
        }
    }

    pub fn phase(&self, phase: &str) {
        self.update(|status| {
            status.phase = phase.into();
            status.processed = None;
            status.total = None;
        });
    }

    pub fn progress(&self, progress: ProjectionProgress) {
        self.update(|status| {
            status.phase = progress.phase.into();
            status.frontier = Some(progress.frontier);
            status.target = Some(progress.target);
            status.processed = progress.processed;
            status.total = progress.total;
        });
        #[cfg(feature = "test-support")]
        if progress.phase == "full-replay/base-claims" && progress.processed == Some(0) {
            crate::test_support::pause_startup_replay();
        }
    }

    pub fn serving(&self) {
        self.update(|status| {
            status.status = "serving".into();
            status.phase = "ready".into();
            status.processed = None;
            status.total = None;
        });
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_queries_do_not_lock_out_a_starting_writer() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sibling(&socket, ".readiness.lock"))
            .unwrap();
        assert!(!writer_is_live(&file));
        let startup = Startup::begin(&socket).unwrap();
        for _ in 0..100 {
            assert!(writer_is_live(&file));
            assert!(read(&socket).is_some());
        }
        drop(file);
        assert!(
            read(&socket).is_some(),
            "closing a reader cannot release the writer lock"
        );
        drop(startup);
    }

    #[test]
    fn a_new_writer_cannot_validate_a_crashed_predecessors_json() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let startup = Startup::begin(&socket).unwrap();
        let mut stale = read(&socket).unwrap();
        let mut predecessor = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        stale.pid = predecessor.id();
        predecessor.wait().unwrap();
        stale.status = "serving".into();
        stale.phase = "ready".into();
        let path = startup.path.clone();
        drop(startup);
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let new_writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(sibling(&socket, ".readiness.lock"))
            .unwrap();
        assert!(lock(&new_writer));
        assert!(
            read(&socket).is_none(),
            "a held new lock does not make the old PID live"
        );
        drop(new_writer);
        let restarted = Startup::begin(&socket).unwrap();
        assert_eq!(read(&socket).unwrap().pid, std::process::id());
        drop(restarted);
    }

    #[test]
    fn readiness_requires_a_live_lock_and_a_second_writer_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let startup = Startup::begin(&socket).unwrap();
        assert_eq!(read(&socket).unwrap().status, "starting");
        assert!(Startup::begin(&socket).is_err());
        startup.serving();
        assert_eq!(read(&socket).unwrap().status, "serving");
        let retained = fs::read(&startup.path).unwrap();
        let path = startup.path.clone();
        drop(startup);
        assert!(read(&socket).is_none());
        fs::write(&path, retained).unwrap();
        assert!(
            read(&socket).is_none(),
            "unlocked stale readiness is ignored"
        );
        let restarted = Startup::begin(&socket).unwrap();
        assert_eq!(read(&socket).unwrap().status, "starting");
        drop(restarted);
    }
}
