//! Product actions over the real client/CLI Unix transport, with disk-backed daemon restarts.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimInput, IntentInput, MissionRunRequest};
use st3::store::Store;
use st3_client::{ActionResult, Client, ClientError, Envelope, ErrorCode, Fence};
use tokio::sync::{Notify, watch};

const NODE: &str = "action-coverage";
const PERSON: &str = "person/avery";
const WORKER: &str = "agent/example/worker";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_owned_seat_rollout_captures_fences_and_survives_restart() {
    use st3::store::owned_sets::{Options, Source};
    let mut daemon = Daemon::new().await;
    let seat = "agent/garden/maple";
    let path = daemon.root.path().join("seats.kdl");
    let mut first_token = String::new();
    for sequence in 1..=2_u64 {
        let text = format!(
            "version 2\nagent \"garden/maple\" {{ host {NODE:?}; workspace {:?}; harness \"claude\" {{ model {:?}; }} }}",
            daemon.root.path().display().to_string(),
            format!("model-{sequence}")
        );
        std::fs::write(&path, &text).unwrap();
        let options = Options {
            set: "maples".into(),
            source: Source {
                repository: "acme/maples".into(),
                r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"),
                sequence,
            },
            expected_set: daemon
                .store()
                .owned_sets()
                .unwrap()
                .first()
                .map_or("absent".into(), |set| set.revision.clone()),
            rollout: if sequence == 2 {
                Some(st3::rollout::Policy::when_idle(1_800_000, false))
            } else {
                None
            },
            adopt: Default::default(),
            allow_empty: false,
            confirm_retire: None,
            expected_subjects: Default::default(),
        };
        let mut args = vec![
            "apply",
            "--set",
            "maples",
            path.to_str().unwrap(),
            "--repository",
            "acme/maples",
            "--ref",
            "refs/heads/main",
            "--sha",
            &options.source.sha,
            "--source-sequence",
            if sequence == 1 { "1" } else { "2" },
            "--expect-set",
            &options.expected_set,
            "--as",
            PERSON,
        ];
        if sequence == 2 {
            args.extend(["--rollout", "when-idle", "--rollout-deadline", "30m"]);
        }
        cli_value(daemon.cli(PERSON, &args).await);
        if sequence == 1 {
            first_token = daemon
                .store()
                .selected_desired_token(seat)
                .unwrap()
                .unwrap();
            daemon.claim(seat,"runtime.observed",json!({"status":"running","runtime_id":"maple","host":NODE,"incarnation_id":"maple-original"}));
            daemon.claim(seat,"runtime.action.succeeded",json!({"action":"start","desired_token":first_token,"incarnation_id":"maple-original"}));
        }
        daemon.restart().await;
    }
    let request = json!({"subject":seat,"actor":PERSON,"expected_desired":first_token,"expected_incarnation":"maple-original",
        "policy":st3::rollout::Policy::when_idle(5000,false),"idempotency_key":"stale-maple"});
    let before = daemon.store().index().unwrap();
    let stale = daemon
        .transport()
        .post::<_, Value>("/v1/agents/rollout", &request)
        .await
        .unwrap_err();
    assert_eq!(
        st3::client::api_error_code(&stale),
        Some("stale-rollout-target")
    );
    assert_eq!(before, daemon.store().index().unwrap());
    let response = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "agents",
                    "rollout",
                    seat,
                    "--deadline",
                    "5s",
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    let operation = daemon.store().rollout(seat).unwrap().unwrap();
    assert_eq!(operation.id, response["id"].as_str().unwrap());
    assert_eq!(operation.policy.deadline_ms, 5000);
    assert_eq!(operation.old_incarnation, "maple-original");
    daemon.restart().await;
    let saved = daemon.store().rollout(seat).unwrap().unwrap();
    assert_eq!(saved.id, operation.id);
    assert_eq!(saved.deadline_unix_ms, operation.deadline_unix_ms);
    let status = cli_value(
        daemon
            .cli(
                PERSON,
                &["sets", "status", "maples", "--sha", &format!("{:040x}", 2)],
            )
            .await,
    );
    assert_eq!(status["value"]["commit_status"]["satisfied"], false);
    assert_eq!(
        status["value"]["members_status"][0]["operation"]["phase"],
        "draining"
    );
}

struct Daemon {
    root: tempfile::TempDir,
    state: AppState,
    server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Daemon {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("config/st3")).unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::write(
            root.path().join("config/st3/config.toml"),
            format!("person = {PERSON:?}\n"),
        )
        .unwrap();
        let state = Self::state(root.path());
        let mut daemon = Self {
            root,
            state,
            server: None,
        };
        daemon.serve().await;
        daemon
    }

    fn state(root: &Path) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), NODE).unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: NODE.into(),
            state_dir: root.into(),
            pty_root: root.join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: Some(root.join("native")),
            planner_default: Default::default(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.root.path().join("st3.sock")
    }
    fn store(&self) -> &Store {
        &self.state.store
    }
    fn client(&self, actor: &str) -> Client {
        Client::unix_as(self.socket(), actor)
    }
    fn transport(&self) -> st3::client::Client {
        st3::client::Client::unix_as(self.socket(), PERSON).unwrap()
    }

    async fn serve(&mut self) {
        let socket = self.socket();
        let state = self.state.clone();
        self.server = Some(tokio::spawn(async move {
            st3::api::serve_unix(&socket, st3::api::router(state)).await
        }));
        let mut last_error = None;
        for _ in 0..200 {
            // bind creates the socket path before listen can accept a request. Probe the
            // public protocol rather than treating the path's existence as readiness.
            match self.client(PERSON).capabilities().await {
                Ok(_) => return,
                Err(ClientError::Unreachable(error)) => last_error = Some(error),
                Err(error) => panic!("isolated daemon readiness failed: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("isolated daemon did not listen: {last_error:?}");
    }

    /// End the listener and its router, reopen the on-disk store, and make fresh daemon state.
    /// No old Store, Notify, action gate, or projection cache is reused.
    async fn restart(&mut self) {
        let server = self.server.take().unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_file(self.socket()).unwrap();
        self.state = Self::state(self.root.path());
        self.serve().await;
    }

    async fn fence(&self, actor: &str) -> Fence {
        let snapshot = self.client(actor).capabilities().await.unwrap().snapshot.id;
        serde_json::from_value(json!({"snapshot_id": snapshot})).unwrap()
    }

    fn apply(&self, source: &str, key: &str) {
        let intent = st3::parse_intent(source, NODE).unwrap();
        let preview = self
            .store()
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        self.store()
            .apply_as(&intent, &preview.subject_tokens, key, Some(PERSON))
            .unwrap();
    }

    fn claim(&self, subject: &str, kind: &str, fields: Value) {
        self.store()
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(subject.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn worker(&self) {
        self.apply(&format!("version 2\nagent \"example/worker\" {{ host {:?}; workspace {:?}; harness \"claude\" {{}}; restart always }}\n", NODE, self.root.path().display().to_string()), "coverage-worker");
        self.claim(WORKER, "runtime.observed", json!({"status": "running", "runtime_id": "coverage-worker", "incarnation_id": "4242:fixture"}));
        self.claim(
            WORKER,
            "harness.observed",
            json!({"state": "idle", "driver": "claude", "incarnation_id": "4242:fixture"}),
        );
    }

    fn start(&self, mission: &str, key: &str) -> st3::model::MissionRunView {
        self.store()
            .create_mission_run(&MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: self.root.path().display().to_string(),
                requester: Some(PERSON.into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap()
    }

    async fn work_fence(&self, step: &str) -> Fence {
        let work = self.store().step_run(step).unwrap().unwrap();
        let mut fence = self.fence(WORKER).await;
        fence.mission_generation = Some(work.generation);
        fence.step_definition = Some(work.definition_hash);
        fence.attempt = Some(work.attempt);
        fence.readiness_epoch = Some(u64::from(work.readiness_epoch));
        fence.runtime_incarnation = Some("4242:fixture".into());
        fence
    }

    /// Stale request: no business write and no success receipt. Then restart and use the
    /// exact fence that was read before the restart. Restart again and replay the accepted
    /// action: the same affected IDs and no duplicate writes, even with its now-spent fence.
    async fn exercise(
        &mut self,
        actor: &str,
        kind: &str,
        parameters: Value,
        fence: Fence,
    ) -> Value {
        let key = format!("coverage:{kind}:accepted");
        let mut stale = fence.clone();
        // Spend the action's own observed fence, rather than adding an unrelated revision.
        if let Some((_, revision)) = stale.subject_revisions.iter_mut().next() {
            *revision = "changed".into();
        } else if stale.mission_generation.is_some() {
            stale.mission_generation = Some("changed".into());
        } else if stale.runtime_incarnation.is_some() {
            stale.runtime_incarnation = Some("changed".into());
        } else if stale.runtime_desired_revision.is_some() {
            stale.runtime_desired_revision = Some("changed".into());
        } else {
            stale.snapshot_id = stale.snapshot_id.replacen(NODE, "another-fixture", 1);
        }
        let index = self.store().index().unwrap();
        let error = dispatch(&self.client(actor), kind, &key, stale, parameters.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ClientError::Api(ErrorCode::StaleFence, ..)),
            "{kind}: {error}"
        );
        assert_eq!(
            self.store().index().unwrap(),
            index,
            "{kind}: stale action wrote"
        );
        self.restart().await;
        let accepted = dispatch(
            &self.client(actor),
            kind,
            &key,
            fence.clone(),
            parameters.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("{kind}: {error}"));
        let accepted = serde_json::to_value(accepted.value).unwrap();
        assert_eq!(accepted["status"], "completed", "{kind}: {accepted}");
        let index = self.store().index().unwrap();
        self.restart().await;
        let replay = dispatch(&self.client(actor), kind, &key, fence, parameters)
            .await
            .unwrap_or_else(|error| panic!("{kind} replay: {error}"));
        let replay = serde_json::to_value(replay.value).unwrap();
        assert_eq!(replay["affected_ids"], accepted["affected_ids"], "{kind}");
        assert_eq!(
            self.store().index().unwrap(),
            index,
            "{kind}: replay wrote twice"
        );
        accepted
    }

    fn cli_command(&self, actor: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"));
        command
            .args([
                "--endpoint",
                self.socket().to_str().unwrap(),
                "--json",
                "--daemon-wait",
                "0",
            ])
            .env_remove("ST_AGENT")
            .env_remove("ST3_INCARNATION")
            .env_remove("ST_MISSION_RUN")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env("GH_CONFIG_DIR", self.root.path().join("gh"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("local-state"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("HOME", self.root.path().join("home"))
            .env("ST3_PERSON", actor)
            .args(args);
        if actor.starts_with("agent/") {
            command.env("ST_AGENT", actor);
        }
        command.kill_on_drop(true);
        command
    }

    async fn cli(&self, actor: &str, args: &[&str]) -> Output {
        let mut command = self.cli_command(actor, args);
        let mut output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .unwrap()
            .unwrap();
        if !output.status.success() {
            output
                .stderr
                .extend_from_slice(format!("\ncommand: {args:?}\n").as_bytes());
        }
        output
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

fn cli_value(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

// Call the same typed builders stui uses. The inventory guard below compares this dispatch
// with the generated contract so a newly offered action cannot silently escape the matrix.
async fn dispatch(
    client: &Client,
    kind: &str,
    key: &str,
    fence: Fence,
    parameters: Value,
) -> Result<Envelope<ActionResult>, ClientError> {
    macro_rules! actions {
        ($($name:literal => $method:ident),+ $(,)?) => {
            match kind {
                $($name => client.$method(format!("action/{}", key.replace(':', "-")), key, fence, serde_json::from_value(parameters).unwrap()).await,)+
                _ => panic!("unregistered action {kind}"),
            }
        };
    }
    actions! {
        "agent.create" => agent_create, "agent.queue-move" => agent_queue_move,
        "agent.resume" => agent_resume, "agent.start" => agent_start,
        "agent.stop" => agent_stop, "agent.suspend" => agent_suspend,
        "attention.resolve" => attention_resolve,
        "lane.approve" => lane_approve, "lane.join" => lane_join, "lane.leave" => lane_leave,
        "lane.mark" => lane_mark, "lane.move" => lane_move,
        "launch.approve" => launch_approve, "launch.cancel" => launch_cancel,
        "launch.create" => launch_create, "launch.preview" => launch_preview, "launch.revise" => launch_revise,
        "message.close" => message_close, "message.read" => message_read, "message.send" => message_send,
        "mission.approve-revision" => mission_approve_revision, "mission.cancel" => mission_cancel,
        "mission.cancel-revision" => mission_cancel_revision, "mission.revise" => mission_revise,
        "mission.start" => mission_start, "pairing.revoke" => pairing_revoke,
        "review.approve" => review_approve, "review.reject" => review_reject, "review.request-changes" => review_request_changes,
        "runtime.context-clear" => runtime_context_clear, "runtime.reset" => runtime_reset,
        "runtime.restart" => runtime_restart, "runtime.signal" => runtime_signal, "runtime.stop" => runtime_stop,
        "session.import" => session_import, "terminal.attach" => terminal_attach,
        "terminal.create" => terminal_create, "terminal.detach" => terminal_detach,
        "terminal.end" => terminal_end, "terminal.input" => terminal_input, "terminal.resize" => terminal_resize,
        "work.ask" => work_ask, "work.cancel-ask" => work_cancel_ask,
        "work.claim" => work_claim, "work.complete" => work_complete,
        "work.done" => work_done, "work.fail" => work_fail, "work.progress" => work_progress,
        "work.publish-mission" => work_publish_mission, "work.release" => work_release,
        "work.renew" => work_renew, "work.retry" => work_retry,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_survive_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    let fence = daemon.fence(PERSON).await;
    let sent = daemon
        .exercise(
            PERSON,
            "message.send",
            json!({"to": WORKER, "content": "A copper kite."}),
            fence,
        )
        .await;
    let message = sent["affected_ids"][0].as_str().unwrap().to_owned();
    assert_eq!(
        daemon.store().message(&message).unwrap().unwrap().content,
        "A copper kite."
    );
    // The harness transport stages and delivers before its recipient can mark a message read.
    for lifecycle in ["staged", "delivered"] {
        let _: Value = daemon.transport().post(&format!("/v1/messages/{}/claims", message.trim_start_matches("message/")), &json!({"lifecycle": lifecycle, "actor": WORKER, "idempotency_key": format!("prepare-{lifecycle}")})).await.unwrap();
    }
    for (kind, lifecycle) in [("message.read", "read"), ("message.close", "closed")] {
        let fence = daemon.fence(WORKER).await;
        daemon
            .exercise(WORKER, kind, json!({"target_id": message}), fence)
            .await;
        assert_eq!(
            daemon.store().message(&message).unwrap().unwrap().status,
            lifecycle
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_and_declaration_actions_survive_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    let workspace = daemon.root.path().display().to_string();
    let fence = daemon.fence(PERSON).await;
    let created = daemon
        .exercise(
            PERSON,
            "agent.create",
            json!({"name": "example/worker", "harness": "claude", "workspace": workspace}),
            fence,
        )
        .await;
    let agent = created["affected_ids"][0].as_str().unwrap().to_owned();
    for (kind, expected) in [("agent.stop", "stop"), ("agent.start", "agent")] {
        let mut fence = daemon.fence(PERSON).await;
        fence.runtime_desired_revision = daemon.store().selected_desired_token(&agent).unwrap();
        daemon
            .exercise(PERSON, kind, json!({"agent": agent}), fence)
            .await;
        assert_eq!(
            daemon
                .store()
                .desired_subjects()
                .unwrap()
                .iter()
                .find(|desired| desired.subject == agent)
                .unwrap()
                .kind,
            expected
        );
    }
    let fence = daemon.fence(PERSON).await;
    let terminal = daemon
        .exercise(
            PERSON,
            "terminal.create",
            json!({"name": "Copper shell", "cwd": workspace}),
            fence,
        )
        .await;
    let target = terminal["affected_ids"][0].as_str().unwrap().to_owned();
    let fence = daemon.fence(PERSON).await;
    daemon
        .exercise(PERSON, "terminal.end", json!({"target_id": target}), fence)
        .await;
    assert_eq!(
        daemon
            .store()
            .desired_subjects()
            .unwrap()
            .iter()
            .find(|desired| target == format!("terminal/{}", desired.subject))
            .unwrap()
            .kind,
        "stop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_actions_survive_stale_fences_and_restarts() {
    for end in ["work.complete", "work.fail", "work.release"] {
        let mut daemon = Daemon::new().await;
        daemon.worker();
        daemon.apply(
            r#"version 2
mission "example/work" state="ready" {
  goal "Record the assigned work."
  step "work" timeout="1h" { assigned-to "agent/example/worker" }
}
"#,
            "work-mission",
        );
        let run = daemon.start("example/work", "work-run");
        let step = &run.steps[0].subject;
        daemon.store().set_step_state(step, "ready", None).unwrap();
        for kind in ["work.claim", "work.renew", "work.progress", end] {
            let fence = daemon.work_fence(step).await;
            daemon.exercise(WORKER, kind, json!({"target_id": step, "summary": "Copper proof recorded.", "reason": "The fixture reached its outcome.", "evidence": ["https://example.com/proof"]}), fence).await;
            let work = daemon.store().step_run(step).unwrap().unwrap();
            match kind {
                "work.claim" | "work.renew" => {
                    assert_eq!(work.status, "claimed");
                    assert_eq!(work.claimant.as_deref(), Some(WORKER));
                }
                "work.progress" => assert_eq!(
                    work.progress_summary.as_deref(),
                    Some("Copper proof recorded.")
                ),
                "work.complete" => assert_eq!(work.status, "verifying"),
                "work.fail" => assert_eq!(work.status, "failed"),
                "work.release" => assert_eq!(work.status, "ready"),
                _ => unreachable!(),
            }
        }
        if end == "work.fail" {
            let fence = daemon.work_fence(step).await;
            daemon
                .exercise(
                    PERSON,
                    "work.retry",
                    json!({"target_id": step, "reason": "Try the work again."}),
                    fence,
                )
                .await;
            assert_eq!(
                daemon.store().step_run(step).unwrap().unwrap().status,
                "pending"
            );
            assert_eq!(daemon.store().step_run(step).unwrap().unwrap().attempt, 2);
        }
    }
}

/// Reconcile declarations and agentless human gates; these cases launch no processes.
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

impl Daemon {
    fn reconcile(&self) {
        let reconciler = st3::reconcile::Reconciler::new(
            self.state.store.clone(),
            Arc::new(NoRuntime),
            NODE.into(),
            self.state.notify.clone(),
        );
        for _ in 0..20 {
            reconciler.reconcile_once().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_review_actions_survive_stale_fences_and_restarts() {
    for (kind, mode, verdict) in [
        ("review.approve", "approve", "pass"),
        ("review.reject", "approve", "fail"),
        ("review.request-changes", "feedback", "feedback"),
    ] {
        let mut daemon = Daemon::new().await;
        let worker = if mode == "feedback" {
            daemon.worker();
            "assigned-to \"agent/example/worker\""
        } else {
            "agentless"
        };
        daemon.apply(&format!(r#"version 2
mission "example/review" state="ready" {{
  goal "Answer a human gate."
  step "review" {{ {worker}; gate "accept" type="human" mode={mode:?} {{ reviewer "person/avery"; question "Is the copper proof acceptable?" }} }}
}}
"#), "review-mission");
        let run = daemon.start("example/review", "review-run");
        let step = &run.steps[0].subject;
        if mode == "feedback" {
            daemon.store().set_step_state(step, "ready", None).unwrap();
            for action in ["claim", "complete"] {
                daemon
                    .store()
                    .work_action(
                        step,
                        action,
                        &st3::model::WorkRequest {
                            actor: Some(WORKER.into()),
                            incarnation: Some("4242:fixture".into()),
                            summary: Some("Drafted the copper proof.".into()),
                            reason: None,
                            evidence: Vec::new(),
                            idempotency_key: format!("prepare-review-{action}"),
                        },
                    )
                    .unwrap();
            }
        }
        daemon.reconcile();
        let request = daemon
            .store()
            .gate_request_for_owner(step)
            .unwrap()
            .unwrap();
        let attention: Value = daemon
            .transport()
            .get("/v1/client/attention")
            .await
            .unwrap();
        let card = attention["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["source_id"] == *step)
            .unwrap();
        let mut fence = daemon.fence(PERSON).await;
        fence.subject_revisions.insert(
            card["id"].as_str().unwrap().into(),
            card["revision"].as_str().unwrap().into(),
        );
        daemon
            .exercise(
                PERSON,
                kind,
                json!({"target_id": step, "reason": "Copper proof was reviewed."}),
                fence,
            )
            .await;
        let results = daemon
            .store()
            .claims_for(&request.subject, Some("gate.result"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].body["fields"]["verdict"], verdict);
        assert_eq!(results[0].actor.as_deref(), Some(PERSON));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_actions_survive_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply(
        r#"version 2
mission "example/lane" state="ready" {
  goal "Keep an ordered lane."
  lane "changes" { entries "resource/github/fixture/app/ci/pull-request/"; approver "person/avery" }
  step "drive" { assigned-to "agent/example/worker" }
}
"#,
        "lane-mission",
    );
    let run = daemon.start("example/lane", "lane-run");
    daemon.reconcile();
    let lane = format!("lane/{}/changes", run.id);
    let first = "resource/github/fixture/app/ci/pull-request/11";
    let second = "resource/github/fixture/app/ci/pull-request/12";
    let fence = daemon.fence(PERSON).await;
    daemon
        .exercise(
            PERSON,
            "lane.join",
            json!({"lane_id": lane, "entry_id": first, "reason": "Add the copper change."}),
            fence,
        )
        .await;
    let fence = daemon.fence(PERSON).await;
    dispatch(
        &daemon.client(PERSON),
        "lane.join",
        "coverage:prepare-second-lane-entry",
        fence,
        json!({"lane_id": lane, "entry_id": second}),
    )
    .await
    .unwrap();
    for (kind, extra) in [
        ("lane.approve", json!({})),
        (
            "lane.mark",
            json!({"state": "ready", "detail": "Checks passed."}),
        ),
        (
            "lane.move",
            json!({"placement": "after", "anchor_id": second}),
        ),
        ("lane.leave", json!({"outcome": "completed"})),
    ] {
        let mut parameters = json!({"lane_id": lane, "entry_id": first});
        parameters
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let fence = daemon.fence(PERSON).await;
        daemon.exercise(PERSON, kind, parameters, fence).await;
        let current: Value = daemon
            .transport()
            .get(&format!("/v1/client/lanes/{lane}"))
            .await
            .unwrap();
        let entries = current["entries"].as_array().unwrap();
        match kind {
            "lane.approve" => assert_eq!(entries[0]["approved_by_id"], PERSON),
            "lane.mark" => assert_eq!(entries[0]["state"], "ready"),
            "lane.move" => assert_eq!(entries[1]["entry_id"], first),
            "lane.leave" => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0]["entry_id"], second);
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_actions_survive_stale_fences_and_restarts() {
    for end in ["launch.approve", "launch.revise", "launch.cancel"] {
        let mut daemon = Daemon::new().await;
        let fence = daemon.fence(PERSON).await;
        let created = daemon.exercise(PERSON, "launch.create", json!({"title": "Copper launch", "request": "Prepare one step to record proof.", "target": {"type": "new-mission", "mission_id": "mission/example/launch", "workspace": daemon.root.path().display().to_string()}}), fence).await;
        let launch = created["affected_ids"][0].as_str().unwrap().to_owned();
        let id = launch.trim_start_matches("launch/");
        let session = daemon.store().planning_session(id).unwrap().unwrap();
        assert_eq!(session.requester, PERSON);
        let _: Value = daemon.transport().post(&format!("/v1/launches/{id}/variants/default/submit"), &st3::model::PlanningCandidateSubmitRequest {
            actor: session.planner, markdown: b"# Copper proof\n".to_vec(),
            kdl: b"version 2\nmission \"example/launch\" state=\"ready\" { goal \"Record the proof.\"; step \"proof\" { agentless } }\n".to_vec(),
            idempotency_key: "coverage:prepare-launch-candidate".into(),
        }).await.unwrap();
        let current: Value = daemon
            .transport()
            .get(&format!("/v1/client/launches/{id}"))
            .await
            .unwrap();
        let mut fence = daemon.fence(PERSON).await;
        fence
            .subject_revisions
            .insert(launch.clone(), current["revision"].as_str().unwrap().into());
        daemon
            .exercise(
                PERSON,
                "launch.preview",
                json!({"launch_id": launch, "variant_id": format!("launch-variant/{id}/default")}),
                fence,
            )
            .await;
        let current: Value = daemon
            .transport()
            .get(&format!("/v1/client/launches/{id}"))
            .await
            .unwrap();
        let mut fence = daemon.fence(PERSON).await;
        fence
            .subject_revisions
            .insert(launch.clone(), current["revision"].as_str().unwrap().into());
        let parameters = match end {
            "launch.approve" => {
                let variant: Value = daemon
                    .transport()
                    .get(&format!("/v1/client/launches/{id}/variants/default"))
                    .await
                    .unwrap();
                fence.preview_token = variant["preview_token"].as_str().map(str::to_owned);
                json!({"launch_id": launch, "variant_id": format!("launch-variant/{id}/default")})
            }
            "launch.revise" => {
                json!({"launch_id": launch, "feedback": "Name the evidence explicitly."})
            }
            "launch.cancel" => json!({"target_id": launch, "reason": "No launch is needed."}),
            _ => unreachable!(),
        };
        daemon.exercise(PERSON, end, parameters, fence).await;
        let session = daemon.store().planning_session(id).unwrap().unwrap();
        assert_eq!(
            session.status,
            match end {
                "launch.approve" => "approved",
                "launch.revise" => "revision-requested",
                _ => "cancelled",
            }
        );
        if end == "launch.approve" {
            assert!(
                daemon
                    .store()
                    .mission_definitions()
                    .unwrap()
                    .iter()
                    .any(|definition| definition.mission.id == "example/launch")
            );
            assert!(
                daemon
                    .store()
                    .active_mission_runs()
                    .unwrap()
                    .iter()
                    .all(|run| run.mission != "mission/example/launch")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seat_queue_moves_survive_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply(r#"version 2
mission "example/queue" state="ready" { concurrent-runs; goal "Order a seat's work."; step "work" { assigned-to "agent/example/worker" } }
"#, "queue-mission");
    let first = daemon.start("example/queue", "queue-first");
    let second = daemon.start("example/queue", "queue-second");
    for run in [&first, &second] {
        daemon
            .store()
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap();
    }
    let fence = daemon.fence(PERSON).await;
    daemon.exercise(PERSON, "agent.queue-move", json!({"agent_id": WORKER, "mission_run_id": second.subject, "placement": "before", "anchor_run_id": first.subject}), fence).await;
    let queue: Value = daemon
        .transport()
        .get(&format!("/v1/client/agent-queues/{WORKER}"))
        .await
        .unwrap();
    assert_eq!(queue["runs"][0]["mission_run_id"], second.subject);
    assert_eq!(queue["runs"][1]["mission_run_id"], first.subject);
    assert_eq!(queue["move_count"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_revocation_survives_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    let challenge: Value = daemon.transport().post("/v1/client/pairings", &json!({"api_version": "st3.client.v0", "device_name": "Copper phone", "person_id": PERSON, "full_control": true})).await.unwrap();
    let pairing = challenge["pairing_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("pairing/");
    daemon.restart().await;
    let paired: Value = daemon.transport().post(&format!("/v1/client/pairings/{pairing}/complete"), &json!({"api_version": "st3.client.v0", "code": challenge["code"], "device_public_key": "copper-phone-key-000000000000000000000000"})).await.unwrap();
    let credential = paired["credential"].as_str().unwrap();
    Client::unix_gateway(daemon.socket(), credential)
        .capabilities()
        .await
        .unwrap();
    let fence = daemon.fence(PERSON).await;
    daemon
        .exercise(
            PERSON,
            "pairing.revoke",
            json!({"target_id": paired["device_id"]}),
            fence,
        )
        .await;
    let error = Client::unix_gateway(daemon.socket(), credential)
        .capabilities()
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Api(ErrorCode::Forbidden, ..)),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn person_work_actions_survive_stale_fences_and_restarts() {
    for kind in ["work.done", "work.cancel-ask"] {
        let mut daemon = Daemon::new().await;
        daemon.worker();
        let fence = daemon.fence(WORKER).await;
        let asked = daemon.exercise(WORKER, "work.ask", json!({"person_id": PERSON, "title": "Choose the fixture outcome", "reason": "Only its requester can answer.", "new_run": "coverage-ask"}), fence).await;
        let step = asked["affected_ids"][0].as_str().unwrap().to_owned();
        let attention: Value = daemon
            .transport()
            .get(&format!("/v1/client/attention?actor={PERSON}"))
            .await
            .unwrap();
        let card = attention["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["source_id"] == step)
            .unwrap();
        let actor = if kind == "work.done" { PERSON } else { WORKER };
        let fence = daemon.fence(actor).await;
        daemon.exercise(actor, kind, json!({"target_id": step, "episode": card["episode"], "summary": "The choice is recorded."}), fence).await;
        let work = daemon.store().step_run(&step).unwrap().unwrap();
        assert_eq!(
            work.status,
            if kind == "work.done" {
                "completed"
            } else {
                "cancelled"
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mission_actions_survive_stale_fences_and_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.apply(r#"version 2
mission "example/cancel" state="ready" { concurrent-runs; goal "Exercise a run cancellation."; step "wait" { agentless } }
"#, "cancel-mission");
    let fence = daemon.fence(PERSON).await;
    let started = daemon.exercise(PERSON, "mission.start", json!({"mission_id": "mission/example/cancel", "workspace": daemon.root.path().display().to_string(), "inputs": {}}), fence).await;
    let run_id = started["affected_ids"][0].as_str().unwrap().to_owned();
    let run = daemon.store().mission_run(&run_id).unwrap().unwrap();
    assert_eq!(run.requester, PERSON);
    let mut fence = daemon.fence(PERSON).await;
    fence.mission_generation = Some(run.generation);
    daemon
        .exercise(
            PERSON,
            "mission.cancel",
            json!({"target_id": run_id, "reason": "No further work is needed."}),
            fence,
        )
        .await;
    let run = daemon.store().mission_run(&run_id).unwrap().unwrap();
    assert_eq!(run.phase, "cleanup-cancelled");
    assert!(run.steps.iter().all(|step| step.status == "cancelled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_import_survives_stale_discovery_and_restarts() {
    let mut daemon = Daemon::new().await;
    let transcript = daemon
        .root
        .path()
        .join("native/.codex/sessions/2026/09/21/import.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, format!("{}\n", json!({"type": "session_meta", "timestamp": "2026-09-21T08:00:00Z", "payload": {"id": "copper-native-session", "cwd": daemon.root.path(), "source": "test"}}))).unwrap();
    let sessions: Value = daemon
        .transport()
        .get("/v1/client/sessions?native_only=true&history=true")
        .await
        .unwrap();
    let external = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["native_session_id"] == "copper-native-session")
        .unwrap_or_else(|| panic!("{sessions}"));
    let external_id = external["id"].as_str().unwrap().to_owned();
    let mut fence = daemon.fence(PERSON).await;
    fence.subject_revisions.insert(
        external_id.clone(),
        external["revision"].as_str().unwrap().into(),
    );
    let mut stale = fence.clone();
    stale
        .subject_revisions
        .insert(external_id.clone(), "old-discovery".into());
    let index = daemon.store().index().unwrap();
    assert!(matches!(
        dispatch(
            &daemon.client(PERSON),
            "session.import",
            "coverage:import:old",
            stale,
            json!({"target_id": external_id})
        )
        .await,
        Err(ClientError::Api(ErrorCode::StaleFence, ..))
    ));
    assert_eq!(daemon.store().index().unwrap(), index);
    let result = daemon
        .exercise(
            PERSON,
            "session.import",
            json!({"target_id": external_id}),
            fence,
        )
        .await;
    let agent = result["affected_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|id| id.starts_with("agent/import/"))
        .unwrap();
    let desired = daemon
        .store()
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == agent)
        .unwrap();
    assert!(
        serde_json::to_string(&desired)
            .unwrap()
            .contains("copper-native-session")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revision_decisions_survive_stale_previews_and_restarts() {
    for kind in ["mission.approve-revision", "mission.cancel-revision"] {
        let mut daemon = Daemon::new().await;
        let source = |goal: &str| {
            format!(
                "version 2\nmission \"example/revision\" state=\"ready\" revisions=\"human-only\" revision-reviewer={PERSON:?} {{ goal \"Review a revision.\"; step \"work\" {{ agentless; goal {goal:?} }} }}\n"
            )
        };
        daemon.apply(&source("Original goal."), "revision-original");
        let run = daemon.start("example/revision", "revision-run");
        let candidate = source("Revised goal.");
        daemon.apply(&candidate, "revision-candidate");
        let intent = st3::parse_intent(&candidate, NODE).unwrap();
        let proposal = daemon
            .store()
            .create_revision_proposal(
                &run.id,
                &intent.missions["example/revision"],
                PERSON,
                "Clarify the goal.",
                "coverage-revision-proposal",
            )
            .unwrap();
        assert_eq!(proposal.status, "pending-approval");
        let mut fence = daemon.fence(PERSON).await;
        fence.mission_generation = Some(proposal.source_generation.clone());
        fence.preview_token = proposal.preview_hash;
        if kind == "mission.approve-revision" {
            let mut stale = fence.clone();
            stale.preview_token = Some("old-preview".into());
            let index = daemon.store().index().unwrap();
            let error = dispatch(
                &daemon.client(PERSON),
                kind,
                "coverage:revision:old",
                stale,
                json!({"target_id": proposal.subject}),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ClientError::Api(ErrorCode::StaleFence, ..)),
                "{error}"
            );
            assert_eq!(daemon.store().index().unwrap(), index);
        }
        daemon
            .exercise(
                PERSON,
                kind,
                json!({"target_id": proposal.subject, "reason": "Decided the proposal."}),
                fence,
            )
            .await;
        let decided = daemon
            .store()
            .revision_proposal(&proposal.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            decided.status,
            if kind == "mission.approve-revision" {
                "applied"
            } else {
                "cancelled"
            }
        );
        if kind == "mission.approve-revision" {
            assert_eq!(decided.approvals, [PERSON]);
            assert_ne!(
                daemon
                    .store()
                    .mission_run(&run.id)
                    .unwrap()
                    .unwrap()
                    .generation,
                run.generation
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspension_requests_survive_stale_incarnations_and_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.claim(WORKER, "harness.session-file", json!({"harness": "claude", "session_id": "copper-suspended-native", "agent": WORKER, "incarnation_id": "4242:fixture", "status": "active"}));
    daemon.claim(WORKER, "harness.observed", json!({"state": "idle", "incarnation_id": "4242:fixture", "quiescent": true, "blocking": []}));
    let mut fence = daemon.fence(PERSON).await;
    fence.runtime_desired_revision = daemon.store().selected_desired_token(WORKER).unwrap();
    fence.runtime_incarnation = Some("4242:fixture".into());
    daemon
        .exercise(
            PERSON,
            "agent.suspend",
            json!({"agent": WORKER, "reason": "Suspend the quiet seat."}),
            fence,
        )
        .await;
    let suspension = st3::suspension::current(daemon.store(), WORKER)
        .unwrap()
        .unwrap();
    assert_eq!(suspension.phase, "quiescing");
    // Fixture owner acknowledgements: the runtime canary separately exercises actual native
    // process shutdown/resume. Here the public client request and its durable receipt are tested.
    for (key, phase) in [
        (
            st3::suspension::suspend_snapshot_key(&suspension.operation_id),
            "snapshotting",
        ),
        (
            st3::suspension::suspend_completed_key(&suspension.operation_id),
            "suspended",
        ),
    ] {
        daemon.store().append_claim(&ClaimInput {
            subject: WORKER.into(), kind: "runtime.action.succeeded".into(), actor: Some(PERSON.into()),
            fields: serde_json::from_value(json!({"action": "suspend", "operation_status": phase, "harness": "claude", "native_session_id": "copper-suspended-native"})).unwrap(),
            evidence: vec![suspension.operation_id.clone()], expected_subject: None, idempotency_key: Some(key),
        }).unwrap();
    }
    assert_eq!(
        st3::suspension::current(daemon.store(), WORKER)
            .unwrap()
            .unwrap()
            .phase,
        "suspended"
    );
    let mut fence = daemon.fence(PERSON).await;
    fence.runtime_desired_revision = daemon.store().selected_desired_token(WORKER).unwrap();
    daemon
        .exercise(PERSON, "agent.resume", json!({"agent": WORKER}), fence)
        .await;
    let resumed = st3::suspension::current(daemon.store(), WORKER)
        .unwrap()
        .unwrap();
    assert_eq!(resumed.phase, "restoring");
    assert_eq!(
        resumed.native_session_id.as_deref(),
        Some("copper-suspended-native")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_actions_refuse_before_and_after_restart_without_writes() {
    let mut daemon = Daemon::new().await;
    for (kind, parameters, code) in [
        (
            "attention.resolve",
            json!({"attention_id": "attention/retired", "outcome": "resolved", "reason": "This command is retired."}),
            ErrorCode::AttentionMigrated,
        ),
        (
            "mission.revise",
            json!({"mission_run_id": "mission-run/fixture", "launch_id": "launch/fixture"}),
            ErrorCode::UnsupportedCapability,
        ),
        (
            "work.publish-mission",
            json!({"target_id": "step-run/fixture/work", "name": "fixture", "mission": {}}),
            ErrorCode::UnsupportedCapability,
        ),
    ] {
        let index = daemon.store().index().unwrap();
        for _ in 0..2 {
            let fence = daemon.fence(PERSON).await;
            let error = dispatch(
                &daemon.client(PERSON),
                kind,
                &format!("coverage:{kind}:refused"),
                fence,
                parameters.clone(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ClientError::Api(ref actual, ..) if actual == &code),
                "{kind}: {error}"
            );
            assert_eq!(daemon.store().index().unwrap(), index);
            daemon.restart().await;
        }
    }
}

struct PtyFixture {
    runtime: st_runtime::PtyRuntime,
    id: String,
    incarnation: String,
}
impl Drop for PtyFixture {
    fn drop(&mut self) {
        let _ = self.runtime.stop(&self.id);
        let _ = self.runtime.remove(&self.id);
    }
}
impl Daemon {
    async fn pty(&self, owner: &str) -> PtyFixture {
        let runtime = st_runtime::PtyRuntime::new(self.state.pty_root.clone()).with_binary("pty");
        let id = self
            .store()
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == owner)
            .unwrap()
            .member
            .unwrap()
            .runtime_id;
        let output = tokio::process::Command::new("pty").env("PTY_ROOT", &self.state.pty_root)
            .args(["run", "-d", "--force", "--id", &id, "--tag", "keep=true", "--", "/bin/sh", "-c", "stty -echo; trap 'printf signal-received' USR1; printf ready; while :; do if IFS= read -r line; then printf '\\r\\naccepted:%s' \"$line\"; fi; done"])
            .output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let live = runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|live| live.name == id)
            .unwrap();
        let incarnation = format!("{}:{}", live.pid.unwrap(), live.created_at.unwrap());
        self.claim(owner, "runtime.observed", json!({"status": "running", "runtime_id": id, "incarnation_id": incarnation, "terminal": true}));
        let fixture = PtyFixture {
            runtime,
            id,
            incarnation,
        };
        self.screen_containing(owner, "ready").await;
        fixture
    }

    async fn screen_containing(&self, owner: &str, text: &str) -> st3_client::TerminalScreen {
        for _ in 0..100 {
            let screen = self
                .client(PERSON)
                .terminal_screen(&format!("terminal/{owner}"))
                .await
                .unwrap()
                .value;
            if serde_json::to_string(&screen).unwrap().contains(text) {
                return screen;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("terminal {owner} did not show {text}");
    }

    async fn terminal_fence(&self, owner: &str, pty: &PtyFixture, sequence: bool) -> Fence {
        let mut fence = self.fence(PERSON).await;
        fence.runtime_incarnation = Some(pty.incarnation.clone());
        if sequence {
            fence.terminal_sequence = Some(
                self.client(PERSON)
                    .terminal_screen(&format!("terminal/{owner}"))
                    .await
                    .unwrap()
                    .value
                    .next_sequence,
            );
        }
        fence
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_controls_survive_stale_screens_and_daemon_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    let pty = daemon.pty(WORKER).await;
    let target = format!("terminal/{WORKER}");
    let fence = daemon.terminal_fence(WORKER, &pty, false).await;
    let attached = daemon
        .exercise(
            PERSON,
            "terminal.attach",
            json!({"target_id": target}),
            fence,
        )
        .await;
    let attachment = attached["affected_ids"][0].as_str().unwrap();
    assert!(
        attached["terminal_attachment"]["stream_capability"].is_string(),
        "{attached}"
    );
    let fence = daemon.terminal_fence(WORKER, &pty, true).await;
    let mut stale = fence.clone();
    stale.terminal_sequence = Some(fence.terminal_sequence.unwrap().saturating_sub(1));
    // Force an actually different sequence even if the first screen has sequence zero.
    if stale.terminal_sequence == fence.terminal_sequence {
        stale.terminal_sequence = Some(u64::MAX);
    }
    let index = daemon.store().index().unwrap();
    assert!(matches!(
        dispatch(
            &daemon.client(PERSON),
            "terminal.input",
            "coverage:terminal:old-screen",
            stale,
            json!({"terminal_id": target, "mode": "line", "value": "must-not-appear"})
        )
        .await,
        Err(ClientError::Api(ErrorCode::StaleFence, ..))
    ));
    assert_eq!(daemon.store().index().unwrap(), index);
    daemon
        .exercise(
            PERSON,
            "terminal.input",
            json!({"terminal_id": target, "mode": "line", "value": "copper-input"}),
            fence,
        )
        .await;
    daemon
        .screen_containing(WORKER, "accepted:copper-input")
        .await;
    let fence = daemon.terminal_fence(WORKER, &pty, true).await;
    daemon
        .exercise(
            PERSON,
            "terminal.resize",
            json!({"terminal_id": target, "rows": 30, "columns": 100}),
            fence,
        )
        .await;
    let screen = daemon.screen_containing(WORKER, "copper-input").await;
    assert_eq!((screen.rows, screen.columns), (30, 100));
    let fence = daemon.terminal_fence(WORKER, &pty, false).await;
    daemon
        .exercise(
            PERSON,
            "terminal.detach",
            json!({"target_id": attachment}),
            fence,
        )
        .await;
    let detached: Value = daemon
        .transport()
        .get(&format!("/v1/client/terminal-attachments/{attachment}"))
        .await
        .unwrap_or(Value::Null);
    assert!(detached.is_null() || detached["status"] != "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_controls_survive_stale_incarnations_and_restarts() {
    for kind in [
        "runtime.context-clear",
        "runtime.signal",
        "runtime.restart",
        "runtime.stop",
    ] {
        let mut daemon = Daemon::new().await;
        daemon.worker();
        let pty = daemon.pty(WORKER).await;
        let mut fence = daemon.terminal_fence(WORKER, &pty, false).await;
        fence.runtime_desired_revision = daemon.store().selected_desired_token(WORKER).unwrap();
        let parameters = if kind == "runtime.signal" {
            json!({"target_id": WORKER, "signal": "user-1"})
        } else {
            json!({"target_id": WORKER, "reason": "Control the fixture runtime."})
        };
        let mut stale = fence.clone();
        stale.runtime_incarnation = Some("1:old-runtime".into());
        let index = daemon.store().index().unwrap();
        let error = dispatch(
            &daemon.client(PERSON),
            kind,
            "coverage:runtime:old-incarnation",
            stale,
            parameters.clone(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("stale") || error.to_string().contains("incarnation"),
            "{kind}: {error}"
        );
        assert_eq!(daemon.store().index().unwrap(), index);
        daemon.exercise(PERSON, kind, parameters, fence).await;
        match kind {
            "runtime.context-clear" => {
                daemon.screen_containing(WORKER, "accepted:/clear").await;
            }
            "runtime.signal" => {
                daemon.screen_containing(WORKER, "signal-received").await;
            }
            "runtime.restart" => {
                // The action acknowledges TERM delivery; the PTY publishes exit afterward.
                // Observe that outcome rather than assuming signal delivery is a join.
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if pty.runtime.snapshot().unwrap().into_iter()
                            .all(|live| live.name != pty.id || live.status != "running") {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.expect("the restarted fixture runtime must exit after TERM");
            }
            "runtime.stop" => assert_eq!(
                daemon
                    .store()
                    .selected_desired_kind(WORKER)
                    .unwrap()
                    .as_deref(),
                Some("stop")
            ),
            _ => unreachable!(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_runtime_reset_survives_stale_desired_revision_and_restarts() {
    let mut daemon = Daemon::new().await;
    daemon.apply(&format!("version 2\nmission \"example/reset\" state=\"ready\" {{ goal \"Reset a runtime.\"; agent \"worker\" {{ workspace {:?}; command \"true\"; restart always }}; step \"hold\" {{ assigned-to \"agent/worker\" }} }}\n", daemon.root.path()), "reset-mission");
    let run = daemon.start("example/reset", "reset-run");
    daemon.reconcile();
    let owner_id = format!("agent/{}/worker", run.id);
    let owner = owner_id.as_str();
    daemon.claim(owner, "runtime.observed", json!({"status": "running", "runtime_id": "copper-reset", "incarnation_id": "4242:fixture", "terminal": false}));
    let mut fence = daemon.fence(PERSON).await;
    fence.runtime_incarnation = Some("4242:fixture".into());
    fence.runtime_desired_revision = daemon.store().selected_desired_token(owner).unwrap();
    let mut stale = fence.clone();
    stale.runtime_desired_revision = Some("old-desired".into());
    let index = daemon.store().index().unwrap();
    assert!(matches!(
        dispatch(
            &daemon.client(PERSON),
            "runtime.reset",
            "coverage:reset:old-desired",
            stale,
            json!({"target_id": owner, "reason": "Reset fixture limits."})
        )
        .await,
        Err(ClientError::Api(ErrorCode::StaleFence, ..))
    ));
    assert_eq!(daemon.store().index().unwrap(), index);
    daemon
        .exercise(
            PERSON,
            "runtime.reset",
            json!({"target_id": owner, "reason": "Reset fixture limits."}),
            fence,
        )
        .await;
    let reset = daemon
        .store()
        .latest_claim(owner, Some("runtime.restart-window-reset"))
        .unwrap()
        .unwrap();
    assert_eq!(reset.body["fields"]["reason"], "Reset fixture limits.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glasses_save_delete_and_read_survive_stale_bases_and_restarts() {
    let mut daemon = Daemon::new().await;
    let id = "019a0000-0000-7000-8000-000000000042";
    let body = json!({"name": "Copper workspace", "layout": {"tabs": [{"title": "Home", "pane": "home:"}]}});
    let request: st3_client::GlassPut =
        serde_json::from_value(json!({"body": body, "base_revision": null})).unwrap();
    daemon.restart().await;
    let saved = daemon
        .client(PERSON)
        .put_glass(id, &request, "coverage:glass:create")
        .await
        .unwrap()
        .value;
    let index = daemon.store().index().unwrap();
    daemon.restart().await;
    let replay = daemon
        .client(PERSON)
        .put_glass(id, &request, "coverage:glass:create")
        .await
        .unwrap()
        .value;
    assert_eq!(replay.header.revision, saved.header.revision);
    assert_eq!(daemon.store().index().unwrap(), index);
    let stale: st3_client::GlassPut =
        serde_json::from_value(json!({"body": body, "base_revision": "old-base"})).unwrap();
    // Glass bases record which edit was replaced: concurrent edits intentionally remain
    // last-writer-wins, unlike execution fences. The response makes that replacement visible.
    let replaced = daemon
        .client(PERSON)
        .put_glass(id, &stale, "coverage:glass:stale-base")
        .await
        .unwrap()
        .value;
    assert_eq!(
        replaced.replaced_revision.as_deref(),
        Some(saved.header.revision.as_str())
    );
    assert_eq!(replaced.base_revision.as_deref(), Some("old-base"));
    let request = st3_client::GlassDelete {
        base_revision: Some(replaced.header.revision),
    };
    daemon.restart().await;
    let deleted = daemon
        .client(PERSON)
        .delete_glass(id, &request, "coverage:glass:delete")
        .await
        .unwrap()
        .value;
    assert!(deleted.deleted);
    let index = daemon.store().index().unwrap();
    daemon.restart().await;
    assert!(
        daemon
            .client(PERSON)
            .delete_glass(id, &request, "coverage:glass:delete")
            .await
            .unwrap()
            .value
            .deleted
    );
    assert_eq!(daemon.store().index().unwrap(), index);
    assert!(matches!(
        daemon.client(PERSON).get_glass(id).await,
        Err(ClientError::Api(ErrorCode::NotFound, ..))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_reads_preserve_operational_views_after_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply("version 2\nmission \"example/inspect\" state=\"ready\" { goal \"Inspect the graph.\"; step \"work\" { assigned-to \"agent/example/worker\" } }\n", "inspect-mission");
    let run = daemon.start("example/inspect", "inspect-run");
    daemon
        .store()
        .set_step_state(&run.steps[0].subject, "ready", None)
        .unwrap();
    let commands: Vec<Vec<&str>> = vec![
        vec!["now", "--as", PERSON],
        vec!["agents", "ls"],
        vec!["agents", "tree"],
        vec!["agents", "repos"],
        vec!["agents", "repos", "--host", "host/other"],
        vec!["agents", "show", WORKER],
        vec!["agents", "queue", WORKER],
        vec!["missions", "ls"],
        vec!["missions", "tree"],
        vec!["missions", "show", &run.subject],
        vec!["missions", "queued", WORKER],
        vec!["attention", "ls", "--as", PERSON],
        vec!["work", "ls", "--as", WORKER],
        vec!["work", "show", &run.steps[0].subject],
        vec!["work", "revision", "generations", &run.subject],
        vec!["work", "revision", "generation", &run.generation],
        vec!["usage"],
        vec!["terminals", "ls"],
        vec!["machines"],
        vec!["launch", "ls"],
        vec!["lanes", "ls"],
        vec!["devices", "ls"],
        vec!["fleet", "status"],
        vec!["fleet", "invites"],
        vec!["replication", "status"],
        vec!["replication", "invalid"],
        vec!["replication", "checkpoint", "status"],
        vec!["rules", "ls"],
        vec!["rules", "audit"],
        vec!["repair", "dry-run"],
        vec!["schema", "subjects"],
        vec!["schema", "resources"],
        vec!["schema", "claims"],
        vec!["schema", "show", "runtime.observed"],
        vec!["schema", "export"],
        vec!["subject", "show", WORKER],
        vec!["subject", "history", WORKER],
        vec!["documents", "ls"],
        vec!["activity"],
        vec!["import", "ls"],
        vec!["conversations", "ls", WORKER],
        vec!["conversations", "sessions"],
        vec!["trace", "show", &run.subject],
        vec![
            "trace",
            "wait",
            &run.steps[0].subject,
            "--for",
            "ready",
            "--timeout",
            "1s",
        ],
        vec!["devices"],
        vec!["recorder", "report"],
    ];
    for _ in 0..2 {
        for command in &commands {
            let output = daemon.cli(PERSON, command).await;
            assert!(
                output.status.success(),
                "{command:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!output.stdout.is_empty(), "{command:?}: no view");
        }
        daemon.restart().await;
        assert_eq!(
            daemon
                .store()
                .step_run(&run.steps[0].subject)
                .unwrap()
                .unwrap()
                .status,
            "ready"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_metadata_documents_blobs_and_rules_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    cli_value(
        daemon
            .cli(PERSON, &["agents", "rename", WORKER, "Copper worker"])
            .await,
    );
    cli_value(
        daemon
            .cli(
                WORKER,
                &[
                    "claim",
                    "custom/coverage/note",
                    "custom.coverage.note",
                    "--actor",
                    WORKER,
                    "--field",
                    "text=durable",
                ],
            )
            .await,
    );
    cli_value(
        daemon
            .cli(
                WORKER,
                &[
                    "diagnostic",
                    "--as",
                    WORKER,
                    "--code",
                    "fixture-diagnostic",
                    "--reason",
                    "A fixture warning.",
                    "--severity",
                    "warning",
                    "--incarnation",
                    "4242:fixture",
                ],
            )
            .await,
    );
    let file = daemon.root.path().join("proof.txt");
    std::fs::write(&file, "Copper proof.\n").unwrap();
    let document = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "documents",
                    "put",
                    file.to_str().unwrap(),
                    "--as",
                    "doc/example/proof",
                ],
            )
            .await,
    );
    let image = daemon.root.path().join("proof.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nCopper proof").unwrap();
    let blob = cli_value(
        daemon
            .cli(PERSON, &["blobs", "put", image.to_str().unwrap()])
            .await,
    );
    daemon.restart().await;
    let read = daemon
        .cli(PERSON, &["documents", "get", "doc/example/proof"])
        .await;
    assert!(
        read.status.success(),
        "{}: {document}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("Copper proof"));
    let reference = blob["blob"].as_str().unwrap_or_else(|| panic!("{blob}"));
    let read = daemon.cli(PERSON, &["blobs", "get", reference]).await;
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("Copper proof"));
    let subjects: Value = daemon
        .transport()
        .get(&format!("/v1/client/agents/{WORKER}"))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&subjects)
            .unwrap()
            .contains("Copper worker")
    );
    cli_value(
        daemon
            .cli(
                PERSON,
                &["rules", "lockdown", "--as", PERSON, "--starter", WORKER],
            )
            .await,
    );
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "rules",
                    "mode",
                    "agents-create-no-missions",
                    "off",
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    daemon.restart().await;
    let audit = cli_value(daemon.cli(PERSON, &["rules", "audit"]).await);
    assert!(audit.is_object() || audit.is_array());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_missions_publish_cancel_outcome_retire_and_work_leases_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    let file = daemon.root.path().join("work.kdl");
    std::fs::write(&file, "version 2\nmission \"example/cli-work\" state=\"ready\" { concurrent-runs; goal \"Record the CLI work.\"; step \"work\" timeout=\"1h\" { assigned-to \"agent/example/worker\" } }\n").unwrap();
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "missions",
                    "publish",
                    file.to_str().unwrap(),
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "missions",
                    "start",
                    "example/cli-work",
                    "--id",
                    "coverage-cli-run",
                    "--workspace",
                    daemon.root.path().to_str().unwrap(),
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    let run = daemon
        .store()
        .mission_run("coverage-cli-run")
        .unwrap()
        .unwrap();
    let step = &run.steps[0].subject;
    daemon.store().set_step_state(step, "ready", None).unwrap();
    // An unclaimed step must still reject an obsolete caller incarnation without writes.
    let mut stale = daemon.work_fence(step).await;
    stale.runtime_incarnation = Some("1:old-runtime".into());
    let index = daemon.store().index().unwrap();
    let error = dispatch(
        &daemon.client(WORKER),
        "work.claim",
        "coverage:claim:old-runtime",
        stale,
        json!({"target_id": step}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("incarnation"), "{error}");
    assert_eq!(daemon.store().index().unwrap(), index);
    for action in ["claim", "renew", "progress", "release", "claim", "fail"] {
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    WORKER,
                    &[
                        "work",
                        action,
                        step,
                        "--as",
                        WORKER,
                        "--incarnation",
                        "4242:fixture",
                        "--summary",
                        "Copper CLI proof.",
                        "--reason",
                        "Recorded the fixture.",
                    ],
                )
                .await,
        );
        if action == "progress" {
            daemon.restart().await;
            cli_value(
                daemon
                    .cli(
                        WORKER,
                        &[
                            "work",
                            "extend",
                            step,
                            "--as",
                            WORKER,
                            "--incarnation",
                            "4242:fixture",
                            "--by",
                            "1h",
                            "--reason",
                            "The fixture needs another hour.",
                        ],
                    )
                    .await,
            );
            assert_eq!(
                daemon
                    .store()
                    .step_run(step)
                    .unwrap()
                    .unwrap()
                    .timeout_extension_ms,
                3_600_000
            );
        }
    }
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "work",
                    "retry",
                    step,
                    "--as",
                    PERSON,
                    "--reason",
                    "One more attempt.",
                ],
            )
            .await,
    );
    assert_eq!(daemon.store().step_run(step).unwrap().unwrap().attempt, 2);
    daemon.store().set_step_state(step, "ready", None).unwrap();
    for action in ["claim", "complete"] {
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    WORKER,
                    &[
                        "work",
                        action,
                        step,
                        "--as",
                        WORKER,
                        "--incarnation",
                        "4242:fixture",
                        "--summary",
                        "Copper CLI proof.",
                    ],
                )
                .await,
        );
    }
    assert_eq!(
        daemon.store().step_run(step).unwrap().unwrap().status,
        "verifying"
    );
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "missions",
                    "cancel",
                    &run.subject,
                    "--as",
                    PERSON,
                    "--reason",
                    "Finished checking the fixture.",
                ],
            )
            .await,
    );
    daemon.reconcile();
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "missions",
                    "outcome",
                    &run.subject,
                    "completed",
                    "--as",
                    PERSON,
                    "--reason",
                    "The proof was independently checked.",
                ],
            )
            .await,
    );
    daemon.restart().await;
    assert_eq!(
        daemon.store().mission_run(&run.id).unwrap().unwrap().status,
        "completed"
    );
    cli_value(
        daemon
            .cli(
                PERSON,
                &["missions", "retire", "example/cli-work", "--as", PERSON],
            )
            .await,
    );
    daemon.restart().await;
    let output = daemon
        .cli(
            PERSON,
            &["missions", "start", "example/cli-work", "--as", PERSON],
        )
        .await;
    assert!(!output.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_send_reply_read_archive_search_and_attachments_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    let image = daemon.root.path().join("copper.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nCopper image").unwrap();
    let arguments = [
        "conversations",
        "send",
        WORKER,
        "--from",
        PERSON,
        "--body",
        "Copper question",
        "--subject",
        "Proof question",
        "--attach",
        image.to_str().unwrap(),
        "--idempotency-key",
        "coverage-cli-send-once",
    ];
    let sent = cli_value(daemon.cli(PERSON, &arguments).await);
    let message = sent["subject"].as_str().unwrap_or_else(|| panic!("{sent}"));
    daemon.restart().await;
    assert_eq!(
        cli_value(daemon.cli(PERSON, &arguments).await)["subject"],
        message
    );
    assert_eq!(
        daemon
            .store()
            .message(message)
            .unwrap()
            .unwrap()
            .attachments
            .len(),
        1
    );
    for lifecycle in ["staged", "delivered"] {
        let _: Value = daemon.transport().post(&format!("/v1/messages/{}/claims", message.trim_start_matches("message/")), &json!({"lifecycle": lifecycle, "actor": WORKER, "idempotency_key": format!("prepare-cli-{lifecycle}")})).await.unwrap();
    }
    for arguments in [
        vec!["conversations", "status", message],
        vec!["conversations", "read", message, "--as", WORKER],
        vec!["conversations", "thread", message],
        vec!["conversations", "search", "Copper"],
    ] {
        daemon.restart().await;
        cli_value(daemon.cli(WORKER, &arguments).await);
    }
    let reply_args = [
        "conversations",
        "reply",
        message,
        "--from",
        WORKER,
        "--body",
        "Copper answer",
        "--idempotency-key",
        "coverage-cli-reply-once",
    ];
    let reply = cli_value(daemon.cli(WORKER, &reply_args).await);
    let reply_id = reply["subject"].as_str().unwrap();
    daemon.restart().await;
    assert_eq!(
        cli_value(daemon.cli(WORKER, &reply_args).await)["subject"],
        reply_id
    );
    assert_eq!(
        daemon
            .store()
            .message(reply_id)
            .unwrap()
            .unwrap()
            .in_reply_to
            .as_deref(),
        Some(message)
    );
    cli_value(
        daemon
            .cli(
                WORKER,
                &["conversations", "archive", message, "--as", WORKER],
            )
            .await,
    );
    daemon.restart().await;
    assert_eq!(
        daemon.store().message(message).unwrap().unwrap().status,
        "closed"
    );
    let export = daemon.root.path().join("export");
    cli_value(
        daemon
            .cli(
                PERSON,
                &["conversations", "export", export.to_str().unwrap()],
            )
            .await,
    );
    assert!(export.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_revision_propose_inspect_approve_and_cancel_survive_restart() {
    for decision in ["approve", "cancel"] {
        let mut daemon = Daemon::new().await;
        let source = |goal: &str| {
            format!(
                "version 2\nmission \"example/cli-revision\" state=\"ready\" revisions=\"human-only\" revision-reviewer={PERSON:?} {{ goal \"Review the CLI revision.\"; step \"work\" {{ agentless; goal {goal:?} }} }}\n"
            )
        };
        daemon.apply(&source("Original goal."), "cli-revision-original");
        let run = daemon.start("example/cli-revision", "cli-revision-run");
        let file = daemon.root.path().join("revision.kdl");
        std::fs::write(&file, source("Updated goal.")).unwrap();
        daemon.restart().await;
        let proposed = cli_value(
            daemon
                .cli(
                    PERSON,
                    &[
                        "work",
                        "revise",
                        &run.subject,
                        file.to_str().unwrap(),
                        "--as",
                        PERSON,
                        "--reason",
                        "Make the goal explicit.",
                    ],
                )
                .await,
        );
        let proposal = daemon
            .store()
            .revision_proposal_for_run(&run.id)
            .unwrap()
            .unwrap_or_else(|| panic!("{proposed}"));
        daemon.restart().await;
        let shown = cli_value(
            daemon
                .cli(PERSON, &["work", "revision", "show", &run.subject])
                .await,
        );
        assert!(
            serde_json::to_string(&shown)
                .unwrap()
                .contains(&proposal.id)
        );
        let arguments = if decision == "approve" {
            vec![
                "work",
                "revision",
                "approve",
                &proposal.subject,
                proposal.preview_hash.as_deref().unwrap(),
                "--as",
                PERSON,
            ]
        } else {
            vec![
                "work",
                "revision",
                "cancel",
                &proposal.subject,
                "--as",
                PERSON,
            ]
        };
        cli_value(daemon.cli(PERSON, &arguments).await);
        daemon.restart().await;
        assert_eq!(
            daemon
                .store()
                .revision_proposal(&proposal.id)
                .unwrap()
                .unwrap()
                .status,
            if decision == "approve" {
                "applied"
            } else {
                "cancelled"
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_person_asks_updates_done_cancel_and_retired_attention_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply("version 2\nmission \"example/update\" state=\"ready\" { goal \"Report to its requester.\"; step \"report\" { assigned-to \"agent/example/worker\" } }\n", "update-mission");
    let update_run = daemon.start("example/update", "update-run");
    for decision in ["done", "cancel-ask"] {
        let asked = cli_value(
            daemon
                .cli(
                    WORKER,
                    &[
                        "work",
                        "ask",
                        "--for",
                        PERSON,
                        "--title",
                        "Choose the proof",
                        "--reason",
                        "The proof needs a choice.",
                        "--new-run",
                        decision,
                        "--as",
                        WORKER,
                        "--idempotency-key",
                        &format!("coverage-cli-ask-{decision}"),
                    ],
                )
                .await,
        );
        let step = asked["subject"]
            .as_str()
            .unwrap_or_else(|| panic!("{asked}"));
        daemon.restart().await;
        cli_value(
            daemon
                .cli(PERSON, &["attention", "show", step, "--as", PERSON])
                .await,
        );
        cli_value(
            daemon
                .cli(
                    WORKER,
                    &[
                        "work",
                        "update",
                        "--for",
                        PERSON,
                        "--about",
                        &update_run.subject,
                        "--title",
                        "Copper progress",
                        "--body",
                        "The proof is ready for your choice.",
                        "--as",
                        WORKER,
                        "--idempotency-key",
                        &format!("coverage-cli-update-{decision}"),
                    ],
                )
                .await,
        );
        daemon.restart().await;
        let actor = if decision == "done" { PERSON } else { WORKER };
        cli_value(
            daemon
                .cli(
                    actor,
                    &[
                        "work",
                        decision,
                        step,
                        "--as",
                        actor,
                        "--summary",
                        "The choice was recorded.",
                    ],
                )
                .await,
        );
        daemon.restart().await;
        assert_eq!(
            daemon.store().step_run(step).unwrap().unwrap().status,
            if decision == "done" {
                "completed"
            } else {
                "cancelled"
            }
        );
    }
    for arguments in [
        vec![
            "attention",
            "request",
            "--for",
            PERSON,
            "--title",
            "Retired",
            "--reason",
            "Use work ask.",
            "--as",
            WORKER,
        ],
        vec![
            "attention",
            "resolve",
            "attention/retired",
            "--outcome",
            "resolved",
            "--as",
            PERSON,
        ],
        vec![
            "attention",
            "withdraw",
            "attention/retired",
            "--reason",
            "Use cancel ask.",
            "--as",
            WORKER,
        ],
    ] {
        let index = daemon.store().index().unwrap();
        for _ in 0..2 {
            let refused = daemon.cli(PERSON, &arguments).await;
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("attention-migrated"),
                "{}",
                String::from_utf8_lossy(&refused.stderr)
            );
            assert_eq!(daemon.store().index().unwrap(), index);
            daemon.restart().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_mission_output_and_manual_wake_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply("version 2\nmission \"example/producer\" state=\"ready\" { goal \"Publish the child mission.\"; step \"produce\" { assigned-to \"agent/example/worker\"; produces-mission \"example/produced\" } }\n", "output-producer");
    let run = daemon.start("example/producer", "output-run");
    let step = &run.steps[0].subject;
    daemon.store().set_step_state(step, "ready", None).unwrap();
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "work",
                    "wake",
                    step,
                    "--as",
                    PERSON,
                    "--reason",
                    "Inspect the ready step.",
                ],
            )
            .await,
    );
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                WORKER,
                &[
                    "work",
                    "claim",
                    step,
                    "--as",
                    WORKER,
                    "--incarnation",
                    "4242:fixture",
                ],
            )
            .await,
    );
    let file = daemon.root.path().join("produced.kdl");
    std::fs::write(&file, "version 2\nmission \"example/produced\" state=\"ready\" { goal \"Use the proof.\"; step \"consume\" { agentless } }\n").unwrap();
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                WORKER,
                &[
                    "work",
                    "publish-mission",
                    step,
                    file.to_str().unwrap(),
                    "--as",
                    WORKER,
                    "--incarnation",
                    "4242:fixture",
                ],
            )
            .await,
    );
    daemon.restart().await;
    assert!(
        daemon
            .store()
            .mission_definitions()
            .unwrap()
            .iter()
            .any(|definition| definition.mission.id == "example/produced")
    );
}

/// Execute the real service CLI and daemon, replacing only the operating system's service
/// manager. Its files, child PIDs, sockets, PATH and XDG directories are entirely private.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_service_install_status_restart_reset_uninstall_use_isolated_manager() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    for directory in ["bin", "home", "config/st3", "state/st3", "data", "run"] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    let config = st3::config::Config {
        node: "fixture-service".into(),
        person: Some(PERSON.into()),
        state_dir: root.path().join("state/st3"),
        socket: root.path().join("run/st3.sock"),
        client_gateway_socket: root.path().join("run/st3-client.sock"),
        pty_root: Some(root.path().join("pty")),
        ..Default::default()
    };
    let config_path = root.path().join("config/st3/config.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let manager = root.path().join("bin/systemctl");
    std::fs::write(&manager, r#"#!/usr/bin/env python3
import os, pathlib, signal, subprocess, sys, time
root = pathlib.Path(os.environ['ST3_COVERAGE_ROOT'])
args = sys.argv[1:]
with (root / 'manager.log').open('a') as log: log.write(' '.join(args) + '\n')
pid_file = root / 'daemon.pid'
def stop():
    if pid_file.exists():
        pid = int(pid_file.read_text())
        try: os.kill(pid, signal.SIGTERM)
        except ProcessLookupError: pass
        for _ in range(100):
            try: os.kill(pid, 0)
            except ProcessLookupError: break
            time.sleep(.01)
        pid_file.unlink(missing_ok=True)
    for name in ['st3.sock', 'st3-client.sock']:
        (root / 'run' / name).unlink(missing_ok=True)
def start():
    log = (root / 'daemon.log').open('ab')
    child = subprocess.Popen([os.environ['ST3_COVERAGE_BINARY'], 'up', '--config', str(root / 'config/st3/config.toml'), '--pty-binary', str(root / 'bin/pty')], stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    pid_file.write_text(str(child.pid))
if 'show' in args:
    print('LoadState=loaded\nActiveState=active\nSubState=running')
elif 'st3.service' in args:
    if 'stop' in args or 'restart' in args or ('disable' in args and '--now' in args): stop()
    if 'start' in args or 'restart' in args: start()
"#).unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.path().join("bin/pty"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        root.path().join("bin/pty"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(self.0.join("daemon.pid")) {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", pid.trim()])
                    .status();
            }
        }
    }
    let _cleanup = Cleanup(root.path().into());
    let run = |args: Vec<String>| {
        let mut command = if args.get(1).map(String::as_str) == Some("reset") {
            let mut command = tokio::process::Command::new("python3");
            command.args(["-c", "import os,pty,subprocess,sys; master,slave=pty.openpty(); os.write(master,b'yes\\nfixture-service\\nerase st state\\n'); result=subprocess.run(sys.argv[1:],stdin=slave); os.close(master); os.close(slave); sys.exit(result.returncode)", assert_cmd::cargo::cargo_bin!("st3").to_str().unwrap()]);
            command
        } else {
            tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
        };
        command
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("ST3_COVERAGE_ROOT", root.path())
            .env("ST3_COVERAGE_BINARY", assert_cmd::cargo::cargo_bin!("st3"))
            .args(args);
        async move {
            tokio::time::timeout(Duration::from_secs(30), command.output())
                .await
                .unwrap()
                .unwrap()
        }
    };
    for action in [
        "install",
        "status",
        "permissions",
        "restart",
        "reset",
        "uninstall",
    ] {
        let mut args = vec!["service".into(), action.into()];
        if matches!(action, "install" | "restart" | "reset") {
            args.extend(["--config".into(), config_path.display().to_string()]);
        }
        if action == "reset" {
            std::fs::write(config.state_dir.join("must-be-erased"), "old state").unwrap();
        }
        let output = run(args).await;
        assert!(
            output.status.success(),
            "service {action}: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(root.path().join("daemon.log")).unwrap_or_default()
        );
        if action == "reset" {
            assert!(!config.state_dir.join("must-be-erased").exists());
        }
        if matches!(action, "install" | "restart" | "reset") {
            Client::unix_as(&config.socket, PERSON)
                .capabilities()
                .await
                .unwrap();
        }
    }
    assert!(!root.path().join("config/systemd/user/st3.service").exists());
    let log = std::fs::read_to_string(root.path().join("manager.log")).unwrap();
    for action in [
        "enable st3.service",
        "restart st3.service",
        "stop st3.service",
        "daemon-reload",
    ] {
        assert!(log.contains(action), "{log}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_launch_planning_questions_variants_approval_and_run_survive_restart() {
    for end in ["approve", "approve-and-launch", "revise", "cancel"] {
        let mut daemon = Daemon::new().await;
        let request = daemon.root.path().join("request.md");
        let markdown = daemon.root.path().join("proof.md");
        let kdl = daemon.root.path().join("candidate.kdl");
        std::fs::write(&request, "Plan the copper proof.").unwrap();
        std::fs::write(&markdown, "A copper proof with explicit evidence.").unwrap();
        std::fs::write(&kdl, "version 2\nmission \"example/cli-launch\" state=\"ready\" { goal \"Check the copper proof.\"; step \"proof\" { agentless } }\n").unwrap();
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &[
                        "launch",
                        "start",
                        request.to_str().unwrap(),
                        "--id",
                        "example/cli-launch",
                        "--workspace",
                        daemon.root.path().to_str().unwrap(),
                        "--as",
                        PERSON,
                    ],
                )
                .await,
        );
        let session = daemon
            .store()
            .planning_sessions(true)
            .unwrap()
            .pop()
            .unwrap();
        let launch = &session.id;
        daemon.restart().await;
        cli_value(daemon.cli(PERSON, &["launch", "show", launch]).await);
        let decision = cli_value(
            daemon
                .cli(
                    &session.planner,
                    &[
                        "launch",
                        "question",
                        launch,
                        "Should the proof include evidence?",
                        "--type",
                        "boolean",
                        "--as",
                        &session.planner,
                    ],
                )
                .await,
        );
        let decision_id = decision["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{decision}"));
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &[
                        "launch",
                        "answer",
                        launch,
                        decision_id,
                        r#"{"type":"boolean","value":true}"#,
                        "--as",
                        PERSON,
                    ],
                )
                .await,
        );
        for variant in ["default", "alternate"] {
            daemon.restart().await;
            cli_value(
                daemon
                    .cli(
                        &session.planner,
                        &[
                            "launch",
                            "submit",
                            launch,
                            "--variant",
                            variant,
                            "--markdown",
                            markdown.to_str().unwrap(),
                            "--kdl",
                            kdl.to_str().unwrap(),
                            "--as",
                            &session.planner,
                        ],
                    )
                    .await,
            );
        }
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &["launch", "compare", launch, "default", "alternate"],
                )
                .await,
        );
        let index = daemon.store().index().unwrap();
        let refused = daemon
            .cli(
                PERSON,
                &[
                    "launch",
                    "propose",
                    launch,
                    "alternate",
                    "--reason",
                    "Use the explicit proof.",
                    "--as",
                    PERSON,
                ],
            )
            .await;
        assert!(
            !refused.status.success(),
            "a new-mission launch cannot propose a run revision"
        );
        assert_eq!(daemon.store().index().unwrap(), index);
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &["launch", "preview", launch, "--variant", "default"],
                )
                .await,
        );
        let variant: Value = daemon
            .transport()
            .get(&format!(
                "/v1/client/launches/{}/variants/default",
                urlencoding::encode(launch)
            ))
            .await
            .unwrap();
        let preview = variant["preview_token"].as_str().unwrap();
        let workspace = daemon.root.path().display().to_string();
        let arguments = match end {
            "approve" => vec!["launch", end, launch, preview, "--as", PERSON],
            "approve-and-launch" => vec![
                "launch",
                end,
                launch,
                preview,
                "--workspace",
                &workspace,
                "--as",
                PERSON,
            ],
            "revise" => vec![
                "launch",
                end,
                launch,
                markdown.to_str().unwrap(),
                "--as",
                PERSON,
            ],
            _ => vec![
                "launch",
                end,
                launch,
                "--reason",
                "The fixture launch is cancelled.",
                "--as",
                PERSON,
            ],
        };
        daemon.restart().await;
        cli_value(daemon.cli(PERSON, &arguments).await);
        daemon.restart().await;
        let finished = daemon.store().planning_session(launch).unwrap().unwrap();
        assert_eq!(
            finished.status,
            match end {
                "revise" => "revision-requested",
                "cancel" => "cancelled",
                _ => "approved",
            }
        );
        if end == "approve" {
            cli_value(
                daemon
                    .cli(
                        PERSON,
                        &[
                            "launch",
                            "run",
                            launch,
                            "--workspace",
                            daemon.root.path().to_str().unwrap(),
                            "--as",
                            PERSON,
                        ],
                    )
                    .await,
            );
            daemon.restart().await;
        }
        if matches!(end, "approve" | "approve-and-launch") {
            assert!(
                daemon
                    .store()
                    .active_mission_runs()
                    .unwrap()
                    .iter()
                    .any(|run| run.mission == "mission/example/cli-launch")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_terminal_controls_and_stream_capabilities_survive_restart() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    let pty = daemon.pty(WORKER).await;
    let target = format!("terminal/{WORKER}");
    for command in ["screen", "peek"] {
        let read = cli_value(daemon.cli(PERSON, &["terminals", command, &target]).await);
        assert!(serde_json::to_string(&read).unwrap().contains("ready"));
    }
    let attached = cli_value(
        daemon
            .cli(PERSON, &["terminals", "attach-info", &target])
            .await,
    );
    let attachment = &attached["value"]["terminal_attachment"];
    assert!(attachment["stream_capability"].is_string(), "{attached}");
    daemon.restart().await;
    let streamed = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "terminals",
                    "stream",
                    &target,
                    "--capability",
                    attachment["stream_capability"].as_str().unwrap(),
                    "--incarnation",
                    &pty.incarnation,
                    "--count",
                    "1",
                ],
            )
            .await,
    );
    assert!(serde_json::to_string(&streamed).unwrap().contains("ready"));
    for arguments in [
        vec!["terminals", "input-client", &target, "copper-raw", "--raw"],
        vec!["terminals", "input-client", &target, "enter", "--key"],
        vec!["terminals", "send", WORKER, "copper-line"],
        vec!["terminals", "signal", WORKER, "user-1"],
    ] {
        daemon.restart().await;
        cli_value(daemon.cli(PERSON, &arguments).await);
    }
    daemon
        .screen_containing(WORKER, "accepted:copper-raw")
        .await;
    daemon
        .screen_containing(WORKER, "accepted:copper-line")
        .await;
    daemon.screen_containing(WORKER, "signal-received").await;
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "terminals",
                    "detach-client",
                    attachment["attachment_id"].as_str().unwrap(),
                    "--incarnation",
                    &pty.incarnation,
                ],
            )
            .await,
    );
    daemon.restart().await;
    let refused = daemon
        .cli(
            PERSON,
            &[
                "terminals",
                "stream",
                &target,
                "--capability",
                attachment["stream_capability"].as_str().unwrap(),
                "--incarnation",
                &pty.incarnation,
                "--count",
                "1",
            ],
        )
        .await;
    assert!(!refused.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_local_skill_completions_holds_and_repairs_use_private_files() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.apply(&format!("version 2\nagent \"example/worker\" {{ host {NODE:?}; workspace {:?}; harness \"codex\" {{}}; restart always }}\n", daemon.root.path()), "codex-hold-worker");
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "agents",
                    "hold",
                    WORKER,
                    "--for",
                    "1h",
                    "--reason",
                    "Pause delivery.",
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    daemon.restart().await;
    let held = cli_value(daemon.cli(PERSON, &["agents", "hold", WORKER]).await);
    assert_eq!(held["active"], true);
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "agents",
                    "hold",
                    WORKER,
                    "--release",
                    "--reason",
                    "Resume delivery.",
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    daemon.restart().await;
    assert_eq!(
        cli_value(daemon.cli(PERSON, &["agents", "hold", WORKER]).await)["active"],
        false
    );
    let plan = cli_value(daemon.cli(PERSON, &["repair", "dry-run"]).await);
    let token = plan["token"].as_str().unwrap();
    daemon.restart().await;
    let repaired = cli_value(daemon.cli(PERSON, &["repair", "apply", token]).await);
    let index = daemon.store().index().unwrap();
    daemon.restart().await;
    let replay = cli_value(daemon.cli(PERSON, &["repair", "apply", token]).await);
    assert_eq!(replay["token"], repaired["token"]);
    assert_eq!(daemon.store().index().unwrap(), index);
    let skill = daemon.cli(PERSON, &["skill"]).await;
    assert!(skill.status.success());
    assert!(String::from_utf8_lossy(&skill.stdout).contains("ST_AGENT"));
    let installed = daemon
        .cli(
            PERSON,
            &[
                "skill", "install", "claude", "codex", "pi", "omp", "opencode",
            ],
        )
        .await;
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        daemon
            .root
            .path()
            .join("home/.claude/skills/st/SKILL.md")
            .is_file()
    );
    assert!(
        daemon
            .root
            .path()
            .join("home/.agents/skills/st/SKILL.md")
            .is_file()
    );
    let completed = daemon.cli(PERSON, &["completions", "bash"]).await;
    assert!(completed.status.success());
    assert!(String::from_utf8_lossy(&completed.stdout).contains("missions"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_subscription_release_and_cancel_survive_restart() {
    let mut daemon = Daemon::new().await;
    for (command, held, expected) in [
        ("release", true, "pending"),
        ("cancel-request", false, "cancelled"),
    ] {
        let subscription = format!("subscription/example/{command}");
        let request = daemon.store().append_claim(&ClaimInput {
            subject: subscription.clone(), kind: "subscription.mission-requested".into(), actor: None,
            fields: serde_json::from_value(json!({"mission": "mission/example/review", "resource": "resource/example/proof", "resource_input": "source", "workspace": daemon.root.path(), "discovery": "fixture-discovery", "held": held})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        daemon.restart().await;
        let listed = cli_value(
            daemon
                .cli(PERSON, &["missions", "requests", &subscription, "--all"])
                .await,
        );
        assert!(
            serde_json::to_string(&listed)
                .unwrap()
                .contains(&request.id)
        );
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &[
                        "missions",
                        command,
                        &request.id,
                        "--as",
                        PERSON,
                        "--reason",
                        "Decided the fixture request.",
                    ],
                )
                .await,
        );
        daemon.restart().await;
        assert_eq!(
            daemon.store().subscription_requests(&subscription).unwrap()[0].status,
            expected
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_replication_inspection_and_repair_survive_restart() {
    use base64::Engine;
    const FLEET: &str = "5e3c1a9b-2d4f-4b6e-8a7c-0f1e2d3c4b5a";
    let mut daemon = Daemon::new().await;
    daemon.store().bind_fleet(FLEET).unwrap();
    let source = Store::open_memory("fixture-source").unwrap();
    let replacement = source
        .append_claim(&ClaimInput {
            subject: "host/fixture-source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: serde_json::from_value(json!({"status": "up"})).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("coverage-replacement".into()),
        })
        .unwrap();
    source.bind_fleet(FLEET).unwrap();
    let mut exchange = source
        .export_replication_exchange(FLEET, &daemon.store().replication_inventory().unwrap())
        .unwrap();
    let original = exchange.envelopes[0].clone();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&original.payload)
        .unwrap();
    let mut payload: smallclaims::claim::ReplicaEnvelopePayload =
        ciborium::from_reader(bytes.as_slice()).unwrap();
    let claim = &mut payload.batch.claims[0];
    claim.body["fields"]["unexpected"] = json!(true);
    claim.id = smallclaims::hash::claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )
    .unwrap();
    let mut bytes = Vec::new();
    ciborium::into_writer(&payload, &mut bytes).unwrap();
    let broken = st3::model::ReplicaEnvelope {
        hash: smallclaims::hash::replica_envelope_hash(
            &original.writer,
            original.sequence,
            original.previous_hash.as_deref(),
            original.accepted_at_unix_ms,
            &bytes,
        ),
        payload: base64::engine::general_purpose::STANDARD.encode(bytes),
        ..original.clone()
    };
    exchange.envelopes = vec![original, broken];
    daemon
        .store()
        .receive_replication_exchange("fixture-source", FLEET, &exchange)
        .unwrap();
    daemon.store().validate_replication_backlog().unwrap();
    daemon.store().project_replication_backlog().unwrap();
    let record = daemon.store().replica_records(true).unwrap().remove(0);
    daemon.restart().await;
    cli_value(daemon.cli(PERSON, &["replication", "invalid"]).await);
    let inspected = cli_value(
        daemon
            .cli(PERSON, &["replication", "inspect", &record.record_ref])
            .await,
    );
    assert!(
        serde_json::to_string(&inspected)
            .unwrap()
            .contains(&record.record_ref)
    );
    let args = [
        "replication",
        "repair",
        &record.record_ref,
        "--with",
        &replacement.id,
        "--as",
        PERSON,
        "--reason",
        "Replace the malformed fixture observation.",
        "--idempotency-key",
        "coverage-replication-repair",
    ];
    let repaired = cli_value(daemon.cli(PERSON, &args).await);
    daemon.restart().await;
    assert_eq!(
        cli_value(daemon.cli(PERSON, &args).await)["id"],
        repaired["id"]
    );
    let record = daemon
        .store()
        .replica_record(&record.record_ref)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, "repaired");
    assert_eq!(
        record.replacement_claim_id.as_deref(),
        Some(replacement.id.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_devices_and_native_import_survive_restart() {
    let mut daemon = Daemon::new().await;
    let challenge = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "devices",
                    "pair",
                    "Copper phone",
                    "--as",
                    PERSON,
                    "--full-control",
                ],
            )
            .await,
    );
    let challenge = challenge.get("value").unwrap_or(&challenge);
    let pairing = challenge["pairing_id"].as_str().unwrap();
    daemon.restart().await;
    let paired: Value = daemon.transport().post(&format!("/v1/client/pairings/{}/complete", urlencoding::encode(pairing.trim_start_matches("pairing/"))), &json!({"api_version": "st3.client.v0", "code": challenge["code"], "device_public_key": "copper-phone-key-000000000000000000000000"})).await.unwrap();
    let device = paired["device_id"].as_str().unwrap();
    daemon.restart().await;
    let listed = cli_value(daemon.cli(PERSON, &["devices"]).await);
    assert!(serde_json::to_string(&listed).unwrap().contains(device));
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "devices",
                    "revoke",
                    device,
                    "--as",
                    PERSON,
                    "--reason",
                    "The fixture device is retired.",
                ],
            )
            .await,
    );
    daemon.restart().await;
    assert!(matches!(
        Client::unix_gateway(daemon.socket(), paired["credential"].as_str().unwrap())
            .capabilities()
            .await,
        Err(ClientError::Api(ErrorCode::Forbidden, ..))
    ));
    let transcript = daemon
        .root
        .path()
        .join("native/.codex/sessions/2026/09/21/cli-import.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, format!("{}\n", json!({"type": "session_meta", "timestamp": "2026-09-21T08:00:00Z", "payload": {"id": "copper-cli-native", "cwd": daemon.root.path(), "source": "test"}}))).unwrap();
    use std::io::Write as _;
    writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(), "{}", json!({"type": "response_item", "timestamp": "2026-09-21T08:00:01Z", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Copper evidence"}]}})).unwrap();
    let listed = cli_value(daemon.cli(PERSON, &["import", "ls", "--all"]).await);
    assert!(
        serde_json::to_string(&listed)
            .unwrap()
            .contains("copper-cli-native")
    );
    let sessions: Value = daemon
        .transport()
        .get("/v1/client/sessions?native_only=true&history=true")
        .await
        .unwrap();
    let session = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_session_id"] == "copper-cli-native")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    daemon.restart().await;
    let timeline = cli_value(
        daemon
            .cli(
                PERSON,
                &["conversations", "timeline", session, "--as", PERSON],
            )
            .await,
    );
    assert!(
        serde_json::to_string(&timeline)
            .unwrap()
            .contains("Copper evidence")
    );
    daemon.restart().await;
    let mut command = daemon.cli_command(
        PERSON,
        &["conversations", "follow", session, "--as", PERSON],
    );
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut observed = line;
    for _ in 0..8 {
        if observed.contains("Copper evidence") {
            break;
        }
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        observed.push_str(&line);
    }
    assert!(observed.contains("Copper evidence"), "{observed}");
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    daemon.restart().await;
    cli_value(daemon.cli(PERSON, &["import", "show", session]).await);
    cli_value(
        daemon
            .cli(PERSON, &["import", "run", session, "--as", PERSON])
            .await,
    );
    daemon.restart().await;
    assert!(
        daemon
            .store()
            .desired_subjects()
            .unwrap()
            .iter()
            .any(|item| serde_json::to_string(item)
                .unwrap()
                .contains("copper-cli-native"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_launch_proposes_a_named_variant_from_an_exact_run_generation() {
    let mut daemon = Daemon::new().await;
    daemon.apply("version 2\nmission \"example/variants\" state=\"ready\" { goal \"Record the initial proof.\"; step \"proof\" { agentless } }\n", "variants-initial");
    let run = daemon.start("example/variants", "variants-run");
    let request = daemon.root.path().join("request.md");
    let markdown = daemon.root.path().join("proof.md");
    let kdl = daemon.root.path().join("variant.kdl");
    std::fs::write(&request, "Revise the copper proof.").unwrap();
    std::fs::write(&markdown, "The explicit copper proof.").unwrap();
    std::fs::write(&kdl, "version 2\nmission \"example/variants\" state=\"ready\" { goal \"Record the explicit proof.\"; step \"proof\" { agentless } }\n").unwrap();
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "launch",
                    "start",
                    request.to_str().unwrap(),
                    "--run",
                    &run.subject,
                    "--workspace",
                    daemon.root.path().to_str().unwrap(),
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    let session = daemon
        .store()
        .planning_sessions(true)
        .unwrap()
        .pop()
        .unwrap();
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                &session.planner,
                &[
                    "launch",
                    "submit",
                    &session.id,
                    "--variant",
                    "copper",
                    "--markdown",
                    markdown.to_str().unwrap(),
                    "--kdl",
                    kdl.to_str().unwrap(),
                    "--as",
                    &session.planner,
                ],
            )
            .await,
    );
    daemon.restart().await;
    cli_value(
        daemon
            .cli(
                PERSON,
                &["launch", "preview", &session.id, "--variant", "copper"],
            )
            .await,
    );
    daemon.restart().await;
    let proposed = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "launch",
                    "propose",
                    &session.id,
                    "copper",
                    "--reason",
                    "Use the explicit proof.",
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    assert!(
        matches!(proposed["status"].as_str(), Some("applied" | "pending")),
        "{proposed}"
    );
    daemon.restart().await;
    let index = daemon.store().index().unwrap();
    let refused = daemon
        .cli(
            PERSON,
            &[
                "launch",
                "propose",
                &session.id,
                "copper",
                "--reason",
                "The source generation changed.",
                "--as",
                PERSON,
            ],
        )
        .await;
    if proposed["status"] == "applied" {
        assert!(!refused.status.success());
        assert_eq!(daemon.store().index().unwrap(), index);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_seat_suspension_and_resume_wait_for_durable_owner_acknowledgements() {
    let mut daemon = Daemon::new().await;
    daemon.worker();
    daemon.claim(WORKER, "harness.session-file", json!({"harness": "claude", "session_id": "copper-cli-suspended", "agent": WORKER, "incarnation_id": "4242:fixture", "status": "active"}));
    daemon.claim(WORKER, "harness.observed", json!({"state": "idle", "incarnation_id": "4242:fixture", "quiescent": true, "blocking": []}));
    for (command, phase) in [("suspend", "suspended"), ("resume", "resumed")] {
        daemon.restart().await;
        let store = daemon.state.store.clone();
        // The runtime owner is a fixture actor here. Process continuity is covered by the
        // native runtime canaries; this tests the public CLI request/wait and durable state.
        let owner = tokio::spawn(async move {
            for _ in 0..200 {
                if let Some(operation) = st3::suspension::current(&store, WORKER).unwrap()
                    && operation.action == command
                    && matches!(operation.phase.as_str(), "quiescing" | "restoring")
                {
                    let key = if command == "suspend" {
                        store.append_claim(&ClaimInput {
                            subject: WORKER.into(), kind: "runtime.action.succeeded".into(), actor: Some(WORKER.into()),
                            fields: serde_json::from_value(json!({"action": command, "operation_status": "snapshotting", "harness": "claude", "native_session_id": "copper-cli-suspended"})).unwrap(),
                            evidence: vec![operation.operation_id.clone()], expected_subject: None, idempotency_key: Some(st3::suspension::suspend_snapshot_key(&operation.operation_id)),
                        }).unwrap();
                        st3::suspension::suspend_completed_key(&operation.operation_id)
                    } else {
                        st3::suspension::resume_completed_key(&operation.operation_id)
                    };
                    store.append_claim(&ClaimInput {
                        subject: WORKER.into(), kind: "runtime.action.succeeded".into(), actor: Some(WORKER.into()),
                        fields: serde_json::from_value(json!({"action": command, "operation_status": phase, "harness": "claude", "native_session_id": "copper-cli-suspended", "incarnation_id": "4242:fixture"})).unwrap(),
                        evidence: vec![operation.operation_id], expected_subject: None, idempotency_key: Some(key),
                    }).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the CLI did not ask its runtime owner to {command}");
        });
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &["agents", command, WORKER, "--as", PERSON, "--timeout", "3s"],
                )
                .await,
        );
        owner.await.unwrap();
        daemon.restart().await;
        let saved = st3::suspension::current(daemon.store(), WORKER)
            .unwrap()
            .unwrap();
        assert_eq!(saved.phase, phase);
        assert_eq!(
            saved.native_session_id.as_deref(),
            Some("copper-cli-suspended")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_human_reviews_act_on_the_current_card_after_restart() {
    for (kind, mode, verdict) in [
        ("approve", "approve", "pass"),
        ("reject", "approve", "fail"),
        ("request-changes", "feedback", "feedback"),
    ] {
        let mut daemon = Daemon::new().await;
        let worker = if mode == "feedback" {
            daemon.worker();
            "assigned-to \"agent/example/worker\""
        } else {
            "agentless"
        };
        daemon.apply(&format!(r#"version 2
mission "example/review" state="ready" {{
  goal "Answer a human gate."
  step "review" {{ {worker}; gate "accept" type="human" mode={mode:?} {{ reviewer "person/avery"; question "Is the copper proof acceptable?" }} }}
}}
"#), "review-mission");
        let run = daemon.start("example/review", "review-run");
        let step = &run.steps[0].subject;
        if mode == "feedback" {
            daemon.store().set_step_state(step, "ready", None).unwrap();
            for action in ["claim", "complete"] {
                daemon
                    .store()
                    .work_action(
                        step,
                        action,
                        &st3::model::WorkRequest {
                            actor: Some(WORKER.into()),
                            incarnation: Some("4242:fixture".into()),
                            summary: Some("Drafted the copper proof.".into()),
                            reason: None,
                            evidence: Vec::new(),
                            idempotency_key: format!("prepare-review-{action}"),
                        },
                    )
                    .unwrap();
            }
        }
        daemon.reconcile();
        let request = daemon
            .store()
            .gate_request_for_owner(step)
            .unwrap()
            .unwrap();
        let attention: Value = daemon
            .transport()
            .get("/v1/client/attention")
            .await
            .unwrap();
        let card = attention["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["source_id"] == *step)
            .unwrap();
        let target = card["id"].as_str().unwrap().to_owned();
        daemon.restart().await;
        cli_value(
            daemon
                .cli(
                    PERSON,
                    &[
                        "attention",
                        kind,
                        &target,
                        "--as",
                        PERSON,
                        "--reason",
                        "Copper proof was reviewed.",
                    ],
                )
                .await,
        );
        daemon.restart().await;
        let results = daemon
            .store()
            .claims_for(&request.subject, Some("gate.result"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].body["fields"]["verdict"], verdict);
        assert_eq!(results[0].actor.as_deref(), Some(PERSON));
    }
}

#[test]
fn action_inventory_matches_contract_and_has_existing_test_references() {
    use std::collections::BTreeSet;
    let inventory: Value =
        serde_json::from_str(include_str!("../../../docs/st3/action-coverage.json")).unwrap();
    let declared: BTreeSet<_> = st3_client::ACTION_NAMES.iter().copied().collect();
    let documented: BTreeSet<_> = inventory["typed_actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        declared, documented,
        "add a restart case when the action contract grows"
    );
    let dispatch = include_str!("action_coverage.rs")
        .split("actions! {")
        .nth(1)
        .unwrap()
        .split("\n    }")
        .next()
        .unwrap();
    for action in declared {
        assert!(
            dispatch.contains(&format!("\"{action}\" =>")),
            "{action} is absent from the real typed-client dispatch"
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for group in [
        "typed_actions",
        "cli",
        "stui_effects",
        "local_ui_actions",
        "palette_actions",
    ] {
        for row in inventory[group].as_array().unwrap() {
            let tests = row["tests"].as_array().unwrap();
            assert!(!tests.is_empty(), "missing test evidence: {row}");
            for reference in tests {
                let (file, test) = reference.as_str().unwrap().split_once("::").unwrap();
                let source = std::fs::read_to_string(root.join(file)).unwrap();
                assert!(
                    source.contains(&format!("fn {test}(")),
                    "coverage reference no longer exists: {reference}"
                );
            }
        }
    }
    let source = include_str!("../../stui/src/ui/mod.rs");
    let declaration = source
        .split("pub enum Effect {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    let offered: BTreeSet<_> = declaration
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("    ")?;
            if !line.chars().next()?.is_ascii_uppercase() {
                return None;
            }
            Some(line.split([' ', '{', '(', ',']).next().unwrap())
        })
        .collect();
    let documented: BTreeSet<_> = inventory["stui_effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["effect"].as_str().unwrap())
        .collect();
    assert_eq!(
        offered, documented,
        "add reducer and transport evidence when stui gains an effect"
    );
    let source = include_str!("../../stui/src/ui/glass.rs");
    let declaration = source
        .split("enum Action {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    let offered: BTreeSet<_> = declaration
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("    ")?;
            if !line.chars().next()?.is_ascii_uppercase() {
                return None;
            }
            Some(line.split([' ', '{', '(', ',']).next().unwrap())
        })
        .collect();
    let documented: BTreeSet<_> = inventory["palette_actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        offered, documented,
        "add coverage when the stui palette grows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_github_gates_pass_wait_and_refuse_over_private_http_across_restart() {
    let mut daemon = Daemon::new().await;
    let index = daemon.store().index().unwrap();
    for _ in 0..2 {
        for args in [
            vec!["gate", "merged", "invalid-fixture-locator"],
            vec![
                "gate",
                "ci-passed",
                "proof",
                "--repo",
                "invalid-fixture-repository",
                "--ref",
                "fixture-commit",
            ],
        ] {
            let output = daemon.cli(PERSON, &args).await;
            assert_eq!(output.status.code(), Some(3));
            assert!(!output.stdout.is_empty() || !output.stderr.is_empty());
            assert_eq!(daemon.store().index().unwrap(), index);
        }
        daemon.restart().await;
    }
    let phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let replies = phase.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(axum::routing::get(
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            let replies = replies.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer fixture-token");
                let phase = replies.load(std::sync::atomic::Ordering::SeqCst);
                if phase == 2 {
                    return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"message": "Private fixture missing"})));
                }
                let path = uri.path();
                let value = if path.ends_with("/pulls/12") {
                    json!({"merged": phase == 0, "state": if phase == 0 { "closed" } else { "open" }, "merge_commit_sha": "fixture-commit"})
                } else if path.ends_with("/check-runs") {
                    let passed = phase == 0 && !path.contains("obsolete-fixture");
                    json!({"check_runs": [{"id": 1, "name": "proof", "status": if passed { "completed" } else { "queued" }, "conclusion": if passed { json!("success") } else { Value::Null }}]})
                } else if path.ends_with("/status") {
                    json!({"statuses": []})
                } else if path.ends_with("/obsolete-fixture") {
                    json!({"sha": "obsolete-fixture"})
                } else {
                    json!({"sha": "fixture-commit"})
                };
                (axum::http::StatusCode::OK, axum::Json(value))
            }
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    for (state, expected, prefix) in [(0, 0, "pass:"), (1, 1, "not yet:"), (2, 3, "broken:")] {
        phase.store(state, std::sync::atomic::Ordering::SeqCst);
        daemon.restart().await;
        for args in [
            vec!["gate", "merged", "fixture/app#12"],
            vec!["gate", "ci-passed", "proof", "--repo", "fixture/app", "--ref", "fixture-commit"],
        ] {
            let output = daemon.cli_command(PERSON, &args)
                .env("GH_TOKEN", "fixture-token")
                .env("ST3_GITHUB_API_URL", &api)
                .output().await.unwrap();
            assert_eq!(output.status.code(), Some(expected), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).starts_with(prefix));
            assert_eq!(daemon.store().index().unwrap(), index);
        }
        if state == 0 {
            let output = daemon.cli_command(PERSON, &["gate", "ci-passed", "proof", "--repo", "fixture/app", "--ref", "obsolete-fixture"])
                .env("GH_TOKEN", "fixture-token")
                .env("ST3_GITHUB_API_URL", &api)
                .output().await.unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&output.stdout).contains("queued"));
            assert_eq!(daemon.store().index().unwrap(), index);
        }
    }
    server.abort();

}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_agent_and_shell_declarations_survive_restart() {
    let mut daemon = Daemon::new().await;
    const SEAT: &str = "agent/example/cli-seat";
    let store = daemon.state.store.clone();
    let notify = daemon.state.notify.clone();
    let events = daemon.state.event_notify.clone();
    let owner = tokio::spawn(async move {
        for _ in 0..300 {
            if store.selected_desired_token(SEAT).unwrap().is_some() {
                for (kind, fields) in [
                    (
                        "runtime.observed",
                        json!({"status": "running", "runtime_id": "cli-seat", "incarnation_id": "4243:fixture"}),
                    ),
                    (
                        "harness.observed",
                        json!({"state": "ready", "driver": "claude", "incarnation_id": "4243:fixture"}),
                    ),
                ] {
                    store
                        .append_claim(&ClaimInput {
                            subject: SEAT.into(),
                            kind: kind.into(),
                            actor: Some(SEAT.into()),
                            fields: serde_json::from_value(fields).unwrap(),
                            evidence: vec![],
                            expected_subject: None,
                            idempotency_key: None,
                        })
                        .unwrap();
                }
                // The CLI waits on the daemon's event channel after its first state read.
                events.send_modify(|version| *version += 1);
                notify.notify_waiters();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the CLI did not declare its new seat");
    });
    cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "agents",
                    "new",
                    "example/cli-seat",
                    "--workspace",
                    daemon.root.path().to_str().unwrap(),
                    "--host",
                    NODE,
                    "--as",
                    PERSON,
                    "--timeout",
                    "3s",
                ],
            )
            .await,
    );
    owner.await.unwrap();
    for (command, expected) in [("stop", "stop"), ("start", "agent")] {
        daemon.restart().await;
        cli_value(
            daemon
                .cli(PERSON, &["agents", command, SEAT, "--as", PERSON])
                .await,
        );
        daemon.restart().await;
        assert_eq!(
            daemon
                .store()
                .desired_subjects()
                .unwrap()
                .iter()
                .find(|item| item.subject == SEAT)
                .unwrap()
                .kind,
            expected
        );
    }
    let file = daemon.root.path().join("declaration.kdl");
    std::fs::write(&file, format!("version 2\nagent \"example/cli-seat\" {{ host {NODE:?}; workspace {:?}; harness \"claude\" {{}}; name \"Copper seat\"; restart always }}\n", daemon.root.path())).unwrap();
    cli_value(
        daemon
            .cli(
                PERSON,
                &["agents", "apply", file.to_str().unwrap(), "--as", PERSON],
            )
            .await,
    );
    daemon.restart().await;
    let visible = cli_value(daemon.cli(PERSON, &["agents", "show", SEAT]).await);
    assert!(
        serde_json::to_string(&visible)
            .unwrap()
            .contains("Copper seat")
    );
    let created = cli_value(
        daemon
            .cli(
                PERSON,
                &[
                    "terminals",
                    "new",
                    "Copper shell",
                    "--cwd",
                    daemon.root.path().to_str().unwrap(),
                    "--as",
                    PERSON,
                ],
            )
            .await,
    );
    let terminal = created["value"]["affected_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|id| id.starts_with("terminal/"))
        .unwrap();
    daemon.restart().await;
    cli_value(
        daemon
            .cli(PERSON, &["terminals", "end", terminal, "--as", PERSON])
            .await,
    );
    daemon.restart().await;
    assert_eq!(
        daemon
            .store()
            .desired_subjects()
            .unwrap()
            .iter()
            .find(|item| terminal == format!("terminal/{}", item.subject))
            .unwrap()
            .kind,
        "stop"
    );
}
