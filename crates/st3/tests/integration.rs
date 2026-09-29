//! Every integration test file in this directory is a module of this one test binary, so a
//! test build links one binary instead of one per file: those links were most of what a
//! build wrote. Cargo builds only the test targets the manifest names (`autotests = false`),
//! so add a `mod NAME;` line here for a new file; `every_test_file_is_built` fails until then.

mod api_accept;
mod client_v0_cli;
mod client_v0_contract;
mod command_recorder;
mod daemon_environment;
mod daemon_restart;
mod examples;
mod fault_isolation;
mod first_sync;
mod fleet;
mod log_diet;
mod operational_state_contract;
mod recorder_report;
mod seat_queue_perf;

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
