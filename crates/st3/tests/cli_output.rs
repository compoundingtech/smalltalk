#![cfg(unix)]

use std::os::fd::{FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use st3::api::AppState;
use st3::store::Store;
use tokio::sync::{Notify, oneshot, watch};

fn closed_pipe() -> Stdio {
    let mut descriptors = [-1; 2];
    // SAFETY: pipe writes exactly two descriptors into the live array.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: these fresh descriptors each have exactly one owner.
    let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    drop(reader);
    Stdio::from(writer)
}

async fn run(socket: Option<&Path>, args: &[&str], closed: bool) -> Output {
    let scratch = tempfile::tempdir().unwrap();
    let mut command = st3::test_support::async_command(test_bin!("st3-fixture"));
    command
        .env("NO_COLOR", "1")
        .env("TMPDIR", scratch.path())
        .kill_on_drop(true);
    if let Some(socket) = socket {
        command.arg("--endpoint").arg(socket);
    }
    command.args(args);
    if closed {
        command.stdout(closed_pipe());
    } else {
        command.stdout(Stdio::piped());
    }
    // Tokio's Command::output() replaces stdout, including the pipe being tested.
    let child = command.stderr(Stdio::piped()).spawn().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("CLI must stop when its reader closes")
        .unwrap()
}

fn assert_clean(output: &Output) {
    assert!(
        output.status.success(),
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
}

#[tokio::test]
async fn shared_cli_output_exits_cleanly_with_a_closed_pipe() {
    for args in [
        vec!["skill"],
        vec!["help", "--all"],
        vec!["--read-contract-inventory-json"],
        vec!["completions", "bash"],
        vec!["sekrets", "presets"],
    ] {
        assert_clean(&run(None, &args, true).await);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shared_cli_output_reports_errors_other_than_a_closed_pipe() {
    let scratch = tempfile::tempdir().unwrap();
    let output = st3::test_support::async_command(test_bin!("st3-fixture"))
        .env("TMPDIR", scratch.path())
        .arg("skill")
        .stdout(
            std::fs::File::options()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        )
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("No space left on device"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("panicked"), "{diagnostic}");
}

/// Fleet leave prints the replicated claim before its local membership cleanup and final
/// leave-cancel request. A truncated receipt must not skip either effect or mask its failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_pipe_preserves_later_mutations_and_their_failure_status() {
    for fail_cleanup in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("state");
        st3::fleet::join::found(
            &state_dir,
            "orchid",
            &st3::fleet::join::MemberSettings {
                mode: st3::config::FleetMode::DialOut,
                port: None,
                transports: Some(vec![]),
                advertise_loopback: false,
                fabric: None,
                tailscale: None,
            },
        )
        .unwrap();
        let left = state_dir.join("left-fleet.json");
        if fail_cleanup {
            std::fs::create_dir(&left).unwrap();
        }
        let config = root.path().join("config/st3");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            format!(
                "node = \"orchid\"\nstate_dir = {}\nperson = \"person/avery\"\n",
                serde_json::to_string(&state_dir).unwrap(),
            ),
        )
        .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = cancelled.clone();
        let app = axum::Router::new().fallback(move |axum::extract::OriginalUri(uri): axum::extract::OriginalUri| {
            let observed = observed.clone();
            async move {
                let value = match uri.path() {
                    "/v1/client/machines" => json!({"items": []}),
                    "/v1/internal/fleet/leave/begin" => json!({}),
                    "/v1/replication/status" => json!({
                        "configured": true, "fleet_id": null,
                        "authority_digest": "same", "graph_digest": "same",
                        "received_envelopes": 0, "pending_records": 0, "valid_records": 0,
                        "unknown_records": 0, "invalid_records": 0, "repaired_records": 0,
                        "unhealthy_projections": 0,
                        "peers": [{"peer": "orchid-peer", "status": "reachable", "authority_digest": "same"}],
                    }),
                    "/v1/internal/fleet/leave/claim" => json!({
                        "id": "claim/leave", "store_index": 1, "batch_id": "batch/leave",
                        "subject": "fleet-member/orchid", "kind": "fleet.member-left", "origin": "orchid",
                        "actor": "person/avery", "body": {}, "predecessors": [], "accepted_at_unix_ms": 0,
                    }),
                    "/v1/internal/fleet/leave/cancel" => {
                        observed.store(true, Ordering::Relaxed);
                        json!({})
                    }
                    other => panic!("unexpected fleet request: {other}"),
                };
                axum::Json(json!({
                    "api_version": "st3.v1", "request_id": "cli-output",
                    "snapshot_host": "orchid", "store_index": 1, "value": value,
                }))
            }
        });
        let socket = root.path().join("st3.sock");
        let served = socket.clone();
        let (ready, started) = oneshot::channel();
        let server = tokio::spawn(async move {
            st3::api::serve_unix_with_ready(&served, app, || {
                ready.send(()).unwrap();
            })
            .await
        });
        started.await.unwrap();
        let child = st3::test_support::async_command(test_bin!("st3-fixture"))
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("TMPDIR", root.path())
            .args([
                "--endpoint",
                socket.to_str().unwrap(),
                "--daemon-wait",
                "0",
                "fleet",
                "leave",
                "--no-service",
                "--as",
                "person/avery",
            ])
            .stdout(closed_pipe())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
        if fail_cleanup {
            assert!(
                !output.status.success(),
                "a closed receipt must not hide a later cleanup failure"
            );
            assert!(st3::config::FleetFile::load(&state_dir).unwrap().is_some());
            assert!(!cancelled.load(Ordering::Relaxed));
        } else {
            assert_clean(&output);
            assert!(
                left.is_file(),
                "the leave record must be written after the truncated receipt"
            );
            assert!(
                st3::config::FleetFile::load(&state_dir).unwrap().is_none(),
                "membership cleanup must finish after the truncated receipt"
            );
            assert!(!st3::fleet::join::key_directory(&state_dir).exists());
            assert!(
                cancelled.load(Ordering::Relaxed),
                "the final leave-cancel mutation must run"
            );
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mission_start_and_read_output_exit_cleanly_with_a_closed_pipe() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("cli-output").unwrap());
    let intent = st3::graph::parse_intent(
        r#"version 2
mission "orchid" state="ready" {
  goal "Exercise CLI output."
  step "build" { assigned-to "person/avery" }
}
"#,
        store.origin(),
    )
    .unwrap();
    store.apply_internal(&intent, "cli-output-fixture").unwrap();
    let state = AppState {
        owning_person: None,
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "cli-output".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let socket = root.path().join("st3.sock");
    let served = socket.clone();
    let (ready, started) = oneshot::channel();
    let server = tokio::spawn(async move {
        st3::api::serve_unix_with_ready(&served, st3::api::router(state), || {
            ready.send(()).unwrap();
        })
        .await
    });
    started.await.unwrap();
    let output = run(
        Some(&socket),
        &[
            "missions",
            "start",
            "orchid",
            "--id",
            "closed",
            "--workspace",
            root.path().to_str().unwrap(),
            "--as",
            "person/avery",
        ],
        true,
    )
    .await;
    assert_clean(&output);
    // Starting succeeded before the output was truncated.
    assert!(
        store
            .mission_run("mission-run/orchid/closed")
            .unwrap()
            .is_some()
    );
    for args in [
        vec!["missions", "show", "mission-run/orchid/closed"],
        vec!["missions", "show", "mission-run/orchid/closed", "--json"],
        vec!["missions", "show", "mission-run/orchid/closed", "--follow"],
    ] {
        assert_clean(&run(Some(&socket), &args, true).await);
    }
    server.abort();
}
