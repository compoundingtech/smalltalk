//! Native stop ordering through st3-fixture's real driver entrypoints.
//!
//! The daemon is an already-admitted protocol fixture with an invented incarnation. It does
//! not prove physical seat admission, scope ownership or recovery. The wrapper, provider,
//! process group, readiness socket and durable observed-state snapshot are real and test-owned.
//! Linux uses pidfds and macOS uses kqueue NOTE_EXIT; both watch exact owned processes.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::{Daemon, TestSeat, seat_command};
use sha2::{Digest as _, Sha256};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn driver_stderr(root: &Path) -> String {
    std::fs::read_to_string(root.join("stderr/driver.log"))
        .unwrap_or_else(|error| format!("cannot read driver stderr evidence: {error}"))
}

const SUBJECT: &str = "agent/eval.stop-orchid";
const READY_BUDGET: Duration = Duration::from_secs(10);
const EXIT_BUDGET: Duration = Duration::from_secs(15);

#[cfg(target_os = "linux")]
fn watch_process_exit(pid: u32) -> OwnedFd {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    assert!(
        fd >= 0,
        "pidfd_open({pid}): {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

// Register both processes before signaling the wrapper. NOTE_EXIT observes an exit even
// when the provider is reparented or reaped by the wrapper; no kill(pid, 0) polling is used.
#[cfg(target_os = "macos")]
fn watch_process_exit(pid: u32) -> OwnedFd {
    let fd = unsafe { libc::kqueue() };
    assert!(fd >= 0, "kqueue: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    event.ident = pid as _;
    event.filter = libc::EVFILT_PROC;
    event.flags = libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT;
    event.fflags = libc::NOTE_EXIT;
    assert_eq!(
        unsafe {
            libc::kevent(
                fd.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        },
        0,
        "cannot watch owned process {pid}: {}",
        std::io::Error::last_os_error()
    );
    fd
}

#[cfg(target_os = "macos")]
fn wait_for_exit(root: &Path, fd: &OwnedFd, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = libc::timespec {
        tv_sec: remaining.as_secs() as _,
        tv_nsec: remaining.subsec_nanos() as _,
    };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::kevent(fd.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &timeout) },
        1,
        "the owned process did not exit within the original 15 s stop-proof budget; stderr: {}",
        driver_stderr(root)
    );
    assert_eq!(
        event.filter,
        libc::EVFILT_PROC,
        "stderr: {}",
        driver_stderr(root)
    );
    assert_ne!(
        event.fflags & libc::NOTE_EXIT,
        0,
        "stderr: {}",
        driver_stderr(root)
    );
    assert_eq!(
        event.flags & libc::EV_ERROR,
        0,
        "stderr: {}",
        driver_stderr(root)
    );
}

#[cfg(target_os = "linux")]
fn wait_for_exit(root: &Path, fd: &OwnedFd, deadline: Instant) {
    let mut watched = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe {
            libc::poll(
                &mut watched,
                1,
                i32::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap(),
            )
        },
        1,
        "the owned process did not exit within the original 15 s stop-proof budget; stderr: {}",
        driver_stderr(root)
    );
    assert_ne!(
        watched.revents & libc::POLLIN,
        0,
        "stderr: {}",
        driver_stderr(root)
    );
}

fn declare(daemon: &Daemon, driver: &str) {
    let source = format!(
        "version 2\nagent {:?} {{ workspace {:?}; harness {:?} {{ model \"example-model\"; }} }}\n",
        SUBJECT.trim_start_matches("agent/"),
        daemon.root.join("workspace"),
        driver,
    );
    let intent = st3::parse_intent(&source, "restart-node").unwrap();
    let preview = daemon
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    daemon
        .store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "declare",
            Some("person/operator"),
        )
        .unwrap();
}

async fn spawn_driver(
    root: &Path,
    daemon: &mut Daemon,
    driver: &str,
    ignore_term: bool,
) -> (TestSeat, OwnedFd, OwnedFd, PathBuf) {
    let ready_path = root.join("ready.sock");
    let ready = tokio::net::UnixDatagram::bind(&ready_path).unwrap();
    let mut command = seat_command(root, &daemon.socket);
    declare(daemon, driver);
    daemon.observe_running(SUBJECT, "stop-orchid:one");
    daemon.start_admitted_protocol().await;
    // Readiness is sent only after the actual provider has installed its TERM disposition.
    // There is no file polling and no timer keeping this provider alive.
    let provider = r#"
import os, signal, socket
signal.signal(signal.SIGTERM, signal.SIG_IGN if os.environ['IGNORE_TERM'] == '1' else signal.SIG_DFL)
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.sendto(str(os.getpid()).encode(), os.environ['READY_SOCKET'])
s.close()
while True:
    signal.pause()
"#;
    std::fs::create_dir_all(root.join("stderr")).unwrap();
    let mut child = TestSeat(Some(
        command
            .env("ST_AGENT", SUBJECT)
            .env("ST3_MAILBOX_TRANSPORT", "push")
            .env("READY_SOCKET", &ready_path)
            .env("IGNORE_TERM", if ignore_term { "1" } else { "0" })
            .args([
                "driver",
                driver,
                "--subject",
                SUBJECT,
                "--",
                "python3",
                "-c",
                provider,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(root.join("stderr/driver.log")).unwrap(),
            ))
            .spawn()
            .unwrap(),
    ));
    let wrapper = watch_process_exit(child.id());
    let mut message = [0; 32];
    let bytes = tokio::time::timeout(READY_BUDGET, ready.recv(&mut message))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "provider never installed its TERM disposition: {error}; stderr: {}",
                driver_stderr(root)
            )
        })
        .unwrap();
    let provider_pid: u32 = std::str::from_utf8(&message[..bytes])
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(
        provider_pid,
        child.id(),
        "the witness must be the provider, not its wrapper"
    );
    assert_eq!(
        unsafe { libc::getpgid(provider_pid as i32) },
        child.id() as i32,
        "provider must belong to this test's wrapper process group"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "wrapper ended before the stop request"
    );
    let provider = watch_process_exit(provider_pid);
    let driver_root = root
        .join("drivers")
        .join(&hex::encode(Sha256::digest(SUBJECT.as_bytes()))[..24]);
    assert!(
        !driver_root.join("catalog").exists(),
        "the native path must not construct a catalog"
    );
    let record = driver_root.join("observations/harness-state");
    (child, wrapper, provider, record)
}

async fn stop_and_reap(
    root: &Path,
    child: &mut TestSeat,
    wrapper: OwnedFd,
    provider: OwnedFd,
) -> ExitStatus {
    // Signal only the wrapper; the real driver must stop its provider itself.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + EXIT_BUDGET;
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        wait_for_exit(&root, &wrapper, deadline);
        wait_for_exit(&root, &provider, deadline);
    })
    .await
    .unwrap();
    child.wait().unwrap()
}

fn assert_record(root: &Path, record: &Path, harness: &str, expected_exit: &str) {
    let agent_dir = record.parent().unwrap();
    assert!(
        st_drivers::harness_events::enabled(agent_dir),
        "native observation outbox must be enabled; stderr: {}",
        driver_stderr(root)
    );
    assert!(
        !record.exists(),
        "native teardown must not create a legacy record file; stderr: {}",
        driver_stderr(root)
    );
    let raw = st_drivers::harness_events::read_snapshot(agent_dir, "harness-state")
        .unwrap_or_else(|error| {
            panic!(
                "cannot read durable terminal snapshot: {error}; stderr: {}",
                driver_stderr(root)
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "durable terminal snapshot must outlive the wrapper; stderr: {}",
                driver_stderr(root)
            )
        });
    let raw = std::str::from_utf8(&raw).unwrap_or_else(|error| {
        panic!(
            "terminal snapshot must be UTF-8 JSON: {error}; stderr: {}",
            driver_stderr(root)
        )
    });
    let value: serde_json::Value = serde_json::from_str(raw).unwrap_or_else(|error| {
        panic!(
            "invalid terminal record: {error}; stderr: {}",
            driver_stderr(root)
        )
    });
    assert_eq!(
        value["agent"],
        SUBJECT.trim_start_matches("agent/"),
        "record: {raw}; stderr: {}",
        driver_stderr(root)
    );
    assert_eq!(
        value["harness"],
        harness,
        "record: {raw}; stderr: {}",
        driver_stderr(root)
    );
    assert!(
        value["incarnation"]
            .as_str()
            .is_some_and(|token| !token.is_empty()),
        "record: {raw}; stderr: {}",
        driver_stderr(root)
    );
    assert_eq!(
        value["state"],
        "ended",
        "record: {raw}; stderr: {}",
        driver_stderr(root)
    );
    assert_eq!(
        value["exit"],
        expected_exit,
        "record: {raw}; stderr: {}",
        driver_stderr(root)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_stop_escalation_records_ended_before_its_group_sigkill() {
    if st3::test_support::supervise_test() {
        return;
    }
    let tmp = st3::test_support::evidence_tempdir();
    let mut daemon = Daemon::new(tmp.path());
    let (mut child, wrapper, provider, record) =
        spawn_driver(tmp.path(), &mut daemon, "claude", true).await;
    let exit = stop_and_reap(tmp.path(), &mut child, wrapper, provider).await;
    assert_eq!(
        exit.signal(),
        Some(libc::SIGKILL),
        "escalation must kill the wrapper: {exit}; stderr: {}",
        driver_stderr(tmp.path())
    );
    assert_record(tmp.path(), &record, "claude", "signal 9");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_stop_escalation_records_cover_before_its_group_sigkill() {
    if st3::test_support::supervise_test() {
        return;
    }
    let tmp = st3::test_support::evidence_tempdir();
    let mut daemon = Daemon::new(tmp.path());
    let (mut child, wrapper, provider, record) =
        spawn_driver(tmp.path(), &mut daemon, "opencode", true).await;
    let exit = stop_and_reap(tmp.path(), &mut child, wrapper, provider).await;
    assert_eq!(
        exit.signal(),
        Some(libc::SIGKILL),
        "escalation must kill the wrapper: {exit}; stderr: {}",
        driver_stderr(tmp.path())
    );
    assert_record(tmp.path(), &record, "opencode", "stopped");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_graceful_stop_rewrites_cover_with_the_reaped_signal() {
    if st3::test_support::supervise_test() {
        return;
    }
    let tmp = st3::test_support::evidence_tempdir();
    let mut daemon = Daemon::new(tmp.path());
    let (mut child, wrapper, provider, record) =
        spawn_driver(tmp.path(), &mut daemon, "opencode", false).await;
    let exit = stop_and_reap(tmp.path(), &mut child, wrapper, provider).await;
    assert!(
        exit.success(),
        "wrapper must survive the provider's graceful stop: {exit}; stderr: {}",
        driver_stderr(tmp.path())
    );
    assert_record(tmp.path(), &record, "opencode", "signal 15");
    daemon.stop().await;
}
