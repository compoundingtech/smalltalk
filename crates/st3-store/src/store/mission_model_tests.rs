//! Mission model tests that parse intents through the graph; they live here because the mission module sits below the store.

#![allow(unused_imports, dead_code)]

use crate::mission::*;
use crate::graph::*;
use crate::store::Store;
use std::collections::{BTreeMap, BTreeSet};
use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use crate::declaration_syntax::*;
use crate::model::{ BaselineSpec, CompletionSpec, DependencySpec, GateSpec, LoopAttentionSpec, LoopExhaustionSpec, LoopSpec, LoopStopSpec, MetricSource, MetricSpec, MissionInputKind, MissionInputSpec, MissionSpec, MissionState, ProductSpec, RetrySpec, RevisionCutover, St3Error, StepSpec, UsedMissionSpec, WorkSelector, };

#[test]
fn the_parent_index_gives_every_step_the_parent_the_search_finds() {
    let source = r#"version 2
mission "outer" state="ready" {
  goal "Nest steps."
  step "top" { agentless }
  step "group" {
agentless
mission "inner" state="ready" {
  goal "Inside."
  step "leaf" { agentless }
  step "deeper" { agentless; mission "innermost" state="ready" { goal "Deepest."; step "bottom" { agentless } } }
}
  }
  step "last" { agentless }
}"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let mission = &intent.missions["outer"];
    let index = crate::mission::parent_step_index(mission);
    assert!(index.len() >= 6, "{index:?}");
    for (path, parent) in &index {
        assert_eq!(*parent, crate::mission::parent_step_path(mission, path), "{path}");
    }
    assert!(index.values().any(Option::is_none) && index.values().any(Option::is_some));
    assert_eq!(crate::mission::parent_step_path(mission, "no-such-step"), None);
    assert!(!index.contains_key("no-such-step"));
}

#[test]
fn keep_open_and_system_are_bare_flags() {
    let source = r#"version 2
mission "observer" state="ready" {
  system
  goal "Watch for work."
  step "hold" { agentless; keep-open }
  step "work" { agentless }
}"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let mission = &intent.missions["observer"];
    assert!(mission.system);
    assert!(mission.steps["hold"].keep_open);
    assert!(!mission.steps["work"].keep_open);
    assert!(crate::mission::step_keeps_open(Some(mission), "hold"));
    assert!(
        crate::mission::step_keeps_open(None, "seat-retirement"),
        "a fleet name, before the flag"
    );
    assert!(!crate::mission::step_keeps_open(Some(mission), "work"));
    assert!(crate::mission::is_system("mission/observer", Some(mission)));
    assert!(crate::mission::is_system("mission/fleet/example/ci/run", None));
    assert!(!crate::mission::is_system("mission/fleet/example/build", None));
    // An unflagged mission serializes as before, so its published revision is unchanged.
    let plain = source.replace("  system\n", "").replace("; keep-open", "");
    let plain = &crate::graph::parse_intent(&plain, "node").unwrap().missions["observer"];
    let body = serde_json::to_value(plain).unwrap();
    assert!(body.get("system").is_none() && body["steps"]["hold"].get("keep_open").is_none());
    for invalid in [
        source.replace("  system\n", "  system #true\n"),
        source.replace("  system\n", "  system\n  system\n"),
        source.replace("keep-open }", "keep-open \"yes\" }"),
        source.replace("keep-open }", "keep-open; keep-open }"),
    ] {
        assert!(
            crate::graph::parse_intent(&invalid, "node").is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn fresh_context_is_opt_in_on_steps_and_seats() {
    let source = r#"version 2
agent "worker" { workspace "/tmp"; command "true"; fresh-context }
mission "context" state="ready" {
  goal "Choose each step's context."
  step "kept" { assigned-to "agent/node.worker" }
  step "fresh" { assigned-to "agent/node.worker"; fresh-context }
}"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let mission = &intent.missions["context"];
    assert!(!mission.steps["kept"].fresh_context);
    assert!(mission.steps["fresh"].fresh_context);
    assert_eq!(
        intent.subjects["agent/node.worker"]
            .member
            .as_ref()
            .unwrap()
            .tags
            .get("st3.fresh_context")
            .map(String::as_str),
        Some("true")
    );
    for invalid in [
        source.replace("fresh-context }", "fresh-context \"yes\" }"),
        source.replace("fresh-context }\n}", "fresh-context \"yes\" }\n}"),
    ] {
        assert!(crate::graph::parse_intent(&invalid, "node").is_err());
    }
}

#[test]
fn quantified_fields_are_available_to_gates_loops_and_dependencies() {
    let source = r#"
version 2

resource "pull" { kind "vcs.pull-request" }
mission "quantified" state="ready" {
  goal "Use observed check lists declaratively."
  gate "all checks passed" {
every "checks" "resource/pull" {
  field "status" "is" "completed"
  field "conclusion" "is" "success"
}
  }
  loop "wait" {
max-rounds 2
until {
  gate "checks settled" {
    every "checks" "resource/pull" {
      field "status" "is" "completed"
    }
  }
}
round { completion { when "all-steps-exhausted" } }
  }
  step "report-failure" {
agentless
depends-on {
  not-every "checks" "resource/pull" {
    field "conclusion" "is" "success"
  }
}
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let mission = &intent.missions["quantified"];
    assert!(matches!(
        &mission.gates[0],
        crate::model::GateSpec::Every { path, subject, fields, .. }
            if path == "checks" && subject == "resource/pull" && fields.len() == 2
    ));
    assert!(matches!(
        &mission.steps["wait"].loop_spec.as_ref().unwrap().until[0],
        crate::model::GateSpec::Every { fields, .. } if fields.len() == 1
    ));
    assert!(matches!(
        &mission.steps["report-failure"].dependencies[0],
        crate::model::DependencySpec::Predicate {
            gate: crate::model::GateSpec::NotEvery { fields, .. }
        } if fields.len() == 1
    ));

    for (predicate, code) in [
        (
            r#"every "checks" "resource/pull""#,
            "empty-quantified-predicate",
        ),
        (
            r#"every "checks" "resource/pull" { exists "resource/other" }"#,
            "invalid-quantified-predicate",
        ),
        (
            r#"not-every "checks" "resource/pull" { field "status" "is" }"#,
            "invalid-quantified-field",
        ),
    ] {
        let source = format!(
            "version 2\nmission \"bad\" state=\"ready\" {{ goal \"Reject a malformed quantifier.\"; gate \"bad\" {{ {predicate} }} }}"
        );
        let error = crate::graph::parse_intent(&source, "node").unwrap_err();
        assert_eq!(error.code, code, "{predicate}");
    }
}

#[test]
fn first_class_loops_have_bounded_child_mission_rounds() {
    let source = r#"
version 2

mission "loop" state="ready" {
  goal "Repeat a graph until its gate passes."
  completion { when "all-steps-exhausted" }
  loop "improve" timeout="2h" {
max-rounds 4
until { gate "the result is ready" { exists "resource/result" } }
round {
  completion { when "all-steps-exhausted" }
  step "work" { agentless }
}
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let step = &intent.missions["loop"].steps["improve"];
    let retry = &step.retry;
    assert_eq!(retry.attempts, 4);
    let loop_spec = step.loop_spec.as_ref().unwrap();
    assert_eq!(loop_spec.max_rounds, 4);
    assert_eq!(loop_spec.timeout_ms, Some(7_200_000));
    assert_eq!(loop_spec.round.timeout_ms, Some(7_200_000));
    assert_eq!(loop_spec.round.steps.len(), 1);
    assert!(intent.missions.contains_key(&loop_spec.round.id));

    let source = r#"
version 2
mission "alert-loop" state="ready" {
  goal "Alert a person after bounded failure."
  loop "review" {
max-rounds 3
round { completion { when "all-steps-exhausted" } }
on-exhausted {
  fail
  attention "Automatic review failed" {
    reviewer "person/alex"
    severity "error"
  }
}
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let loop_spec = intent.missions["alert-loop"].steps["review"]
        .loop_spec
        .as_ref()
        .unwrap();
    let attention = loop_spec.exhaustion_attention.as_ref().unwrap();
    assert_eq!(attention.title, "Automatic review failed");
    assert_eq!(attention.reviewer, "person/alex");
    assert_eq!(attention.severity, "error");

    let source = r#"
version 2
mission "eval-loop" state="ready" timeout="20m" {
  goal "Bound every generated eval mission."
  loop "work" {
max-rounds 2
round { completion { when "all-steps-exhausted" } }
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let loop_spec = intent.missions["eval-loop"].steps["work"]
        .loop_spec
        .as_ref()
        .unwrap();
    assert_eq!(loop_spec.round.timeout_ms, Some(1_200_000));

    for (source, code) in [
        (
            r#"version 2
mission "bad" state="ready" {
  goal "Reject an unbounded loop."
  loop "work" { round { completion { when "all-steps-exhausted" } } }
}"#,
            "missing-loop-rounds",
        ),
        (
            r#"version 2
mission "bad" state="ready" {
  goal "Reject a zero-round loop."
  loop "work" { max-rounds 0; round { completion { when "all-steps-exhausted" } } }
}"#,
            "invalid-loop-rounds",
        ),
        (
            r#"version 2
mission "bad" state="ready" {
  goal "Reject a loop without a round completion."
  loop "work" { max-rounds 2; round { step "one" { agentless } } }
}"#,
            "missing-round-completion",
        ),
        (
            r#"version 2
mission "bad" state="ready" {
  goal "Reject the removed step loop."
  step "work" { loop { max-rounds 2 } }
}"#,
            "loop-is-graph-node",
        ),
        (
            r#"version 2
mission "bad" state="ready" {
  goal "Reject attention after success."
  loop "work" {
max-rounds 2
round { completion { when "all-steps-exhausted" } }
on-exhausted { succeed; attention "Wrong" { reviewer "person/alex" } }
  }
}"#,
            "invalid-loop-exhaustion-attention",
        ),
    ] {
        assert_eq!(
            crate::graph::parse_intent(source, "node").unwrap_err().code,
            code
        );
    }
}

#[test]
fn loop_collection_modes_are_removed_from_the_loop_grammar() {
    for source in [
        r#"version 2
mission "bad" state="ready" { goal "Reject for-each."; loop "items" for-each="resource/items" field="values" { max-rounds 2; round { step "read" { agentless } } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject best-of-N."; loop "choose" { max-rounds 1; candidates 3 { select metric="quality" }; round { step "create" { agentless } } } }"#,
    ] {
        assert_eq!(
            crate::graph::parse_intent(source, "node").unwrap_err().code,
            "removed-loop-shape"
        );
    }
}

#[test]
fn parses_parallel_steps_nested_work_and_products() {
    let source = r#"
version 2

  mission "demo" state="ready" {
goal "Complete mission demo."
step "start" {
  agentless

    agent "worker" {
      workspace "."
      harness "codex" {}
    }

}
step "one" {
  assigned-to "agent/${ST_MISSION_RUN}/worker"
  depends-on { step "start" completed }
  mission "work" { goal "Complete mission work."; step "inspect" { } }
  produces {
    resource "mission-run/${ST_MISSION_RUN}/change" { kind "vcs.commit"; state "published" }
  }
}
step "two" { agentless; depends-on { step "start" completed } }
step "join" { agentless; depends-on { step "one" completed; step "two" completed } }
completion { when "all-steps-exhausted" }
finally {
  step "cleanup" {
    agentless
     stop "agent/${ST_MISSION_RUN}/worker"
  }
}
  }

"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let mission = &intent.missions["demo"];
    assert_eq!(mission.steps.len(), 5);
    assert_eq!(mission.max_active_runs, Some(1));
    assert!(mission.steps["one"].nested_mission.is_some());
    assert_eq!(
        mission.steps["one"].products[0].subject,
        "resource/mission-run/${ST_MISSION_RUN}/change"
    );
}

#[test]
fn rejects_checkpoint_and_dependency_cycles() {
    let old =
        crate::graph::parse_intent("version 2\n checkpoints \"old\" { } ", "node").unwrap_err();
    assert_eq!(old.code, "unknown-node");
    let cycle = crate::graph::parse_intent(
        "version 2\n\n  mission \"cycle\" state=\"ready\" {\n    goal \"The cycle is rejected.\"\n    step \"a\" { depends-on \"b\" }\n    step \"b\" { depends-on \"a\" }\n  }\n\n",
        "node",
    )
    .unwrap_err();
    assert_eq!(cycle.code, "dependency-cycle");
}

#[test]
fn parses_a_human_review_contract() {
    let intent = crate::graph::parse_intent(
        r#"
version 2

  mission "review" state="ready" {
goal "Complete mission review."
step "approval" {
  gate "human-review" type="human" {
    reviewer "person/alex"
    question "Is this change ready to merge?"
    review "resource/mission-run/${ST_MISSION_RUN}/pull-request"
    review "doc/reports/run@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }
}

  }

"#,
        "node",
    )
    .unwrap();
    let gate = &intent.missions["review"].steps["approval"].gates[0];
    let crate::model::GateSpec::Human {
        reviewer,
        mode,
        question,
        review_targets,
        ..
    } = gate
    else {
        panic!("the gate is not human");
    };
    assert_eq!(reviewer, "person/alex");
    assert_eq!(mode.as_deref(), Some("approve"));
    assert_eq!(question.as_deref(), Some("Is this change ready to merge?"));
    assert_eq!(
        review_targets,
        &[
            "resource/mission-run/${ST_MISSION_RUN}/pull-request",
            "doc/reports/run@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]
    );
}

#[test]
fn feedback_gate_example_requires_a_worker_step() {
    let source = include_str!("../../../../examples/st3/human-feedback.kdl");
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let gate = &intent.missions["example/draft-feedback"].steps["draft"].gates[0];
    assert!(
        matches!(gate, crate::model::GateSpec::Human { mode, .. } if mode.as_deref() == Some("feedback"))
    );

    let invalid = source.replace("assigned-to \"agent/example/worker\"", "agentless");
    let error = crate::graph::parse_intent(&invalid, "node").unwrap_err();
    assert_eq!(error.code, "feedback-gate-needs-worker");
}

#[test]
fn parses_attempt_bound_mission_production_and_exact_mission_use() {
    let intent = crate::graph::parse_intent(
        &format!(
            r#"
version 2

  mission "bootstrap" state="ready" {{
goal "Complete mission bootstrap."
step "compile" {{
  document "doc/project/mission@{}"
  produces-mission "project/work"
}}
step "execute" {{
  depends-on {{ step "compile" completed }}
  uses-mission output-of="compile"
}}
step "reuse" {{
  uses-mission "project/work@{}"
}}
  }}

"#,
            "b".repeat(64),
            "a".repeat(64)
        ),
        "node",
    )
    .unwrap();
    let mission = &intent.missions["bootstrap"];
    assert_eq!(
        mission.steps["compile"].produces_mission.as_deref(),
        Some("project/work")
    );
    assert_eq!(
        mission.steps["compile"].documents,
        vec![format!("doc/project/mission@{}", "b".repeat(64))]
    );
    assert_eq!(
        mission.steps["execute"].uses_mission,
        Some(crate::model::UsedMissionSpec::StepOutput {
            step: "compile".into()
        })
    );
    assert_eq!(
        mission.steps["reuse"].uses_mission,
        Some(crate::model::UsedMissionSpec::Revision {
            mission: "project/work".into(),
            revision: "a".repeat(64),
        })
    );
}

#[test]
fn rejects_unpinned_or_unordered_mission_use() {
    let unpinned = crate::graph::parse_intent(
        r#"version 2
 mission "bad" state="ready" { goal "Use a mission."; step "use" { uses-mission "work" } } "#,
        "node",
    )
    .unwrap_err();
    assert_eq!(unpinned.code, "unpinned-mission-reference");

    let unpinned_document = crate::graph::parse_intent(
        r#"version 2
 mission "bad" state="ready" { goal "Use a document."; step "use" { document "doc/project/mission" } } "#,
        "node",
    )
    .unwrap_err();
    assert_eq!(unpinned_document.code, "unpinned-step-document");

    let unordered = crate::graph::parse_intent(
        r#"version 2

  mission "bad" state="ready" {
goal "Complete mission bad."
step "compile" { produces-mission "work" }
step "use" { uses-mission output-of="compile" }
  }
"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(unordered.code, "missing-mission-output-dependency");
}

#[test]
fn mission_and_step_contracts_accept_the_new_flat_language() {
    let intent = crate::graph::parse_intent(
        r#"
version 2

  mission "release" state="ready" {
goal "Publish the release."
goal "Keep the workspace clean."
baseline "release is open" { field "state" "resource/release" is "open" }
produces { resource "release" { kind "custom.test.release"; state "published" } }
gate "release is approved" { field "approval" "resource/release" is "yes" }
step "build" {
  goal "Build the artifact."
  goal "Record the checksum."
  baseline "source exists" { exists "resource/source" }
  produces { resource "artifact" { kind "custom.test.artifact"; state "published" } }
  gate "artifact is valid" { field "valid" "resource/artifact" is #true }
}

step "publish" { depends-on { step "build" completed } }
  }

"#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["release"];
    assert_eq!(mission.goals.len(), 2);
    assert_eq!(mission.baselines.len(), 1);
    assert_eq!(mission.products.len(), 1);
    assert_eq!(mission.gates.len(), 1);
    assert_eq!(mission.steps["build"].goals.len(), 2);
    assert_eq!(mission.steps["build"].baselines.len(), 1);
    assert_eq!(mission.steps["build"].products.len(), 1);
    assert_eq!(mission.steps["build"].gates.len(), 1);
    assert!(mission.steps["publish"].goals.is_empty());

    let unknown_product = crate::graph::parse_intent(
        r#"version 2

  mission "bad-product" state="ready" {
goal "Publish one invalid resource."
produces { resource "result" { kind "document.result"; state "published" } }
  }
"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(unknown_product.code, "unknown-resource-kind");

    let duplicate_field = crate::graph::parse_intent(
        r#"version 2

  mission "duplicate-product-field" state="ready" {
goal "Reject an ambiguous product."
produces {
  resource "result" {
    kind "custom.test.result"
    state "ready"
    state "published"
  }
}
  }
"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(duplicate_field.code, "duplicate-product-field");
}

#[test]
fn a_doubled_dollar_keeps_literal_variable_text() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "quote" state="ready" {
   goal "Explain what a shell variable expands to."
   step "explain" {
 goal "Print `$${HOME}` for ${ST_MISSION_RUN}."
 constraint "Keep `$${name}` as written."
   }
 }"#,
        "node",
    )
    .unwrap();
    let step = &intent.missions["quote"].steps["explain"];
    let variables =
        std::collections::BTreeMap::from([("ST_MISSION_RUN".to_owned(), "quote/1".to_owned())]);
    assert_eq!(
        crate::mission::interpolate(&step.goals[0], &variables).unwrap(),
        "Print `${HOME}` for quote/1."
    );
    assert_eq!(
        crate::mission::interpolate(&step.constraints[0], &variables).unwrap(),
        "Keep `${name}` as written."
    );

    let error = crate::graph::parse_intent(
        r#"version 2
 mission "quote" state="ready" {
   goal "Explain a shell variable."
   step "explain" { goal "Print `${HOME}`." }
 }"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(error.code, "unknown-variable");
    assert_eq!(
        error.message,
        "variable `HOME` is not registered; write `$${HOME}` for the literal text"
    );
}

#[test]
fn after_run_steps_are_agentless_and_start_before_the_rest_of_a_run() {
    use crate::mission::{AFTER_RUN_STEP, after_run_subject, mission_after_run, run_mission};
    use crate::model::{DependencySpec, WorkSelector};
    use std::collections::BTreeMap;

    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "ship" state="ready" {
   goal "Ship after the build run."
   assigned-to "agent/example/shipper"
   input "build" kind="text"
   step "wait" { after-run "${input.build}" }
   step "ship" { depends-on { step "wait" completed } }
   finally { step "report" { agentless } }
 }"#,
        "node",
    )
    .unwrap();
    let mission = intent.missions["ship"].clone();
    let wait = &mission.steps["wait"];
    assert_eq!(wait.after_run.as_deref(), Some("${input.build}"));
    assert_eq!(wait.work_selector, Some(WorkSelector::Agentless));
    assert_eq!(after_run_subject("build/1"), "mission-run/build/1");
    assert_eq!(
        after_run_subject("mission-run/build/1"),
        "mission-run/build/1"
    );

    let selected = crate::graph::parse_intent(
        r#"version 2
 mission "bad" state="ready" {
   goal "Wait with an agent."
   step "wait" { assigned-to "agent/example/shipper"; after-run "build/1" }
 }"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(selected.code, "conflicting-work-selector");

    let hashes = mission
        .steps
        .values()
        .map(|step| (step.id.clone(), step.definition_hash.clone()))
        .collect::<BTreeMap<_, _>>();
    let after = mission_after_run(mission.clone(), "build/1").unwrap();
    assert_eq!(after.display_order[0], AFTER_RUN_STEP);
    let step = &after.steps[AFTER_RUN_STEP];
    assert_eq!(step.after_run.as_deref(), Some("mission-run/build/1"));
    assert_eq!(step.work_selector, Some(WorkSelector::Agentless));
    let waits = |id: &str| {
        after.steps[id]
            .dependencies
            .contains(&DependencySpec::Step {
                step: AFTER_RUN_STEP.into(),
                state: "completed".into(),
            })
    };
    assert!(waits("wait") && waits("ship") && !waits("report"));
    for (id, hash) in hashes {
        assert_eq!(after.steps[&id].definition_hash, hash);
    }
    assert_eq!(run_mission(mission.clone(), None).unwrap(), mission);
    assert_eq!(
        mission_after_run(after, "build/2").unwrap_err().code,
        "after-run-step-exists"
    );

    let itself = crate::graph::parse_intent(
        r#"version 2
 mission-run "ship/1" {
   mission "mission/ship@0000000000000000000000000000000000000000000000000000000000000000"
   workspace "/work"
   requester "person/operator"
   after "ship/1"
 }"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(itself.code, "invalid-mission-run-after");
}

#[test]
fn agent_block_constraints_leave_the_runtime_declaration() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "handoff" state="ready" {
   goal "Build and merge one change."
   constraint "Keep the change small."
   agent "builder" {
 workspace "."
 command "true"
 constraint "Never push the release branch."
   }
   step "build" {
 assigned-to "agent/${ST_MISSION_RUN}/builder"
 agent "checker" {
   identity "reviewer"
   workspace "."
   command "true"
   constraint "Report findings without editing files."
 }
   }
 }"#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["handoff"];
    assert_eq!(mission.constraints, ["Keep the change small."]);
    assert_eq!(
        mission.agent_constraints,
        std::collections::BTreeMap::from([(
            "agent/${ST_MISSION_RUN}/builder".to_owned(),
            vec!["Never push the release branch.".to_owned()]
        )])
    );
    assert_eq!(
        mission.steps["build"].agent_constraints,
        std::collections::BTreeMap::from([(
            "agent/${ST_MISSION_RUN}/reviewer".to_owned(),
            vec!["Report findings without editing files.".to_owned()]
        )])
    );
    assert!(mission.steps["build"].constraints.is_empty());
    for declarations in [
        mission.declarations_kdl.as_deref().unwrap(),
        mission.steps["build"].declarations_kdl.as_deref().unwrap(),
    ] {
        assert!(declarations.contains("command"));
        assert!(!declarations.contains("constraint"));
    }

    let duplicate = crate::graph::parse_intent(
        r#"version 2
 mission "duplicate" state="ready" {
   goal "Reject duplicate agent text."
   agent "builder" {
 workspace "."
 command "true"
 constraint "Do not push."
 constraint "Do not push."
   }
 }"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(duplicate.code, "duplicate-constraint");

    let seat = crate::graph::parse_intent(
        r#"version 2
 agent "example/example/builder" {
   workspace "."
   command "true"
   constraint "Do not push."
 }"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(seat.code, "agent-constraint-outside-mission");
}

#[test]
fn missions_without_agent_constraints_keep_their_serialized_shape() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "plain" state="ready" {
   goal "Keep the revision hash stable."
   agent "builder" { workspace "."; command "true" }
   step "build" { assigned-to "agent/${ST_MISSION_RUN}/builder" }
 }"#,
        "node",
    )
    .unwrap();
    let value = serde_json::to_value(&intent.missions["plain"]).unwrap();
    assert!(value.get("agent_constraints").is_none());
    assert!(value["steps"]["build"].get("agent_constraints").is_none());
}

#[test]
fn constraints_repeat_at_each_level_and_reject_empty_or_duplicate_text() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "safe" state="ready" {
   goal "Do the work."
   constraint "Do not push."
   constraint "Keep private data outside Git."
   step "inspect" {
 constraint "Do not edit files."
 constraint "Report only sanitized findings."
   }
 }"#,
        "node",
    )
    .unwrap();
    assert_eq!(
        intent.missions["safe"].constraints,
        ["Do not push.", "Keep private data outside Git."]
    );
    assert_eq!(
        intent.missions["safe"].steps["inspect"].constraints,
        ["Do not edit files.", "Report only sanitized findings."]
    );

    for (source, code) in [
        (
            r#"version 2
 mission "empty" state="ready" { goal "Reject empty text."; constraint "" }"#,
            "invalid-constraint",
        ),
        (
            r#"version 2
 mission "duplicate" state="ready" {
   goal "Reject duplicate text."
   constraint "Do not push."
   constraint "Do not push."
 }"#,
            "duplicate-constraint",
        ),
        (
            r#"version 2
 mission "duplicate-step" state="ready" {
   goal "Reject duplicate step text."
   step "work" {
 constraint "Do not push."
 constraint "Do not push."
   }
 }"#,
            "duplicate-constraint",
        ),
    ] {
        assert_eq!(
            crate::graph::parse_intent(source, "node").unwrap_err().code,
            code
        );
    }
}

#[test]
fn selectors_completion_and_finally_use_the_explicit_language() {
    let intent = crate::graph::parse_intent(
        r#"
version 2

  mission "pool" state="ready" {
goal "Complete the pool work."
available-to "agent/node.one"
available-to "agent/node.two"
completion { depends-on { step "assigned" completed } }
step "inherited" { }
step "assigned" {
  assigned-to "agent/node.one"
  depends-on { step "inherited" completed }
}
finally { step "cleanup" { agentless } }
  }

"#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["pool"];
    assert_eq!(
        mission.work_selector,
        Some(crate::model::WorkSelector::Available {
            agents: vec!["agent/node.one".into(), "agent/node.two".into()]
        })
    );
    assert_eq!(mission.steps["inherited"].work_selector, None);
    assert_eq!(
        mission.steps["assigned"].work_selector,
        Some(crate::model::WorkSelector::Assigned {
            agent: "agent/node.one".into()
        })
    );
    assert!(mission.steps["cleanup"].finally);
    assert_eq!(
        mission.steps["cleanup"].work_selector,
        Some(crate::model::WorkSelector::Agentless)
    );
    assert!(matches!(
        mission.completion,
        Some(crate::model::CompletionSpec::Dependencies { .. })
    ));

    for source in [
        r#"version 2
 mission "bad" state="ready" { goal "Reject selectors."; assigned-to "agent/node.one"; agentless; step "work" { } } "#,
        r#"version 2
 mission "bad" state="ready" { goal "Reject selectors."; available-to "agent/node.one"; available-to "agent/node.one"; step "work" { } } "#,
        r#"version 2
 mission "bad" state="ready" { goal "Reject completion."; completion { when "all-steps-exhausted"; depends-on { step "work" completed } }; step "work" { } } "#,
        r#"version 2
 mission "bad" state="ready" { goal "Reject a final completion dependency."; completion { depends-on { step "cleanup" completed } }; finally { step "cleanup" { } } } "#,
        r#"version 2
 mission "bad" state="ready" { goal "Reject a cross-phase dependency."; step "work" { }; finally { step "cleanup" { depends-on { step "work" completed } } } } "#,
    ] {
        let invalid = crate::graph::parse_intent(source, "node").and_then(|intent| {
            crate::graph::parse_execution_intent(
                intent.missions["bad"].declarations_kdl.as_deref().unwrap(),
                "node",
                "bad",
            )
        });
        assert!(invalid.is_err());
    }
}

#[test]
fn queues_retain_order_and_apply_selector_precedence() {
    let intent = crate::graph::parse_intent(
        r#"version 2
mission "queued" state="ready" {
  goal "Complete the queue."
  assigned-to "mission-worker"
  step "before" { agentless }
  queue "investigations" {
available-to "pool.one"
available-to "pool.two"
step "first" { }
step "second" {
  depends-on { step "first" completed; step "first" completed; step "before" completed }
}
step "third" {
  assigned-to "agent/external/run/worker"
}
  }
  step "after" { }
}"#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["queued"];
    assert_eq!(
        mission.display_order,
        ["before", "first", "second", "third", "after"]
    );
    assert_eq!(
        mission.steps["first"].queue.as_deref(),
        Some("investigations")
    );
    assert_eq!(mission.steps["first"].queue_position, Some(1));
    assert_eq!(mission.steps["second"].queue_position, Some(2));
    assert_eq!(mission.steps["third"].queue_position, Some(3));
    assert_eq!(
        mission.steps["first"].work_selector,
        Some(crate::model::WorkSelector::Available {
            agents: vec!["agent/pool.one".into(), "agent/pool.two".into()]
        })
    );
    assert_eq!(
        mission.steps["third"].work_selector,
        Some(crate::model::WorkSelector::Assigned {
            agent: "agent/external/run/worker".into()
        })
    );
    assert_eq!(
        mission.steps["second"]
            .dependencies
            .iter()
            .filter(|dependency| matches!(
                dependency,
                crate::model::DependencySpec::Step { step, state }
                    if step == "first" && state == "completed"
            ))
            .count(),
        1
    );
    assert!(
        mission.steps["second"]
            .dependencies
            .iter()
            .any(|dependency| matches!(
                dependency,
                crate::model::DependencySpec::Step { step, state }
                    if step == "before" && state == "completed"
            ))
    );
    assert!(mission.steps["after"].queue.is_none());
    assert!(mission.steps["first"].definition_hash != mission.steps["second"].definition_hash);
}

#[test]
fn queues_reject_ambiguous_or_unsupported_shapes() {
    for (source, code) in [
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject an empty queue."; queue "empty" { agentless } }"#,
            "empty-queue",
        ),
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject queue selector conflict."; queue "q" { agentless; assigned-to "one"; step "work" { } } }"#,
            "conflicting-work-selector",
        ),
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject nested queues."; queue "outer" { queue "inner" { step "work" { } } } }"#,
            "invalid-queue-child",
        ),
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject a final queue."; finally { queue "q" { step "work" { } } } }"#,
            "invalid-finally-child",
        ),
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject duplicate steps."; step "same" { }; queue "q" { step "same" { } } }"#,
            "duplicate-step",
        ),
        (
            r#"version 2
mission "bad" state="ready" { goal "Reject a conflicting queue dependency."; queue "q" { step "one" { }; step "two" { depends-on { step "one" failed } } } }"#,
            "conflicting-queue-dependency",
        ),
    ] {
        assert_eq!(
            crate::graph::parse_intent(source, "node").unwrap_err().code,
            code
        );
    }
}

#[test]
fn mission_authority_still_parses_and_free_mode_names_it_as_ignored() {
    let intent = crate::graph::parse_intent(
        r#"version 2
mission "standing" state="ready" {
  goal "Keep the planner available."
  agent "planner" {
workspace "."
command "true"
mission-authority {
  publish "project/generated"
  publish "example/fabric/*"
  start "example/fabric/*"
  revise "example/fabric/queue"
}
  }
}"#,
        "node",
    )
    .unwrap();
    let runtime = crate::graph::parse_execution_intent(
        intent.missions["standing"]
            .declarations_kdl
            .as_deref()
            .unwrap(),
        "node",
        "standing",
    )
    .unwrap();
    let desired = &runtime.subjects["agent/standing/planner"].desired;
    assert_eq!(
        crate::graph::declared_authority_blocks(desired),
        ["mission-authority"]
    );
    assert_eq!(
        crate::graph::mission_declared_authority_blocks(&intent.missions["standing"], "node")
            .unwrap(),
        std::collections::BTreeMap::from([(
            "agent/migration-proof/planner".to_owned(),
            vec!["mission-authority"]
        )])
    );

    for source in [
        r#"version 2
mission "bad" state="ready" { goal "Reject empty authority."; agent "bad" { workspace "."; command "true"; mission-authority { } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a prefixed mission."; agent "bad" { workspace "."; command "true"; mission-authority { publish "mission/work" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject an internal wildcard."; agent "bad" { workspace "."; command "true"; mission-authority { publish "work/*/bad" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a duplicate rule."; agent "bad" { workspace "."; command "true"; mission-authority { publish "work/*"; publish "work/*" } } }"#,
    ] {
        assert!(crate::graph::parse_intent(source, "node").is_err());
    }
}

#[test]
fn agent_authority_and_mission_cancel_reject_invalid_rules() {
    for block in [
        "agent-authority { }",
        "agent-authority { start \"example/*\" }",
        "agent-authority { apply \"agent/example/*\" }",
        "agent-authority { apply \"example/*/bad\" }",
        "agent-authority { apply \"example/*\"; apply \"example/*\" }",
        "mission-authority { cancel \"mission/example/*\" }",
        "mission-authority { cancel \"example/*\"; cancel \"example/*\" }",
    ] {
        let source = format!(
            "version 2\nagent \"example/operator\" {{ workspace \".\"; command \"true\"; {block} }}\n"
        );
        assert!(
            crate::graph::parse_intent(&source, "node").is_err(),
            "{block}"
        );
    }
}

#[test]
fn mission_authority_none_still_parses_and_other_values_are_refused() {
    let intent = crate::graph::parse_intent(
        r#"version 2
agent "fleet/fixture-quiet/standing/quiet" { workspace "."; command "true"; mission-authority "none"; }
agent "fleet/fixture-website/standing/website" { workspace "."; command "true"; }"#,
        "node",
    )
    .unwrap();
    assert_eq!(
        crate::graph::declared_authority_blocks(
            &intent.subjects["agent/fleet/fixture-quiet/standing/quiet"].desired
        ),
        ["mission-authority"]
    );
    assert!(
        crate::graph::declared_authority_blocks(
            &intent.subjects["agent/fleet/fixture-website/standing/website"].desired
        )
        .is_empty()
    );

    for source in [
        r#"version 2
agent "fleet/fixture-bad/seat" { workspace "."; command "true"; mission-authority "all"; }"#,
        r#"version 2
agent "fleet/fixture-bad/seat" { workspace "."; command "true"; mission-authority "none" { publish "fleet/fixture-bad/*" } }"#,
        r#"version 2
agent "fleet/fixture-bad/seat" { workspace "."; command "true"; mission-authority "none" "none"; }"#,
        r#"version 2
agent "fleet/fixture-bad/seat" { workspace "."; command "true"; mission-authority none=#true; }"#,
    ] {
        assert!(
            crate::graph::parse_intent(source, "node").is_err(),
            "{source}"
        );
    }
}

#[test]
fn queue_authority_still_parses_and_rejects_invalid_rules() {
    let intent = crate::graph::parse_intent(
        r#"version 2
mission "standing" state="ready" {
  goal "Keep the chief of staff available."
  agent "chief" {
workspace "."
command "true"
queue-authority {
  move "example/fabric/builder"
  move "example/review/*"
  move "${ST_MISSION_RUN}/worker"
}
  }
}"#,
        "node",
    )
    .unwrap();
    // A run interpolates its declarations before it materializes them.
    let declarations = crate::mission::interpolate_kdl(
        intent.missions["standing"]
            .declarations_kdl
            .as_deref()
            .unwrap(),
        &std::collections::BTreeMap::from([("ST_MISSION_RUN".into(), "standing".into())]),
    )
    .unwrap();
    let runtime =
        crate::graph::parse_execution_intent(&declarations, "node", "standing").unwrap();
    let desired = &runtime.subjects["agent/standing/chief"].desired;
    assert_eq!(
        crate::graph::declared_authority_blocks(desired),
        ["queue-authority"]
    );

    for source in [
        r#"version 2
mission "bad" state="ready" { goal "Reject empty authority."; agent "bad" { workspace "."; command "true"; queue-authority { } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a prefixed seat."; agent "bad" { workspace "."; command "true"; queue-authority { move "agent/example/worker" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject an internal wildcard."; agent "bad" { workspace "."; command "true"; queue-authority { move "example/*/worker" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a bare wildcard."; agent "bad" { workspace "."; command "true"; queue-authority { move "*" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a duplicate rule."; agent "bad" { workspace "."; command "true"; queue-authority { move "example/*"; move "example/*" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject an unknown verb."; agent "bad" { workspace "."; command "true"; queue-authority { reorder "example/worker" } } }"#,
        r#"version 2
mission "bad" state="ready" { goal "Reject a second block."; agent "bad" { workspace "."; command "true"; queue-authority { move "a" }; queue-authority { move "b" } } }"#,
    ] {
        assert!(
            crate::graph::parse_intent(source, "node").is_err(),
            "{source}"
        );
    }
}

#[test]
fn nested_declarations_are_fully_validated_before_publication() {
    let declarations = [
        r#"agent "worker" { command "true"; unexpected "value" }"#,
        r#"exec "worker" { command "true"; unexpected "value" }"#,
        r#"pty "worker" { command "true"; unexpected "value" }"#,
        r#"host "node" { unexpected "value" }"#,
        r#"doc "proof" { hash "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; unexpected "value" }"#,
        r#"resource "source" { kind "vcs.repository"; unexpected "value" }"#,
        r#"observer "source" { resource "resource/source"; provider "local.file"; locator "/tmp/source"; field "state"; unexpected "value" }"#,
        r#"subscription "source" { observer "observer/source"; to "agent/worker"; on "state"; delivery "message"; unexpected "value" }"#,
        r#"person "reviewer" unexpected=#true"#,
        r#"message "review" { to "agent/worker"; subject "wrong"; body "wrong" }"#,
        r#"schedule "daily" { every "1d"; work { mission "mission/work@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; workspace "/tmp/work" }; unexpected "value" }"#,
        r#"stop "agent/worker" unexpected=#true"#,
    ];

    for declaration in declarations {
        let source = format!(
            "version 2\nmission \"bad\" state=\"ready\" {{ goal \"Reject an invalid nested declaration.\"; step \"work\" {{ agentless; {declaration} }} }}"
        );
        let error = crate::graph::parse_intent(&source, "node")
            .expect_err("a nested declaration must use its full validator");
        assert!(
            matches!(
                error.code,
                "unknown-child" | "unknown-property" | "invalid-host-child"
            ),
            "{declaration}: {error:?}"
        );
    }
}

#[test]
fn nested_validation_uses_typed_input_placeholders() {
    crate::graph::parse_intent(
        r#"version 2
mission "observed" state="ready" {
  input "source" kind="resource"
  goal "Observe the supplied resource."
  step "observe" {
agentless
observer "source" {
  resource "${input.source}"
  provider "local.file"
  locator "/tmp/source"
  field "state"
}
  }
}"#,
        "node",
    )
    .expect("a resource input must remain valid during nested declaration validation");
}

#[test]
fn a_zero_step_mission_is_valid_and_completes_by_default() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "standing" state="ready" { goal "Keep the agent available." } "#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["standing"];
    assert!(mission.steps.is_empty());
    assert_eq!(
        mission.completion,
        Some(crate::model::CompletionSpec::AllStepsExhausted)
    );
}

#[test]
fn a_mission_timeout_is_a_positive_run_deadline() {
    let intent = crate::graph::parse_intent(
        r#"version 2
 mission "bounded" state="ready" timeout="20m" {
   goal "Finish before the run deadline."
 } "#,
        "node",
    )
    .unwrap();
    assert_eq!(intent.missions["bounded"].timeout_ms, Some(1_200_000));

    let error = crate::graph::parse_intent(
        r#"version 2
 mission "unbounded" state="ready" timeout="0ms" {
   goal "Reject a disabled deadline."
 } "#,
        "node",
    )
    .unwrap_err();
    assert_eq!(error.code, "invalid-mission-timeout");
}

#[test]
fn a_mission_reports_to_an_agent_with_a_default_stall_limit() {
    let parse = |header: &str| {
        crate::graph::parse_intent(
            &format!("version 2\nmission \"watched\" state=\"ready\" {header} {{\n  goal \"Report.\"\n}}\n"),
            "node",
        )
    };
    let unset = parse("").unwrap();
    let unset = &unset.missions["watched"];
    assert_eq!(unset.report_to, None);
    assert_eq!(unset.stalled_after_ms, None);
    assert!(!unset.report_completed);
    assert!(unset.report_stalls);
    // An unset report changes nothing about the published definition.
    assert!(!serde_json::to_string(unset).unwrap().contains("report"));

    let default = parse(r#"report-to="agent/ops/watcher""#).unwrap();
    let default = &default.missions["watched"];
    assert_eq!(default.report_to.as_deref(), Some("agent/ops/watcher"));
    assert_eq!(default.stalled_after_ms, Some(30 * 60_000));
    assert!(!default.report_completed);
    assert!(default.report_stalls);

    let tuned = parse(
        r#"report-to="agent/ops/watcher" stalled-after="45m" report-completed=#true"#,
    )
    .unwrap();
    let tuned = &tuned.missions["watched"];
    assert_eq!(tuned.stalled_after_ms, Some(45 * 60_000));
    assert!(tuned.report_completed);
    assert!(tuned.report_stalls);
    let explicit_default = parse(r#"report-to="agent/ops/watcher" report-stalls=#true"#).unwrap();
    assert_eq!(default.revision, explicit_default.missions["watched"].revision);
    let muted = parse(r#"report-to="agent/ops/watcher" report-stalls=#false"#).unwrap();
    let muted = &muted.missions["watched"];
    assert!(!muted.report_stalls);
    assert_eq!(muted.stalled_after_ms, Some(30 * 60_000));
    assert_ne!(default.revision, muted.revision);
    // Older stored definitions decode and serialize byte-for-byte without a new field.
    let old = serde_json::to_value(default).unwrap();
    assert!(old.get("report_stalls").is_none());
    let replay: crate::model::MissionSpec = serde_json::from_value(old.clone()).unwrap();
    assert!(replay.report_stalls);
    assert_eq!(serde_json::to_value(replay).unwrap(), old);

}

#[test]
fn a_run_report_names_an_agent_and_its_options_need_one() {
    for (header, code) in [
        (r#"report-to="person/ada""#, "invalid-report-to"),
        (r#"report-to="agent/""#, "invalid-report-to"),
        (r#"report-to="agent/a b""#, "invalid-report-to"),
        (r#"report-to="agent/ops/watcher" stalled-after="0m""#, "invalid-stalled-after"),
        (r#"report-to="agent/ops/watcher" stalled-after="soon""#, "invalid-duration"),
        (r#"stalled-after="30m""#, "report-without-recipient"),
        (r#"report-completed=#true"#, "report-without-recipient"),
        (r#"report-stalls=#false"#, "report-without-recipient"),
        (r#"report-stalls=#true"#, "report-without-recipient"),
        (r#"report-to="agent/ops/watcher" report-stalls="false""#, "invalid-property"),
        (r#"report-to="agent/ops/watcher" report-stalls=0"#, "invalid-property"),
        (r#"report-to="agent/ops/watcher" report-stalls=#false stalled-after="0m""#, "invalid-stalled-after"),
    ] {
        let error = crate::graph::parse_intent(
            &format!("version 2\nmission \"bad\" state=\"ready\" {header} {{\n  goal \"Reject.\"\n}}\n"),
            "node",
        )
        .unwrap_err();
        assert_eq!(error.code, code, "{header}");
    }
}

#[test]
fn goal_limits_and_removed_mission_language_are_strict() {
    for source in [
        r#"version 2
 mission "none" state="ready" { step "work" { } } "#,
        r#"version 2
 mission "four" state="ready" { goal "1"; goal "2"; goal "3"; goal "4"; step "work" { } } "#,
        r#"version 2
 mission "step-four" state="ready" { goal "Run."; step "work" { goal "1"; goal "2"; goal "3"; goal "4" } } "#,
    ] {
        assert_eq!(
            crate::graph::parse_intent(source, "node").unwrap_err().code,
            "invalid-goal-count"
        );
    }
    for removed in [
        r#"version 2
 mission "old" state="ready" { goal "Run."; outcome { } ; step "work" { } } "#,
        r#"version 2
 mission "old" state="ready" { goal "Run."; judges { } ; step "work" { } } "#,
        r#"version 2
 mission "old" state="ready" { goal "Run."; step "work" { judge "old" { exec "true" } } } "#,
        r#"version 2
 mission "old" state="ready" change-policy="agent" { goal "Run."; step "work" { } } "#,
        r#"version 2
 mission "old" state="ready" change-authority="agent/worker" { goal "Run."; step "work" { } } "#,
    ] {
        assert!(crate::graph::parse_intent(removed, "node").is_err());
    }
}

#[test]
fn revision_authority_comes_from_graph_placement() {
    let intent = crate::graph::parse_intent(
        r#"
version 2

  mission "placed" state="ready" revisions="human-only" revision-reviewer="person/mission" revision-cutover="when-idle" {
goal "Test revision placement."
 agent "mission-owner" { workspace "."; command "true" }
step "work" revisions="human-only" revision-reviewer="person/step" {
  assigned-to "agent/assignee"
   agent "step-owner" { workspace "."; command "true" }
}
  }

"#,
        "node",
    )
    .unwrap();
    let mission = &intent.missions["placed"];
    assert_eq!(
        mission.revision_owners,
        vec!["agent/${ST_MISSION_RUN}/mission-owner"]
    );
    assert!(mission.revisions_human_only);
    assert_eq!(mission.revision_reviewer.as_deref(), Some("person/mission"));
    assert_eq!(
        mission.revision_cutover,
        crate::model::RevisionCutover::WhenIdle
    );
    let step = &mission.steps["work"];
    assert_eq!(
        step.revision_owners,
        vec!["agent/${ST_MISSION_RUN}/step-owner"]
    );
    assert!(step.revisions_human_only);
    assert_eq!(step.revision_reviewer.as_deref(), Some("person/step"));
    assert!(!step.revision_owners.contains(&"agent/assignee".into()));

    let separate_seat = crate::graph::parse_intent(
        r#"
version 2
agent "example/reviewer" { workspace "."; command "true" }
mission "finite" state="ready" {
  goal "Keep root seats separate from mission authority."
  step "work" { assigned-to "agent/example/reviewer" }
}
"#,
        "node",
    )
    .unwrap();
    assert!(separate_seat.missions["finite"].revision_owners.is_empty());
}

#[test]
fn reserved_context_variables_cannot_be_authored() {
    let reserved = crate::graph::parse_intent(
        r#"
version 2

  mission "reserved" state="ready" {
goal "Reject a context override."
step "work" {
   exec "task" { command "true"; env { ST_MISSION_RUN "forged" } }
}
  }

"#,
        "node",
    )
    .unwrap_err();
    assert_eq!(reserved.code, "reserved-context-variable");

    crate::graph::parse_intent(
        r#"
version 2

  mission "allowed" state="ready" {
goal "Allow an application variable."
step "work" {
   exec "task" { command "true"; env { ST_ROOT "allowed" } }
}
  }

"#,
        "node",
    )
    .unwrap();
}

#[test]
fn mission_inputs_and_run_limits_use_the_explicit_language() {
    let intent = crate::graph::parse_intent(
        r#"
version 2

  mission "parameterized" state="ready" {
input "message" kind="text"
input "source" kind="resource"
concurrent-runs max=4
goal "Process ${input.message}."
step "work" {
  agentless
   exec "task" { command "printf '%s' '${input.message}'"; restart "never" }
  gate "the source is ready" { field "state" "${input.source}" is "ready" }
}
  }
  mission "unbounded" state="ready" {
concurrent-runs
goal "Allow concurrent runs."
  }

"#,
        "node",
    )
    .unwrap();
    let parameterized = &intent.missions["parameterized"];
    assert_eq!(parameterized.inputs.len(), 2);
    assert_eq!(
        parameterized.inputs["message"].kind,
        crate::model::MissionInputKind::Text
    );
    assert_eq!(
        parameterized.inputs["source"].kind,
        crate::model::MissionInputKind::Resource
    );
    assert_eq!(parameterized.max_active_runs, Some(4));
    assert_eq!(intent.missions["unbounded"].max_active_runs, None);

    for source in [
        r#"version 2
 mission "bad" state="ready" { input "x" kind="text"; input "x" kind="text"; goal "Reject duplicate input." } "#,
        r#"version 2
 mission "bad" state="ready" { input "x" kind="secret"; goal "Reject the input kind." } "#,
        r#"version 2
 mission "bad" state="ready" { concurrent-runs max=0; goal "Reject the run limit." } "#,
        r#"version 2
 mission "bad" state="ready" { goal "Use ${input.missing}." } "#,
        r#"version 2
 mission "bad" state="ready" { agentless; goal "Reject a mission selector." } "#,
    ] {
        assert!(crate::graph::parse_intent(source, "node").is_err());
    }
}
