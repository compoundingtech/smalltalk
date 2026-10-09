//! Most integration test files share one binary so they link once rather than per file.
//! Tests that mutate process-wide limits have separate test targets in Cargo.toml.
//! Cargo builds only the targets the manifest names (`autotests = false`), so add a
//! `mod NAME;` line here or an explicit test target for every new file.

#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

mod action_coverage;
mod adhoc_work;
mod agents_restart;
mod authored_resume;
mod backup;
mod binary_payloads;
mod boot_canaries;
mod broken_gates;
mod claude_idle;
mod client_creation;
mod client_glasses;
mod client_presence;
mod client_only;
mod client_v0_cli;
mod client_v0_contract;
mod codex_bootstrap;
mod command_recorder;
mod completion_shells;
mod convergence;
mod daemon_bench;
mod daemon_cost;
mod daemon_load;
mod daemon_environment;
mod daemon_restart;
mod driver_incarnation;
mod delivery_probe;
mod examples;
mod fault_isolation;
mod first_sync;
mod follow_retry;
mod fleet;
mod gate_kinds;
mod getting_started;
mod hermetic;
mod hook_telemetry;
mod human_gates;
mod idle_budget;
mod log_diet;
mod message_send_once;
mod messaging_faults;
mod mission_cancellation;
mod no_st2_seat;
mod node_identity_cli;
mod operational_state_contract;
mod otel_export;
mod owned_sets;
mod placement_handoff;
mod reconcile_pass_perf;
mod recorder_report;
mod revision_seats;
mod seat_queue_perf;
mod setup;
mod startup_readiness;
mod subagent_publisher;
mod subagents_seat;
mod terminal_attach;
mod terminal_binding;
mod version;
mod typed_keys;

#[test]
fn every_test_file_is_built() {
    let modules = include_str!("integration.rs");
    let manifest = include_str!("../Cargo.toml");
    let directory = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut unbuilt = Vec::new();
    for entry in std::fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        if name != "integration"
            && !modules.contains(&format!("\nmod {name};"))
            && !manifest.contains(&format!("path = \"tests/{name}.rs\""))
        {
            unbuilt.push(name);
        }
    }
    assert!(
        unbuilt.is_empty(),
        "add `mod NAME;` to tests/integration.rs for each of {unbuilt:?}"
    );
}
