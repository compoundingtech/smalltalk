#[path = "../../../prototypes/agent-card/working_run.rs"]
mod working_run;

use std::collections::BTreeMap;

use serde_json::Value;
use st3::model::ClaimInput;
use st3::store::Store;
use working_run::WorkingRun;

#[test]
fn working_run_summary_matches_the_actual_store_fold_at_each_source_cut() {
    let store = Store::open_memory("invented-working-run-oracle").unwrap();
    let agent = "agent/example/worker";
    let mut summaries = BTreeMap::<&str, WorkingRun>::new();
    for (position, incarnation, state) in [
        (0, "inc-one", "idle"),
        (1, "inc-one", "working"),
        (2, "inc-two", "working"),
        (3, "inc-one", "working"),
        (4, "inc-one", "idle"),
        (5, "inc-one", "working"),
        (6, "inc-two", "idle"),
    ] {
        let claim = store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "harness.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String(state.into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String(incarnation.into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("working-oracle-{position}")),
            })
            .unwrap();
        let summary = summaries
            .get(incarnation)
            .copied()
            .unwrap_or_else(WorkingRun::empty)
            .then(WorkingRun::event(state, claim.accepted_at_unix_ms));
        summaries.insert(incarnation, summary);
        let cut = store.index().unwrap();
        for incarnation in ["inc-one", "inc-two"] {
            assert_eq!(
                store.agent_working_since(agent, incarnation, cut).unwrap(),
                summaries.get(incarnation).copied().and_then(WorkingRun::start),
                "incarnation {incarnation} at source cut {cut}"
            );
        }
    }
}
