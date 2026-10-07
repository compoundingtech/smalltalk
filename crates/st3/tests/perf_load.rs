#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

// Compile the existing workload and its comparison tests without the other integration fixtures.
mod daemon_bench;
mod daemon_load;
