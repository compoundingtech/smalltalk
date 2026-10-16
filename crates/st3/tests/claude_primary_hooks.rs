#![cfg(unix)]
//! REAL driver_hook executable beneath real test-executable provider/nested chains.
//! No Claude, provider service, daemon or live seat participates. Witness setup is a fixture.
use serde_json::{Value, json};
use std::os::unix::net::UnixListener;
use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
const ROLE: &str = "ST_TEST_PRIMARY_HOOK_ROLE";
const PRIMARY: &str = "01961b54-0000-7000-8000-000000000001";
const NESTED: &str = "01961b54-0000-7000-8000-000000000002";
const TEST: &str = "claude_primary_hooks::inherited_hook_processes_cannot_bind_record_context_or_request_permission";
fn helper(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture"])
        .env(ROLE, role);
    command
}
fn records(dir: &Path) -> Value {
    json!({
        "binding":fs::read(dir.join("claude-native-session")).ok(),
        "session_binding":fs::read(PathBuf::from(std::env::var_os(st_drivers::driver_paths::SESSION_DIR_ENV).unwrap()).join("binding.json")).ok(),
        "context":st_drivers::harness_events::read_snapshot(dir,"harness-context").unwrap(),
        "state":st_drivers::harness_events::read_snapshot(dir,"harness-state").unwrap(),
        "events":serde_json::to_value(st_drivers::harness_events::pending(dir,128).unwrap()).unwrap(),
    })
}
fn hook(event: &str, native: &str, raw: Option<&[u8]>) -> std::process::Output {
    let mut command = Command::new(
        std::env::var_os("ST_TEST_PRIMARY_HOOK_BINARY").expect("root-resolved fixture binary"),
    );
    if event == "statusline" {
        command.args(["driver-hook", "claude-statusline"]);
    } else {
        command.args(["driver-hook", "claude-observe", event]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = json!({"session_id":native,"transcript_path":PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude/projects/fixture").join(format!("{native}.jsonl")),
        "tool_name":"Bash","tool_input":{"command":"fixture-only"}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.unwrap_or(payload.to_string().as_bytes()))
        .unwrap();
    let start = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "owned hook exceeded its fixture bound: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}
#[test]
fn inherited_hook_processes_cannot_bind_record_context_or_request_permission() {
    if let Ok(role) = std::env::var(ROLE) {
        let dir = PathBuf::from(std::env::var_os("ST_TEST_PRIMARY_HOOK_DIR").unwrap());
        if role == "provider" {
            let endpoint = std::env::var_os("ST3_ENDPOINT").unwrap();
            let listener = UnixListener::bind(endpoint).unwrap();
            listener.set_nonblocking(true).unwrap();
            std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
            assert!(
                helper("nested").status().unwrap().success(),
                "nested before primary SessionStart"
            );
            assert!(matches!(listener.accept(), Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock),
                "secondary before primary must not contact a daemon");
            let out = hook("SessionStart", PRIMARY, None);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &fs::read(dir.join("claude-native-session")).unwrap()
                )
                .unwrap()["native_session_id"],
                PRIMARY
            );
            let session_dir =
                PathBuf::from(std::env::var_os(st_drivers::driver_paths::SESSION_DIR_ENV).unwrap());
            let binding: Value =
                serde_json::from_slice(&fs::read(session_dir.join("binding.json")).unwrap())
                    .unwrap();
            assert_eq!(
                binding["nativeSessionId"], PRIMARY,
                "positive real SessionStart must bind fully"
            );
            let raw = include_bytes!(
                "../../st-drivers/tests/fixtures/harness-context/claude-statusline-mid-session.json"
            );
            assert!(hook("statusline", PRIMARY, Some(raw)).status.success());
            assert!(
                st_drivers::harness_events::read_snapshot(&dir, "harness-context")
                    .unwrap()
                    .is_some()
            );
            // The admitted primary SessionStart legitimately submits hook telemetry.
            // Inspect that request, then establish a clean cut before the next secondary.
            let (telemetry, _) = listener.accept().expect("primary hook telemetry");
            telemetry.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
            let mut request = Vec::new();
            telemetry.take(16_385).read_to_end(&mut request).unwrap();
            assert!(request.len() <= 16_384, "bounded fixture telemetry request");
            assert!(request.starts_with(b"POST /v1/claims "));
            let body = request.windows(4).position(|part| part == b"\r\n\r\n")
                .expect("telemetry HTTP headers") + 4;
            let claim: Value = serde_json::from_slice(&request[body..]).unwrap();
            assert_eq!(claim["kind"], "harness.telemetry");
            let invocations = claim["fields"]["signals"]["hook_invocations"].as_array().unwrap();
            assert_eq!(invocations.len(), 1);
            assert_eq!(invocations[0]["event"], "SessionStart");
            assert!(matches!(listener.accept(), Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock),
                "primary emitted only its admitted telemetry request");
            assert!(
                helper("nested").status().unwrap().success(),
                "nested after primary SessionStart"
            );
            assert!(matches!(listener.accept(), Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock),
                "secondary after primary must not contact a daemon");
        } else {
            assert_eq!(role, "nested");
            let before = records(&dir);
            for event in ["SessionStart", "PreToolUse", "PermissionRequest"] {
                let output = hook(event, NESTED, None);
                assert!(output.status.success());
                assert!(
                    output.stdout.is_empty(),
                    "secondary must never emit a permission decision"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("Claude ownership refused")
                );
                assert!(output.stderr.len() < 1024);
                assert_eq!(
                    records(&dir),
                    before,
                    "secondary {event} mutated native/context/prompt observations"
                );
            }
            let raw = include_bytes!(
                "../../st-drivers/tests/fixtures/harness-context/claude-statusline-mid-session.json"
            );
            let output = hook("statusline", NESTED, Some(raw));
            assert!(output.status.success());
            assert_eq!(output.stdout, b"private-renderer");
            assert_eq!(records(&dir), before);
        }
        return;
    }
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let paths = st_drivers::driver_paths::Paths {
        root: root.path().join("root"),
        agent_dir: root.path().join("agent"),
        session_dir: root.path().join("sessions"),
    };
    for dir in [&paths.root, &paths.agent_dir, &paths.session_dir] {
        fs::create_dir_all(dir).unwrap();
    }
    st_drivers::harness_events::enable(&paths.agent_dir, "fixture-runtime").unwrap();
    let seq = st_drivers::harness_state::claim(
        &paths.agent_dir,
        "fixture/primary-hooks",
        "claude",
        "wrapper-session",
    )
    .unwrap();
    let transcripts = root.path().join(".claude/projects/fixture");
    fs::create_dir_all(&transcripts).unwrap();
    let workspace = fs::canonicalize(root.path()).unwrap();
    for native in [PRIMARY, NESTED] {
        fs::write(
            transcripts.join(format!("{native}.jsonl")),
            format!("{}\n", json!({"sessionId":native,"cwd":workspace})),
        )
        .unwrap();
    }
    let endpoint = root.path().join("must-not-connect.sock");
    let wrapper = st_drivers::claude_process::process(std::process::id()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(paths.agent_dir.join("claude-primary-process.lock"))
        .unwrap();
    let owner = |provider| {
        json!({"version":1,"session":"wrapper-session","sequence":seq,
        "runtime":"fixture-runtime","wrapper":wrapper,"provider":provider,"closed":false})
    };
    fs::write(
        paths.agent_dir.join("claude-primary-process.json"),
        serde_json::to_vec(&owner(Value::Null)).unwrap(),
    )
    .unwrap();
    let mut child = helper("provider")
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.path())
        .env(ROLE, "provider")
        .envs(paths.environment("fixture/primary-hooks"))
        .env("ST_TEST_PRIMARY_HOOK_DIR", &paths.agent_dir)
        .env(
            "ST_TEST_PRIMARY_HOOK_BINARY",
            test_env!("CARGO_BIN_EXE_st3-fixture"),
        )
        .env("ST_AGENT", "agent/fixture/primary-hooks")
        .env("ST3_SUBJECT", "agent/fixture/primary-hooks")
        .env("ST_CLAUDE_SESSION", "wrapper-session")
        .env("ST_CLAUDE_SESSION_SEQ", seq.to_string())
        .env("ST_CLAUDE_RUNTIME_ID", "fixture-runtime")
        .env("ST3_ENDPOINT", &endpoint)
        .env(
            "ST_CLAUDE_STATUSLINE_RENDERER",
            "cat >/dev/null; printf private-renderer",
        )
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let provider = st_drivers::claude_process::process(child.id()).unwrap();
    fs::write(
        paths.agent_dir.join("claude-primary-process.json"),
        serde_json::to_vec(&owner(json!(provider))).unwrap(),
    )
    .unwrap();
    child.stdin.take().unwrap().write_all(b"g").unwrap();
    assert!(child.wait().unwrap().success());

}
