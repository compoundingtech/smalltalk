//! A person answers a mission's human gate from every surface: client-v0 actions (stui, iOS),
//! the CLI, and the HTTP API. Each test drives a real reconciler and an in-process API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimInput, IntentInput, MissionRunRequest};
use st3::reconcile::Reconciler;
use st3::store::Store;
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

const NODE: &str = "human-gates";

fn test_state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory(NODE).unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: NODE.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

/// A runtime that starts nothing: gates here wait only on people.
struct NoRuntime;

impl st3::reconcile::RuntimeControl for NoRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

fn reconciler(store: &Arc<Store>) -> Reconciler<NoRuntime> {
    Reconciler::new(
        store.clone(),
        Arc::new(NoRuntime),
        NODE.into(),
        Arc::new(Notify::new()),
    )
}

/// Reconcile until `done` holds, or fail after a bounded number of passes.
fn reconcile_until(reconciler: &Reconciler<NoRuntime>, what: &str, done: impl Fn() -> bool) {
    for _ in 0..30 {
        reconciler.reconcile_once().unwrap();
        if done() {
            return;
        }
    }
    panic!("{what} did not happen within 30 reconcile passes");
}

const RELEASE: &str = r#"version 2
mission "release" state="ready" {
  goal "Ship the release once a person approves it."
  step "approve" {
    agentless
    title "Approve the release"
    gate "accept" type="human" {
      reviewer "person/avery"
      question "Ship this release?"
    }
  }
}
"#;

fn apply(store: &Store, source: &str, key: &str) {
    let intent = st3::parse_intent(source, NODE).unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
    store.apply(&intent, &plan.subject_tokens, key).unwrap();
}

fn start(store: &Store, mission: &str, key: &str) -> st3::model::MissionRunView {
    store
        .create_mission_run(&MissionRunRequest {
            mission: mission.into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/avery".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

fn step_status(store: &Store, step: &str) -> String {
    store.step_run(step).unwrap().unwrap().status
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    (status, value)
}

/// The person's client-v0 attention page.
async fn attention(app: &axum::Router, person: &str) -> Value {
    let (status, page) = send(
        app,
        Request::builder()
            .uri("/v1/client/attention")
            .header("x-st3-person", person)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page
}

/// The person's open card for `source`, and the snapshot it was read at.
async fn card(app: &axum::Router, person: &str, source: &str) -> Option<(Value, String)> {
    let page = attention(app, person).await;
    let snapshot = page["snapshot"]["id"].as_str().unwrap().to_owned();
    page["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["source_id"] == source)
        .cloned()
        .map(|item| (item, snapshot))
}

/// Answer `card` as `person` through `/v1/client/actions`, fenced on the card as read.
async fn act(
    app: &axum::Router,
    person: &str,
    action: &str,
    key: &str,
    card: &(Value, String),
    reason: Option<&str>,
) -> (StatusCode, Value) {
    let (item, snapshot) = card;
    let mut parameters = json!({ "target_id": item["source_id"] });
    if let Some(reason) = reason {
        parameters["reason"] = json!(reason);
    }
    let body = json!({
        "api_version": "st3.client.v0",
        "id": format!("action/{key}"),
        "type": action,
        "idempotency_key": format!("{key}-0000000000000000"),
        "fence": {
            "snapshot_id": snapshot,
            "subject_revisions": { (item["id"].as_str().unwrap()): item["revision"] },
        },
        "parameters": parameters,
    });
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/v1/client/actions")
            .header("x-st3-person", person)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
}

/// `POST /v1/reviews/{target}`, the route `st attention approve|reject` uses.
async fn review(
    app: &axum::Router,
    target: &str,
    decision: &str,
    actor: &str,
    reason: Option<&str>,
) -> (StatusCode, Value) {
    let body = json!({ "decision": decision, "actor": actor, "reason": reason });
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/v1/reviews/{target}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn a_person_approves_a_human_gate_through_client_actions_with_its_attention_fence() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    apply(&store, RELEASE, "release");
    let run = start(&store, "release", "release-run");
    let step = run.steps[0].subject.clone();
    let reconciler = reconciler(&store);
    reconcile_until(&reconciler, "the gate asked", || {
        store.gate_request_for_owner(&step).unwrap().is_some()
    });
    let app = st3::api::router(state);

    let seen = card(&app, "person/avery", &step).await.expect("no card");
    assert_eq!(seen.0["attention_kind"], "human-gate");
    assert_eq!(
        seen.0["actions"],
        json!(["review.approve", "review.reject"])
    );
    let (status, approved) = act(
        &app,
        "person/avery",
        "review.approve",
        "approve",
        &seen,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    reconcile_until(&reconciler, "the step completed", || {
        step_status(&store, &step) == "completed"
    });
    assert!(card(&app, "person/avery", &step).await.is_none());

    // The card is spent: a second answer through it is stale, and the review route says why
    // instead of only that nothing is pending.
    let (status, again) = act(
        &app,
        "person/avery",
        "review.reject",
        "again",
        &seen,
        Some("no"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    assert_eq!(again["code"], "stale-fence");
    let (status, refused) = review(&app, &step, "approved", "person/avery", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["code"], "review-not-requested");
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains("already answered") && message.contains("approved by person/avery"),
        "{message}"
    );
}

#[tokio::test]
async fn a_gate_asked_again_refuses_the_old_card_and_takes_the_current_one() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    apply(&store, RELEASE, "release");
    let run = start(&store, "release", "release-run");
    let step = run.steps[0].subject.clone();
    let reconciler = reconciler(&store);
    reconcile_until(&reconciler, "the gate asked", || {
        store.gate_request_for_owner(&step).unwrap().is_some()
    });
    let app = st3::api::router(state);
    let seen = card(&app, "person/avery", &step).await.expect("no card");

    // The step fails and is retried before the person answers. Until st asks again, the gate
    // has no current request, and the review route says which attempt the old one was for.
    let asked = store.gate_request_for_owner(&step).unwrap().unwrap().id;
    store
        .set_step_state(&step, "failed", Some("the release build broke"))
        .unwrap();
    store.retry_step(&step, "rebuilt the release", 0).unwrap();
    let (status, refused) = review(&app, &step, "approved", "person/avery", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains("attempt 1") && message.contains("attempt 2"),
        "{message}"
    );

    // The next passes ask the gate again for the attempt that now runs.
    reconcile_until(&reconciler, "the gate asked again", || {
        store
            .gate_request_for_owner(&step)
            .unwrap()
            .is_some_and(|request| request.id != asked)
    });
    let (status, stale) = act(
        &app,
        "person/avery",
        "review.approve",
        "old-card",
        &seen,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["code"], "stale-fence");
    assert!(
        store
            .claims_for_kind_at("gate.result", None, true, 10)
            .unwrap()
            .claims
            .is_empty(),
        "a stale card writes nothing"
    );
    let current = card(&app, "person/avery", &step)
        .await
        .expect("the gate was not asked again");
    assert_ne!(current.0["id"], seen.0["id"]);
    let (status, approved) = act(
        &app,
        "person/avery",
        "review.approve",
        "new-card",
        &current,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    reconcile_until(&reconciler, "the step completed", || {
        step_status(&store, &step) == "completed"
    });
}

async fn run_cli(socket: &Path, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let socket = socket.to_path_buf();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .arg("--endpoint")
            .arg(socket)
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

async fn serve(state: AppState, root: &Path) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = root.join("st3.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(state))
            .await
            .unwrap();
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists(), "the test socket did not appear");
    (socket, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_approves_a_gate_by_the_attention_id_it_lists_and_says_why_it_refuses() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    apply(&store, RELEASE, "release");
    let run = start(&store, "release", "release-run");
    let step = run.steps[0].subject.clone();
    let reconciler = reconciler(&store);
    reconcile_until(&reconciler, "the gate asked", || {
        store.gate_request_for_owner(&step).unwrap().is_some()
    });
    let app = st3::api::router(state.clone());
    let (socket, server) = serve(state, root.path()).await;

    // `st attention ls` leads each card with its attention ID; that is what a person copies.
    let listed = run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let listed = String::from_utf8(listed.stdout).unwrap();
    let (seen, _) = card(&app, "person/avery", &step).await.expect("no card");
    let id = seen["id"].as_str().unwrap();
    assert!(listed.contains(id), "{listed}");

    let approved = run_cli(
        &socket,
        &["attention", "approve", id, "--as", "person/someone-else"],
    )
    .await;
    assert!(!approved.status.success());
    let wrong = String::from_utf8_lossy(&approved.stderr);
    assert!(
        wrong.contains("wrong-reviewer") && wrong.contains("person/avery"),
        "{wrong}"
    );
    let approved = run_cli(
        &socket,
        &[
            "--json",
            "attention",
            "approve",
            id,
            "--as",
            "person/avery",
            "--reason",
            "ship it",
        ],
    )
    .await;
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let result: Value = serde_json::from_slice(&approved.stdout).unwrap();
    assert_eq!(result["kind"], "gate.result");
    assert_eq!(result["body"]["fields"]["verdict"], "pass");
    reconcile_until(&reconciler, "the step completed", || {
        step_status(&store, &step) == "completed"
    });

    // A target that names no gate is refused as such.
    let unknown = run_cli(
        &socket,
        &[
            "attention",
            "approve",
            "mission/release",
            "--as",
            "person/avery",
        ],
    )
    .await;
    assert!(!unknown.status.success());
    let unknown = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        unknown.contains("review-target-unknown") && unknown.contains("mission/release"),
        "{unknown}"
    );

    // Answering again explains itself: the card is gone, and the step was already approved.
    let again = run_cli(
        &socket,
        &[
            "attention",
            "reject",
            id,
            "--as",
            "person/avery",
            "--reason",
            "no",
        ],
    )
    .await;
    assert!(!again.status.success());
    let again = String::from_utf8_lossy(&again.stderr);
    assert!(again.contains("not open"), "{again}");
    let again = run_cli(
        &socket,
        &[
            "attention",
            "reject",
            &step,
            "--as",
            "person/avery",
            "--reason",
            "no",
        ],
    )
    .await;
    assert!(!again.status.success());
    let again = String::from_utf8_lossy(&again.stderr);
    assert!(
        again.contains("review-not-requested") && again.contains("approved by person/avery"),
        "{again}"
    );
    server.abort();
}

const LOOPS: &str = r#"version 2
resource "result" { kind "custom.test.loop-result" }
mission "accept-best" state="ready" {
  goal "Let a person accept the bounded result."
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 1
    until { gate "ready" { field "state" "resource/result" is "ready" } }
    round { completion { when "all-steps-exhausted" } }
    on-exhausted {
      gate "accept" type="human" {
        reviewer "person/avery"
        question "Accept the best bounded result?"
      }
    }
  }
}
mission "accept-round" state="ready" {
  goal "Let a person accept a round."
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 2
    until {
      gate "accept" type="human" {
        reviewer "person/avery"
        question "Accept this round?"
      }
    }
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_human_gates_are_listed_approved_and_rejected_like_step_gates() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    apply(&store, LOOPS, "loops");
    store
        .append_claim(&ClaimInput {
            subject: "resource/result".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), json!("custom.test.loop-result")),
                ("state".into(), json!("not-ready")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("loop-result".into()),
        })
        .unwrap();
    let reconciler = reconciler(&store);
    let loop_owner = |run: &st3::model::MissionRunView| {
        format!(
            "loop-run/{}/improve",
            run.generation.strip_prefix("run-generation/").unwrap()
        )
    };
    let app = st3::api::router(state.clone());
    let (socket, server) = serve(state, root.path()).await;

    // An exhausted loop's acceptance gate: a person sees it and approves it from a client.
    let best = start(&store, "accept-best", "accept-best-run");
    let best_owner = loop_owner(&best);
    reconcile_until(&reconciler, "the exhaustion gate asked", || {
        store.gate_request_for_owner(&best_owner).unwrap().is_some()
    });
    let (status, reviews) = send(
        &app,
        Request::builder()
            .uri("/v1/reviews?reviewer=person%2Favery")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviews}");
    assert!(
        reviews["value"]
            .as_array()
            .is_some_and(|reviews| reviews.iter().any(|review| review["owner"] == best_owner)),
        "{reviews}"
    );
    let seen = card(&app, "person/avery", &best_owner)
        .await
        .expect("the loop gate has no card");
    let (status, approved) = act(&app, "person/avery", "review.approve", "loop", &seen, None).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    reconcile_until(&reconciler, "the loop run completed", || {
        store.mission_run(&best.id).unwrap().unwrap().status == "completed"
    });

    // A round's acceptance gate: a person rejects it from the CLI, by its loop subject.
    let round = start(&store, "accept-round", "accept-round-run");
    let round_owner = loop_owner(&round);
    reconcile_until(&reconciler, "the round gate asked", || {
        store
            .gate_request_for_owner(&round_owner)
            .unwrap()
            .is_some()
    });
    let rejected = run_cli(
        &socket,
        &[
            "attention",
            "reject",
            &round_owner,
            "--as",
            "person/avery",
            "--reason",
            "the round missed the point",
        ],
    )
    .await;
    assert!(
        rejected.status.success(),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    let result = store
        .claims_for_kind_at("gate.result", None, true, 10)
        .unwrap()
        .claims
        .into_iter()
        .find(|claim| claim.body["fields"]["verdict"] == "fail")
        .expect("the rejection was not recorded");
    assert_eq!(result.actor.as_deref(), Some("person/avery"));
    let (status, answered) = review(&app, &round_owner, "approved", "person/avery", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answered}");
    assert!(
        answered["message"]
            .as_str()
            .unwrap()
            .contains("rejected by person/avery"),
        "{answered}"
    );
    server.abort();
}
