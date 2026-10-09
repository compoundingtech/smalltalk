//! Keys reach the text box as a terminal sends them. A terminal that reports every key as an
//! escape code sends Shift+i as `i` with Shift; the actual stui binary, under a PTY that plays
//! such a terminal, must still type `I`, `:` and `J`. (Decoded key events in unit tests cannot
//! show this: the bug lived between the terminal's bytes and the key event.)

#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

#[test]
fn shifted_keys_are_typed_as_a_keyboard_protocol_terminal_sends_them() {
    let output = std::process::Command::new("python3")
        .arg(test_env!("CARGO_MANIFEST_DIR", "/../stui/tests/pty_smoke.py"))
        .arg(test_env!("CARGO_BIN_EXE_st3"))
        .arg("--only-shifted-keys")
        .arg("--ui")
        .env("ST3_PERSON", "person/alex")
        .env_remove("ST_AGENT")
        .env_remove("ST3_ENDPOINT")
        .output()
        .expect("python3 is required by the PTY smoke (provided by CI)");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bare_st_seats_and_redirected_streams_keep_help() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../stui/tests/pty_smoke.py"
        ))
        .arg(env!("CARGO_BIN_EXE_st3"))
        .arg("--only-entrypoint")
        .output()
        .expect("python3 is required by the PTY smoke");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
