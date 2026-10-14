//! Dedicated real-systemd VM proofs. The VM must explicitly run these ignored tests.
//! Missing scopes fail; a normal workspace run does not claim this coverage.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use st_runtime::{ExecObservation, ExecRuntime, Isolation, Launch, PtyRuntime};

const MEMBER: &str = "scope-proof";
const ROOT_ENV: &str = "SCOPE_PROOF_ROOT";
const LITERAL_CHILD: &str = "SCOPE_PROOF_LITERAL_CHILD";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn environment(root: &Path) -> BTreeMap<String, String> {
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut result: BTreeMap<_, _> = ["PATH", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
    result.insert("HOME".into(), home.to_str().unwrap().into());
    result.insert("ST_AGENT".into(), "agent/fixture/scope-proof".into());
    result.insert(
        "ST3_ENDPOINT".into(),
        root.join("unused.sock").to_str().unwrap().into(),
    );
    result
}

fn require_scopes(root: &Path) -> BTreeMap<String, String> {
    let environment = environment(root);
    assert_eq!(
        st_runtime::initialize_isolation(&environment),
        Isolation::Scope,
        "this proof requires a real systemd user manager, not a detached substitute"
    );
    environment
}

fn exec_runtime(root: &Path) -> ExecRuntime {
    ExecRuntime::new(root.join("exec"), root.join("logs"))
}

fn pty_runtime(root: &Path) -> PtyRuntime {
    PtyRuntime::new(root.join("pty")).with_environment(environment(root))
}

fn signal_ready(root: &Path) {
    UnixDatagram::unbound()
        .unwrap()
        .send_to(b"ready", root.join("ready.sock"))
        .unwrap();
}

fn remain_running() -> ! {
    loop {
        std::thread::park();
    }
}

fn pidfd(pid: u32) -> OwnedFd {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    assert!(
        fd >= 0,
        "cannot watch process {pid}: {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

fn wait_for_exit(fd: &OwnedFd) {
    let mut watched = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut watched, 1, 15_000) };
    assert_eq!(ready, 1, "the exact process did not exit after its stop");
    assert_ne!(watched.revents & libc::POLLIN, 0);
}

fn cgroup(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    ready: UnixDatagram,
    unit: String,
    parent_running: bool,
    member_removed: bool,
    pty: bool,
}

impl Fixture {
    fn new(pty: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        require_scopes(root.path());
        let ready = UnixDatagram::bind(root.path().join("ready.sock")).unwrap();
        ready
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        Self {
            root,
            ready,
            unit: format!(
                "scope-proof-{}-{}.service",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            parent_running: false,
            member_removed: false,
            pty,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn wait_ready(&self) {
        let mut message = [0; 32];
        let count = self.ready.recv(&mut message).unwrap();
        assert_eq!(&message[..count], b"ready");
    }

    fn start_parent(&mut self, test: &str) -> OwnedFd {
        let mut command = Command::new("systemd-run");
        command.args([
            "--user",
            "--quiet",
            "--collect",
            "--property=Type=exec",
            "--property=KillMode=control-group",
        ]);
        command.arg(format!("--unit={}", self.unit));
        for (key, value) in environment(self.path()) {
            command.arg(format!("--setenv={key}={value}"));
        }
        command
            .arg(format!("--setenv={ROOT_ENV}={}", self.path().display()))
            .arg(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                test,
                "--nocapture",
                "--test-threads=1",
            ]);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "parent service did not start: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.parent_running = true;
        self.wait_ready();
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=MainPID",
                "--value",
                &self.unit,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let pid: u32 = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid > 0);
        assert!(cgroup(pid).contains(&self.unit));
        let mode = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=KillMode",
                "--value",
                &self.unit,
            ])
            .output()
            .unwrap();
        assert!(mode.status.success());
        assert_eq!(
            String::from_utf8(mode.stdout).unwrap().trim(),
            "control-group"
        );
        pidfd(pid)
    }

    fn kill_parent(&mut self, parent: &OwnedFd) {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "kill",
                "--signal=SIGKILL",
                "--kill-whom=all",
                &self.unit,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        wait_for_exit(parent);
        self.parent_running = false;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.parent_running {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &self.unit])
                .output();
        }
        if self.member_removed {
            return;
        }
        let result = if self.pty {
            pty_runtime(self.path()).kill(MEMBER)
        } else {
            exec_runtime(self.path()).kill(MEMBER)
        };
        if let Err(error) = result {
            eprintln!("fixture cleanup could not end its member: {error:#}");
        }
    }
}

fn running_exec(root: &Path) -> st_runtime::ExecGeneration {
    match exec_runtime(root).observe(MEMBER).unwrap() {
        Some(ExecObservation::Running(generation)) => generation,
        other => panic!("the member is not running: {other:?}"),
    }
}

fn running_pty(root: &Path) -> st_runtime::PtyObservation {
    let observation = pty_runtime(root)
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|observation| observation.name == MEMBER)
        .expect("the PTY must be published");
    assert_eq!(observation.status, "running");
    assert!(observation.pid.is_some());
    assert!(observation.created_at.is_some());
    observation
}

// The registry identifies the PTY server. Stats fence that server before naming its harness.
fn pty_process(root: &Path, server_pid: u32) -> u32 {
    let status = pty_core::registry::with_root(&root.join("pty"), || {
        pty_client::query_status_json(MEMBER, pty_client::STATS_TIMEOUT)
    })
    .unwrap();
    let stats: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(stats["name"], MEMBER);
    assert_eq!(stats["daemon"]["pid"].as_u64(), Some(u64::from(server_pid)));
    assert_eq!(stats["process"]["alive"], true);
    u32::try_from(stats["process"]["pid"].as_u64().unwrap()).unwrap()
}

#[test]
#[ignore = "requires the dedicated real-systemd isolation VM"]
fn exec_survives_parent_service_sigkill_with_the_same_incarnation_and_stops() {
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        let root = PathBuf::from(root);
        let environment = require_scopes(&root);
        exec_runtime(&root)
            .spawn(
                MEMBER,
                &Launch::Argv(vec!["sleep".into(), "300".into()]),
                &root,
                &environment,
            )
            .unwrap();
        signal_ready(&root);
        remain_running();
    }
    let mut fixture = Fixture::new(false);
    let parent = fixture
        .start_parent("exec_survives_parent_service_sigkill_with_the_same_incarnation_and_stops");
    let before = running_exec(fixture.path());
    assert_eq!(before.isolation_mode, "scope");
    assert!(before.scope_unit.as_ref().unwrap().ends_with(".scope"));
    let group = cgroup(before.pid);
    assert!(!group.contains(&fixture.unit));
    assert!(group.contains(before.scope_unit.as_ref().unwrap()));
    let member = pidfd(before.pid);

    fixture.kill_parent(&parent);
    assert_eq!(running_exec(fixture.path()), before);
    assert_eq!(cgroup(before.pid), group);

    let runtime = exec_runtime(fixture.path());
    runtime
        .stop_if(MEMBER, Some(&before.generation_id))
        .unwrap();
    wait_for_exit(&member);
    assert!(matches!(
        runtime.observe(MEMBER).unwrap(),
        Some(ExecObservation::Exited(_))
    ));
    runtime.end_leftovers(MEMBER).unwrap();
    runtime.remove(MEMBER).unwrap();
    fixture.member_removed = true;
}

#[test]
#[ignore = "requires the dedicated real-systemd isolation VM"]
fn pty_survives_parent_service_sigkill_with_the_same_incarnation_and_stops() {
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        let root = PathBuf::from(root);
        let environment = require_scopes(&root);
        pty_runtime(&root)
            .spawn(
                MEMBER,
                &Launch::Argv(vec!["sleep".into(), "300".into()]),
                &root,
                &environment,
                None,
                &BTreeMap::new(),
                None,
            )
            .unwrap();
        signal_ready(&root);
        remain_running();
    }
    let mut fixture = Fixture::new(true);
    let parent = fixture
        .start_parent("pty_survives_parent_service_sigkill_with_the_same_incarnation_and_stops");
    let before = running_pty(fixture.path());
    let server_pid = before.pid.unwrap();
    let harness_pid = pty_process(fixture.path(), server_pid);
    assert_ne!(server_pid, harness_pid);
    let scope = before.tags.get("st3.scope-unit").unwrap();
    assert!(scope.ends_with(".scope"));
    let group = cgroup(harness_pid);
    assert!(
        group.contains(scope),
        "PTY harness {harness_pid} is outside its work scope {scope:?}: {group:?}"
    );
    assert!(!group.contains(&fixture.unit));
    assert!(!cgroup(server_pid).contains(&fixture.unit));
    let server = pidfd(server_pid);
    let member = pidfd(harness_pid);

    fixture.kill_parent(&parent);
    assert_eq!(running_pty(fixture.path()), before);
    assert_eq!(pty_process(fixture.path(), server_pid), harness_pid);
    assert_eq!(cgroup(harness_pid), group);
    assert!(!cgroup(server_pid).contains(&fixture.unit));

    let runtime = pty_runtime(fixture.path());
    runtime
        .stop_if(
            MEMBER,
            Some(&format!("{server_pid}:{}", before.created_at.unwrap())),
        )
        .unwrap();
    wait_for_exit(&member);
    wait_for_exit(&server);
    assert!(
        runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .all(|observation| observation.name != MEMBER || observation.status != "running")
    );
    runtime.remove(MEMBER).unwrap();
    fixture.member_removed = true;
}

#[test]
#[ignore = "requires the dedicated real-systemd isolation VM"]
fn literal_environment_is_preserved_across_a_work_scope() {
    if std::env::var_os(LITERAL_CHILD).is_some() {
        let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
        fs::write(root.join("literal.txt"), std::env::var("LITERAL").unwrap()).unwrap();
        signal_ready(&root);
        remain_running();
    }
    let mut fixture = Fixture::new(false);
    let mut environment = require_scopes(fixture.path());
    let literal = "literal $HOME %n %i 'quoted'\nsecond line";
    environment.insert("LITERAL".into(), literal.into());
    environment.insert(LITERAL_CHILD.into(), "1".into());
    environment.insert(ROOT_ENV.into(), fixture.path().to_str().unwrap().into());
    let runtime = exec_runtime(fixture.path());
    let generation = runtime
        .spawn(
            MEMBER,
            &Launch::Argv(vec![
                std::env::current_exe().unwrap().to_str().unwrap().into(),
                "--ignored".into(),
                "--exact".into(),
                "literal_environment_is_preserved_across_a_work_scope".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ]),
            fixture.path(),
            &environment,
        )
        .unwrap();
    fixture.wait_ready();
    assert_eq!(generation.isolation_mode, "scope");
    assert_eq!(
        fs::read_to_string(fixture.path().join("literal.txt")).unwrap(),
        literal
    );
    let member = pidfd(generation.pid);
    runtime
        .stop_if(MEMBER, Some(&generation.generation_id))
        .unwrap();
    wait_for_exit(&member);
    assert!(matches!(
        runtime.observe(MEMBER).unwrap(),
        Some(ExecObservation::Exited(_))
    ));
    runtime.end_leftovers(MEMBER).unwrap();
    runtime.remove(MEMBER).unwrap();
    fixture.member_removed = true;
}
