#![cfg(target_os = "linux")]
//! An unreadable adoption observation is not evidence that a gate group stopped.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use st3::model::{ClaimInput, MissionRunRequest};
use st3::reconcile::{NativeRuntime, Reconciler, RuntimeControl};
use st3::store::Store;
use tokio::sync::Notify;

fn live(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ").is_some_and(|(_, fields)| !fields.starts_with('Z'))
    })
}

struct Group(u32);
impl Drop for Group {
    fn drop(&mut self) {
        unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL); }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "gate group did not reach the expected state");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn indeterminate_adopted_gate_remains_scheduled_until_sigkill() {
    // This test binary has only this test. Select degraded mode without changing the
    // process environment or depending on whether the host has a user manager.
    assert_eq!(
        st_runtime::initialize_isolation(&BTreeMap::new()),
        st_runtime::Isolation::DegradedDetached,
    );
    let root = tempfile::tempdir().unwrap();
    let state = root.path();
    let store = Arc::new(Store::open_memory("node").unwrap());
    let intent = st3::graph::parse_owned_set_intent(
        "version 2\nmission \"cancel\" state=\"ready\" { goal \"Stop the gate.\"; step \"wait\" { agentless } }",
        "node",
    ).unwrap();
    store.apply_internal(&intent, "gate-teardown-source").unwrap();
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "cancel".into(), revision: None, workspace: state.display().to_string(),
        requester: None, mode: None, inputs: BTreeMap::new(),
        idempotency_key: "gate-teardown-run".into(),
    }).unwrap();
    let step = &run.steps[0];
    let subject = format!("gate-operation/{}/resistant", step.subject.replace('/', "."));
    let runtime_id = subject.replace('/', ".");
    store.append_claim(&ClaimInput {
        subject: subject.clone(), kind: "gate.requested".into(), actor: None,
        fields: serde_json::from_value(serde_json::json!({"runner":"exec", "owner":step.subject})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let exec = st_runtime::ExecRuntime::new(state.join("exec"), state.join("logs"));
    let generation = exec.spawn(
        &runtime_id,
        &st_runtime::Launch::Shell("trap '' TERM; sleep 600 & echo $! > child.pid; echo ready > ready; wait".into()),
        state, &std::env::vars().collect(),
    ).unwrap();
    let _cleanup = Group(generation.pid);
    assert!(generation.scope_unit.is_none());
    wait_for(|| state.join("ready").exists());
    let child: u32 = std::fs::read_to_string(state.join("child.pid")).unwrap().trim().parse().unwrap();
    drop(exec);
    let runtime = Arc::new(NativeRuntime::new(state, None, Path::new("/bin/false")));
    assert_eq!(runtime.observe_exec(&runtime_id).unwrap().unwrap().status, "running");
    store.set_step_state(&step.subject, "cancelled", Some("test cancellation")).unwrap();
    store.request_mission_run_cancellation(&run.id, "test cancellation").unwrap();
    let reconciler = Reconciler::new(store.clone(), runtime.clone(), "node".into(), Arc::new(Notify::new()));
    reconciler.reconcile_once().unwrap();
    assert!(live(generation.pid) && live(child), "SIGTERM must be ignored");
    assert!(store.observations_for(&subject, "runtime.action.requested").unwrap().iter()
        .any(|claim| claim.body["fields"]["action"] == "terminate"));
    let record = state.join("exec").join(format!("{runtime_id}.json"));
    let bytes = std::fs::read(&record).unwrap();
    // Inject a readable but undecodable record to force the real runtime's
    // indeterminate result between terminate and the zero-grace kill pass.
    std::fs::write(&record, b"{").unwrap();
    assert_eq!(runtime.observe_exec(&runtime_id).unwrap().unwrap().status, "indeterminate");
    reconciler.reconcile_once().unwrap();
    std::fs::write(&record, bytes).unwrap();
    assert!(live(generation.pid) && live(child));
    assert!(store.observations_for(&subject, "runtime.action.deadline-reached").unwrap().is_empty());
    assert!(
        store.observations_for(&subject, "runtime.observed").unwrap().iter()
            .all(|claim| claim.body["fields"]["status"] != "stopped"),
        "an indeterminate observation must not retire a live process group",
    );
    reconciler.reconcile_once().unwrap();
    wait_for(|| !live(generation.pid) && !live(child));
    reconciler.reconcile_once().unwrap();
    assert!(store.observations_for(&subject, "runtime.action.succeeded").unwrap().iter()
        .any(|claim| claim.body["fields"]["action"] == "kill"));
}
