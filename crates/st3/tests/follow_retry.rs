//! The real CLI retries a lost answer without replaying delivered history or snapshots.
//! Each fake daemon owns a temporary Unix socket. A timeout is injected by holding its
//! connection until the client's actual request deadline closes it, without timing a sleep.
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
    Timeout,
    Disconnect,
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
                Reply::Timeout => {
                    let mut rest = Vec::new();
                    caller.read_to_end(&mut rest).await.unwrap();
                    assert!(rest.is_empty());
                    continue;
                }
                Reply::Disconnect => continue,
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
        st3::test_support::async_command(assert_cmd::cargo::cargo_bin!("st3-fixture"));
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

fn claims_path(after: u64) -> String {
    format!("/v1/claims?subject=host%2Ffollow-fixture&after_index={after}&order=asc&limit=1")
}

fn events_path(after: u64) -> String {
    format!("/v1/events?after={after}&subject=host%2Ffollow-fixture")
}

fn event(index: u64) -> Value {
    json!({"store_index": index, "kind": "transport.observed", "subject": SUBJECT, "body": {}})
}

fn claim(index: u64) -> Value {
    json!({
        "id": format!("claim/{index}"), "store_index": index, "batch_id": "batch/fixture",
        "subject": SUBJECT, "kind": "transport.observed", "origin": "fixture", "actor": null,
        "body": {"status": format!("event-{index}")}, "predecessors": [], "accepted_at_unix_ms": 0,
    })
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
async fn trace_follow_retries_timeout_after_last_delivered_index_once() {
    let initial = "/v1/claims?limit=2&order=asc&subject=host%2Ffollow-fixture&after_index=10";
    let output = scripted_cli(
        &[
            "--json",
            "trace",
            "show",
            SUBJECT,
            "--after-index",
            "10",
            "--limit",
            "2",
            "--follow",
        ],
        vec![
            (
                initial.into(),
                Reply::Value(json!({"claims": [], "next_cursor": null})),
            ),
            (events_path(10), Reply::Value(json!([event(11), event(12)]))),
            (events_path(12), Reply::Timeout),
            (events_path(12), Reply::Disconnect),
            (events_path(12), Reply::Value(json!([event(13), event(14)]))),
            (events_path(14), Reply::Error(404)),
        ],
    )
    .await;
    assert_refusal(&output, 1);
    assert!(stderr(&output).contains("request and response limit of 15000 ms"));
    let delivered: Vec<u64> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["store_index"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(delivered, [11, 12, 13, 14]);
}

#[tokio::test]
async fn trace_follow_retries_claim_details_before_advancing_index() {
    let initial = "/v1/claims?limit=2&order=asc&subject=host%2Ffollow-fixture&after_index=10";
    let output = scripted_cli(
        &[
            "trace",
            "show",
            SUBJECT,
            "--after-index",
            "10",
            "--limit",
            "2",
            "--follow",
        ],
        vec![
            (
                initial.into(),
                Reply::Value(json!({"claims": [], "next_cursor": null})),
            ),
            (events_path(10), Reply::Value(json!([event(11), event(12)]))),
            (
                claims_path(10),
                Reply::Value(json!({"claims": [claim(11)], "next_cursor": null})),
            ),
            (claims_path(11), Reply::Timeout),
            (
                claims_path(11),
                Reply::Value(json!({"claims": [claim(12)], "next_cursor": null})),
            ),
            (events_path(12), Reply::Error(422)),
        ],
    )
    .await;
    assert_refusal(&output, 1);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("event-11").count(), 1, "{stdout}");
    assert_eq!(stdout.matches("event-12").count(), 1, "{stdout}");
}

async fn mission_timeout(interrupt_tree: bool) {
    let before = run("running", "normal");
    let after = run("running", "recovered");
    let finished = run("completed", "normal");
    let mut script = vec![
        (RUN_PATH.into(), Reply::Value(before.clone())),
        (TREE_PATH.into(), Reply::Value(json!([before]))),
    ];
    if interrupt_tree {
        script.push((RUN_PATH.into(), Reply::Value(after.clone())));
        script.push((TREE_PATH.into(), Reply::Timeout));
    } else {
        script.push((RUN_PATH.into(), Reply::Timeout));
        script.push((RUN_PATH.into(), Reply::Value(after.clone())));
    }
    script.extend([
        (TREE_PATH.into(), Reply::Value(json!([after]))),
        (RUN_PATH.into(), Reply::Value(finished.clone())),
        (TREE_PATH.into(), Reply::Value(json!([finished]))),
    ]);
    let output = scripted_cli(&["missions", "show", RUN, "--follow"], script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output).matches("follow gap:").count(),
        1,
        "{}",
        stderr(&output)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let states: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("STATE"))
        .collect();
    assert_eq!(
        states,
        [
            "STATE     running · normal",
            "STATE     running · recovered",
            "STATE     completed · normal"
        ]
    );
}

#[tokio::test]
async fn mission_follow_retries_run_timeout_without_repeating_snapshots() {
    mission_timeout(false).await;
}

#[tokio::test]
async fn mission_follow_retries_tree_timeout_without_repeating_snapshots() {
    mission_timeout(true).await;
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
