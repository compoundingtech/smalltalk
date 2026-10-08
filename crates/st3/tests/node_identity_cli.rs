#![cfg(unix)]
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(root: &Path, node: &str, socket: &Path) -> std::process::Command {
    let mut command = st3::test_support::command(test_env!("CARGO_BIN_EXE_st3-fixture"));
    command
        .env_clear()
        .env("HOME", root.join("home"))
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TOKIO_WORKER_THREADS", "4")
        .current_dir(root)
        .args(["up", "--node", node])
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--socket")
        .arg(socket)
        .arg("--client-gateway-socket")
        .arg(root.join("client.sock"))
        .arg("--pty-root")
        .arg(root.join("pty"))
        .stdin(Stdio::null());
    command
}

fn start(root: &Path, node: &str, socket: &Path) -> Daemon {
    let path = root.join(format!("{node}.log"));
    let log = std::fs::File::create(&path).unwrap();
    let mut child = Daemon(
        command(root, node, socket)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = st3::client::Client::unix(socket.to_path_buf());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&path).unwrap()
        );
        if let Ok(health) = runtime.block_on(client.get::<Value>("/v1/health")) {
            assert_eq!(health["node"], "orchid", "{health}");
            return child;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            std::fs::read_to_string(&path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn daemon_cli_retains_same_state_identity_and_rejects_a_second_socket_owner() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    for directory in ["home", "runtime"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    std::fs::write(
        root.path().join("home/.bash_profile"),
        format!("export PATH='{}'\n", std::env::var("PATH").unwrap()),
    )
    .unwrap();
    let socket: PathBuf = root.path().join("api.sock");
    let daemon = start(root.path(), "orchid", &socket);
    let duplicate = command(
        root.path(),
        "orchid-laptop",
        &root.path().join("other.sock"),
    )
    .output()
    .unwrap();
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr)
            .contains("another daemon owns the state directory"),
        "{duplicate:?}"
    );
    drop(daemon);
    for name in ["orchid-laptop", "orchid"] {
        let daemon = start(root.path(), name, &socket);
        let pin: String = serde_json::from_slice(
            &std::fs::read(root.path().join("state/node-identity.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(pin, "orchid");
        drop(daemon);
    }
}
