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
            message.push_str("; the API is not ready, retry after startup");
        }
        message
    }
}

fn sibling(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// A local reader uses the effective API socket, following its discovery link if present.
/// Unlocked files belong to an exited process and are ignored, including after PID reuse.
pub fn read(socket: &Path) -> Option<Readiness> {
    let socket = fs::read_link(socket).unwrap_or_else(|_| socket.to_path_buf());
    let file = OpenOptions::new()
        .read(true)
        .open(sibling(&socket, ".readiness.lock"))
        .ok()?;
    if lock(&file) {
        return None;
    }
    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
        return None;
    }
    let bytes = fs::read(sibling(&socket, ".readiness.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
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
            path: sibling(socket, ".readiness.json"),
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
