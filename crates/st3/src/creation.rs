//! Declaration builders shared by client actions and CLI creation.
use kdl::{KdlDocument, KdlEntry, KdlNode};

pub use st3_client::agent_branch;

pub fn validate_agent_checkout(args: &st3_client::AgentCreateParameters) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.repo.is_some()
            || (args.base.is_none() && args.branch.is_none() && args.remove_at_run_end.is_none()),
        "base, branch and remove_at_run_end require a repository"
    );
    if let Some(repository) = &args.repo {
        anyhow::ensure!(
            std::path::Path::new(repository).is_absolute(),
            "repo must be absolute on the selected host"
        );
    }
    Ok(())
}

/// The Claude settings the fleet's Claude seats run with: st's own channel plugin on, and the
/// plugins st2's marketplace shipped off.
pub const CLAUDE_SEAT_SETTINGS: &str = r#"{"enabledPlugins":{"st2-channel@st2":false,"st3-channel@st2":false,"st3-channel@st3":false,"st-channel@st":true}}"#;

/// What auto mode's classifier is told about st, so it does not treat the seat's own daemon as an
/// outside destination. These go in the `--settings` JSON because the classifier reads `autoMode`
/// from user, managed and `--settings` sources only, never from a workspace's
/// `.claude/settings.local.json`. `$defaults` keeps Claude's built-in rules in every list.
const AUTO_MODE_ENVIRONMENT: &str = "Key internal services: the local `st` daemon (Smalltalk), reached only through the `st` command over a unix socket on this machine. The daemon, its socket and the other agents it connects this seat to are trusted infrastructure of this fleet, not an external destination.";
const AUTO_MODE_ALLOW: &str = "St Commands: Running the `st` command against the local st daemon is ordinary control-plane traffic for this seat's work: reading and answering messages, listing, claiming and completing mission steps, recording progress, and reading or storing st documents. It is not an external data flow.";

/// The `--settings` JSON for a Claude seat started in `mode`. Bypass seats keep exactly the JSON
/// they always had; an auto-mode seat also tells the classifier that st is trusted infrastructure.
pub fn claude_seat_settings(mode: crate::permission_mode::PermissionMode) -> String {
    match mode {
        crate::permission_mode::PermissionMode::Bypass => CLAUDE_SEAT_SETTINGS.to_owned(),
        crate::permission_mode::PermissionMode::Auto => {
            let mut settings: serde_json::Value = serde_json::from_str(CLAUDE_SEAT_SETTINGS)
                .expect("the seat settings are JSON");
            settings["autoMode"] = serde_json::json!({
                "environment": ["$defaults", AUTO_MODE_ENVIRONMENT],
                "allow": ["$defaults", AUTO_MODE_ALLOW],
            });
            settings.to_string()
        }
    }
}

/// The declaration `st agents new` publishes: what a person writes by hand for a fleet seat.
/// Claude and Codex seats get the harness defaults the fleet's existing seats run with; a Claude
/// seat starts in `claude_mode`.
pub fn agent_document(
    args: &st3_client::AgentCreateParameters,
    workspace: &str,
    create_workspace: bool,
    creation_key: Option<&str>,
    claude_mode: crate::permission_mode::PermissionMode,
) -> String {
    let mut body = KdlDocument::new();
    if let Some(description) = &args.description {
        body.nodes_mut()
            .push(kdl_node("description", [description.as_str()]));
    }
    if let Some(host) = &args.host {
        body.nodes_mut().push(kdl_node("host", [host.as_str()]));
    }
    let mut workspace_node = kdl_node("workspace", [workspace]);
    if create_workspace && args.repo.is_none() {
        workspace_node
            .entries_mut()
            .push(KdlEntry::new_prop("create", true));
    }
    body.nodes_mut().push(workspace_node);
    if let Some(repository) = &args.repo {
        let branch = args
            .branch
            .clone()
            .unwrap_or_else(|| agent_branch(&args.name));
        let mut checkout = kdl_node("checkout", [repository.as_str()]);
        checkout.entries_mut().extend([
            KdlEntry::new_prop("base", args.base.as_deref().unwrap_or("origin/main")),
            KdlEntry::new_prop("branch", branch.as_str()),
        ]);
        if args.remove_at_run_end == Some(true) {
            checkout
                .entries_mut()
                .push(KdlEntry::new_prop("remove-at-run-end", true));
        }
        body.nodes_mut().push(checkout);
    }
    let claude_settings = claude_seat_settings(claude_mode);
    let arguments: Vec<&str> = match args.harness.as_str() {
        "claude" => {
            let mut environment = KdlNode::new("env");
            let mut variables = KdlDocument::new();
            variables
                .nodes_mut()
                .push(kdl_node("CLAUDE_CODE_CHILD_SESSION", ["0"]));
            environment.set_children(variables);
            body.nodes_mut().push(environment);
            body.nodes_mut().push(render_node(&[
                kdl_node("git-exclude", [".claude/"]),
                kdl_node(
                    "json-upsert",
                    [".claude/settings.local.json", CLAUDE_SEAT_SETTINGS],
                ),
            ]));
            let mut arguments = claude_mode.claude_flags().to_vec();
            arguments.extend(["--settings", claude_settings.as_str()]);
            arguments
        }
        // The workspace-write sandbox with automatic review of anything it blocks, plus the st
        // tool bridge the driver adds at launch. A Codex older than 0.147 does not have the flag;
        // its driver launches such a seat with the full-access flags instead.
        "codex" => vec!["--approve-for-me"],
        _ => Vec::new(),
    };
    let mut harness = kdl_node("harness", [args.harness.as_str()]);
    let mut harness_body = KdlDocument::new();
    if let Some(model) = &args.model {
        harness_body
            .nodes_mut()
            .push(kdl_node("model", [model.as_str()]));
    }
    if let Some(effort) = &args.effort {
        harness_body
            .nodes_mut()
            .push(kdl_node("effort", [effort.as_str()]));
    }
    if !arguments.is_empty() {
        harness_body
            .nodes_mut()
            .push(kdl_node("args", arguments.iter().copied()));
    }
    if let Some(message) = &args.message {
        let mut node = kdl_node("message", [message.as_str()]);
        node.entries_mut().push(KdlEntry::new_prop(
            "id",
            creation_key.expect("initial messages require a creation key"),
        ));
        harness_body.nodes_mut().push(node);
    }
    // The harness grammar requires a body even when the provider has no default flags.
    harness.set_children(harness_body);
    body.nodes_mut().push(harness);
    if let Some(key) = creation_key {
        body.nodes_mut().push(creation_tags(key));
    }
    body.nodes_mut().push(kdl_node("restart", ["always"]));
    let mut agent = kdl_node("agent", [args.name.as_str()]);
    agent.set_children(body);
    publication_document(agent)
}

fn render_node(operations: &[KdlNode]) -> KdlNode {
    let mut render = KdlNode::new("render");
    let mut body = KdlDocument::new();
    body.nodes_mut().extend(operations.iter().cloned());
    render.set_children(body);
    render
}

fn kdl_node<'a>(name: &str, values: impl IntoIterator<Item = &'a str>) -> KdlNode {
    let mut node = KdlNode::new(name);
    for value in values {
        node.entries_mut().push(KdlEntry::new(value));
    }
    node
}
fn publication_document(node: KdlNode) -> String {
    let mut document = KdlDocument::new();
    let mut version = KdlNode::new("version");
    version.entries_mut().push(KdlEntry::new(2));
    document.nodes_mut().extend([version, node]);
    document.autoformat();
    document.to_string()
}
fn creation_tags(key: &str) -> KdlNode {
    let mut tags = KdlNode::new("tags");
    tags.entries_mut()
        .push(KdlEntry::new_prop("st3.client.create-key", key));
    tags
}
/// A caller-owned PTY shell, with no harness or agent declaration.
pub fn terminal_document(
    person: &str,
    id: &str,
    args: &st3_client::TerminalCreateParameters,
    host: &str,
    cwd: &str,
    creation_key: &str,
) -> String {
    let identity = format!("{person}/{id}");
    // With no directory named, the shell starts in the host user's home: a service's own working
    // directory is `/` (Nathan, 2026-10-05). A home that is unset or missing leaves it where it is.
    let command = if cwd == "." {
        "cd \"${HOME:-.}\" 2>/dev/null; exec \"${SHELL:-/bin/sh}\" -i"
    } else {
        "exec \"${SHELL:-/bin/sh}\" -i"
    };
    let mut terminal = kdl_node("pty", [identity.as_str()]);
    let mut body = KdlDocument::new();
    body.nodes_mut().extend([
        kdl_node("name", [args.name.as_str()]),
        kdl_node("host", [host]),
        kdl_node("workspace", [cwd]),
        kdl_node("command", [command]),
        kdl_node("restart", ["never"]),
        creation_tags(creation_key),
    ]);
    terminal.set_children(body);
    publication_document(terminal)
}

/// Add an explicit first message using the harness's native CLI. The argument boundary preserves
/// text beginning with a dash; OpenCode's positional argument is a project, so it uses a flag.
pub fn append_native_message(
    harness: &str,
    argv: &mut Vec<String>,
    message: &str,
) -> anyhow::Result<()> {
    match harness {
        "opencode" => argv.push(format!("--prompt={message}")),
        "pi" if message.starts_with('@') => argv.extend(["--".into(), format!("\n{message}")]),
        "claude" | "codex" | "pi" | "omp" => argv.extend(["--".into(), message.into()]),
        _ => anyhow::bail!("unknown native harness {harness}"),
    }
    Ok(())
}

/// Record one attempt to invoke a provider with its first message. The receipt survives restart
/// and replication. A crash between this commit and provider spawn does not replay the prompt.
pub async fn claim_initial_message(
    client: &crate::client::Client,
    subject: &str,
    id: &str,
    incarnation: &str,
) -> anyhow::Result<bool> {
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(subject, id))?));
    let marker = format!("custom/agent/initial-message-{key}");
    let path = format!(
        "/v1/claims?subject={}&limit=1",
        urlencoding::encode(&marker)
    );
    let prior: crate::model::ClaimsPage = client.get(&path).await?;
    if !prior.claims.is_empty() {
        return Ok(false);
    }
    let input = crate::model::ClaimInput {
        subject: marker,
        kind: "custom.agent.initial-message".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("incarnation".into(), Value::String(incarnation.into())),
            ("agent".into(), Value::String(subject.into())),
            ("message_id".into(), Value::String(id.into())),
        ]),
        evidence: vec![],
        expected_subject: Some(None),
        idempotency_key: Some(format!("initial-message:{key}")),
    };
    match client
        .post::<_, crate::model::ClaimRecord>("/v1/claims", &input)
        .await
    {
        Ok(_) => Ok(true),
        Err(error) => {
            // A concurrent incarnation may already have committed this key. Accept only an
            // actual durable receipt; connection errors still propagate and retry normally.
            let prior: crate::model::ClaimsPage = client.get(&path).await?;
            if prior.claims.is_empty() {
                Err(error)
            } else {
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_mode::PermissionMode::{Auto, Bypass};

    #[test]
    fn a_shell_with_no_directory_starts_in_home_and_one_with_a_directory_keeps_it() {
        let args = st3_client::TerminalCreateParameters {
            name: "scratch".into(),
            ..Default::default()
        };
        let plain = terminal_document("person/example", "one", &args, "host", ".", "key");
        // The command is quoted for KDL, so its own quotes are escaped.
        assert!(
            plain.contains("cd \\\"${HOME:-.}\\\" 2>/dev/null; exec"),
            "{plain}"
        );
        assert!(plain.contains("${SHELL:-/bin/sh}"), "{plain}");
        let placed = terminal_document("person/example", "two", &args, "host", "/srv/work", "key");
        assert!(!placed.contains("HOME"), "{placed}");
        assert!(placed.contains("workspace \"/srv/work\""), "{placed}");
        // The command really lands in HOME, and falls back when HOME is unset or missing.
        for (home, expected) in [(Some("/tmp"), "/tmp"), (Some("/no/such/dir"), "/")] {
            let mut shell = std::process::Command::new("sh");
            shell.current_dir("/");
            shell.args(["-c", "cd \"${HOME:-.}\" 2>/dev/null; pwd"]);
            match home {
                Some(home) => shell.env("HOME", home),
                None => shell.env_remove("HOME"),
            };
            let out = shell.output().unwrap();
            let expected = std::fs::canonicalize(expected).unwrap();
            let got = String::from_utf8(out.stdout).unwrap();
            assert_eq!(
                std::fs::canonicalize(got.trim()).unwrap(),
                expected,
                "HOME={home:?}"
            );
        }
    }
    #[test]
    fn created_agents_use_checkout_for_new_and_existing_branches_and_keep_plain_workspaces() {
        use crate::checkout::Checkout;
        use crate::checkout::test_support::{git, repository};
        let root = tempfile::tempdir().unwrap();
        let repository = repository(root.path());
        let parameters = st3_client::AgentCreateParameters {
            name: "agent/example.parser".into(),
            harness: "codex".into(),
            repo: Some(repository.display().to_string()),
            remove_at_run_end: Some(true),
            ..Default::default()
        };
        let workspace = root.path().join("parser");
        let source = agent_document(&parameters, &workspace.display().to_string(), true, None, Bypass);
        let intent = crate::graph::parse_intent(&source, "example").unwrap();
        let desired = intent.subjects.values().next().unwrap();
        let checkout = Checkout::from_desired(&desired.desired).unwrap();
        assert_eq!(checkout.branch, "parser");
        assert_eq!(checkout.base, "origin/main");
        assert!(!desired.member.as_ref().unwrap().workspace_create);
        checkout.create(&workspace).unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("README")).unwrap(),
            "second\n"
        );
        // Existing branches retain their own commits instead of resetting to the base.
        std::fs::write(workspace.join("README"), "agent work\n").unwrap();
        git(&workspace, &["commit", "--quiet", "-am", "agent work"]);
        checkout.remove(&workspace).unwrap();
        checkout.create(&workspace).unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("README")).unwrap(),
            "agent work\n"
        );
        checkout.remove(&workspace).unwrap();
        let missing = st3_client::AgentCreateParameters {
            repo: Some(root.path().join("missing").display().to_string()),
            ..parameters.clone()
        };
        let source = agent_document(&missing, &workspace.display().to_string(), true, None, Bypass);
        let intent = crate::graph::parse_intent(&source, "example").unwrap();
        let checkout =
            Checkout::from_desired(&intent.subjects.values().next().unwrap().desired).unwrap();
        assert!(
            checkout
                .create(&workspace)
                .unwrap_err()
                .to_string()
                .contains("failed")
        );
        assert!(!workspace.exists());
        let plain = st3_client::AgentCreateParameters {
            repo: None,
            remove_at_run_end: None,
            ..parameters
        };
        let source = agent_document(&plain, &workspace.display().to_string(), true, None, Bypass);
        let intent = crate::graph::parse_intent(&source, "example").unwrap();
        let desired = intent.subjects.values().next().unwrap();
        assert!(Checkout::from_desired(&desired.desired).is_none());
        assert!(desired.member.as_ref().unwrap().workspace_create);
    }

    #[test]
    fn default_branches_are_simple_safe_seat_names() {
        for (name, branch) in [
            ("parser", "parser"),
            ("agent/example.parser", "parser"),
            ("fleet/example/--parser.lock", "parser-lock"),
            ("agent/example/HEAD", "head"),
            ("???", "agent"),
        ] {
            assert_eq!(agent_branch(name), branch);
            let output = std::process::Command::new("git")
                .args(["check-ref-format", "--branch", branch])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "Git rejected default branch {branch}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let args = st3_client::AgentCreateParameters {
            branch: Some("parser".into()),
            ..Default::default()
        };
        assert!(validate_agent_checkout(&args).is_err());
    }
    #[test]
    fn pi_initial_text_does_not_expand_a_file_argument() {
        let mut argv = vec!["pi".into()];
        append_native_message("pi", &mut argv, "@private-file").unwrap();
        assert_eq!(argv, ["pi", "--", "\n@private-file"]);
    }
    #[test]
    fn first_messages_use_native_arguments_and_leave_defaults_idle() {
        for harness in ["claude", "codex", "pi", "omp", "opencode"] {
            let parameters = st3_client::AgentCreateParameters {
                name: "worker".into(),
                harness: harness.into(),
                model: Some("model".into()),
                ..Default::default()
            };
            let idle = agent_document(&parameters, "/tmp", true, None, Bypass);
            let idle = crate::graph::parse_intent(&idle, "test").unwrap();
            let member = idle
                .subjects
                .values()
                .next()
                .unwrap()
                .member
                .as_ref()
                .unwrap();
            let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
                panic!()
            };
            assert!(!argv.iter().any(|arg| arg == "--initial-message"));
            let parameters = st3_client::AgentCreateParameters {
                message: Some("--literal\ntext".into()),
                ..parameters
            };
            let source = agent_document(&parameters, "/tmp", true, Some("first-launch"), Bypass);
            let intent = crate::graph::parse_intent(&source, "test").unwrap();
            let member = intent
                .subjects
                .values()
                .next()
                .unwrap()
                .member
                .as_ref()
                .unwrap();
            let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
                panic!()
            };
            assert_eq!(
                argv[argv
                    .iter()
                    .position(|arg| arg == "--initial-message")
                    .unwrap()
                    + 1],
                "--literal\ntext"
            );
            let mut native = vec![harness.into(), "--model".into(), "model".into()];
            append_native_message(harness, &mut native, "--literal\ntext").unwrap();
            if harness == "opencode" {
                assert_eq!(native.last().unwrap(), "--prompt=--literal\ntext");
            } else {
                assert_eq!(&native[3..], &["--", "--literal\ntext"]);
            }
        }
    }

    #[test]
    fn codex_seats_start_with_automatic_review_and_no_hook_trust_bypass() {
        let args = st3_client::AgentCreateParameters {
            name: "worker".into(),
            harness: "codex".into(),
            ..Default::default()
        };
        let source = agent_document(&args, "/srv/work", true, None, Bypass);
        let intent = crate::graph::parse_intent(&source, "example").unwrap();
        let member = intent.subjects.values().next().unwrap().member.as_ref().unwrap();
        let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
            panic!()
        };
        assert!(argv.iter().any(|argument| argument == "--approve-for-me"), "{argv:?}");
        assert!(
            !argv.iter().any(|argument| argument.starts_with("--dangerously-bypass")),
            "{argv:?}"
        );
    }
    fn claude_seat(mode: crate::permission_mode::PermissionMode) -> Vec<String> {
        let args = st3_client::AgentCreateParameters {
            name: "worker".into(),
            harness: "claude".into(),
            ..Default::default()
        };
        let source = agent_document(&args, "/srv/work", true, None, mode);
        let intent = crate::graph::parse_intent(&source, "example").unwrap();
        let member = intent.subjects.values().next().unwrap().member.as_ref().unwrap();
        let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
            panic!()
        };
        argv.clone()
    }

    #[test]
    fn a_bypass_claude_seat_is_declared_exactly_as_it_always_was() {
        let argv = claude_seat(Bypass);
        let joined = argv.join(" ");
        assert!(joined.contains("--dangerously-skip-permissions"), "{joined}");
        assert!(!joined.contains("--permission-mode"), "{joined}");
        assert!(argv.iter().any(|argument| argument == CLAUDE_SEAT_SETTINGS), "{joined}");
        assert_eq!(claude_seat_settings(Bypass), CLAUDE_SEAT_SETTINGS);
    }

    #[test]
    fn an_auto_claude_seat_asks_for_auto_and_tells_the_classifier_st_is_trusted() {
        let argv = claude_seat(Auto);
        let position = argv.iter().position(|argument| argument == "--permission-mode").unwrap();
        assert_eq!(argv[position + 1], "auto");
        assert!(!argv.iter().any(|argument| argument == "--dangerously-skip-permissions"));
        // The launch may add settings of its own; the seat's is the one that carries autoMode.
        let settings: serde_json::Value = argv
            .iter()
            .filter_map(|argument| serde_json::from_str::<serde_json::Value>(argument).ok())
            .find(|value| value.get("autoMode").is_some())
            .unwrap_or_else(|| panic!("no --settings JSON carries autoMode: {argv:?}"));
        // The plugin switches stay, and every list keeps Claude's own rules.
        assert_eq!(settings["enabledPlugins"]["st-channel@st"], true);
        for list in ["environment", "allow"] {
            assert_eq!(settings["autoMode"][list][0], "$defaults", "{list}");
            assert!(settings["autoMode"][list][1].as_str().unwrap().contains("`st`"));
        }
        assert!(settings["autoMode"].get("soft_deny").is_none());
        assert!(settings["autoMode"].get("hard_deny").is_none());
    }

    /// The classifier must read the st entries from the `--settings` JSON. Claude can print the
    /// effective auto mode config without a model call, so this proves the JSON is honoured on the
    /// installed Claude Code. Without `claude` on PATH there is nothing to check.
    #[test]
    fn claude_reads_the_st_entries_from_the_settings_json_it_is_given() {
        let Ok(output) = std::process::Command::new("claude")
            .args(["--settings", &claude_seat_settings(Auto), "auto-mode", "config"])
            .output()
        else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let config: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let has = |list: &str, text: &str| {
            config[list].as_array().unwrap().iter().any(|entry| entry.as_str() == Some(text))
        };
        assert!(has("environment", AUTO_MODE_ENVIRONMENT), "{config}");
        assert!(has("allow", AUTO_MODE_ALLOW), "{config}");
        // `$defaults` kept the built-in rules.
        assert!(config["soft_deny"].as_array().unwrap().len() > 10);
        assert!(config["environment"].as_array().unwrap().len() > 2);
    }
}
