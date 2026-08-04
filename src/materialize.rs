//! Declarative workspace materialization for catalog `render {}` blocks.
//!
//! The runner stays harness-agnostic: these are generic, ordered file operations. Content operations
//! gate an agent's boot; `git-exclude` is advisory and can never prevent a launch.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use kdl::{KdlDocument, KdlNode};

use agent_spec::spec::AgentSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderOp {
    Copy {
        source: String,
        destination: String,
    },
    File {
        destination: String,
        content: String,
    },
    JsonUpsert {
        destination: String,
        content: String,
    },
    EnsureLine {
        destination: String,
        line: String,
    },
    GitExclude {
        path: String,
    },
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
            } => {
                references_variable(source, variable) || references_variable(destination, variable)
            }
            Self::File {
                destination,
                content,
            }
            | Self::JsonUpsert {
                destination,
                content,
            } => {
                references_variable(destination, variable) || references_variable(content, variable)
            }
            Self::EnsureLine { destination, line } => {
                references_variable(destination, variable) || references_variable(line, variable)
            }
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
                let [source, destination] = args.as_slice() else {
                    anyhow::bail!(
                        "agent '{agent}': copy expects `copy \"<source>\" \"<destination>\"`"
                    );
                };
                plan.ops.push(RenderOp::Copy {
                    source: source.clone(),
                    destination: destination.clone(),
                });
            }
            "file" => {
                let Some(destination) = args.first() else {
                    anyhow::bail!("agent '{agent}': file needs a destination");
                };
                let content = content_arg(directive)
                    .with_context(|| format!("agent '{agent}': file needs content"))?;
                plan.ops.push(RenderOp::File {
                    destination: destination.clone(),
                    content,
                });
            }
            "json-upsert" => {
                let Some(destination) = args.first() else {
                    anyhow::bail!("agent '{agent}': json-upsert needs a destination");
                };
                let content = content_arg(directive)
                    .with_context(|| format!("agent '{agent}': json-upsert needs content"))?;
                serde_json::from_str::<serde_json::Value>(&content).with_context(|| {
                    format!("agent '{agent}': json-upsert content is not valid JSON")
                })?;
                plan.ops.push(RenderOp::JsonUpsert {
                    destination: destination.clone(),
                    content,
                });
            }
            "ensure-line" => {
                let [destination, line] = args.as_slice() else {
                    anyhow::bail!(
                        "agent '{agent}': ensure-line expects `ensure-line \"<file>\" \"<line>\"`"
                    );
                };
                plan.ops.push(RenderOp::EnsureLine {
                    destination: destination.clone(),
                    line: line.clone(),
                });
            }
            "git-exclude" => {
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
    let plan = parse_plan(spec)?;
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
        let resolved = source(root, spec_dir, &raw_source, &env)?;
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
) -> Result<PathBuf> {
    let raw = expand(raw, env);
    let path = Path::new(&raw);
    if path.is_absolute() {
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

fn deep_merge(target: &mut serde_json::Value, patch: serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

fn git_exclude(workspace: &Path, line: &str) -> Result<bool> {
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
        note: String,
    },
    Note(String),
    GitExclude {
        path: String,
        expanded: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum RenderClaim {
    Replace(Vec<u8>),
    JsonUpsert(serde_json::Value),
    EnsureLine(String),
}

#[derive(Debug)]
pub struct RenderOwnershipConflict {
    pub destination: PathBuf,
    pub owners: BTreeSet<String>,
}

impl RenderOwnershipConflict {
    fn error(&self) -> String {
        format!(
            "conflicting render ownership for '{}': active agents {} declare incompatible content for one shared workspace target",
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
    let plan = parse_plan(spec)?;
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
            } => {
                let source = source(root, spec_dir, &raw_source, &env)?;
                let bytes = fs::read(&source)
                    .with_context(|| format!("reading copy source {}", source.display()))?;
                (
                    destination(&workspace, &raw_destination, &env)?,
                    RenderClaim::Replace(bytes),
                )
            }
            RenderOp::File {
                destination: raw_destination,
                content,
            } => (
                destination(&workspace, &raw_destination, &env)?,
                RenderClaim::Replace(expand(&content, &env).into_bytes()),
            ),
            RenderOp::JsonUpsert {
                destination: raw_destination,
                content,
            } => {
                let patch = serde_json::from_str(&expand(&content, &env)).with_context(|| {
                    format!(
                        "expanded json-upsert for '{}' is not valid JSON",
                        spec.identity
                    )
                })?;
                (
                    destination(&workspace, &raw_destination, &env)?,
                    RenderClaim::JsonUpsert(patch),
                )
            }
            RenderOp::EnsureLine {
                destination: raw_destination,
                line,
            } => (
                destination(&workspace, &raw_destination, &env)?,
                RenderClaim::EnsureLine(expand(&line, &env)),
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
        if spec.retired || spec.resolved_host(this_host) != this_host {
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
    let plan = parse_plan(spec)?;
    if plan.ops.is_empty() {
        return Ok(Vec::new());
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
    let mut virtual_files = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut changed_targets = BTreeMap::<PathBuf, String>::new();
    let mut prepared = Vec::new();
    for op in plan.ops {
        match op {
            RenderOp::Copy {
                source: raw_source,
                destination: raw_destination,
            } => {
                let source = source(root, spec_dir, &raw_source, &env)?;
                let destination = destination(&workspace, &raw_destination, &env)?;
                let bytes = fs::read(&source)
                    .with_context(|| format!("reading copy source {}", source.display()))?;
                let current = virtual_files
                    .get(&destination)
                    .cloned()
                    .map(Some)
                    .unwrap_or(read_optional(&destination)?);
                let note = format!("{}: copied {}", spec.identity, raw_destination);
                if current.as_deref() == Some(bytes.as_slice()) {
                    prepared.push(PreparedOp::Note(note));
                } else {
                    changed_targets.insert(destination.clone(), raw_destination);
                    prepared.push(PreparedOp::Write {
                        destination: destination.clone(),
                        bytes: bytes.clone(),
                        note,
                    });
                }
                virtual_files.insert(destination, bytes);
            }
            RenderOp::File {
                destination: raw_destination,
                content,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let bytes = expand(&content, &env).into_bytes();
                let current = virtual_files
                    .get(&destination)
                    .cloned()
                    .map(Some)
                    .unwrap_or(read_optional(&destination)?);
                let note = format!("{}: wrote {}", spec.identity, raw_destination);
                if current.as_deref() == Some(bytes.as_slice()) {
                    prepared.push(PreparedOp::Note(note));
                } else {
                    changed_targets.insert(destination.clone(), raw_destination);
                    prepared.push(PreparedOp::Write {
                        destination: destination.clone(),
                        bytes: bytes.clone(),
                        note,
                    });
                }
                virtual_files.insert(destination, bytes);
            }
            RenderOp::JsonUpsert {
                destination: raw_destination,
                content,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let current = virtual_files
                    .get(&destination)
                    .cloned()
                    .map(Some)
                    .unwrap_or(read_optional(&destination)?);
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
                deep_merge(&mut target, patch);
                let mut bytes = serde_json::to_vec_pretty(&target)?;
                bytes.push(b'\n');
                let note = format!("{}: upserted {}", spec.identity, raw_destination);
                if current.as_deref() == Some(bytes.as_slice()) {
                    prepared.push(PreparedOp::Note(note));
                } else {
                    changed_targets.insert(destination.clone(), raw_destination);
                    prepared.push(PreparedOp::Write {
                        destination: destination.clone(),
                        bytes: bytes.clone(),
                        note,
                    });
                }
                virtual_files.insert(destination, bytes);
            }
            RenderOp::EnsureLine {
                destination: raw_destination,
                line,
            } => {
                let destination = destination(&workspace, &raw_destination, &env)?;
                let current = virtual_files
                    .get(&destination)
                    .cloned()
                    .map(Some)
                    .unwrap_or(read_optional(&destination)?);
                let bytes =
                    ensure_line_bytes(current.as_deref(), &expand(&line, &env), &destination)?;
                let note = format!("{}: ensured {}", spec.identity, raw_destination);
                if current.as_deref() == Some(bytes.as_slice()) {
                    prepared.push(PreparedOp::Note(note));
                } else {
                    changed_targets.insert(destination.clone(), raw_destination);
                    prepared.push(PreparedOp::Write {
                        destination: destination.clone(),
                        bytes: bytes.clone(),
                        note,
                    });
                }
                virtual_files.insert(destination, bytes);
            }
            RenderOp::GitExclude { path } => {
                prepared.push(PreparedOp::GitExclude {
                    expanded: expand(&path, &env),
                    path,
                });
            }
        }
    }

    for (target, raw_destination) in &changed_targets {
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
                 keep the tracked file byte-identical, choose an untracked overlay target, or edit \
                 the tracked file intentionally outside st2",
                spec.identity,
                raw_destination,
                target.display()
            );
        }
    }

    let mut notes = Vec::new();
    for op in prepared {
        match op {
            PreparedOp::Write {
                destination,
                bytes,
                note,
            } => {
                write_owned(&destination, &bytes)?;
                notes.push(note);
            }
            PreparedOp::Note(note) => notes.push(note),
            PreparedOp::GitExclude { path, expanded } => {
                // Advisory by contract: leave a visible note, but never fail materialization/boot.
                match git_exclude(&workspace, &expanded) {
                    Ok(_) => notes.push(format!("{}: excluded {path}", spec.identity)),
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

/// Validate an agent's render declaration and all catalog-owned inputs without writing its workspace.
pub fn validate_agent(root: &Path, spec: &AgentSpec, this_host: &str) -> Result<()> {
    crate::reconcile::validate_task_identities(std::slice::from_ref(spec), this_host)?;
    let plan = parse_plan(spec)?;
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
            } => {
                source(root, spec_dir, &raw_source, &env)?;
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
        if spec.retired || spec.resolved_host(this_host) != this_host {
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
                }],
            };
            assert!(
                !plan.references_variable("ST_HOOKS"),
                "{literal} is not an expandable ST_HOOKS reference"
            );
        }
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
}
