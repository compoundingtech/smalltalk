use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agent_spec::spec::Driver;
use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand};
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use st2::eval_spec::{Check, JsonScalar, JudgeKind, Spec as EvalSpec};
use walkdir::WalkDir;

const WAIT_TEAM_DONE: &[u8] = include_bytes!("../assets/wait-team-done.sh");
const ST3_CONTEXT_VARIABLES: &[&str] = &[
    "ST_MISSION",
    "ST_MISSION_REVISION",
    "ST_MISSION_RUN",
    "ST_RUN_GENERATION",
    "ST_ROOT_MISSION_RUN",
    "ST_SCOPE",
    "ST_WORKSPACE",
    "ST_REQUESTER",
    "ST_STEP",
    "ST_STEP_RUN",
    "ST_ATTEMPT",
    "ST_ASSIGNEE",
    "ST_PARENT_STEP_RUN",
    "ST_GATE",
    "ST_AGENT",
    "ST3_SUBJECT",
];
const EXPERIMENTAL_WARNING: &str =
    "Experimental output. Review every generated KDL file before you run it.";

#[derive(Parser)]
#[command(
    name = "st3-migrate",
    version,
    about = "Offline st2 to st3 KDL transformer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    File(FileArgs),
    Catalog(TreeArgs),
    Evals(TreeArgs),
}

#[derive(Args)]
struct FileArgs {
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    report: PathBuf,
}

#[derive(Args)]
struct TreeArgs {
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value = "local")]
    host: String,
}

#[derive(Debug, Serialize)]
struct Report {
    schema: &'static str,
    experimental: bool,
    review_required: bool,
    mode: String,
    input: String,
    output: String,
    files: Vec<FileReport>,
    documents: Vec<DocumentReport>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FileReport {
    input: String,
    output: String,
    subjects: Vec<String>,
    source_hash: String,
}

#[derive(Debug, Serialize)]
struct DocumentReport {
    name: String,
    hash: String,
    source: String,
    staged: String,
    put_command: String,
}

fn main() -> Result<()> {
    eprintln!("st3-migrate: {EXPERIMENTAL_WARNING}");
    match Cli::parse().command {
        Command::File(args) => migrate_file(args),
        Command::Catalog(args) => migrate_catalog(args),
        Command::Evals(args) => migrate_evals(args),
    }
}

fn migrate_file(args: FileArgs) -> Result<()> {
    anyhow::ensure!(
        args.input != args.output,
        "the output must differ from the input"
    );
    let source = fs::read_to_string(&args.input)?;
    let mut warnings = vec![EXPERIMENTAL_WARNING.into()];
    warnings.extend(legacy_resource_warnings(&source, &args.input)?);
    let transformed = transform_declaration(&source, None)?;
    let host = "local";
    let normalized = st3::parse_intent(&transformed, host)?;
    let runtime_subjects = st3::validate_mission_runtimes(&normalized, host)?;
    write_file(&args.output, transformed.as_bytes())?;
    let report = Report {
        schema: "st3-migrate-report.v1",
        experimental: true,
        review_required: true,
        mode: "file".into(),
        input: args.input.display().to_string(),
        output: args.output.display().to_string(),
        files: vec![FileReport {
            input: args.input.display().to_string(),
            output: args.output.display().to_string(),
            subjects: normalized
                .subjects
                .keys()
                .cloned()
                .chain(runtime_subjects)
                .collect(),
            source_hash: normalized.source_hash,
        }],
        documents: Vec::new(),
        warnings,
    };
    write_report(&args.report, &report)
}

fn migrate_catalog(args: TreeArgs) -> Result<()> {
    validate_tree_args(&args)?;
    clear_generated_assets(&args.output)?;
    let discovery = agent_spec::discovery::discover_strict(&args.input);
    anyhow::ensure!(
        discovery.errors.is_empty(),
        "catalog discovery failed: {}",
        discovery
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("; ")
    );
    let states = discovery
        .specs
        .iter()
        .map(|spec| (spec.path.clone(), spec.desired_state.is_running()))
        .collect::<BTreeMap<_, _>>();
    let mut report = new_report("catalog", &args);
    let mut files = discovery
        .declarations
        .iter()
        .filter(|declaration| {
            declaration
                .path
                .extension()
                .and_then(|value| value.to_str())
                == Some("kdl")
        })
        .map(|declaration| declaration.path.clone())
        .collect::<Vec<_>>();
    files.sort();
    for input in files {
        let relative = input.strip_prefix(&args.input)?;
        let output = args.output.join(relative);
        let source = fs::read_to_string(&input)?;
        report
            .warnings
            .extend(legacy_resource_warnings(&source, &input)?);
        let running = states.get(&input).copied();
        let mut transformed = transform_declaration(&source, running)?;
        let documents =
            rewrite_render_documents(&mut transformed, &args.input, &args.output, relative)?;
        report.documents.extend(documents);
        let normalized = st3::parse_intent(&transformed, &args.host)
            .with_context(|| format!("validate transformed {}", input.display()))?;
        let runtime_subjects = st3::validate_mission_runtimes(&normalized, &args.host)
            .with_context(|| format!("validate deferred runtimes in {}", input.display()))?;
        write_file(&output, transformed.as_bytes())?;
        report.files.push(FileReport {
            input: input.display().to_string(),
            output: output.display().to_string(),
            subjects: normalized
                .subjects
                .keys()
                .cloned()
                .chain(runtime_subjects)
                .collect(),
            source_hash: normalized.source_hash,
        });
    }
    report.warnings.extend(discovery.warnings);
    write_report(&args.report, &report)
}

fn migrate_evals(args: TreeArgs) -> Result<()> {
    validate_tree_args(&args)?;
    let mut report = new_report("evals", &args);
    for input in eval_definition_files(&args.input)? {
        let source = fs::read_to_string(&input)?;
        let spec = match st2::eval_spec::parse_spec(&source) {
            Ok(spec) if spec.eval.is_some() => spec,
            Ok(_) => continue,
            Err(error) => {
                report
                    .warnings
                    .push(format!("{} is not an eval: {error}", input.display()));
                continue;
            }
        };
        let cell = input.parent().context("eval KDL has no parent")?;
        let relative_cell = cell.strip_prefix(&args.input)?;
        let output_cell = args.output.join(relative_cell);
        fs::create_dir_all(&output_cell)?;
        clear_generated_assets(&output_cell)?;
        copy_eval_assets(
            cell,
            &output_cell,
            &input,
            spec.eval.as_ref().and_then(|eval| eval.copy.as_deref()),
        )?;
        let (transformed, documents) = transform_eval(&spec, cell, &output_cell, &args.host)?;
        report.documents.extend(documents);
        let normalized = st3::parse_intent(&transformed, &args.host)
            .with_context(|| format!("validate transformed {}", input.display()))?;
        let runtime_subjects = st3::validate_mission_runtimes(&normalized, &args.host)
            .with_context(|| format!("validate deferred runtimes in {}", input.display()))?;
        let output = output_cell.join("eval.kdl");
        write_file(&output, transformed.as_bytes())?;
        report.files.push(FileReport {
            input: input.display().to_string(),
            output: output.display().to_string(),
            subjects: normalized
                .subjects
                .keys()
                .cloned()
                .chain(runtime_subjects)
                .collect(),
            source_hash: normalized.source_hash,
        });
    }
    write_report(&args.report, &report)
}

fn transform_declaration(source: &str, running: Option<bool>) -> Result<String> {
    let document: KdlDocument = source
        .parse()
        .with_context(|| format!("parse legacy checkpoint KDL after harness rewrite:\n{source}"))?;
    let version = st2::kdl_version::document_version(&document)?;
    let roots = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() != "version")
        .collect::<Vec<_>>();
    if version == 2 && st3::parse_intent(source, "local").is_ok() {
        return Ok(source.to_owned());
    }
    anyhow::ensure!(
        st2::kdl_version::ST2_KDL_VERSIONS.contains(&version),
        "the source uses unsupported KDL version {version}"
    );
    anyhow::ensure!(!document.nodes().is_empty(), "the declaration is empty");
    let mut children = KdlDocument::new();
    for node in roots {
        anyhow::ensure!(
            node.name().value() == "agent",
            "old declaration root `{}` is not an agent",
            node.name().value()
        );
        let name = node
            .get(0)
            .and_then(|value| value.as_string())
            .context("old agent has no name")?;
        let host = node
            .children()
            .and_then(|body| body.get("host"))
            .and_then(|host| host.get(0))
            .and_then(|value| value.as_string());
        let identity = node
            .children()
            .and_then(|body| body.get("identity"))
            .and_then(|identity| identity.get(0))
            .and_then(|value| value.as_string())
            .unwrap_or(name);
        let bus = if identity.contains('.') {
            identity.to_owned()
        } else if let Some(host) = host {
            format!("{host}.{identity}")
        } else {
            identity.to_owned()
        };
        let mut agent = node.clone();
        let body = agent.children_mut().get_or_insert_with(KdlDocument::new);
        let needs_ding = body
            .nodes()
            .iter()
            .any(|child| child.name().value() == "ding");
        body.nodes_mut().retain(|child| {
            !matches!(
                child.name().value(),
                "retired"
                    | "desired-state"
                    | "suspended"
                    | "role"
                    | "type"
                    | "supervisor"
                    | "keep"
                    | "lifecycle"
                    | "deliver"
                    | "ding"
                    | "meta"
                    | "resource"
                    | "stream"
            ) && !(child.name().value() == "harness" && child.children().is_none())
        });
        if needs_ding {
            body.nodes_mut().push(ding_exec_node());
        }
        rewrite_harness_nodes(body);
        remove_legacy_context_hooks(body);
        remove_legacy_lifecycle_metadata(body);
        remove_reserved_context_envs(body);
        rewrite_path_variables(body);
        let has_restart_type = body.nodes().iter().any(|child| {
            child.name().value() == "restart"
                && child.children().is_none()
                && child.get(0).and_then(|value| value.as_string()).is_some()
        });
        if !has_restart_type {
            let mut restart = KdlNode::new("restart");
            restart.entries_mut().push(KdlEntry::new("always"));
            body.nodes_mut().push(restart);
        }
        let mut mission = KdlNode::new("mission");
        mission
            .entries_mut()
            .push(KdlEntry::new(format!("catalog/{bus}")));
        mission.entries_mut().push(KdlEntry::new_prop(
            "state",
            if running == Some(false) {
                "retired"
            } else {
                "ready"
            },
        ));
        let mut mission_body = KdlDocument::new();
        let mut goal = KdlNode::new("goal");
        goal.entries_mut()
            .push(KdlEntry::new(format!("Keep agent {bus} available.")));
        mission_body.nodes_mut().push(goal);
        if running != Some(false) {
            mission_body.nodes_mut().push(agent);
        }
        mission.set_children(mission_body);
        children.nodes_mut().push(mission);
    }
    let mut output = KdlDocument::new();
    let mut version = KdlNode::new("version");
    version.entries_mut().push(KdlEntry::new(2));
    output.nodes_mut().push(version);
    output.nodes_mut().extend(children.nodes().iter().cloned());
    output.autoformat();
    Ok(rewrite_catalog_text(&output.to_string()))
}

fn rewrite_harness_nodes(document: &mut KdlDocument) {
    for node in document.nodes_mut() {
        let provider = match node.name().value() {
            "claude" | "codex" | "pi" | "opencode" | "omp" if node.children().is_some() => {
                Some(node.name().value().to_owned())
            }
            _ => None,
        };
        if let Some(provider) = provider {
            node.set_name("harness");
            node.entries_mut().insert(0, KdlEntry::new(provider));
        }
    }
}

fn ding_exec_node() -> KdlNode {
    let mut exec = KdlNode::new("exec");
    exec.entries_mut().push(KdlEntry::new("ding"));
    let mut body = KdlDocument::new();
    let mut argv = KdlNode::new("argv");
    for value in ["st3", "driver", "ding"] {
        argv.entries_mut().push(KdlEntry::new(value));
    }
    body.nodes_mut().push(argv);
    exec.set_children(body);
    exec
}

fn rewrite_path_variables(document: &mut KdlDocument) {
    for node in document.nodes_mut() {
        for entry in node.entries_mut() {
            if let KdlValue::String(value) = entry.value_mut() {
                *value = rewrite_path_variable(value);
            }
        }
        if let Some(children) = node.children_mut() {
            rewrite_path_variables(children);
        }
    }
}

fn rewrite_path_variable(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(index) = remaining.find("$PATH") {
        output.push_str(&remaining[..index]);
        let suffix = &remaining[index + "$PATH".len()..];
        if suffix
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            output.push_str("$PATH");
        } else {
            output.push_str("${PATH}");
        }
        remaining = suffix;
    }
    output.push_str(remaining);
    output
}

fn legacy_resource_warnings(source: &str, input: &Path) -> Result<Vec<String>> {
    fn visit(
        document: &KdlDocument,
        identity: Option<&str>,
        input: &Path,
        warnings: &mut Vec<String>,
    ) {
        for node in document.nodes() {
            let next_identity = if node.name().value() == "agent" {
                node.children()
                    .and_then(|body| body.get("identity"))
                    .and_then(|identity| identity.get(0))
                    .and_then(|value| value.as_string())
                    .or_else(|| node.get(0).and_then(|value| value.as_string()))
                    .or(identity)
            } else {
                identity
            };
            if node.name().value() == "resource" {
                let name = node
                    .get(0)
                    .and_then(|value| value.as_string())
                    .unwrap_or("resource");
                let uri = node
                    .get("uri")
                    .and_then(|value| value.as_string())
                    .unwrap_or("");
                if let Some(path) = uri.strip_prefix("file://") {
                    let owner = next_identity.unwrap_or("agent").replace('/', "-");
                    warnings.push(format!(
                        "{}: import legacy resource `{name}` as a document after review: st3 documents put {} --as {}",
                        input.display(),
                        shell(path),
                        shell(&format!("doc/migration/{owner}/{name}")),
                    ));
                } else {
                    warnings.push(format!(
                        "{}: legacy resource `{name}` was not migrated; declare a typed st3 resource after review",
                        input.display()
                    ));
                }
            }
            if let Some(children) = node.children() {
                visit(children, next_identity, input, warnings);
            }
        }
    }

    let document: KdlDocument = source.parse()?;
    let mut warnings = Vec::new();
    visit(&document, None, input, &mut warnings);
    Ok(warnings)
}

fn remove_legacy_context_hooks(document: &mut KdlDocument) {
    document.nodes_mut().retain(|node| {
        if node.name().value() != "json-upsert" {
            return true;
        }
        let content = node
            .get(1)
            .and_then(|value| value.as_string())
            .unwrap_or("");
        !content.contains("codex-pre-compact.sh")
            && !content.contains("codex-session-start.sh")
            && !content.contains("codex-stop.sh")
    });
    for node in document.nodes_mut() {
        if node.name().value() == "json-upsert"
            && let Some(content) = node.get(1).and_then(|value| value.as_string())
            && content.contains("$ST_HOOKS/claude-")
            && let Some(cleaned) = clean_claude_hook_json(content)
        {
            node.entries_mut()[1] = KdlEntry::new(cleaned);
        }
        if let Some(children) = node.children_mut() {
            remove_legacy_context_hooks(children);
        }
    }
}

fn remove_reserved_context_envs(document: &mut KdlDocument) {
    for node in document.nodes_mut() {
        if node.name().value() == "env"
            && let Some(environment) = node.children_mut()
        {
            environment
                .nodes_mut()
                .retain(|entry| !is_reserved_context_variable(entry.name().value()));
        }
        if let Some(children) = node.children_mut() {
            remove_reserved_context_envs(children);
        }
    }
}

fn remove_legacy_lifecycle_metadata(document: &mut KdlDocument) {
    document
        .nodes_mut()
        .retain(|node| node.name().value() != "lifecycle" || node.children().is_none());
    for node in document.nodes_mut() {
        if let Some(children) = node.children_mut() {
            remove_legacy_lifecycle_metadata(children);
        }
    }
}

fn is_reserved_context_variable(name: &str) -> bool {
    ST3_CONTEXT_VARIABLES.contains(&name)
}

fn clean_claude_hook_json(content: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(content).ok()?;
    let hooks = value.get_mut("hooks")?.as_object_mut()?;
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        groups.retain_mut(|group| {
            let Some(entries) = group
                .get_mut("hooks")
                .and_then(|value| value.as_array_mut())
            else {
                return true;
            };
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(|value| value.as_str())
                    .is_some_and(|command| command.contains("$ST_HOOKS/claude-"))
            });
            !entries.is_empty()
        });
        !groups.is_empty()
    });
    serde_json::to_string_pretty(&value).ok()
}

fn rewrite_render_documents(
    source: &mut String,
    catalog_root: &Path,
    output_root: &Path,
    relative: &Path,
) -> Result<Vec<DocumentReport>> {
    let mut document: KdlDocument = source.parse()?;
    let mut documents = Vec::new();
    for root in document.nodes_mut() {
        rewrite_copy_nodes(root, catalog_root, output_root, relative, &mut documents)?;
    }
    document.autoformat();
    *source = document.to_string();
    Ok(documents)
}

fn rewrite_copy_nodes(
    node: &mut KdlNode,
    catalog_root: &Path,
    output_root: &Path,
    relative: &Path,
    documents: &mut Vec<DocumentReport>,
) -> Result<()> {
    if node.name().value() == "copy"
        && let Some(source) = node.get(0).and_then(|value| value.as_string())
    {
        let path = catalog_root.join(source);
        if path.is_file() {
            let bytes = fs::read(&path)?;
            let bytes = match std::str::from_utf8(&bytes) {
                Ok(text) => rewrite_catalog_text(text).into_bytes(),
                Err(_) => bytes,
            };
            let hash = hex::encode(Sha256::digest(&bytes));
            let clean = source.trim_start_matches("./").replace(['\\', ' '], "-");
            let name = format!("doc/catalog/{clean}");
            let reference = format!("{name}@{hash}");
            node.entries_mut()[0] = KdlEntry::new(reference);
            documents.push(stage_document(output_root, &name, &hash, &path, &bytes)?);
        }
    }
    if let Some(children) = node.children_mut() {
        for child in children.nodes_mut() {
            rewrite_copy_nodes(child, catalog_root, output_root, relative, documents)?;
        }
    }
    let _ = relative;
    Ok(())
}

fn rewrite_catalog_text(source: &str) -> String {
    source
        .split_inclusive('\n')
        .map(|line| {
            if line.contains("Catalog selection on every catalog-aware command:") {
                return "st3 selects its daemon through --endpoint or ST3_ENDPOINT.\n".into();
            }
            if line.contains("Bus ops retain --root as a legacy") {
                return String::new();
            }
            if line.trim_start().starts_with("${XDG_STATE_HOME:")
                || (line.contains("Bus ops retain") && line.contains("--root"))
            {
                return String::new();
            }
            line.replace("`st2` CLI", "`st3` CLI")
                .replace("The host (`st2 up`)", "The host (`st3 up`)")
                .replace("st2 message", "st3 conversations")
                .replace("st2 status", "st3 status")
                .replace("st2 agents", "st3 agents")
                .replace("st2 context", "st3 context")
                .replace("st2 resource", "st3 resource")
                .replace("st2 bus", "st3 graph message API")
                .replace("st2 boot ritual", "st3 boot ritual")
        })
        .collect()
}

fn transform_eval(
    spec: &EvalSpec,
    cell: &Path,
    output_cell: &Path,
    host: &str,
) -> Result<(String, Vec<DocumentReport>)> {
    let (mut legacy, documents) = transform_eval_checkpoint(spec, cell, output_cell, host)?;
    let eval = spec.eval.as_ref().context("eval is missing")?;
    let mut identities = spec
        .agents
        .iter()
        .chain(eval.agents.iter())
        .map(|agent| eval_agent_identity(&agent.id, host))
        .filter(|identity| identity != "requester")
        .collect::<Vec<_>>();
    identities.sort_by_key(|identity| std::cmp::Reverse(identity.len()));
    identities.dedup();
    let mut document: KdlDocument = legacy
        .parse()
        .context("parse translated eval before agent reference rewrite")?;
    rewrite_eval_agent_references(&mut document, &identities);
    document.autoformat();
    legacy = document.to_string();
    legacy = legacy.replace("${EVAL_ROOT}", "${ST_WORKSPACE}");
    let name = cell
        .file_name()
        .and_then(|value| value.to_str())
        .context("eval name is not UTF-8")?;
    Ok((checkpoint_intent_to_mission(&legacy, name)?, documents))
}

fn rewrite_eval_agent_references(document: &mut KdlDocument, identities: &[String]) {
    for node in document.nodes_mut() {
        let agent_node = node.name().value() == "agent";
        for (index, entry) in node.entries_mut().iter_mut().enumerate() {
            if agent_node && index == 0 && entry.name().is_none() {
                continue;
            }
            let KdlValue::String(value) = entry.value_mut() else {
                continue;
            };
            for identity in identities {
                let actor = format!("agent/${{ST_MISSION_RUN}}/{identity}");
                *value = value.replace(identity, &actor);
                *value = value.replace(&format!("agent/{actor}"), &actor);
            }
        }
        if let Some(children) = node.children_mut() {
            rewrite_eval_agent_references(children, identities);
        }
    }
}

fn transform_eval_checkpoint(
    spec: &EvalSpec,
    cell: &Path,
    output_cell: &Path,
    host: &str,
) -> Result<(String, Vec<DocumentReport>)> {
    let eval = spec.eval.as_ref().context("eval is missing")?;
    let name = cell
        .file_name()
        .and_then(|value| value.to_str())
        .context("eval name is not UTF-8")?;
    let sequence = format!("eval/{name}");
    let restart = if eval.supervise { "always" } else { "never" };
    let mut documents = Vec::new();
    let mut output = String::new();
    output.push_str("version 2\n");
    output.push_str(&format!("checkpoints {sequence:?} {{\n"));
    let mut team_checkpoint = String::new();
    team_checkpoint.push_str("  checkpoint \"The eval team is running\" {\n");
    for agent in spec.agents.iter().chain(eval.agents.iter()) {
        write_eval_agent(&mut team_checkpoint, agent, restart, host);
    }
    if let Some(kick) = &eval.message {
        let content = if cell.join(&kick.content).is_file() {
            let path = cell.join(&kick.content);
            let bytes = fs::read(&path)?;
            let hash = hex::encode(Sha256::digest(&bytes));
            let doc_name = format!("doc/evals/{name}/task");
            documents.push(stage_document(
                output_cell,
                &doc_name,
                &hash,
                &path,
                &bytes,
            )?);
            format!("{doc_name}@{hash}")
        } else {
            kick.content.clone()
        };
        team_checkpoint.push_str(&format!(
            "        message \"kickoff/${{ST_MISSION_RUN}}\" {{\n          from {:?}\n          to {:?}\n          content {:?}\n        }}\n",
            eval_agent_identity(&kick.from, host),
            eval_agent_identity(&kick.to, host),
            content
        ));
    }
    let agents = spec
        .agents
        .iter()
        .chain(eval.agents.iter())
        .collect::<Vec<_>>();
    let has_team_checkpoint = !agents.is_empty() || eval.message.is_some();
    if !agents.is_empty() {
        for (ordinal, agent) in agents.into_iter().enumerate() {
            let subject = format!("agent/{}", eval_agent_identity(&agent.id, host));
            if agent.driver.is_some() {
                // The checkpoint reconciler requires a native driver to reach ready, working, or
                // idle before it evaluates this explicit existence predicate.
                team_checkpoint.push_str(&format!(
                    "      gate {:?} {{ exists {subject:?} }}\n",
                    format!("team member {} is ready", ordinal + 1)
                ));
            } else {
                team_checkpoint.push_str(&format!(
                    "      gate {:?} {{ field \"status\" {subject:?} \"is\" \"running\" }}\n",
                    format!("team member {} is running", ordinal + 1)
                ));
            }
        }
    }
    team_checkpoint.push_str(&format!(
        "      gate \"eval team startup deadline\" {{ deadline {:?} }}\n",
        format!("{}ms", eval.max_timeout.as_millis())
    ));
    team_checkpoint.push_str("    }\n");

    for (ordinal, step) in eval.run_steps.iter().enumerate() {
        let subject = format!("eval/{name}/run/{ordinal}-{}", step.id);
        output.push_str(&format!(
            "  checkpoint {:?} {{\n",
            format!("Run step {} finishes", step.id)
        ));
        output.push_str(&format!("        exec {subject:?} {{\n"));
        output.push_str(&format!("          host {host:?}\n"));
        output.push_str(&format!(
            "          workspace {:?}\n          cwd {:?}\n          command {:?}\n          restart \"never\"\n",
            "${ST_WORKSPACE}",
            step.workspace.as_deref().unwrap_or("${ST_WORKSPACE}"),
            rewrite_bus_command(&step.command)
        ));
        output.push_str("          env {\n");
        for (key, value) in &step.env {
            if key != "ST_ROOT"
                && key != "CATALOG"
                && key != "ST3_MESSAGE_ROOT"
                && !is_reserved_context_variable(key)
            {
                output.push_str(&format!("            {key} {value:?}\n"));
            }
        }
        output.push_str("            CATALOG \"${ST_WORKSPACE}\"\n");
        output.push_str("            ST_ROOT \"${ST_WORKSPACE}/.st3-messages\"\n");
        output.push_str("            ST3_MESSAGE_ROOT \"${ST_WORKSPACE}/.st3-messages\"\n");
        output.push_str("          }\n");
        if !step.unset.is_empty() {
            output.push_str("          unset");
            for name in &step.unset {
                output.push_str(&format!(" {name:?}"));
            }
            output.push('\n');
        }
        output.push_str("        }\n");
        output.push_str(&format!(
            "      gate {:?} {{ field \"status\" {:?} \"is\" \"exited\" }}\n",
            format!("run step {} exited", step.id),
            format!("exec/{subject}")
        ));
        if !step.allow_nonzero {
            output.push_str(&format!(
                "      gate {:?} {{ field \"exit_code\" {:?} \"is\" 0 }}\n",
                format!("run step {} succeeded", step.id),
                format!("exec/{subject}")
            ));
        }
        output.push_str(&format!(
            "      gate {:?} {{ deadline {:?} }}\n",
            format!("run step {} deadline", step.id),
            format!("{}ms", eval.max_timeout.as_millis())
        ));
        output.push_str("    }\n");
    }

    if has_team_checkpoint {
        output.push_str(&team_checkpoint);
    }
    if let Some(kick) = &eval.message {
        let generated = output_cell.join(".st3-migration/wait-team-done.sh");
        write_file(&generated, WAIT_TEAM_DONE)?;
        let supervisor = eval_agent_identity(&kick.to, host);
        let workers = spec
            .agents
            .iter()
            .map(|agent| eval_agent_identity(&agent.id, host))
            .filter(|identity| identity != &supervisor)
            .collect::<Vec<_>>();
        write_team_completion_checkpoint(
            &mut output,
            &eval_agent_identity(&kick.from, host),
            &supervisor,
            &workers,
            host,
            eval.max_timeout.as_millis() as u64,
        );
    }

    output.push_str("  checkpoint \"All held-out gates pass\" {\n");
    let signal_gates = eval
        .judges
        .iter()
        .enumerate()
        .filter(|(_, judge)| judge.signal)
        .collect::<Vec<_>>();
    if !signal_gates.is_empty() {
        for (ordinal, judge) in signal_gates {
            let command = match &judge.kind {
                JudgeKind::Bash(command) => rewrite_bus_command(command),
                JudgeKind::Declarative(checks) => declarative_command(checks),
                JudgeKind::Ask { .. } => {
                    anyhow::bail!(
                        "signal ask judge {:?} needs a manual translation",
                        judge.name
                    )
                }
            };
            output.push_str(&format!(
                "        exec {:?} {{ host {host:?}; workspace \"${{ST_WORKSPACE}}\"; command {command:?}; restart \"never\"; env {{ CATALOG \"${{ST_WORKSPACE}}\"; ST_ROOT \"${{ST_WORKSPACE}}/.st3-messages\"; ST3_MESSAGE_ROOT \"${{ST_WORKSPACE}}/.st3-messages\" }} }}\n",
                format!("eval/{name}/signal/{ordinal}")
            ));
        }
    }
    let mut gating_gates = 0usize;
    for judge in eval.judges.iter().filter(|judge| !judge.signal) {
        gating_gates += 1;
        match &judge.kind {
            JudgeKind::Bash(command) => write_mechanical_gate(
                &mut output,
                &judge.name,
                &rewrite_bus_command(command),
                host,
                judge.timeout.unwrap_or(eval.max_timeout).as_millis() as u64,
            ),
            JudgeKind::Declarative(checks) => {
                let command = declarative_command(checks);
                write_mechanical_gate(
                    &mut output,
                    &judge.name,
                    &command,
                    host,
                    judge.timeout.unwrap_or(eval.max_timeout).as_millis() as u64,
                );
            }
            JudgeKind::Ask { agent, prompt } => {
                let model = eval
                    .agents
                    .iter()
                    .find(|candidate| &candidate.id == agent)
                    .and_then(infer_agent_model)
                    .unwrap_or_else(|| "gpt-5.6-sol".into());
                output.push_str(&format!("      gate {:?} type=\"llm\" {{\n", judge.name));
                output.push_str(&format!(
                    "        model {model:?}\n        host {host:?}\n        workspace \"${{ST_WORKSPACE}}\"\n        tools \"shell\" \"git\"\n        env {{ CATALOG \"${{ST_WORKSPACE}}\"; ST_ROOT \"${{ST_WORKSPACE}}/.st3-messages\"; ST3_MESSAGE_ROOT \"${{ST_WORKSPACE}}/.st3-messages\" }}\n        token-budget 8192\n        time-limit {:?}\n        prompt {:?}\n",
                    format!("{}ms", judge.timeout.unwrap_or(eval.max_timeout).as_millis()),
                    prompt
                ));
                output.push_str("      }\n");
            }
        }
    }
    if gating_gates == 0 {
        write_mechanical_gate(
            &mut output,
            "The non-gating signals were recorded",
            "true",
            host,
            1_000,
        );
    }
    output.push_str(&format!(
        "      gate \"held-out gate deadline\" {{ deadline {:?} }}\n",
        format!("{}ms", eval.max_timeout.as_millis())
    ));
    output.push_str("    }\n");
    output.push_str("}\n");
    let mut formatted: KdlDocument = output
        .parse()
        .with_context(|| format!("parse migrated mission KDL:\n{output}"))?;
    formatted.autoformat();
    Ok((formatted.to_string(), documents))
}

fn checkpoint_intent_to_mission(source: &str, name: &str) -> Result<String> {
    let document: KdlDocument = source
        .parse()
        .with_context(|| format!("parse legacy checkpoint KDL after harness rewrite:\n{source}"))?;
    let checkpoints = document
        .nodes()
        .iter()
        .find(|node| node.name().value() == "checkpoints")
        .context("translated eval has no checkpoint sequence")?;
    let stages = checkpoints
        .children()
        .context("translated checkpoint sequence is empty")?;
    let mut output = format!(
        "version 2\nmission {:?} state=\"ready\" {{\n  completion {{ when \"all-steps-exhausted\" }}\n  goal {:?}\n",
        format!("eval/{name}"),
        format!("Complete the migrated {name} eval.")
    );
    let mut prior = None::<String>;
    for (ordinal, checkpoint) in stages.nodes().iter().enumerate() {
        let title = checkpoint
            .entries()
            .iter()
            .find(|entry| entry.name().is_none())
            .and_then(|entry| entry.value().as_string())
            .context("translated checkpoint has no title")?;
        let id = format!("{:02}-{}", ordinal, slug(title));
        let mut timeout = None::<String>;
        let mut body_nodes = Vec::new();
        if let Some(body) = checkpoint.children() {
            for child in body.nodes() {
                let child = child.clone();
                if child.name().value() == "gate"
                    && let Some(gate) = child.children()
                    && gate.nodes().len() == 1
                    && gate.nodes()[0].name().value() == "deadline"
                {
                    timeout = gate.nodes()[0]
                        .entries()
                        .iter()
                        .find(|entry| entry.name().is_none())
                        .and_then(|entry| entry.value().as_string())
                        .map(str::to_owned);
                    continue;
                }
                body_nodes.push(child);
            }
        }
        output.push_str(&format!("      step {id:?}"));
        if let Some(timeout) = timeout {
            output.push_str(&format!(" timeout={timeout:?}"));
        }
        output.push_str(" {\n");
        output.push_str(&format!("        title {title:?}\n"));
        if let Some(prior) = &prior {
            output.push_str(&format!(
                "        depends-on {{ step {prior:?} {} }}\n",
                "completed"
            ));
        }
        for child in body_nodes {
            output.push_str(&child.to_string());
            output.push('\n');
        }
        output.push_str("      }\n");
        prior = Some(id);
    }
    output.push_str("}\n");
    let mut formatted: KdlDocument = output
        .parse()
        .with_context(|| format!("parse checkpoint conversion KDL:\n{output}"))?;
    formatted.autoformat();
    Ok(formatted.to_string())
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if dash && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
    }
    output.trim_matches('-').chars().take(80).collect()
}

fn write_eval_agent(
    output: &mut String,
    agent: &st2::eval_spec::SpecAgent,
    restart: &str,
    host: &str,
) {
    output.push_str(&format!(
        "    agent {:?} {{\n",
        eval_agent_identity(&agent.id, host)
    ));
    if let Some(workspace) = &agent.workspace {
        output.push_str(&format!(
            "      workspace {:?}\n",
            format!("${{ST_WORKSPACE}}/{workspace}")
        ));
    } else {
        output.push_str("      workspace \"${ST_WORKSPACE}\"\n");
    }
    if let Some(command) = &agent.command {
        output.push_str(&format!(
            "      command {:?}\n",
            rewrite_bus_command(command)
        ));
    }
    if let Some(driver) = &agent.driver {
        write_eval_driver(output, driver);
    }
    output.push_str(&format!("      restart {restart:?}\n"));
    output.push_str("      env {\n");
    for (key, value) in &agent.env {
        if key != "ST_ROOT"
            && key != "CATALOG"
            && key != "ST3_MESSAGE_ROOT"
            && !is_reserved_context_variable(key)
        {
            output.push_str(&format!("        {key} {:?}\n", rewrite_bus_command(value)));
        }
    }
    output.push_str("        CATALOG \"${ST_WORKSPACE}\"\n");
    output.push_str("        ST_ROOT \"${ST_WORKSPACE}/.st3-messages\"\n");
    output.push_str("        ST3_MESSAGE_ROOT \"${ST_WORKSPACE}/.st3-messages\"\n");
    output.push_str("      }\n");
    for exec in &agent.execs {
        if exec.derived {
            continue;
        }
        let leaf = exec.id.rsplit('.').next().unwrap_or(&exec.id);
        output.push_str(&format!(
            "      exec {leaf:?} {{ command {:?} }}\n",
            rewrite_bus_command(&exec.command)
        ));
    }
    output.push_str("    }\n");
}

fn write_eval_driver(output: &mut String, driver: &Driver) {
    let (name, model, effort, prompt, args, dev_channels) = match driver {
        Driver::Claude(driver) => (
            "claude",
            driver.model.as_deref(),
            driver.effort.as_deref(),
            driver.prompt.as_str(),
            driver.args.as_slice(),
            Some(driver.dev_channels),
        ),
        Driver::Codex(driver) => (
            "codex",
            driver.model.as_deref(),
            driver.effort.as_deref(),
            driver.prompt.as_str(),
            driver.args.as_slice(),
            None,
        ),
        Driver::Pi(_) | Driver::OpenCode(_) | Driver::Omp(_) => {
            unreachable!("the compact eval grammar accepts only Claude and Codex drivers")
        }
    };
    output.push_str(&format!("      harness {name:?} {{\n"));
    if let Some(model) = model {
        output.push_str(&format!("        model {model:?}\n"));
    }
    if let Some(effort) = effort {
        output.push_str(&format!("        effort {effort:?}\n"));
    }
    if dev_channels == Some(true) {
        output.push_str("        dev-channels #true\n");
    }
    output.push_str(&format!(
        "        prompt {:?}\n",
        rewrite_eval_prompt(prompt)
    ));
    if !args.is_empty() {
        output.push_str("        args");
        for arg in args {
            output.push_str(&format!(" {arg:?}"));
        }
        output.push('\n');
    }
    output.push_str("      }\n");
}

fn eval_agent_identity(identity: &str, host: &str) -> String {
    if identity.contains('.') || identity.contains('/') || identity == "requester" {
        identity.into()
    } else {
        format!("{host}.{identity}")
    }
}

fn write_team_completion_checkpoint(
    output: &mut String,
    requester: &str,
    supervisor: &str,
    workers: &[String],
    host: &str,
    timeout_ms: u64,
) {
    let mut command = format!(
        "TIMEOUT_SECONDS={} bash ./.st3-migration/wait-team-done.sh {} {} kickoff/${{ST_MISSION_RUN}}",
        timeout_ms.div_ceil(1_000),
        shell(requester),
        shell(supervisor)
    );
    for worker in workers {
        command.push(' ');
        command.push_str(&shell(worker));
    }
    output.push_str("    checkpoint \"The team reported completion\" {\n");
    let gate_name = if workers.is_empty() {
        "The supervisor confirmed after kickoff"
    } else {
        "Every worker reported before the supervisor confirmed"
    };
    write_raw_mechanical_gate(
        output,
        gate_name,
        &command,
        host,
        timeout_ms.saturating_add(5_000),
    );
    output.push_str("    }\n");
}

fn write_mechanical_gate(
    output: &mut String,
    name: &str,
    command: &str,
    host: &str,
    timeout_ms: u64,
) {
    let command = format!(
        "st3 conversations export \"${{ST_WORKSPACE}}/.st3-messages\" >/dev/null && {command}"
    );
    write_raw_mechanical_gate(output, name, &command, host, timeout_ms);
}

fn write_raw_mechanical_gate(
    output: &mut String,
    name: &str,
    command: &str,
    host: &str,
    timeout_ms: u64,
) {
    output.push_str(&format!("      gate {name:?} {{\n"));
    output.push_str(&format!(
        "        exec {command:?}\n        host {host:?}\n        workspace \"${{ST_WORKSPACE}}\"\n        env {{ CATALOG \"${{ST_WORKSPACE}}\"; ST_ROOT \"${{ST_WORKSPACE}}/.st3-messages\"; ST3_MESSAGE_ROOT \"${{ST_WORKSPACE}}/.st3-messages\" }}\n        time-limit {:?}\n",
        format!("{timeout_ms}ms")
    ));
    output.push_str("      }\n");
}

fn declarative_command(checks: &[Check]) -> String {
    let mut commands = vec!["set -eu".to_owned()];
    for check in checks {
        match check {
            Check::FileHas { path, text } => {
                commands.push(format!(
                    "grep -F -- {} {} >/dev/null",
                    shell(text),
                    shell(path)
                ));
            }
            Check::FileLacks { path, text } => {
                commands.push(format!(
                    "! grep -F -- {} {} >/dev/null",
                    shell(text),
                    shell(path)
                ));
            }
            Check::JsonField { path, field, value } => {
                let value = match value {
                    JsonScalar::String(value) => serde_json::to_string(value).unwrap(),
                    JsonScalar::Bool(value) => value.to_string(),
                    JsonScalar::Integer(value) => value.to_string(),
                };
                commands.push(format!(
                    "test \"$(jq -c {} {})\" = {}",
                    shell(&format!(".{field}")),
                    shell(path),
                    shell(&value)
                ));
            }
            Check::Committed { path } => commands.push(format!(
                "git -C {} diff --quiet --exit-code && git -C {} diff --cached --quiet --exit-code",
                shell(path),
                shell(path)
            )),
        }
    }
    commands.join("; ")
}

fn infer_model(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--model")
        .map(|pair| pair[1].trim_matches(['\'', '"']).to_owned())
}

fn infer_agent_model(agent: &st2::eval_spec::SpecAgent) -> Option<String> {
    match agent.driver.as_ref() {
        Some(Driver::Claude(driver)) => driver.model.clone(),
        Some(Driver::Codex(driver)) => driver.model.clone(),
        Some(Driver::Pi(_)) | Some(Driver::OpenCode(_)) | Some(Driver::Omp(_)) => None,
        None => agent.command.as_deref().and_then(infer_model),
    }
}

fn rewrite_bus_command(value: &str) -> String {
    value
        .replace("st2 message", "st3 conversations")
        .replace("st2 channel message", "st3 graph message notification")
        .replace("st2 bus", "st3 graph message API")
        .replace("hermetic st2 eval", "hermetic st3 eval")
}

fn rewrite_eval_prompt(value: &str) -> String {
    let mut output = rewrite_bus_command(value).replace(
        "wait for an st3 graph message notification",
        "end the turn and stay idle",
    );
    if !output.ends_with(char::is_whitespace) {
        output.push(' ');
    }
    output.push_str(
        "After an empty inbox drain, end the turn and stay idle. Do not run a blocking wait, trace, poll, or sleep command. The native driver will start a new turn when a message arrives.",
    );
    output
}

fn clear_generated_assets(output_root: &Path) -> Result<()> {
    for name in [".st3-documents", ".st3-migration"] {
        let generated = output_root.join(name);
        match fs::remove_dir_all(&generated) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove stale output {}", generated.display()));
            }
        }
    }
    Ok(())
}

fn stage_document(
    output_root: &Path,
    name: &str,
    hash: &str,
    source: &Path,
    bytes: &[u8],
) -> Result<DocumentReport> {
    std::str::from_utf8(bytes).context("st3 documents must contain UTF-8 text")?;
    let staged = output_root.join(".st3-documents").join(hash);
    write_file(&staged, bytes)?;
    Ok(DocumentReport {
        name: name.into(),
        hash: hash.into(),
        source: source.display().to_string(),
        staged: staged.display().to_string(),
        put_command: format!(
            "st3 documents put {} --as {}",
            shell(&staged.display().to_string()),
            shell(name)
        ),
    })
}

fn copy_eval_assets(
    cell: &Path,
    output: &Path,
    input_kdl: &Path,
    fixture: Option<&str>,
) -> Result<()> {
    let fixture = fixture
        .map(|fixture| {
            let mut relative = PathBuf::new();
            for component in Path::new(fixture).components() {
                match component {
                    std::path::Component::CurDir => {}
                    std::path::Component::Normal(component) => relative.push(component),
                    _ => anyhow::bail!("eval copy path must stay inside its eval directory"),
                }
            }
            anyhow::ensure!(!relative.as_os_str().is_empty(), "eval copy path is empty");
            let source = cell.join(relative);
            anyhow::ensure!(
                source.is_dir(),
                "eval copy source {} is not a directory",
                source.display()
            );
            Ok(source)
        })
        .transpose()?;
    for entry in WalkDir::new(cell).follow_links(false) {
        let entry = entry?;
        if entry.path() == cell
            || entry.path() == input_kdl
            || fixture
                .as_ref()
                .is_some_and(|fixture| entry.path().starts_with(fixture))
        {
            continue;
        }
        let relative = entry.path().strip_prefix(cell)?;
        copy_eval_asset(entry.path(), &output.join(relative))?;
    }
    if let Some(fixture) = fixture {
        for entry in WalkDir::new(&fixture).min_depth(1).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&fixture)?;
            copy_eval_asset(entry.path(), &output.join(relative))?;
        }
    }
    Ok(())
}

fn copy_eval_asset(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "eval contains a symlink"
    );
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
    } else if metadata.is_file() {
        let bytes = fs::read(source)?;
        let bytes = match std::str::from_utf8(&bytes) {
            Ok(text) => rewrite_eval_asset(text).into_bytes(),
            Err(_) => bytes,
        };
        write_file(target, &bytes)?;
    } else {
        anyhow::bail!("eval contains special file {}", source.display());
    }
    Ok(())
}

fn rewrite_eval_asset(source: &str) -> String {
    source
        .split_inclusive('\n')
        .map(|line| {
            if line.contains("--catalog") {
                line.to_owned()
            } else {
                line.replace("st2 message", "st3 conversations")
                    .replace("st2 status", "st3 status")
                    .replace("st2 agents", "st3 agents")
                    .replace("st2 bus", "st3 graph message API")
            }
        })
        .collect()
}

fn validate_tree_args(args: &TreeArgs) -> Result<()> {
    anyhow::ensure!(
        args.input.is_dir(),
        "input {} is not a directory",
        args.input.display()
    );
    anyhow::ensure!(
        args.input != args.output,
        "the output must differ from the input"
    );
    anyhow::ensure!(
        !args.output.starts_with(&args.input),
        "the output cannot be inside the input tree"
    );
    Ok(())
}

fn new_report(mode: &str, args: &TreeArgs) -> Report {
    Report {
        schema: "st3-migrate-report.v1",
        experimental: true,
        review_required: true,
        mode: mode.into(),
        input: args.input.display().to_string(),
        output: args.output.display().to_string(),
        files: Vec::new(),
        documents: Vec::new(),
        warnings: vec![EXPERIMENTAL_WARNING.into()],
    }
}

fn eval_definition_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut cells = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    cells.sort_by_key(|entry| entry.file_name());
    for cell in cells {
        if !cell.file_type()?.is_dir() {
            continue;
        }
        let mut candidates = fs::read_dir(cell.path())?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("kdl")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        candidates.sort();
        anyhow::ensure!(
            candidates.len() == 1,
            "eval {} must contain exactly one top-level KDL file",
            cell.path().display()
        );
        files.push(candidates.remove(0));
    }
    Ok(files)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn write_report(path: &Path, report: &Report) -> Result<()> {
    write_file(path, &serde_json::to_vec_pretty(report)?)
}

fn shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_completion_wait(
        supervisor_messages: &str,
        requester_messages: &str,
        workers: &[&str],
    ) -> std::process::ExitStatus {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("wait-team-done.sh");
        fs::write(&script, WAIT_TEAM_DONE).unwrap();
        let bin = temporary.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let st3 = bin.join("st3");
        fs::write(
            &st3,
            r#"#!/bin/sh
case "$3" in
  supervisor) printf '%s\n' "$SUPERVISOR_MESSAGES" ;;
  requester) printf '%s\n' "$REQUESTER_MESSAGES" ;;
  *) printf '%s\n' '[]' ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&st3, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        std::process::Command::new("bash")
            .arg(script)
            .args(["requester", "supervisor", "kickoff"])
            .args(workers)
            .env("TIMEOUT_SECONDS", "0")
            .env("SUPERVISOR_MESSAGES", supervisor_messages)
            .env("REQUESTER_MESSAGES", requester_messages)
            .env("PATH", path)
            .status()
            .unwrap()
    }

    #[test]
    fn catalog_translation_wraps_old_agents_and_adds_restart_policy() {
        let translated = transform_declaration(
            r#"version 1
agent "worker" {
  host "host-a"
  workspace "/work"
  command "true"
  lifecycle { lifetime "standing" }
  env { ST_AGENT "host-a.worker"; PATH "/bin" }
}"#,
            Some(true),
        )
        .unwrap();
        assert!(translated.starts_with("version 2\n"));
        let intent = st3::parse_intent(&translated, "local").unwrap();
        let mission = &intent.missions["catalog/host-a.worker"];
        assert_eq!(mission.state, st3::model::MissionState::Ready);
        let graph = mission.declarations_kdl.as_deref().unwrap();
        assert!(graph.contains("agent worker"));
        assert!(graph.contains("restart always"));
        assert!(graph.contains("PATH \"/bin\""));
        assert!(!translated.contains("ST_AGENT"));
        assert!(!translated.contains("lifetime"));
    }

    #[test]
    fn catalog_translation_removes_legacy_metadata_and_builds_an_explicit_ding_exec() {
        let translated = transform_declaration(
            r#"version 1
agent "worker" {
  identity "host-a.worker"
  workspace "/work"
  supervisor "host-a.root"
  role "worker"
  meta { note "legacy" }
  resource "proof" uri="file:///tmp/proof.txt" reason="Legacy proof."
  ding
  env { PATH "/opt/tools:$PATH" }
  omp { prompt "Do the work." }
}"#,
            Some(true),
        )
        .unwrap();

        assert!(!translated.contains("supervisor"));
        assert!(!translated.contains("role worker"));
        assert!(!translated.contains("meta {"));
        assert!(!translated.contains("resource proof"));
        assert!(!translated.contains("$PATH"));
        assert!(translated.contains("${PATH}"));
        assert!(translated.contains("harness omp"));
        assert!(translated.contains("exec ding"));
        assert!(translated.contains("argv st3 driver ding"));
        let intent = st3::parse_intent(&translated, "local").unwrap();
        let runtimes = st3::validate_mission_runtimes(&intent, "local").unwrap();
        assert!(runtimes.contains("agent/migration-proof/host-a.worker"));
        assert!(runtimes.contains("exec/migration-proof/host-a.worker/ding"));
    }

    #[test]
    fn path_rewrite_changes_only_the_path_variable() {
        assert_eq!(
            rewrite_path_variable("/opt/tools:$PATH:$PATHOLOGY:${PATH}"),
            "/opt/tools:${PATH}:$PATHOLOGY:${PATH}"
        );
    }

    #[test]
    fn catalog_translation_warns_with_a_document_import_command() {
        let warnings = legacy_resource_warnings(
            r#"agent "worker" {
              identity "host-a.worker"
              resource "proof" uri="file:///tmp/proof file.txt" reason="Legacy proof."
            }"#,
            Path::new("agent.kdl"),
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("st3 documents put '/tmp/proof file.txt'"));
        assert!(warnings[0].contains("'doc/migration/host-a.worker/proof'"));
    }

    #[test]
    fn file_migration_reports_legacy_resource_import_work() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("agent.kdl");
        let output = root.path().join("agent.st3.kdl");
        let report = root.path().join("report.json");
        fs::write(
            &input,
            r#"agent "worker" {
  identity "host-a.worker"
  workspace "/work"
  command "true"
  resource "proof" uri="file:///tmp/proof.txt" reason="Legacy proof."
}"#,
        )
        .unwrap();

        migrate_file(FileArgs {
            input,
            output,
            report: report.clone(),
        })
        .unwrap();

        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert!(report["experimental"].as_bool().unwrap());
        assert!(report["review_required"].as_bool().unwrap());
        assert!(
            report["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .contains("st3 documents put '/tmp/proof.txt'"))
        );
    }

    #[test]
    fn catalog_translation_rewrites_typed_drivers_to_harness_blocks() {
        let translated = transform_declaration(
            r#"
agent "worker" {
  host "host-a"
  harness "claude"
  codex {
    model "gpt-5.6-sol"
    effort "medium"
    prompt "Do the work."
  }
}
"#,
            Some(true),
        )
        .unwrap();
        let intent = st3::parse_intent(&translated, "local").unwrap();
        assert_eq!(
            intent.missions["catalog/host-a.worker"].state,
            st3::model::MissionState::Ready
        );
        assert!(translated.contains("harness codex"));
        assert!(!translated.contains("harness claude"));
    }

    #[test]
    fn retired_catalog_agents_become_retired_missions() {
        let translated = transform_declaration(
            r#"agent "worker" { host "host-a"; workspace "/work"; command "true" }"#,
            Some(false),
        )
        .unwrap();
        let intent = st3::parse_intent(&translated, "local").unwrap();
        let mission = &intent.missions["catalog/host-a.worker"];
        assert_eq!(mission.state, st3::model::MissionState::Retired);
        assert!(mission.declarations_kdl.is_none());
    }

    #[test]
    fn claude_hook_cleanup_preserves_unrelated_hooks() {
        let source = serde_json::json!({
            "hooks": {
                "Stop": [{"hooks": [
                    {"type": "command", "command": "\"$ST_HOOKS/claude-observe.sh\" Stop"},
                    {"type": "command", "command": "echo keep"}
                ]}]
            }
        })
        .to_string();
        let cleaned = clean_claude_hook_json(&source).unwrap();
        assert!(!cleaned.contains("$ST_HOOKS/claude-"));
        assert!(cleaned.contains("echo keep"));
    }

    #[test]
    fn eval_translation_qualifies_short_agent_identities() {
        assert_eq!(eval_agent_identity("worker", "node-a"), "node-a.worker");
        assert_eq!(eval_agent_identity("team.worker", "node-a"), "team.worker");
        assert_eq!(eval_agent_identity("requester", "node-a"), "requester");
    }

    #[test]
    fn team_completion_requires_every_worker_report() {
        let status = run_completion_wait(
            r#"[{"from":"agent/worker.one","created_index":10}]"#,
            r#"[{"from":"agent/supervisor","created_index":20}]"#,
            &["worker.one", "worker.two"],
        );
        assert!(!status.success());
    }

    #[test]
    fn team_completion_requires_confirmation_after_the_latest_report() {
        let reports = r#"[
            {"from":"agent/worker.one","created_index":10},
            {"from":"agent/worker.two","created_index":15}
        ]"#;
        let early = run_completion_wait(
            reports,
            r#"[{"from":"agent/supervisor","created_index":14}]"#,
            &["worker.one", "worker.two"],
        );
        assert!(!early.success());

        let final_confirmation = run_completion_wait(
            reports,
            r#"[{"from":"agent/supervisor","created_index":16}]"#,
            &["worker.one", "worker.two"],
        );
        assert!(final_confirmation.success());
    }

    #[test]
    fn eval_translation_preserves_native_driver_declarations() {
        let source = r#"
            team "mix" {
              agent "sup" {
                workspace "./sup"
                claude {
                  model "claude-sonnet-5"
                  effort "medium"
                  prompt "Coordinate the task."
                  args "--permission-mode" "bypassPermissions"
                }
              }
            }
            eval {
              message { from "requester"; to "mix.sup"; content "Do the work." }
              max-timeout "60s"
              agent "judge" {
                codex {
                  model "gpt-5.6-sol"
                  effort "medium"
                  prompt "Judge the result."
                }
              }
              judges {
                judge "review" { ask "judge" "Check the result." }
              }
            }
        "#;
        let spec = st2::eval_spec::parse_spec(source).unwrap();
        let cell = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let (translated, documents) =
            transform_eval(&spec, cell.path(), output.path(), "local").unwrap();
        let intent = st3::parse_intent(&translated, "local").unwrap();

        assert!(documents.is_empty());
        assert!(translated.starts_with("version 2\n"));
        assert!(translated.contains("harness claude {"));
        assert!(translated.contains("harness codex {"));
        assert!(!translated.contains("command \"exec claude"));
        let mission = intent.missions.values().next().unwrap();
        let team = &mission.steps["00-the-eval-team-is-running"];
        let team_graph = team.declarations_kdl.as_deref().unwrap();
        assert!(team_graph.contains("agent mix.sup"));
        assert!(team_graph.contains("agent local.judge"));
        assert!(team_graph.contains("message \"kickoff/${ST_MISSION_RUN}\""));
        assert!(team_graph.contains("to \"agent/${ST_MISSION_RUN}/mix.sup\""));
        assert!(translated.contains("model gpt-5.6-sol"));
        assert!(translated.contains("${ST_WORKSPACE}"));
        assert!(!translated.contains("${EVAL_ROOT}"));
        assert!(!translated.contains("scope "));
        assert!(team.gates.iter().any(|gate| {
            matches!(
                gate,
                st3::model::GateSpec::Exists { subject, .. }
                    if subject == "agent/${ST_MISSION_RUN}/mix.sup"
            )
        }));
        assert!(translated.contains("title \"The team reported completion\""));
        assert!(translated.contains(".st3-migration/wait-team-done.sh"));
        assert!(translated.contains("kickoff/${ST_MISSION_RUN}"));
        assert!(!translated.contains("supervisor eval-"));
        assert!(!translated.contains("terminal-control"));
        assert!(mission.completion.is_some());
        assert!(
            !translated.contains("wait-team-done.sh 'requester' 'mix.sup' kickoff 'local.judge'")
        );
        assert!(
            output
                .path()
                .join(".st3-migration/wait-team-done.sh")
                .is_file()
        );
    }

    #[test]
    fn eval_run_steps_finish_before_the_team_starts_and_gates_wait_for_completion() {
        let source = r#"
            agent "sup" { command "sleep 60" }
            eval {
              run "setup" { command "true" }
              message { from "requester"; to "sup"; content "Do the work." }
              max-timeout "60s"
              judges { judge "result" { exec "true" } }
            }
        "#;
        let spec = st2::eval_spec::parse_spec(source).unwrap();
        let cell = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let (translated, _) = transform_eval(&spec, cell.path(), output.path(), "local").unwrap();
        st3::parse_intent(&translated, "local").unwrap();

        let run = translated
            .find("title \"Run step setup finishes\"")
            .unwrap();
        let team = translated
            .find("title \"The eval team is running\"")
            .unwrap();
        let completion = translated
            .find("title \"The team reported completion\"")
            .unwrap();
        let gates = translated
            .find("title \"All held-out gates pass\"")
            .unwrap();
        assert!(run < team && team < completion && completion < gates);
    }

    #[test]
    fn teamless_eval_omits_the_empty_team_step() {
        let source = r#"
            eval {
              run "setup" { command "true" }
              max-timeout "60s"
              judges { judge "result" { exec "true" } }
            }
        "#;
        let spec = st2::eval_spec::parse_spec(source).unwrap();
        let cell = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let (translated, _) = transform_eval(&spec, cell.path(), output.path(), "local").unwrap();
        st3::parse_intent(&translated, "local").unwrap();

        assert!(!translated.contains("The eval team is running"));
        assert!(translated.contains("Run step setup finishes"));
        assert!(translated.contains("All held-out gates pass"));
    }

    #[test]
    fn eval_copy_contents_become_the_runtime_root() {
        let cell = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::create_dir_all(cell.path().join("fixture/sup")).unwrap();
        fs::create_dir_all(cell.path().join("judges")).unwrap();
        fs::write(
            cell.path().join("fixture/sup/CLAUDE.md"),
            "Use st2 message.\n",
        )
        .unwrap();
        fs::write(cell.path().join("judges/grade.sh"), "st2 status\n").unwrap();
        let input = cell.path().join("cell.kdl");
        fs::write(&input, "eval { copy \"./fixture\" }").unwrap();

        copy_eval_assets(cell.path(), output.path(), &input, Some("./fixture")).unwrap();

        assert_eq!(
            fs::read_to_string(output.path().join("sup/CLAUDE.md")).unwrap(),
            "Use st3 conversations.\n"
        );
        assert_eq!(
            fs::read_to_string(output.path().join("judges/grade.sh")).unwrap(),
            "st3 status\n"
        );
        assert!(!output.path().join("fixture").exists());
    }

    #[test]
    fn eval_prompt_rewrite_removes_st2_channel_language() {
        let translated = rewrite_eval_prompt(
            "In a hermetic st2 eval, use st2 message and wait for an st2 channel message.",
        );
        assert!(translated.starts_with(
            "In a hermetic st3 eval, use st3 conversations and end the turn and stay idle."
        ));
        assert!(translated.contains("Do not run a blocking wait, trace, poll, or sleep command."));
        assert!(!translated.contains("st2"));
    }

    #[test]
    fn migration_refresh_removes_stale_staged_documents() {
        let output = tempfile::tempdir().unwrap();
        let stage = output.path().join(".st3-documents");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("old-hash"), "old").unwrap();

        clear_generated_assets(output.path()).unwrap();
        clear_generated_assets(output.path()).unwrap();

        assert!(!stage.exists());
    }
}
