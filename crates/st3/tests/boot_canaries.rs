//! Boot canaries: a seat that cannot start never ships.
//!
//! Each case runs a real st3 daemon with real `pty` seats of one harness (Claude, Codex, pi, omp or
//! OpenCode) whose provider is a token-free stand-in speaking that harness's wire contract
//! (`scripts/st3-boot-canaries`, no login, no model). Every seat must reach a claimed step, with its
//! current incarnation, and read an st message within the time bound, and none may park in the
//! crash-loop guard. The scenarios are the launches that broke in production:
//!
//! - `fresh`: a new seat boots.
//! - `restart`: the seat is restarted and the new incarnation boots.
//! - `daemon-restart`: the daemon dies with the seat's provider, and the new daemon, whose latest
//!   observation of the seat is still its dead predecessor, boots a replacement.
//! - `reexec`: the st binary is replaced and the seat's driver and channels re-exec into it.
//! - `concurrent`: five seats launch at once.
//! - `suspend`: a quiet seat suspends and resumes its own native session, and a resume whose
//!   transcript is gone fails with a typed reason while the seat stays suspended.
//!
//! The stand-ins answer instantly where the real providers take seconds. That is the point: a race
//! between a provider and the daemon that a slow provider hides, a fast one hits every time.
//! These cases never retry (see `.config/nextest.toml`): a race that sometimes loses is the bug.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

/// Run only by the cleanup regression's disposable Rust test process.
#[test]
#[ignore = "subprocess fixture for the cleanup regression"]
fn cleanup_fixture() {
    if st3::test_support::supervise_test() {
        return;
    }
    let evidence = std::env::var_os("ST3_CLEANUP_FIXTURE").expect("regression fixture directory");
    let mode = std::env::var("ST3_CLEANUP_FIXTURE_MODE").unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut fixture = Command::new("python3")
        .arg(repo.join("scripts/st3-test-process-test"))
        .arg("--boot-fixture")
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .arg(&mode)
        .spawn()
        .unwrap();
    if mode == "panic" {
        let ready = PathBuf::from(evidence).join("ready.json");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "cleanup fixture did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("injected Rust panic before fixture cleanup");
    }
    let status = fixture.wait().unwrap();
    assert!(status.success(), "injected cleanup fixture: {status}");
}

#[test]
fn every_exit_reaps_the_daemon_even_when_the_rust_test_is_killed() {
    if st3::test_support::supervise_test() {
        return;
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repo.join("scripts/st3-test-process-test"))
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn canary(harness: &str, scenario: &str) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let in_ci = std::env::var_os("CI_RUN_ID").is_some();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path =
        |program: &str| std::env::split_paths(&path).any(|dir| dir.join(program).is_file());
    let tools: &[&str] = if matches!(harness, "pi" | "omp") {
        &["pty", "git", "python3", "curl", "node"]
    } else {
        &["pty", "git", "python3", "curl"]
    };
    let missing: Vec<_> = tools.iter().filter(|tool| !on_path(tool)).collect();
    if !missing.is_empty() {
        assert!(!in_ci, "the boot canaries need {missing:?} on PATH in CI");
        eprintln!("skipped: the {harness} boot canary needs {missing:?} on PATH");
        return;
    }
    let name = format!("{harness}-{scenario}");
    let (evidence_root, keep) = if in_ci {
        // The Linux gate uploads target/boot-canaries/ with its logs.
        let artifacts = repo.join("target/boot-canaries");
        std::fs::create_dir_all(&artifacts).unwrap();
        (
            tempfile::Builder::new()
                .prefix(&format!("{name}-"))
                .tempdir_in(artifacts)
                .unwrap(),
            true,
        )
    } else {
        (tempfile::tempdir().unwrap(), false)
    };
    let evidence = evidence_root.path().join("evidence");
    // The replacement binary for `reexec` is a copy of st3, hundreds of megabytes in a debug build:
    // it goes on the target directory's disk, not the small tmpfs the daemon's sockets live on.
    let scratch = tempfile::Builder::new()
        .prefix("boot-canary-scratch-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    // Double-fork out of the calling seat's ancestry: the isolated daemon binds a caller to the
    // nearest ST_AGENT above it, and the canary acts as person/eval.
    let output = Command::new("setsid")
        .args([
            "-f",
            "env",
            "-u",
            "ST_AGENT",
            "-u",
            "ST3_SUBJECT",
            "PYTHONDONTWRITEBYTECODE=1",
            "python3",
        ])
        .arg(repo.join(if scenario == "when-idle" {
            "scripts/st3-rollout-binding-canary/run"
        } else {
            "scripts/st3-boot-canaries/run"
        }))
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .args([harness, scenario, "--bound", "180", "--scratch"])
        .arg(scratch.path())
        .output()
        .expect("run the boot canary");
    let result = std::fs::read_to_string(evidence.join("result.json"));
    let passed = result
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|result| result["verdict"] == "pass");
    if scenario == "when-idle" {
        eprintln!(
            "{}",
            result.as_ref().map(String::as_str).unwrap_or_default()
        );
    }
    if !passed {
        eprintln!("boot canary evidence: {}", evidence.display());
        let _ = evidence_root.keep();
    } else if keep {
        // Passing runs are not worth uploading.
        let _ = std::fs::remove_dir_all(&evidence);
    }
    assert!(
        passed,
        "{}\n{}\n{}",
        result.unwrap_or_default(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The canary's private root is redrawn until its name avoids "st2": the fresh-seat boundary check
/// rejects any launch path that mentions it, and a random name holds those letters by chance.
#[test]
fn the_canary_never_names_its_root_after_st2() {
    if st3::test_support::supervise_test() {
        return;
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repo.join("scripts/st3-boot-canaries/run"))
        .arg("--self-test")
        .output();
    let Ok(output) = output else {
        assert!(
            std::env::var_os("CI_RUN_ID").is_none(),
            "CI must provide python3"
        );
        return;
    };
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

macro_rules! harness {
    ($module:ident, $harness:literal) => {
        mod $module {
            #[test]
            fn a_fresh_seat_boots() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "fresh");
            }
            #[test]
            fn a_restarted_seat_boots() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "restart");
            }
            #[test]
            fn a_seat_boots_after_the_daemon_restarts_with_its_predecessor_still_observed() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "daemon-restart");
            }
            #[test]
            fn a_driver_re_execs_into_a_new_binary_and_still_takes_a_message() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "reexec");
            }
            #[test]
            fn five_seats_launching_at_once_all_boot() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "concurrent");
            }
            #[test]
            fn a_suspended_seat_resumes_its_own_native_session() {
                if st3::test_support::supervise_test() {
                return;
            }
                super::canary($harness, "suspend");
            }
        }
    };
}

harness!(claude, "claude");
harness!(codex, "codex");
harness!(pi, "pi");
harness!(omp, "omp");
harness!(opencode, "opencode");

#[test]
fn claude_missing_channel_blocks_and_recovers_early_mail_without_replay() {
    canary("claude", "channel-missing");
}

#[test]
fn claude_uninitialized_channel_blocks_until_native_attachment() {
    canary("claude", "channel-uninitialized");
}

#[test]
fn claude_permanent_channel_failure_parks_after_three_attempts_without_a_restart_loop() {
    canary("claude", "channel-parked");
}

#[test]
fn omp_when_idle_replacement_binds_its_original_native_session() {
    if st3::test_support::supervise_test() {
        return;
    }
    canary("omp", "when-idle");
}

#[test]
fn codex_transcript_utf8_and_discovery_failures_preserve_delivery_and_private_warnings() {
    if st3::test_support::supervise_test() {
        return;
    }
    canary("codex", "utf8");
}
