use std::io::{self, Read as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use reqwest::header::HeaderValue;

const MAX_TOKEN_BYTES: usize = 4096;

pub(crate) fn discover_authorization(executable: &Path, deadline: Instant) -> Option<HeaderValue> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new(executable);
    command
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let child = command.spawn().ok()?;
    let mut child = OwnedChild::new(child).ok()?;
    let mut stdout = child.child.stdout.take()?;
    let (status, token) = collect_token(&mut child, &mut stdout, deadline)?;
    if !status.success() {
        return None;
    }
    authorization_header(token)
}

fn collect_token(
    child: &mut OwnedChild,
    stdout: &mut ChildStdout,
    deadline: Instant,
) -> Option<(ExitStatus, SecretBytes)> {
    let mut token = SecretBytes(Vec::with_capacity(128));
    let mut status = None;
    let mut stdout_closed = false;
    let mut buffer = SecretBuffer([0_u8; 1024]);

    loop {
        if Instant::now() >= deadline {
            child.terminate_and_reap();
            return None;
        }
        if status.is_none() {
            status = child.try_wait().ok()?;
            if status.is_some() {
                // The direct child is reaped by try_wait. Terminate any descendants that retained
                // stdout so the dedicated process group cannot outlive credential discovery.
                child.terminate_group();
            }
        }
        if stdout_closed {
            if let Some(status) = status {
                return Some((status, token));
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(1)),
            );
            continue;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        // SAFETY: descriptor points to one valid pollfd for the duration of this call.
        let polled = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if polled < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if polled == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return None;
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }

        let read = if token.0.len() == MAX_TOKEN_BYTES {
            let mut overflow = SecretBuffer([0_u8; 1]);
            match stdout.read(&mut overflow.0) {
                Ok(0) => {
                    stdout_closed = true;
                    continue;
                }
                Ok(_) => {
                    child.terminate_and_reap();
                    return None;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        } else {
            let available = (MAX_TOKEN_BYTES - token.0.len()).min(buffer.0.len());
            match stdout.read(&mut buffer.0[..available]) {
                Ok(0) => {
                    stdout_closed = true;
                    continue;
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        };
        token.0.extend_from_slice(&buffer.0[..read]);
    }
}

fn authorization_header(token: SecretBytes) -> Option<HeaderValue> {
    let token_text = std::str::from_utf8(&token.0).ok()?;
    let token_text = token_text.trim();
    if token_text.is_empty() {
        return None;
    }
    let mut header = SecretBytes(Vec::with_capacity("Bearer ".len() + token_text.len()));
    header.0.extend_from_slice(b"Bearer ");
    header.0.extend_from_slice(token_text.as_bytes());
    let mut value = HeaderValue::from_bytes(&header.0).ok()?;
    value.set_sensitive(true);
    Some(value)
}

struct SecretBytes(Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct SecretBuffer<const N: usize>([u8; N]);

impl<const N: usize> Drop for SecretBuffer<N> {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct OwnedChild {
    child: Child,
    process_group: i32,
    reaped: bool,
}

impl OwnedChild {
    fn new(mut child: Child) -> io::Result<Self> {
        let process_group = match i32::try_from(child.id()) {
            Ok(process_group) => process_group,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("child pid did not fit in pid_t"));
            }
        };
        Ok(Self {
            child,
            process_group,
            reaped: false,
        })
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn terminate_group(&self) {
        // SAFETY: a negative pid addresses the dedicated process group created by process_group.
        let _ = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
    }

    fn terminate_and_reap(&mut self) {
        self.terminate_group();
        while !self.reaped {
            match self.child.wait() {
                Ok(_) => self.reaped = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn discovers_a_sensitive_authorization_header() {
        let (_temporary, executable) = executable_fixture("printf '%s\\n' 'fixture-token'");
        let authorization =
            discover_authorization(&executable, Instant::now() + Duration::from_secs(15)).unwrap();

        assert!(authorization.is_sensitive());
        assert_eq!(authorization.as_bytes(), b"Bearer fixture-token");
    }

    #[test]
    fn rejects_oversized_and_failed_credentials() {
        let oversized = format!("printf '{}'", "x".repeat(MAX_TOKEN_BYTES + 1));
        let (_temporary, executable) = executable_fixture(&oversized);
        assert!(
            discover_authorization(&executable, Instant::now() + Duration::from_secs(5)).is_none()
        );

        let (_temporary, executable) = executable_fixture("printf '%s\\n' 'ignored-token'; exit 7");
        assert!(
            discover_authorization(&executable, Instant::now() + Duration::from_secs(5)).is_none()
        );
    }

    // The descendant liveness assertion uses Linux /proc; without it this test
    // cannot verify process-group cleanup. Other platforms still test discovery.
    #[cfg(target_os = "linux")]
    #[test]
    fn deadline_terminates_the_owned_process_group() {
        let (_temporary, executable) = executable_fixture(
            r#"
(
    trap '' TERM
    while :; do sleep 10; done
) &
descendant=$!
printf '%s %s\n' "$$" "$descendant" > "$0.pids"
wait "$descendant"
"#,
        );
        let started = Instant::now();
        assert!(
            discover_authorization(&executable, started + Duration::from_millis(200),).is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        let pids = fs::read_to_string(format!("{}.pids", executable.display())).unwrap();
        let pids = pids
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        while pids.iter().copied().any(process_is_running) && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!pids.iter().copied().any(process_is_running));
    }

    fn executable_fixture(body: &str) -> (TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("gh");
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        (temporary, executable)
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: i32) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            .is_some_and(|state| !matches!(state, 'Z' | 'X'))
    }
}
