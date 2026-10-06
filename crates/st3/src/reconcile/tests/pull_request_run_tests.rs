use super::*;

const SOURCE: &str = r#"version 2
mission "review" state="ready" {
  goal "Review one immutable pull request snapshot."
  concurrent-runs max=4
  input "source" kind="resource"
  completion { when "all-steps-exhausted" }
  step "review" { agentless; gate "wait" { document "doc/review-ready" } }
}
resource "repo" { kind "vcs.repository" }
observer "repo" { resource "resource/repo"; provider "github.repository"; locator "acme/garden"; field "pull_requests" }
subscription "reviews" { observer "observer/repo"; on "pull_requests"; delivery "mission" { mission "review"; resource "source"; workspace "/tmp/example-reviews" } }
"#;

const PR: &str = "resource/repo/pull-request/7";

fn observe(store: &Store, head: &str, state: &str) -> String {
    store
        .append_claim(&ClaimInput {
            subject: PR.into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("vcs.pull-request".into())),
                (
                    "facts".into(),
                    serde_json::json!({"number":7, "head_sha":head, "state":state, "draft":false}),
                ),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .id
}

fn request(store: &Store, snapshot: &str, key: &str) -> String {
    store
        .append_claim(&ClaimInput {
            subject: "subscription/reviews".into(),
            kind: "subscription.mission-requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("mission".into(), Value::String("mission/review".into())),
                ("resource".into(), Value::String(PR.into())),
                ("discovery".into(), Value::String(snapshot.into())),
                ("resource_input".into(), Value::String("source".into())),
                (
                    "workspace".into(),
                    Value::String("/tmp/example-reviews".into()),
                ),
                ("requester".into(), Value::String("person/operator".into())),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
        .unwrap()
        .id
}

fn reconciler(store: &Arc<Store>) -> Reconciler<FakeRuntime> {
    Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
}

fn start(store: &Arc<Store>, snapshot: &str, key: &str) -> MissionRunView {
    request(store, snapshot, key);
    reconciler(store)
        .reconcile_subscription_missions(&store.desired_subjects().unwrap())
        .unwrap();
    let started = store
        .claims_for("subscription/reviews", Some("subscription.mission-started"))
        .unwrap();
    store
        .mission_run(
            started.last().unwrap().body["fields"]["mission_run"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
        .unwrap()
}

#[test]
fn completed_pull_request_snapshot_is_deduplicated_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store.db");
    let store = Arc::new(Store::open(&path, "node").unwrap());
    apply_source(&store, SOURCE, "fixture");
    let snapshot = observe(&store, &"a".repeat(40), "open");
    let run = start(&store, &snapshot, "first");
    store
        .set_mission_run_state(&run.id, "completed", "terminal", None)
        .unwrap();
    drop(store);

    let store = Arc::new(Store::open(&path, "node").unwrap());
    let duplicate = request(&store, &snapshot, "duplicate");
    let r = reconciler(&store);
    for _ in 0..2 {
        r.reconcile_subscription_missions(&store.desired_subjects().unwrap())
            .unwrap();
    }
    assert_eq!(
        store
            .claims_for("subscription/reviews", Some("subscription.mission-started"))
            .unwrap()
            .len(),
        1
    );
    let cancelled = store
        .claims_for(
            "subscription/reviews",
            Some("subscription.mission-request-cancelled"),
        )
        .unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].body["fields"]["request"], duplicate);
    assert!(
        cancelled[0].body["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("already completed")
    );

    // A fresh observation of the same head is a different immutable snapshot.
    let next = observe(&store, &"a".repeat(40), "open");
    assert_ne!(snapshot, next);
    let next_run = start(&store, &next, "next-snapshot");
    assert_ne!(run.id, next_run.id);
    assert_eq!(
        next_run.inputs["source"].claim_id.as_deref(),
        Some(next.as_str())
    );
}

#[test]
fn failed_pull_request_snapshot_can_be_retried() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, SOURCE, "fixture");
    let snapshot = observe(&store, &"a".repeat(40), "open");
    let first = start(&store, &snapshot, "first");
    store
        .set_mission_run_state(&first.id, "failed", "terminal", Some("review failed"))
        .unwrap();
    let retry = start(&store, &snapshot, "retry");
    assert_ne!(first.id, retry.id);
}

#[test]
fn superseded_subscription_pull_request_runs_cancel_but_authored_runs_keep_their_input() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, SOURCE, "fixture");
    let snapshot = observe(&store, &"a".repeat(40), "open");
    let run = start(&store, &snapshot, "first");
    let authored = store
        .create_mission_run(&MissionRunRequest {
            mission: "review".into(),
            revision: None,
            workspace: "/tmp/example-authored".into(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::from([("source".into(), format!("{PR}@{snapshot}"))]),
            idempotency_key: "authored".into(),
        })
        .unwrap();
    let r = reconciler(&store);
    r.evaluate_mission_runs().unwrap();
    assert_eq!(store.mission_run(&run.id).unwrap().unwrap().phase, "normal");
    let (_, reads) =
        smallclaims::touched::record(|| store.stale_subscription_pull_request_run(&run).unwrap());
    assert!(
        reads.contains(PR),
        "the next observation must wake the run evaluator"
    );

    observe(&store, &"b".repeat(40), "open");
    for _ in 0..5 {
        r.evaluate_mission_runs().unwrap();
    }
    let ended = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(ended.status, "cancelled");
    assert!(ended.steps.iter().all(|step| step.status == "cancelled"));
    assert_eq!(
        ended.inputs["source"].claim_id.as_deref(),
        Some(snapshot.as_str())
    );
    assert_eq!(
        store.mission_run(&authored.id).unwrap().unwrap().status,
        "running"
    );
    let outcomes = store
        .claims_for(&run.subject, Some("mission-run.state"))
        .unwrap();
    assert!(outcomes.iter().any(|c| {
        c.body["fields"]["reason"]
            .as_str()
            .is_some_and(|s| s.contains("moved from head"))
    }));
    assert!(
        !outcomes
            .iter()
            .any(|c| c.body["fields"]["status"] == "failed")
    );
}

#[test]
fn run_creation_rechecks_a_pull_request_that_moved_after_queue_inspection() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, SOURCE, "fixture");
    let snapshot = observe(&store, &"a".repeat(40), "open");
    observe(&store, &"b".repeat(40), "open");
    let error = store
        .create_subscription_mission_run(
            &MissionRunRequest {
                mission: "review".into(),
                revision: None,
                workspace: "/tmp/example-reviews".into(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::from([("source".into(), format!("{PR}@{snapshot}"))]),
                idempotency_key: "stale-creation".into(),
            },
            None,
            "subscription/reviews",
            PR,
            &snapshot,
        )
        .unwrap_err();
    assert_eq!(error.code, "stale-pull-request");
    assert!(
        store
            .active_mission_runs_for_mission("review")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn review_goal_product_and_contract_gates_share_the_same_run_context() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, SOURCE, "fixture");
    apply_source(
        &store,
        r#"version 2
mission "contract" state="ready" {
  goal "Verify the review artifact contract."
  input "source" kind="resource"
  completion { when "all-steps-exhausted" }
  step "inspect" {
    agentless
    goal "Publish resource/mission-run/${ST_MISSION_RUN}/review for ${input.source}."
    produces { resource "mission-run/${ST_MISSION_RUN}/review" { kind "human.review" } }
    gate "target" { field "target" "resource/mission-run/${ST_MISSION_RUN}/review" "is" "${input.source}" }
    gate "document" { field "document" "resource/mission-run/${ST_MISSION_RUN}/review" "contains" "mission-run/${ST_MISSION_RUN}/review@" }
  }
}"#,
        "contract",
    );
    let snapshot = observe(&store, &"a".repeat(40), "open");
    let target = format!("{PR}@{snapshot}");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "contract".into(),
            revision: None,
            workspace: "/tmp/example-contract".into(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::from([("source".into(), target.clone())]),
            idempotency_key: "contract-run".into(),
        })
        .unwrap();
    assert_eq!(
        run.steps[0].goals,
        [format!(
            "Publish resource/mission-run/{}/review for {target}.",
            run.id
        )]
    );
    let write = |document: String| {
        store
            .append_claim(&ClaimInput {
                subject: format!("resource/mission-run/{}/review", run.id),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("human.review".into())),
                    ("target".into(), Value::String(target.clone())),
                    ("document".into(), Value::String(document)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let r = reconciler(&store);
    write(format!(
        "doc/mission-run/another-round/review@{}",
        "a".repeat(64)
    ));
    for _ in 0..3 {
        r.evaluate_mission_runs().unwrap();
    }
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().status,
        "running"
    );
    write(format!(
        "doc/mission-run/{}/review@{}",
        run.id,
        "b".repeat(64)
    ));
    for _ in 0..5 {
        r.evaluate_mission_runs().unwrap();
    }
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().status,
        "completed"
    );
}
