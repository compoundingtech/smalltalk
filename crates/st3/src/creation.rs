//! Declaration builders shared by client actions and CLI creation.
use kdl::{KdlDocument, KdlEntry, KdlNode};

/// The Claude settings the fleet's Claude seats run with: st's own channel plugin on, and the
/// plugins st2's marketplace shipped off.
pub const CLAUDE_SEAT_SETTINGS: &str = r#"{"enabledPlugins":{"st2-channel@st2":false,"st3-channel@st2":false,"st3-channel@st3":false,"st-channel@st":true}}"#;

/// The declaration `st agents new` publishes: what a person writes by hand for a fleet seat.
/// Claude and Codex seats get the harness defaults the fleet's existing seats run with.
pub fn agent_document(
    args: &st3_client::AgentCreateParameters,
    workspace: &str,
    create_workspace: bool,
    creation_key: Option<&str>,
) -> String {
    let mut body = KdlDocument::new();
    if let Some(description) = &args.description {
        body.nodes_mut()
            .push(kdl_node("description", [description.as_str()]));
    }
    if let Some(host) = &args.host {
        body.nodes_mut().push(kdl_node("host", [host.as_str()]));
    }
    let mut workspace = kdl_node("workspace", [workspace]);
    if create_workspace {
        workspace
            .entries_mut()
            .push(KdlEntry::new_prop("create", true));
    }
    body.nodes_mut().push(workspace);
    let arguments: &[&str] = match args.harness.as_str() {
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
            &[
                "--dangerously-skip-permissions",
                "--settings",
                CLAUDE_SEAT_SETTINGS,
            ]
        }
        "codex" => &[
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust",
        ],
        _ => &[],
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
    if !harness_body.nodes().is_empty() {
        harness.set_children(harness_body);
    }
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
    let mut terminal = kdl_node("pty", [identity.as_str()]);
    let mut body = KdlDocument::new();
    body.nodes_mut().extend([
        kdl_node("name", [args.name.as_str()]),
        kdl_node("host", [host]),
        kdl_node("workspace", [cwd]),
        kdl_node("command", ["exec \"${SHELL:-/bin/sh}\" -i"]),
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
            let idle = agent_document(&parameters, "/tmp", true, None);
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
            let source = agent_document(&parameters, "/tmp", true, Some("first-launch"));
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
}
