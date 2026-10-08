use super::*;

fn fixture() -> (Store, MissionRunView) {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
exec "probe" { command "false"; restart "never" }
mission "tree-root" state="ready" {
 goal "Preserve active diagnostics and all descendants."
 concurrent-runs max=8
 step "spawn" { agentless }
 gate "exit" { field "exit_code" "exec/probe" "is" 0 }
}
mission "tree-child" state="ready" {
 goal "Preserve queue labels."
 concurrent-runs max=8
 queue "ordered" { assigned-to "agent/worker"
  step "first" { }
  step "second" { }
 }
}
mission "tree-empty" state="ready" {
 goal "Preserve diagnostics with no step rows."
 concurrent-runs max=8
 gate "exit" { field "exit_code" "exec/probe" "is" 0 }
}"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &plan.subject_tokens, "tree-definitions")
        .unwrap();
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/test".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let root = store
        .create_mission_run(&request("tree-root", "root"))
        .unwrap();
    let child = store
        .create_child_mission_run(
            &request("tree-child", "child"),
            &root,
            &root.steps[0].subject,
            None,
        )
        .unwrap();
    let ended = store
        .create_child_mission_run(
            &request("tree-child", "ended"),
            &child,
            &child.steps[0].subject,
            None,
        )
        .unwrap();
    store
        .set_mission_run_state(&ended.id, "completed", "terminal", None)
        .unwrap();
    store
        .set_step_state(&root.steps[0].subject, "completed", None)
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "exec/probe".into(),
            kind: "runtime.observed".into(),
            actor: Some("daemon/node".into()),
            fields: BTreeMap::from([
                ("status".into(), json!("exited")),
                ("exit_code".into(), json!(1)),
                ("incarnation_id".into(), json!("probe-one")),
            ]),
            evidence: store.launch_lineage("exec/probe").unwrap(),
            expected_subject: None,
            idempotency_key: Some("probe-exited".into()),
        })
        .unwrap();
    (store, root)
}

#[test]
fn default_read_tree_reuses_definitions_with_complete_response_and_active_gate_parity() {
    let (store, root) = fixture();
    store
        .read_snapshot(|_| {
            let _clock = smallclaims::store::clock_snapshot();
            crate::store::STEP_DEFINITIONS_READ.with(|reads| reads.set(0));
            let mut expected = store.mission_runs_for_root(&root.id)?;
            annotate_stuck_gates(&store, &mut expected)?;
            assert_eq!(
                crate::store::STEP_DEFINITIONS_READ.with(std::cell::Cell::get),
                5
            );
            crate::store::STEP_DEFINITIONS_READ.with(|reads| reads.set(0));
            let mut tree = store.mission_run_tree_for_root(&root.id)?;
            assert_eq!(
                crate::store::STEP_DEFINITIONS_READ.with(std::cell::Cell::get),
                3
            );
            assert_eq!(tree.runs.len(), 3);
            assert_eq!(tree.definitions.len(), 2);
            annotate_stuck_gates_with_definitions(&store, &mut tree.runs, &tree.definitions)?;
            assert_eq!(
                serde_json::to_value(&tree.runs)?,
                serde_json::to_value(&expected)?
            );
            assert_eq!(
                tree.runs
                    .iter()
                    .find(|run| run.id == root.id)
                    .unwrap()
                    .stuck_gates
                    .len(),
                1
            );
            assert!(
                tree.runs
                    .iter()
                    .filter(|run| run.status == "completed")
                    .all(|run| run.stuck_gates.is_empty())
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn default_read_tree_keeps_empty_active_definition_and_gate_behavior() {
    let (store, _) = fixture();
    let empty = store
        .create_mission_run(&MissionRunRequest {
            mission: "tree-empty".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "empty".into(),
        })
        .unwrap();
    assert!(empty.steps.is_empty());
    store
        .read_snapshot(|_| {
            let _clock = smallclaims::store::clock_snapshot();
            let mut expected = store.mission_runs_for_root(&empty.id)?;
            annotate_stuck_gates(&store, &mut expected)?;
            let mut tree = store.mission_run_tree_for_root(&empty.id)?;
            annotate_stuck_gates_with_definitions(&store, &mut tree.runs, &tree.definitions)?;
            assert_eq!(
                serde_json::to_value(&tree.runs)?,
                serde_json::to_value(&expected)?
            );
            assert_eq!(tree.runs[0].stuck_gates.len(), 1);
            Ok(())
        })
        .unwrap();
}
