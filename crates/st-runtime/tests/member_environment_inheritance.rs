#![cfg(unix)]
//! A member's environment comes from the launch path the daemon uses: the account login-shell
//! probe overlaid with declared and runtime-owned values, then handed to a detached `pty`
//! session. These tests drive that exact path with a temporary PTY root and read the environment
//! of the running member and of a subprocess it starts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use st_runtime::{Launch, PtyRuntime, materialize_environment, resolve_executable};

/// Session markers that interactive coding harnesses export to the processes they start. A
/// process that carries one believes it runs inside that harness session.
const HARNESS_NESTING_MARKERS: &[(&str, &str)] = &[
    ("CLAUDECODE", "1"),
    ("CLAUDE_CODE_CHILD_SESSION", "1"),
    ("CLAUDE_CODE_ENTRYPOINT", "cli"),
    ("CODEX_SANDBOX_NETWORK_DISABLED", "1"),
    ("PI_CODING_AGENT", "true"),
    ("AI_AGENT", "outer-harness-agent"),
];

fn keep_launches_out_of_the_user_manager() {
    // Without a runtime directory the shared isolation mode is a detached session, so these
    // launches never create transient units in the account's service manager.
    unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
}

struct Member {
    runtime: PtyRuntime,
    id: String,
}

impl Drop for Member {
    fn drop(&mut self) {
        let _ = self.runtime.stop(&self.id);
        for _ in 0..40 {
            if self.runtime.remove(&self.id).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Launch one terminal member the way the reconciler does and return the environment names and
/// values observed by the member itself and by a subprocess the member starts.
fn launch_and_observe(
    root: &Path,
    declared: &BTreeMap<String, String>,
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    Member,
) {
    let executable = std::env::current_exe().unwrap();
    let environment = materialize_environment(declared, &executable).unwrap();
    let pty = resolve_executable("pty", &environment)
        .expect("the member launch path needs `pty` on the login-shell PATH");
    let member_out = root.join("member.env");
    let tool_out = root.join("tool.env");
    let script = format!(
        "env -0 > {member}.tmp && mv {member}.tmp {member}; sh -c 'env -0' > {tool}.tmp && mv {tool}.tmp {tool}; exec sleep 60",
        member = shell_quote(&member_out),
        tool = shell_quote(&tool_out),
    );
    let runtime = PtyRuntime::new(root.join("pty")).with_binary(pty.to_string_lossy());
    let id = "environment-probe".to_string();
    runtime
        .spawn(
            &id,
            &Launch::Shell(script),
            root,
            &environment,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
    let member = Member { runtime, id };
    let member_environment = read_environment(&member_out);
    let tool_environment = read_environment(&tool_out);
    (environment, member_environment, tool_environment, member)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn read_environment(path: &PathBuf) -> BTreeMap<String, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "the member never wrote {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let bytes = std::fs::read(path).unwrap();
    let environment = bytes
        .split(|byte| *byte == 0)
        .filter_map(|record| std::str::from_utf8(record).ok()?.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect::<BTreeMap<_, _>>();
    assert!(
        environment.len() > 3,
        "an empty environment observation proves nothing: {} names",
        environment.len()
    );
    environment
}

fn runtime_identity() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("ST_AGENT".into(), "agent/node.environment-probe".into()),
        ("ST3_SUBJECT".into(), "agent/node.environment-probe".into()),
        ("ST3_ENDPOINT".into(), "/nonexistent/st3-test.sock".into()),
        (
            "ST3_DRIVER_STATE_DIR".into(),
            "/nonexistent/st3-test-drivers".into(),
        ),
    ])
}

#[test]
fn the_runtime_identity_reaches_a_member_and_the_subprocesses_it_starts() {
    keep_launches_out_of_the_user_manager();
    let root = tempfile::tempdir().unwrap();
    let declared = runtime_identity();
    let (_, member, tool, _guard) = launch_and_observe(root.path(), &declared);
    for (name, value) in &declared {
        assert_eq!(
            member.get(name),
            Some(value),
            "the member lacks runtime identity {name} ({} names observed)",
            member.len()
        );
        assert_eq!(
            tool.get(name),
            Some(value),
            "a subprocess of the member lacks runtime identity {name} ({} names observed)",
            tool.len()
        );
    }
}

#[test]
#[ignore = "fails on main: the launching process's harness session markers reach every member"]
fn a_launching_processs_harness_session_markers_never_reach_a_member() {
    keep_launches_out_of_the_user_manager();
    for (name, value) in HARNESS_NESTING_MARKERS {
        // The daemon inherits these when it is started from inside another harness session.
        unsafe { std::env::set_var(name, value) };
    }
    let root = tempfile::tempdir().unwrap();
    let (materialized, member, tool, _guard) = launch_and_observe(root.path(), &runtime_identity());
    let markers = HARNESS_NESTING_MARKERS
        .iter()
        .map(|(name, _)| *name)
        .collect::<BTreeSet<_>>();
    let leaked = |environment: &BTreeMap<String, String>| {
        environment
            .keys()
            .filter(|name| markers.contains(name.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };
    let in_materialized = leaked(&materialized);
    let in_member = leaked(&member);
    let in_tool = leaked(&tool);
    assert!(
        in_materialized.is_empty() && in_member.is_empty() && in_tool.is_empty(),
        "harness session markers from the launching process reached the member: \
         materialized environment {in_materialized:?} ({} names), running member {in_member:?} \
         ({} names), member subprocess {in_tool:?} ({} names)",
        materialized.len(),
        member.len(),
        tool.len()
    );
}
