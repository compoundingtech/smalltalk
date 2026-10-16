//! Real isolated daemon/runtime controls. Run only in the admitted ordinary hosted lane.
//! Neither table injection nor two fake runtimes substitutes for the two physical starts.
use super::*;

fn source(root: &Path, schedule: &str, timing: &str, after: &str, work_root: &Path) -> String {
    let marker = root.join("actual-starts");
    let command = format!("printf '%s %s\\n' \"$HOME\" \"$ST_MISSION_RUN\" >> '{}'", marker.display());
    format!(r#"version 2
mission "harvest" state="ready" {{
  goal "Perform the scheduled harvest occurrence."
  completion {{ when "all-steps-exhausted" }}
  step "perform" {{
    agentless
    exec "job" {{ host "local"; workspace "${{ST_WORKSPACE}}"; command {}; restart "never" }}
    gate "job performed" {{ field "exit_code" "exec/${{ST_MISSION_RUN}}/job" is 0 }}
  }}
}}
schedule "{schedule}" {{
 host "ivory"
 standby {{ host "jade"; after "{after}" }}
 {timing}
 work {{ mission "harvest"; workspace {:?} }}
}}
"#, serde_json::to_string(&command).unwrap(), work_root.to_str().unwrap())
}

fn starts(root: &Path) -> Vec<(String, String)> {
    fs::read_to_string(root.join("actual-starts")).unwrap_or_default().lines()
        .filter_map(|line| line.split_once(' ').map(|(home, run)| (home.into(), run.into())))
        .collect()
}

async fn schedule_claims(node: &Node, name: &str, kind: &str) -> Vec<Value> {
    let page: Value = node.client().get(&format!("/v1/claims?subject=schedule%2F{name}&order=desc&limit=100")).await.unwrap();
    page["claims"].as_array().unwrap().iter().filter(|claim| claim["kind"] == kind).cloned().collect()
}

// Keep the same deadline and checks; collect bounded read-only fixture evidence on failure.
async fn wait_for_standby<F, Fut>(what: &str, seconds: u64, node: &Node, root: &Path,
    schedule: &str, mut check: F)
where F: FnMut() -> Fut, Fut: std::future::Future<Output = bool> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if check().await { return; }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let client = node.client();
    let paths = [
        format!("/v1/status?subject=schedule%2F{schedule}"),
        "/v1/replication/status".into(),
        format!("/v1/status?subject=daemon%2F{}", node.name),
        format!("/v1/claims?subject=schedule%2F{schedule}&order=desc&limit=40"),
    ];
    let mut diagnostics = String::new();
    for path in paths {
        let read = async {
            let result: Result<Value, _> = client.get(&path).await;
            format!("{result:?}")
        };
        let body = match tokio::time::timeout(Duration::from_secs(3), read).await {
            Ok(body) => body.chars().take(16_000).collect::<String>(),
            Err(_) => "diagnostic read timed out".into(),
        };
        diagnostics.push_str(&format!("\n{path}: {body}"));
    }
    panic!("timed out waiting until {what}\nphysical starts: {:?}\n{}\n{diagnostics}",
        starts(root), node.logs());
}

async fn available_pair(root: &Path) -> (Node, Node) {
    let primary = anchor(root, "ivory").await;
    let standby = joined(root, &primary, "jade", &[]).await;
    standby.wait_listening().await;
    // Read the actual replicated own-origin startup advertisements on both daemons before
    // publication. Missing/old advertisements are tested separately by the source controls.
    wait_until("both members advertise standby on both daemons", 60, || async {
        for node in [&primary, &standby] {
            let claims = node.claims().await;
            for host in ["ivory", "jade"] {
                if !claims.iter().any(|claim| claim["subject"] == format!("daemon/{host}")
                    && claim["origin"] == host && claim["kind"] == "daemon.started"
                    && claim["body"]["fields"]["features"]["schedule_standby"] == 1) {
                    return false;
                }
            }
        }
        true
    }).await;
    (primary, standby)
}

async fn publish_pair(primary: &Node, standby: &Node, name: &str, source: &str) {
    let file = primary.root.join(format!("{name}.kdl"));
    fs::write(&file, source).unwrap();
    primary.st_ok(&["apply", "--no-gate-check", file.to_str().unwrap(), "--as", PERSON]);
    wait_until("standby applied schedule and complete primary exchange before due", 60, || async {
        let status: Value = standby.client().get(&format!("/v1/status?subject=schedule%2F{name}")).await.unwrap();
        let replicated = status["subjects"].as_array().is_some_and(|subjects| subjects.iter().any(|subject|
            subject["desired"].is_object()));
        let sync: Value = standby.client().get("/v1/replication/status").await.unwrap();
        replicated && sync["peers"].as_array().is_some_and(|peers| peers.iter().any(|peer|
            peer["peer"] == "ivory" && peer["sync"]["peer_only_envelopes"] == 0
                && peer["sync"]["measured_at_unix_ms"].as_u64().is_some()))
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn partition_performs_two_real_starts_then_converges_on_one_occurrence_run() {
    if st3::test_support::supervise_test() { return; }
    let root = tempfile::tempdir().unwrap();
    let (mut primary, mut standby) = available_pair(root.path()).await;
    let at = (chrono::Utc::now() + chrono::Duration::seconds(8)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let declaration = source(root.path(), "split", &format!("at {at:?}"), "2s", &root.path().join("work"));
    publish_pair(&primary, &standby, "split", &declaration).await;
    // Stop only the actual replication workers/listeners. Both daemons and real execution
    // runtimes stay live, with a known complete pre-partition primary view.
    primary.stop_worker();
    standby.stop_worker();
    wait_for_standby("both isolated runtimes physically performed the job", 60, &standby, root.path(), "split", || async {
        let rows = starts(root.path());
        rows.iter().any(|(home, _)| home.contains("/ivory/home"))
            && rows.iter().any(|(home, _)| home.contains("/jade/home"))
    }).await;
    let rows = starts(root.path());
    assert_eq!(rows.len(), 2, "unexpected repeated physical starts: {rows:?}\n{}\n{}", primary.logs(), standby.logs());
    assert_eq!(rows[0].1, rows[1].1, "physical starts used different occurrence identities");
    let run = st3::store::Store::scheduled_mission_run_subject("schedule/split", 0);
    for node in [&primary, &standby] {
        let started = schedule_claims(node, "split", "schedule.work-started").await;
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["body"]["fields"]["mission_run"], run);
    }
    primary.start_worker();
    standby.start_worker();
    wait_until("both starts and one canonical generation are replicated", 60, || async {
        let left = primary.st_json(&["missions", "show", &run]);
        let right = standby.st_json(&["missions", "show", &run]);
        let left_claims = schedule_claims(&primary, "split", "schedule.work-started").await;
        let right_claims = schedule_claims(&standby, "split", "schedule.work-started").await;
        left["generation"] == right["generation"] && left_claims.len() == 2 && right_claims.len() == 2
            && left["steps"].as_array().is_some_and(|steps| steps.len() == 1)
            && right["steps"].as_array().is_some_and(|steps| steps.len() == 1)
    }).await;
    assert_eq!(starts(root.path()).len(), 2, "healing replayed physical work");
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_primary_holds_standby_then_returning_primary_does_not_replay_standby_occurrence() {
    if st3::test_support::supervise_test() { return; }
    let root = tempfile::tempdir().unwrap();
    let (mut primary, standby) = available_pair(root.path()).await;
    let anchor = (chrono::Utc::now() + chrono::Duration::seconds(6)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let declaration = source(root.path(), "return", &format!("every \"8s\"; anchor {anchor:?}; catch-up \"latest\""), "2s", &root.path().join("work"));
    publish_pair(&primary, &standby, "return", &declaration).await;
    wait_until("healthy primary physically performs first occurrence", 60, || async {
        starts(root.path()).iter().any(|(home, _)| home.contains("/ivory/home"))
    }).await;
    assert!(schedule_claims(&standby, "return", "schedule.work-started").await.iter().all(|claim| claim["origin"] != "jade"), "fresh primary admitted standby work");
    let first_run = st3::store::Store::scheduled_mission_run_subject("schedule/return", 0);
    wait_until("first primary run completed and completion replicated before stopping it", 60, || async {
        for node in [&primary, &standby] {
            let page: Value = node.client().get(&format!("/v1/claims?subject={first_run}&order=desc&limit=100")).await.unwrap();
            if !page["claims"].as_array().is_some_and(|claims| claims.iter().any(|claim|
                claim["kind"] == "mission-run.state" && claim["body"]["fields"]["status"] == "completed")) {
                return false;
            }
        }
        true
    }).await;
    primary.stop();
    wait_for_standby("stale-primary standby physically performs next due occurrence", 60, &standby, root.path(), "return", || async {
        starts(root.path()).iter().any(|(home, _)| home.contains("/jade/home"))
    }).await;
    let standby_run = starts(root.path()).into_iter().find(|(home, _)| home.contains("/jade/home")).unwrap().1;
    primary.start().await;
    wait_until("returning primary received standby reached/start facts", 60, || async {
        schedule_claims(&primary, "return", "schedule.work-started").await.iter().any(|claim|
            claim["origin"] == "jade" && claim["body"]["fields"]["mission_run"].as_str()
                .is_some_and(|run| run.trim_start_matches("mission-run/") == standby_run.trim_start_matches("mission-run/")))
    }).await;
    wait_until("primary performs a later occurrence after handback", 60, || async {
        starts(root.path()).iter().filter(|(home, _)| home.contains("/ivory/home")).count() >= 2
    }).await;
    assert_eq!(starts(root.path()).iter().filter(|(_, run)| run == &standby_run).count(), 1, "primary replayed standby's known occurrence");
    assert!(schedule_claims(&standby, "return", "schedule.occurrence-reached").await.iter().any(|claim|
        claim["body"]["fields"]["standby"]["primary_heartbeat_age_ms"].as_u64().is_some()));
}

#[tokio::test(flavor = "multi_thread")]
async fn restarted_standby_exposes_skip_limit_and_does_not_use_persisted_success_as_coverage() {
    if st3::test_support::supervise_test() { return; }
    let root = tempfile::tempdir().unwrap();
    let (mut primary, mut standby) = available_pair(root.path()).await;
    let at = (chrono::Utc::now() + chrono::Duration::seconds(6)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let declaration = source(root.path(), "cold", &format!("every \"8s\"; anchor {at:?}; catch-up \"latest\""), "1s", &root.path().join("work"));
    publish_pair(&primary, &standby, "cold", &declaration).await;
    primary.stop();
    standby.restart().await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(starts(root.path()).is_empty(), "cold standby invented complete coverage from persisted success");
    let shown = standby.st_json(&["subject", "show", "schedule/cold"]);
    assert_eq!(shown["schedule_standby"]["cold_standby_limit"], st3::schedule_standby::COLD_STANDBY_LIMIT);
    assert_eq!(shown["schedule_standby"]["run_twice_warning"], st3::schedule_standby::RUN_TWICE_WARNING);
    assert_eq!(shown["schedule_standby"]["heartbeat_basis"], "unknown");
    let text = standby.st_ok(&["subject", "show", "schedule/cold"]);
    assert!(text.contains(st3::schedule_standby::COLD_STANDBY_LIMIT));
    let history = standby.st_ok(&["subject", "history", "schedule/cold"]);
    assert!(history.contains(st3::schedule_standby::COLD_STANDBY_LIMIT));
    // The returning daemon's authenticated replication/startup facts and the existing
    // stage/schedule-sync retry are related wakes. Reads below do not force reconciliation.
    primary.start().await;
    wait_until("primary exchange/schedule-sync wake seeds the cold standby heartbeat", 60, || async {
        let status: Value = standby.client().get("/v1/status?subject=schedule%2Fcold").await.unwrap();
        status["subjects"].as_array().is_some_and(|subjects| subjects.iter().any(|subject|
            subject["actual"]["schedule_standby"]["heartbeat_basis"] == "current-incarnation"))
    }).await;
    primary.stop();
    // No unrelated user write or explicit reconcile call: the source-owned occurrence and
    // standby-eligibility deadlines alone must eventually perform a later due job.
    wait_for_standby("heard-primary standby deadline actually performs a later occurrence", 60, &standby, root.path(), "cold", || async {
        starts(root.path()).iter().any(|(home, _)| home.contains("/jade/home"))
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_standby_workspace_fails_visibly_once_without_retry_burn() {
    if st3::test_support::supervise_test() { return; }
    let root = tempfile::tempdir().unwrap();
    let (mut primary, standby) = available_pair(root.path()).await;
    let blocked = root.path().join("not-a-directory");
    fs::write(&blocked, "an existing file").unwrap();
    let at = (chrono::Utc::now() + chrono::Duration::seconds(6)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let declaration = source(root.path(), "missing-work", &format!("at {at:?}"), "1s", &blocked);
    publish_pair(&primary, &standby, "missing-work", &declaration).await;
    primary.stop();
    wait_for_standby("one typed unavailable-work failure", 60, &standby, root.path(), "missing-work", || async {
        schedule_claims(&standby, "missing-work", "schedule.work-failed").await.iter().any(|claim|
            claim["origin"] == "jade" && claim["body"]["fields"]["code"] == "standby-work-unavailable")
    }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let failures = schedule_claims(&standby, "missing-work", "schedule.work-failed").await;
    assert_eq!(failures.iter().filter(|claim| claim["origin"] == "jade").count(), 1);
    assert!(starts(root.path()).is_empty());
}
