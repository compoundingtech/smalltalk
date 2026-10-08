#![cfg(target_os = "linux")]
//! A real, isolated daemon pauses inside canonical replay. Connections queue on both listeners,
//! but no response or serving readiness is published before migrations and replay complete.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::model::ClaimInput;
use st3::store::Store;

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(root: &Path) -> Command {
    let mut command = st3::test_support::command(test_env!("CARGO_BIN_EXE_st3-fixture"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("XDG_STATE_HOME", root.join("home/.local/state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("ST3_ENDPOINT", root.join("run/api.sock"))
        .current_dir(root)
        .stdin(Stdio::null());
    command
}

#[test]
fn replay_is_visible_before_the_api_serves_and_stale_files_are_ignored() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    for directory in ["home/.config/st3", "run", "state", "barrier"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let socket = root.join("run/api.sock");
    let database = root.join("state/claims.sqlite3");
    {
        let store = Store::open(&database, "studio").unwrap();
        for state in ["ready", "idle", "ready"] {
            store
                .append_claim(&ClaimInput {
                    subject: "agent/studio.test".into(),
                    kind: "harness.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("state".into(), json!(state))]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        store.project_replication_backlog().unwrap();
        // Model an already-started store: the one-time canonical settlement otherwise replays
        // and repairs graph health before the backlog projector sees the forced fallback.
        store.settle_runs_for_canonical_replay().unwrap();
        assert!(store.index().unwrap() >= 3);
    }
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("DELETE FROM projection_health WHERE aggregate='graph'", [])
        .unwrap();
    std::fs::write(root.join("home/.config/st3/config.toml"), format!(
        "node = \"studio\"\nstate_dir = {:?}\nsocket = {:?}\nclient_gateway_socket = {:?}\npty_root = {:?}\n",
        root.join("state"), socket, root.join("run/client.sock"), root.join("pty")
    )).unwrap();
    let log_path = root.join("daemon.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut daemon = Daemon(
        command(root)
            .arg("up")
            .env("ST3_TEST_STARTUP_REPLAY_BARRIER", root.join("barrier"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("barrier/entered").exists() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "replay not reached: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let readiness = st3::startup::read(&socket).unwrap();
    assert_eq!(readiness.status, "starting");
    assert_eq!(readiness.phase, "full-replay/base-claims");
    assert_eq!(readiness.frontier, Some(0));
    assert!(readiness.target.unwrap() >= 3);
    assert_eq!(readiness.processed, Some(0));
    assert!(readiness.total.unwrap() >= 3);
    let early_log = std::fs::read_to_string(&log_path).unwrap();
    assert!(early_log.contains("st: projection full replay phase=startup/project-replication-backlog reason=missing-health"), "fallback must be logged before the replay finishes: {early_log}");
    // Both HTTP requests arrive while replay holds its transaction. Binding is not serving:
    // neither socket may respond from an incomplete or silently stale projection.
    let mut pending = [
        std::os::unix::net::UnixStream::connect(&socket).unwrap(),
        std::os::unix::net::UnixStream::connect(root.join("run/client.sock")).unwrap(),
    ];
    for stream in &mut pending {
        stream
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.set_nonblocking(true).unwrap();
        let error = stream.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = root.join("status-bin");
        std::fs::create_dir_all(&directory).unwrap();
        let systemctl = directory.join("systemctl");
        std::fs::write(&systemctl, "#!/bin/sh\nprintf '%s\\n' 'LoadState=loaded' 'ActiveState=active' 'SubState=running'\n").unwrap();
        std::fs::set_permissions(systemctl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let service = command(root)
            .env(
                "PATH",
                format!("{}:{}", directory.display(), std::env::var("PATH").unwrap()),
            )
            .args(["service", "status", "--json"])
            .output()
            .unwrap();
        assert!(
            service.status.success(),
            "{}",
            String::from_utf8_lossy(&service.stderr)
        );
        let service: Value = serde_json::from_slice(&service.stdout).unwrap();
        assert_eq!(service["services"][0]["running"], true);
        assert!(
            service["services"][0]["state"]
                .as_str()
                .unwrap()
                .contains("daemon starting")
        );
        assert!(
            service["services"][0]["state"]
                .as_str()
                .unwrap()
                .contains("full-replay/base-claims")
        );
    }
    std::fs::write(root.join("barrier/release"), b"resume").unwrap();
    while st3::startup::read(&socket).is_none_or(|state| state.status != "serving") {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "API not ready: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    for mut stream in pending {
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["value"]["status"], "ready");
        assert!(body["value"]["store_index"].as_u64().unwrap() >= 3);
    }
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    assert!(std::os::unix::net::UnixStream::connect(root.join("run/client.sock")).is_ok());
    let readiness = st3::startup::read(&socket).unwrap();
    assert_eq!(readiness.phase, "ready");
    assert_eq!(readiness.frontier, readiness.target);
    let doctor = command(root).args(["doctor", "--json"]).output().unwrap();
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["startup"]["status"], "serving");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let fallbacks = log
        .lines()
        .filter(|line| line.starts_with("st: projection full replay"))
        .collect::<Vec<_>>();
    assert_eq!(fallbacks.len(), 1, "{log}");
    assert!(
        fallbacks[0].contains("phase=startup/project-replication-backlog reason=missing-health")
    );
    #[cfg(target_os = "linux")]
    {
        let service = command(root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("status-bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .args(["service", "status", "--json"])
            .output()
            .unwrap();
        let service: Value = serde_json::from_slice(&service.stdout).unwrap();
        assert!(
            service["services"][0]["state"]
                .as_str()
                .unwrap()
                .contains("daemon serving")
        );
    }
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    assert!(socket.with_file_name("api.sock.readiness.json").exists());
    assert!(
        st3::startup::read(&socket).is_none(),
        "a killed daemon's file is not readiness"
    );
}
