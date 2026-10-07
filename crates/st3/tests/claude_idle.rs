#![cfg(unix)]
use serde_json::{Value, json};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;

#[test]
fn unscoped_claude_channel_serves_mcp_until_eof_without_a_daemon_or_state() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = st3::test_support::command(env!("CARGO_BIN_EXE_st3"))
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env_remove("ST3_SUBJECT")
        .arg("--endpoint")
        .arg(root.path().join("absent.sock"))
        .args(["driver", "claude-mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let calls = [
        "initialize",
        "tools/list",
        "resources/list",
        "resources/templates/list",
        "prompts/list",
        "ping",
        "tools/call",
    ];
    for (index, method) in calls.iter().enumerate() {
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":index,"method":method,"params":{"protocolVersion":"2025-03-26"}})).unwrap();
        if index == 0 {
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), calls.len());
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(response["id"], index);
        assert_eq!(response["jsonrpc"], "2.0");
    }
    assert_eq!(responses[0]["result"]["capabilities"], json!({}));
    assert_eq!(responses[1]["result"], json!({"tools":[]}));
    assert_eq!(responses[2]["result"], json!({"resources":[]}));
    assert_eq!(responses[3]["result"], json!({"resourceTemplates":[]}));
    assert_eq!(responses[4]["result"], json!({"prompts":[]}));
    assert_eq!(responses[5]["result"], json!({}));
    assert_eq!(responses[6]["error"]["code"], -32601);
    assert!(
        !root.path().join("state").exists(),
        "unscoped MCP must not create state or a mailbox"
    );
}

#[test]
fn channel_installer_leaves_user_plugin_disabled_and_local_settings_intact() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CLAUDE_FIXTURE_LOG\"\ncase \"$*\" in\n*--json*) printf '[]\\n';;\nesac\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let local = root.path().join("seat/.claude/settings.local.json");
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    let settings = b"{\"enabledPlugins\":{\"st-channel@st\":true}}\n";
    std::fs::write(&local, settings).unwrap();
    let log = root.path().join("commands");
    let output = st3::test_support::command(env!("CARGO_BIN_EXE_st3"))
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("CLAUDE_FIXTURE_LOG", &log)
        .current_dir(local.parent().unwrap().parent().unwrap())
        .args(["claude-channel", "install", "--no-policy"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(
        calls.contains("plugin install st-channel@st --scope user --yes"),
        "{calls}"
    );
    assert!(
        calls
            .trim_end()
            .ends_with("plugin disable st-channel@st --scope user"),
        "{calls}"
    );
    assert!(!calls.contains("--scope local") && !calls.contains("--scope project"));
    assert_eq!(std::fs::read(local).unwrap(), settings);
}

#[test]
fn identity_without_subject_is_a_configuration_error_instead_of_idle_mcp() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let output = st3::test_support::command(env!("CARGO_BIN_EXE_st3"))
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env_remove("ST3_SUBJECT")
        .arg("--endpoint")
        .arg(root.path().join("absent.sock"))
        .args(["driver", "claude-mcp", "--identity", "example/one"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("identity requires a subject"));
    assert!(output.stdout.is_empty());
    assert!(!root.path().join("state").exists());
}

#[test]
fn unsupported_provider_cannot_enable_the_plugin_before_disable_support_is_checked() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CLAUDE_FIXTURE_LOG\"\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = root.path().join("commands");
    let output = st3::test_support::command(env!("CARGO_BIN_EXE_st3"))
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("CLAUDE_FIXTURE_LOG", &log)
        .current_dir(root.path())
        .args(["claude-channel", "install", "--no-policy"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("update Claude first"));
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "plugin disable --help\n"
    );
}
