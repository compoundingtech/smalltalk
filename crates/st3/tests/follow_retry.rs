//! Real CLI processes exit 2 immediately on non-transient follow errors.
//! Timeout, delivery, idle-poll and backoff proofs use an injected clock in follow_tests.rs.
//! Each fake daemon here owns a temporary Unix socket.
use std::process::Output;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixListener;

const SUBJECT: &str = "host/follow-fixture";
const RUN: &str = "mission-run/example/follow-fixture";
const RUN_PATH: &str = "/v1/mission-runs/mission-run%2Fexample%2Ffollow-fixture";
const TREE_PATH: &str = "/v1/mission-runs?root=mission-run%2Fexample%2Ffollow-fixture";

enum Reply {
    Value(Value),
    Error(u16),
}

async fn scripted_cli(args: &[&str], script: Vec<(String, Reply)>) -> Output {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        for (expected, reply) in script {
            let (mut caller, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(caller.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert_eq!(
                request.lines().next().unwrap(),
                format!("GET {expected} HTTP/1.1")
            );
            let (status, body) = match reply {
                Reply::Value(value) => (
                    200,
                    json!({
                        "api_version": "st3.v1", "request_id": "follow-fixture",
                        "snapshot_host": "fixture", "store_index": 0, "value": value,
                    }),
                ),
                Reply::Error(status) => (
                    status,
                    json!({
                        "code": "fixture-refusal", "message": "fixture read refused",
                    }),
                ),
            };
            let body = serde_json::to_vec(&body).unwrap();
            caller.write_all(format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ).as_bytes()).await.unwrap();
            caller.write_all(&body).await.unwrap();
        }
    });
    let mut command =
        st3::test_support::async_command(test_bin!("st3-fixture"));
    command
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .args(["--endpoint", socket.to_str().unwrap(), "--daemon-wait", "0"])
        .args(args);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("follow did not retry or stop at the scripted error")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("the viewer exited before consuming the script")
        .unwrap();
    output
}

fn events_path(after: u64) -> String {
    format!("/v1/events?after={after}&wait=true&timeout_ms=30000&subject=host%2Ffollow-fixture")
}

fn run(status: &str, phase: &str) -> Value {
    json!({
        "subject": RUN, "id": "example/follow-fixture", "mission": "mission/example/follow-fixture",
        "generation": "run-generation/fixture", "initial_revision": "fixture", "revision": "fixture",
        "root_revision": "fixture", "root_mission_run": RUN, "workspace": "/tmp/follow-fixture",
        "requester": "person/avery", "mode": "run", "status": status, "phase": phase,
        "created_at_unix_ms": 0, "updated_at_unix_ms": 0, "steps": [],
    })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_refusal(output: &Output, gaps: usize) {
    assert_eq!(output.status.code(), Some(2), "{}", stderr(output));
    assert!(
        stderr(output).contains("fixture read refused"),
        "{}",
        stderr(output)
    );
    assert_eq!(
        stderr(output).matches("follow gap:").count(),
        gaps,
        "{}",
        stderr(output)
    );
}

#[tokio::test]
async fn follow_auth_not_found_and_validation_errors_stop_at_once() {
    for status in [401, 404, 422] {
        let output = scripted_cli(
            &[
                "--json",
                "trace",
                "show",
                SUBJECT,
                "--after-index",
                "10",
                "--follow",
            ],
            vec![
                (
                    "/v1/claims?limit=100&order=asc&subject=host%2Ffollow-fixture&after_index=10"
                        .into(),
                    Reply::Value(json!({"claims": [], "next_cursor": null})),
                ),
                (events_path(10), Reply::Error(status)),
            ],
        )
        .await;
        assert_refusal(&output, 0);
        for path in [RUN_PATH, TREE_PATH] {
            let mut script = vec![(RUN_PATH.into(), Reply::Value(run("running", "normal")))];
            if path == RUN_PATH {
                script.push((
                    TREE_PATH.into(),
                    Reply::Value(json!([run("running", "normal")])),
                ));
            }
            script.push((path.into(), Reply::Error(status)));
            let output = scripted_cli(&["missions", "show", RUN, "--follow"], script).await;
            assert_refusal(&output, 0);
        }
    }
}
