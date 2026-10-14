//! The managed Codex profile under the real Codex sandbox (`codex sandbox`), no model or daemon.
//!
//! The argv tests prove which flags a seat receives; these prove what the named profile lets a
//! command do under `codex sandbox`. A seat launched by the daemon runs through an app server and
//! a remote TUI, which resolves the profile differently (it drops the profile's network table, so
//! the driver also passes `sandbox_workspace_write.network_access`); the live eval
//! `evals/st3/codex-minimal-access` proves that path with a real seat. They need a `codex` that supports `sandbox -P` and `python3`, and skip loudly without them
//! unless `ST_REQUIRE_CODEX_SANDBOX=1`, which the hetz2 run sets.

use std::fs;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use st_drivers::access::{CODEX_PROFILE, codex_profile};

fn available() -> bool {
    let ok = |program: &str| {
        Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    };
    let ready = ok("codex") && ok("python3") && ok("git");
    if !ready {
        assert!(
            std::env::var_os("ST_REQUIRE_CODEX_SANDBOX").is_none(),
            "ST_REQUIRE_CODEX_SANDBOX is set but codex, python3 or git is missing"
        );
        eprintln!("SKIPPED: the real-sandbox controls need codex, python3 and git on PATH");
    }
    ready
}

/// A directory the sandbox treats as outside every writable root: not a temporary directory.
fn outside_root() -> Option<PathBuf> {
    let base = PathBuf::from(test_env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(&base).ok()?;
    let canonical = fs::canonicalize(&base).ok()?;
    let temporary = ["/tmp", "/var/tmp"]
        .iter()
        .map(PathBuf::from)
        .chain(std::env::var_os("TMPDIR").map(PathBuf::from))
        .filter_map(|path| fs::canonicalize(path).ok());
    for root in temporary {
        if canonical.starts_with(&root) {
            return None;
        }
    }
    Some(canonical)
}

struct Fixture {
    _root: tempfile::TempDir,
    base: PathBuf,
    socket: PathBuf,
    _listener: UnixListener,
}

impl Fixture {
    fn new() -> Option<Self> {
        let base = outside_root()?;
        let root = tempfile::Builder::new().prefix("st-sb-").tempdir_in(&base).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        // A short path: a socket path has a hard length limit.
        let socket = std::env::temp_dir().join(format!("st-sb-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        Some(Self { _root: root, base: path, socket, _listener: listener })
    }
}

/// Run `script` under the managed profile exactly as the driver builds it for `workspace`.
fn sandboxed(workspace: &Path, socket: &Path, script: &str, args: &[&str]) -> Output {
    let profile = codex_profile(socket, workspace).unwrap();
    Command::new("codex")
        .args(["sandbox", "-C"])
        .arg(workspace)
        .args(["-P", CODEX_PROFILE, "-c"])
        .arg(format!("default_permissions=\"{CODEX_PROFILE}\""))
        .arg("-c")
        .arg(profile)
        .args(["--", "/bin/sh", "-c", script, "sh"])
        .args(args)
        .output()
        .unwrap()
}

fn says(output: &Output, text: &str) -> bool {
    String::from_utf8_lossy(&output.stdout).contains(text)
}

fn report(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const CONNECT: &str = "import socket,sys\ns=socket.socket(socket.AF_UNIX)\ns.connect(sys.argv[1])\nprint('connected')";

#[test]
fn the_profile_writes_the_workspace_and_temp_and_refuses_everything_else() {
    if !available() {
        return;
    }
    let Some(fixture) = Fixture::new() else {
        eprintln!("SKIPPED: the target directory is itself under a temporary directory");
        return;
    };
    let workspace = fixture.base.join("workspace");
    let elsewhere = fixture.base.join("elsewhere");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let temp = std::env::temp_dir().join(format!("st-sb-temp-{}", std::process::id()));
    let _ = fs::remove_file(&temp);

    let written = sandboxed(&workspace, &fixture.socket, "echo ok > \"$1/inside.txt\" && echo wrote", &[workspace.to_str().unwrap()]);
    assert!(says(&written, "wrote"), "workspace write: {}", report(&written));
    assert!(workspace.join("inside.txt").exists());

    let temp_write = sandboxed(&workspace, &fixture.socket, "echo ok > \"$1\" && echo wrote", &[temp.to_str().unwrap()]);
    assert!(says(&temp_write, "wrote"), "temp write: {}", report(&temp_write));
    let _ = fs::remove_file(&temp);

    let outside = elsewhere.join("outside.txt");
    let refused = sandboxed(&workspace, &fixture.socket, "echo no > \"$1\" && echo wrote", &[outside.to_str().unwrap()]);
    assert!(!says(&refused, "wrote") && !outside.exists(), "outside write: {}", report(&refused));
    // bubblewrap mounts the rest read-only; macOS's Seatbelt denies the write outright.
    let refusal = if cfg!(target_os = "macos") { "Operation not permitted" } else { "Read-only file system" };
    assert!(String::from_utf8_lossy(&refused.stderr).contains(refusal), "{}", report(&refused));
    // The home directory is the other place a seat must not write.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|home| home.starts_with("/home") || home.starts_with("/Users")) {
        let marker = home.join(format!(".st-sandbox-control-{}", std::process::id()));
        let refused = sandboxed(&workspace, &fixture.socket, "echo no > \"$1\" && echo wrote", &[marker.to_str().unwrap()]);
        assert!(!says(&refused, "wrote") && !marker.exists(), "home write: {}", report(&refused));
    }
}

#[test]
fn the_profile_reaches_the_st_socket_and_the_network() {
    if !available() {
        return;
    }
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let workspace = fixture.base.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let connected = sandboxed(&workspace, &fixture.socket, &format!("python3 -c \"{CONNECT}\" \"$1\""), &[fixture.socket.to_str().unwrap()]);
    assert!(says(&connected, "connected"), "st socket: {}", report(&connected));

    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port().to_string();
    let reached = sandboxed(
        &workspace,
        &fixture.socket,
        "python3 -c \"import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 5); print('reached')\" \"$1\"",
        &[&port],
    );
    assert!(says(&reached, "reached"), "network: {}", report(&reached));
}

/// Codex 0.160.1 cannot express "network, but only this one local socket": with networking
/// enabled the sandbox lets any local Unix socket through, and with it disabled it refuses even
/// the allowed one. This control keeps that limit visible; when it starts failing, Codex learned
/// the rule and the profile and docs/st3/harness-access.md should tighten.
#[test]
fn the_known_limit_other_local_sockets_stay_reachable() {
    if !available() {
        return;
    }
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let workspace = fixture.base.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let other = std::env::temp_dir().join(format!("st-sb-other-{}.sock", std::process::id()));
    let _ = fs::remove_file(&other);
    let _listener = UnixListener::bind(&other).unwrap();
    let reached = sandboxed(&workspace, &fixture.socket, &format!("python3 -c \"{CONNECT}\" \"$1\""), &[other.to_str().unwrap()]);
    let _ = fs::remove_file(&other);
    assert!(
        says(&reached, "connected"),
        "Codex now refuses a socket the profile does not name; tighten the profile and the docs: {}",
        report(&reached)
    );
}
