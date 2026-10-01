//! Most integration test files share one binary so they link once rather than per file.
//! Tests that mutate process-wide limits have separate test targets in Cargo.toml.
//! Cargo builds only the targets the manifest names (`autotests = false`), so add a
//! `mod NAME;` line here or an explicit test target for every new file.

mod client_glasses;
mod client_v0_cli;
mod client_v0_contract;
mod command_recorder;
mod convergence;
mod daemon_bench;
mod daemon_environment;
mod daemon_restart;
mod delivery_probe;
mod examples;
mod fault_isolation;
mod first_sync;
mod fleet;
mod log_diet;
mod messaging_faults;
mod mission_cancellation;
mod no_st2_seat;
mod operational_state_contract;
mod recorder_report;
mod seat_queue_perf;
mod terminal_attach;

#[test]
fn every_test_file_is_built() {
    let modules = include_str!("integration.rs");
    let manifest = include_str!("../Cargo.toml");
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
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
