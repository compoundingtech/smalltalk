//! Child-process execution with bounded output capture.
//!
//! Moved verbatim out of the parent module: the capture cap, the bounded reader, and every
//! deadline-bounded `Command` runner the supervisor uses.

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::*;

/// Per-stream cap for captured child diagnostics. Tail-preserving: when output exceeds the cap,
/// the LAST [`CAPTURE_CAP_BYTES`] bytes are kept — recent output is what a failure message needs,
/// and an uncapped capture lets one chatty child balloon sidecar memory without bound.
pub(crate) const CAPTURE_CAP_BYTES: usize = 256 * 1024;

/// One captured child stream capped to [`CAPTURE_CAP_BYTES`], keeping the tail.
pub(crate) struct BoundedStream {
    pub bytes: Vec<u8>, // last <= cap bytes
    pub total: u64,     // complete stream size before capping
}

impl BoundedStream {
    pub fn truncated(&self) -> bool {
        self.total as usize > self.bytes.len()
    }
}

/// Read back at most `cap` bytes of a temp-file capture, preserving the tail. The file is stat'ed
/// and seek'ed straight to `len - cap`, so the cost is O(cap) no matter how much the child wrote.
pub(crate) fn read_bounded_tail(
    file: &mut std::fs::File,
    cap: usize,
) -> std::io::Result<BoundedStream> {
    let total = file.metadata()?.len();
    let skip = total.saturating_sub(cap as u64);
    file.seek(std::io::SeekFrom::Start(skip))?;
    let mut bytes = Vec::with_capacity((total - skip) as usize);
    file.take(cap as u64).read_to_end(&mut bytes)?;
    Ok(BoundedStream { bytes, total })
}

/// Send an already-killed child to ONE shared reaper thread instead of spawning a detached thread
/// per timed-out child: under a timeout storm one-thread-per-child accumulates without bound.
/// The thread starts lazily on first use.
pub(crate) fn reap_detached(child: std::process::Child) {
    static REAPER: std::sync::LazyLock<std::sync::mpsc::Sender<Child>> =
        std::sync::LazyLock::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<Child>();
            // Thread-spawn exhaustion is the only failure mode; panicking here surfaces it at the
            // call site instead of silently leaking unreaped children.
            std::thread::Builder::new()
                .name("st2-child-reaper".to_string())
                .spawn(move || {
                    for mut child in receiver {
                        let _ = child.wait();
                    }
                })
                .expect("spawn shared child reaper thread");
            sender
        });
    let _ = REAPER.send(child);
}

/// Run a non-interactive child with bounded output capture: each stream keeps at most its last
/// [`CAPTURE_CAP_BYTES`] bytes (tail-preserving, with a diagnostic line on truncation). Regular
/// temporary files keep an escaped descendant that inherited stdout/stderr from blocking cleanup
/// after the direct child times out.
/// The child still gets a fresh process group so the common wrapper-and-descendants case is reaped.
#[cfg(test)]
pub(super) fn output_with_timeout(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    output_with_input_timeout(command, timeout, None)
}

pub(super) fn terminate_and_reap_before(mut child: Child, pid: i32, deadline: Instant) {
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            Ok(None) | Err(_) => {
                reap_detached(child);
                return;
            }
        }
    }
}

pub(super) fn write_all_before(
    mut stdin: ChildStdin,
    mut input: &[u8],
    deadline: Instant,
) -> anyhow::Result<bool> {
    let fd = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read metadata stdin flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("make metadata stdin nonblocking");
    }
    while !input.is_empty() {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match stdin.write(input) {
            Ok(0) => {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
                    .context("write metadata patch payload");
            }
            Ok(written) => input = &input[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            Err(error) => return Err(error).context("write metadata patch payload"),
        }
    }
    Ok(true)
}

pub(super) fn output_with_input_timeout(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
) -> anyhow::Result<Output> {
    output_with_input_timeout_observed(command, timeout, input, |_| {})
}

/// `on_spawn` observes the direct child's pid at the moment it exists. The child is `setsid`, so
/// that pid is also its process group id — the group this function signals on every failure path.
/// Tests need it to assert the child was reaped, and the child cannot supply it: a test whose
/// deadline expires before the child is first scheduled would never see anything the child wrote.
///
/// It runs BEFORE the child deadline starts, and tests rely on that: a lifecycle test blocks in
/// `on_spawn` until the fixture reached the state it wants to measure, so fork+exec scheduling is
/// paid outside the deadline instead of out of it. See
/// `tests::the_spawn_observer_runs_before_the_child_deadline_starts`.
pub(super) fn output_with_input_timeout_observed(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
    on_spawn: impl FnOnce(i32),
) -> anyhow::Result<Output> {
    run_captured(command, timeout, input, on_spawn, false)
}

/// Like [`output_with_timeout`], but returns the COMPLETE stdout: callers parse structured data
/// (e.g. `pty list --json`) that must be whole, and capping it would corrupt the parse for large
/// fleets. Stdout is therefore intentionally uncapped — one chatty child can balloon this buffer.
/// Stderr stays tail-capped at [`CAPTURE_CAP_BYTES`] with a diagnostic line on truncation,
/// because stderr is only surfaced inside error messages.
pub(crate) fn output_full_stdout_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> anyhow::Result<Output> {
    run_captured(command, timeout, None, |_| {}, true)
}

#[cfg(test)]
pub(super) fn output_full_stdout_with_timeout_observed(
    command: &mut Command,
    timeout: Duration,
    on_spawn: impl FnOnce(i32),
) -> anyhow::Result<Output> {
    run_captured(command, timeout, None, on_spawn, true)
}

/// Shared spawn/wait/read-back core. The child is `setsid`, so its pid is also its process group
/// id — the group this function signals on every failure path.
pub(super) fn run_captured(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
    on_spawn: impl FnOnce(i32),
    full_stdout: bool,
) -> anyhow::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    on_spawn(pid);
    // Load-bearing order: the deadline starts after `on_spawn` returns, so a test that blocks there
    // as a readiness barrier spends none of `timeout` on fork+exec. Moving this line above
    // `on_spawn` is silent in production and makes every barrier test load-sensitive again.
    let deadline = Instant::now() + timeout;
    if let Some(input) = input {
        let Some(stdin) = child.stdin.take() else {
            terminate_and_reap_before(child, pid, deadline);
            anyhow::bail!("metadata patch child has no piped stdin");
        };
        match write_all_before(stdin, &input, deadline) {
            Ok(true) => {}
            Ok(false) => {
                terminate_and_reap_before(child, pid, deadline);
                anyhow::bail!("timed out after {:.1}s", timeout.as_secs_f64());
            }
            Err(error) => {
                terminate_and_reap_before(child, pid, deadline);
                return Err(error);
            }
        }
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_and_reap_before(child, pid, deadline);
            anyhow::bail!("timed out after {:.1}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout_stream = if full_stdout {
        // Intentionally uncapped: callers parse structured data that must be whole.
        stdout.rewind()?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes)?;
        BoundedStream {
            total: bytes.len() as u64,
            bytes,
        }
    } else {
        read_bounded_tail(&mut stdout, CAPTURE_CAP_BYTES)?
    };
    let stderr_stream = read_bounded_tail(&mut stderr, CAPTURE_CAP_BYTES)?;
    let program = command.get_program().to_string_lossy();
    for (stream, name) in [(&stdout_stream, "stdout"), (&stderr_stream, "stderr")] {
        if stream.truncated() {
            eprintln!(
                "st2: truncated {name} capture of `{program}`: keeping last {} of {} bytes (cap {CAPTURE_CAP_BYTES})",
                stream.bytes.len(),
                stream.total,
            );
        }
    }
    Ok(Output {
        status,
        stdout: stdout_stream.bytes,
        stderr: stderr_stream.bytes,
    })
}
