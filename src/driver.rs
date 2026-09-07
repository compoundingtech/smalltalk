//! Pure typed-driver expansion into ordinary hand-authorable Agent Spec KDL primitives.
//!
//! Expansion does not read files, inspect a harness, mutate a declaration, or execute a process.
//! Print, reconcile, and materialization use this same expansion.

use agent_spec::spec::{
    AgentSpec, ClaudeDriver, CodexDriver, DeliveryTransport, Driver, OmpDriver, OpenCodeDriver,
    PiDriver, TaskKind,
};
use anyhow::{Context, Result};
use kdl::{KdlDocument, KdlEntry, KdlNode};

const ST2: &str = "st2";
const CATALOG: &str = "$CATALOG";
const DEV_CHANNELS_FLAG: &str = "--dangerously-load-development-channels";

/// What a materialized Claude channel server can actually do, given how Claude Code admits
/// channels.
///
/// Claude Code registers a `server:` channel only when its allowed-channels entry carries
/// `dev: true`, and that flag is set in exactly one place: the merge of the entries parsed from
/// `--dangerously-load-development-channels`. Measured admission table: compoundingtech/st2#373.
pub const CHANNEL_NOT_REGISTERED: &str = concat!(
    "this Claude seat launches without the packaged st2 channel or an admitted development ",
    "channel, so no inbox message reaches the model through native delivery. See ",
    "compoundingtech/st2#373",
);

/// The other half of a skipped channel: whether anything else carries this seat's inbox. The
/// `ding` sidecar is opt-in and is never implied by a driver, so a seat can have no inbox
/// transport at all — which is worth saying out loud rather than leaving to be discovered.
pub const CHANNEL_NO_INBOX_TRANSPORT: &str = concat!(
    "and this seat declares no `ding` sidecar, so nothing else carries its inbox either: ",
    "messages sent to it reach the model by no path at all",
);

/// The explicit development-channel flag bypasses the packaged channel and requires consent.
pub const CHANNEL_DEV_CONSENT_REQUIRED: &str = concat!(
    "this seat launches with `--dangerously-load-development-channels`, which stops startup at ",
    "a consent dialog: no MCP server connects and the startup prompt is not read until a human ",
    "attaches to the pane and accepts. The consent is session state only, so the dialog returns ",
    "on every launch. See compoundingtech/st2#373",
);

/// A shell program can reach the flag through a wrapper, an alias, or a variable, so the absence
/// of the flag from its source text proves nothing. Saying so beats asserting the wrong state.
pub const CHANNEL_ROUTE_UNKNOWN: &str = concat!(
    "the st2 MCP channel server is materialized and this seat launches an opaque shell program, ",
    "so st2 cannot tell whether Claude Code receives `--dangerously-load-development-channels`. ",
    "Either the channel is silently skipped or startup stops at the consent dialog; declare the ",
    "launch as `argv` to make this legible. See compoundingtech/st2#373",
);

/// What the launch this seat will actually run does about the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelRoute {
    /// Nothing on the command line registers a channel.
    Unregistered,
    /// The command line carries the development-channels flag.
    DevConsent,
    /// The typed driver selects the approved marketplace plugin.
    Packaged,
    /// The launch is an opaque shell program, so neither can be proven.
    Opaque,
}

/// Structured argv carries the flag only as a bare argument or in `--flag=value` form. Prefix
/// lookalikes are distinct arguments and must not trigger the development-channel diagnostic.
fn carries_dev_channels(argument: &str) -> bool {
    argument == DEV_CHANNELS_FLAG
        || argument
            .strip_prefix(DEV_CHANNELS_FLAG)
            .is_some_and(|value| value.starts_with('='))
}

fn claude_channel_route(spec: &AgentSpec) -> Option<ChannelRoute> {
    match (&spec.driver, spec.delivery) {
        // An authored development flag stays explicit. The typed field selects the packaged
        // plugin and writes no project MCP state.
        (Some(Driver::Claude(driver)), _) => Some(
            if driver
                .args
                .iter()
                .any(|argument| carries_dev_channels(argument))
            {
                ChannelRoute::DevConsent
            } else if driver.dev_channels {
                ChannelRoute::Packaged
            } else {
                ChannelRoute::Unregistered
            },
        ),
        // The legacy transport renders the same `.mcp.json` from a hand-authored launch.
        // Structured argv is legible, but a shell program can reach or merely mention the flag
        // through shell syntax, wrappers, aliases, and variables, so its source is always opaque.
        (None, Some(DeliveryTransport::Mcp)) => {
            let mut opaque = false;
            for task in &spec.tasks {
                if let Some(argv) = task.argv.as_deref()
                    && argv.iter().any(|argument| carries_dev_channels(argument))
                {
                    return Some(ChannelRoute::DevConsent);
                }
                if task.command.is_some() {
                    opaque = true;
                }
            }
            Some(if opaque {
                ChannelRoute::Opaque
            } else {
                ChannelRoute::Unregistered
            })
        }
        _ => None,
    }
}

/// Whether this declaration carries the opt-in DING sidecar.
fn declares_ding_sidecar(spec: &AgentSpec) -> bool {
    spec.tasks
        .iter()
        .any(|task| task.derived && task.kind == TaskKind::Exec && task.name == "ding")
}

/// The channel advisories that apply to one spec.
///
/// Pure, like the rest of this module: it reads the declaration and nothing else. Callers that
/// actually create a seat surface these; read-only expansion stays silent.
pub fn claude_channel_advisories(spec: &AgentSpec) -> Vec<&'static str> {
    let Some(route) = claude_channel_route(spec) else {
        return Vec::new();
    };
    let mut advisories = vec![match route {
        ChannelRoute::Unregistered => CHANNEL_NOT_REGISTERED,
        ChannelRoute::DevConsent => CHANNEL_DEV_CONSENT_REQUIRED,
        ChannelRoute::Packaged => return Vec::new(),
        ChannelRoute::Opaque => CHANNEL_ROUTE_UNKNOWN,
    }];
    // Only a proven skip justifies the stronger claim; an opaque launch may yet deliver.
    if route == ChannelRoute::Unregistered && !declares_ding_sidecar(spec) {
        advisories.push(CHANNEL_NO_INBOX_TRANSPORT);
    }
    advisories
}

/// Reject two launch sources before expansion, task compilation, or workspace writes.
pub(crate) fn ensure_single_source(spec: &AgentSpec) -> Result<()> {
    anyhow::ensure!(
        spec.driver.is_none() || spec.delivery.is_none(),
        "agent '{}' declares both a driver block and `deliver`; choose one launch source",
        spec.identity
    );
    Ok(())
}

/// Expand one typed driver into KDL nodes that can be written inside an `agent {}` block.
pub fn expand_driver(spec: &AgentSpec, this_host: &str) -> Result<KdlDocument> {
    ensure_single_source(spec)?;
    let driver = spec
        .driver
        .as_ref()
        .with_context(|| format!("agent '{}' has no driver block", spec.identity))?;
    anyhow::ensure!(
        spec.host.is_some() || !this_host.is_empty(),
        "agent '{}' has no host and driver expansion received no host fallback",
        spec.identity
    );
    let bus_id = spec.bus_id(this_host);
    let mut output = match driver {
        Driver::Claude(driver) => expand_claude(driver, &bus_id)?,
        Driver::Codex(driver) => expand_codex(driver, &bus_id),
        Driver::Pi(driver) => expand_pi(driver, &bus_id),
        Driver::OpenCode(driver) => expand_opencode(driver, &bus_id),
        Driver::Omp(driver) => expand_omp(driver, &bus_id),
    };
    output.autoformat();
    Ok(output)
}

fn expand_codex(driver: &CodexDriver, bus_id: &str) -> KdlDocument {
    let mut provider = vec!["codex".to_string()];
    if let Some(model) = &driver.model {
        provider.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &driver.effort {
        provider.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]);
    }
    provider.extend(driver.args.iter().cloned());
    provider.push(driver.prompt.clone());

    let mut argv = vec![
        ST2.to_string(),
        "--catalog".to_string(),
        CATALOG.to_string(),
        "driver".to_string(),
        "codex".to_string(),
        "--identity".to_string(),
        bus_id.to_string(),
        "--runtime-id".to_string(),
        bus_id.to_string(),
        "--".to_string(),
    ];
    argv.extend(provider);
    document([node("argv", argv)])
}

/// pi needs no rendered configuration file: its channel is an extension, and the wrapper injects
/// that extension from this binary's verified hook set rather than writing a machine-local path
/// into the declaration. `-a` accepts the workspace for this run only, which is why no pi analogue
/// of [`crate::pretrust`] exists — nothing in the operator's ambient pi config is mutated.
fn expand_pi(driver: &PiDriver, bus_id: &str) -> KdlDocument {
    let mut provider = vec!["pi".to_string(), "-a".to_string()];
    if let Some(model) = &driver.model {
        provider.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &driver.effort {
        provider.extend(["--thinking".to_string(), effort.clone()]);
    }
    provider.extend(driver.args.iter().cloned());
    provider.push(driver.prompt.clone());

    let mut argv = vec![
        ST2.to_string(),
        "--catalog".to_string(),
        CATALOG.to_string(),
        "driver".to_string(),
        "pi-session".to_string(),
        "--identity".to_string(),
        bus_id.to_string(),
        "--runtime-id".to_string(),
        bus_id.to_string(),
        "--".to_string(),
    ];
    argv.extend(provider);
    document([node("argv", argv)])
}

/// omp needs no rendered configuration file either: its channel is a pi-style extension the
/// wrapper injects from this binary's verified hook set. omp has no pi `-a` equivalent it needs
/// for a workspace launch — the wrapper already runs with the workspace as cwd.
fn expand_omp(driver: &OmpDriver, bus_id: &str) -> KdlDocument {
    let mut provider = vec!["omp".to_string()];
    if let Some(model) = &driver.model {
        provider.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &driver.effort {
        provider.extend(["--thinking".to_string(), effort.clone()]);
    }
    provider.extend(driver.args.iter().cloned());
    provider.push(driver.prompt.clone());

    let mut argv = vec![
        ST2.to_string(),
        "--catalog".to_string(),
        CATALOG.to_string(),
        "driver".to_string(),
        "omp-session".to_string(),
        "--identity".to_string(),
        bus_id.to_string(),
        "--runtime-id".to_string(),
        bus_id.to_string(),
        "--".to_string(),
    ];
    argv.extend(provider);
    document([node("argv", argv)])
}

/// OpenCode's server surface is wrapper-owned runtime state: the wrapper allocates the port and
/// password at launch, so the expansion stays a pure declaration with no machine-local values.
fn expand_opencode(driver: &OpenCodeDriver, bus_id: &str) -> KdlDocument {
    let mut provider = vec!["opencode".to_string()];
    if let Some(model) = &driver.model {
        provider.extend(["--model".to_string(), model.clone()]);
    }
    provider.extend(driver.args.iter().cloned());
    // Unlike the sibling harnesses, OpenCode's positional argument is a project directory; the
    // startup prompt is a named flag.
    provider.extend(["--prompt".to_string(), driver.prompt.clone()]);

    let mut argv = vec![
        ST2.to_string(),
        "--catalog".to_string(),
        CATALOG.to_string(),
        "driver".to_string(),
        "opencode-session".to_string(),
        "--identity".to_string(),
        bus_id.to_string(),
        "--runtime-id".to_string(),
        bus_id.to_string(),
        "--".to_string(),
    ];
    argv.extend(provider);
    document([node("argv", argv)])
}

fn expand_claude(driver: &ClaudeDriver, bus_id: &str) -> Result<KdlDocument> {
    // The same registration a hand-authored seat carries: without it a driver-declared
    // seat has no observed-state producer and no lifecycle hooks at all.
    let settings = serde_json::to_string_pretty(&crate::hooks::claude_settings_registration())?;
    let mut render = KdlNode::new("render");
    render.set_children(document([{
        // Hook arrays join whatever the workspace already declares: replacement would clobber
        // user-registered hooks on every materialization, and union is idempotent.
        let mut upsert = node(
            "json-upsert",
            vec![".claude/settings.local.json".to_string(), settings],
        );
        upsert
            .entries_mut()
            .push(KdlEntry::new_prop("arrays", "union"));
        upsert
    }]));

    let mut provider = vec!["claude".to_string()];
    if let Some(model) = &driver.model {
        provider.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &driver.effort {
        provider.extend(["--effort".to_string(), effort.clone()]);
    }
    if driver.dev_channels {
        provider.extend([
            "--channels".to_string(),
            crate::claude_channel::CHANNEL.to_string(),
        ]);
    }
    provider.extend(driver.args.iter().cloned());
    provider.push(driver.prompt.clone());

    let mut argv = vec![
        ST2.to_string(),
        "--catalog".to_string(),
        CATALOG.to_string(),
        "driver".to_string(),
        "claude-session".to_string(),
        "--identity".to_string(),
        bus_id.to_string(),
        "--runtime-id".to_string(),
        bus_id.to_string(),
        "--".to_string(),
    ];
    argv.extend(provider);
    Ok(document([render, node("argv", argv)]))
}

fn node(name: &str, args: Vec<String>) -> KdlNode {
    let mut node = KdlNode::new(name);
    node.entries_mut()
        .extend(args.into_iter().map(KdlEntry::new));
    node
}

fn document<const N: usize>(nodes: [KdlNode; N]) -> KdlDocument {
    let mut document = KdlDocument::new();
    document.nodes_mut().extend(nodes);
    document
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_spec::spec::{AgentDesiredState, JobType, Task};
    use kdl::KdlValue;

    use super::*;

    fn spec(driver: Driver) -> AgentSpec {
        AgentSpec {
            id: None,
            address: None,
            identity: "worker".into(),
            name: None,
            description: None,
            host: Some("host".into()),
            role: None,
            job_type: JobType::Service,
            workspace: Some("/work".into()),
            supervisor: None,
            desired_state: AgentDesiredState::Running,
            keep: false,
            restart: None,
            delivery: None,
            session_driver: None,
            driver: Some(driver),
            delivery_readiness: None,
            resources: Vec::new(),
            streams: Vec::new(),
            tasks: Vec::new(),
            path: PathBuf::from("/catalog/agents/host/worker/agent.kdl"),
        }
    }

    fn legacy_launch(command: Option<&str>, argv: Option<&[&str]>) -> AgentSpec {
        let mut launch = spec(Driver::Claude(ClaudeDriver {
            model: None,
            effort: None,
            dev_channels: false,
            prompt: String::new(),
            args: Vec::new(),
        }));
        launch.delivery = Some(DeliveryTransport::Mcp);
        launch.driver = None;
        launch.tasks.push(Task {
            kind: TaskKind::Pty,
            derived: false,
            name: "agent".into(),
            id: None,
            command: command.map(str::to_owned),
            argv: argv.map(|arguments| {
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect()
            }),
            cwd: None,
            tags: Default::default(),
            env: Default::default(),
            keep: false,
            lifecycle: Default::default(),
        });
        launch
    }

    fn strings(node: &KdlNode) -> Vec<&str> {
        node.entries()
            .iter()
            .filter_map(|entry| match entry.value() {
                KdlValue::String(value) => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn structured_argv_recognizes_only_exact_dev_channels_flag_spellings() {
        for argument in [
            DEV_CHANNELS_FLAG,
            "--dangerously-load-development-channels=server:st2",
        ] {
            let launch = legacy_launch(None, Some(&["claude", argument]));
            assert_eq!(
                claude_channel_route(&launch),
                Some(ChannelRoute::DevConsent),
                "{argument}"
            );
        }

        let lookalike = legacy_launch(
            None,
            Some(&["claude", "--dangerously-load-development-channels-extra"]),
        );
        assert_eq!(
            claude_channel_route(&lookalike),
            Some(ChannelRoute::Unregistered)
        );
    }

    #[test]
    fn legacy_shell_commands_are_opaque_even_when_the_flag_is_quoted_or_commented() {
        for command in [
            "claude",
            "printf '%s' '--dangerously-load-development-channels'",
            "claude # --dangerously-load-development-channels",
        ] {
            let launch = legacy_launch(Some(command), None);
            assert_eq!(
                claude_channel_route(&launch),
                Some(ChannelRoute::Opaque),
                "{command}"
            );
            assert_eq!(
                claude_channel_advisories(&launch),
                [CHANNEL_ROUTE_UNKNOWN],
                "{command}"
            );
        }
    }

    #[test]
    fn codex_expands_to_one_plain_argv_with_typed_fields_before_verbatim_args() {
        let output = expand_driver(
            &spec(Driver::Codex(CodexDriver {
                model: Some("gpt-5.6-sol".into()),
                effort: Some("xhigh".into()),
                prompt: "Start work.".into(),
                args: vec!["--model".into(), "override".into()],
            })),
            "unused",
        )
        .unwrap();

        assert_eq!(output.nodes().len(), 1);
        assert_eq!(
            strings(output.get("argv").unwrap()),
            [
                "st2",
                "--catalog",
                "$CATALOG",
                "driver",
                "codex",
                "--identity",
                "host.worker",
                "--runtime-id",
                "host.worker",
                "--",
                "codex",
                "--model",
                "gpt-5.6-sol",
                "-c",
                "model_reasoning_effort=xhigh",
                "--model",
                "override",
                "Start work."
            ]
        );
    }

    #[test]
    fn opencode_expands_to_the_session_wrapper_with_a_prompt_flag() {
        let output = expand_driver(
            &spec(Driver::OpenCode(OpenCodeDriver {
                model: Some("anthropic/claude-opus-5".into()),
                prompt: "Start work.".into(),
                args: vec!["--agent".into(), "build".into()],
            })),
            "unused",
        )
        .unwrap();

        assert_eq!(output.nodes().len(), 1);
        assert_eq!(
            strings(output.get("argv").unwrap()),
            [
                "st2",
                "--catalog",
                "$CATALOG",
                "driver",
                "opencode-session",
                "--identity",
                "host.worker",
                "--runtime-id",
                "host.worker",
                "--",
                "opencode",
                "--model",
                "anthropic/claude-opus-5",
                "--agent",
                "build",
                "--prompt",
                "Start work."
            ]
        );
    }

    #[test]
    fn claude_expands_to_the_packaged_channel_and_session_owned_launch() {
        let output = expand_driver(
            &spec(Driver::Claude(ClaudeDriver {
                model: Some("opus".into()),
                effort: Some("xhigh".into()),
                dev_channels: true,
                prompt: "Start work.".into(),
                args: vec!["--model".into(), "override".into()],
            })),
            "unused",
        )
        .unwrap();

        assert_eq!(output.nodes().len(), 2);
        let render = output.get("render").unwrap();
        let upserts: Vec<&KdlNode> = render
            .children()
            .unwrap()
            .nodes()
            .iter()
            .filter(|node| node.name().value() == "json-upsert")
            .collect();
        assert_eq!(upserts.len(), 1);
        let settings = strings(upserts[0]);
        assert_eq!(settings[0], ".claude/settings.local.json");
        let settings: serde_json::Value = serde_json::from_str(settings[1]).unwrap();
        assert_eq!(settings, crate::hooks::claude_settings_registration());
        assert_eq!(
            strings(output.get("argv").unwrap()),
            [
                "st2",
                "--catalog",
                "$CATALOG",
                "driver",
                "claude-session",
                "--identity",
                "host.worker",
                "--runtime-id",
                "host.worker",
                "--",
                "claude",
                "--model",
                "opus",
                "--effort",
                "xhigh",
                "--channels",
                "plugin:st2-channel@st2",
                "--model",
                "override",
                "Start work."
            ]
        );
    }
}
