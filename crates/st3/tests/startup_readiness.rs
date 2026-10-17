#![cfg(target_os = "linux")]
//! A real, isolated daemon pauses inside canonical replay. Observation must work while SQLite
//! holds its write transaction, then advance to serving only after both listeners bind.

use std::collections::BTreeMap;
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
fn private_runtime_socket_refuses_shared_gateway_before_creating_state() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("run")).unwrap();
    let alias = root.path().join("runtime-alias");
    std::os::unix::fs::symlink(root.path().join("run"), &alias).unwrap();
    for directory in [root.path().join("run"), alias] {
        let output = command(root.path())
            .arg("up")
            .arg("--socket")
            .arg(directory.join("private-st.sock"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("pass --client-gateway-socket; the derived private gateway"),
            "{error}"
        );
        assert!(
            error.contains("equals the shared default gateway"),
            "{error}"
        );
        assert!(!root.path().join("home/.local/state").exists());
        assert!(!root.path().join("run/private-st.sock").exists());
        assert!(!root.path().join("run/st3-client.sock").exists());
    }
}

#[test]
fn private_runtime_socket_refuses_shared_gateway_before_creating_state_with_missing_runtime_directory()
{
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let output = command(root.path())
        .env("XDG_RUNTIME_DIR", real.join("run"))
        .arg("up")
        .arg("--socket")
        .arg(alias.join("run/private-st.sock"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("pass --client-gateway-socket; the derived private gateway"),
        "{error}"
    );
    assert!(
        error.contains("equals the shared default gateway"),
        "{error}"
    );
    assert!(!root.path().join("home/.local/state").exists());
    assert!(!real.join("run").exists());
}

#[test]
fn private_runtime_socket_refuses_shared_gateway_before_creating_state_with_missing_parent_traversal()
{
    if st3::test_support::supervise_test() {
        return;
    }
    for above_ancestor in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let physical_ancestor = if above_ancestor {
            real.join("deep")
        } else {
            real.clone()
        };
        std::fs::create_dir_all(&physical_ancestor).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&physical_ancestor, &alias).unwrap();
        let suffix = if above_ancestor {
            "./new/../../run/private-st.sock"
        } else {
            "./new/../run/private-st.sock"
        };
        let output = command(root.path())
            .env("XDG_RUNTIME_DIR", real.join("run"))
            .arg("up")
            .arg("--socket")
            .arg(alias.join(suffix))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("pass --client-gateway-socket; the derived private gateway"),
            "{error}"
        );
        assert!(
            error.contains("equals the shared default gateway"),
            "{error}"
        );
        assert!(!root.path().join("home/.local/state").exists());
        assert!(!physical_ancestor.join("new").exists());
        assert!(!real.join("run").exists());
    }
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
    // This fixture contains only harness observations. Base progress counts bodies with
    // a base projection, while target still names the full uncommitted source frontier.
    assert_eq!(readiness.total, Some(0));
    let early_log = std::fs::read_to_string(&log_path).unwrap();
    assert!(early_log.contains("st: projection full replay phase=startup/project-replication-backlog reason=missing-health"), "fallback must be logged before the replay finishes: {early_log}");
    assert!(!early_log.contains("st3: startup phases "), "{early_log}");
    // Both addresses exist early, but neither is listening or queueing a request
    // while projection is uncommitted. Keep the main refusal/phase contract.
    for path in [
        &socket,
        &root.join("run/client.sock"),
        &root.join("state/run/st3.sock"),
    ] {
        use std::os::unix::fs::FileTypeExt as _;
        assert!(std::fs::metadata(path).unwrap().file_type().is_socket());
        let error = std::os::unix::net::UnixStream::connect(path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
    }
    let started = Instant::now();
    let doctor = command(root).args(["doctor", "--json"]).output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!doctor.status.success());
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["startup"]["phase"], "full-replay/base-claims");
    assert_eq!(report["startup"]["status"], "starting");
    assert!(!String::from_utf8_lossy(&doctor.stderr).contains("st setup"), "{doctor:?}");
    let started = Instant::now();
    let ordinary = command(root).args(["agents", "ls"]).output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "CLI waited through replay"
    );
    assert!(!ordinary.status.success());
    assert!(String::from_utf8_lossy(&ordinary.stderr).contains("daemon starting"));
    assert!(String::from_utf8_lossy(&ordinary.stderr).contains("full-replay/base-claims"));
    let started = Instant::now();
    let generated = command(root)
        .args(["work", "ls", "--as", "agent/replay-readiness-probe"])
        .output()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "generated CLI waited through replay"
    );
    assert!(!generated.status.success());
    assert!(String::from_utf8_lossy(&generated.stderr).contains("full-replay/base-claims"));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = root.join("status-bin");
        std::fs::create_dir_all(&directory).unwrap();
        let systemctl = directory.join("systemctl");
        std::fs::write(&systemctl, r#"#!/bin/sh
if [ "$2" != show ]; then exit 9; fi
if [ "$3" = st3.service ]; then
    printf '%s\n' 'LoadState=loaded' 'ActiveState=active' 'SubState=running'
else
    printf '%s\n' 'LoadState=not-found' 'ActiveState=inactive' 'SubState=dead'
fi
"#).unwrap();
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
        assert_eq!(service["services"][0]["name"], "st3.service");
        assert_eq!(service["services"][1]["installed"], false);
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
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    assert!(std::os::unix::net::UnixStream::connect(root.join("run/client.sock")).is_ok());
    let readiness = st3::startup::read(&socket).unwrap();
    assert_eq!(readiness.phase, "ready");
    assert_eq!(readiness.frontier, readiness.target);
    let doctor = command(root).args(["doctor", "--json"]).output().unwrap();
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["startup"]["status"], "serving");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let timing_lines = log
        .lines()
        .filter_map(|line| line.strip_prefix("st3: startup phases "))
        .collect::<Vec<_>>();
    assert_eq!(timing_lines.len(), 1, "{log}");
    let timings: Value = serde_json::from_str(timing_lines[0]).unwrap();
    let keys = [
        "configuration_ms", "install_hooks_ms", "open_store_ms", "judge_claims_ms",
        "validate_replication_backlog_ms", "apply_replication_repairs_ms",
        "settlement_projection_ms", "runtime_prelude_ms", "login_environment_ms",
        "isolation_pty_ms", "recorder_ms", "app_state_ms", "reconciler_ms",
        "background_tasks_ms", "bind_ms", "total_ms",
    ];
    assert_eq!(timings.as_object().unwrap().len(), keys.len());
    let total_ms = timings["total_ms"].as_f64().unwrap();
    let mut phase_sum_ms = 0.0;
    for key in keys {
        let milliseconds = timings[key].as_f64().unwrap();
        assert!(milliseconds.is_finite() && milliseconds >= 0.0, "{key}: {timings}");
        assert!(milliseconds <= total_ms, "{key}: {timings}");
        if key != "total_ms" {
            phase_sum_ms += milliseconds;
        }
    }
    assert!((phase_sum_ms - total_ms).abs() < 0.001, "{timings}");
    eprintln!("st3: startup phases {}", timing_lines[0]);
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
