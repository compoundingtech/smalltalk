//! Actual field-gate evaluation used by the old-registry/prefix store controls.
use super::*;

pub(crate) fn gate_passes(store: Arc<Store>, episode: &str) -> bool {
    let Some(mission) = store.mission_spec(&format!("slo/{episode}"), None).unwrap() else { return false; };
    let step = mission.steps.get("dispatch-ready").unwrap();
    let reconciler = Reconciler::new(store, Arc::new(FakeRuntime::default()), "prefix".into(), Arc::new(Notify::new()));
    let stage = GateContext { subject: "step-run/prefix/dispatch-ready".into(), name: "dispatch-ready".into(),
        started_at_unix_ms: 0, attempt: 1, run: format!("mission-run/slo/{episode}"), generation: "run-generation/prefix".into(), eval: false };
    !step.gates.is_empty() && step.gates.iter().all(|gate|
        matches!(reconciler.evaluate_gate(&stage, gate).unwrap(), GateOutcome::Pass))
}

pub(crate) fn drive_ordinary_flow(store: Arc<Store>, host: &str) {
    let reconciler = Reconciler::new(store, Arc::new(FakeRuntime::default()), host.into(), Arc::new(Notify::new()));
    for _ in 0..4 { reconciler.reconcile_once().unwrap(); }
}
