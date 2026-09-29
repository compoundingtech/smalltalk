//! The wrapper-owned process group a Codex provider launcher runs in.
//!
//! Moved verbatim out of the parent module: the group's lifetime, its close-on-exec discipline,
//! and the spawn that establishes it.

use std::fs;
use std::os::unix::io::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::provider_session::ProviderProcess;

/// One app-server process group and the write end of its wrapper-liveness channel.
///
/// A watchdog in the dedicated process group owns the read end. The watchdog kills only that
/// group if this wrapper disappears without running Rust cleanup. Its membership also prevents
/// the operating system from reusing the group ID before cleanup.
pub(super) struct OwnedProcessGroup {
    child: ProviderProcess,
    watchdog: ProviderProcess,
    owner_write: Option<UnixStream>,
    socket_path: Option<PathBuf>,
    active: bool,
}

/// An app-server group released for adoption by the driver's next image.
pub(super) struct ReleasedProcessGroup {
    pub(super) server_pid: u32,
    pub(super) watchdog_pid: u32,
    /// The watchdog pipe's write end, no longer owned by any Rust value. The next image must
    /// inherit it: its closing is what ends the group.
    pub(super) owner_write_fd: RawFd,
}

impl OwnedProcessGroup {
    /// Take ownership of a group a predecessor image spawned and released. `execve` kept the
    /// parent relationship and the watchdog pipe, so this image cleans up exactly as the
    /// launching one would have.
    ///
    /// # Safety
    ///
    /// `owner_write_fd` must be the released watchdog pipe descriptor and owned by nothing else.
    pub(super) unsafe fn adopt(
        server_pid: u32,
        watchdog_pid: u32,
        owner_write_fd: RawFd,
        socket_path: PathBuf,
    ) -> Self {
        // SAFETY: the caller hands over sole ownership of the inherited descriptor.
        let owner_write = unsafe { UnixStream::from_raw_fd(owner_write_fd) };
        let _ = set_close_on_exec(owner_write.as_raw_fd());
        Self {
            child: ProviderProcess::adopted(server_pid),
            watchdog: ProviderProcess::adopted(watchdog_pid),
            owner_write: Some(owner_write),
            socket_path: Some(socket_path),
            active: true,
        }
    }

    /// Leave the group running for the next driver image and give up every handle to it.
    pub(super) fn release(mut self) -> Option<ReleasedProcessGroup> {
        let owner_write = self.owner_write.take()?;
        self.active = false;
        Some(ReleasedProcessGroup {
            server_pid: self.child.id(),
            watchdog_pid: self.watchdog.id(),
            owner_write_fd: owner_write.into_raw_fd(),
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn child_mut(&mut self) -> &mut ProviderProcess {
        &mut self.child
    }

    pub(super) fn terminate(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let process_group = self.watchdog.id() as i32;
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.watchdog.kill();
        let _ = self.watchdog.wait();
        if let Some(socket_path) = self.socket_path.as_deref() {
            let _ = fs::remove_file(socket_path);
        }
        self.owner_write.take();
    }
}

impl Drop for OwnedProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn set_close_on_exec(fd: libc::c_int) -> std::io::Result<()> {
    let mut flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    flags |= libc::FD_CLOEXEC;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Spawn a provider launcher in an isolated, wrapper-owned process group.
///
/// Explicit cleanup covers normal returns and Rust errors. The in-group watchdog covers wrapper
/// crashes, SIGKILL, and supervisor teardown. The watchdog holds the group ID until cleanup, so a
/// stale PID can never identify a process group that belongs to another live owner. A crash can
/// leave one dead socket file; the next launch proves that it has no listener and removes it.
pub(super) fn spawn_process_group(
    command: &mut Command,
    socket_path: Option<&Path>,
) -> std::io::Result<OwnedProcessGroup> {
    let (watchdog_read, owner_write) = UnixStream::pair()?;
    set_close_on_exec(owner_write.as_raw_fd())?;
    let owner_write_fd = owner_write.as_raw_fd();
    let mut watchdog_command = Command::new("/bin/sh");
    watchdog_command
        .arg("-c")
        .arg("IFS= read -r ignored; kill -KILL 0")
        .arg("st2-codex-watchdog")
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(watchdog_read)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        watchdog_command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut watchdog = watchdog_command.spawn()?;
    let watchdog_process_group = watchdog.id() as i32;
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, watchdog_process_group) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(owner_write_fd);
            Ok(())
        });
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(owner_write);
            unsafe {
                libc::kill(-watchdog_process_group, libc::SIGKILL);
            }
            let _ = watchdog.kill();
            let _ = watchdog.wait();
            return Err(error);
        }
    };
    Ok(OwnedProcessGroup {
        child: ProviderProcess::Spawned(child),
        watchdog: ProviderProcess::Spawned(watchdog),
        owner_write: Some(owner_write),
        socket_path: socket_path.map(Path::to_path_buf),
        active: true,
    })
}
