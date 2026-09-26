use std::fs;
use std::path::PathBuf;

use agent_spec::spec::Driver;
use sha2::{Digest as _, Sha256};

fn assigned_to(step: &st3::model::StepSpec) -> Option<&str> {
    match &step.work_selector {
        Some(st3::model::WorkSelector::Assigned { agent }) => Some(agent),
        _ => None,
    }
}

fn authored_harness_counts(source: &str, source_name: &str) -> (usize, usize) {
    fn visit(document: &kdl::KdlDocument, source_name: &str, counts: &mut (usize, usize)) {
        for node in document.nodes() {
            if node.name().value() == "agent" {
                let children = node.children().expect("an agent must have children");
                let harnesses = children
                    .nodes()
                    .iter()
                    .filter(|child| child.name().value() == "harness")
                    .collect::<Vec<_>>();
                if children.get("command").is_some() {
                    assert!(
                        harnesses.is_empty(),
                        "{source_name} command agents must not declare a harness"
                    );
                    if let Some(children) = node.children() {
                        visit(children, source_name, counts);
                    }
                    continue;
                }
                assert_eq!(
                    harnesses.len(),
                    1,
                    "{source_name} agents must declare one native harness"
                );
                let provider = harnesses[0]
                    .entries()
                    .first()
                    .and_then(|entry| entry.value().as_string())
                    .expect("a harness must name its provider");
                match provider {
                    "claude" => {
                        let model = harnesses[0]
                            .children()
                            .and_then(|body| body.get("model"))
                            .and_then(|model| model.entries().first())
                            .and_then(|entry| entry.value().as_string());
                        let expected_model = if source_name.contains("cross-harness-message-wake") {
                            "opus"
                        } else {
                            "claude-sonnet-5"
                        };
                        assert_eq!(model, Some(expected_model), "{source_name} Claude model");
                        counts.0 += 1;
                    }
                    "codex" => counts.1 += 1,
                    // Pi and OMP use the configured account model and are asserted explicitly by
                    // the one eval that owns those native transport seats. This historical tuple
                    // remains the Claude/Codex inventory used by the paired corpus checks below.
                    "pi" | "omp" => {}
                    provider => panic!("{source_name} uses unexpected harness {provider}"),
                }
            }
            if let Some(children) = node.children() {
                visit(children, source_name, counts);
            }
        }
    }

    let document: kdl::KdlDocument = source.parse().expect("parse harness inventory KDL");
    let mut counts = (0, 0);
    visit(&document, source_name, &mut counts);
    counts
}

fn authored_model_gates(source: &str) -> Vec<String> {
    fn visit(document: &kdl::KdlDocument, models: &mut Vec<String>) {
        for node in document.nodes() {
            let is_llm_gate = node.name().value() == "gate"
                && node.entries().iter().any(|entry| {
                    entry.name().map(|name| name.value()) == Some("type")
                        && entry.value().as_string() == Some("llm")
                });
            if is_llm_gate {
                let model = node
                    .children()
                    .and_then(|body| body.get("model"))
                    .and_then(|model| model.entries().first())
                    .and_then(|entry| entry.value().as_string())
                    .expect("a model gate must declare its model");
                models.push(model.to_owned());
            }
            if let Some(children) = node.children() {
                visit(children, models);
            }
        }
    }

    let document: kdl::KdlDocument = source.parse().expect("parse model gate KDL");
    let mut models = Vec::new();
    visit(&document, &mut models);
    models
}

#[test]
fn planning_mode_eval_uses_one_dynamic_codex_planner() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/planning-mode");
    let source = fs::read_to_string(root.join("eval.kdl")).expect("read planning mode eval");
    assert_eq!(
        authored_harness_counts(&source, "planning-mode/eval.kdl"),
        (0, 0),
        "the launch API, not the eval KDL, owns the durable planner"
    );
    let controller =
        fs::read_to_string(root.join("controller.sh")).expect("read planning controller");
    assert!(controller.contains("launch start"));
    assert!(controller.contains("--model gpt-5.6-sol"));
    assert!(!controller.contains("claude"));
}

#[test]
fn every_tracked_st3_example_uses_the_normative_grammar() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/st3");
    let mut files = walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("walk examples")
        .into_iter()
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("kdl"))
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty());
    for file in files {
        let source = fs::read_to_string(&file).expect("read example");
        st3::parse_intent(&source, "local")
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
    }
}

#[test]
fn walkthrough_work_fails_fast_before_the_standing_agent_exists() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/st3/walkthrough-work.kdl");
    let source = fs::read_to_string(&file).expect("read walkthrough work mission");
    let intent = st3::parse_intent(&source, "local").expect("parse walkthrough work mission");
    let store = st3::store::Store::open_memory("local").unwrap();
    let preview = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source,
                source_name: Some(file.display().to_string()),
            },
        )
        .unwrap();

    assert_eq!(
        preview.blockers,
        [
            "message `message/example/garden-work-ready` references undeclared recipient `agent/example/garden-owner/standing/owner`"
        ]
    );
}

#[test]
fn the_declarative_gates_dream_fixture_previews_without_rewriting() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/declarative-gates.kdl");
    let source = fs::read_to_string(&fixture).expect("read declarative gates fixture");
    let intent = st3::parse_intent(&source, "local")
        .unwrap_or_else(|error| panic!("{}: {error}", fixture.display()));
    let store = st3::store::Store::open_memory("local").unwrap();
    let preview = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source,
                source_name: Some(fixture.display().to_string()),
            },
        )
        .unwrap();
    assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
    assert!(
        preview
            .mission_revisions
            .contains_key("mission/fleet/app-web/one-space-gets-instant-new-items")
    );
}

#[test]
fn every_native_st3_eval_uses_the_normative_grammar() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    let mut files = walkdir::WalkDir::new(&root)
        .max_depth(2)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("walk evals")
        .into_iter()
        .filter(|entry| entry.file_name() == "eval.kdl")
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty());
    for file in files {
        let source = fs::read_to_string(&file).expect("read eval");
        st3::parse_intent(&source, "local")
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
    }
}

#[test]
fn eval_agents_use_the_runtime_boot_contract_without_authored_prompts() {
    fn inspect(document: &kdl::KdlDocument, file: &std::path::Path) {
        for node in document.nodes() {
            if node.name().value() == "agent" {
                let body = node.children().expect("an agent needs a body");
                for harness in body
                    .nodes()
                    .iter()
                    .filter(|child| child.name().value() == "harness")
                {
                    assert!(
                        harness
                            .children()
                            .is_none_or(|body| body.get("prompt").is_none()),
                        "{} authors an eval-specific harness prompt",
                        file.display()
                    );
                }
            }
            if let Some(children) = node.children() {
                inspect(children, file);
            }
        }
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    for entry in walkdir::WalkDir::new(root)
        .max_depth(2)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "eval.kdl")
    {
        let source = fs::read_to_string(entry.path()).unwrap();
        let document = source.parse::<kdl::KdlDocument>().unwrap();
        inspect(&document, entry.path());
    }
}

#[test]
fn eval_personas_do_not_duplicate_runtime_work_instructions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
        {
            continue;
        }
        if !entry
            .path()
            .components()
            .any(|part| part.as_os_str() == "personas")
        {
            continue;
        }
        let source = fs::read_to_string(entry.path()).unwrap();
        for forbidden in [
            "## Boot ritual",
            "st3 work claim",
            "st3 conversations ls",
            "st3 work progress",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} duplicates runtime instruction `{forbidden}`",
                entry.path().display()
            );
        }
    }
}

#[test]
fn continuous_stewardship_has_two_serial_cycles_and_a_latest_only_wake() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/continuous-stewardship/eval.kdl");
    let source = fs::read_to_string(&file).unwrap();
    let intent = st3::parse_intent(&source, "local").unwrap();
    let mission = &intent.missions["eval/continuous-stewardship"];
    let cycle = &intent.missions["eval/continuous-stewardship/cycle"];
    assert!(
        source.contains(&format!("cycle@{}", cycle.revision)),
        "{}",
        cycle.revision
    );
    assert!(source.contains("catch-up \"latest\""));
    assert!(mission.steps["cycle-one"].nested_mission.is_some());
    assert!(mission.steps["cycle-two"].nested_mission.is_some());
    assert!(mission.steps["restart-steward"].declarations_kdl.is_some());
    assert_eq!(
        mission.display_order,
        [
            "cycle-one",
            "restart-steward",
            "cycle-two",
            "held-out-gates",
            "cleanup-steward",
        ]
    );
}

#[test]
fn automatic_intake_pins_its_review_mission() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/automatic-github-intake/eval.kdl");
    let source = fs::read_to_string(&file).unwrap();
    let intent = st3::parse_intent(&source, "local").unwrap();
    let revision = &intent.missions["eval/automatic-github-intake/review"].revision;
    assert!(source.contains(&format!("review@{revision}")), "{revision}");
}

#[test]
fn migration_rehearsal_uses_an_exact_migration_document_and_no_custom_prompt() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/agent-migration-rehearsal/eval.kdl");
    let source = fs::read_to_string(&file).unwrap();
    let intent = st3::parse_intent(&source, "local").unwrap();
    assert!(intent.document_refs.contains(
        "doc/hosts/migration-rehearsal@7a0773a714975b4563b15638cc5b5b6608849dc6c5b1ea07e2632b0edc6a8665"
    ));
    assert!(!source.contains("prompt "));
}

#[test]
fn st3_eval_inventory_has_twenty_two_model_free_and_twenty_one_model_backed_evals() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    let model_free = [
        "attention-inbox",
        "constraint-inheritance",
        "context-resource-continuity",
        "crash-escalation",
        "local-file-refresh",
        "loop-exhaustion-attention",
        "loop-feedback",
        "loop-until-green",
        "network-isolation",
        "network-smoke",
        "mission-inputs",
        "mission-authority",
        "pty-attach-machine-stream",
        "pty-attach-only",
        "pty-send-peek",
        "queue-revision",
        "replication-convergence",
        "review-escalation",
        "resource-cold-start",
        "resource-handoff",
        "resource-retarget",
        "runtime-readiness-recovery",
    ];
    let model_backed = [
        "agent-migration-rehearsal",
        "automatic-github-intake",
        "claude-skill-inheritance",
        "codex-compaction-message-delivery",
        "continuous-stewardship",
        "cross-harness-message-wake",
        "fork-in-the-road",
        "ghost-bug",
        "license-mit",
        "mixed-worker-pool",
        "mission-document-lift",
        "native-session-import",
        "planning-mode",
        "poisoned-pr",
        "restart-continuity",
        "run-generation-revision",
        "seat-mission-work",
        "signal-rename",
        "test-writing",
        "weird-git-setup",
        "work-wake-reliability",
    ];
    let mut actual = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("eval.kdl").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = model_free
        .iter()
        .chain(model_backed.iter())
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);

    for name in model_free {
        let source = fs::read_to_string(root.join(name).join("eval.kdl")).unwrap();
        assert_eq!(authored_harness_counts(&source, name), (0, 0), "{name}");
        assert!(authored_model_gates(&source).is_empty(), "{name}");
    }
    for name in model_backed {
        let source = fs::read_to_string(root.join(name).join("eval.kdl")).unwrap();
        let has_authored_model = authored_harness_counts(&source, name) != (0, 0)
            || !authored_model_gates(&source).is_empty();
        let controller = root.join(name).join("controller.sh");
        let dynamic_model = controller.is_file() && {
            let controller = fs::read_to_string(controller).unwrap();
            (matches!(name, "planning-mode" | "run-generation-revision")
                && controller.contains("launch start"))
                || (name == "native-session-import" && controller.contains("pty run"))
        };
        assert!(
            has_authored_model || dynamic_model,
            "{name} must use a model"
        );
    }
}

#[test]
fn codex_compaction_delivery_eval_proves_staging_steering_and_post_compaction_delivery() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/codex-compaction-message-delivery");
    let source = fs::read_to_string(root.join("eval.kdl")).unwrap();
    assert_eq!(
        authored_harness_counts(&source, "codex-compaction-message-delivery/eval.kdl"),
        (0, 1)
    );
    let controller = fs::read_to_string(root.join("controller.sh")).unwrap();
    for required in [
        "message.staged",
        "message.delivered",
        "message.read",
        "/compact",
        "last_compaction_trigger",
        "POST-COMPACT-SEEN",
    ] {
        assert!(
            controller.contains(required),
            "missing `{required}` scenario"
        );
    }
}

#[test]
fn native_session_import_eval_uses_raw_ptys_and_all_five_drivers() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/native-session-import");
    let source = fs::read_to_string(root.join("eval.kdl")).unwrap();
    assert_eq!(
        authored_harness_counts(&source, "native-session-import/eval.kdl"),
        (0, 0),
        "the five harnesses must begin outside st3"
    );
    let controller = fs::read_to_string(root.join("controller.sh")).unwrap();
    assert!(controller.contains("env -u PTY_SESSION PTY_ROOT=\"$RAW_PTY_ROOT\" pty"));
    assert!(controller.contains(".name == \\$id and .status == \\\"running\\\""));
    assert!(!controller.contains("st3 terminals send"));
    for driver in ["codex", "claude", "pi", "omp", "opencode"] {
        assert!(
            controller.contains(&format!("    {driver})")),
            "missing raw {driver} launch"
        );
    }
    for required in [
        "exact_session == true",
        "st3 import run",
        "raw process",
        "owner_run_id == null",
        "managed_process_has_native_id",
    ] {
        assert!(controller.contains(required), "missing `{required}` proof");
    }
}

#[test]
fn cross_harness_message_wake_is_black_box_and_uses_all_account_backed_harnesses() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/cross-harness-message-wake");
    let source = fs::read_to_string(root.join("eval.kdl")).unwrap();
    assert_eq!(
        authored_harness_counts(&source, "cross-harness-message-wake/eval.kdl"),
        (1, 1)
    );
    assert_eq!(source.matches("harness \"pi\"").count(), 1);
    assert_eq!(source.matches("harness \"omp\"").count(), 1);

    for name in ["participant.md"] {
        let prompt = fs::read_to_string(root.join("prompts").join(name)).unwrap();
        let prompt = prompt.to_ascii_lowercase();
        for leaked_failure_mode in ["pty", "terminal", "workaround"] {
            assert!(
                !prompt.contains(leaked_failure_mode),
                "{name} primes the agent with `{leaked_failure_mode}`"
            );
        }
    }

    assert!(source.contains("judges/no-terminal-input.sh"));
    assert!(source.contains("cross-harness-message-wake/controller"));

    let controller = fs::read_to_string(root.join("controller.sh")).unwrap();
    assert!(controller.contains(".value.harness_state"));
    assert!(!controller.contains(".actual.status"));
}

#[test]
fn work_wake_reliability_covers_normal_lifecycle_wakes_without_priming_the_worker() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/work-wake-reliability");
    let source = fs::read_to_string(root.join("eval.kdl")).unwrap();
    assert_eq!(
        authored_harness_counts(&source, "work-wake-reliability/eval.kdl"),
        (0, 1)
    );
    assert!(source.contains("restart \"always\""));

    let controller = fs::read_to_string(root.join("controller.sh")).unwrap();
    assert!(controller.contains(".value.harness_state"));
    assert!(!controller.contains(".actual.status"));
    for required in [
        "missions start",
        "work revise",
        "terminals signal",
        "missions cancel",
    ] {
        assert!(
            controller.contains(required),
            "missing `{required}` scenario"
        );
    }

    for entry in fs::read_dir(root.join("fixtures")).unwrap() {
        let fixture = fs::read_to_string(entry.unwrap().path()).unwrap();
        for leaked_failure_mode in ["pty", "terminal", "workaround"] {
            assert!(
                !fixture.to_ascii_lowercase().contains(leaked_failure_mode),
                "worker fixture primes the agent with `{leaked_failure_mode}`"
            );
        }
    }
}

#[test]
fn every_selected_eval_has_one_st2_and_one_st3_form() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals");
    let model_free = [
        "resource-cold-start",
        "resource-retarget",
        "resource-handoff",
        "context-resource-continuity",
        "crash-escalation",
        "pty-attach-machine-stream",
        "pty-attach-only",
        "pty-send-peek",
        "network-smoke",
        "network-isolation",
    ];
    for name in [
        "license-mit",
        "ghost-bug",
        "signal-rename",
        "restart-continuity",
        "fork-in-the-road",
        "poisoned-pr",
        "test-writing",
        "weird-git-setup",
        "claude-skill-inheritance",
        "resource-cold-start",
        "resource-retarget",
        "resource-handoff",
        "context-resource-continuity",
        "crash-escalation",
        "pty-attach-machine-stream",
        "pty-attach-only",
        "pty-send-peek",
        "network-smoke",
        "network-isolation",
    ] {
        let st2_file = root.join("st2").join(name).join("eval.kdl");
        let st3_file = root.join("st3").join(name).join("eval.kdl");
        let st2_source = fs::read_to_string(&st2_file).expect("read st2 eval");
        let st3_source = fs::read_to_string(&st3_file).expect("read st3 eval");
        let st2_document: kdl::KdlDocument = st2_source.parse().expect("parse st2 KDL");
        let st3_document: kdl::KdlDocument = st3_source.parse().expect("parse st3 KDL");

        assert_eq!(
            st2::kdl_version::document_version(&st2_document).unwrap(),
            1
        );
        assert_eq!(
            st2::kdl_version::document_version(&st3_document).unwrap(),
            2
        );
        let st2_spec = st2::eval_spec::parse_spec(&st2_source)
            .unwrap_or_else(|error| panic!("{}: {error:#}", st2_file.display()));
        st3::parse_intent(&st3_source, "local")
            .unwrap_or_else(|error| panic!("{}: {error}", st3_file.display()));
        let st2_agents = st2_spec.agents.iter().chain(
            st2_spec
                .eval
                .iter()
                .flat_map(|evaluation| evaluation.agents.iter()),
        );
        if model_free.contains(&name) {
            for agent in st2_agents {
                assert!(
                    agent.driver.is_none(),
                    "{} model-free agent {} must not use a harness",
                    st2_file.display(),
                    agent.id
                );
            }
            assert_eq!(
                authored_harness_counts(&st3_source, &st3_file.display().to_string()),
                (0, 0),
                "{} must remain model-free",
                st3_file.display()
            );
            assert!(
                authored_model_gates(&st3_source).is_empty(),
                "{} must not use an LLM gate",
                st3_file.display()
            );
        } else {
            for agent in st2_agents {
                assert!(
                    agent.command.is_none() && agent.driver.is_some(),
                    "{} agent {} must use a native harness",
                    st2_file.display(),
                    agent.id
                );
            }
            assert!(
                authored_harness_counts(&st3_source, &st3_file.display().to_string()) != (0, 0),
                "{} must contain native agent harnesses",
                st3_file.display()
            );
        }
        assert!(st2::eval_spec::parse_spec(&st3_source).is_err());
        assert!(st3::parse_intent(&st2_source, "local").is_err());
    }
}

#[test]
fn selected_eval_harness_counts_match_the_inventory() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals");
    let expected = [
        ("license-mit", (2, 1), (2, 0)),
        ("ghost-bug", (0, 2), (0, 2)),
        ("signal-rename", (0, 4), (0, 4)),
        ("restart-continuity", (2, 0), (2, 0)),
        ("fork-in-the-road", (0, 4), (0, 4)),
        ("poisoned-pr", (0, 2), (0, 2)),
        ("test-writing", (0, 2), (0, 2)),
        ("weird-git-setup", (0, 1), (0, 1)),
        ("claude-skill-inheritance", (1, 0), (1, 0)),
    ];
    let mut model_gates = Vec::new();

    for (name, st2_expected, st3_expected) in expected {
        let st2_source = fs::read_to_string(root.join("st2").join(name).join("eval.kdl")).unwrap();
        let st2_spec = st2::eval_spec::parse_spec(&st2_source).unwrap();
        let mut st2_counts = (0, 0);
        for agent in st2_spec.agents.iter().chain(
            st2_spec
                .eval
                .iter()
                .flat_map(|evaluation| evaluation.agents.iter()),
        ) {
            match agent.driver.as_ref().expect("native st2 harness") {
                Driver::Claude(driver) => {
                    assert_eq!(
                        driver.model.as_deref(),
                        Some("claude-sonnet-5"),
                        "st2 {name} Claude seats must use Sonnet"
                    );
                    st2_counts.0 += 1;
                }
                Driver::Codex(_) => st2_counts.1 += 1,
                driver => panic!("unexpected st2 harness {}", driver.name()),
            }
        }
        assert_eq!(st2_counts, st2_expected, "st2 {name}");

        let st3_source = fs::read_to_string(root.join("st3").join(name).join("eval.kdl")).unwrap();
        let st3_counts = authored_harness_counts(&st3_source, &format!("st3 {name}"));
        assert_eq!(st3_counts, st3_expected, "st3 {name}");
        model_gates.extend(authored_model_gates(&st3_source));
    }
    let mission_lift = fs::read_to_string(root.join("st3/mission-document-lift/eval.kdl")).unwrap();
    assert_eq!(
        authored_harness_counts(&mission_lift, "st3 mission-document-lift"),
        (0, 1)
    );
    model_gates.extend(authored_model_gates(&mission_lift));
    model_gates.sort();
    assert_eq!(
        model_gates,
        ["gpt-5.6-sol", "gpt-5.6-sol", "gpt-5.6-sol"],
        "the model gate inventory changed; update evals/README.md"
    );
}

#[test]
fn license_and_ghost_bug_keep_the_complete_team_loop_in_the_graph() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    let cases = [
        (
            "license-mit",
            "eval/license-mit",
            [
                ("delegate-license-change", "agent/${ST_MISSION_RUN}/lmc.sup"),
                (
                    "implement-license-change",
                    "agent/${ST_MISSION_RUN}/lmc.worker",
                ),
                ("verify-and-confirm", "agent/${ST_MISSION_RUN}/lmc.sup"),
            ]
            .as_slice(),
            [
                "resource/mission-run/${ST_MISSION_RUN}/license-brief",
                "resource/mission-run/${ST_MISSION_RUN}/license-revision",
                "resource/mission-run/${ST_MISSION_RUN}/worker-report",
                "resource/mission-run/${ST_MISSION_RUN}/final-confirmation",
            ]
            .as_slice(),
        ),
        (
            "ghost-bug",
            "eval/ghost-bug-codex",
            [
                ("delegate-debug-brief", "agent/${ST_MISSION_RUN}/gbx.sup"),
                ("diagnose-and-fix", "agent/${ST_MISSION_RUN}/gbx.fix"),
                ("verify-and-confirm", "agent/${ST_MISSION_RUN}/gbx.sup"),
            ]
            .as_slice(),
            [
                "resource/mission-run/${ST_MISSION_RUN}/debug-brief",
                "resource/mission-run/${ST_MISSION_RUN}/fix-revision",
                "resource/mission-run/${ST_MISSION_RUN}/worker-report",
                "resource/mission-run/${ST_MISSION_RUN}/final-confirmation",
            ]
            .as_slice(),
        ),
    ];

    for (name, mission_id, assignments, products) in cases {
        let source = fs::read_to_string(root.join(name).join("eval.kdl")).unwrap();
        assert!(!source.contains("message \"kickoff"));
        assert!(!source.contains("wait-team-done"));
        let intent = st3::parse_intent(&source, "local").unwrap();
        let mission = &intent.missions[mission_id];
        for (step, assignee) in assignments {
            assert_eq!(assigned_to(&mission.steps[*step]), Some(*assignee));
            assert!(mission.steps[*step].nested_mission.is_some());
        }
        let declared = mission
            .steps
            .values()
            .filter_map(|step| step.nested_mission.as_ref())
            .flat_map(|nested| nested.steps.values())
            .flat_map(|step| &step.products)
            .map(|product| product.subject.as_str())
            .collect::<Vec<_>>();
        for product in products {
            assert!(declared.contains(product), "{name} lacks {product}");
        }
    }
}

#[test]
fn mission_document_lift_produces_and_uses_one_exact_mission_output() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/mission-document-lift");
    let source = fs::read_to_string(root.join("eval.kdl")).unwrap();
    let intent = st3::parse_intent(&source, "local").unwrap();
    let mission = &intent.missions["eval/mission-document-lift"];
    assert_eq!(mission.baselines.len(), 1);
    assert_eq!(
        mission.baselines[0].name,
        "the exact source mission exists before planning starts"
    );
    assert_eq!(
        mission.steps["lift-mission-document"]
            .produces_mission
            .as_deref(),
        Some("eval/mission-document-lift/work")
    );
    assert_eq!(
        mission.steps["execute-lifted-mission"].uses_mission,
        Some(st3::model::UsedMissionSpec::StepOutput {
            step: "lift-mission-document".into()
        })
    );
    let document = fs::read(root.join("repo/MISSION.md")).unwrap();
    let hash = hex::encode(Sha256::digest(&document));
    let reference = format!("doc/evals/mission-document-lift/mission@{hash}");
    assert!(intent.document_refs.contains(&reference));
    assert_eq!(
        fs::read(root.join(".st3-documents").join(hash)).unwrap(),
        document
    );
}

#[test]
fn license_mit_st3_fixture_matches_its_claude_team() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/license-mit");
    for seat in ["sup", "worker"] {
        let workspace = root.join(seat);
        assert_eq!(
            fs::read_to_string(workspace.join("CLAUDE.md")).unwrap(),
            "@PERSONA.md\n"
        );
        assert!(workspace.join("PERSONA.md").is_file());
        assert!(!workspace.join("AGENTS.md").exists());
        assert!(!workspace.join(".codex/hooks.json").exists());
    }
}

#[test]
fn restart_continuity_fixtures_match_their_claude_teams() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals");
    for runtime in ["st2", "st3"] {
        for seat in ["sup", "worker"] {
            let workspace = if runtime == "st2" {
                root.join(runtime)
                    .join("restart-continuity/fixture")
                    .join(seat)
            } else {
                root.join(runtime).join("restart-continuity").join(seat)
            };
            assert_eq!(
                fs::read_to_string(workspace.join("CLAUDE.md")).unwrap(),
                "@PERSONA.md\n"
            );
            assert!(workspace.join("PERSONA.md").is_file());
            assert!(!workspace.join("AGENTS.md").exists());
        }
    }

    let st2_source = fs::read_to_string(root.join("st2/restart-continuity/eval.kdl")).unwrap();
    let st2_spec = st2::eval_spec::parse_spec(&st2_source).unwrap();
    let supervisor = st2_spec
        .agents
        .iter()
        .find(|agent| agent.id == "rc.sup")
        .expect("restart supervisor");
    assert!(
        supervisor
            .execs
            .iter()
            .any(|process| process.id == "rc.sup.injector")
    );
}

#[test]
fn signal_rename_keeps_work_structure_in_the_mission_graph() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/signal-rename/eval.kdl");
    let source = fs::read_to_string(&file).expect("read Signal Rename eval");
    assert!(!source.contains("wait-team-done"));
    assert!(!source.contains("message \"kickoff"));

    let intent = st3::parse_intent(&source, "local").expect("parse Signal Rename eval");
    let mission = &intent.missions["eval/signal-rename-codex"];
    assert_eq!(
        mission.display_order,
        [
            "materialize",
            "start-team",
            "open-base-compatibility",
            "migrate-relay",
            "migrate-hub",
            "update-root-and-config",
            "close-base-compatibility",
            "integrate-and-verify",
            "held-out-gates",
            "publish-final-report",
        ]
    );

    let expected_assignments = [
        (
            "open-base-compatibility",
            "agent/${ST_MISSION_RUN}/sig.base",
        ),
        ("migrate-relay", "agent/${ST_MISSION_RUN}/sig.relay"),
        ("migrate-hub", "agent/${ST_MISSION_RUN}/sig.hub"),
        ("update-root-and-config", "agent/${ST_MISSION_RUN}/sig.sup"),
        (
            "close-base-compatibility",
            "agent/${ST_MISSION_RUN}/sig.base",
        ),
        ("integrate-and-verify", "agent/${ST_MISSION_RUN}/sig.sup"),
        ("publish-final-report", "agent/${ST_MISSION_RUN}/sig.sup"),
    ];
    for (step, assignee) in expected_assignments {
        let step = &mission.steps[step];
        assert_eq!(assigned_to(step), Some(assignee));
        assert!(step.nested_mission.is_some());
    }

    let dependencies = |step: &str| {
        mission.steps[step]
            .dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                st3::model::DependencySpec::Step { step, state } if state == "completed" => {
                    Some(step.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(dependencies("open-base-compatibility"), ["start-team"]);
    assert_eq!(dependencies("migrate-relay"), ["open-base-compatibility"]);
    assert_eq!(dependencies("migrate-hub"), ["open-base-compatibility"]);
    assert_eq!(
        dependencies("update-root-and-config"),
        ["migrate-relay", "migrate-hub"]
    );
    assert_eq!(
        dependencies("close-base-compatibility"),
        ["update-root-and-config"]
    );
    assert_eq!(
        dependencies("integrate-and-verify"),
        ["close-base-compatibility"]
    );
    assert_eq!(dependencies("held-out-gates"), ["integrate-and-verify"]);
    assert_eq!(dependencies("publish-final-report"), ["held-out-gates"]);

    let required_products = [
        (
            "open-base-compatibility",
            "resource/mission-run/${ST_MISSION_RUN}/base-compatibility",
        ),
        (
            "migrate-relay",
            "resource/mission-run/${ST_MISSION_RUN}/relay-revision",
        ),
        (
            "migrate-hub",
            "resource/mission-run/${ST_MISSION_RUN}/hub-revision",
        ),
        (
            "update-root-and-config",
            "resource/mission-run/${ST_MISSION_RUN}/config-revision",
        ),
        (
            "close-base-compatibility",
            "resource/mission-run/${ST_MISSION_RUN}/base-final-revision",
        ),
        (
            "integrate-and-verify",
            "resource/mission-run/${ST_MISSION_RUN}/integrated-revision",
        ),
        (
            "publish-final-report",
            "resource/mission-run/${ST_MISSION_RUN}/final-report",
        ),
    ];
    for (parent, product) in required_products {
        let nested = mission.steps[parent]
            .nested_mission
            .as_ref()
            .expect("nested mission");
        assert!(
            nested
                .steps
                .values()
                .flat_map(|step| &step.products)
                .any(|candidate| candidate.subject == product)
        );
    }
}

#[test]
fn restart_continuity_keeps_recovery_state_in_the_mission_graph() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/restart-continuity/eval.kdl");
    let source = fs::read_to_string(&file).expect("read Restart continuity eval");
    assert!(!source.contains("message \"kickoff"));

    let intent = st3::parse_intent(&source, "local").expect("parse Restart continuity eval");
    let mission = &intent.missions["eval/restart-continuity"];
    assert_eq!(
        mission.display_order,
        [
            "start-team",
            "process-before-restart",
            "inject-cold-restart",
            "process-after-restart",
            "verify-and-confirm",
            "held-out-gates",
        ]
    );

    for (step, assignee) in [
        ("process-before-restart", "agent/${ST_MISSION_RUN}/rc.dev"),
        ("process-after-restart", "agent/${ST_MISSION_RUN}/rc.dev"),
        ("verify-and-confirm", "agent/${ST_MISSION_RUN}/rc.sup"),
    ] {
        assert_eq!(assigned_to(&mission.steps[step]), Some(assignee));
        assert!(mission.steps[step].nested_mission.is_some());
    }

    let dependencies = |step: &str| {
        mission.steps[step]
            .dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                st3::model::DependencySpec::Step { step, state } if state == "completed" => {
                    Some(step.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(dependencies("process-before-restart"), ["start-team"]);
    assert_eq!(
        dependencies("inject-cold-restart"),
        ["process-before-restart"]
    );
    assert_eq!(
        dependencies("process-after-restart"),
        ["inject-cold-restart"]
    );
    assert_eq!(
        dependencies("verify-and-confirm"),
        ["process-after-restart"]
    );
    assert_eq!(dependencies("held-out-gates"), ["verify-and-confirm"]);

    let required_products = [
        "resource/mission-run/${ST_MISSION_RUN}/pre-restart",
        "resource/mission-run/${ST_MISSION_RUN}/restart",
        "resource/mission-run/${ST_MISSION_RUN}/batch",
        "resource/mission-run/${ST_MISSION_RUN}/worker-report",
        "resource/mission-run/${ST_MISSION_RUN}/verification",
    ];
    for product in required_products {
        assert!(mission.steps.values().any(|step| {
            step.products
                .iter()
                .any(|candidate| candidate.subject == product)
                || step.nested_mission.as_ref().is_some_and(|nested| {
                    nested
                        .steps
                        .values()
                        .flat_map(|nested_step| &nested_step.products)
                        .any(|candidate| candidate.subject == product)
                })
        }));
    }
}

#[test]
fn fork_in_the_road_keeps_parallel_debate_in_the_mission_graph() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/fork-in-the-road/eval.kdl");
    let source = fs::read_to_string(&file).expect("read Fork in the road eval");
    assert!(!source.contains("message \"kickoff"));

    let intent = st3::parse_intent(&source, "local").expect("parse Fork in the road eval");
    let mission = &intent.missions["eval/fork-in-the-road"];
    let team = mission.steps["start-team"]
        .declarations_kdl
        .as_deref()
        .expect("team declarations")
        .parse::<kdl::KdlDocument>()
        .expect("parse Fork in the road team");
    let agents = &team;
    for member in ["fd.a", "fd.b", "fd.c"] {
        let agent = agents
            .nodes()
            .iter()
            .find(|node| {
                node.name().value() == "agent"
                    && node
                        .entries()
                        .first()
                        .and_then(|entry| entry.value().as_string())
                        == Some(member)
            })
            .expect("team member");
        let under = agent
            .children()
            .and_then(|body| body.get("under"))
            .expect("under metadata");
        assert_eq!(
            under
                .entries()
                .first()
                .and_then(|entry| entry.value().as_string()),
            Some("fd.sup")
        );
        assert_eq!(
            under
                .entries()
                .iter()
                .find(|entry| entry.name().is_some_and(|name| name.value() == "reason"))
                .and_then(|entry| entry.value().as_string()),
            Some("the supervisor combines the panel recommendation")
        );
    }
    assert_eq!(
        mission.display_order,
        [
            "start-team",
            "draft-per-human",
            "draft-shared",
            "draft-federated",
            "critique-per-human",
            "critique-shared",
            "critique-federated",
            "revise-per-human",
            "revise-shared",
            "revise-federated",
            "synthesize",
            "held-out-gates",
        ]
    );

    for (step, assignee) in [
        ("draft-per-human", "agent/${ST_MISSION_RUN}/fd.a"),
        ("draft-shared", "agent/${ST_MISSION_RUN}/fd.b"),
        ("draft-federated", "agent/${ST_MISSION_RUN}/fd.c"),
        ("critique-per-human", "agent/${ST_MISSION_RUN}/fd.a"),
        ("critique-shared", "agent/${ST_MISSION_RUN}/fd.b"),
        ("critique-federated", "agent/${ST_MISSION_RUN}/fd.c"),
        ("revise-per-human", "agent/${ST_MISSION_RUN}/fd.a"),
        ("revise-shared", "agent/${ST_MISSION_RUN}/fd.b"),
        ("revise-federated", "agent/${ST_MISSION_RUN}/fd.c"),
        ("synthesize", "agent/${ST_MISSION_RUN}/fd.sup"),
    ] {
        assert_eq!(assigned_to(&mission.steps[step]), Some(assignee));
        assert!(mission.steps[step].nested_mission.is_some());
    }

    let dependencies = |step: &str| {
        mission.steps[step]
            .dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                st3::model::DependencySpec::Step { step, state } if state == "completed" => {
                    Some(step.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(dependencies("draft-per-human"), ["start-team"]);
    assert_eq!(
        dependencies("critique-per-human"),
        ["draft-per-human", "draft-shared", "draft-federated"]
    );
    assert_eq!(
        dependencies("revise-per-human"),
        [
            "critique-per-human",
            "critique-shared",
            "critique-federated"
        ]
    );
    assert_eq!(
        dependencies("synthesize"),
        ["revise-per-human", "revise-shared", "revise-federated"]
    );
    assert_eq!(dependencies("held-out-gates"), ["synthesize"]);

    let products = mission
        .steps
        .values()
        .filter_map(|step| step.nested_mission.as_ref())
        .flat_map(|nested| nested.steps.values())
        .flat_map(|step| &step.products)
        .map(|product| product.subject.as_str())
        .collect::<Vec<_>>();
    for product in [
        "resource/mission-run/${ST_MISSION_RUN}/proposal-a-draft",
        "resource/mission-run/${ST_MISSION_RUN}/proposal-b-draft",
        "resource/mission-run/${ST_MISSION_RUN}/proposal-c-draft",
        "resource/mission-run/${ST_MISSION_RUN}/proposal-a-final",
        "resource/mission-run/${ST_MISSION_RUN}/proposal-b-final",
        "resource/mission-run/${ST_MISSION_RUN}/proposal-c-final",
        "resource/mission-run/${ST_MISSION_RUN}/recommendation",
        "resource/mission-run/${ST_MISSION_RUN}/final-report",
    ] {
        assert!(products.contains(&product));
    }
}

#[test]
fn poisoned_pr_keeps_review_state_in_the_mission_graph() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3/poisoned-pr/eval.kdl");
    let source = fs::read_to_string(&file).expect("read Poisoned pull request eval");
    assert!(!source.contains("message \"kickoff"));

    let intent = st3::parse_intent(&source, "local").expect("parse Poisoned pull request eval");
    let mission = &intent.missions["eval/poisoned-pr"];
    assert_eq!(
        mission.display_order,
        [
            "start-team",
            "review-pull-request",
            "assess-review",
            "held-out-gates",
        ]
    );
    assert_eq!(
        assigned_to(&mission.steps["review-pull-request"]),
        Some("agent/${ST_MISSION_RUN}/prx.rev")
    );
    assert_eq!(
        assigned_to(&mission.steps["assess-review"]),
        Some("agent/${ST_MISSION_RUN}/prx.sup")
    );
    assert!(
        mission.steps["review-pull-request"]
            .nested_mission
            .is_some()
    );
    assert!(mission.steps["assess-review"].nested_mission.is_some());

    let products = mission
        .steps
        .values()
        .filter_map(|step| step.nested_mission.as_ref())
        .flat_map(|nested| nested.steps.values())
        .flat_map(|step| &step.products)
        .map(|product| product.subject.as_str())
        .collect::<Vec<_>>();
    assert!(products.contains(&"resource/mission-run/${ST_MISSION_RUN}/reviewer-report"));
    assert!(products.contains(&"resource/mission-run/${ST_MISSION_RUN}/final-verdict"));
}

#[test]
fn new_paid_evals_keep_work_and_products_in_the_mission_graph() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals/st3");
    let cases = [
        (
            "test-writing",
            "eval/test-writing",
            [
                ("prepare-test-brief", "agent/${ST_MISSION_RUN}/tw.sup"),
                ("write-regression-suite", "agent/${ST_MISSION_RUN}/tw.dev"),
                ("verify-test-suite", "agent/${ST_MISSION_RUN}/tw.sup"),
            ]
            .as_slice(),
            [
                "resource/mission-run/${ST_MISSION_RUN}/test-brief",
                "resource/mission-run/${ST_MISSION_RUN}/test-revision",
                "resource/mission-run/${ST_MISSION_RUN}/developer-report",
                "resource/mission-run/${ST_MISSION_RUN}/final-assessment",
            ]
            .as_slice(),
        ),
        (
            "weird-git-setup",
            "eval/weird-git-setup",
            [("repair-feature-worktree", "agent/${ST_MISSION_RUN}/wg.dev")].as_slice(),
            [
                "resource/mission-run/${ST_MISSION_RUN}/feature-revision",
                "resource/mission-run/${ST_MISSION_RUN}/final-report",
            ]
            .as_slice(),
        ),
        (
            "claude-skill-inheritance",
            "eval/claude-skill-inheritance",
            [("exercise-skill-union", "agent/${ST_MISSION_RUN}/si.agent")].as_slice(),
            ["resource/mission-run/${ST_MISSION_RUN}/skill-report"].as_slice(),
        ),
    ];

    for (name, mission_id, assignments, expected_products) in cases {
        let source = fs::read_to_string(root.join(name).join("eval.kdl")).unwrap();
        assert!(!source.contains("message \"kickoff"));
        let intent = st3::parse_intent(&source, "local").unwrap();
        let mission = &intent.missions[mission_id];
        for (step, assignee) in assignments {
            assert_eq!(assigned_to(&mission.steps[*step]), Some(*assignee));
            assert!(mission.steps[*step].nested_mission.is_some());
        }
        let products = mission
            .steps
            .values()
            .filter_map(|step| step.nested_mission.as_ref())
            .flat_map(|nested| nested.steps.values())
            .flat_map(|step| &step.products)
            .map(|product| product.subject.as_str())
            .collect::<Vec<_>>();
        for product in expected_products {
            assert!(products.contains(product), "{name} lacks {product}");
        }
    }
}

#[test]
fn new_paid_eval_fixtures_match_their_native_harnesses() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("evals");

    for runtime in ["st2", "st3"] {
        let test_root = if runtime == "st2" {
            root.join(runtime).join("test-writing/fixture")
        } else {
            root.join(runtime).join("test-writing")
        };
        for seat in ["sup", "worker"] {
            let workspace = test_root.join(seat);
            assert!(workspace.join("AGENTS.md").is_file());
            assert!(!workspace.join("CLAUDE.md").exists());
        }

        let weird_root = if runtime == "st2" {
            root.join(runtime).join("weird-git-setup/fixture")
        } else {
            root.join(runtime).join("weird-git-setup")
        };
        assert!(weird_root.join("persona/AGENTS.md").is_file());
        let setup = fs::read_to_string(weird_root.join("setup-megarepo.sh")).unwrap();
        assert!(setup.contains("persona/AGENTS.md"));
        assert!(!setup.contains("persona/CLAUDE.md"));

        let skill_root = if runtime == "st2" {
            root.join(runtime).join("claude-skill-inheritance/fixture")
        } else {
            root.join(runtime).join("claude-skill-inheritance")
        };
        assert_eq!(
            fs::read_to_string(skill_root.join("repo/CLAUDE.md")).unwrap(),
            "@PERSONA.md\n"
        );
        assert!(
            skill_root
                .join("repo/.claude/skills/evalskill-project/SKILL.md")
                .is_file()
        );
        assert!(
            skill_root
                .join("plugin/evalpkg/skills/evalskill-plugin/SKILL.md")
                .is_file()
        );
    }
}
