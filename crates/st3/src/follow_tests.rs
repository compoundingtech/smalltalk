//! Follow loops use a scripted client and Tokio's injected clock. This tests actual
//! request deadlines and backoff without sleeping through 15 or 35 seconds per case.
use super::*;
use st3::client::FollowTestReply as Reply;

const SUBJECT: &str = "host/follow-fixture";
const RUN: &str = "mission-run/example/follow-fixture";
const RUN_PATH: &str = "/v1/mission-runs/mission-run%2Fexample%2Ffollow-fixture";
const TREE_PATH: &str = "/v1/mission-runs?root=mission-run%2Fexample%2Ffollow-fixture";

struct FollowOutput {
    result: Result<()>,
    stdout: Vec<u8>,
    requests: Vec<(String, tokio::time::Instant)>,
    messages: Vec<String>,
}

async fn scripted_cli(args: &[&str], script: Vec<(String, Reply)>) -> FollowOutput {
    let cli = Cli::try_parse_from(std::iter::once("st").chain(args.iter().copied())).unwrap();
    let client = Client::scripted_follow_test(script).with_follow_retry();
    let mut stdout = Vec::new();
    let result = match cli.command {
        Command::Trace {
            command: TraceCommand::Show(args),
        } => run_trace_to(&client, args, cli.json, &mut stdout).await,
        Command::Missions {
            command: MissionViewCommand::Show(args),
        } => {
            let run = client
                .get(&format!(
                    "/v1/mission-runs/{}",
                    urlencoding::encode(&args.mission_or_run)
                ))
                .await
                .unwrap();
            follow_mission_run_to(
                &client,
                run,
                cli.json,
                false,
                OutputStyle::plain(),
                &mut stdout,
            )
            .await
        }
        _ => panic!("expected a follow viewer"),
    };
    let (requests, messages) = client.follow_test_result();
    FollowOutput {
        result,
        stdout,
        requests,
        messages,
    }
}

fn claims_path(after: u64) -> String {
    format!("/v1/claims?subject=host%2Ffollow-fixture&after_index={after}&order=asc&limit=1")
}

fn events_path(after: u64) -> String {
    format!("/v1/events?after={after}&wait=true&timeout_ms=30000&subject=host%2Ffollow-fixture")
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

fn stderr(output: &FollowOutput) -> String {
    let error = output
        .result
        .as_ref()
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    format!("{}\n{error}", output.messages.join("\n"))
}

fn assert_refusal(output: &FollowOutput, gaps: usize) {
    assert!(output.result.is_err(), "the viewer ignored the refusal");
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
    assert_eq!(
        stderr(output).matches("follow recovered after").count(),
        gaps
    );
}

#[tokio::test(start_paused = true)]
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
    assert!(stderr(&output).contains("request and response limit of 35000 ms"));
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
    let delay = output.requests[3].1 - output.requests[2].1 - Duration::from_secs(35);
    assert!((Duration::from_secs(5)..=Duration::from_secs(15)).contains(&delay));
}

#[tokio::test(start_paused = true)]
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
            (events_path(10), Reply::Timeout),
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
    assert!(output.result.is_ok(), "{}", stderr(&output));
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
    assert_eq!(
        output.requests[2].1 - output.requests[1].1,
        Duration::from_millis(250)
    );
    assert_eq!(
        output.requests[5].1 - output.requests[4].1,
        Duration::from_millis(250)
    );
    let failed = if interrupt_tree { 3 } else { 2 };
    let delay = output.requests[failed + 1].1 - output.requests[failed].1 - Duration::from_secs(15);
    assert!((Duration::from_secs(5)..=Duration::from_secs(15)).contains(&delay));
    assert_eq!(
        output
            .messages
            .iter()
            .filter(|m| m.contains("follow recovered after"))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn idle_trace_long_polls_thirty_seconds_without_deadlines_or_gaps() {
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
            (
                events_path(10),
                Reply::Delayed(json!([]), Duration::from_secs(30)),
            ),
            (
                events_path(10),
                Reply::Delayed(json!([event(11)]), Duration::from_secs(30)),
            ),
            (events_path(11), Reply::Error(404)),
        ],
    )
    .await;
    assert_refusal(&output, 0);
    assert_eq!(
        output.requests[2].1 - output.requests[1].1,
        Duration::from_secs(30)
    );
    assert_eq!(
        output.requests[3].1 - output.requests[2].1,
        Duration::from_secs(30)
    );
    let delivered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(delivered["store_index"], 11);
}

#[tokio::test(start_paused = true)]
async fn deadline_backoff_survives_recovered_pages_and_caps_at_sixty_seconds() {
    let mut script = vec![(
        "/v1/claims?limit=100&order=asc&subject=host%2Ffollow-fixture&after_index=10".into(),
        Reply::Value(json!({"claims": [], "next_cursor": null})),
    )];
    for index in 10..16 {
        script.push((events_path(index), Reply::Timeout));
        script.push((events_path(index), Reply::Value(json!([event(index + 1)]))));
    }
    script.push((events_path(16), Reply::Error(404)));
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
        script,
    )
    .await;
    assert_refusal(&output, 6);
    for (attempt, base) in [10, 20, 40, 60, 60, 60].into_iter().enumerate() {
        let failed = attempt * 2 + 1;
        let delay =
            output.requests[failed + 1].1 - output.requests[failed].1 - Duration::from_secs(35);
        assert!(
            delay >= Duration::from_secs(base / 2),
            "attempt {attempt}: {delay:?}"
        );
        assert!(
            delay <= Duration::from_secs((base * 3 / 2).min(60)),
            "attempt {attempt}: {delay:?}"
        );
    }
    let delivered: Vec<u64> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["store_index"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(delivered, [11, 12, 13, 14, 15, 16]);
}

#[tokio::test(start_paused = true)]
async fn mission_follow_retries_run_timeout_without_repeating_snapshots() {
    mission_timeout(false).await;
}

#[tokio::test(start_paused = true)]
async fn mission_follow_retries_tree_timeout_without_repeating_snapshots() {
    mission_timeout(true).await;
}
