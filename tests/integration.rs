//! Every integration test file in this directory is a module of this one test binary, so a
//! test build links one binary instead of one per file: those links were most of what a
//! build wrote. Cargo builds only the test targets the manifest names (`autotests = false`),
//! so add a `mod NAME;` line here for a new file; `every_test_file_is_built` fails until then.
//! The files that are test targets of their own set process-wide environment variables:
//! `cargo test` runs a binary's tests as threads of one process, where they would leak.

#[macro_use]
#[path = "../scripts/ci-test-paths.rs"]
mod ci_test_paths;

mod support;

mod agent_address;
mod agent_desired_state;
mod agent_presentation;
mod agent_publish;
mod atomic_pty_snapshot;
mod catalog_apply;
mod catalog_archive;
mod catalog_config;
mod catalog_diff;
mod catalog_graph;
mod catalog_selection;
mod claude_channel_install;
mod claude_hooks;
mod claude_statusline;
mod codex_hooks;
mod doctor;
mod eval_run_e2e;
mod eval_up;
mod harness_state_teardown;
mod hooks;
mod invariants;
mod materialize;
mod message_cli;
mod native_only;
mod nomad_survival;
mod otel_export;
mod parked_recovery;
mod predecessor_ding_migration;
mod public_repo;
mod pty;
mod request_cli;
mod resource_provider_e2e;
mod resync;
mod resync_notify_chain;
mod service;
mod status_agents;
mod stream_authoring_cli;
mod supervisor_auto_archive;
mod targeted_reconcile;
mod task_inventory_cli;
mod transport_isolation;
mod transport_isolation_macos;
mod up_once_exit;
mod validate;
mod vrs_ledger;

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
