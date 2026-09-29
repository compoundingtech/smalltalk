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

fn start_service(root: &Path, home: &Path) -> (Service, std::path::PathBuf) {
    let socket = root.join("daemon.sock");
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    let binary = assert_cmd::cargo::cargo_bin!("st3");
    let shell = st_runtime::resolve_executable("bash", &std::env::vars().collect()).unwrap();
    let mut service = Service(
        Command::new(binary)
            .env_clear()
            .env("HOME", home)
            .env("SHELL", &shell)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
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
    let deadline = Instant::now() + Duration::from_secs(30);
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
    let output = Command::new(assert_cmd::cargo::cargo_bin!("st3"))
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
    let (_service, socket) = start_service(root.path(), &home);
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
    let (_service, socket) = start_service(root.path(), &home);
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
