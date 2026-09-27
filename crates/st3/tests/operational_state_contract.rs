use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use st3::model::{ClaimInput, MissionRunRequest};
use st3::store::Store;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/operational-state")
}

fn fixture(name: &str) -> Value {
    let path = fixture_root().join(name);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
        panic!("read operational-state fixture {}: {error}", path.display())
    }))
    .unwrap_or_else(|error| {
        panic!(
            "parse operational-state fixture {}: {error}",
            path.display()
        )
    })
}

fn append_fixture_claims(store: &Store, fixture: &Value) {
    for claim in fixture["claims"].as_array().expect("fixture claims") {
        let input = ClaimInput {
            subject: claim["subject"].as_str().unwrap().into(),
            kind: claim["kind"].as_str().unwrap().into(),
            actor: claim
                .get("actor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            fields: claim["fields"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        store.append_claim(&input).unwrap();
    }
}

#[test]
fn regression_fixture_set_is_small_complete_and_deterministic() {
    let expected = BTreeSet::from([
        "cli-regressions.json",
        "eval-person-message.json",
        "expired-lease.json",
        "ready-idle-wake.json",
        "ready-step-timeout.json",
        "stopped-agents.json",
        "superseded-attention.json",
        "terminal-nested-orphan.json",
        "terminal-sig-sup.json",
        "versioned-reminders.json",
    ])
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    let actual = std::fs::read_dir(fixture_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    for name in actual {
        let value = fixture(&name);
        assert!(value["id"].is_string(), "fixture {name} has no stable ID");
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(
            encoded.len() < 16 * 1024,
            "fixture {name} is not unit-sized"
        );
    }
}

#[test]
fn every_screen_model_has_one_cli_renderer_and_required_rendering_state() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/st3/operational-state/screens.json");
    let contract: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let required = BTreeSet::from([
        "now",
        "missions.list",
        "missions.detail",
        "attention",
        "launch",
        "machines",
        "agents",
        "conversations",
        "activity",
        "devices",
        "import",
        "subject",
        "terminals",
        "work",
    ]);
    let screens = contract["screens"].as_array().unwrap();
    let actual = screens
        .iter()
        .map(|screen| {
            for field in ["id", "task", "model", "cli", "sections", "actions"] {
                assert!(
                    !screen[field].is_null(),
                    "screen is missing {field}: {screen}"
                );
            }
            screen["id"].as_str().unwrap()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, required);
    assert_eq!(contract["parity"]["human_and_json_same_membership"], true);
    assert_eq!(contract["parity"]["client_side_graph_joins"], false);
    assert_eq!(contract["parity"]["historical_default"], false);
}

#[test]
fn command_inventory_resolves_every_baseline_command_to_a_complete_purpose() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/st3/operational-state/cli-commands.json");
    let contract: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let purposes = contract["purposes"].as_object().unwrap();
    let purpose_fields = contract["purpose_fields"].as_array().unwrap();
    for (id, purpose) in purposes {
        for field in purpose_fields {
            let field = field.as_str().unwrap();
            assert!(
                purpose[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "purpose {id} has no {field}"
            );
        }
    }
    let installed = contract["installed_baseline"].as_array().unwrap();
    let mut paths = BTreeSet::new();
    for command in installed {
        let path = command["path"].as_str().unwrap();
        assert!(paths.insert(path), "duplicate command inventory row {path}");
        assert!(
            purposes.contains_key(command["purpose_id"].as_str().unwrap()),
            "unknown purpose for {path}"
        );
        assert!(
            command["disposition"].is_string(),
            "missing disposition for {path}"
        );
    }
    for root in [
        "up",
        "claude",
        "codex",
        "preview",
        "planning",
        "mission",
        "publish",
        "import",
        "exec",
        "logs",
        "pty",
        "inspect",
        "trace",
        "wait",
        "doctor",
        "repair",
        "replication",
        "service",
        "claude-channel",
        "doc",
        "eval",
        "graph",
        "status",
        "agents",
        "runtime",
        "context",
        "resource",
        "claim",
        "diagnostic",
        "schema",
        "review",
        "attention",
        "work",
        "message",
        "gate-result",
        "completions",
    ] {
        assert!(
            installed
                .iter()
                .any(|row| row["path"] == format!("st3 {root}")),
            "installed root st3 {root} is absent from the transition matrix"
        );
    }
    assert!(contract["conformance"].as_array().unwrap().len() >= 14);
}

#[test]
fn eval_message_keeps_its_explicit_person_identity() {
    let store = Store::open_memory("operational-fixture").unwrap();
    let value = fixture("eval-person-message.json");
    append_fixture_claims(&store, &value);
    let items = store
        .attention_items(Some("person/eval-requester"))
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].person, "person/eval-requester");
    assert_eq!(items[0].subject, "message/eval/report");
}

#[test]
fn terminal_signal_rename_sig_sup_is_history_not_default_state() {
    let store = Store::open_memory("operational-fixture").unwrap();
    append_fixture_claims(&store, &fixture("terminal-sig-sup.json"));
    let subjects = store
        .status(None)
        .unwrap()
        .subjects
        .into_iter()
        .map(|subject| subject.subject)
        .collect::<Vec<_>>();
    assert!(!subjects.contains(&"agent/signal-rename-codex/sig.sup".into()));
}

#[test]
fn stopped_agents_require_explicit_all_or_history() {
    let store = Store::open_memory("operational-fixture").unwrap();
    append_fixture_claims(&store, &fixture("stopped-agents.json"));
    assert!(store.status(None).unwrap().subjects.is_empty());
}

#[test]
fn expired_lease_is_actionably_ready_at_snapshot_time() {
    let value = fixture("expired-lease.json");
    let store = Store::open_memory("operational-fixture").unwrap();
    let source = value["mission"].as_str().unwrap();
    let intent = st3::parse_intent(source, "operational-fixture").unwrap();
    store
        .apply_internal(&intent, "expired-lease-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "lease-fixture".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "expired-lease-run".into(),
        })
        .unwrap();
    let step = &run.steps[0];
    store.set_step_state(&step.subject, "ready", None).unwrap();
    let claimant = step.assigned_to.clone().unwrap();
    let mut fields = value["expired_claim"]["fields"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    fields.insert("claimant".into(), Value::String(claimant.clone()));
    store
        .append_claim(&ClaimInput {
            subject: step.subject.clone(),
            kind: "work.claimed".into(),
            actor: Some(claimant.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let work = store.work(Some(&claimant), false).unwrap();
    assert_eq!(work[0].status, "ready");
    assert_eq!(work[0].claimant, None);
}

#[test]
fn superseded_revision_and_readiness_attention_are_history_only() {
    let store = Store::open_memory("operational-fixture").unwrap();
    append_fixture_claims(&store, &fixture("superseded-attention.json"));
    assert!(
        store
            .attention_items(Some("person/operator"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_the_selected_reminder_version_is_actionable() {
    let store = Store::open_memory("operational-fixture").unwrap();
    append_fixture_claims(&store, &fixture("versioned-reminders.json"));
    let subjects = store
        .attention_items(Some("person/operator"))
        .unwrap()
        .into_iter()
        .map(|item| item.subject)
        .collect::<Vec<_>>();
    assert_eq!(subjects, ["message/reminder/v2"]);
}

#[test]
fn cli_release_blocker_observations_match_the_contract() {
    let value = fixture("cli-regressions.json");
    let enrich = &value["cases"][0];
    assert_eq!(enrich["observed_human_ids"], enrich["expected_ids"]);
    assert_eq!(enrich["observed_json_ids"], enrich["expected_ids"]);
    let reply = &value["cases"][1];
    assert_eq!(reply["printed_id"], reply["expected_printed_id"]);
    assert_eq!(reply["accepted_input"], reply["expected_accepted_input"]);
}

#[test]
fn recovery_failure_observations_are_exact_and_self_consistent() {
    let timeout = fixture("ready-step-timeout.json");
    assert_eq!(timeout["claim_events"], 0);
    assert!(timeout["execution_started_at_unix_ms"].is_null());
    assert_eq!(
        timeout["timeline"][1]["accepted_at_unix_ms"]
            .as_u64()
            .unwrap()
            - timeout["timeline"][0]["accepted_at_unix_ms"]
                .as_u64()
                .unwrap(),
        timeout["elapsed_ms"].as_u64().unwrap()
    );
    assert!(timeout["elapsed_ms"].as_u64().unwrap() >= timeout["timeout_ms"].as_u64().unwrap());

    let orphan = fixture("terminal-nested-orphan.json");
    assert_eq!(orphan["observed"]["root_run_status"], "failed");
    assert_eq!(orphan["observed"]["root_run_phase"], "terminal");
    assert_eq!(orphan["observed"]["child_run_status"], "running");
    assert_eq!(orphan["observed"]["step_status_after_cancel"], "ready");

    let wake = fixture("ready-idle-wake.json");
    let attempts = wake["observed"]["attempts"].as_array().unwrap();
    assert!(
        attempts[..4]
            .iter()
            .all(|attempt| { attempt["accepted"] == true && attempt["turn_started"] == false })
    );
    assert_eq!(attempts[4]["method"], "full-boot-prompt");
    assert_eq!(attempts[4]["turn_started"], true);
}

#[test]
fn recovery_release_contract_keeps_the_resolved_required_outcomes() {
    let timeout = fixture("ready-step-timeout.json");
    assert_eq!(timeout["expected"]["status"], "ready");
    assert_eq!(
        timeout["expected"]["reason"],
        "the execution timeout has not started"
    );

    let orphan = fixture("terminal-nested-orphan.json");
    assert_eq!(orphan["expected"]["child_run_status"], "cancelled");
    assert_eq!(orphan["expected"]["step_status"], "cancelled");
    assert_eq!(orphan["expected"]["work_default_visible"], false);

    let wake = fixture("ready-idle-wake.json");
    for field in [
        "automatic_driver_wake",
        "bounded_retry",
        "diagnostic_after_exhaustion",
        "manual_wake_command",
    ] {
        assert_eq!(wake["expected"][field], true);
    }
    assert_eq!(wake["expected"]["pty_input_required"], false);
}
