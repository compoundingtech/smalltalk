//! Every integration test file in this directory is a module of this one test binary, so a
//! test build links one binary instead of one per file: those links were most of what a
//! build wrote. Cargo builds only the test targets the manifest names (`autotests = false`),
//! so add a `mod NAME;` line here for a new file; `every_test_file_is_built` fails until then.

#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

mod github_issue_component;
mod github_pr_component;

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
