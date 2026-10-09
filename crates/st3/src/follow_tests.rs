//! Follow loops use a scripted client and Tokio's injected clock. This tests actual
//! request deadlines and backoff without sleeping through 15 or 35 seconds per case.
use super::*;
use st3::client::FollowTestReply as Reply;

const SUBJECT: &str = "host/follow-fixture";
const RUN: &str = "mission-run/example/follow-fixture";
const RUN_PATH: &str = "/v1/mission-runs/mission-run%2Fexample%2Ffollow-fixture";
const TREE_PATH: &str = "/v1/mission-runs/tree?root=mission-run%2Fexample%2Ffollow-fixture&limit=50";

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
                args.limit,
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
    format!(
        "/v1/events/page?after={after}&wait=true&timeout_ms=30000&subject=host%2Ffollow-fixture"
    )
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

fn tree(run: Value, next_cursor: Option<&str>) -> Value {
    json!({
        "runs": [run],
        "next_cursor": next_cursor,
        "has_more": next_cursor.is_some(),
        "frontier": 0,
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
async fn default_trace_follow_starts_at_the_frontier_for_old_and_empty_history() {
    for history in [vec![], vec![claim(10)]] {
        let history_length = history.len();
        let output = scripted_cli(
            &["--json", "trace", "show", SUBJECT, "--follow"],
            vec![
                (
                    "/v1/claims?limit=100&order=desc&subject=host%2Ffollow-fixture".into(),
                    Reply::Value(json!({"claims":history,"next_cursor":null})),
                ),
                ("/v1/health".into(), Reply::Value(json!({"store_index":50}))),
                (
                    events_path(50),
                    Reply::Value(json!({"items":[event(51)],"next_after":51})),
                ),
                (events_path(51), Reply::Error(401)),
            ],
        )
        .await;
        assert_refusal(&output, 0);
        let rows = String::from_utf8(output.stdout).unwrap();
        assert_eq!(rows.lines().count(), history_length + 1);
        let last: Value = serde_json::from_str(rows.lines().last().unwrap()).unwrap();
        assert_eq!(last["store_index"], 51);
    }
}

#[tokio::test(start_paused = true)]
async fn trace_follow_advances_empty_filtered_pages_and_stops_on_resync() {
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
                Reply::Value(json!({"items":[],"next_after":30})),
            ),
            (events_path(30), Reply::Error(410)),
        ],
    )
    .await;
    assert_refusal(&output, 0);
    assert!(output.stdout.is_empty());
}

#[tokio::test(start_paused = true)]
async fn trace_follow_uses_legacy_only_when_the_page_route_is_absent() {
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
            (events_path(10), Reply::MissingRoute),
            (
                "/v1/health".into(),
                Reply::Value(json!({"features":{"bounded_legacy_events":1}})),
            ),
            (
                events_path(10).replace("/events/page?", "/events?"),
                Reply::Value(json!([event(11)])),
            ),
            (
                events_path(11).replace("/events/page?", "/events?"),
                Reply::Error(410),
            ),
        ],
    )
    .await;
    assert_refusal(&output, 0);
    let event: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["store_index"], 11);
}

#[tokio::test(start_paused = true)]
async fn trace_follow_refuses_an_unbounded_legacy_daemon_without_reading_its_history() {
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
            (events_path(10), Reply::MissingRoute),
            ("/v1/health".into(), Reply::Value(json!({"features":{}}))),
        ],
    )
    .await;
    assert!(output.result.is_err());
    assert_eq!(output.requests.len(), 3);
    assert!(
        output
            .result
            .unwrap_err()
            .to_string()
            .contains("upgrade the daemon")
    );
}

#[tokio::test(start_paused = true)]
async fn trace_follow_reports_the_gap_and_explicit_continuation_without_resetting() {
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
                Reply::CursorGap {
                    floor: 30,
                    frontier: 50,
                },
            ),
        ],
    )
    .await;
    let error = output.result.unwrap_err();
    assert!(error.downcast_ref::<EventCursorGap>().is_some());
    assert!(error.to_string().contains("retained floor 30, frontier 50"));
    assert!(
        error
            .to_string()
            .contains("st trace show 'host/follow-fixture' --after-index 50 --follow")
    );
    assert_eq!(output.requests.len(), 2);
    assert!(output.stdout.is_empty());
}

#[tokio::test(start_paused = true)]
async fn non_trace_event_gap_does_not_suggest_a_trace_continuation() {
    let path = events_path(10);
    let client = Client::scripted_follow_test(vec![(
        path.clone(),
        Reply::CursorGap {
            floor: 30,
            frontier: 50,
        },
    )]);
    let error = LocalEventFeed::default()
        .read(&client, path.split_once('?').unwrap().1)
        .await
        .err()
        .unwrap();
    assert!(error.downcast_ref::<EventCursorGap>().is_some());
    assert!(error.to_string().contains("retained floor 30, frontier 50"));
    assert!(error.to_string().contains("retry this command explicitly"));
    assert!(!error.to_string().contains("st trace show"));
    assert_eq!(client.follow_test_result().0.len(), 1);
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
            (
                events_path(10),
                Reply::Value(json!({"items":[event(11), event(12)],"next_after":12})),
            ),
            (events_path(12), Reply::Timeout),
            (events_path(12), Reply::Disconnect),
            (
                events_path(12),
                Reply::Value(json!({"items":[event(13), event(14)],"next_after":14})),
            ),
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
            (
                events_path(10),
                Reply::Value(json!({"items":[event(11), event(12)],"next_after":12})),
            ),
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
        (TREE_PATH.into(), Reply::Value(tree(before, None))),
    ];
    if interrupt_tree {
        script.push((RUN_PATH.into(), Reply::Value(after.clone())));
        script.push((TREE_PATH.into(), Reply::Timeout));
    } else {
        script.push((RUN_PATH.into(), Reply::Timeout));
        script.push((RUN_PATH.into(), Reply::Value(after.clone())));
    }
    script.extend([
        (TREE_PATH.into(), Reply::Value(tree(after, None))),
        (RUN_PATH.into(), Reply::Value(finished.clone())),
        (TREE_PATH.into(), Reply::Value(tree(finished, None))),
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
async fn mission_follow_honors_page_limit_and_refreshes_when_lookahead_appears() {
    let running = run("running", "normal");
    let finished = run("completed", "normal");
    let tree_path = TREE_PATH.replace("limit=50", "limit=20");
    let output = scripted_cli(
        &["missions", "show", RUN, "--follow", "--limit", "20"],
        vec![
            (RUN_PATH.into(), Reply::Value(running.clone())),
            (tree_path.clone(), Reply::Value(tree(running.clone(), None))),
            (RUN_PATH.into(), Reply::Value(running.clone())),
            (tree_path.clone(), Reply::Value(tree(running, Some("page-child-020")))),
            (RUN_PATH.into(), Reply::Value(finished.clone())),
            (tree_path.clone(), Reply::Value(tree(finished, Some("page-child-020")))),
        ],
    )
    .await;
    assert!(output.result.is_ok(), "{}", stderr(&output));
    assert_eq!(
        output.requests.iter().filter(|(path, _)| path == &tree_path).count(),
        3
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("STATE     running · normal").count(), 2, "{stdout}");
    assert_eq!(stdout.matches("More runs follow").count(), 2, "{stdout}");
    assert!(stdout.contains("--cursor page-child-020 --limit 20"));
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
                Reply::Delayed(
                    json!({"items":[],"next_after":null}),
                    Duration::from_secs(30),
                ),
            ),
            (
                events_path(10),
                Reply::Delayed(
                    json!({"items":[event(11)],"next_after":11}),
                    Duration::from_secs(30),
                ),
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
        script.push((
            events_path(index),
            Reply::Value(json!({"items":[event(index + 1)],"next_after":index+1})),
        ));
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
