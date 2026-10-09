//! The canonical example missions that onboarding offers: they parse, the weekly schedule names the
//! exact weekly review, and the review's credential guard rejects a report that quotes a secret.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn canonical(name: &str) -> PathBuf {
    PathBuf::from(test_env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/st3/canonical")
        .join(name)
}

fn source(name: &str) -> String {
    fs::read_to_string(canonical(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

#[test]
fn the_weekly_schedule_names_the_weekly_review_and_leaves_its_revision_to_fill_in() {
    // A mission revision depends on the machine that publishes it, so the schedule cannot ship a
    // real one: it carries a zero placeholder that whoever publishes the review replaces.
    let schedule = source("weekly-schedule.kdl");
    assert!(
        schedule.contains(&format!(
            "mission \"example/canonical/weekly-session-review@{}\"",
            "0".repeat(64)
        )),
        "weekly-schedule.kdl must name the weekly review with a 64-digit placeholder revision"
    );
    st3::parse_intent(&schedule, "local").unwrap();
    st3::parse_intent(&source("weekly-session-review.kdl"), "local").unwrap();
    st3::parse_intent(&source("review-pull-request.kdl"), "local").unwrap();
}

#[test]
fn only_the_pull_request_review_may_involve_github() {
    for name in ["weekly-session-review.kdl", "weekly-schedule.kdl"] {
        // Comments may say that GitHub is not needed; the mission itself must not touch it.
        let text = source(name)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        assert!(!text.contains("github"), "{name} must not need GitHub");
        assert!(!text.contains(" gh "), "{name} must not need the GitHub CLI");
    }
}

/// The exec command of the weekly review's report gate.
fn report_gate() -> String {
    let document: kdl::KdlDocument = source("weekly-session-review.kdl").parse().unwrap();
    fn find(document: &kdl::KdlDocument) -> Option<String> {
        for node in document.nodes() {
            if node.name().value() == "gate" {
                if let Some(exec) = node.children().and_then(|body| body.get("exec")) {
                    return exec.get(0)?.as_string().map(str::to_owned);
                }
            }
            if let Some(found) = node.children().and_then(find) {
                return Some(found);
            }
        }
        None
    }
    find(&document).expect("the report step carries an exec gate")
}

fn gate_answer(command: &str, report: Option<&str>) -> Option<i32> {
    let workspace = tempfile::tempdir().unwrap();
    if let Some(report) = report {
        fs::write(workspace.path().join("report.md"), report).unwrap();
    }
    Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace.path())
        .output()
        .unwrap()
        .status
        .code()
}

#[test]
fn the_report_gate_rejects_a_quoted_credential_and_accepts_a_described_one() {
    let gate = report_gate();
    // The fixture session holds invented secrets. A report that copies them is refused, and so is
    // one that copies the whole session; one that describes the pattern passes.
    let session = fs::read_to_string(canonical("fixtures/session-with-invented-secret.txt")).unwrap();
    assert!(session.contains("sk-invented0123456789abcdef"));
    assert_eq!(gate_answer(&gate, Some(&session)), Some(1));
    for (fixture, expected) in [
        ("fixtures/report-quotes-secret.md", Some(1)),
        ("fixtures/report-describes-pattern.md", Some(0)),
    ] {
        let report = fs::read_to_string(canonical(fixture)).unwrap();
        assert_eq!(gate_answer(&gate, Some(&report)), expected, "{fixture}");
    }
    // Each kind of secret the guard knows, and the not-yet answers.
    for secret in [
        "AKIAINVENTED01234567",
        "ghp_inventedinventedinvented0123",
        "-----BEGIN RSA PRIVATE KEY-----",
        "password=hunter2-invented-value",
        "\"api_key\": \"invented-value\"",
    ] {
        assert_eq!(
            gate_answer(&gate, Some(&format!("# Review\n\nSeen: {secret}\n"))),
            Some(1),
            "{secret}"
        );
    }
    assert_eq!(gate_answer(&gate, None), Some(1), "a missing report is not yet");
    assert_eq!(gate_answer(&gate, Some("")), Some(1), "an empty report is not yet");
}
