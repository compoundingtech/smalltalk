//! Declarative workspace materialization for catalog `render {}` blocks.
//!
//! The runner stays harness-agnostic: these are generic, ordered file operations. Content operations
//! gate an agent's boot; `git-exclude` is advisory and can never prevent a launch.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use kdl::{KdlDocument, KdlNode};

use agent_spec::spec::AgentSpec;

const REGULAR_FILE_MODE: u32 = 0o644;
const EXECUTABLE_FILE_MODE: u32 = 0o755;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderOp {
    Copy {
        source: String,
        destination: String,
        executable: bool,
    },
    File {
        destination: String,
        content: String,
        executable: bool,
    },
    JsonUpsert {
        destination: String,
        content: String,
        arrays: ArrayMerge,
        executable: bool,
    },
    EnsureLine {
        destination: String,
        line: String,
        executable: bool,
    },
    GitExclude {
        path: String,
    },
}

/// How a json-upsert treats an array both sides declare. `Replace` is the default and the
/// original contract; `union` appends patch elements the target lacks (exact-equality dedupe), so
/// registrations can join arrays other owners also write — user-declared entries survive and
/// re-materialization is idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArrayMerge {
    #[default]
    Replace,
    Union,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderPlan {
    pub ops: Vec<RenderOp>,
}

fn references_variable(input: &str, variable: &str) -> bool {
    // Use a marker absent from the input so this observes an actual substitution, not merely a
    // lookup callback (which the expander may consult while preserving a malformed token).
    let mut marker = "\0st2-render-variable\0".to_string();
    while input.contains(&marker) {
        marker.push('\0');
    }
    crate::expand::expand_vars(input, |name| (name == variable).then(|| marker.clone()))
        .contains(&marker)
}

impl RenderOp {
    fn references_variable(&self, variable: &str) -> bool {
        match self {
            Self::Copy {
                source,
                destination,
                ..
            } => {
                references_variable(source, variable) || references_variable(destination, variable)
            }
            Self::File {
                destination,
                content,
                ..
            }
            | Self::JsonUpsert {
                destination,
                content,
                ..
            } => {
                references_variable(destination, variable) || references_variable(content, variable)
            }
            Self::EnsureLine {
                destination, line, ..
            } => references_variable(destination, variable) || references_variable(line, variable),
            Self::GitExclude { path } => references_variable(path, variable),
        }
    }
}

impl RenderPlan {
    fn references_variable(&self, variable: &str) -> bool {
        self.ops
            .iter()
            .any(|operation| operation.references_variable(variable))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaterializeReport {
    pub materialized: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// Bus ids whose gating render failed. Callers must not boot these agents.
    pub failed_agents: HashSet<String>,
}

impl MaterializeReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

fn string_args(node: &KdlNode) -> Vec<String> {
    node.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .filter_map(|entry| entry.value().as_string().map(String::from))
        .collect()
}

fn content_arg(node: &KdlNode) -> Option<String> {
    string_args(node).get(1).cloned().or_else(|| {
        node.children()?
            .nodes()
            .iter()
            .find(|child| child.name().value() == "content")
            .and_then(|child| child.get(0))
            .and_then(|value| value.as_string())
            .map(String::from)
    })
}

fn executable_property(node: &KdlNode, agent: &str, other_properties: &[&str]) -> Result<bool> {
    let mut executable = None;
    for entry in node.entries().iter().filter(|entry| entry.name().is_some()) {
        let name = entry.name().unwrap().value();
        if other_properties.contains(&name) {
            continue;
        }
        anyhow::ensure!(
            name == "executable",
            "agent '{agent}': {} has unknown property '{name}'",
            node.name().value()
        );
        anyhow::ensure!(
            executable.is_none(),
            "agent '{agent}': {} has duplicate property 'executable'",
            node.name().value()
        );
        executable = Some(entry.value().as_bool().with_context(|| {
            format!(
                "agent '{agent}': {} property 'executable' must be a boolean",
                node.name().value()
            )
        })?);
    }
    Ok(executable.unwrap_or(false))
}

fn reject_properties(node: &KdlNode, agent: &str) -> Result<()> {
    if let Some(entry) = node.entries().iter().find(|entry| entry.name().is_some()) {
        anyhow::bail!(
            "agent '{agent}': {} has unknown property '{}'",
            node.name().value(),
            entry.name().unwrap().value()
        );
    }
    Ok(())
}

fn parse_render_node(node: &KdlNode, agent: &str) -> Result<RenderPlan> {
    let mut plan = RenderPlan::default();
    let Some(children) = node.children() else {
        return Ok(plan);
    };
    for directive in children.nodes() {
        let name = directive.name().value();
        let args = string_args(directive);
        match name {
            "copy" => {
                let executable = executable_property(directive, agent, &[])?;
                let [source, destination] = args.as_slice() else {
                    anyhow::bail!(
                        "agent '{agent}': copy expects `copy \"<source>\" \"<destination>\"`"
                    );
                };
                plan.ops.push(RenderOp::Copy {
                    source: source.clone(),
                    destination: destination.clone(),
                    executable,
                });
            }
            "file" => {
                let executable = executable_property(directive, agent, &[])?;
                let Some(destination) = args.first() else {
                    anyhow::bail!("agent '{agent}': file needs a destination");
                };
                let content = content_arg(directive)
                    .with_context(|| format!("agent '{agent}': file needs content"))?;
                plan.ops.push(RenderOp::File {
                    destination: destination.clone(),
                    content,
                    executable,
                });
            }
            "json-upsert" => {
                let executable = executable_property(directive, agent, &["arrays"])?;
                let Some(destination) = args.first() else {
                    anyhow::bail!("agent '{agent}': json-upsert needs a destination");
                };
                let content = content_arg(directive)
                    .with_context(|| format!("agent '{agent}': json-upsert needs content"))?;
                serde_json::from_str::<serde_json::Value>(&content).with_context(|| {
                    format!("agent '{agent}': json-upsert content is not valid JSON")
                })?;
                let arrays = match directive
                    .entries()
                    .iter()
                    .find(|entry| entry.name().is_some_and(|name| name.value() == "arrays"))
                    .and_then(|entry| entry.value().as_string())
                {
                    None | Some("replace") => ArrayMerge::Replace,
                    Some("union") => ArrayMerge::Union,
                    Some(other) => anyhow::bail!(
                        "agent '{agent}': json-upsert arrays=\"{other}\" (expected replace|union)"
                    ),
                };
                plan.ops.push(RenderOp::JsonUpsert {
                    destination: destination.clone(),
                    content,
                    arrays,
                    executable,
                });
            }
            "ensure-line" => {
                let executable = executable_property(directive, agent, &[])?;
                let [destination, line] = args.as_slice() else {
                    anyhow::bail!(
                        "agent '{agent}': ensure-line expects `ensure-line \"<file>\" \"<line>\"`"
                    );
                };
                plan.ops.push(RenderOp::EnsureLine {
                    destination: destination.clone(),
                    line: line.clone(),
                    executable,
                });
            }
            "git-exclude" => {
                reject_properties(directive, agent)?;
                if args.is_empty() {
                    anyhow::bail!("agent '{agent}': git-exclude needs at least one path");
                }
                plan.ops
                    .extend(args.into_iter().map(|path| RenderOp::GitExclude { path }));
            }
            other => anyhow::bail!(
                "agent '{agent}': unknown render directive '{other}' \
                 (expected copy|file|json-upsert|ensure-line|git-exclude)"
            ),
        }
    }
    Ok(plan)
}

/// Parse the `render {}` belonging to `spec`. An absent block is a valid empty plan.
pub fn parse_plan(spec: &AgentSpec) -> Result<RenderPlan> {
    if spec.path.extension().and_then(|ext| ext.to_str()) != Some("kdl") {
        return Ok(RenderPlan::default());
    }
    let text = fs::read_to_string(&spec.path)
        .with_context(|| format!("reading {}", spec.path.display()))?;
    let doc =
        KdlDocument::parse(&text).map_err(|error| anyhow::anyhow!("KDL parse error: {error}"))?;
    let agents: Vec<&KdlNode> = doc
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "agent")
        .collect();
    let node = agents
        .iter()
        .copied()
        .find(|node| {
            node.get(0)
                .and_then(|value| value.as_string())
                .is_some_and(|identity| identity == spec.identity)
        })
        .or_else(|| (agents.len() == 1).then_some(agents[0]));
    let Some(node) = node else {
        anyhow::bail!(
            "could not find agent '{}' in {}",
            spec.identity,
            spec.path.display()
        );
    };
    let render = node
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .find(|child| child.name().value() == "render");
    match render {
        Some(render) => parse_render_node(render, &spec.identity),
        None => Ok(RenderPlan::default()),
    }
}

/// Append render operations from the same pure driver expansion used by the print command.
fn parse_plan_with_driver(spec: &AgentSpec, this_host: &str) -> Result<RenderPlan> {
    let mut plan = parse_plan(spec)?;
    if spec.driver.is_none() {
        return Ok(plan);
    }
    let expansion = crate::driver::expand_driver(spec, this_host)?;
    let mut generated = RenderPlan::default();
    for render in expansion
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "render")
    {
        generated
            .ops
            .extend(parse_render_node(render, &spec.identity)?.ops);
    }
    resolve_driver_render_executable(&mut generated, &spec.identity)?;
    plan.ops.extend(generated.ops);
    Ok(plan)
}

fn resolve_driver_render_executable(plan: &mut RenderPlan, agent: &str) -> Result<()> {
    if plan.ops.is_empty() {
        return Ok(());
    }
    let executable =
        std::env::current_exe().context("resolving st2 executable for driver materialization")?;
    for operation in &mut plan.ops {
        let RenderOp::JsonUpsert {
            destination,
            content,
            ..
        } = operation
        else {
            continue;
        };
        // The hook registration ships verbatim: its `$ST_HOOKS` references are render
        // variables the materializer resolves against the verified set, not an executable
        // path this binary should rewrite.
        if destination == ".claude/settings.local.json" {
            continue;
        }
        anyhow::ensure!(
            destination == ".mcp.json",
            "agent '{agent}' driver expansion produced an unexpected JSON destination"
        );
        let mut patch: serde_json::Value = serde_json::from_str(content)?;
        let command = patch
            .pointer_mut("/mcpServers/st2/command")
            .with_context(|| format!("agent '{agent}' driver expansion has no st2 MCP command"))?;
        anyhow::ensure!(
            command.as_str() == Some("st2"),
            "agent '{agent}' driver expansion has an unexpected st2 MCP command"
        );
        *command = serde_json::Value::String(executable.to_string_lossy().into_owned());
        *content = serde_json::to_string(&patch)?;
    }
    Ok(())
}

/// Add either the typed driver render or the unchanged legacy delivery render.
fn effective_plan(root: &Path, spec: &AgentSpec, this_host: &str) -> Result<RenderPlan> {
    crate::driver::ensure_single_source(spec)?;
    let mut plan = parse_plan_with_driver(spec, this_host)?;
    if spec.driver.is_none() && spec.delivery == Some(agent_spec::spec::DeliveryTransport::Mcp) {
        let executable = std::env::current_exe()
            .context("resolving st2 executable for Claude MCP declaration")?;
        let content = serde_json::json!({
            "mcpServers": {"st2": {
                "type": "stdio",
                "command": executable.to_string_lossy(),
                "args": ["--catalog", root.display().to_string(), "claude-mcp", "--identity", spec.bus_id(this_host)]
            }}
        }).to_string();
        plan.ops.push(RenderOp::JsonUpsert {
            destination: ".mcp.json".into(),
            content,
            arrays: ArrayMerge::Replace,
            executable: false,
        });
        // The same canonical hook registration the typed driver renders: these seats run under
        // claude-session too, and a wrapper that claims and ends a record nobody transitions is
        // worse than no record at all.
        plan.ops.push(RenderOp::JsonUpsert {
            destination: ".claude/settings.local.json".into(),
            content: serde_json::to_string(&crate::hooks::claude_settings_registration())
                .context("serializing the canonical Claude hook registration")?,
            arrays: ArrayMerge::Union,
            executable: false,
        });
    }
    Ok(plan)
}

/// Catalog-owned files read by this agent's `render { copy ... }` operations.
///
/// Absolute/external sources are deliberately absent: a declaration snapshot owns catalog bytes,
/// not arbitrary workspace or host files. The returned paths are exact existing files; callers
/// still decide which file kinds are admissible for their transaction.
pub(crate) fn catalog_owned_render_inputs(
    root: &Path,
    spec: &AgentSpec,
    this_host: &str,
) -> Result<Vec<PathBuf>> {
    let plan = effective_plan(root, spec, this_host)?;
    let env = render_env(root, spec, this_host);
    let spec_dir = spec.path.parent().unwrap_or(root);
    let mut inputs = BTreeSet::new();
    for operation in plan.ops {
        let RenderOp::Copy {
            source: raw_source, ..
        } = operation
        else {
            continue;
        };
        let resolved = source(root, spec_dir, &raw_source, &env, true)?;
        if resolved.strip_prefix(root).is_ok() {
            inputs.insert(resolved);
        }
    }
    Ok(inputs.into_iter().collect())
}

fn render_env(root: &Path, spec: &AgentSpec, this_host: &str) -> BTreeMap<String, String> {
    let bus_id = spec.bus_id(this_host);
    let mut env = BTreeMap::from([
        ("CATALOG".to_string(), root.display().to_string()),
        ("ST_ROOT".to_string(), root.display().to_string()),
        (
            "PTY_ROOT".to_string(),
            crate::run::effective_pty_root(root).display().to_string(),
        ),
        ("ST_AGENT".to_string(), bus_id.clone()),
    ]);
    if let Ok(path) = crate::hooks::versioned_hooks_dir() {
        env.insert("ST_HOOKS".to_string(), path.display().to_string());
    }
    if let Some(task) = spec.tasks.iter().find(|task| task.name == "agent") {
        for (key, value) in &task.env {
            let expanded = crate::expand::expand_vars(value, |name| {
                env.get(name).cloned().or_else(|| std::env::var(name).ok())
            });
            env.insert(key.clone(), expanded);
        }
    }
    env.insert("ST_AGENT".to_string(), bus_id);
    env
}

fn expand(input: &str, env: &BTreeMap<String, String>) -> String {
    crate::expand::expand_vars(input, |name| {
        env.get(name).cloned().or_else(|| std::env::var(name).ok())
    })
}

fn destination(workspace: &Path, raw: &str, env: &BTreeMap<String, String>) -> Result<PathBuf> {
    let raw = expand(raw, env);
    let path = Path::new(&raw);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("render destination '{raw}' must be a workspace-relative path without `..`");
    }
    Ok(workspace.join(path))
}

fn source(
    root: &Path,
    spec_dir: &Path,
    raw: &str,
    env: &BTreeMap<String, String>,
    require_presence: bool,
) -> Result<PathBuf> {
    let raw = expand(raw, env);
    let path = Path::new(&raw);
    if path.is_absolute() {
        if require_presence && path.starts_with(root) && !path.exists() {
            anyhow::bail!("copy source '{raw}' does not exist");
        }
        return Ok(path.to_path_buf());
    }
    // Canonical format uses catalog-rooted `_templates/...`; the already-blessed COS prototype uses
    // paths relative to the agent file (`../../_templates/...`). Supporting both keeps that catalog
    // portable without making the operation harness-aware.
    let catalog_relative = root.join(path);
    if catalog_relative.exists() {
        return Ok(catalog_relative);
    }
    let spec_relative = spec_dir.join(path);
    if spec_relative.exists() {
        return Ok(spec_relative);
    }
    if !require_presence {
        return Ok(catalog_relative);
    }
    anyhow::bail!(
        "copy source '{raw}' does not exist (tried {} and {})",
        catalog_relative.display(),
        spec_relative.display()
    )
}

fn write_owned(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn desired_mode(executable: bool) -> u32 {
    if executable {
        EXECUTABLE_FILE_MODE
    } else {
        REGULAR_FILE_MODE
    }
}

fn read_optional_mode(path: &Path) -> Result<Option<u32>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions().mode() & 0o7777)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading mode for {}", path.display())),
    }
}

fn set_owned_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode {mode:04o} on {}", path.display()))
}

fn write_owned_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    write_owned(path, bytes)?;
    set_owned_mode(path, mode)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn ensure_line_bytes(current: Option<&[u8]>, line: &str, path: &Path) -> Result<Vec<u8>> {
    let mut contents = match current {
        Some(bytes) => String::from_utf8(bytes.to_vec())
            .with_context(|| format!("existing {} is not UTF-8", path.display()))?,
        None => String::new(),
    };
    if contents.lines().any(|existing| existing == line) {
        return Ok(contents.into_bytes());
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(line);
    contents.push('\n');
    Ok(contents.into_bytes())
}

fn ensure_line(path: &Path, line: &str) -> Result<bool> {
    let current = read_optional(path)?;
    let contents = ensure_line_bytes(current.as_deref(), line, path)?;
    if current.as_deref() == Some(contents.as_slice()) {
        return Ok(false);
    }
    write_owned(path, &contents)?;
    Ok(true)
}

fn deep_merge(
    target: &mut serde_json::Value,
    patch: serde_json::Value,
    arrays: ArrayMerge,
    supersede_managed_hooks: bool,
) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value, arrays, supersede_managed_hooks),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (serde_json::Value::Array(target), serde_json::Value::Array(patch))
            if arrays == ArrayMerge::Union =>
        {
            // Exact-equality union alone would accumulate st2's own entries across hook-set
            // upgrades: `$ST_HOOKS` expands content-addressed, so every upgrade renders each
            // entry with a new path and the old one would be retained beside it. Supersession
            // DESCENDS: only the managed nested entries (structurally st2's — a managed hook
            // file under any set-shaped path, or `$ST_HOOKS` at a token boundary) the patch no
            // longer states are removed, so a matcher group holding a user hook beside an st2
            // one keeps the user's; containers left with nothing but empty arrays are dropped.
            target.retain_mut(|element| {
                if patch.contains(element)
                    || !supersede_managed_hooks
                    || !contains_owned_string(element)
                {
                    return true;
                }
                keep_after_supersession(element)
            });
            for element in patch {
                if !target.contains(&element) {
                    target.push(element);
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

/// Prune superseded managed entries INSIDE `element`, returning whether the element itself is
/// still worth keeping. A managed leaf (no nested arrays) never survives here: it belongs only
/// inside its canonical group, which the caller already retained by whole-element equality — a
/// managed entry nested in a USER's group would otherwise register twice. Containers are pruned
/// recursively and dropped once every nested array is empty — husks that only ever held
/// superseded registrations.
fn keep_after_supersession(element: &mut serde_json::Value) -> bool {
    if !has_nested_array(element) {
        return !contains_owned_string(element);
    }
    let mut any_array_content = false;
    match element {
        serde_json::Value::Array(items) => {
            items.retain_mut(keep_after_supersession);
            any_array_content = !items.is_empty();
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                if let serde_json::Value::Array(items) = value {
                    items.retain_mut(keep_after_supersession);
                    if !items.is_empty() {
                        any_array_content = true;
                    }
                } else if has_nested_array(value) && keep_after_supersession(value) {
                    any_array_content = true;
                }
            }
        }
        _ => {}
    }
    any_array_content
}

fn has_nested_array(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(_) => true,
        serde_json::Value::Object(map) => map.values().any(has_nested_array),
        _ => false,
    }
}

/// Whether any string inside `value` marks it as an st2-rendered element, structurally (see
/// [`crate::hooks::is_managed_hook_reference`]).
fn contains_owned_string(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => crate::hooks::is_managed_hook_reference(text),
        serde_json::Value::Array(items) => items.iter().any(contains_owned_string),
        serde_json::Value::Object(map) => map.values().any(contains_owned_string),
        _ => false,
    }
}

fn has_git_marker(workspace: &Path) -> bool {
    workspace
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

fn git_exclude(workspace: &Path, line: &str) -> Result<bool> {
    // A failed `git rev-parse` costs a process spawn per op per pass. On catalogs whose
    // workspaces are plain directories this repeats every reconcile (measured live: ~28 spawns
    // plus stderr captures per pass on dev3). The marker probe answers "not a repo" for free;
    // `.git` may be a directory or a file (linked worktrees), which `exists` covers either way.
    if !has_git_marker(workspace) {
        anyhow::bail!(
            "{} is not a Git worktree (no .git marker)",
            workspace.display()
        );
    }
    let output = Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .context("running git rev-parse")?;
    if !output.status.success() {
        anyhow::bail!(
            "{} is not a Git worktree: {}",
            workspace.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8(output.stdout).context("git returned a non-UTF-8 exclude path")?;
    let raw = raw.trim();
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace.join(raw)
    };
    ensure_line(&path, line)
}

fn is_git_tracked(workspace: &Path, relative: &Path) -> Result<bool> {
    let probe = Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("running git rev-parse for materialization safety")?;
    if !probe.status.success() {
        let has_git_marker = workspace
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists());
        if has_git_marker {
            anyhow::bail!(
                "could not determine Git worktree state for {}: {}",
                workspace.display(),
                String::from_utf8_lossy(&probe.stderr).trim()
            );
        }
        return Ok(false);
    }

    let output = Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .output()
        .context("running git ls-files for materialization safety")?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    anyhow::bail!(
        "could not determine whether {} is tracked: {}",
        relative.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

enum PreparedOp {
    Write {
        destination: PathBuf,
        bytes: Vec<u8>,
        mode: u32,
        note: String,
    },
    SetMode {
        destination: PathBuf,
        mode: u32,
        note: String,
    },
    GitExclude {
        path: String,
        expanded: String,
    },
}

#[derive(Default)]
struct Preparation {
    virtual_files: BTreeMap<PathBuf, Vec<u8>>,
    virtual_modes: BTreeMap<PathBuf, u32>,
    changed_targets: BTreeMap<PathBuf, String>,
    ops: Vec<PreparedOp>,
}

impl Preparation {
    fn current_bytes(&self, destination: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self
            .virtual_files
            .get(destination)
            .cloned()
            .map(Some)
            .unwrap_or(read_optional(destination)?))
    }

    fn push_content(
        &mut self,
        destination: PathBuf,
        raw_destination: String,
        bytes: Vec<u8>,
        executable: bool,
        note: String,
        current: Option<Vec<u8>>,
    ) -> Result<()> {
        let current_mode = match self.virtual_modes.get(&destination) {
            Some(mode) => Some(*mode),
            None => read_optional_mode(&destination)?,
        };
        let mode = desired_mode(executable);
        if current.as_deref() == Some(bytes.as_slice()) {
            if current_mode != Some(mode) {
                self.changed_targets
                    .insert(destination.clone(), raw_destination);
                self.ops.push(PreparedOp::SetMode {
                    destination: destination.clone(),
                    mode,
                    note,
                });
            }
        } else {
            self.changed_targets
                .insert(destination.clone(), raw_destination);
            self.ops.push(PreparedOp::Write {
                destination: destination.clone(),
                bytes: bytes.clone(),
                mode,
                note,
            });
        }
        self.virtual_files.insert(destination.clone(), bytes);
        self.virtual_modes.insert(destination, mode);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum RenderContentClaim {
    Replace(Vec<u8>),
    JsonUpsert(serde_json::Value, ArrayMerge),
    EnsureLine(String),
}

#[derive(Debug, Clone, PartialEq)]
struct RenderClaim {
    content: RenderContentClaim,
    executable: bool,
}

#[derive(Debug)]
pub struct RenderOwnershipConflict {
    pub destination: PathBuf,
    pub owners: BTreeSet<String>,
}

impl RenderOwnershipConflict {
    fn error(&self) -> String {
        format!(
            "conflicting render ownership for '{}': active agents {} declare incompatible content or mode for one shared workspace target",
            self.destination.display(),
            self.owners.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

fn claims_for_agent(
    root: &Path,
    spec: &AgentSpec,
    this_host: &str,
) -> Result<BTreeMap<PathBuf, Vec<RenderClaim>>> {
    let plan = effective_plan(root, spec, this_host)?;
    if plan.ops.is_empty() {
        return Ok(BTreeMap::new());
    }
    let workspace_raw = spec
        .workspace
        .as_deref()
        .with_context(|| format!("agent '{}' has render{{}} but no workspace", spec.identity))?;
    let env = render_env(root, spec, this_host);
    let workspace = PathBuf::from(expand(workspace_raw, &env));
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace {}", workspace.display()))?;
    let spec_dir = spec.path.parent().unwrap_or(root);
    let mut claims = BTreeMap::<PathBuf, Vec<RenderClaim>>::new();
    for op in plan.ops {
        let (destination, claim) = match op {
            RenderOp::Copy {
                source: raw_source,
                destination: raw_destination,
                executable,
            } => {
                let source = source(root, spec_dir, &raw_source, &env, true)?;
                let bytes = fs::read(&source)
                    .with_context(|| format!("reading copy source {}", source.display()))?;
                (
                    destination(&workspace, &raw_destination, &env)?,
                    RenderClaim {
                        content: RenderContentClaim::Replace(bytes),
                        executable,
                    },
                )
            }
            RenderOp::File {
                destination: raw_destination,
                content,
                executable,
            } => (
                destination(&workspace, &raw_destination, &env)?,
                RenderClaim {
                    content: RenderContentClaim::Replace(expand(&content, &env).into_bytes()),
                    executable,
                },
            ),
            RenderOp::JsonUpsert {
                destination: raw_destination,
                content,
                arrays,
                executable,
            } => {
                let patch = serde_json::from_str(&expand(&content, &env)).with_context(|| {
                    format!(
                        "expanded json-upsert for '{}' is not valid JSON",
                        spec.identity
                    )
                })?;
                (
                    destination(&workspace, &raw_destination, &env)?,
                    RenderClaim {
                        content: RenderContentClaim::JsonUpsert(patch, arrays),
                        executable,
                    },
                )
            }
            RenderOp::EnsureLine {
                destination: raw_destination,
                line,
                executable,
            } => (
                destination(&workspace, &raw_destination, &env)?,
                RenderClaim {
                    content: RenderContentClaim::EnsureLine(expand(&line, &env)),
                    executable,
                },
            ),
            // Every git-exclude is additive by contract and resolves through Git's own shared
            // metadata path rather than a declared workspace-relative render destination.
            RenderOp::GitExclude { .. } => continue,
        };
        claims.entry(destination).or_default().push(claim);
    }
    Ok(claims)
}

/// Find active local agents that declare incompatible desired content for one resolved workspace
/// destination. Equivalent idempotent plans may share a target; differing plans have no implicit
/// last-writer-wins semantics.
pub fn render_ownership_conflicts(
    root: &Path,
    specs: &[AgentSpec],
    this_host: &str,
) -> Vec<RenderOwnershipConflict> {
    let mut by_destination = BTreeMap::<PathBuf, BTreeMap<String, Vec<RenderClaim>>>::new();
    for spec in specs {
        if !spec.desired_state.is_running() || spec.resolved_host(this_host) != this_host {
            continue;
        }
        let Ok(claims) = claims_for_agent(root, spec, this_host) else {
            // The normal per-agent materialization path reports malformed plans and unavailable
            // inputs. Ownership analysis only compares claims it can resolve without writing.
            continue;
        };
        let owner = spec.bus_id(this_host);
        for (destination, plan) in claims {
            by_destination
                .entry(destination)
                .or_default()
                .insert(owner.clone(), plan);
        }
    }

    by_destination
        .into_iter()
        .filter_map(|(destination, claims)| {
            let mut plans = claims.values();
            let first = plans.next()?;
            plans
                .any(|plan| plan != first)
                .then(|| RenderOwnershipConflict {
                    destination,
                    owners: claims.into_keys().collect(),
                })
        })
        .collect()
}

/// Execute one agent's render plan in declaration order.
pub fn materialize_agent(root: &Path, spec: &AgentSpec, this_host: &str) -> Result<Vec<String>> {
    crate::reconcile::validate_task_identities(std::slice::from_ref(spec), this_host)?;
    let plan = effective_plan(root, spec, this_host)?;
    if plan.ops.is_empty() {
        return Ok(Vec::new());
    }
    // Creating the seat is the moment the operator can still act on it. Expansion itself stays
    // pure and silent so `st2 driver expand`, reconcile, and ownership checks read the same.
    for advisory in crate::driver::claude_channel_advisories(spec) {
        tracing::warn!("st2 materialize: agent '{}': {advisory}", spec.identity);
    }
    if plan.references_variable("ST_HOOKS") {
        crate::hooks::verify_required_set().with_context(|| {
            format!(
                "agent '{}' render plan references $ST_HOOKS, but this binary's lifecycle hook set is not verified",
                spec.identity
            )
        })?;
    }
    let workspace_raw = spec
        .workspace
        .as_deref()
        .with_context(|| format!("agent '{}' has render{{}} but no workspace", spec.identity))?;
    let env = render_env(root, spec, this_host);
    let workspace = PathBuf::from(expand(workspace_raw, &env));
    if !workspace.is_dir() {
        anyhow::bail!(
            "agent '{}' workspace {} does not exist or is not a directory",
            spec.identity,
            workspace.display()
        );
    }
    let spec_dir = spec.path.parent().unwrap_or(root);
    let mut preparation = Preparation::default();
    for op in plan.ops {
        match op {
            RenderOp::Copy {
                source: raw_source,
                destination: raw_destination,
                executable,
            } => {
                let source = source(root, spec_dir, &raw_source, &env, true)?;
                let destination = destination(&workspace, &raw_destination, &env)?;
                let bytes = fs::read(&source)
                    .with_context(|| format!("reading copy source {}", source.display()))?;
                let current = preparation.current_bytes(&destination)?;
                let note = format!("{}: copied {}", spec.identity, raw_destination);
                preparation.push_content(
                    destination,
                    raw_destination,
                    bytes,
                    executable,
                    note,
                    current,
                )?;
            }
            RenderOp::File {
                destination: raw_destination,
                content,
                executable,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let bytes = expand(&content, &env).into_bytes();
                let current = preparation.current_bytes(&destination)?;
                let note = format!("{}: wrote {}", spec.identity, raw_destination);
                preparation.push_content(
                    destination,
                    raw_destination,
                    bytes,
                    executable,
                    note,
                    current,
                )?;
            }
            RenderOp::JsonUpsert {
                destination: raw_destination,
                content,
                arrays,
                executable,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let current = preparation.current_bytes(&destination)?;
                let mut target = match current.as_deref() {
                    Some(existing) => serde_json::from_slice(existing).with_context(|| {
                        format!("existing {} is not valid JSON", destination.display())
                    })?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let patch: serde_json::Value = serde_json::from_str(&expand(&content, &env))
                    .with_context(|| {
                        format!(
                            "expanded json-upsert for '{}' is not valid JSON",
                            spec.identity
                        )
                    })?;
                // st2-hook supersession is the CANONICAL Claude registration's maintenance
                // rule, not a property of union merges: an unrelated union merge must never
                // delete user array elements that merely match the managed-path heuristic.
                let supersede_managed_hooks = raw_destination == ".claude/settings.local.json";
                deep_merge(&mut target, patch, arrays, supersede_managed_hooks);
                let mut bytes = serde_json::to_vec_pretty(&target)?;
                bytes.push(b'\n');
                let note = format!("{}: upserted {}", spec.identity, raw_destination);
                preparation.push_content(
                    destination,
                    raw_destination,
                    bytes,
                    executable,
                    note,
                    current,
                )?;
            }
            RenderOp::EnsureLine {
                destination: raw_destination,
                line,
                executable,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let current = preparation.current_bytes(&destination)?;
                let bytes =
                    ensure_line_bytes(current.as_deref(), &expand(&line, &env), &destination)?;
                let note = format!("{}: ensured {}", spec.identity, raw_destination);
                preparation.push_content(
                    destination,
                    raw_destination,
                    bytes,
                    executable,
                    note,
                    current,
                )?;
            }
            RenderOp::GitExclude { path } => {
                preparation.ops.push(PreparedOp::GitExclude {
                    expanded: expand(&path, &env),
                    path,
                });
            }
        }
    }

    for (target, raw_destination) in &preparation.changed_targets {
        let relative = target.strip_prefix(&workspace).with_context(|| {
            format!(
                "render destination {} escaped workspace {}",
                target.display(),
                workspace.display()
            )
        })?;
        if is_git_tracked(&workspace, relative)? {
            anyhow::bail!(
                "agent '{}': generated materialization would change Git-tracked target '{}' ({}); \
                 keep the tracked file byte- and mode-identical, choose an untracked overlay target, \
                 or edit the tracked file intentionally outside st2",
                spec.identity,
                raw_destination,
                target.display()
            );
        }
    }

    let mut notes = Vec::new();
    for op in preparation.ops {
        match op {
            PreparedOp::Write {
                destination,
                bytes,
                mode,
                note,
            } => {
                write_owned_with_mode(&destination, &bytes, mode)?;
                notes.push(note);
            }
            PreparedOp::SetMode {
                destination,
                mode,
                note,
            } => {
                set_owned_mode(&destination, mode)?;
                notes.push(note);
            }
            PreparedOp::GitExclude { path, expanded } => {
                // Advisory by contract: report a real change, but never fail materialization/boot.
                match git_exclude(&workspace, &expanded) {
                    Ok(true) => notes.push(format!("{}: excluded {path}", spec.identity)),
                    Ok(false) => {}
                    Err(error) => notes.push(format!(
                        "WARN {}: could not git-exclude '{path}': {error}",
                        spec.identity
                    )),
                }
            }
        }
    }
    Ok(notes)
}

/// Validate an agent's complete running render readiness without writing its workspace.
pub fn validate_agent(root: &Path, spec: &AgentSpec, this_host: &str) -> Result<()> {
    validate_agent_with_source_presence(root, spec, this_host, true)
}

/// Validate lifecycle-independent render declaration structure without consulting source presence.
pub(crate) fn validate_agent_structure(
    root: &Path,
    spec: &AgentSpec,
    this_host: &str,
) -> Result<()> {
    validate_agent_with_source_presence(root, spec, this_host, false)
}

fn validate_agent_with_source_presence(
    root: &Path,
    spec: &AgentSpec,
    this_host: &str,
    require_source_presence: bool,
) -> Result<()> {
    crate::reconcile::validate_task_identities(std::slice::from_ref(spec), this_host)?;
    let plan = parse_plan_with_driver(spec, this_host)?;
    if plan.ops.is_empty() {
        return Ok(());
    }
    let workspace_raw = spec
        .workspace
        .as_deref()
        .with_context(|| format!("agent '{}' has render{{}} but no workspace", spec.identity))?;
    let env = render_env(root, spec, this_host);
    let workspace = PathBuf::from(expand(workspace_raw, &env));
    let spec_dir = spec.path.parent().unwrap_or(root);
    for op in plan.ops {
        match op {
            RenderOp::Copy {
                source: raw_source,
                destination: raw_destination,
                ..
            } => {
                source(root, spec_dir, &raw_source, &env, require_source_presence)?;
                destination(&workspace, &raw_destination, &env)?;
            }
            RenderOp::File {
                destination: raw_destination,
                ..
            }
            | RenderOp::JsonUpsert {
                destination: raw_destination,
                ..
            }
            | RenderOp::EnsureLine {
                destination: raw_destination,
                ..
            } => {
                destination(&workspace, &raw_destination, &env)?;
            }
            RenderOp::GitExclude { .. } => {}
        }
    }
    Ok(())
}

/// Materialize every active agent assigned to `this_host`.
pub fn materialize_catalog(root: &Path, specs: &[AgentSpec], this_host: &str) -> MaterializeReport {
    materialize_catalog_against(root, specs, specs, this_host)
}

/// Materialize `selected_specs` after checking their workspace target ownership against the complete
/// active fleet. This preserves shortest-path selection while preventing a selected owner from
/// bypassing a collision declared by an unselected sibling.
pub fn materialize_catalog_against(
    root: &Path,
    selected_specs: &[AgentSpec],
    ownership_specs: &[AgentSpec],
    this_host: &str,
) -> MaterializeReport {
    let mut report = MaterializeReport::default();
    let selected_ids = selected_specs
        .iter()
        .map(|spec| spec.bus_id(this_host))
        .collect::<HashSet<_>>();
    if let Err(error) = crate::reconcile::validate_task_identities(ownership_specs, this_host) {
        report.failed_agents.extend(selected_ids);
        report.errors.push(error.to_string());
        return report;
    }
    for conflict in render_ownership_conflicts(root, ownership_specs, this_host) {
        let affected = conflict
            .owners
            .iter()
            .filter(|owner| selected_ids.contains(*owner))
            .cloned()
            .collect::<Vec<_>>();
        if affected.is_empty() {
            continue;
        }
        report.failed_agents.extend(affected);
        report.errors.push(conflict.error());
    }

    for spec in selected_specs {
        if !spec.desired_state.is_running() || spec.resolved_host(this_host) != this_host {
            continue;
        }
        let bus_id = spec.bus_id(this_host);
        if report.failed_agents.contains(&bus_id) {
            continue;
        }
        match materialize_agent(root, spec, this_host) {
            Ok(notes) => {
                for note in notes {
                    if note.starts_with("WARN ") {
                        report.warnings.push(note);
                    } else {
                        report.materialized.push(note);
                    }
                }
            }
            Err(error) => {
                report
                    .errors
                    .push(format!("{}: {error:#}", spec.path.display()));
                report.failed_agents.insert(bus_id);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_dependency_detection_uses_the_expansion_grammar() {
        let plan = RenderPlan {
            ops: vec![RenderOp::File {
                destination: "settings".to_string(),
                content: "${ST_HOOKS}/claude-stop-failure.sh".to_string(),
                executable: false,
            }],
        };
        assert!(plan.references_variable("ST_HOOKS"));

        for literal in [
            "$$ST_HOOKS/escaped",
            "$ST_HOOKS_SUFFIX/not-the-hook-root",
            "${ST_HOOKS",
        ] {
            let plan = RenderPlan {
                ops: vec![RenderOp::File {
                    destination: "settings".to_string(),
                    content: literal.to_string(),
                    executable: false,
                }],
            };
            assert!(
                !plan.references_variable("ST_HOOKS"),
                "{literal} is not an expandable ST_HOOKS reference"
            );
        }
    }

    #[test]
    fn git_exclude_reports_missing_repo_without_spawning_git() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        assert!(!has_git_marker(&workspace));
        let error = git_exclude(&workspace, ".st2/").unwrap_err();
        assert!(
            error.to_string().contains("no .git marker"),
            "the no-repo case must be answered by the marker probe: {error:#}"
        );

        // A `.git` entry anywhere above the workspace (directory or file form, as linked
        // worktrees use) re-enables the real git path.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert!(has_git_marker(&workspace));
    }

    #[test]
    fn deep_merge_preserves_unrelated_keys_and_replaces_arrays() {
        let mut target = serde_json::json!({
            "keep": true,
            "nested": {"left": 1, "replace": "old"},
            "array": [1]
        });
        deep_merge(
            &mut target,
            serde_json::json!({
                "nested": {"right": 2, "replace": "new"},
                "array": [2]
            }),
            ArrayMerge::Replace,
            false,
        );
        assert_eq!(
            target,
            serde_json::json!({
                "keep": true,
                "nested": {"left": 1, "right": 2, "replace": "new"},
                "array": [2]
            })
        );
    }

    /// Union mode joins arrays idempotently: foreign entries survive and repeating the same
    /// patch adds nothing — the contract the generated hook registration relies on.
    #[test]
    fn deep_merge_union_preserves_foreign_array_entries_and_is_idempotent() {
        let mut target = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "user-audit.sh"}]}]}
        });
        let ours = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "$ST_HOOKS/claude-observe.sh Stop"}]}]}
        });
        deep_merge(&mut target, ours.clone(), ArrayMerge::Union, false);
        deep_merge(&mut target, ours, ArrayMerge::Union, false);
        assert_eq!(
            target,
            serde_json::json!({
                "hooks": {"Stop": [
                    {"hooks": [{"type": "command", "command": "user-audit.sh"}]},
                    {"hooks": [{"type": "command", "command": "$ST_HOOKS/claude-observe.sh Stop"}]}
                ]}
            })
        );
    }

    /// A hook-set upgrade renders every entry under a new content-addressed path. Union must
    /// supersede st2's prior entries — recognizable by the hook root — rather than accumulate
    /// them, while a user's entry under any other path survives every merge.
    #[test]
    fn union_supersedes_prior_hook_set_entries_but_never_foreign_ones() {
        // The prior set lives under a RELOCATED root — recognition is structural (set-shaped
        // path + managed basename), not derived from the current environment — and a MIXED
        // matcher group keeps its user hook while losing only the superseded st2 entry.
        let mut target = serde_json::json!({
            "hooks": {"Stop": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "user-guard.sh"},
                    {"type": "command", "command": "/old/root/sets/sha256-aaa/claude-observe.sh Stop"}
                ]},
                {"hooks": [{"type": "command", "command": "user-audit.sh"}]},
                {"hooks": [{"type": "command", "command": "/old/root/sets/sha256-aaa/claude-observe.sh Stop"}]},
                {"hooks": [{"type": "command", "command": "/home/x/claude-observe.sh Stop"}]},
                {"hooks": [{"type": "command", "command": "$ST_HOOKS_SUFFIX/tool.sh"}]}
            ]}
        });
        let upgraded = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/new/root/sets/sha256-bbb/claude-observe.sh Stop"}]}]}
        });
        deep_merge(&mut target, upgraded.clone(), ArrayMerge::Union, true);
        deep_merge(&mut target, upgraded, ArrayMerge::Union, true);
        assert_eq!(
            target,
            serde_json::json!({
                "hooks": {"Stop": [
                    // The mixed group survives with only its user hook.
                    {"matcher": "Bash", "hooks": [
                        {"type": "command", "command": "user-guard.sh"}
                    ]},
                    {"hooks": [{"type": "command", "command": "user-audit.sh"}]},
                    // A managed basename OUTSIDE a set-shaped path is a user's wrapper: foreign.
                    {"hooks": [{"type": "command", "command": "/home/x/claude-observe.sh Stop"}]},
                    // `$ST_HOOKS_SUFFIX` is somebody else's variable, not ours at a boundary.
                    {"hooks": [{"type": "command", "command": "$ST_HOOKS_SUFFIX/tool.sh"}]},
                    {"hooks": [{"type": "command", "command": "/new/root/sets/sha256-bbb/claude-observe.sh Stop"}]}
                ]}
            })
        );
    }

    /// W8-14: a CURRENT managed entry nested inside a USER's group is still superseded — the
    /// canonical registration lives only in its own group, or Claude runs it twice.
    #[test]
    fn a_managed_leaf_survives_only_inside_its_canonical_group() {
        let current = "/root/sets/sha256-bbb/claude-observe.sh Stop";
        let mut target = serde_json::json!({
            "hooks": {"Stop": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "user-guard.sh"},
                    {"type": "command", "command": current}
                ]}
            ]}
        });
        let patch = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": current}]}]}
        });
        deep_merge(&mut target, patch.clone(), ArrayMerge::Union, true);
        deep_merge(&mut target, patch, ArrayMerge::Union, true);
        assert_eq!(
            target,
            serde_json::json!({
                "hooks": {"Stop": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "user-guard.sh"}]},
                    {"hooks": [{"type": "command", "command": current}]}
                ]}
            })
        );
    }

    /// Supersession is the CANONICAL Claude registration's maintenance rule, not a property
    /// of union merges: the same merge against any other destination must never delete user
    /// array elements that merely match the managed-path heuristic.
    #[test]
    fn union_outside_the_canonical_settings_never_prunes_managed_shaped_entries() {
        let mut target = serde_json::json!({
            "tools": {"audit": [
                {"command": "user-audit.sh"},
                {"command": "/old/root/sets/sha256-aaa/claude-observe.sh Stop"}
            ]}
        });
        let patch = serde_json::json!({
            "tools": {"audit": [{"command": "vendor-check.sh"}]}
        });
        deep_merge(&mut target, patch, ArrayMerge::Union, false);
        assert_eq!(
            target,
            serde_json::json!({
                "tools": {"audit": [
                    {"command": "user-audit.sh"},
                    // A managed-SHAPED path is only superseded where st2 owns the file;
                    // here it is somebody's data.
                    {"command": "/old/root/sets/sha256-aaa/claude-observe.sh Stop"},
                    {"command": "vendor-check.sh"}
                ]}
            })
        );
    }

    /// W8-13: legacy `deliver "mcp"` seats render the same canonical hook registration the typed
    /// driver does — they run under claude-session too.
    #[test]
    fn legacy_mcp_seats_render_the_canonical_hook_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let declaration = tmp.path().join("agents/h/worker/agent.kdl");
        std::fs::create_dir_all(declaration.parent().unwrap()).unwrap();
        std::fs::write(
            &declaration,
            r#"agent "worker" { host "h"; command "claude"; deliver "mcp"; workspace "$CATALOG" }"#,
        )
        .unwrap();
        let found = crate::discover(tmp.path());
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let plan = effective_plan(tmp.path(), &found.specs[0], "h").unwrap();
        let settings = plan
            .ops
            .iter()
            .find_map(|op| match op {
                RenderOp::JsonUpsert {
                    destination,
                    content,
                    arrays,
                    ..
                } if destination == ".claude/settings.local.json" => Some((content, arrays)),
                _ => None,
            })
            .expect("legacy mcp seats register hooks");
        assert_eq!(*settings.1, ArrayMerge::Union);
        let rendered: serde_json::Value = serde_json::from_str(settings.0).unwrap();
        assert_eq!(rendered, crate::hooks::claude_settings_registration());
    }

    #[test]
    fn source_presence_does_not_require_an_external_absolute_path() {
        let catalog = tempfile::tempdir().unwrap();
        let external = catalog
            .path()
            .parent()
            .unwrap()
            .join(format!("missing-external-{}", std::process::id()));
        let env = BTreeMap::new();

        assert_eq!(
            source(
                catalog.path(),
                catalog.path(),
                external.to_str().unwrap(),
                &env,
                true,
            )
            .unwrap(),
            external
        );
        assert!(
            source(
                catalog.path(),
                catalog.path(),
                catalog.path().join("missing").to_str().unwrap(),
                &env,
                true,
            )
            .is_err()
        );
    }
}
