#![cfg(target_os = "linux")]

//! Proves a tile's boundary on a real Linux host, with synthetic accounts.
//!
//! ```sh
//! ST_TILES_ROOT_TEST=1 cargo test -p st3 --test integration tiles_root:: -- --nocapture
//! ```
//!
//! It needs passwordless `sudo -n`, systemd as PID 1, cgroups v2 and `python3` on a Nix host,
//! whose closure it binds into each tile for the probes. Without `ST_TILES_ROOT_TEST=1` every
//! test returns at once. Each test creates its own registry and state directory under
//! `/var/lib/st-tiles-test-*`, whose tiles the launcher gives user IDs from a test range that no
//! account holds, and removes them all with `st tiles remove`, and any unit left running, when it
//! ends. It never reads or changes a real login, key or running seat.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use st3::tiles::registry::{self, Registry};

fn enabled() -> bool {
    if std::env::var_os("ST_TILES_ROOT_TEST").as_deref() != Some("1".as_ref()) {
        eprintln!("skipped: set ST_TILES_ROOT_TEST=1 on a Linux host with sudo -n");
        return false;
    }
    true
}

fn sudo<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .expect("run sudo")
}

fn ok(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The prefix of the account a test makes, so a later run can remove what a killed run left. A
/// host may declare its own fixture users (hetz2 has `sttfx1`..`sttfx32`), so it is not theirs.
const PREFIX: &str = "sttiletest";

/// Whether `user` is an account these tests made: the prefix, then a user ID in the test range.
fn ours(user: &str) -> bool {
    user.strip_prefix(PREFIX)
        .and_then(|uid| uid.parse::<u32>().ok())
        .is_some_and(|uid| (90_000..98_000).contains(&uid))
}

/// A tile and the user ID the launcher gave it, which is also its group ID.
struct TileUser {
    name: String,
    uid: u32,
}

/// The user IDs a test host's launcher allocates from: above any account on a test host, outside
/// systemd's ranges and the usual subordinate ranges, apart from the production default.
fn test_range() -> (u32, u32) {
    let first = 90_000 + (std::process::id() % 80) * 100;
    (first, first + 99)
}

/// Run `args` as root under user ID `uid` with no groups, on the host, outside any tile. The ID
/// belongs to no account, so `sudo -u` cannot name it.
fn as_uid<S: AsRef<std::ffi::OsStr>>(uid: u32, args: &[S]) -> Output {
    let ids = [
        format!("--reuid={uid}"),
        format!("--regid={uid}"),
        "--clear-groups".to_owned(),
    ];
    sudo(
        std::iter::once(std::ffi::OsStr::new("setpriv"))
            .chain(ids.iter().map(std::ffi::OsStr::new))
            .chain(std::iter::once(std::ffi::OsStr::new("--")))
            .chain(args.iter().map(AsRef::as_ref)),
    )
}

/// These tests share the host's tiles slice and, in one test, add and remove an account, which
/// `useradd` locks `/etc/passwd` for, and the first one sweeps what a killed run left: they run
/// one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SWEPT: std::sync::Once = std::sync::Once::new();

/// A disposable host: a root-owned directory with a copy of st, a registry, and one system user
/// per tile. Dropping it stops the tiles and removes the users and the directory.
struct Host {
    _serial: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
    st: PathBuf,
    registry: PathBuf,
    tiles: Vec<TileUser>,
}

impl Host {
    fn new(tiles: &[&str], tasks_max: u32) -> Self {
        Self::with(tiles, tasks_max, "", false)
    }

    /// A host whose registry also has `head`'s top-level settings, and whose tiles can run st
    /// itself when `st_inside`.
    fn with(tiles: &[&str], tasks_max: u32, head: &str, st_inside: bool) -> Self {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        SWEPT.call_once(sweep);
        let tag = format!(
            "{:x}{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
                % 0xffff
        );
        let dir = PathBuf::from(format!("/var/lib/st-tiles-test-{tag}"));
        ok(sudo(
            ["install", "-d", "-o", "root", "-g", "root", "-m", "0755"]
                .iter()
                .map(std::ffi::OsStr::new)
                .chain([dir.as_os_str()]),
        ));
        let bin = dir.join("bin");
        ok(sudo(
            ["install", "-d", "-o", "root", "-g", "root", "-m", "0755"]
                .iter()
                .map(std::ffi::OsStr::new)
                .chain([bin.as_os_str()]),
        ));
        let st = bin.join("st3");
        ok(sudo([
            "install".as_ref(),
            "-o".as_ref(),
            "root".as_ref(),
            "-g".as_ref(),
            "root".as_ref(),
            "-m".as_ref(),
            "0755".as_ref(),
            std::ffi::OsStr::new(test_env!("CARGO_BIN_EXE_st3")),
            st.as_os_str(),
        ]));
        let mut host = Self {
            _serial: serial,
            registry: dir.join("tiles.toml"),
            dir,
            st,
            tiles: Vec::new(),
        };
        let names: Vec<String> = tiles
            .iter()
            .map(|suffix| format!("t{tag}{suffix}"))
            .collect();
        let python = std::fs::canonicalize(which("python3")).unwrap();
        let mut roots = vec![python.clone()];
        let mut path = vec![python.parent().unwrap().to_path_buf()];
        let mut read_only = Vec::new();
        if st_inside {
            // st's own libraries, from the store paths its loader names.
            let libraries = text(&ok(Command::new("ldd").arg(&host.st).output().unwrap()));
            roots.extend(libraries.split_whitespace().filter_map(|word| {
                let rest = word.strip_prefix("/nix/store/")?;
                Some(PathBuf::from("/nix/store").join(rest.split('/').next()?))
            }));
            read_only.push(bin.display().to_string());
            path.push(bin.clone());
        }
        let closure = text(&ok(Command::new("nix-store")
            .arg("-qR")
            .args(&roots)
            .output()
            .unwrap()));
        read_only.extend(closure.lines().map(str::to_owned));
        read_only.sort();
        read_only.dedup();
        let (first, last) = test_range();
        let mut registry = format!(
            "state_root = \"{dir}/state\"\nruntime_root = \"{dir}/run\"\nuids = {{ first = {first}, last = {last} }}\nread_only = [{read_only}]\npath = [{path}]\n{head}",
            dir = host.dir.display(),
            read_only = read_only
                .iter()
                .map(|line| format!("\"{line}\""))
                .collect::<Vec<_>>()
                .join(", "),
            path = path
                .iter()
                .map(|dir| format!("\"{}\"", dir.display()))
                .collect::<Vec<_>>()
                .join(", "),
        );
        for name in &names {
            registry.push_str(&format!(
                "\n[tiles.{name}]\nagent = \"agent/fleet/fixture-example/{name}\"\nperson = \"person/example\"\nsupervisor_uid = {me}\ntasks_max = {tasks_max}\nmemory_max = \"256M\"\n",
                me = unsafe { libc::getuid() },
            ));
        }
        let mut tee = Command::new("sudo")
            .args(["-n", "tee"])
            .arg(&host.registry)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        tee.stdin
            .take()
            .unwrap()
            .write_all(registry.as_bytes())
            .unwrap();
        assert!(tee.wait().unwrap().success());
        ok(sudo([
            "chmod".as_ref(),
            "0644".as_ref(),
            host.registry.as_os_str(),
        ]));
        // The launcher gives each tile its user ID the first time it prepares or runs it.
        for name in names {
            host.prepare(&name);
            let uid = host.registry().uid(&name).expect("allocated");
            host.tiles.push(TileUser { name, uid });
        }
        host
    }

    fn st(&self, args: &[&str]) -> Output {
        let mut command = Command::new("sudo");
        command.arg("-n").arg(&self.st).arg("tiles");
        let (verb, rest) = args.split_first().unwrap();
        command
            .arg(verb)
            .arg("--registry")
            .arg(&self.registry)
            .args(rest);
        command.output().unwrap()
    }

    fn prepare(&self, name: &str) -> Output {
        ok(self.st(&["prepare", name]))
    }

    fn tile(&self, suffix: &str) -> &TileUser {
        self.tiles
            .iter()
            .find(|tile| tile.name.ends_with(suffix))
            .unwrap()
    }

    fn registry(&self) -> Registry {
        Registry::load(&self.registry).unwrap()
    }

    fn command(&self, tile: &TileUser, args: &[&str]) -> Command {
        let mut command = Command::new("sudo");
        command
            .arg("-n")
            .arg(&self.st)
            .args(["tiles", "run", "--registry"])
            .arg(&self.registry)
            .arg(&tile.name)
            .arg("--")
            .args(args);
        command
    }

    /// Run a Python probe in a tile and return what it printed.
    fn probe(&self, tile: &TileUser, code: &str, input: &str) -> Output {
        let mut child = self
            .command(tile, &["python3", "-c", code])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Start a long-running probe; it prints a line when ready, read with [`ready`].
    fn start(&self, tile: &TileUser, code: &str) -> Child {
        self.command(tile, &["python3", "-c", code])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn unit_state(&self, tile: &TileUser) -> String {
        let output = Command::new("systemctl")
            .args(["show", "-p", "ActiveState", "--value"])
            .arg(registry::tools_unit(&tile.name))
            .output()
            .unwrap();
        text(&output).trim().to_owned()
    }

    fn main_pid(&self, tile: &TileUser) -> u32 {
        let output = Command::new("systemctl")
            .args(["show", "-p", "MainPID", "--value"])
            .arg(registry::tools_unit(&tile.name))
            .output()
            .unwrap();
        text(&output).trim().parse().unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        for tile in &self.tiles {
            // `remove` stops the tile, removes its state and gives its user ID back.
            let _ = self.st(&["remove", &tile.name]);
            let _ = sudo([
                "systemctl",
                "stop",
                &registry::tools_unit(&tile.name),
                &registry::slice_unit(&tile.name),
            ]);
        }
        let _ = sudo([
            "rm".as_ref(),
            "-rf".as_ref(),
            "--one-file-system".as_ref(),
            self.dir.as_os_str(),
        ]);
        // The parent slice too, once no tile's slice is left in it.
        let parent = PathBuf::from("/sys/fs/cgroup").join(registry::TILES_SLICE);
        let children = std::fs::read_dir(&parent)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or(0);
        if parent.exists() && children == 0 {
            let _ = sudo(["systemctl", "stop", registry::TILES_SLICE]);
        }
    }
}

/// Remove what an earlier, killed run of these tests left: only this test's own prefix.
fn sweep() {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap();
    for user in passwd
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter(|user| ours(user))
    {
        let _ = sudo(["pkill", "-KILL", "-u", user]);
        let _ = sudo(["userdel", user]);
        let _ = sudo(["groupdel", user]);
    }
    let Ok(entries) = std::fs::read_dir("/var/lib") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("st-tiles-test-")
            && entry.metadata().is_ok_and(|m| {
                m.modified()
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() > 3600))
            })
        {
            let _ = sudo([
                "rm".as_ref(),
                "-rf".as_ref(),
                "--one-file-system".as_ref(),
                entry.path().as_os_str(),
            ]);
        }
    }
}

fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

/// Read the probe's first line: it is ready.
fn ready(child: &mut Child) -> String {
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(!line.is_empty(), "the probe exited before it was ready");
    line.trim().to_owned()
}

fn json(output: &Output) -> serde_json::Value {
    let stdout = text(output);
    serde_json::from_str(stdout.lines().last().unwrap_or_default()).unwrap_or_else(|error| {
        panic!(
            "{error}: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Processes of a Unix user anywhere on the host.
fn processes_of(uid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("Uid:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|uid| uid.parse::<u32>().ok())
                    == Some(uid)
            })
        })
        .collect()
}

const IDENTITY_PROBE: &str = r#"
import os
# Before any other import: a library may open files of its own (libffi opens its .so).
fds = sorted(os.listdir("/proc/self/fd"))
import json, socket
status = dict(line.split(":\t", 1) for line in open("/proc/self/status").read().splitlines() if ":\t" in line)
def exists(path):
    return os.path.lexists(path)
print(json.dumps({
  "uid": os.getuid(), "gid": os.getgid(), "groups": os.getgroups(),
  "pid": os.getpid(), "root": sorted(os.listdir("/")),
  "pids": sorted(int(p) for p in os.listdir("/proc") if p.isdigit()),
  "fds": fds,
  "cgroup": open("/proc/self/cgroup").read().strip(),
  "caps": [status[k].strip() for k in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb")],
  "no_new_privs": status["NoNewPrivs"].strip(), "seccomp": status["Seccomp"].strip(),
  "hostname": socket.gethostname(), "home": os.environ["HOME"], "cwd": os.getcwd(),
  "env": sorted(os.environ),
  "host_paths": [p for p in ("/sys", "/home", "/run/user", "/root",
                             "/var", "/etc/shadow", "/run/systemd", "/nix/var") if exists(p)],
  "interfaces": [name for _, name in socket.if_nameindex()],
  "root_writable": os.access("/", os.W_OK), "endpoint_writable": os.access("/run/st", os.W_OK),
}))
os.sys.exit(int(input() or 0))
"#;

#[test]
fn a_tile_runs_as_its_own_user_seeing_nothing_of_the_host() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a"], 64);
    let a = host.tile("a");
    let output = host.probe(a, IDENTITY_PROBE, "7\n");
    assert_eq!(
        output.status.code(),
        Some(7),
        "the command's exit status comes back"
    );
    let seen = json(&output);
    assert_eq!(seen["uid"], a.uid);
    assert_eq!(seen["gid"], a.uid, "a tile's group ID is its user ID");
    assert_eq!(seen["groups"], serde_json::json!([]));
    // PID 1 is the tile's own reaper; the command is the only other process.
    assert_eq!(seen["pid"], 2);
    assert_eq!(seen["pids"], serde_json::json!([1, 2]));
    assert_eq!(
        seen["root"],
        serde_json::json!(["dev", "etc", "nix", "proc", "run", "tile", "tmp"])
    );
    assert_eq!(
        seen["fds"],
        serde_json::json!(["0", "1", "2", "3"]),
        "only stdio and the listing"
    );
    assert_eq!(seen["cgroup"], "0::/");
    assert_eq!(
        seen["caps"],
        serde_json::json!(vec!["0000000000000000"; 5]),
        "no capability in any set"
    );
    assert_eq!(seen["no_new_privs"], "1");
    assert_eq!(seen["seccomp"], "2");
    assert_eq!(seen["hostname"], format!("tile-{}", a.name));
    assert_eq!(seen["home"], "/tile/home");
    assert_eq!(seen["cwd"], "/tile/work");
    assert_eq!(seen["host_paths"], serde_json::json!([]));
    assert_eq!(seen["interfaces"], serde_json::json!(["lo"]));
    assert_eq!(seen["root_writable"], false);
    assert_eq!(seen["endpoint_writable"], false);
    let env = seen["env"].as_array().unwrap();
    assert!(
        env.iter().all(|name| [
            "HOME",
            "USER",
            "LOGNAME",
            "TMPDIR",
            "PATH",
            "ST_TILE",
            "ST_AGENT",
            "ST_TILE_ENDPOINT",
            "TERM",
            "LC_CTYPE"
        ]
        .contains(&name.as_str().unwrap())),
        "the environment is built from nothing: {env:?}"
    );
}

const SIBLING_SERVER: &str = r#"
import ctypes, os, socket, sys
libc = ctypes.CDLL(None, use_errno=True)
for path in (os.environ["HOME"] + "/secret", "/tile/work/secret", "/tmp/secret", "/dev/shm/secret"):
    open(path, "w").write("tile a's secret")
abstract = socket.socket(socket.AF_UNIX); abstract.bind("\0st-tile-probe"); abstract.listen()
pathname = socket.socket(socket.AF_UNIX); pathname.bind("/tmp/probe.sock"); pathname.listen()
os.chmod("/tmp/probe.sock", 0o777)
tcp = socket.socket(); tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp.bind(("127.0.0.1", 47123)); tcp.listen()
shm = libc.shmget(0x5717e, 4096, 0o1666 | 0o1000)
print(os.getpid(), shm, flush=True)
sys.stdin.read()
"#;

const SIBLING_CLIENT: &str = r#"
import ctypes, json, os, socket
libc = ctypes.CDLL(None, use_errno=True)
results = {}
def attempt(name, action):
    try:
        action(); results[name] = "reached"
    except OSError as error:
        results[name] = error.errno
def unix(address):
    s = socket.socket(socket.AF_UNIX); s.settimeout(2); s.connect(address)
def tcp(port):
    s = socket.socket(); s.settimeout(2); s.connect(("127.0.0.1", port))
attempt("abstract", lambda: unix("\0st-tile-probe"))
attempt("host_abstract", lambda: unix("\0st-tile-host-probe"))
attempt("pathname", lambda: unix("/tmp/probe.sock"))
attempt("tcp", lambda: tcp(47123))
attempt("host_tcp", lambda: tcp(int(input())))
results["files"] = [p for p in (os.environ["HOME"] + "/secret", "/tile/work/secret", "/tmp/secret", "/dev/shm/secret") if os.path.exists(p)]
results["shm"] = libc.shmget(0x5717e, 0, 0)
results["pids"] = sorted(int(p) for p in os.listdir("/proc") if p.isdigit())
results["signal_host_pid"] = None
try:
    os.kill(int(input()), 0); results["signal_host_pid"] = "reached"
except OSError as error:
    results["signal_host_pid"] = error.errno
print(json.dumps(results))
"#;

#[test]
fn sibling_tiles_cannot_reach_each_other_or_the_host() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a", "b"], 64);
    let (a, b) = (host.tile("a"), host.tile("b"));
    let mut server = host.start(a, SIBLING_SERVER);
    let line = ready(&mut server);
    let shm: i64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(shm >= 0, "tile a made its shared memory segment");
    // The host listens on its loopback and an abstract socket too.
    let host_tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = host_tcp.local_addr().unwrap().port();
    let host_abstract = {
        use std::os::linux::net::SocketAddrExt as _;
        let address =
            std::os::unix::net::SocketAddr::from_abstract_name("st-tile-host-probe").unwrap();
        UnixListener::bind_addr(&address).unwrap()
    };
    let a_pid_on_host = host.main_pid(a);
    let seen = json(&ok(host.probe(
        b,
        SIBLING_CLIENT,
        &format!("{host_port}\n{a_pid_on_host}\n"),
    )));
    // Abstract sockets and loopback ports belong to each tile's network namespace.
    assert_eq!(seen["abstract"], libc::ECONNREFUSED);
    assert_eq!(seen["host_abstract"], libc::ECONNREFUSED);
    // A path in a's /tmp does not exist in b's.
    assert_eq!(seen["pathname"], libc::ENOENT);
    assert_eq!(seen["tcp"], libc::ECONNREFUSED);
    assert_eq!(seen["host_tcp"], libc::ECONNREFUSED);
    assert_eq!(seen["files"], serde_json::json!([]));
    // System V IPC is per IPC namespace.
    assert_eq!(seen["shm"], -1);
    assert_eq!(seen["pids"], serde_json::json!([1, 2]));
    // A pid of the host or of a sibling names nothing in b's PID namespace.
    assert_eq!(seen["signal_host_pid"], libc::ESRCH);
    drop((host_tcp, host_abstract));

    // Outside its namespaces, b's Unix user still cannot read a's private state.
    let a_home = host.registry().tile_state(&a.name).join("home");
    let as_b = as_uid(b.uid, &["cat".as_ref(), a_home.join("secret").as_os_str()]);
    assert!(!as_b.status.success());
    assert!(String::from_utf8_lossy(&as_b.stderr).contains("Permission denied"));
    // Not even through its own user: only root and the tile, inside its namespaces, reach it.
    let as_a = as_uid(a.uid, &["cat".as_ref(), a_home.join("secret").as_os_str()]);
    assert!(!as_a.status.success());
    let as_root = ok(sudo(["cat".as_ref(), a_home.join("secret").as_os_str()]));
    assert_eq!(
        text(&as_root),
        "tile a's secret",
        "the file the probe wrote is there"
    );
    drop(server.stdin.take());
    assert!(server.wait().unwrap().success());
}

#[test]
fn a_tile_reads_no_key_of_its_supervisor() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a"], 64);
    let a = host.tile("a");
    // st writes a node key the way the daemon does: a 0600 file in a 0700 directory.
    let state = tempfile::Builder::new()
        .prefix("st-tiles-supervisor-")
        .tempdir_in("/var/tmp")
        .unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let keys = state.path().join("keys");
    smallclaims::fleet::keys::MemberKey::load_or_create(&keys.join("node.key")).unwrap();
    for path in [keys.clone(), keys.join("node.key")] {
        let read = as_uid(a.uid, &["cat".as_ref(), path.as_os_str()]);
        assert!(
            !read.status.success(),
            "{} is readable by the tile",
            path.display()
        );
    }
    // Inside the tile the supervisor's state does not exist at all.
    let code = format!(
        "import os; print(os.path.lexists({:?}), os.path.lexists('/var/tmp'))",
        keys.join("node.key")
    );
    assert_eq!(text(&ok(host.probe(a, &code, ""))).trim(), "False False");
}

const ENDPOINT_CLIENT: &str = r#"
import os, socket
s = socket.socket(socket.AF_UNIX); s.connect("/run/st/probe.sock"); s.sendall(b"hello")
print(s.recv(100).decode())
try:
    open("/run/st/planted", "w"); print("planted")
except OSError as error:
    print(error.errno)
"#;

#[test]
fn a_tile_reaches_its_endpoint_and_the_kernel_says_which_tile_called() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a", "b"], 64);
    let (a, b) = (host.tile("a"), host.tile("b"));
    // The endpoint directory exists once a tile has launched.
    ok(host.probe(a, "pass", ""));
    let registry = host.registry();
    let endpoint = registry.endpoint_dir(&a.name).join("probe.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o666)).unwrap();
    let serve = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (pid, uid) = peer(&stream);
        let cgroup = st3::sekrets::identity::process_cgroup(pid).unwrap();
        let identified = registry
            .identify(uid, &cgroup)
            .map(|(name, tile)| (name.to_owned(), tile.agent.clone()));
        let mut hello = [0u8; 5];
        stream.read_exact(&mut hello).unwrap();
        stream.write_all(b"ok").unwrap();
        (uid, cgroup, identified)
    });
    let output = ok(host.probe(a, ENDPOINT_CLIENT, ""));
    let (uid, cgroup, identified) = serve.join().unwrap();
    assert_eq!(uid, a.uid);
    assert_eq!(cgroup, registry::tools_cgroup(&a.name));
    assert_eq!(
        identified,
        Some((
            a.name.clone(),
            format!("agent/fleet/fixture-example/{}", a.name)
        ))
    );
    let lines = text(&output);
    let mut lines = lines.lines();
    assert_eq!(lines.next(), Some("ok"));
    assert_eq!(
        lines.next(),
        Some(libc::EROFS.to_string().as_str()),
        "the tile cannot plant files there"
    );
    // A sibling's endpoint is not in b at all; b's own is empty.
    let seen = text(&ok(host.probe(
        b,
        "import os; print(os.listdir('/run/st'))",
        "",
    )));
    assert_eq!(seen.trim(), "[]");
}

fn peer(stream: &std::os::unix::net::UnixStream) -> (i32, u32) {
    use std::os::fd::AsRawFd as _;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    assert_eq!(result, 0);
    (credentials.pid, credentials.uid)
}

const ESCAPE_PROBE: &str = r#"
import ctypes, json, os
libc = ctypes.CDLL(None, use_errno=True)
results = {}
def call(name, *args):
    result = getattr(libc, name)(*args)
    results.setdefault(name, []).append(ctypes.get_errno() if result == -1 else "succeeded")
for flag in (0x10000000, 0x00020000, 0x40000000, 0x20000000, 0x08000000, 0x04000000, 0x02000000):
    call("unshare", flag)
fd = os.open("/proc/self/ns/net", os.O_RDONLY)
call("setns", fd, 0)
call("mount", b"none", b"/tmp", b"tmpfs", 0, None)
call("umount2", b"/proc", 2)
call("chroot", b"/tmp")
results["clone3"] = ctypes.get_errno() if libc.syscall(435, None, 0) == -1 else "succeeded"
CLONE_NEWUSER = 0x10000000; SIGCHLD = 17
results["clone_newuser"] = ctypes.get_errno() if libc.syscall(56, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0) == -1 else "succeeded"
child = os.fork()
if child == 0:
    os._exit(0)
os.waitpid(child, 0); results["fork"] = "succeeded"
results["setuid_root"] = ctypes.get_errno() if libc.setuid(0) == -1 else "succeeded"
children = []
try:
    for _ in range(64):
        pid = os.fork()
        if pid == 0:
            import time; time.sleep(30); os._exit(0)
        children.append(pid)
    results["fork_limit"] = "none"
except OSError as error:
    results["fork_limit"] = (error.errno, len(children))
for pid in children:
    os.kill(pid, 9)
import platform
numbers = {
    "x86_64": {"io_uring_setup": 425, "bpf": 321, "userfaultfd": 323, "perf_event_open": 298,
               "add_key": 248, "request_key": 249, "keyctl": 250, "kcmp": 312,
               "process_vm_readv": 310, "process_vm_writev": 311},
    "aarch64": {"io_uring_setup": 425, "bpf": 280, "userfaultfd": 282, "perf_event_open": 241,
                "add_key": 217, "request_key": 218, "keyctl": 219, "kcmp": 272,
                "process_vm_readv": 270, "process_vm_writev": 271},
}[platform.machine()]
# keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING, 1) works for any user without the filter.
arguments = {"keyctl": (0, -3 & 0xffffffff, 1), "io_uring_setup": (1, ctypes.create_string_buffer(120))}
for name, number in numbers.items():
    result = libc.syscall(number, *arguments.get(name, (0, 0, 0, 0, 0)))
    results["kernel_" + name] = ctypes.get_errno() if result == -1 else "succeeded"
print(json.dumps(results), flush=True)
"#;

#[test]
fn a_tile_cannot_make_or_join_namespaces_mount_or_leave_its_cgroup() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a", "b"], 16);
    let (a, b) = (host.tile("a"), host.tile("b"));
    let seen = json(&ok(host.probe(a, ESCAPE_PROBE, "")));
    assert_eq!(seen["unshare"], serde_json::json!(vec![libc::EPERM; 7]));
    assert_eq!(seen["setns"], serde_json::json!([libc::EPERM]));
    assert_eq!(seen["mount"], serde_json::json!([libc::EPERM]));
    assert_eq!(seen["umount2"], serde_json::json!([libc::EPERM]));
    assert_eq!(seen["chroot"], serde_json::json!([libc::EPERM]));
    assert_eq!(seen["clone3"], libc::ENOSYS);
    assert_eq!(seen["clone_newuser"], libc::EPERM);
    assert_eq!(seen["fork"], "succeeded");
    assert_eq!(seen["setuid_root"], libc::EPERM);
    assert_eq!(seen["kernel_io_uring_setup"], libc::ENOSYS);
    for call in [
        "bpf",
        "userfaultfd",
        "perf_event_open",
        "add_key",
        "request_key",
        "keyctl",
        "kcmp",
        "process_vm_readv",
        "process_vm_writev",
    ] {
        assert_eq!(seen[format!("kernel_{call}")], libc::EPERM, "{call}");
    }
    // tasks_max = 16 counts the unit's launcher, the tile's init, the probe and thirteen
    // children; the next fork is refused.
    assert_eq!(seen["fork_limit"], serde_json::json!([libc::EAGAIN, 13]));

    // Outside its namespaces, a's user cannot move a process into b's cgroup or its own: root
    // owns both, and nothing is delegated.
    let mut b_running = host.start(
        b,
        "import sys; print('ready', flush=True); sys.stdin.read()",
    );
    ready(&mut b_running);
    for tile in [b, a] {
        let procs = format!(
            "/sys/fs/cgroup{}/cgroup.procs",
            registry::tools_cgroup(&tile.name)
        );
        let moved = as_uid(a.uid, &["sh", "-c", format!("echo $$ > {procs}").as_str()]);
        assert!(!moved.status.success());
        assert!(
            String::from_utf8_lossy(&moved.stderr).contains("Permission denied")
                || String::from_utf8_lossy(&moved.stderr).contains("No such file"),
            "{}",
            String::from_utf8_lossy(&moved.stderr)
        );
    }
    let b_cgroup = PathBuf::from(format!("/sys/fs/cgroup{}", registry::tools_cgroup(&b.name)));
    let owner = std::fs::metadata(b_cgroup.join("cgroup.procs")).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(owner.uid(), 0, "root owns the tile's cgroup");
    assert_eq!(
        std::fs::read_to_string(b_cgroup.join("pids.max"))
            .unwrap()
            .trim(),
        "16"
    );
    assert_eq!(
        std::fs::read_to_string(b_cgroup.join("memory.max"))
            .unwrap()
            .trim(),
        (256u64 << 20).to_string()
    );
    drop(b_running.stdin.take());
    assert!(b_running.wait().unwrap().success());
}

/// A tile that leaves work behind: a child in its own session that ignores SIGTERM, and a
/// grandchild that double-forked away from it.
const LINGERING: &str = r#"
import os, signal, sys, time
for _ in range(2):
    if os.fork() == 0:
        os.setsid()
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        if os.fork() == 0:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            time.sleep(600); os._exit(0)
        time.sleep(600); os._exit(0)
time.sleep(0.2)
print("ready", len([p for p in os.listdir("/proc") if p.isdigit()]), flush=True)
sys.stdin.read()
"#;

/// After the tile ends, nothing of it is left on the host: no unit, no cgroup, no process of
/// its user, no mount.
fn assert_gone(host: &Host, tile: &TileUser) {
    let state = host.unit_state(tile);
    assert!(
        state == "inactive" || state.is_empty(),
        "the unit is {state}, not gone"
    );
    let cgroup = PathBuf::from(format!(
        "/sys/fs/cgroup{}",
        registry::tools_cgroup(&tile.name)
    ));
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
    assert_eq!(
        processes_of(tile.uid),
        Vec::<u32>::new(),
        "processes of the tile are left"
    );
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        !mounts.contains(host.dir.to_str().unwrap()),
        "a tile mount is left on the host"
    );
}

#[test]
fn a_tile_ends_completely_however_its_launch_ends() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a"], 64);
    let a = host.tile("a");
    for ending in [
        "launcher killed",
        "tile init killed",
        "stopped",
        "command exits",
    ] {
        let mut tile = host.start(a, LINGERING);
        let line = ready(&mut tile);
        assert_eq!(
            line, "ready 6",
            "{ending}: init, the probe, two children, two grandchildren"
        );
        assert_eq!(processes_of(a.uid).len(), 6, "{ending}");
        let main = host.main_pid(a);
        match ending {
            "launcher killed" => {
                ok(sudo(["kill", "-KILL", &main.to_string()]));
            }
            "tile init killed" => {
                let init = std::fs::read_to_string(format!("/proc/{main}/task/{main}/children"))
                    .unwrap()
                    .trim()
                    .to_owned();
                ok(sudo(["kill", "-KILL", &init]));
            }
            "stopped" => {
                ok(sudo([
                    host.st.as_os_str(),
                    "tiles".as_ref(),
                    "stop".as_ref(),
                    a.name.as_ref(),
                ]));
            }
            _ => drop(tile.stdin.take()),
        }
        // `st tiles run` waits for the unit to be gone.
        let status = tile.wait().unwrap();
        // A stop is a clean end for systemd; a killed launcher or init is a failure.
        match ending {
            "command exits" | "stopped" => assert!(status.success(), "{ending}: {status:?}"),
            _ => assert!(!status.success(), "{ending}: {status:?}"),
        }
        assert_gone(&host, a);
    }
    // And the tile starts again cleanly.
    assert_eq!(
        text(&ok(host.probe(a, "print('again')", ""))).trim(),
        "again"
    );
    assert_gone(&host, a);
}

#[test]
fn only_root_starts_a_tile_and_only_inside_its_unit() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a"], 64);
    let a = host.tile("a");
    let as_user = Command::new(&host.st)
        .args(["tiles", "run", "--registry"])
        .arg(&host.registry)
        .args([&a.name, "--", "true"])
        .output()
        .unwrap();
    assert!(!as_user.status.success());
    assert!(String::from_utf8_lossy(&as_user.stderr).contains("runs as root"));
    // Root outside the tile's unit is refused before any namespace is made.
    let outside = sudo([
        host.st.as_os_str(),
        "tiles".as_ref(),
        "enter".as_ref(),
        "--registry".as_ref(),
        host.registry.as_os_str(),
        a.name.as_ref(),
        "--".as_ref(),
        "true".as_ref(),
    ]);
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("runs only as the main process"));
    // A registry someone other than root can change is refused.
    let copy = host.dir.join("copy.toml");
    ok(sudo([
        "cp".as_ref(),
        host.registry.as_os_str(),
        copy.as_os_str(),
    ]));
    ok(sudo(["chmod".as_ref(), "0666".as_ref(), copy.as_os_str()]));
    let writable = sudo([
        host.st.as_os_str(),
        "tiles".as_ref(),
        "run".as_ref(),
        "--registry".as_ref(),
        copy.as_os_str(),
        a.name.as_ref(),
        "--".as_ref(),
        "true".as_ref(),
    ]);
    assert!(!writable.status.success());
    assert!(
        String::from_utf8_lossy(&writable.stderr)
            .contains("can be changed by someone other than root")
    );
    // An account that takes a tile's user ID would be the tile: the launch is refused.
    let account = format!("{PREFIX}{}", a.uid);
    assert!(ours(&account));
    ok(sudo([
        "useradd",
        "--system",
        "--no-create-home",
        "--home-dir",
        "/nonexistent",
        "--shell",
        "/run/current-system/sw/bin/nologin",
        "--no-user-group",
        "--uid",
        &a.uid.to_string(),
        &account,
    ]));
    let taken = host.st(&["run", &a.name, "--", "true"]);
    let _ = sudo(["userdel", &account]);
    assert!(!taken.status.success());
    assert!(
        String::from_utf8_lossy(&taken.stderr).contains("now belongs to the account"),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );
    // A st that someone other than root can change never runs as root in a tile's unit.
    let user_st = Command::new("sudo")
        .arg("-n")
        .arg(test_env!("CARGO_BIN_EXE_st3"))
        .args(["tiles", "run", "--registry"])
        .arg(&host.registry)
        .args([&a.name, "--", "true"])
        .output()
        .unwrap();
    assert!(!user_st.status.success());
    assert!(String::from_utf8_lossy(&user_st.stderr).contains("root must control it"));
}

#[test]
fn a_launch_with_an_inherited_descriptor_does_not_hand_it_to_the_tile() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a"], 64);
    let a = host.tile("a");
    let secret = tempfile::NamedTempFile::new_in("/var/tmp").unwrap();
    std::fs::write(secret.path(), "inherited").unwrap();
    // Root's shell opens the file on descriptor 9 and keeps it open across exec into st.
    let script = format!(
        "exec 9<{} 8>&1 && exec {} tiles run --registry {} {} -- python3 -c \"$0\"",
        secret.path().display(),
        host.st.display(),
        host.registry.display(),
        a.name
    );
    // The listing's own descriptor is closed by the time it is read.
    let code = "import json, os\nfds = {}\nfor fd in os.listdir('/proc/self/fd'):\n    try: fds[fd] = os.readlink('/proc/self/fd/' + fd)\n    except OSError: fds[fd] = 'closed'\nprint(json.dumps(fds))";
    let output = ok(sudo(["sh", "-c", &script, code]));
    let seen = json(&output);
    let fds = seen.as_object().unwrap();
    let mut names = fds.keys().cloned().collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["0", "1", "2", "3"], "{seen}");
    assert!(
        fds.values()
            .all(|target| !target.as_str().unwrap().contains("tmp")),
        "{seen}"
    );
}

const ENDPOINT_ST: &str = r#"
import http.client, json, socket
class Endpoint(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect("/run/st/st.sock")
def call(method, path, body=None):
    connection = Endpoint("tile")
    connection.request(method, path, body=None if body is None else json.dumps(body),
                       headers={"content-type": "application/json"})
    response = connection.getresponse()
    data = response.read()
    try:
        data = json.loads(data)
    except ValueError:
        data = None
    return [response.status, data]
other = input()
me = call("GET", "/v1/tile/whoami")[1]["agent"]
print(json.dumps({
    "whoami": call("GET", "/v1/tile/whoami"),
    "send": call("POST", "/v1/messages", {"idempotency_key": "tile-1", "from": me, "to": other,
                                         "content": "from the tile"}),
    "send_as_other": call("POST", "/v1/messages", {"idempotency_key": "tile-2", "from": other,
                                                  "to": me, "content": "forged"}),
    "other_mailbox": call("GET", "/v1/messages?to=" + other),
    "admin": call("POST", "/v1/intent/apply", {}),
}))
"#;

#[test]
fn a_tile_acts_through_its_endpoint_only_as_its_own_agent() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a", "b"], 64);
    let (a, b) = (host.tile("a"), host.tile("b"));
    let registry = host.registry();
    for tile in [a, b] {
        ok(sudo([
            host.st.as_os_str(),
            "tiles".as_ref(),
            "prepare".as_ref(),
            "--registry".as_ref(),
            host.registry.as_os_str(),
            tile.name.as_ref(),
        ]));
    }
    // This test process is the tiles' supervisor: its daemon state serves both endpoints.
    let state_dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        st3::store::Store::open(&state_dir.path().join("graph.sqlite3"), "example").unwrap(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    {
        let _context = runtime.enter();
        st3::api::spawn_tile_endpoints_from(
            st3::api::AppState {
                store: store.clone(),
                notify: std::sync::Arc::new(tokio::sync::Notify::new()),
                event_notify: tokio::sync::watch::channel(0_u64).0,
                node: "example".into(),
                state_dir: state_dir.path().to_path_buf(),
                pty_root: state_dir.path().join("pty"),
                pty_binary: "pty".into(),
                fleet_id: None,
                configured_peers: vec![],
                client_relay: None,
                native_session_home: None,
                planner_default: st3::model::PlannerSpec::default(),
            },
            host.registry.clone(),
        );
    }
    for tile in [a, b] {
        let socket = registry
            .endpoint_dir(&tile.name)
            .join(st3::api::TILE_ENDPOINT_SOCKET);
        // The daemon binds when it sees the directory; give it a bounded moment.
        let mut served = false;
        for _ in 0..400 {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                served = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(served, "{} is not served", socket.display());
    }
    let a_agent = format!("agent/fleet/fixture-example/{}", a.name);
    let b_agent = format!("agent/fleet/fixture-example/{}", b.name);
    let seen = json(&ok(host.probe(a, ENDPOINT_ST, &format!("{b_agent}\n"))));
    assert_eq!(seen["whoami"][0], 200);
    assert_eq!(seen["whoami"][1]["agent"], a_agent);
    assert_eq!(seen["whoami"][1]["tile"], a.name);
    assert_eq!(seen["send"][0], 200, "{seen}");
    assert_eq!(seen["send"][1]["from"], a_agent);
    assert_eq!(seen["send_as_other"][0], 403, "{seen}");
    assert_eq!(seen["other_mailbox"][0], 403, "{seen}");
    assert_eq!(seen["admin"][0], 403, "{seen}");
    // The graph holds the tile's message, from its own agent, and never the forged one.
    let mail = store.messages(Some(&b_agent), true).unwrap();
    assert!(
        mail.iter()
            .any(|message| message.from == a_agent && message.content == "from the tile")
    );
    let forged = store.messages(Some(&a_agent), true).unwrap();
    assert!(forged.iter().all(|message| message.content != "forged"));
    // The sibling's endpoint answers for the sibling alone.
    let b_seen = text(&ok(host.probe(
        b,
        "import socket, http.client\nclass E(http.client.HTTPConnection):\n    def connect(self):\n        self.sock = socket.socket(socket.AF_UNIX); self.sock.connect('/run/st/st.sock')\nc = E('tile'); c.request('GET', '/v1/tile/whoami'); print(c.getresponse().read().decode())",
        "",
    )));
    assert!(b_seen.contains(&b_agent), "{b_seen}");
    assert!(!b_seen.contains(&a_agent), "{b_seen}");
    drop(runtime);
}

/// The kernel's word about a caller, read from /proc as the gateway itself does.
struct ProcKernel;

impl st3::sekrets::gateway::Kernel for ProcKernel {
    fn cgroup(&self, pid: i32) -> Option<String> {
        st3::sekrets::identity::process_cgroup(pid)
    }
    fn start(&self, pid: i32) -> Option<u64> {
        st3::sekrets::identity::process_start(pid)
    }
}

#[test]
fn sekrets_names_a_tile_its_agent_and_its_user_outside_the_tile_no_one() {
    if !enabled() {
        return;
    }
    // A gateway like st-sekrets.service, run by this test's user over a disposable store.
    let gateway_dir = tempfile::Builder::new()
        .prefix("st-tiles-sekrets-")
        .tempdir_in("/var/tmp")
        .unwrap();
    std::fs::set_permissions(gateway_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = gateway_dir.path().join("store");
    std::fs::create_dir(&store).unwrap();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = gateway_dir.path().join("gateway.sock");
    let host = Host::with(
        &["a"],
        64,
        &format!("sekrets_socket = {:?}\n", socket),
        true,
    );
    let a = host.tile("a");
    let config: st3::sekrets::gateway::GatewayConfig = toml::from_str(&format!(
        "socket = {socket:?}\nstore = {store:?}\npath = [\"/run/current-system/sw/bin\"]\n\
         tile_registry = {registry:?}\n[people]\n",
        registry = host.registry,
    ))
    .unwrap();
    // The gateway reads root's registry the way it does in production: Registry::load.
    let gateway = std::sync::Arc::new(
        st3::sekrets::gateway::Gateway::new(
            config,
            unsafe { libc::getuid() },
            None,
            Box::new(ProcKernel),
        )
        .unwrap(),
    );
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    std::thread::spawn(move || gateway.serve_listener(listener));

    // Inside the tile, at the client's default socket, with no daemon to vouch for it.
    let inside = ok(host
        .command(a, &["st3", "sekrets", "whoami"])
        .output()
        .unwrap());
    let said = text(&inside);
    assert!(
        said.contains(&format!(
            "agent/fleet/fixture-example/{}, working for person/example",
            a.name
        )),
        "{said}\n{}",
        String::from_utf8_lossy(&inside.stderr)
    );

    // The same Unix user outside the tile is no one.
    let outside = as_uid(
        a.uid,
        &[
            "env".as_ref(),
            "-i".as_ref(),
            format!("ST_SEKRETS_SOCKET={}", socket.display()).as_ref(),
            "HOME=/nonexistent".as_ref(),
            host.st.as_os_str(),
            "sekrets".as_ref(),
            "whoami".as_ref(),
        ],
    );
    let said = format!(
        "{}{}",
        text(&outside),
        String::from_utf8_lossy(&outside.stderr)
    );
    assert!(said.contains("outside the tile"), "{said}");
    assert!(!said.contains(", working for"), "{said}");

    // Without its gateway, a tile that should reach sekrets is refused, not started.
    std::fs::remove_file(&socket).unwrap();
    let refused = host.probe(a, "print('started')", "");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("no socket there"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn the_launcher_allocates_each_tiles_user_id_and_gives_it_back() {
    if !enabled() {
        return;
    }
    let host = Host::new(&["a", "b"], 64);
    let (a, b) = (host.tile("a"), host.tile("b"));
    let (first, last) = test_range();
    // Each tile has its own ID from the range, and no account or group has it.
    assert_ne!(a.uid, b.uid);
    for tile in [a, b] {
        assert!((first..=last).contains(&tile.uid), "{}", tile.uid);
        for database in ["passwd", "group"] {
            let entry = Command::new("getent")
                .args([database, &tile.uid.to_string()])
                .output()
                .unwrap();
            assert!(!entry.status.success(), "{database} has {}", tile.uid);
        }
    }
    // Root's launcher keeps the record, readable by the daemon and the gateway, writable by no
    // one else.
    let uids = host.registry().uids_file();
    let metadata = std::fs::metadata(&uids).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(
        (metadata.uid(), metadata.mode() & 0o7777),
        (0, 0o644),
        "{}",
        uids.display()
    );
    // The ID lasts across launches: the tile's state is still its own.
    let wrote = host.probe(
        a,
        "import os; open(os.environ['HOME'] + '/kept', 'w').write('kept'); print(os.getuid())",
        "",
    );
    assert!(wrote.status.success());
    assert_eq!(text(&wrote).trim(), a.uid.to_string());
    host.prepare(&a.name);
    assert_eq!(host.registry().uid(&a.name), Some(a.uid));
    let again = host.probe(
        a,
        "import os; print(open(os.environ['HOME'] + '/kept').read())",
        "",
    );
    assert_eq!(text(&again).trim(), "kept");

    // A process of the tile's ID outside the tile keeps its ID allocated.
    let mut stray = Command::new("sudo")
        .args([
            "-n",
            "setpriv",
            &format!("--reuid={}", a.uid),
            &format!("--regid={}", a.uid),
            "--clear-groups",
            "--",
            "sleep",
            "300",
        ])
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if !processes_of(a.uid).is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!processes_of(a.uid).is_empty(), "the stray process started");
    let refused = host.st(&["remove", &a.name]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("still runs processes"));
    assert_eq!(host.registry().uid(&a.name), Some(a.uid));
    let _ = sudo(["pkill", "-KILL", "-U", &a.uid.to_string()]);
    let _ = stray.wait();

    // Removing the tile removes its state and gives the ID back.
    ok(host.st(&["remove", &a.name]));
    let state = host.registry().tile_state(&a.name);
    assert!(
        !sudo([
            std::ffi::OsStr::new("test"),
            std::ffi::OsStr::new("-e"),
            state.as_os_str()
        ])
        .status
        .success(),
        "{} remains",
        state.display()
    );
    assert_eq!(host.registry().uid(&a.name), None);
    assert!(processes_of(a.uid).is_empty());

    // An ID that still owns a file under the state root is never handed out again, even with
    // no record of it.
    let leftover = host.registry().state_root.join("leftover");
    ok(sudo([
        std::ffi::OsStr::new("install"),
        std::ffi::OsStr::new("-d"),
        std::ffi::OsStr::new("-o"),
        std::ffi::OsStr::new(&a.uid.to_string()),
        std::ffi::OsStr::new("-m"),
        std::ffi::OsStr::new("0700"),
        leftover.as_os_str(),
    ]));
    host.prepare(&a.name);
    let fresh = host.registry().uid(&a.name).unwrap();
    assert_ne!(
        fresh,
        a.uid,
        "an ID that owns {} was reused",
        leftover.display()
    );
    assert_ne!(fresh, b.uid);
    // Starting afresh, nothing of the old tile is there.
    let home = host.probe(a, "import os; print(os.listdir(os.environ['HOME']))", "");
    assert_eq!(text(&home).trim(), "[]");

    // A range that holds an account is refused before anything is allocated.
    let account = std::fs::read_to_string("/etc/passwd")
        .unwrap()
        .lines()
        .filter_map(|line| line.split(':').nth(2)?.parse::<u32>().ok())
        .find(|uid| (1000..60_000).contains(uid) && *uid != unsafe { libc::getuid() })
        .expect("an account with an ordinary user ID");
    let narrow = host.dir.join("narrow.toml");
    let text_now = std::fs::read_to_string(&host.registry).unwrap().replace(
        &format!("uids = {{ first = {first}, last = {last} }}"),
        &format!("uids = {{ first = {account}, last = {account} }}"),
    );
    let mut tee = Command::new("sudo")
        .args(["-n", "tee"])
        .arg(&narrow)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    tee.stdin
        .take()
        .unwrap()
        .write_all(
            text_now
                .replace(
                    &format!("{}/state", host.dir.display()),
                    &format!("{}/narrow-state", host.dir.display()),
                )
                .as_bytes(),
        )
        .unwrap();
    assert!(tee.wait().unwrap().success());
    ok(sudo([
        "chmod".as_ref(),
        "0644".as_ref(),
        narrow.as_os_str(),
    ]));
    let holds = sudo([
        host.st.as_os_str(),
        "tiles".as_ref(),
        "prepare".as_ref(),
        "--registry".as_ref(),
        narrow.as_os_str(),
        b.name.as_ref(),
    ]);
    assert!(!holds.status.success());
    assert!(
        String::from_utf8_lossy(&holds.stderr).contains("holds the account"),
        "{}",
        String::from_utf8_lossy(&holds.stderr)
    );
}
