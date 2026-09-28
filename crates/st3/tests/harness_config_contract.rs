//! What a seat launch does to the harness's own files, and what the lifecycle hooks it registers
//! need in order to run.
//!
//! A typed Claude seat admits its workspace in the operator's `.claude.json` before Claude starts,
//! and registers `$ST_HOOKS/claude-observe.sh` for every turn edge. The config belongs to the
//! operator, so the admission must leave it exactly as the operator keeps it apart from the two
//! trust flags. The hook is the only source of Claude's turn state and delivery receipts, so it
//! must reach an implementation using only what the st3 runtime gives every member.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn canonical_key(dir: &Path) -> String {
    fs::canonicalize(dir)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn trusted(config: &Path, workspace: &Path) -> bool {
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(config).unwrap()).unwrap();
    value["projects"][canonical_key(workspace)]["hasTrustDialogAccepted"] == true
}

/// The operator's config in the order and shape Claude writes it: top-level keys that are not
/// sorted, and a project entry with fields of its own.
fn operator_config(other_project: &str) -> String {
    format!(
        "{{\n  \"numStartups\": 12,\n  \"theme\": \"dark\",\n  \"autoUpdates\": false,\n  \"projects\": {{\n    \"{other_project}\": {{\n      \"lastCost\": 1.5,\n      \"allowedTools\": [],\n      \"hasTrustDialogAccepted\": true\n    }}\n  }},\n  \"customApiKeyResponses\": {{\n    \"approved\": [],\n    \"rejected\": []\n  }},\n  \"firstStartTime\": \"2026-01-01T00:00:00.000Z\"\n}}"
    )
}

fn positions(text: &str, keys: &[&str]) -> Vec<usize> {
    keys.iter()
        .map(|key| {
            text.find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("`{key}` is missing from the rewritten config:\n{text}"))
        })
        .collect()
}

#[test]
fn workspace_admission_keeps_the_operators_keys_in_order_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let config = temp.path().join(".claude.json");
    fs::write(&config, operator_config("/elsewhere")).unwrap();

    st2::pretrust::pretrust_at(&config, std::slice::from_ref(&workspace)).unwrap();

    let first = fs::read_to_string(&config).unwrap();
    assert!(trusted(&config, &workspace));
    let keys = [
        "numStartups",
        "theme",
        "autoUpdates",
        "projects",
        "customApiKeyResponses",
        "firstStartTime",
    ];
    let order = positions(&first, &keys);
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "the operator's top-level key order changed:\n{first}"
    );
    let project = positions(
        &first,
        &["lastCost", "allowedTools", "hasTrustDialogAccepted"],
    );
    assert!(
        project.windows(2).all(|pair| pair[0] < pair[1]),
        "a project entry's field order changed:\n{first}"
    );
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["projects"]["/elsewhere"]["lastCost"], 1.5);
    assert_eq!(value["theme"], "dark");

    // A seat relaunch admits the same workspace again; the file must not change.
    st2::pretrust::pretrust_at(&config, std::slice::from_ref(&workspace)).unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), first);
    let leftovers = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".claude.json.") && !name.ends_with(".lock"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "the admission left staging files: {leftovers:?}"
    );
}

#[test]
#[ignore = "fails on main: workspace admission replaces a symlinked .claude.json with a regular file"]
fn workspace_admission_writes_through_a_symlinked_config() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let dotfiles = temp.path().join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let managed = dotfiles.join("claude.json");
    fs::write(&managed, operator_config("/elsewhere")).unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let config = home.join(".claude.json");
    std::os::unix::fs::symlink(&managed, &config).unwrap();

    st2::pretrust::pretrust_at(&config, std::slice::from_ref(&workspace)).unwrap();

    let link = fs::symlink_metadata(&config).unwrap();
    assert!(
        link.file_type().is_symlink(),
        "the operator's symlinked config was replaced by a regular file; its target is now \
         detached from what Claude reads"
    );
    assert!(
        trusted(&managed, &workspace),
        "the admission did not reach the file the symlink names"
    );
}

#[test]
#[ignore = "fails on main: workspace admission rewrites .claude.json with the umask mode instead of its own"]
fn workspace_admission_keeps_the_config_file_mode() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let config = temp.path().join(".claude.json");
    fs::write(&config, operator_config("/elsewhere")).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    st2::pretrust::pretrust_at(&config, std::slice::from_ref(&workspace)).unwrap();

    assert!(trusted(&config, &workspace));
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "an owner-only config became {mode:o} after the admission"
    );
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn bash() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("bash"))
        .find(|candidate| candidate.is_file())
        .expect("bash must be installed to run the Claude lifecycle hooks")
}

/// A stand-in program that records that it ran and drains its stdin like the real one.
fn recorder(path: &Path, record: &Path) {
    executable(
        path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> '{}'\ncat >/dev/null\nexit 0\n",
            record.display()
        ),
    );
}

struct HookRun {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
}

/// Run one registered hook command the way Claude does: payload on stdin, then EOF, with only
/// the environment the st3 runtime and the Claude driver give a typed seat.
fn run_hook(
    set: &Path,
    hook: &str,
    event: &str,
    path: &str,
    st3_bin: &Path,
    root: &Path,
) -> HookRun {
    let started = Instant::now();
    let mut child = Command::new(bash())
        .arg(set.join(hook))
        .arg(event)
        .env_clear()
        .env("PATH", path)
        .env("HOME", root.join("home"))
        .env("ST_HOOKS", set)
        .env("ST3_BIN", st3_bin)
        .env("ST3_ENDPOINT", root.join("st3.sock"))
        .env("ST3_DRIVER_STATE_DIR", root.join("drivers"))
        .env("ST3_SUBJECT", "agent/example/worker")
        .env("ST_AGENT", "agent/example/worker")
        .env("ST2_CLAUDE_IDENTITY", "example/worker")
        .env("ST2_CLAUDE_RUNTIME_ID", "example/worker")
        .env("ST2_CLAUDE_SESSION", "session-token")
        .env("ST2_CLAUDE_SESSION_SEQ", "1")
        .env("CATALOG", root.join("drivers/catalog"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"session_id":"native-1","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    HookRun {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        elapsed: started.elapsed(),
    }
}

#[test]
#[ignore = "fails on main: the Claude observe hook needs an st2 binary on PATH that the st3 runtime does not provide"]
fn the_claude_observe_hook_reaches_an_implementation_through_the_st3_runtime_alone() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("home")).unwrap();
    let set = st2::hooks::install_at(&root.join("hooks"), false).unwrap();

    // The st3 executable directory is first on every member's PATH and ST3_BIN names it.
    let runtime_bin = root.join("st3-bin");
    fs::create_dir_all(&runtime_bin).unwrap();
    let runtime_record = root.join("st3-invocations");
    let st3 = runtime_bin.join("st3");
    recorder(&st3, &runtime_record);

    // Control: with the previous generation's binary also installed, the hook does run.
    let legacy_bin = root.join("legacy-bin");
    fs::create_dir_all(&legacy_bin).unwrap();
    let legacy_record = root.join("st2-invocations");
    recorder(&legacy_bin.join("st2"), &legacy_record);
    let with_legacy = format!("{}:{}", runtime_bin.display(), legacy_bin.display());
    let control = run_hook(
        &set,
        "claude-observe.sh",
        "UserPromptSubmit",
        &with_legacy,
        &st3,
        root,
    );
    assert!(control.status.success());
    assert!(control.stdout.is_empty(), "the hook printed into Claude");
    assert!(
        fs::read_to_string(&legacy_record)
            .unwrap_or_default()
            .contains("driver claude-observe"),
        "control: the hook did not forward the turn edge to the installed binary"
    );

    // An st3-only installation: only the runtime's own binary is available.
    let only_runtime = runtime_bin.display().to_string();
    let run = run_hook(
        &set,
        "claude-observe.sh",
        "UserPromptSubmit",
        &only_runtime,
        &st3,
        root,
    );
    assert!(run.status.success(), "the hook failed: {:?}", run.status);
    assert!(run.stdout.is_empty() && run.stderr.is_empty());
    assert!(run.elapsed < Duration::from_secs(5));
    assert!(
        runtime_record.exists(),
        "the hook exited 0 without recording the turn edge: nothing it can reach from the st3 \
         runtime implements `claude-observe`, so the seat's turn state and delivery receipts are \
         silently never written"
    );
}

#[test]
fn the_claude_hooks_a_seat_registers_are_quiet_fast_and_fail_open() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("home")).unwrap();
    let set = st2::hooks::install_at(&root.join("hooks"), false).unwrap();
    let registration = st2::hooks::claude_st3_settings_registration();
    let registered = registration.to_string();
    let runtime_bin = root.join("st3-bin");
    fs::create_dir_all(&runtime_bin).unwrap();
    let st3 = runtime_bin.join("st3");
    recorder(&st3, &root.join("st3-invocations"));

    // Every command the registration names is a file of the verified set, and uses no helper
    // other than bash and the st binary (no jq or python on the per-turn path).
    for hook in ["claude-observe.sh", "claude-statusline.sh"] {
        assert!(registered.contains(&format!("$ST_HOOKS/{hook}")), "{hook}");
        let body = fs::read_to_string(set.join(hook)).unwrap();
        for helper in ["jq", "python"] {
            assert!(
                !body
                    .lines()
                    .filter(|line| !line.trim_start().starts_with('#'))
                    .any(|line| line.contains(helper)),
                "{hook} depends on {helper}"
            );
        }
    }

    // A failing implementation neither blocks the turn nor prints into the agent.
    let failing_bin = root.join("failing-bin");
    fs::create_dir_all(&failing_bin).unwrap();
    executable(
        &failing_bin.join("st2"),
        "#!/bin/sh\necho 'implementation failed' >&2\necho 'noise'\nexit 3\n",
    );
    let path = format!("{}:{}", runtime_bin.display(), failing_bin.display());
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
    ] {
        let run = run_hook(&set, "claude-observe.sh", event, &path, &st3, root);
        assert!(run.status.success(), "{event}: {:?}", run.status);
        assert!(run.stdout.is_empty(), "{event} printed into Claude");
        assert!(
            run.stderr.is_empty(),
            "{event} printed an error into Claude"
        );
        assert!(
            run.elapsed < Duration::from_secs(5),
            "{event} took {:?}",
            run.elapsed
        );
    }
}

#[test]
fn the_user_scope_channel_server_does_nothing_in_a_claude_session_that_is_not_a_seat() {
    // The channel plugin is installed at user scope, so Claude starts its server in every
    // session, including the operator's own. Outside a seat it must refuse before touching state.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_st3"))
        .args(["driver", "claude-mcp"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n");
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "the server answered for a non-seat session"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        fs::read_dir(&home).unwrap().count(),
        0,
        "the server wrote state for a session that is not a seat"
    );
}
