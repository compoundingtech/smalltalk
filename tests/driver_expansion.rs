use std::fs;
use std::path::Path;
use std::process::Command;

use st2::driver::{
    CHANNEL_DEV_CONSENT_REQUIRED, CHANNEL_NO_INBOX_TRANSPORT, CHANNEL_NOT_REGISTERED,
    CHANNEL_ROUTE_UNKNOWN, claude_channel_advisories,
};
use st2::materialize::materialize_agent;
use st2::reconcile::{TaskCompileContext, compile_generated_tasks};
use st2::{discover, driver::expand_driver};

fn assert_snapshot(input: &str, expected: &str) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agents/host/worker/agent.kdl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, input).unwrap();

    let found = discover(temp.path());
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert_eq!(found.specs.len(), 1);
    let actual = expand_driver(&found.specs[0], "unused")
        .unwrap()
        .to_string();
    assert_eq!(actual, expected);
    expected.parse::<kdl::KdlDocument>().unwrap();
}

#[test]
fn claude_kdl_expansion_matches_snapshot() {
    assert_snapshot(
        include_str!("fixtures/driver/claude.in.kdl"),
        include_str!("fixtures/driver/claude.out.kdl"),
    );
}

#[test]
fn codex_kdl_expansion_matches_snapshot() {
    assert_snapshot(
        include_str!("fixtures/driver/codex.in.kdl"),
        include_str!("fixtures/driver/codex.out.kdl"),
    );
}

#[test]
fn pi_kdl_expansion_matches_snapshot() {
    assert_snapshot(
        include_str!("fixtures/driver/pi.in.kdl"),
        include_str!("fixtures/driver/pi.out.kdl"),
    );
}

/// The declaration must not name the channel extension. `st2 driver pi-session` splices it in from
/// this binary's verified hook set at launch, so a catalog never carries a machine-local path and
/// an operator reading the expansion sees only the harness policy they authored.
#[test]
fn pi_expansion_carries_no_machine_local_extension_path() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agents/host/worker/agent.kdl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, include_str!("fixtures/driver/pi.in.kdl")).unwrap();

    let found = discover(temp.path());
    let expanded = expand_driver(&found.specs[0], "unused")
        .unwrap()
        .to_string();

    assert!(!expanded.contains("pi-channel.ts"), "{expanded}");
    assert!(!expanded.contains("ST_HOOKS"), "{expanded}");
    assert!(!expanded.contains(" -e "), "{expanded}");
    // The one thing expansion does owe the launch is accepting the workspace without mutating the
    // operator's ambient pi config.
    assert!(expanded.contains("-- pi -a "), "{expanded}");
}

/// A hand-authored pi seat reaches the same wrapper as the typed driver. Unlike `deliver "mcp"`,
/// this transport renders nothing into the workspace: pi's channel is an extension st2 injects at
/// launch, so the authored argv is wrapped and otherwise left alone.
#[test]
fn pi_deliver_wraps_the_authored_launch_without_rendering_anything() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&catalog).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = catalog.join("legacy.kdl");
    fs::write(
        &path,
        format!(
            r#"agent "worker" {{
  host "h"
  workspace "{}"
  deliver "pi-channel"
  argv "pi" "-a" "--model" "anthropic/claude-opus-5" "boot"
}}
"#,
            workspace.display()
        ),
    )
    .unwrap();
    let (specs, _) = st2::discover_file(&catalog, &path).unwrap();
    let mut spec = specs.into_iter().next().unwrap();
    let executable = catalog.join("bin/st2");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "test binary").unwrap();
    let context = TaskCompileContext::new(catalog.clone(), executable.clone()).unwrap();

    compile_generated_tasks(std::slice::from_mut(&mut spec), "h", &context).unwrap();

    let task = spec.tasks.iter().find(|task| task.name == "agent").unwrap();
    assert_eq!(
        task.argv.as_ref().unwrap(),
        &[
            executable.to_string_lossy().into_owned(),
            "--catalog".into(),
            catalog.to_string_lossy().into_owned(),
            "driver".into(),
            "pi-session".into(),
            "--identity".into(),
            "h.worker".into(),
            "--runtime-id".into(),
            "h.worker".into(),
            "--".into(),
            "pi".into(),
            "-a".into(),
            "--model".into(),
            "anthropic/claude-opus-5".into(),
            "boot".into(),
        ]
    );
    // No derived DING companion, and no rendered channel configuration of any kind.
    assert!(spec.tasks.iter().all(|task| !task.derived));
    let rendered = materialize_agent(&catalog, &spec, "h").unwrap();
    assert!(
        !format!("{rendered:?}").contains("pi-channel"),
        "{rendered:?}"
    );
}
#[test]
fn opaque_session_driver_materializes_without_rewriting_or_adding_launch_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let path = catalog.join("agent.kdl");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        &path,
        r#"agent "worker" {
  host "h"
  argv "axe" "agent" "launch" "--harness" "claude-code"
  session-driver "claude"
}"#,
    )
    .unwrap();
    let (specs, errors) = st2::discover_file(&catalog, &path).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let mut spec = specs.into_iter().next().unwrap();
    let original_task = spec.tasks[0].clone();
    let executable = catalog.join("bin/st2");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "test binary").unwrap();
    let context = TaskCompileContext::new(catalog.clone(), executable).unwrap();

    compile_generated_tasks(std::slice::from_mut(&mut spec), "h", &context).unwrap();

    assert_eq!(spec.tasks, [original_task]);
    assert!(materialize_agent(&catalog, &spec, "h").unwrap().is_empty());
}

#[test]
fn cli_prints_each_snapshot_without_changing_its_input() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/driver");
    for provider in ["claude", "codex", "pi", "omp"] {
        let input = fixtures.join(format!("{provider}.in.kdl"));
        let before = fs::read(&input).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["--catalog"])
            .arg(&fixtures)
            .args(["driver", "expand"])
            .arg(&input)
            .args(
                (provider == "claude")
                    .then_some(["--agent", "ExampleMac.fabric"])
                    .into_iter()
                    .flatten(),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            fs::read(fixtures.join(format!("{provider}.out.kdl"))).unwrap()
        );
        // Expansion is a read of the declaration, so it stays silent; the channel advisories
        // belong to the command that actually creates the seat.
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("channel"), "{provider}: {stderr}");
        assert_eq!(fs::read(&input).unwrap(), before);
    }
}

#[test]
fn claude_driver_uses_the_packaged_channel_without_project_mcp_state() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let driver_workspace = temp.path().join("driver-workspace");
    fs::create_dir_all(&driver_workspace).unwrap();
    let driver_path = catalog.join("driver.kdl");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        &driver_path,
        format!(
            r#"agent "worker" {{
  host "h"
  workspace "{}"
  claude {{
    model "opus"
    effort "xhigh"
    dev-channels #true
    prompt "boot"
    args "--permission-mode" "bypassPermissions"
  }}
}}
"#,
            driver_workspace.display()
        ),
    )
    .unwrap();
    let (driver, _) = st2::discover_file(&catalog, &driver_path).unwrap();
    let mut driver = driver.into_iter().next().unwrap();
    assert!(!st2::hooks::required_by_codex_agent(&driver, "h", &catalog));
    let executable = catalog.join("bin/st2");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "test binary").unwrap();
    let context = TaskCompileContext::new(catalog.clone(), executable.clone()).unwrap();

    compile_generated_tasks(std::slice::from_mut(&mut driver), "h", &context).unwrap();
    let driver_task = driver
        .tasks
        .iter()
        .find(|task| task.name == "agent")
        .unwrap();
    let executable_string = executable.to_string_lossy().into_owned();
    let catalog_string = catalog.to_string_lossy().into_owned();
    assert_eq!(
        &driver_task.argv.as_ref().unwrap()[..10],
        [
            executable_string.as_str(),
            "--catalog",
            catalog_string.as_str(),
            "driver",
            "claude-session",
            "--identity",
            "h.worker",
            "--runtime-id",
            "h.worker",
            "--",
        ]
    );
    let argv = driver_task.argv.as_ref().unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--channels", "plugin:st2-channel@st2"])
    );
    assert!(!argv.iter().any(|arg| arg == "--mcp-config"));

    // The driver still registers lifecycle hooks. The channel plugin is machine state, not a file
    // in the product workspace.
    let hooks = tempfile::tempdir().unwrap().keep();
    st2::hooks::install_at(&hooks, false).unwrap();
    unsafe { std::env::set_var("ST_HOOKS", &hooks) };
    materialize_agent(&catalog, &driver, "h").unwrap();
    assert!(!driver_workspace.join(".mcp.json").exists());
    assert!(
        driver_workspace
            .join(".claude/settings.local.json")
            .exists()
    );
}

#[test]
fn claude_mcp_is_canonical_and_claude_is_a_hidden_alias() {
    let help = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["driver", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(
        help.lines()
            .any(|line| line.trim_start().starts_with("claude-mcp "))
    );
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("claude "))
    );

    for command in ["claude-mcp", "claude"] {
        let output = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["driver", command, "--help"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let old = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(temp.path())
        .args(["driver", "claude", "--identity", "missing"])
        .output()
        .unwrap();
    let old_error = String::from_utf8(old.stderr).unwrap();
    assert!(old_error.contains("`st2 driver claude` is deprecated"));

    let current = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(temp.path())
        .args(["driver", "claude-mcp", "--identity", "missing"])
        .output()
        .unwrap();
    let current_error = String::from_utf8(current.stderr).unwrap();
    assert!(!current_error.contains("deprecated"));

    let implicit = Command::new(env!("CARGO_BIN_EXE_st2"))
        .env("ST_AGENT", "missing")
        .arg("--catalog")
        .arg(temp.path())
        .args(["driver", "claude-mcp"])
        .output()
        .unwrap();
    let implicit_error = String::from_utf8(implicit.stderr).unwrap();
    assert!(!implicit_error.contains("--identity is required"));
}

#[test]
fn legacy_claude_shell_launch_keeps_its_source_under_the_session_wrapper() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let path = catalog.join("agent.kdl");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        &path,
        r#"agent "worker" {
  host "h"
  deliver "mcp"
  command "exec claude boot"
}
"#,
    )
    .unwrap();
    let (mut specs, _) = st2::discover_file(&catalog, &path).unwrap();
    let executable = catalog.join("bin/st2");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "test binary").unwrap();
    let context = TaskCompileContext::new(catalog, executable).unwrap();

    compile_generated_tasks(&mut specs, "h", &context).unwrap();

    let argv = specs[0].tasks[0].argv.as_ref().unwrap();
    assert_eq!(
        &argv[3..10],
        [
            "driver",
            "claude-session",
            "--identity",
            "h.worker",
            "--runtime-id",
            "h.worker",
            "--",
        ]
    );
    assert_eq!(&argv[10..], ["sh", "-c", "exec claude boot"]);
}

#[test]
fn ambiguous_driver_source_neither_compiles_nor_materializes() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let path = catalog.join("agent.kdl");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        &path,
        format!(
            r#"agent "worker" {{
  host "h"
  workspace "{}"
  deliver "mcp"
  claude {{ prompt "boot" }}
}}
"#,
            workspace.display()
        ),
    )
    .unwrap();
    let (specs, _) = st2::discover_file(&catalog, &path).unwrap();
    let mut spec = specs.into_iter().next().unwrap();
    let before = spec.clone();
    let executable = catalog.join("bin/st2");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "test binary").unwrap();
    let context = TaskCompileContext::new(catalog.clone(), executable).unwrap();

    let compile_error =
        compile_generated_tasks(std::slice::from_mut(&mut spec), "h", &context).unwrap_err();
    assert!(
        compile_error
            .to_string()
            .contains("choose one launch source")
    );
    assert_eq!(spec, before);
    let materialize_error = materialize_agent(&catalog, &spec, "h").unwrap_err();
    assert!(
        materialize_error
            .to_string()
            .contains("choose one launch source")
    );
    assert!(!workspace.join(".mcp.json").exists());
}

fn spec_from(catalog: &Path, body: &str) -> st2::spec::AgentSpec {
    let path = catalog.join("agent.kdl");
    fs::create_dir_all(catalog).unwrap();
    fs::write(&path, body).unwrap();
    let (specs, errors) = st2::discover_file(catalog, &path).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    specs.into_iter().next().unwrap()
}

/// A Claude seat's channel state comes from the launch that will actually run.
#[test]
fn claude_driver_names_the_channel_state_the_seat_is_in() {
    let temp = tempfile::tempdir().unwrap();
    let unrouted = spec_from(
        &temp.path().join("unrouted"),
        r#"agent "worker" {
  host "h"
  workspace "/work"
  claude { prompt "boot" }
}
"#,
    );
    // No channel and no `ding`: this seat has no inbox transport whatsoever, which is a stronger
    // and more urgent fact than the skipped channel on its own.
    assert_eq!(
        claude_channel_advisories(&unrouted),
        vec![CHANNEL_NOT_REGISTERED, CHANNEL_NO_INBOX_TRANSPORT]
    );

    let dev = spec_from(
        &temp.path().join("dev"),
        r#"agent "worker" {
  host "h"
  workspace "/work"
  claude { dev-channels #true; prompt "boot" }
}
"#,
    );
    assert!(claude_channel_advisories(&dev).is_empty());

    // An explicit development flag still reaches the provider launch verbatim.
    let dev_via_args = spec_from(
        &temp.path().join("dev-via-args"),
        r#"agent "worker" {
  host "h"
  workspace "/work"
  claude { prompt "boot"; args "--dangerously-load-development-channels=server:st2" }
}
"#,
    );
    assert_eq!(
        claude_channel_advisories(&dev_via_args),
        vec![CHANNEL_DEV_CONSENT_REQUIRED]
    );

    // A harness that renders no st2 channel server has nothing to say.
    let codex = spec_from(
        &temp.path().join("codex"),
        r#"agent "worker" {
  host "h"
  workspace "/work"
  codex { prompt "boot" }
}
"#,
    );
    assert!(claude_channel_advisories(&codex).is_empty());
}

/// The legacy `deliver "mcp"` transport renders the same `.mcp.json`, so it is in the same states
/// — read back from structured argv when the operator provides it. A shell program can reach the
/// flag through a wrapper or variable, while a source-text occurrence may only be quoted or
/// commented, so shell source never proves the route and must remain opaque.
#[test]
fn legacy_mcp_delivery_reads_the_channel_state_off_the_authored_launch() {
    let temp = tempfile::tempdir().unwrap();
    for (launch, expected) in [
        (
            r#"argv "claude" "boot""#,
            vec![CHANNEL_NOT_REGISTERED, CHANNEL_NO_INBOX_TRANSPORT],
        ),
        (
            r#"argv "claude" "--dangerously-load-development-channels" "boot""#,
            vec![CHANNEL_DEV_CONSENT_REQUIRED],
        ),
        (
            r#"argv "claude" "--dangerously-load-development-channels=server:st2" "boot""#,
            vec![CHANNEL_DEV_CONSENT_REQUIRED],
        ),
        (r#"command "exec claude boot""#, vec![CHANNEL_ROUTE_UNKNOWN]),
        (
            r#"command "exec claude --dangerously-load-development-channels=server:st2 boot""#,
            vec![CHANNEL_ROUTE_UNKNOWN],
        ),
    ] {
        let spec = spec_from(
            &temp.path().join(format!("legacy{}", launch.len())),
            &format!(
                r#"agent "worker" {{
  host "h"
  workspace "/work"
  deliver "mcp"
  {launch}
}}
"#
            ),
        );
        assert_eq!(claude_channel_advisories(&spec), expected, "{launch}");
    }
}
