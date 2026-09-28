#![cfg(unix)]
//! A service manager supplies HOME and state locations, but no PATH or credentials.
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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
    let socket = root.path().join("daemon.sock");
    let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
    let binary = assert_cmd::cargo::cargo_bin!("st3");
    let shell = st_runtime::resolve_executable("bash", &std::env::vars().collect()).unwrap();
    let mut service = Service(
        Command::new(binary)
            .env_clear()
            .env("HOME", &home)
            .env("SHELL", &shell)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .current_dir(root.path())
            .args(["up", "--node", "orchid"])
            .arg("--socket")
            .arg(&socket)
            .arg("--client-gateway-socket")
            .arg(root.path().join("client.sock"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            service.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(root.path().join("daemon.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "isolated service did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let doctor = || {
        let output = Command::new(binary)
            .env_clear()
            .env("HOME", &home)
            .arg("--endpoint")
            .arg(&socket)
            .args(["--json", "doctor"])
            .output()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
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
