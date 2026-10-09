//! The onboarding-play eval's judge accepts a good play and rejects each bad one. The judge is pure,
//! so this needs no harness; it needs only bash and jq.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn available(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

#[test]
fn the_judge_accepts_a_good_play_and_rejects_each_bad_one() {
    if !available("jq") || !available("bash") {
        eprintln!("skipped: needs bash and jq");
        return;
    }
    let eval = PathBuf::from(test_env!("CARGO_MANIFEST_DIR")).join("../../evals/st3/onboarding-play");
    let output = Command::new("bash")
        .arg(eval.join("self-test.sh"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
