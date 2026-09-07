//! KDL job parsing — lower a canonical VRS `agent.kdl` into the same [`RawSpec`] the TOML/JSON path
//! uses (spec.md §2). KDL is the canonical format. This walks the node tree by hand (KDL is not
//! serde-native) and fills a [`RawSpec`], which then flows through the shared identity/host
//! resolution. A file may hold more than one `agent` node.
//!
//! Runner-normative fields, managed delivery readiness, and metadata `role` are read. Other
//! render-only fields (`harness`, `model`, `persona`, `permissions`, `transport`, `strategy`) and
//! the inert `meta{}` block are ignored.

use crate::declared::{DeclaredDocument, DeclaredNode, DeclaredValue};
use crate::spec::{
    ClaudeDriver, CodexDriver, DeliveryReadiness, OmpDriver, OpenCodeDriver, PiDriver, RawResource,
    RawRestart, RawSpec, RawTask, SessionDriver,
};

/// Lower an already parsed declaration document into the runner's raw representation.
pub(crate) fn lower_declared_document(document: &DeclaredDocument) -> anyhow::Result<Vec<RawSpec>> {
    document
        .agents
        .iter()
        .map(|agent| agent_node_to_raw(&agent.node))
        .collect()
}

/// First positional (unnamed) argument of a node, as a string.
fn arg_string(node: &DeclaredNode) -> Option<String> {
    node.argument(0)
        .and_then(DeclaredValue::as_str)
        .map(String::from)
}

/// Every positional argument of a node, as strings.
fn argv(node: &DeclaredNode) -> anyhow::Result<Vec<String>> {
    node.arguments()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("`argv` accepts only string arguments"))
        })
        .collect()
}

/// First positional argument as a bool (`#true`/`#false`), defaulting to `false`.
fn arg_bool(node: &DeclaredNode) -> bool {
    node.argument(0)
        .and_then(DeclaredValue::as_bool)
        .unwrap_or(false)
}

/// First positional argument as an integer.
fn arg_u32(node: &DeclaredNode) -> Option<u32> {
    node.argument(0)
        .and_then(DeclaredValue::as_integer)
        .and_then(|i| u32::try_from(i).ok())
}

fn agent_node_to_raw(node: &DeclaredNode) -> anyhow::Result<RawSpec> {
    let mut raw = RawSpec {
        identity: arg_string(node), // agent "<identity>" — may be overridden by an `identity` child
        ..Default::default()
    };

    // Environment is an agent-level scope in the compact format and cascades into explicit legacy
    // tasks too. Parse it first so declaration order does not change semantics.
    for child in &node.children {
        if child.name == "env" {
            raw.env.extend(env_node_to_raw(child));
        }
    }

    for child in &node.children {
        match child.name.as_str() {
            "id" => parse_single_string_field(child, "id", &mut raw.id)?,
            "address" => parse_single_string_field(child, "address", &mut raw.address)?,
            "identity" => raw.identity = arg_string(child).or(raw.identity),
            "name" => parse_single_string_field(child, "name", &mut raw.name)?,
            "description" => parse_single_string_field(child, "description", &mut raw.description)?,
            "host" => raw.host = arg_string(child),
            "role" => raw.role = arg_string(child),
            "type" => raw.job_type = arg_string(child),
            "workspace" => raw.workspace = arg_string(child),
            "supervisor" => raw.supervisor = arg_string(child),
            "retired" => {
                anyhow::ensure!(
                    raw.retired.is_none(),
                    "agent declares `retired` more than once"
                );
                raw.retired = Some(Some(arg_bool(child)));
            }
            "desired-state" => {
                anyhow::ensure!(
                    raw.desired_state.is_none(),
                    "agent declares `desired-state` more than once"
                );
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.children.is_empty()
                        && child.arguments().count() == 1
                        && child.properties_named("reason").count() <= 1
                        && child.entries.len() <= 2,
                    "agent `desired-state` must contain one state string and at most one `reason` property"
                );
                anyhow::ensure!(
                    child.entries.iter().all(|entry| {
                        entry.name.is_none() || entry.name.as_deref() == Some("reason")
                    }),
                    "agent `desired-state` accepts only the `reason` property"
                );
                let desired_state = arg_string(child);
                anyhow::ensure!(
                    desired_state.is_some(),
                    "agent desired-state value must be a string"
                );
                raw.desired_state = Some(desired_state);
                raw.desired_state_reason = child
                    .property("reason")
                    .map(|reason| reason.as_str().map(String::from));
                anyhow::ensure!(
                    child.property("reason").is_none()
                        || matches!(raw.desired_state_reason, Some(Some(_))),
                    "agent desired-state `reason` must be a string"
                );
            }
            "keep" => raw.keep = arg_bool(child),
            "lifecycle" => raw.lifecycle = arg_string(child),
            "restart" => raw.restart = Some(restart_node_to_raw(child)),
            "resource" => {
                let (name, resource) = resource_node_to_raw(child)?;
                raw.resource.insert(name, resource)?;
            }
            "command" => raw.command = arg_string(child),
            "argv" => raw.argv = Some(argv(child)?),
            "ding" => raw.ding = true,
            "deliver" => {
                anyhow::ensure!(
                    raw.deliver.is_none(),
                    "agent declares `deliver` more than once"
                );
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.children.is_empty()
                        && child.entries.len() == 1
                        && child.entries[0].name.is_none(),
                    "agent `deliver` must contain exactly one positional string"
                );
                raw.deliver = Some(Some(arg_string(child).ok_or_else(|| {
                    anyhow::anyhow!("agent `deliver` value must be a string")
                })?));
            }
            "session-driver" => {
                anyhow::ensure!(
                    raw.session_driver.is_none(),
                    "agent declares `session-driver` more than once"
                );
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.children.is_empty()
                        && child.entries.len() == 1
                        && child.entries[0].name.is_none(),
                    "agent `session-driver` must contain exactly one positional string"
                );
                raw.session_driver = Some(Some(arg_string(child).ok_or_else(|| {
                    anyhow::anyhow!("agent `session-driver` value must be a string")
                })?));
            }
            "delivery-readiness" => {
                anyhow::ensure!(
                    raw.delivery_readiness.is_none(),
                    "agent declares `delivery-readiness` more than once"
                );
                raw.delivery_readiness = Some(delivery_readiness_node_to_raw(child)?);
            }
            "claude" => {
                anyhow::ensure!(
                    raw.driver.claude.is_none(),
                    "agent declares `claude` more than once"
                );
                raw.driver.claude = Some(claude_driver_node_to_raw(child)?);
            }
            "codex" => {
                anyhow::ensure!(
                    raw.driver.codex.is_none(),
                    "agent declares `codex` more than once"
                );
                raw.driver.codex = Some(codex_driver_node_to_raw(child)?);
            }
            "pi" => {
                anyhow::ensure!(
                    raw.driver.pi.is_none(),
                    "agent declares `pi` more than once"
                );
                raw.driver.pi = Some(pi_driver_node_to_raw(child)?);
            }
            "opencode" => {
                anyhow::ensure!(
                    raw.driver.opencode.is_none(),
                    "agent declares `opencode` more than once"
                );
                raw.driver.opencode = Some(opencode_driver_node_to_raw(child)?);
            }
            "omp" => {
                anyhow::ensure!(
                    raw.driver.omp.is_none(),
                    "agent declares `omp` more than once"
                );
                raw.driver.omp = Some(omp_driver_node_to_raw(child)?);
            }
            "env" => {}
            "pty" => {
                if let Some(name) = arg_string(child) {
                    raw.pty.insert(name, task_node_to_raw(child)?);
                }
            }
            "exec" => {
                if let Some(name) = arg_string(child) {
                    raw.exec.insert(name, task_node_to_raw(child)?);
                }
            }
            "stream" => {
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.entries.len() == 1
                        && child.entries[0].name.is_none(),
                    "agent `stream` must contain exactly one positional name string and no properties"
                );
                let name = arg_string(child).ok_or_else(|| {
                    anyhow::anyhow!(
                        "agent `stream` must contain exactly one positional name string and no properties"
                    )
                })?;
                let stream = stream_node_to_raw(child, &name)?;
                anyhow::ensure!(
                    raw.stream.insert(name.clone(), stream).is_none(),
                    "agent declares `stream \"{name}\"` more than once"
                );
            }
            // meta, harness, model, persona, permissions, transport, strategy, … — ignored.
            _ => {}
        }
    }
    if raw.ding && raw.exec.contains_key("ding") {
        anyhow::bail!(
            "agent '{}' declares both compact `ding` and `exec \"ding\"`; choose one form",
            raw.identity.as_deref().unwrap_or("<unnamed>")
        );
    }
    Ok(raw)
}

fn driver_string(node: &DeclaredNode, provider: &str, field: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        node.type_name.is_none()
            && node.children.is_empty()
            && node.entries.len() == 1
            && node.entries[0].name.is_none(),
        "agent `{provider}.{field}` must contain exactly one positional string"
    );
    node.argument(0)
        .and_then(DeclaredValue::as_str)
        .map(String::from)
        .ok_or_else(|| {
            anyhow::anyhow!("agent `{provider}.{field}` must contain exactly one positional string")
        })
}

fn driver_args(node: &DeclaredNode, provider: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        node.type_name.is_none()
            && node.children.is_empty()
            && node.entries.iter().all(|entry| entry.name.is_none()),
        "agent `{provider}.args` must contain only positional strings"
    );
    node.arguments()
        .map(|value| {
            value.as_str().map(String::from).ok_or_else(|| {
                anyhow::anyhow!("agent `{provider}.args` must contain only positional strings")
            })
        })
        .collect()
}

fn driver_bool(node: &DeclaredNode, provider: &str, field: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(
        node.type_name.is_none()
            && node.children.is_empty()
            && node.entries.len() == 1
            && node.entries[0].name.is_none(),
        "agent `{provider}.{field}` must contain exactly one positional bool"
    );
    node.argument(0)
        .and_then(DeclaredValue::as_bool)
        .ok_or_else(|| {
            anyhow::anyhow!("agent `{provider}.{field}` must contain exactly one positional bool")
        })
}

type CommonDriverFields = (Option<String>, Option<String>, bool, String, Vec<String>);

fn common_driver_fields(
    node: &DeclaredNode,
    provider: &str,
    allow_dev_channels: bool,
) -> anyhow::Result<CommonDriverFields> {
    anyhow::ensure!(
        node.type_name.is_none() && node.entries.is_empty(),
        "agent `{provider}` must be a child block without entries"
    );
    let mut model = None;
    let mut effort = None;
    let mut dev_channels = None;
    let mut prompt = None;
    let mut args = None;
    for child in &node.children {
        match child.name.as_str() {
            "model" => {
                anyhow::ensure!(model.is_none(), "agent `{provider}` has duplicate `model`");
                model = Some(driver_string(child, provider, "model")?);
            }
            "effort" => {
                anyhow::ensure!(
                    effort.is_none(),
                    "agent `{provider}` has duplicate `effort`"
                );
                effort = Some(driver_string(child, provider, "effort")?);
            }
            "dev-channels" if allow_dev_channels => {
                anyhow::ensure!(
                    dev_channels.is_none(),
                    "agent `{provider}` has duplicate `dev-channels`"
                );
                dev_channels = Some(driver_bool(child, provider, "dev-channels")?);
            }
            "prompt" => {
                anyhow::ensure!(
                    prompt.is_none(),
                    "agent `{provider}` has duplicate `prompt`"
                );
                prompt = Some(driver_string(child, provider, "prompt")?);
            }
            "args" => {
                anyhow::ensure!(args.is_none(), "agent `{provider}` has duplicate `args`");
                args = Some(driver_args(child, provider)?);
            }
            other => anyhow::bail!("agent `{provider}` has unsupported field `{other}`"),
        }
    }
    Ok((
        model,
        effort,
        dev_channels.unwrap_or(false),
        prompt.ok_or_else(|| anyhow::anyhow!("agent `{provider}` requires `prompt`"))?,
        args.unwrap_or_default(),
    ))
}

fn claude_driver_node_to_raw(node: &DeclaredNode) -> anyhow::Result<ClaudeDriver> {
    let (model, effort, dev_channels, prompt, args) = common_driver_fields(node, "claude", true)?;
    Ok(ClaudeDriver {
        model,
        effort,
        dev_channels,
        prompt,
        args,
    })
}

fn codex_driver_node_to_raw(node: &DeclaredNode) -> anyhow::Result<CodexDriver> {
    let (model, effort, _, prompt, args) = common_driver_fields(node, "codex", false)?;
    Ok(CodexDriver {
        model,
        effort,
        prompt,
        args,
    })
}

fn pi_driver_node_to_raw(node: &DeclaredNode) -> anyhow::Result<PiDriver> {
    let (model, effort, _, prompt, args) = common_driver_fields(node, "pi", false)?;
    Ok(PiDriver {
        model,
        effort,
        prompt,
        args,
    })
}

fn omp_driver_node_to_raw(node: &DeclaredNode) -> anyhow::Result<OmpDriver> {
    let (model, effort, _, prompt, args) = common_driver_fields(node, "omp", false)?;
    Ok(OmpDriver {
        model,
        effort,
        prompt,
        args,
    })
}

fn opencode_driver_node_to_raw(node: &DeclaredNode) -> anyhow::Result<OpenCodeDriver> {
    let (model, effort, _, prompt, args) = common_driver_fields(node, "opencode", false)?;
    anyhow::ensure!(
        effort.is_none(),
        "agent `opencode` has unsupported field `effort` (OpenCode has no effort axis)"
    );
    Ok(OpenCodeDriver {
        model,
        prompt,
        args,
    })
}

/// Parse one direct child declaring exactly one positional string, at most once.
///
/// Shared by the presentation fields (`name`, `description`) and the routing fields (`id`,
/// `address`), which have the same source shape and the same once-only rule.
fn parse_single_string_field(
    node: &DeclaredNode,
    field: &str,
    destination: &mut Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        destination.is_none(),
        "agent declares `{field}` more than once"
    );
    anyhow::ensure!(
        node.children.is_empty() && node.entries.len() == 1 && node.entries[0].name.is_none(),
        "agent `{field}` must contain exactly one positional string"
    );
    let value = node
        .argument(0)
        .and_then(DeclaredValue::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent `{field}` must contain a string"))?;
    *destination = Some(value.to_owned());
    Ok(())
}

fn resource_node_to_raw(node: &DeclaredNode) -> anyhow::Result<(String, RawResource)> {
    if !node.children.is_empty() {
        anyhow::bail!("resource binding cannot have children");
    }

    let mut name = None;
    let mut uri = None;
    let mut reason = None;
    let mut inactive_reason = None;
    let mut selector = None;
    for entry in &node.entries {
        let Some(property) = entry.name.as_deref() else {
            if name.is_some() {
                anyhow::bail!("resource binding accepts exactly one positional name");
            }
            name = entry.value.as_str().map(String::from);
            if name.is_none() {
                anyhow::bail!("resource binding needs a string name");
            }
            continue;
        };

        let value = entry.value.as_str().map(String::from);
        match property {
            "uri" => {
                if uri.is_some() {
                    anyhow::bail!("resource binding has duplicate `uri`");
                }
                uri = value;
                if uri.is_none() {
                    anyhow::bail!("resource binding needs string `uri`");
                }
            }
            "reason" => {
                if reason.is_some() {
                    anyhow::bail!("resource binding has duplicate `reason`");
                }
                reason = value;
                if reason.is_none() {
                    anyhow::bail!("resource binding needs string `reason`");
                }
            }
            "inactive-reason" => {
                if inactive_reason.is_some() {
                    anyhow::bail!("resource binding has duplicate `inactive-reason`");
                }
                inactive_reason = value;
                if inactive_reason.is_none() {
                    anyhow::bail!("resource binding needs string `inactive-reason`");
                }
            }
            "selector" => {
                if selector.is_some() {
                    anyhow::bail!("resource binding has duplicate `selector`");
                }
                let encoded = value
                    .ok_or_else(|| anyhow::anyhow!("resource binding needs string `selector`"))?;
                selector = Some(
                    serde_json::from_str(&encoded)
                        .map_err(|error| anyhow::anyhow!("resource binding `selector` is not valid JSON: {error}"))?,
                );
            }
            other => anyhow::bail!("resource binding has unsupported property `{other}`"),
        }
    }

    Ok((
        name.ok_or_else(|| anyhow::anyhow!("resource binding needs a string name"))?,
        RawResource {
            uri: uri.ok_or_else(|| anyhow::anyhow!("resource binding needs string `uri`"))?,
            reason: reason
                .ok_or_else(|| anyhow::anyhow!("resource binding needs string `reason`"))?,
            inactive_reason,
            selector,
        },
    ))
}

fn restart_node_to_raw(node: &DeclaredNode) -> RawRestart {
    let mut r = RawRestart::default();
    for child in &node.children {
        match child.name.as_str() {
            "attempts" => r.attempts = arg_u32(child),
            "interval" => r.interval = arg_string(child),
            "delay" => r.delay = arg_string(child),
            "mode" => r.mode = arg_string(child),
            _ => {}
        }
    }
    r
}

fn delivery_readiness_node_to_raw(node: &DeclaredNode) -> anyhow::Result<DeliveryReadiness> {
    anyhow::ensure!(
        node.type_name.is_none() && node.children.is_empty(),
        "agent `delivery-readiness` cannot have a type annotation or children"
    );
    let kind = node
        .argument(0)
        .and_then(DeclaredValue::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent `delivery-readiness` needs a kind string"))?;
    match kind {
        "credential" => {
            anyhow::ensure!(
                node.arguments().count() == 1
                    && node.properties_named("account-id").count() <= 1
                    && node.entries.len() <= 2,
                "credential delivery-readiness must be `delivery-readiness \"credential\"` with at most one string `account-id`"
            );
            let account_id = node
                .property("account-id")
                .map(|value| {
                    value.as_str().map(String::from).ok_or_else(|| {
                        anyhow::anyhow!(
                            "credential delivery-readiness `account-id` must be a string"
                        )
                    })
                })
                .transpose()?;
            Ok(DeliveryReadiness::Credential { account_id })
        }
        "anonymous" => {
            anyhow::ensure!(
                node.arguments().count() >= 2
                    && node.properties_named("harness").count() == 1
                    && node.entries.len() == node.arguments().count() + 1,
                "anonymous delivery-readiness must be `delivery-readiness \"anonymous\" \"<model>\"… harness=\"<driver>\"`"
            );
            let harness = node
                .property("harness")
                .and_then(DeclaredValue::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("anonymous delivery-readiness `harness` must be a string")
                })
                .and_then(SessionDriver::from_name)?;
            let models = node
                .arguments()
                .skip(1)
                .map(|value| {
                    value.as_str().map(String::from).ok_or_else(|| {
                        anyhow::anyhow!(
                            "anonymous delivery-readiness accepts only string model arguments"
                        )
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(DeliveryReadiness::Anonymous { harness, models })
        }
        other => anyhow::bail!(
            "unsupported delivery-readiness kind '{other}' (expected `credential` or `anonymous`)"
        ),
    }
}

fn task_node_to_raw(node: &DeclaredNode) -> anyhow::Result<RawTask> {
    let mut t = RawTask::default();
    for child in &node.children {
        match child.name.as_str() {
            "id" => t.id = arg_string(child),
            "command" => t.command = arg_string(child),
            "argv" => t.argv = Some(argv(child)?),
            "cwd" => t.cwd = arg_string(child),
            "keep" => t.keep = arg_bool(child),
            "lifecycle" => t.lifecycle = arg_string(child),
            // `tags role="agent" "st.network"="$CATALOG"` — properties on the node.
            "tags" => {
                for entry in &child.entries {
                    if let (Some(name), Some(val)) = (entry.name.as_deref(), entry.value.as_str()) {
                        t.tags.insert(name.to_string(), val.to_string());
                    }
                }
            }
            // `env { KEY "value"; … }` — child nodes, name = key, first arg = value.
            "env" => {
                t.env.extend(env_node_to_raw(child));
            }
            _ => {}
        }
    }
    Ok(t)
}

/// `stream "<name>" { command "…" }` or `stream "<name>" { argv "prog" "arg" }`.
///
/// The child set is deliberately minimal. A stream declares WHERE events come from; everything about
/// how they are supervised is inherited from the agent (restart policy, teardown, parking), and
/// everything about how they are delivered is the bus contract. `every` is rejected by the
/// declaration parser rather than accepted here: an interval makes this scheduled work, which is
/// the reserved `schedule` node's contract.
fn stream_node_to_raw(node: &DeclaredNode, name: &str) -> anyhow::Result<crate::spec::RawStream> {
    let mut stream = crate::spec::RawStream::default();
    for child in &node.children {
        match child.name.as_str() {
            "command" => {
                anyhow::ensure!(
                    stream.command.is_none(),
                    "stream '{name}' has duplicate `command`"
                );
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.children.is_empty()
                        && child.entries.len() == 1
                        && child.entries[0].name.is_none(),
                    "stream '{name}' `command` must be exactly one positional string"
                );
                stream.command = Some(arg_string(child).ok_or_else(|| {
                    anyhow::anyhow!(
                        "stream '{name}' `command` must be exactly one positional string"
                    )
                })?);
            }
            "argv" => {
                anyhow::ensure!(
                    stream.argv.is_none(),
                    "stream '{name}' has duplicate `argv`"
                );
                anyhow::ensure!(
                    child.type_name.is_none()
                        && child.children.is_empty()
                        && child.entries.iter().all(|entry| entry.name.is_none()),
                    "stream '{name}' `argv` must contain only positional string arguments"
                );
                stream.argv = Some(argv(child)?);
            }
            "every" => anyhow::bail!(
                "stream '{name}' declares `every`; scheduled work is the reserved `schedule` \
                 contract, a stream is a long-running event source"
            ),
            other => anyhow::bail!("stream '{name}' has unsupported field `{other}`"),
        }
    }
    anyhow::ensure!(
        !(stream.command.is_some() && stream.argv.is_some()),
        "stream '{name}' must declare at most one of `command` or `argv`"
    );
    Ok(stream)
}

fn env_node_to_raw(node: &DeclaredNode) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    for child in &node.children {
        if let Some(value) = arg_string(child) {
            env.insert(child.name.clone(), value);
        }
    }
    env
}
