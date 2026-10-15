//! Consumed replay input identity and dirty-pass read budgets under a pre-admitted lease.
//! These exercise the production capture/retention reads, not physical process admission;
//! native stream tests cover report authentication and replay repair itself.
use super::*;
use authority::ReplayProofInputs;
use crate::mailbox::Authority;
use smallclaims::sqlite::work::SqliteWorkScope;

fn fixture(state: &AppState, name: &str) -> (Fence, NativeDeliveryPeer) {
    let subject = format!("agent/{name}");
    let intent = crate::graph::parse_intent(
        &format!("version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; harness \"omp\" {{}} }}"),
        "node",
    ).unwrap();
    state.store.apply_internal(&intent, &format!("replay-fixture-{name}")).unwrap();
    state.store.append_claim(&ClaimInput {
        subject: subject.clone(), kind: "runtime.observed".into(), actor: Some(subject.clone()),
        fields: serde_json::from_value(json!({"status":"running", "host":"node",
            "runtime_id":name, "incarnation_id":"current"})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
    let pid = std::process::id();
    let request = Fence::new(&subject, "current", "delivery");
    let agent_dir = crate::hooks::claude_agent_dir(
        &state.state_dir.join("drivers"), &subject, &state.node,
    );
    st_drivers::harness_events::enable(&agent_dir, "current").unwrap();
    let session = format!("replay-session-{name}");
    let sequence = st_drivers::harness_state::claim(&agent_dir, name, "omp", &session).unwrap();
    let owner = Authority {
        provider: "omp".into(), session, sequence,
        pid, process_token: st_runtime::process_start_token(pid).unwrap(),
    };
    let fence = state.store.bind_mailbox_with_lease(
        &request, Some(&owner),
    ).unwrap();
    let peer = NativeDeliveryPeer { agent: subject, transport: "omp-channel", pid, archives_inbox: true };
    (fence, peer)
}

fn state(root: &Path) -> AppState {
    let mut state = crate::api::tests::state(root);
    state.store = Arc::new(Store::open_memory("node").unwrap());
    state
}

#[test]
fn every_replay_input_change_invalidates_equality_and_retention() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (fence, peer) = fixture(&state, "eval.replay-inputs");
    let proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(proof, proof.clone());
    assert!(proof.is_current(&state, &fence, &peer));
    type Change = fn(&mut ReplayProofInputs);
    let changes: [(&str, Change); 15] = [
        ("failure", |input| input.failure = Some("another-failure".into())),
        ("subject", |input| input.subject.push_str("-other")),
        ("component", |input| input.component.push_str("-other")),
        ("incarnation", |input| input.incarnation.push_str("-other")),
        ("epoch", |input| input.epoch += 1),
        ("peer_agent", |input| input.peer_agent.push_str("-other")),
        ("peer_transport", |input| input.peer_transport = "claude-channel"),
        ("peer_pid", |input| input.peer_pid += 1),
        ("peer_archives_inbox", |input| input.peer_archives_inbox = !input.peer_archives_inbox),
        ("process_birth", |input| input.process_birth += 1),
        ("lease_owner.provider", |input| input.lease_owner.provider.push_str("-other")),
        ("lease_owner.session", |input| input.lease_owner.session.push_str("-other")),
        ("lease_owner.sequence", |input| input.lease_owner.sequence += 1),
        ("lease_owner.pid", |input| input.lease_owner.pid += 1),
        ("lease_owner.process_token", |input| input.lease_owner.process_token += 1),
    ];
    for (field, change) in changes {
        let mut changed = proof.clone();
        change(&mut changed);
        assert_ne!(changed, proof, "{field} must participate in replay input equality");
        assert!(!changed.is_current(&state, &fence, &peer), "{field} must invalidate retention");
    }
}

#[test]
fn unavailable_process_input_clears_consumed_proof() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (fence, mut peer) = fixture(&state, "eval.replay-read-error");
    let mut proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    peer.pid = u32::MAX;
    proof.peer_pid = peer.pid;
    assert!(authority::replay_proof_inputs(&state, &fence, &peer).is_err());
    assert!(!proof.is_current(&state, &fence, &peer));
}

#[tokio::test(start_paused = true)]
async fn heartbeat_retention_reads_only_the_lease_and_indexed_episode() {
    for count in [1usize, 8] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let streams: Vec<_> = (0..count).map(|number| {
            let (fence, peer) = fixture(&state, &format!("eval.replay-cost-{number}"));
            let scope = SqliteWorkScope::start();
            let proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
            let cost = scope.finish();
            assert_eq!(cost.statements, 2, "initial capture reads only lease and episode: {cost:?}");
            (fence, peer, proof)
        }).collect();
        let index = state.store.index().unwrap();
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10), Duration::from_secs(10),
        );
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(10)).await;
            heartbeat.tick().await;
            for (fence, peer, proof) in &streams {
                let scope = SqliteWorkScope::start();
                assert!(proof.is_current(&state, fence, peer));
                let cost = scope.finish();
                assert_eq!(cost.statements, 2,
                    "heartbeat retains proof without custody/provider/repair authentication reads: {cost:?}");
                assert_eq!(&authority::replay_proof_inputs(&state, fence, peer).unwrap(), proof);
            }
        }
        assert_eq!(state.store.index().unwrap(), index, "retaining a consumed proof must not write claims");
        println!("retained replay: streams={count}, heartbeat_passes=6, repair_authorization_calls=0, retention_statements={}, statements_per_pass=2", count * 6 * 2);
    }
}

fn failure(state: &AppState, fence: &Fence, reason: &str) -> String {
    state.store.append_claim(&ClaimInput {
        subject: fence.subject.clone(), kind: "operational.failure".into(),
        actor: Some("daemon/runtime".into()),
        fields: serde_json::from_value(json!({"episode":authority::episode(fence),
            "condition":"mailbox-channel-lost", "incarnation":fence.incarnation,
            "reason":reason, "reviewer":fence.subject, "title":"Mailbox lost",
            "severity":"error", "targets":[fence.subject]})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap().id
}

fn recover(state: &AppState, fence: &Fence, proof: &ReplayProofInputs, failure: &str) {
    // A pre-admitted store fixture publishes through the same lease-fenced writer as
    // repair_proven. It does not replace the native test of physical authorization.
    state.store.append_mailbox_lease_claim(fence, &proof.lease_owner, &ClaimInput {
        subject: fence.subject.clone(), kind: "operational.recovered".into(),
        actor: Some("daemon/runtime".into()),
        fields: serde_json::from_value(json!({"episode":authority::episode(fence),
            "failure":failure, "reason":"current native delivery consumed a replay"})).unwrap(),
        evidence: vec![], expected_subject: None,
        idempotency_key: Some(format!("{}:repaired:{failure}", authority::episode(fence))),
    }, true).unwrap();
}

#[test]
fn new_pending_loss_invalidates_proof_and_recovery_is_visible_on_the_next_pass() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (fence, peer) = fixture(&state, "eval.replay-loss");
    let consumed = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(consumed.failure, None);
    let first = failure(&state, &fence, "first interruption");
    let pending = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(pending.failure.as_deref(), Some(first.as_str()));
    assert_ne!(pending, consumed);
    assert!(!consumed.is_current(&state, &fence, &peer));
    recover(&state, &fence, &pending, &first);
    assert_eq!(state.store.mailbox_failure_episode(&fence, &authority::episode(&fence)).unwrap(),
        Some((first.clone(), false)), "the resolved episode remains in history");
    let next_pass = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(next_pass, consumed, "resolved history must not invalidate a consumed proof");
    assert!(!pending.is_current(&state, &fence, &peer));
    assert!(next_pass.is_current(&state, &fence, &peer));

    let second = failure(&state, &fence, "second interruption after proof consumption");
    let second_pending = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(second_pending.failure.as_deref(), Some(second.as_str()));
    assert_ne!(second_pending, pending, "failure identity changes even while pending remains true");
    assert!(!next_pass.is_current(&state, &fence, &peer));
    recover(&state, &fence, &second_pending, &second);
    let repaired = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert_eq!(repaired, consumed);
    assert!(repaired.is_current(&state, &fence, &peer));
}

#[test]
fn indexed_episode_read_error_clears_consumed_proof() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("claims.sqlite3");
    let mut state = state(root.path());
    state.store = Arc::new(Store::open(&database, "node").unwrap());
    let (fence, peer) = fixture(&state, "eval.replay-episode-error");
    let proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert!(proof.is_current(&state, &fence, &peer));
    // Break only the indexed episode lookup in this isolated file store.
    rusqlite::Connection::open(&database).unwrap()
        .execute_batch(/* sqlite-large-value: allow-for-now lv-he127ff49b19a owner=st3::api::mailbox::replay_proof_tests TODO=bound-or-reference-values-and-reclamation */ "DROP INDEX claims_subject_kind_accepted_index").unwrap();
    assert!(state.store.mailbox_failure_episode(&fence, &authority::episode(&fence)).is_err());
    assert!(!proof.is_current(&state, &fence, &peer));
}

#[test]
fn native_sequence_change_clears_proof_before_the_lease_is_readmitted() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let name = "eval.replay-native-sequence";
    let (fence, peer) = fixture(&state, name);
    let proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    let agent_dir = crate::hooks::claude_agent_dir(
        &state.state_dir.join("drivers"), &fence.subject, &state.node,
    );
    let sequence = st_drivers::harness_state::claim(
        &agent_dir, name, "omp", &proof.lease_owner.session,
    ).unwrap();
    assert!(sequence > proof.lease_owner.sequence);
    assert_eq!(state.store.mailbox_lease_authority(&fence).unwrap(), Some(proof.lease_owner.clone()));
    assert!(!proof.is_current(&state, &fence, &peer));
}

#[test]
fn revoked_lease_clears_proof_with_the_same_owner_identity() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("claims.sqlite3");
    let mut state = state(root.path());
    state.store = Arc::new(Store::open(&database, "node").unwrap());
    let (fence, peer) = fixture(&state, "eval.replay-lease-revoked");
    let proof = authority::replay_proof_inputs(&state, &fence, &peer).unwrap();
    assert!(proof.is_current(&state, &fence, &peer));
    rusqlite::Connection::open(&database).unwrap().execute(
        /* sqlite-large-value: allow-for-now lv-h746da844fa22 owner=st3::api::mailbox::replay_proof_tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_mailbox_leases SET revoked=1 WHERE subject=?1 AND component=?2",
        rusqlite::params![fence.subject, fence.component],
    ).unwrap();
    assert_eq!(state.store.mailbox_lease_authority(&fence).unwrap(), Some(proof.lease_owner.clone()));
    assert!(!proof.is_current(&state, &fence, &peer));
    let report = json!({"transport":peer.transport, "pid":peer.pid, "ready":true}).to_string();
    assert!(authority::admit_report(&state, &fence, &peer, &report).is_err());
}
