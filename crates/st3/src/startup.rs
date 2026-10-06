//! Local startup observation independent of SQLite. A held file lock establishes the writer's
//! lifetime; a leftover JSON file after SIGKILL never means a live daemon is ready.

#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd as _;
#[cfg(target_os = "linux")]
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
        message
    }
}

fn sibling(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

// Resolve discovery links (including relative or dangling links), then canonicalize the
// directory. The API socket need not exist yet. Both publisher and readers use this spelling.
fn effective_socket(socket: &Path) -> std::io::Result<PathBuf> {
    let mut socket = std::path::absolute(socket)?;
    for _ in 0..40 {
        match fs::read_link(&socket) {
            Ok(target) => {
                socket = if target.is_absolute() {
                    target
                } else {
                    socket.parent().unwrap_or(Path::new(".")).join(target)
                };
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                ) =>
            {
                let name = socket.file_name().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket has no file name")
                })?;
                return Ok(fs::canonicalize(socket.parent().unwrap_or(Path::new(".")))?.join(name));
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "too many socket discovery links",
    ))
}

/// Private observations owned by the daemon at this socket, also removed by reset/uninstall.
pub fn paths(socket: &Path) -> [PathBuf; 2] {
    let socket = effective_socket(socket).unwrap_or_else(|_| socket.to_path_buf());
    [
        sibling(&socket, ".readiness.json"),
        sibling(&socket, ".readiness.lock"),
    ]
}

#[cfg(target_os = "linux")]
fn lock_query() -> libc::flock {
    // OFD locks belong to this open file description, so a reader closing its own descriptor
    // cannot release the daemon's lock. GETLK only queries; it never holds a competing lock.
    let mut query: libc::flock = unsafe { std::mem::zeroed() };
    query.l_type = libc::F_WRLCK as _;
    query.l_whence = libc::SEEK_SET as _;
    query
}

#[cfg(target_os = "linux")]
fn lock(file: &File) -> std::io::Result<()> {
    let mut query = lock_query();
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut query) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn writer_is_live(file: &File) -> bool {
    let mut query = lock_query();
    (unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut query) == 0 })
        && query.l_type == libc::F_WRLCK as libc::c_short
}

#[cfg(target_os = "linux")]
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        None
    }
    #[cfg(target_os = "linux")]
    {
        let [path, lock_path] = paths(socket);
        let file = OpenOptions::new().read(true).open(lock_path).ok()?;
        if !writer_is_live(&file) {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        let report: Readiness = serde_json::from_slice(&bytes).ok()?;
        // A new lock cannot validate the JSON of a crashed predecessor before first publish.
        (process_is_live(report.pid) && writer_is_live(&file)).then_some(report)
    }
}

pub struct Startup {
    path: Option<PathBuf>,
    _lock: Option<File>,
    status: Arc<RwLock<Readiness>>,
    warned: AtomicBool,
}

impl Startup {
    fn new(path: Option<PathBuf>, lock: Option<File>) -> Self {
        Self {
            path,
            _lock: lock,
            status: Arc::new(RwLock::new(Readiness {
                status: "starting".into(),
                phase: "open-store".into(),
                pid: std::process::id(),
                frontier: None,
                target: None,
                processed: None,
                total: None,
            })),
            warned: AtomicBool::new(false),
        }
    }

    fn disabled(reason: impl std::fmt::Display) -> Self {
        eprintln!(
            "st: startup readiness unavailable: {reason}; continuing without local readiness"
        );
        Self::new(None, None)
    }

    pub fn begin(socket: &Path) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = socket;
            Ok(Self::disabled(
                "local observation is supported only on Linux",
            ))
        }
        #[cfg(target_os = "linux")]
        {
            Self::begin_with_lock(socket, lock)
        }
    }

    #[cfg(target_os = "linux")]
    fn begin_with_lock(
        socket: &Path,
        acquire: impl FnOnce(&File) -> std::io::Result<()>,
    ) -> Result<Self> {
        let setup = || -> std::io::Result<(PathBuf, File)> {
            if let Some(parent) = socket.parent() {
                fs::create_dir_all(parent)?;
            }
            let [path, lock_path] = paths(socket);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(lock_path)?;
            Ok((path, file))
        };
        let (path, file) = match setup() {
            Ok(files) => files,
            Err(error) => return Ok(Self::disabled(error)),
        };
        if let Err(error) = acquire(&file) {
            // Only these SETLK errors mean contention. Unsupported locks, permissions on
            // the observation directory, and I/O failures must not prevent daemon startup.
            if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)) {
                return Err(error).with_context(|| {
                    format!(
                        "another daemon owns startup readiness at {}",
                        socket.display()
                    )
                });
            }
            return Ok(Self::disabled(error));
        }
        let startup = Self::new(Some(path), Some(file));
        let initialize = || -> Result<()> {
            let path = startup
                .path
                .as_ref()
                .expect("enabled observation has a path");
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            startup.publish()
        };
        if let Err(error) = initialize() {
            drop(startup);
            return Ok(Self::disabled(error));
        }
        Ok(startup)
    }

    fn publish(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let report = self
            .status
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
        serde_json::to_writer(&mut temporary, &report)?;
        temporary.flush()?;
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("publish startup readiness at {}", path.display()))?;
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
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn lock_contention_is_distinct_from_unavailable_observation() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        for errno in [
            libc::EINVAL,
            libc::EOPNOTSUPP,
            libc::ENOSYS,
            libc::EIO,
            libc::EPERM,
        ] {
            let startup = Startup::begin_with_lock(&socket, |_| {
                Err(std::io::Error::from_raw_os_error(errno))
            })
            .unwrap();
            startup.phase("full-replay/base-claims");
            startup.serving();
            assert!(
                startup.path.is_none(),
                "errno {errno} must disable observation"
            );
            assert!(read(&socket).is_none());
            assert!(!paths(&socket)[0].exists());
        }
        for errno in [libc::EACCES, libc::EAGAIN] {
            let error = Startup::begin_with_lock(&socket, |_| {
                Err(std::io::Error::from_raw_os_error(errno))
            })
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("another daemon owns startup readiness")
            );
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(errno)
            );
        }
        let blocked_parent = root.path().join("not-a-directory");
        fs::write(&blocked_parent, "fixture").unwrap();
        assert!(
            Startup::begin(&blocked_parent.join("api.sock"))
                .unwrap()
                .path
                .is_none()
        );
    }

    #[test]
    fn publisher_and_readers_share_symlinked_and_relative_socket_paths() {
        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical/run");
        fs::create_dir_all(&physical).unwrap();
        std::os::unix::fs::symlink("physical", root.path().join("alias")).unwrap();
        let alias = root.path().join("alias/run/api.sock");
        let real = physical.join("api.sock");
        let discovery_dir = root.path().join("discovery");
        fs::create_dir_all(&discovery_dir).unwrap();
        let discovery = discovery_dir.join("st3.sock");
        std::os::unix::fs::symlink("../alias/run/api.sock", &discovery).unwrap();
        let startup = Startup::begin(&alias).unwrap();
        assert_eq!(paths(&alias), paths(&real));
        assert_eq!(paths(&discovery), paths(&real));
        assert_eq!(startup.path.as_ref().unwrap(), &paths(&real)[0]);
        assert_eq!(
            read(&discovery).unwrap().status,
            "starting",
            "dangling discovery target"
        );
        assert!(Startup::begin(&real).is_err());
        let listener = std::os::unix::net::UnixListener::bind(&alias).unwrap();
        startup.serving();
        assert_eq!(read(&alias).unwrap().status, "serving");
        assert_eq!(read(&real).unwrap().status, "serving");
        assert_eq!(read(&discovery).unwrap().status, "serving");
        drop(listener);
    }

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
        let path = startup.path.clone().unwrap();
        drop(startup);
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let new_writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(sibling(&socket, ".readiness.lock"))
            .unwrap();
        lock(&new_writer).unwrap();
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
        let retained = fs::read(startup.path.as_ref().unwrap()).unwrap();
        let path = startup.path.clone().unwrap();
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

#[cfg(all(test, not(target_os = "linux")))]
mod unsupported_platform_tests {
    #[test]
    fn startup_continues_without_observation_on_other_platforms() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let startup = super::Startup::begin(&socket).unwrap();
        startup.phase("full-replay/base-claims");
        startup.serving();
        assert!(startup.path.is_none());
        assert!(super::read(&socket).is_none());
        assert!(super::paths(&socket).iter().all(|path| !path.exists()));
    }
}
