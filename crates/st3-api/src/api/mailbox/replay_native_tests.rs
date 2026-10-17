use super::*;
use st_drivers::{harness_events, harness_state};
use std::sync::atomic::Ordering::SeqCst;

fn native_peer_process() -> std::process::Child {
    // Physical capture reads the peer's real /proc identity, not an unrelated
    // native driver path contract inherited from the test runner.
    std::process::Command::new("sleep").arg("300")
        .env_remove(st_drivers::driver_paths::ROOT_ENV)
        .env_remove(st_drivers::driver_paths::AGENT_DIR_ENV)
        .env_remove(st_drivers::driver_paths::SESSION_DIR_ENV)
        .env_remove(st_drivers::driver_paths::IDENTITY_ENV)
        .spawn().unwrap()
}

struct NativeReplay {
    root: tempfile::TempDir,
    state: AppState,
    fence: Fence,
    peer: NativeDeliveryPeer,
    agent_dir: std::path::PathBuf,
    process: std::process::Child,
}

impl NativeReplay {
    fn new(name: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::api::tests::state(root.path());
        state.store = Arc::new(Store::open_memory("node").unwrap());
        let subject = format!("agent/{name}");
        let intent = crate::graph::parse_intent(
            &format!("version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; harness \"codex\" {{}} }}"),
            "node",
        ).unwrap();
        state.store.apply_internal(&intent, "native-replay-fixture").unwrap();
        let member = state.store.desired_subjects_named(std::slice::from_ref(&subject))
            .unwrap().remove(0).member.unwrap();
        let process = native_peer_process();
        let pid = process.id();
        let stamp = "2026-10-12T00:00:00.000Z";
        let incarnation = format!("{pid}:{stamp}");
        // A registry entry for the isolated live process exercises production
        // ancestry, process birth and provider admission without a PTY launch.
        std::fs::create_dir_all(&state.pty_root).unwrap();
        std::fs::write(state.pty_root.join(format!("{}.pid", member.runtime_id)), pid.to_string()).unwrap();
        std::fs::write(state.pty_root.join(format!("{}.sock", member.runtime_id)), "").unwrap();
        std::fs::write(state.pty_root.join(format!("{}.json", member.runtime_id)),
            json!({"createdAt":stamp,"tags":{"st3.subject":subject}}).to_string()).unwrap();
        state.store.append_claim(&ClaimInput {
            subject: subject.clone(), kind: "runtime.observed".into(), actor: Some(subject.clone()),
            fields: serde_json::from_value(json!({"status":"running","host":"node",
                "runtime_id":member.runtime_id,"incarnation_id":incarnation})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let agent_dir = crate::hooks::claude_agent_dir(&state.state_dir.join("drivers"), &subject, "node");
        harness_events::enable(&agent_dir, &incarnation).unwrap();
        let sequence = harness_state::claim(&agent_dir, name, "codex", "replay-session").unwrap();
        let peer = NativeDeliveryPeer { agent: subject.clone(), transport: "app-server", pid, archives_inbox: false };
        let owner = crate::mailbox::Authority { provider: "codex".into(), session: "replay-session".into(),
            sequence, pid, process_token: st_runtime::process_start_token(pid).unwrap() };
        let fence = state.store.bind_mailbox_with_lease(
            &Fence::new(&subject, &incarnation, "delivery"), Some(&owner),
        ).unwrap();
        Self { root, state, fence, peer, agent_dir, process }
    }

    async fn stream(&self) -> ControlledStream {
        controlled_stream_with_peer(self.state.clone(), &self.fence,
            &self.root.path().join("replay.sock"), Some(self.peer.clone())).await
    }

    fn report(&self, nonce: Option<&str>) -> String {
        let mut report = json!({"transport":self.peer.transport,"pid":self.peer.pid,
            "ready":true,"mailbox_replay_ack":true});
        if let Some(nonce) = nonce { report["mailbox_replay_nonce"] = json!(nonce); }
        report.to_string()
    }

    fn loss(&self) -> String {
        authority::loss_if_current(&self.state, &self.fence, &self.peer).unwrap();
        authority::replay_proof_inputs(&self.state, &self.fence, &self.peer)
            .unwrap().failure.expect("a new loss must be unresolved")
    }

    async fn consume(&self, stream: &mut ControlledStream) -> (String, authority::ReplayProofInputs) {
        stream.socket.send(Message::Text(self.report(None).into())).await.unwrap();
        let Frame::Replay { nonce } = next(&mut stream.socket).await else {
            panic!("an admitted native report must negotiate replay");
        };
        self.send_report(stream, &nonce, 1).await;
        let inputs = authority::replay_proof_inputs(&self.state, &self.fence, &self.peer).unwrap();
        assert!(inputs.failure.is_none(), "consuming replay must repair the pending loss");
        (nonce, inputs)
    }

    async fn send_report(&self, stream: &mut ControlledStream, nonce: &str, barrier: u8) {
        stream.socket.send(Message::Text(self.report(Some(nonce)).into())).await.unwrap();
        stream.socket.send(Message::Ping(vec![barrier].into())).await.unwrap();
        expect_pong(&mut stream.socket, &[barrier]).await;
    }
}

impl Drop for NativeReplay {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

async fn heartbeat(stream: &mut ControlledStream) {
    stream.heartbeat.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match stream.socket.next().await.unwrap().unwrap() {
                Message::Ping(bytes) => {
                    stream.socket.send(Message::Pong(bytes)).await.unwrap();
                    break;
                }
                Message::Text(raw) => {
                    let frame: Frame = serde_json::from_str(&raw).unwrap();
                    assert!(!matches!(frame, Frame::Replay { .. } | Frame::Fenced { .. }),
                        "an unchanged heartbeat must neither rechallenge nor fence: {frame:?}");
                }
                other => panic!("unexpected heartbeat frame: {other:?}"),
            }
        }
    }).await.unwrap();
}

#[tokio::test]
async fn consumed_native_replay_survives_heartbeat_and_repairs_the_next_loss() {
    let fixture = NativeReplay::new("eval.replay-retention");
    let first_loss = fixture.loss();
    let mut stream = fixture.stream().await;
    let (nonce, consumed) = fixture.consume(&mut stream).await;
    assert_eq!(stream.dirty_gate.replay_proofs.load(SeqCst), 1);
    heartbeat(&mut stream).await;
    fixture.send_report(&mut stream, &nonce, 2).await;
    assert_eq!(authority::replay_proof_inputs(&fixture.state, &fixture.fence, &fixture.peer).unwrap(), consumed);
    assert!(consumed.is_current(&fixture.state, &fixture.fence, &fixture.peer));
    assert_eq!(stream.dirty_gate.replay_proofs.load(SeqCst), 1,
        "heartbeat and admitted report must not repeat repair authorization");
    // Hold ordinary notifications until the explicit heartbeat. The already-consumed
    // nonce, not a new challenge or a reconnect, must recover this distinct episode.
    stream.dirty_gate.allowed.store(false, SeqCst);
    let second_loss = fixture.loss();
    assert_ne!(first_loss, second_loss);
    assert!(!consumed.is_current(&fixture.state, &fixture.fence, &fixture.peer));
    stream.dirty_gate.allowed.store(true, SeqCst);
    heartbeat(&mut stream).await;
    fixture.send_report(&mut stream, &nonce, 3).await;
    assert_eq!(stream.dirty_gate.replay_proofs.load(SeqCst), 2,
        "the next loss requires exactly one new authorization");
    assert_eq!(authority::replay_proof_inputs(&fixture.state, &fixture.fence, &fixture.peer).unwrap(), consumed);
    let recovered = fixture.state.store.latest_claim(&fixture.fence.subject, Some("operational.recovered"))
        .unwrap().unwrap();
    assert_eq!(recovered.body["fields"]["failure"], json!(second_loss));
}

#[tokio::test]
async fn consumed_replay_never_bypasses_revoked_provider_report_admission() {
    let fixture = NativeReplay::new("eval.replay-revoked");
    fixture.loss();
    let mut stream = fixture.stream().await;
    let (nonce, _) = fixture.consume(&mut stream).await;
    stream.dirty_gate.allowed.store(false, SeqCst);
    let pending = fixture.loss();
    let mut terminal: Value = serde_json::from_slice(&harness_events::read_runtime_state(
        &fixture.agent_dir, &fixture.fence.incarnation).unwrap().unwrap()).unwrap();
    terminal["state"] = json!("ended");
    terminal["exit"] = json!("completed");
    harness_events::write_snapshot(&fixture.agent_dir, "harness-state",
        &serde_json::to_vec(&terminal).unwrap()).unwrap();
    let index = fixture.state.store.index().unwrap();
    assert!(authority::admit_report(&fixture.state, &fixture.fence, &fixture.peer,
        &fixture.report(Some(&nonce))).is_err());
    fixture.send_report(&mut stream, &nonce, 2).await;
    assert_eq!(fixture.state.store.index().unwrap(), index, "refused reports write no recovery");
    assert_eq!(authority::replay_proof_inputs(&fixture.state, &fixture.fence, &fixture.peer)
        .unwrap().failure.as_deref(), Some(pending.as_str()));
}

#[tokio::test]
async fn consumed_replay_does_not_authorize_a_changed_epoch_or_replaced_peer() {
    let mut fixture = NativeReplay::new("eval.replay-replaced");
    fixture.loss();
    let mut stream = fixture.stream().await;
    let (nonce, consumed) = fixture.consume(&mut stream).await;
    assert!(consumed.is_current(&fixture.state, &fixture.fence, &fixture.peer));
    let mut missing_process = fixture.peer.clone();
    missing_process.pid = u32::MAX;
    assert!(!consumed.is_current(&fixture.state, &fixture.fence, &missing_process));
    let missing_report = json!({"transport":missing_process.transport,"pid":missing_process.pid,
        "ready":true,"mailbox_replay_nonce":nonce}).to_string();
    assert!(authority::repair_proven(&fixture.state, &fixture.fence, &missing_process, &missing_report).is_err());
    stream.dirty_gate.allowed.store(false, SeqCst);
    fixture.loss();
    let pending_inputs = authority::replay_proof_inputs(&fixture.state, &fixture.fence, &fixture.peer).unwrap();
    assert!(pending_inputs.is_current(&fixture.state, &fixture.fence, &fixture.peer));
    let mut replacement = fixture.peer.clone();
    replacement.transport = "omp-channel";
    assert!(!pending_inputs.is_current(&fixture.state, &fixture.fence, &replacement));
    let foreign = json!({"transport":replacement.transport,"pid":replacement.pid,"ready":true,
        "mailbox_replay_ack":true,"mailbox_replay_nonce":nonce}).to_string();
    assert!(authority::repair_proven(&fixture.state, &fixture.fence, &replacement, &foreign).is_err());
    stream.socket.send(Message::Text(foreign.into())).await.unwrap();
    stream.socket.send(Message::Ping(vec![2].into())).await.unwrap();
    expect_pong(&mut stream.socket, &[2]).await;
    let mut owner = fixture.state.store.mailbox_lease_authority(&fixture.fence).unwrap().unwrap();
    // Only a dead native predecessor can yield its live canonical capability.
    fixture.process.kill().unwrap();
    fixture.process.wait().unwrap();
    fixture.process = native_peer_process();
    let mut successor_peer = fixture.peer.clone();
    successor_peer.pid = fixture.process.id();
    owner.pid = successor_peer.pid;
    owner.process_token = st_runtime::process_start_token(owner.pid).unwrap();
    let successor = fixture.state.store.bind_mailbox_with_lease(
        &Fence::new(&fixture.fence.subject, &fixture.fence.incarnation, "delivery"), Some(&owner),
    ).unwrap();
    assert_ne!(successor.epoch, fixture.fence.epoch);
    assert!(!consumed.is_current(&fixture.state, &successor, &successor_peer));
    let index = fixture.state.store.index().unwrap();
    assert!(authority::repair_proven(&fixture.state, &fixture.fence, &fixture.peer,
        &fixture.report(Some(&nonce))).is_err());
    fixture.send_report(&mut stream, &nonce, 3).await;
    assert_eq!(fixture.state.store.index().unwrap(), index, "the retired stream cannot recover its loss");
    assert!(authority::replay_proof_inputs(&fixture.state, &successor, &successor_peer)
        .unwrap().failure.is_some());
}
