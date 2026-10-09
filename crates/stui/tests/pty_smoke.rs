//! The release PTY smoke is also part of every PR's Cargo/nextest suite.
//! Each case owns its terminal and disposable configuration; no daemon is needed.
//! Shifted keys remain covered by `typed_keys.rs`, using the same Python harness.

fn smoke(case: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pty_smoke.py"))
        .arg(env!("CARGO_BIN_EXE_stui"))
        .args(["--case", case])
        .env_remove("ST_AGENT")
        .env_remove("ST3_ENDPOINT")
        .output()
        .expect("python3 is required by the PTY smoke (provided by CI)");
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn first_frame_keys_and_quit_restore_the_terminal() {
    smoke("normal");
}

#[test]
fn signal_restores_the_terminal() {
    smoke("signal");
}

// The production binary deliberately has no panic injection hook.
#[cfg(debug_assertions)]
#[test]
fn panic_restores_the_terminal() {
    smoke("panic");
}

#[test]
fn outstanding_getter_does_not_block_keys_or_quit() {
    smoke("delayed-getter");
}

#[test]
fn terminal_close_exits_after_navigation() {
    smoke("hangup");
}

#[test]
fn tmux_session_close_exits_after_first_frame() {
    smoke("tmux-hangup");
}
