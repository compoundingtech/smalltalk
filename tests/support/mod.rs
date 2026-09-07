// Shared by several test binaries; each one uses only the parts it needs.
#![allow(dead_code)]

use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);

/// A retired agent's declared `resource` bindings, shared by the desired-state and doctor fixtures.
pub const RETIRED_RESOURCES: &str = concat!(
    "  resource \"work\" uri=\"work://h/current-task\" reason=\"Current implementation task.\"\n",
    "  resource \"issue\" uri=\"github-issue://example/project/41\" reason=\"Tracking issue.\"\n",
);

/// A subprocess group whose lifetime is bounded by the test process that spawned it.
///
/// A watchdog pins the process-group identity and observes a close-on-exec socket owned only by the
/// test process. `Drop` handles normal returns and unwinding; socket EOF makes the watchdog kill the
/// same group if the test process dies before Rust cleanup can run.
pub struct OwnedChildGroup {
    child: Option<Child>,
    watchdog: Option<Child>,
    owner_write: Option<UnixStream>,
    process_group: libc::pid_t,
}

impl OwnedChildGroup {
    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("owned child was reaped")
    }

    pub fn wait_with_output(mut self) -> io::Result<Output> {
        let output = self
            .child
            .take()
            .expect("owned child was reaped")
            .wait_with_output();
        self.cleanup();
        output
    }

    pub fn terminate(&mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if self.child.is_none() && self.watchdog.is_none() {
            self.owner_write.take();
            return;
        }

        // The watchdog remains a member until this signal, pinning the group ID against PID reuse.
        unsafe {
            libc::kill(-self.process_group, libc::SIGKILL);
        }
        self.owner_write.take();

        let deadline = Instant::now() + CLEANUP_DEADLINE;
        while (self.child.is_some() || self.watchdog.is_some()) && Instant::now() < deadline {
            reap_if_finished(&mut self.child);
            reap_if_finished(&mut self.watchdog);
            if self.child.is_some() || self.watchdog.is_some() {
                thread::sleep(Duration::from_millis(2));
            }
        }

        if self.child.is_none() && self.watchdog.is_none() {
            return;
        }

        let remaining = Arc::new(Mutex::new((self.child.take(), self.watchdog.take())));
        let reaper_remaining = Arc::clone(&remaining);
        if thread::Builder::new()
            .name("st2-test-child-reaper".into())
            .spawn(move || wait_for_remaining(&reaper_remaining))
            .is_err()
        {
            // The killed children remain owned by `remaining`; abort is the only bounded,
            // non-leaking outcome when the process cannot create their reaper.
            std::process::abort();
        }
    }
}

impl Drop for OwnedChildGroup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Deref for OwnedChildGroup {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        self.child.as_ref().expect("owned child was reaped")
    }
}

impl DerefMut for OwnedChildGroup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child_mut()
    }
}

pub trait CommandOwnedChildGroupExt {
    fn spawn_owned(&mut self) -> io::Result<OwnedChildGroup>;
}

impl CommandOwnedChildGroupExt for Command {
    fn spawn_owned(&mut self) -> io::Result<OwnedChildGroup> {
        let (watchdog_read, owner_write) = UnixStream::pair()?;
        set_close_on_exec(owner_write.as_raw_fd())?;
        let owner_write_fd = owner_write.as_raw_fd();

        let mut watchdog_command = Command::new("/bin/sh");
        watchdog_command
            .arg("-c")
            .arg("IFS= read -r ignored; kill -KILL 0")
            .arg("st2-test-child-watchdog")
            .stdin(Stdio::from(OwnedFd::from(watchdog_read)))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            watchdog_command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut watchdog = watchdog_command.spawn()?;
        let process_group = watchdog.id() as libc::pid_t;

        unsafe {
            self.pre_exec(move || {
                if libc::setpgid(0, process_group) == -1 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(owner_write_fd);
                Ok(())
            });
        }

        let child = match self.spawn() {
            Ok(child) => child,
            Err(error) => {
                unsafe {
                    libc::kill(-process_group, libc::SIGKILL);
                }
                drop(owner_write);
                let _ = watchdog.wait();
                return Err(error);
            }
        };

        Ok(OwnedChildGroup {
            child: Some(child),
            watchdog: Some(watchdog),
            owner_write: Some(owner_write),
            process_group,
        })
    }
}

fn set_close_on_exec(fd: libc::c_int) -> io::Result<()> {
    let mut flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    flags |= libc::FD_CLOEXEC;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn reap_if_finished(child: &mut Option<Child>) {
    let finished = child
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))));
    if finished {
        child.take();
    }
}

fn wait_for_remaining(remaining: &Mutex<(Option<Child>, Option<Child>)>) {
    let (child, watchdog) = {
        let mut remaining = remaining.lock().unwrap_or_else(|error| error.into_inner());
        (remaining.0.take(), remaining.1.take())
    };
    wait_if_present(child);
    wait_if_present(watchdog);
}

fn wait_if_present(child: Option<Child>) {
    if let Some(mut child) = child {
        let _ = child.wait();
    }
}
