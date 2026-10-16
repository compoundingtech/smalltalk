//! Required isolation-VM case. Ignored locally because it requires a real
//! delegated user manager; the VM must explicitly execute it, never credit a skip.

use super::*;
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::Child;

const READY_BUDGET: Duration = Duration::from_secs(20);
const EXIT_BUDGET: Duration = Duration::from_secs(10);

struct Fixture {
    units: Vec<String>,
    children: Vec<Child>,
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        assert_eq!(
            mode(),
            Isolation::Scope,
            "real systemd user scopes are required"
        );
        assert!(
            systemd_user_available(),
            "the user manager must be reachable"
        );
        Self {
            units: Vec::new(),
            children: Vec::new(),
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn pipe(&self, name: &str) -> (PathBuf, File) {
        let path = self.root.path().join(name);
        let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        // CLOEXEC prevents either workload retaining the reader's write end.
        // The parent holds both ends so startup has no spurious FIFO EOF.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .unwrap();
        (path, file)
    }

    fn spawn(&mut self, label: &str, script: &str, ready: &Path) -> (String, usize) {
        let nonce = self.root.path().file_name().unwrap().to_string_lossy();
        let unit = scope_unit("st3", &format!("op-retirement-{label}-{nonce}"));
        self.units.push(unit.clone());
        let arguments = [
            OsStr::new("-c"),
            OsStr::new(script),
            OsStr::new("scope-fixture"),
            ready.as_os_str(),
        ];
        let child = wrap(&unit, OsStr::new("/bin/sh"), &arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        self.children.push(child);
        (unit, self.children.len() - 1)
    }

    fn request(&self, name: &str, unit: &str) -> (PathBuf, Vec<u8>) {
        let path = self.root.path().join(name);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version":1,"request_id":name,"action":"terminate","scope_unit":unit
        }))
        .unwrap();
        let mut file = File::create(&path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        File::open(self.root.path()).unwrap().sync_all().unwrap();
        (path, bytes)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this fixture's unique scopes and unreaped direct children.
        for unit in &self.units {
            let _ = end_scope(unit, Duration::ZERO);
        }
        for child in &mut self.children {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn wait_readable(fd: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        let result = unsafe { libc::poll(&mut poll, 1, millis) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert!(result >= 0, "owned evidence descriptor is unreadable");
        if result == 0 {
            return false;
        }
        assert_eq!(poll.revents & (libc::POLLERR | libc::POLLNVAL), 0);
        return poll.revents & libc::POLLIN != 0;
    }
}

fn ready_pid(pipe: &mut File) -> u32 {
    let deadline = Instant::now() + READY_BUDGET;
    let mut bytes = Vec::new();
    loop {
        assert!(
            wait_readable(
                pipe.as_raw_fd(),
                deadline.saturating_duration_since(Instant::now())
            ),
            "workload did not publish its identity within the existing setup budget"
        );
        let mut chunk = [0; 32];
        let count = pipe.read(&mut chunk).unwrap();
        assert!(
            count > 0 && bytes.len() + count <= 64,
            "invalid workload identity"
        );
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.last() == Some(&b'\n') {
            return std::str::from_utf8(&bytes)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
        }
    }
}

fn pin(pid: u32) -> OwnedFd {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(
        fd >= 0,
        "this Linux gate requires an exact kernel process handle"
    );
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

fn assert_in_scope(pid: u32, unit: &str) {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(
        cgroup.lines().any(|line| line
            .strip_prefix("0::")
            .is_some_and(|path| path.ends_with(&format!("/{unit}")))),
        "workload must belong to the exact original scope"
    );
}

#[test]
#[ignore = "required real-manager isolation VM control; explicitly run there"]
fn a_bound_scope_retires_escaped_descendants_and_preserves_a_neighbor() {
    let mut fixture = Fixture::new();
    let (escaped_path, mut escaped_ready) = fixture.pipe("escaped-ready");
    // The inner shell creates a new session, forks sleep, reports that child,
    // then exits. The original shell exits too: the workload has escaped both
    // the original process tree and session while remaining in its cgroup.
    let script = r#"setsid /bin/sh -c 'sleep 300 </dev/null >/dev/null 2>&1 & printf "%s\n" "$!" > "$1"' scope-child "$1""#;
    let (unit, original) = fixture.spawn("escaped", script, &escaped_path);
    let escaped_pid = ready_pid(&mut escaped_ready);
    let escaped = pin(escaped_pid);
    assert_in_scope(escaped_pid, &unit);
    let original_launcher = pin(fixture.children[original].id());
    assert!(
        wait_readable(original_launcher.as_raw_fd(), EXIT_BUDGET),
        "the original launcher must exit within the existing exit budget"
    );
    assert!(fixture.children[original].wait().unwrap().success());
    assert!(
        !wait_readable(escaped.as_raw_fd(), Duration::ZERO),
        "CONTROL: top-process/session exit must leave the escaped descendant alive"
    );
    let before = observe(&unit).unwrap();
    assert!(
        before
            .cgroup
            .as_ref()
            .is_some_and(|group| is_populated(&group.events).unwrap())
    );
    assert!(
        before.validate(&unit).is_err(),
        "a live cgroup is not ending proof"
    );

    let (neighbor_path, mut neighbor_ready) = fixture.pipe("neighbor-ready");
    let (neighbor_unit, _) = fixture.spawn(
        "neighbor",
        r#"printf "%s\n" "$$" > "$1"; exec sleep 300"#,
        &neighbor_path,
    );
    let neighbor_pid = ready_pid(&mut neighbor_ready);
    let neighbor = pin(neighbor_pid);
    assert_in_scope(neighbor_pid, &neighbor_unit);
    let (request_path, original_request) = fixture.request("original-retirement", &unit);
    let guard_calls = std::cell::Cell::new(0);
    let guard = || -> Result<()> {
        anyhow::ensure!(
            std::fs::read(&request_path)? == original_request,
            "original scope request changed before effect"
        );
        guard_calls.set(guard_calls.get() + 1);
        Ok(())
    };
    // A changed original request has no signal authority. This negative
    // control must leave both exact processes alive, not just return an error.
    let mut repaired = File::create(&request_path).unwrap();
    repaired.write_all(b"changed original request").unwrap();
    repaired.sync_all().unwrap();
    assert!(end_scope_with_proof(&unit, &guard).is_err());
    assert!(!wait_readable(escaped.as_raw_fd(), Duration::ZERO));
    assert!(!wait_readable(neighbor.as_raw_fd(), Duration::ZERO));
    let mut restored = File::create(&request_path).unwrap();
    restored.write_all(&original_request).unwrap();
    restored.sync_all().unwrap();
    let ending = end_scope_with_proof(&unit, &guard)
        .unwrap()
        .expect("scope closure must be proven");
    ending.validate(&unit).unwrap();
    assert!(guard_calls.get() > 0);
    assert!(
        wait_readable(escaped.as_raw_fd(), EXIT_BUDGET),
        "exact escaped incarnation must exit"
    );
    assert!(
        !wait_readable(neighbor.as_raw_fd(), Duration::ZERO),
        "a neighboring scope must survive"
    );
    assert_in_scope(neighbor_pid, &neighbor_unit);
    assert!(
        ending.validate(&neighbor_unit).is_err(),
        "ending proof cannot change its bound scope"
    );
    let (cleanup_path, cleanup_request) = fixture.request("neighbor-cleanup", &neighbor_unit);
    end_scope_with_proof(&neighbor_unit, &|| {
        anyhow::ensure!(
            std::fs::read(&cleanup_path)? == cleanup_request,
            "cleanup request changed"
        );
        Ok(())
    })
    .unwrap()
    .expect("neighbor's separate cleanup must be proven");
    assert!(wait_readable(neighbor.as_raw_fd(), EXIT_BUDGET));
}
