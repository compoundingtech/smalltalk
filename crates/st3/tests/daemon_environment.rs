#![cfg(unix)]
//! A service manager supplies HOME and state locations, but no PATH or credentials.
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn start_service(root: &Path, home: &Path, state_home: &Path) -> (Service, std::path::PathBuf) {
    let socket = root.join("daemon.sock");
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture");
    let mut service = Service(
        st3::test_support::command(binary)
            .env_clear()
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", state_home)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .current_dir(root)
            .args(["up", "--node", "orchid"])
            .arg("--socket")
            .arg(&socket)
            .arg("--client-gateway-socket")
            .arg(root.join("client.sock"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            service.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(root.join("daemon.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "isolated service did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    (service, socket)
}

fn doctor_report(home: &Path, socket: &Path) -> serde_json::Value {
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        .env_clear()
        .env("HOME", home)
        .arg("--endpoint")
        .arg(socket)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
}

#[test]
fn a_fixture_listener_ignores_the_callers_host_seat_ancestry() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join(".bash_profile"), format!("export PATH='{}'\n", std::env::var("PATH").unwrap())).unwrap();
    let (_service, socket) = start_service(root.path(), &home, &root.path().join("state"));
    let source = root.path().join("fixture.kdl");
    std::fs::write(&source, r#"version 2
mission "hermetic" state="ready" {
  goal "Fixture goal."
  step "work" {
    goal "Fixture work."
    gate "done" { exec "true"; host "orchid"; workspace "."; time-limit "1m"; }
  }
}
"#).unwrap();
    let env = st_runtime::resolve_executable("env", &std::env::vars().collect()).unwrap();
    // Keep a synthetic agent parent alive while its CLI child has ST_AGENT removed. CI thus
    // exercises the ancestry boundary too, even when its runner is not itself an agent seat.
    let output = std::process::Command::new(env!("ST3_FIXTURE_BASH"))
        .env_clear()
        .env("HOME", &home)
        .env("ST_AGENT", "agent/fixture/host")
        .args(["--noprofile", "--norc", "-c", "\"$@\"; status=$?; exit \"$status\"", "fixture-parent"])
        .arg(env).args(["-u", "ST_AGENT"])
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg("--endpoint").arg(&socket)
        .args(["missions", "publish"]).arg(source)
        .args(["--as", "person/pat", "--no-gate-check"])
        .output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn client_without_runtime_dir_reaches_daemon_with_different_socket() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The service has no PATH; its login shell must provide one before socket discovery.
    let profile = format!("export PATH='{}'\n", std::env::var("PATH").unwrap());
    for name in [".bash_profile", ".zprofile", ".zshrc"] {
        std::fs::write(home.join(name), &profile).unwrap();
    }
    let state_home = root.path().join("state").join("long".repeat(26));
    let (_service, socket) = start_service(root.path(), &home, &state_home);
    let state_socket = state_home.join("st3/run/st3.sock");
    assert!(state_socket.as_os_str().len() > 108);
    assert_eq!(std::fs::read_link(&state_socket).unwrap(), socket);

    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", &state_home)
        .env("ST3_DAEMON_WAIT", "0")
        .args(["agents", "ls"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bare_service_environment_loads_shell_path_and_rechecks_credentials() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = home.join("orchid-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let profile = format!(
        "export PATH='{}:{}'\nexport ORCHID_CONTROL=from-shell\n",
        bin.display(),
        std::env::var("PATH").unwrap()
    );
    for name in [".bash_profile", ".zprofile", ".zshrc"] {
        std::fs::write(home.join(name), &profile).unwrap();
    }
    executable(
        &bin.join("pty"),
        "#!/bin/sh\n[ \"$ORCHID_CONTROL\" = from-shell ] || exit 9\nprintf '[]\\n'\n",
    );
    executable(&bin.join("gh"), "#!/bin/sh\nexit 1\n");
    let (_service, socket) = start_service(root.path(), &home, &root.path().join("state"));
    let doctor = || doctor_report(&home, &socket);
    let report = doctor();
    let checks = report["checks"].as_array().unwrap();
    let environment = checks
        .iter()
        .find(|check| check["name"] == "daemon-environment")
        .unwrap();
    assert_eq!(environment["status"], "pass");
    assert!(
        environment["message"]
            .as_str()
            .unwrap()
            .contains(bin.to_str().unwrap())
    );
    let pty = checks
        .iter()
        .find(|check| check["name"] == "pty-runtime")
        .unwrap();
    assert_eq!(pty["status"], "pass", "{pty}");
    let auth = checks
        .iter()
        .find(|check| check["name"] == "github-observer-auth")
        .unwrap();
    assert_eq!(auth["status"], "warn");
    assert!(auth["message"].as_str().unwrap().contains("gh auth login"));
    executable(
        &bin.join("gh"),
        "#!/bin/sh\nprintf 'orchid-test-credential\\n'\n",
    );
    let report = doctor();
    let auth = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "github-observer-auth")
        .unwrap();
    assert_eq!(auth["status"], "pass");
    assert!(!report.to_string().contains("orchid-test-credential"));
}

#[test]
fn doctor_reports_missing_build_tools_and_whether_a_small_crate_links() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = home.join("orchid-bin");
    std::fs::create_dir_all(&bin).unwrap();
    // The login PATH holds only these tools, so the report names exactly what is absent. The
    // shell needs `env` to export its environment.
    std::os::unix::fs::symlink(
        st_runtime::resolve_executable("env", &std::env::vars().collect()).unwrap(),
        bin.join("env"),
    )
    .unwrap();
    let profile = format!("export PATH='{}'\n", bin.display());
    for name in [".bash_profile", ".zprofile", ".zshrc"] {
        std::fs::write(home.join(name), &profile).unwrap();
    }
    executable(&bin.join("pty"), "#!/bin/sh\nprintf '[]\\n'\n");
    for tool in ["cargo", "rustc", "mold", "sccache", "gh", "git"] {
        executable(&bin.join(tool), "#!/bin/sh\nexit 0\n");
    }
    let (_service, socket) = start_service(root.path(), &home, &root.path().join("state"));
    let build_tools = || {
        let report = doctor_report(&home, &socket);
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "build-tools")
            .unwrap()
            .clone();
        (
            check["status"].as_str().unwrap().to_owned(),
            check["message"].as_str().unwrap().to_owned(),
        )
    };

    let (status, message) = build_tools();
    assert_eq!(status, "warn", "{message}");
    assert!(
        message.contains("missing from the login PATH: nix;"),
        "{message}"
    );
    assert!(!message.contains("did not link"), "{message}");

    executable(&bin.join("nix"), "#!/bin/sh\nexit 0\n");
    let (status, message) = build_tools();
    assert_eq!(status, "pass", "{message}");
    assert!(message.contains("a small crate links"), "{message}");

    executable(
        &bin.join("cargo"),
        "#!/bin/sh\necho 'error: linker `mold` not found' >&2\nexit 101\n",
    );
    let (status, message) = build_tools();
    assert_eq!(status, "warn", "{message}");
    assert!(
        message.contains("a small crate did not link")
            && message.contains("linker `mold` not found"),
        "{message}"
    );
}

/// A deploy restarts the daemon while the machine is busy building, and a login shell that takes
/// seconds when idle can take minutes then. The first capture here overruns its ten seconds; the
/// daemon must retry with more patience and start, not exit.
#[test]
fn a_login_shell_too_slow_for_the_first_capture_still_lets_the_daemon_start() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = home.join("orchid-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let profile = format!(
        "export PATH='{}:{}'\nexport ORCHID_CONTROL=from-shell\n",
        bin.display(),
        std::env::var("PATH").unwrap()
    );
    for name in [".bash_profile", ".zprofile", ".zshrc"] {
        std::fs::write(home.join(name), &profile).unwrap();
    }
    executable(&bin.join("pty"), "#!/bin/sh\nprintf '[]\\n'\n");
    executable(&bin.join("gh"), "#!/bin/sh\nexit 1\n");
    // The controlled fixture shell reads .bash_profile, independent of the account shell.
    // Its first run exceeds the capture allowance; subsequent runs finish immediately.
    let marker = root.path().join("slow-shell-ran");
    let slow = format!(
        "[ -e '{marker}' ] || {{ : > '{marker}'; sleep 120; }}\n",
        marker = marker.display()
    );
    for name in [".bash_profile", ".zprofile", ".zshrc"] {
        let current = std::fs::read_to_string(home.join(name)).unwrap();
        std::fs::write(home.join(name), format!("{slow}{current}")).unwrap();
    }
    std::fs::create_dir_all(root.path().join("config/fish")).unwrap();
    std::fs::write(
        root.path().join("config/fish/config.fish"),
        format!(
            "if not test -e '{marker}'\n  touch '{marker}'\n  sleep 120\nend\n",
            marker = marker.display()
        ),
    )
    .unwrap();
    let started = Instant::now();
    let (_service, socket) = start_service(root.path(), &home, &root.path().join("state"));
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "the first capture should have waited out its allowance: elapsed={:?}",
        started.elapsed()
    );
    let log = std::fs::read_to_string(root.path().join("daemon.log")).unwrap();
    assert!(log.contains("the login shell is slow to start"), "{log}");
    let report = doctor_report(&home, &socket);
    let environment = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "daemon-environment")
        .unwrap();
    assert_eq!(environment["status"], "pass", "{environment}");
}
