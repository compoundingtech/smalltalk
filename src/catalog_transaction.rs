//! Canonical declaration snapshots and crash-recoverable whole-catalog application.
//!
//! This module owns the declaration projection shared by bulk readers and writers. Runtime state
//! is never copied, hashed, removed, or locked behind this transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::catalog_lock::{APPLY_MARKER, CONTROL_DIR, CatalogLock};

const SNAPSHOT_SCHEMA: &str = "st2.catalog-snapshot.v1";
const DIFF_SCHEMA: &str = "st2.catalog-diff.v1";
const BOOTSTRAP_SCHEMA: &str = "st2.catalog-bootstrap.v1";
const APPLY_SCHEMA: &str = "st2.catalog-apply.v1";
const MARKER_SCHEMA: &str = "st2.catalog-apply-incomplete.v1";
const RAW_SNAPSHOT_SCHEMA: &str = "st2.catalog-raw-preimage-snapshot.v1";
const RAW_APPLY_SCHEMA: &str = "st2.catalog-raw-preimage-apply.v1";
const RAW_MARKER_SCHEMA: &str = "st2.catalog-raw-preimage-apply-incomplete.v1";
const DIGEST_SCHEMA: &str = "st2.catalog-digest.v1";
const HASH_DOMAIN: &[u8] = b"st2.catalog-declaration-root.v1\0";
const RAW_HASH_DOMAIN: &[u8] = b"st2.catalog-raw-preimage-root.v1\0";
const STAGE_PREFIX: &str = "catalog-apply-stage-";
const WRITER_TEMP_PREFIXES: [&str; 3] = [
    ".agent.kdl.presentation-",
    ".agent.kdl.publish-",
    ".catalog-apply-file-",
];
const TEMPLATE_MAX_DEPTH: usize = 8;
const TEMPLATE_MAX_FILES: usize = 256;
const TEMPLATE_MAX_FILE_BYTES: u64 = 1024 * 1024;
const TEMPLATE_MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct SnapshotRequest {
    pub catalog: PathBuf,
    pub output: PathBuf,
    pub raw_preimage: bool,
}

#[derive(Debug)]
pub struct DiffRequest {
    pub catalog: PathBuf,
    pub prepared: PathBuf,
    pub expect_sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub schema: &'static str,
    pub catalog: PathBuf,
    pub prepared: PathBuf,
    pub before_root_sha256: String,
    pub after_root_sha256: String,
    pub paths: Vec<PathDelta>,
    pub agents: Vec<AgentSemanticDelta>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathDelta {
    pub path: String,
    pub kind: DeltaKind,
    pub before: Option<PathVersion>,
    pub after: Option<PathVersion>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathVersion {
    pub class: PathClass,
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathClass {
    Catalog,
    AgentSpec,
    Render,
    Template,
    Static,
    WorkspaceFact,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeltaKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSemanticDelta {
    pub host: String,
    pub identity: String,
    pub kind: DeltaKind,
    pub fields: Vec<FieldDelta>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDelta {
    pub address: String,
    pub before: SemanticValue,
    pub after: SemanticValue,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticValue {
    pub state: SemanticState,
    #[serde(rename = "type")]
    pub value_type: SemanticType,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SemanticState {
    Absent,
    Default,
    Present,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SemanticType {
    String,
    Bool,
    U64,
    Duration,
    Object,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResult {
    pub schema: &'static str,
    pub status: SnapshotStatus,
    pub catalog: PathBuf,
    pub output: PathBuf,
    pub root_sha256: String,
    pub entries: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotStatus {
    Created,
    Unchanged,
}

#[derive(Debug)]
pub struct BootstrapRequest {
    pub catalog: PathBuf,
    pub prepared: PathBuf,
    pub input_sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResult {
    pub schema: &'static str,
    pub status: BootstrapStatus,
    pub catalog: PathBuf,
    pub prepared: PathBuf,
    pub root_sha256: String,
    pub entries: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BootstrapStatus {
    Created,
    Unchanged,
}

#[derive(Debug)]
pub struct ApplyRequest {
    pub catalog: PathBuf,
    pub mode: ApplyMode,
}

#[derive(Debug)]
pub enum ApplyMode {
    Prepared {
        prepared: PathBuf,
        input_sha256: String,
        expect_sha256: String,
    },
    RawPreimage {
        prepared: PathBuf,
        input_sha256: String,
        expect_sha256: String,
    },
    Resume,
}

/// Compute the declaration-root digest that binds a prepared whole-catalog input.
///
/// This uses the same retained, no-follow capture and projection as [`apply`], but performs no
/// catalog mutation. Deployment producers normally retain this digest from their own prepared
/// artifact; the public helper keeps tests and other Rust callers on the transaction's exact hash
/// domain.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDigest {
    pub schema: &'static str,
    pub catalog: PathBuf,
    pub prepared: PathBuf,
    pub root_sha256: String,
}

pub fn digest_prepared(catalog: &Path, prepared: &Path) -> Result<CatalogDigest> {
    let catalog = canonical_real_dir(catalog, "catalog")?;
    let prepared = canonical_real_dir_no_alias(prepared, "prepared catalog")?;
    anyhow::ensure!(
        !prepared.starts_with(&catalog),
        "prepared catalog must be outside the live catalog: {}",
        prepared.display()
    );
    let captured = tempfile::tempdir().context("create prepared-catalog capture root")?;
    capture_prepared_catalog(&prepared, captured.path())?;
    let root_sha256 = project(captured.path(), ProjectionSource::Prepared, &catalog)?.root_sha256;
    Ok(CatalogDigest {
        schema: DIGEST_SCHEMA,
        catalog,
        prepared,
        root_sha256,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub schema: &'static str,
    pub status: ApplyStatus,
    pub catalog: PathBuf,
    pub prepared: Option<PathBuf>,
    pub before_sha256: String,
    pub after_sha256: String,
    pub entries: usize,
    pub recovered: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplyStatus {
    Applied,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectedFile {
    bytes: Vec<u8>,
    executable: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DeclarationProjection {
    files: BTreeMap<String, ProjectedFile>,
    profile_modules: BTreeSet<String>,
    workspace_dirs: BTreeSet<String>,
    root_sha256: String,
}

impl DeclarationProjection {
    pub(crate) fn entries(&self) -> usize {
        self.files.len()
    }
}

fn raw_declarations_match(
    catalog: &Path,
    current: &DeclarationProjection,
    desired: &DeclarationProjection,
) -> bool {
    if !current
        .files
        .iter()
        .all(|(path, file)| desired.files.get(path) == Some(file))
    {
        return false;
    }

    let mut live_profile_modules = BTreeMap::new();
    for path in &desired.profile_modules {
        if current.files.contains_key(path) {
            continue;
        }
        if add_profile_module(catalog, Path::new(path), &mut live_profile_modules).is_err() {
            return false;
        }
    }
    desired.files.iter().all(|(path, file)| {
        current.files.get(path) == Some(file) || live_profile_modules.get(path) == Some(file)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionSource {
    Current,
    Prepared,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyMarker {
    schema: String,
    stage_name: String,
    expected_root_sha256: String,
    prepared_root_sha256: String,
    #[serde(default)]
    original_profile_modules: Vec<String>,
    original_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentKey {
    pub host: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTransition {
    pub original_agents: BTreeSet<AgentKey>,
}

pub fn catalog_transition(catalog: &Path) -> Result<Option<CatalogTransition>> {
    let root = open_dir_beneath(catalog, catalog)?;
    let control = match openat_dir_nofollow(&root, std::ffi::OsStr::new(CONTROL_DIR)) {
        Ok(control) => control,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open catalog control directory"),
    };
    let marker = read_marker_optional(&retained_dir_path(&control)?.join(APPLY_MARKER))?;
    let Some(marker) = marker else {
        return Ok(None);
    };
    validate_marker(&marker)?;
    let original_agents = marker
        .original_paths
        .iter()
        .filter_map(|path| {
            let components = path.split('/').collect::<Vec<_>>();
            (components.len() == 4 && components[0] == "agents" && components[3] == "agent.kdl")
                .then(|| AgentKey {
                    host: components[1].to_string(),
                    identity: components[2].to_string(),
                })
        })
        .collect();
    Ok(Some(CatalogTransition { original_agents }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticAtom {
    value_type: SemanticType,
    state: SemanticAtomState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemanticAtomState {
    Absent,
    Default,
    Present(String),
}

/// Compare a retained prepared capture with one coherent live declaration snapshot.
///
/// This operation deliberately owns no migration policy and performs no publication. The shared
/// lock excludes cooperating declaration writers while the live projection is observed; the
/// prepared source is copied through retained no-follow capabilities before it is parsed.
pub fn diff(request: DiffRequest) -> Result<DiffResult> {
    validate_sha256(&request.expect_sha256)?;
    let catalog = canonical_real_dir_no_alias(&request.catalog, "catalog")?;
    let prepared = canonical_real_dir_no_alias(&request.prepared, "prepared catalog")?;
    anyhow::ensure!(
        !prepared.starts_with(&catalog),
        "prepared diff source must be outside the catalog: {}",
        prepared.display()
    );

    let lock = CatalogLock::shared_existing(&catalog)?;
    let retained_catalog = retained_dir_path(lock.root())?
        .canonicalize()
        .context("resolve retained live catalog")?;
    let before = project(&retained_catalog, ProjectionSource::Current, &catalog)?;
    validate_projection_link_counts(&retained_catalog, &before, "live catalog")?;
    validate_live_workspace_facts(&catalog, &before.workspace_dirs)?;
    validate_full_catalog(&retained_catalog).context("validate live catalog for diff")?;
    anyhow::ensure!(
        before.root_sha256 == request.expect_sha256,
        "catalog diff precondition failed: expected root sha256 {}, found {}",
        request.expect_sha256,
        before.root_sha256
    );

    let captured = tempfile::tempdir().context("create prepared diff capture root")?;
    capture_prepared_catalog(&prepared, captured.path())?;
    let after = project(captured.path(), ProjectionSource::Prepared, &catalog)?;
    validate_full_catalog(captured.path()).context("validate prepared catalog for diff")?;

    let before_specs = canonical_semantic_specs(&retained_catalog)?;
    let after_specs = canonical_semantic_specs(captured.path())?;
    let before_render = render_input_paths(&retained_catalog, &before_specs)?;
    let after_render = render_input_paths(captured.path(), &after_specs)?;
    let paths = path_deltas(&before, &after, &before_render, &after_render);
    let agents = agent_semantic_deltas(&before_specs, &after_specs)?;

    Ok(DiffResult {
        schema: DIFF_SCHEMA,
        catalog,
        prepared,
        before_root_sha256: before.root_sha256,
        after_root_sha256: after.root_sha256,
        paths,
        agents,
    })
}

fn canonical_semantic_specs(root: &Path) -> Result<BTreeMap<AgentKey, agent_spec::AgentSpec>> {
    let discovered = crate::discover(root);
    anyhow::ensure!(
        discovered.errors.is_empty() && discovered.warnings.is_empty(),
        "catalog semantic discovery is not exact: {} error(s), {} warning(s)",
        discovered.errors.len(),
        discovered.warnings.len()
    );
    let mut specs = BTreeMap::new();
    for spec in discovered.specs {
        let host = spec
            .host
            .as_deref()
            .context("canonical semantic spec is missing explicit host")?;
        let key = AgentKey {
            host: host.to_string(),
            identity: spec.identity.clone(),
        };
        anyhow::ensure!(
            specs.insert(key.clone(), spec).is_none(),
            "catalog semantic identity is ambiguous: {}.{}",
            key.host,
            key.identity
        );
    }
    Ok(specs)
}

fn render_input_paths(
    root: &Path,
    specs: &BTreeMap<AgentKey, agent_spec::AgentSpec>,
) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for spec in specs.values() {
        let host = spec
            .host
            .as_deref()
            .context("semantic spec host disappeared")?;
        for input in crate::materialize::catalog_owned_render_inputs(root, spec, host)? {
            paths.insert(normalized_relative(root, &input)?);
        }
    }
    Ok(paths)
}

fn path_deltas(
    before: &DeclarationProjection,
    after: &DeclarationProjection,
    before_render: &BTreeSet<String>,
    after_render: &BTreeSet<String>,
) -> Vec<PathDelta> {
    let paths = before
        .files
        .keys()
        .chain(after.files.keys())
        .chain(before.workspace_dirs.iter())
        .chain(after.workspace_dirs.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter_map(|path| {
            let before_version = path_version(before, before_render, &path);
            let after_version = path_version(after, after_render, &path);
            if path_is_equal(before, after, before_render, after_render, &path) {
                return None;
            }
            let kind = match (&before_version, &after_version) {
                (None, Some(_)) => DeltaKind::Added,
                (Some(_), None) => DeltaKind::Removed,
                (Some(_), Some(_)) => DeltaKind::Modified,
                (None, None) => unreachable!("union path must exist in one projection"),
            };
            Some(PathDelta {
                path,
                kind,
                before: before_version,
                after: after_version,
            })
        })
        .collect()
}

fn path_is_equal(
    before: &DeclarationProjection,
    after: &DeclarationProjection,
    before_render: &BTreeSet<String>,
    after_render: &BTreeSet<String>,
    path: &str,
) -> bool {
    let before_workspace = before.workspace_dirs.contains(path);
    let after_workspace = after.workspace_dirs.contains(path);
    if before_workspace || after_workspace {
        return before_workspace == after_workspace;
    }
    match (before.files.get(path), after.files.get(path)) {
        (Some(before), Some(after)) => {
            before == after
                && std::mem::discriminant(&classify_path(path, before_render))
                    == std::mem::discriminant(&classify_path(path, after_render))
        }
        (None, None) => true,
        _ => false,
    }
}

fn path_version(
    projection: &DeclarationProjection,
    render_inputs: &BTreeSet<String>,
    path: &str,
) -> Option<PathVersion> {
    if projection.workspace_dirs.contains(path) {
        return Some(PathVersion {
            class: PathClass::WorkspaceFact,
            executable: false,
        });
    }
    projection.files.get(path).map(|file| PathVersion {
        class: classify_path(path, render_inputs),
        executable: file.executable,
    })
}

fn classify_path(path: &str, render_inputs: &BTreeSet<String>) -> PathClass {
    if path == crate::catalog::CONFIG_FILE {
        PathClass::Catalog
    } else if path.starts_with("_templates/") {
        PathClass::Template
    } else if is_canonical_agent_spec(path) {
        PathClass::AgentSpec
    } else if render_inputs.contains(path) {
        PathClass::Render
    } else {
        PathClass::Static
    }
}

fn agent_semantic_deltas(
    before: &BTreeMap<AgentKey, agent_spec::AgentSpec>,
    after: &BTreeMap<AgentKey, agent_spec::AgentSpec>,
) -> Result<Vec<AgentSemanticDelta>> {
    let keys = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut agents = Vec::new();
    for key in keys {
        let before_fields = before
            .get(&key)
            .map(normalize_agent)
            .transpose()?
            .unwrap_or_default();
        let after_fields = after
            .get(&key)
            .map(normalize_agent)
            .transpose()?
            .unwrap_or_default();
        let addresses = before_fields
            .keys()
            .chain(after_fields.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let fields = addresses
            .into_iter()
            .filter_map(|address| {
                let before = before_fields
                    .get(&address)
                    .cloned()
                    .unwrap_or_else(|| absent_atom(after_fields[&address].value_type));
                let after = after_fields
                    .get(&address)
                    .cloned()
                    .unwrap_or_else(|| absent_atom(before_fields[&address].value_type));
                (before != after).then(|| FieldDelta {
                    before: semantic_value(before),
                    after: semantic_value(after),
                    address,
                })
            })
            .collect::<Vec<_>>();
        if fields.is_empty() {
            continue;
        }
        let kind = match (before.contains_key(&key), after.contains_key(&key)) {
            (false, true) => DeltaKind::Added,
            (true, false) => DeltaKind::Removed,
            (true, true) => DeltaKind::Modified,
            (false, false) => unreachable!("union agent key must exist in one projection"),
        };
        agents.push(AgentSemanticDelta {
            host: key.host,
            identity: key.identity,
            kind,
            fields,
        });
    }
    Ok(agents)
}

fn normalize_agent(spec: &agent_spec::AgentSpec) -> Result<BTreeMap<String, SemanticAtom>> {
    use agent_spec::{Restart, RestartMode, StreamLaunch, TaskKind, TaskLifecycle};

    let host = spec
        .host
        .as_deref()
        .context("canonical semantic spec has no host")?;
    let base = format!(
        "/agents/{}/{}",
        pointer_segment(host),
        pointer_segment(&spec.identity)
    );
    let mut fields = BTreeMap::new();
    insert_value(
        &mut fields,
        &format!("{base}/identity"),
        SemanticType::String,
        &spec.identity,
    );
    insert_optional(
        &mut fields,
        &format!("{base}/name"),
        SemanticType::String,
        spec.name.as_deref(),
    );
    insert_optional(
        &mut fields,
        &format!("{base}/description"),
        SemanticType::String,
        spec.description.as_deref(),
    );
    insert_optional(
        &mut fields,
        &format!("{base}/host"),
        SemanticType::String,
        spec.host.as_deref(),
    );
    insert_optional(
        &mut fields,
        &format!("{base}/role"),
        SemanticType::String,
        spec.role.as_deref(),
    );
    fields.insert(format!("{base}/type"), default_atom(SemanticType::String));
    insert_optional(
        &mut fields,
        &format!("{base}/workspace"),
        SemanticType::String,
        spec.workspace.as_deref(),
    );
    insert_optional(
        &mut fields,
        &format!("{base}/supervisor"),
        SemanticType::String,
        spec.supervisor.as_deref(),
    );
    insert_default_bool(
        &mut fields,
        &format!("{base}/retired"),
        spec.desired_state.is_retired(),
        false,
    );
    insert_default_value(
        &mut fields,
        &format!("{base}/desired-state"),
        SemanticType::String,
        spec.desired_state.as_str().to_owned(),
        spec.desired_state.is_running(),
    );
    insert_optional(
        &mut fields,
        &format!("{base}/desired-state/reason"),
        SemanticType::String,
        spec.desired_state.reason(),
    );
    insert_default_bool(&mut fields, &format!("{base}/keep"), spec.keep, false);
    insert_optional(
        &mut fields,
        &format!("{base}/delivery"),
        SemanticType::String,
        spec.delivery.map(|delivery| delivery.as_str()),
    );

    for stream in &spec.streams {
        let stream_base = format!("{base}/streams/{}", pointer_segment(&stream.name));
        match &stream.launch {
            None => insert_value(
                &mut fields,
                &format!("{stream_base}/launch"),
                SemanticType::String,
                "external",
            ),
            Some(StreamLaunch::Command(command)) => {
                insert_value(
                    &mut fields,
                    &format!("{stream_base}/launch"),
                    SemanticType::String,
                    "command",
                );
                insert_value(
                    &mut fields,
                    &format!("{stream_base}/command"),
                    SemanticType::String,
                    command,
                );
            }
            Some(StreamLaunch::Argv(argv)) => {
                insert_value(
                    &mut fields,
                    &format!("{stream_base}/launch"),
                    SemanticType::String,
                    "argv",
                );
                for (index, argument) in argv.iter().enumerate() {
                    insert_value(
                        &mut fields,
                        &format!("{stream_base}/argv/{index}"),
                        SemanticType::String,
                        argument,
                    );
                }
            }
        }
    }

    let restart = spec.restart_policy();
    let default_restart = Restart::default();
    insert_default_value(
        &mut fields,
        &format!("{base}/restart/attempts"),
        SemanticType::U64,
        restart.attempts.to_string(),
        restart.attempts == default_restart.attempts,
    );
    insert_default_value(
        &mut fields,
        &format!("{base}/restart/interval-ms"),
        SemanticType::Duration,
        restart.interval.as_millis().to_string(),
        restart.interval == default_restart.interval,
    );
    insert_default_value(
        &mut fields,
        &format!("{base}/restart/delay-ms"),
        SemanticType::Duration,
        restart.delay.as_millis().to_string(),
        restart.delay == default_restart.delay,
    );
    let restart_mode = match restart.mode {
        RestartMode::Fail => "fail",
        RestartMode::Delay => "delay",
    };
    insert_default_value(
        &mut fields,
        &format!("{base}/restart/mode"),
        SemanticType::String,
        restart_mode.into(),
        restart.mode == default_restart.mode,
    );

    for resource in &spec.resources {
        let root = format!("{base}/resources/{}", pointer_segment(resource.name()));
        insert_value(
            &mut fields,
            &format!("{root}/uri"),
            SemanticType::String,
            resource.uri(),
        );
        insert_value(
            &mut fields,
            &format!("{root}/reason"),
            SemanticType::String,
            resource.reason(),
        );
        insert_optional(
            &mut fields,
            &format!("{root}/inactive-reason"),
            SemanticType::String,
            resource.inactive_reason(),
        );
        let selector = resource
            .selector()
            .map(serde_json::to_string)
            .transpose()
            .context("encode normalized Resource selector")?;
        insert_optional(
            &mut fields,
            &format!("{root}/selector"),
            SemanticType::Object,
            selector.as_deref(),
        );
    }
    for task in &spec.tasks {
        let kind = match task.kind {
            TaskKind::Pty => "pty",
            TaskKind::Exec => "exec",
        };
        let root = format!("{base}/tasks/{kind}/{}", pointer_segment(&task.name));
        insert_value(
            &mut fields,
            &format!("{root}/kind"),
            SemanticType::String,
            kind,
        );
        insert_default_bool(&mut fields, &format!("{root}/derived"), task.derived, false);
        let effective_id = task
            .id
            .clone()
            .unwrap_or_else(|| format!("{}.{}", spec.bus_id(host), task.name));
        insert_value(
            &mut fields,
            &format!("{root}/id"),
            SemanticType::String,
            &effective_id,
        );
        insert_optional(
            &mut fields,
            &format!("{root}/command"),
            SemanticType::String,
            task.command.as_deref(),
        );
        match &task.argv {
            Some(argv) => {
                fields.insert(
                    format!("{root}/argv"),
                    present_atom(SemanticType::Object, "argv"),
                );
                for (index, value) in argv.iter().enumerate() {
                    insert_value(
                        &mut fields,
                        &format!("{root}/argv/{index}"),
                        SemanticType::String,
                        value,
                    );
                }
            }
            None => {
                fields.insert(format!("{root}/argv"), absent_atom(SemanticType::Object));
            }
        }
        match task.cwd.as_deref().or(spec.workspace.as_deref()) {
            Some(effective_cwd) => insert_value(
                &mut fields,
                &format!("{root}/cwd"),
                SemanticType::String,
                effective_cwd,
            ),
            None => {
                fields.insert(format!("{root}/cwd"), default_atom(SemanticType::String));
            }
        }
        for (key, value) in &task.tags {
            insert_value(
                &mut fields,
                &format!("{root}/tags/{}", pointer_segment(key)),
                SemanticType::String,
                value,
            );
        }
        for (key, value) in &task.env {
            insert_value(
                &mut fields,
                &format!("{root}/env/{}", pointer_segment(key)),
                SemanticType::String,
                value,
            );
        }
        insert_default_bool(&mut fields, &format!("{root}/keep"), task.keep, false);
        insert_default_value(
            &mut fields,
            &format!("{root}/lifecycle"),
            SemanticType::String,
            match task.lifecycle {
                TaskLifecycle::Service => "service",
                TaskLifecycle::AdoptOnly => "adopt-only",
            }
            .into(),
            task.lifecycle == TaskLifecycle::Service,
        );
    }

    for (index, operation) in crate::materialize::parse_plan(spec)?.ops.iter().enumerate() {
        use crate::materialize::RenderOp;
        let root = format!("{base}/render/{index}");
        match operation {
            RenderOp::Copy {
                source,
                destination,
                executable,
            } => {
                insert_value(
                    &mut fields,
                    &format!("{root}/kind"),
                    SemanticType::String,
                    "copy",
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/source"),
                    SemanticType::String,
                    source,
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/destination"),
                    SemanticType::String,
                    destination,
                );
                insert_default_bool(
                    &mut fields,
                    &format!("{root}/executable"),
                    *executable,
                    false,
                );
            }
            RenderOp::File {
                destination,
                content,
                executable,
            } => {
                insert_value(
                    &mut fields,
                    &format!("{root}/kind"),
                    SemanticType::String,
                    "file",
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/destination"),
                    SemanticType::String,
                    destination,
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/content"),
                    SemanticType::String,
                    content,
                );
                insert_default_bool(
                    &mut fields,
                    &format!("{root}/executable"),
                    *executable,
                    false,
                );
            }
            RenderOp::JsonUpsert {
                destination,
                content,
                arrays,
                executable,
            } => {
                insert_value(
                    &mut fields,
                    &format!("{root}/kind"),
                    SemanticType::String,
                    "json-upsert",
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/destination"),
                    SemanticType::String,
                    destination,
                );
                let normalized = serde_json::to_string(
                    &serde_json::from_str::<serde_json::Value>(content)
                        .context("normalize json-upsert content")?,
                )?;
                insert_value(
                    &mut fields,
                    &format!("{root}/content"),
                    SemanticType::String,
                    &normalized,
                );
                if *arrays == crate::materialize::ArrayMerge::Union {
                    insert_value(
                        &mut fields,
                        &format!("{root}/arrays"),
                        SemanticType::String,
                        "union",
                    );
                }
                insert_default_bool(
                    &mut fields,
                    &format!("{root}/executable"),
                    *executable,
                    false,
                );
            }
            RenderOp::EnsureLine {
                destination,
                line,
                executable,
            } => {
                insert_value(
                    &mut fields,
                    &format!("{root}/kind"),
                    SemanticType::String,
                    "ensure-line",
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/destination"),
                    SemanticType::String,
                    destination,
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/line"),
                    SemanticType::String,
                    line,
                );
                insert_default_bool(
                    &mut fields,
                    &format!("{root}/executable"),
                    *executable,
                    false,
                );
            }
            RenderOp::GitExclude { path } => {
                insert_value(
                    &mut fields,
                    &format!("{root}/kind"),
                    SemanticType::String,
                    "git-exclude",
                );
                insert_value(
                    &mut fields,
                    &format!("{root}/path"),
                    SemanticType::String,
                    path,
                );
            }
        }
    }
    Ok(fields)
}

fn insert_value(
    fields: &mut BTreeMap<String, SemanticAtom>,
    address: &str,
    value_type: SemanticType,
    value: &str,
) {
    fields.insert(address.to_string(), present_atom(value_type, value));
}

fn insert_optional(
    fields: &mut BTreeMap<String, SemanticAtom>,
    address: &str,
    value_type: SemanticType,
    value: Option<&str>,
) {
    fields.insert(
        address.to_string(),
        value
            .map(|value| present_atom(value_type, value))
            .unwrap_or_else(|| absent_atom(value_type)),
    );
}

fn insert_default_bool(
    fields: &mut BTreeMap<String, SemanticAtom>,
    address: &str,
    value: bool,
    default: bool,
) {
    insert_default_value(
        fields,
        address,
        SemanticType::Bool,
        value.to_string(),
        value == default,
    );
}

fn insert_default_value(
    fields: &mut BTreeMap<String, SemanticAtom>,
    address: &str,
    value_type: SemanticType,
    value: String,
    is_default: bool,
) {
    fields.insert(
        address.to_string(),
        if is_default {
            default_atom(value_type)
        } else {
            present_atom(value_type, &value)
        },
    );
}

fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn present_atom(value_type: SemanticType, value: &str) -> SemanticAtom {
    SemanticAtom {
        value_type,
        state: SemanticAtomState::Present(value.to_string()),
    }
}

fn absent_atom(value_type: SemanticType) -> SemanticAtom {
    SemanticAtom {
        value_type,
        state: SemanticAtomState::Absent,
    }
}

fn default_atom(value_type: SemanticType) -> SemanticAtom {
    SemanticAtom {
        value_type,
        state: SemanticAtomState::Default,
    }
}

fn semantic_value(value: SemanticAtom) -> SemanticValue {
    SemanticValue {
        state: match value.state {
            SemanticAtomState::Absent => SemanticState::Absent,
            SemanticAtomState::Default => SemanticState::Default,
            SemanticAtomState::Present(_) => SemanticState::Present,
        },
        value_type: value.value_type,
    }
}

fn validate_projection_link_counts(
    root: &Path,
    projection: &DeclarationProjection,
    label: &str,
) -> Result<()> {
    for path in projection.files.keys() {
        let target = root.join(path);
        let metadata = fs::symlink_metadata(&target)
            .with_context(|| format!("inspect {label} projected file {}", target.display()))?;
        anyhow::ensure!(
            metadata.nlink() == 1,
            "{label} contains a hard-linked projected file: {}",
            target.display()
        );
    }
    Ok(())
}

/// Capture one coherent declaration plane under the shared catalog-authoring lock.
pub fn snapshot(request: SnapshotRequest) -> Result<SnapshotResult> {
    let catalog = canonical_real_dir(&request.catalog, "catalog")?;
    let output = absolute_path(&request.output)?;
    anyhow::ensure!(
        !output.starts_with(&catalog),
        "snapshot output must be outside the catalog: {}",
        output.display()
    );
    let parent = output.parent().context("snapshot output has no parent")?;
    let parent = canonical_real_dir(parent, "snapshot output parent")?;
    let output = parent.join(
        output
            .file_name()
            .context("snapshot output has no final path component")?,
    );

    let _lock = CatalogLock::shared(&catalog)?;
    let projection = if request.raw_preimage {

        let projection = project_raw_current(&catalog)?;
        validate_projection_link_counts(&catalog, &projection, "raw live catalog")?;
        projection
    } else {
        let projection = project(&catalog, ProjectionSource::Current, &catalog)?;
        validate_live_workspace_facts(&catalog, &projection.workspace_dirs)?;
        projection
    };
    match fs::symlink_metadata(&output) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "snapshot output is not a real directory: {}",
                output.display()
            );
            let existing = if request.raw_preimage {
                let existing = project_raw_current(&output)?;
                validate_projection_link_counts(&output, &existing, "raw snapshot output")?;
                existing
            } else {
                project(&output, ProjectionSource::Prepared, &catalog)?
            };
            anyhow::ensure!(
                existing.root_sha256 == projection.root_sha256,
                "snapshot output already exists with root sha256 {}, expected {}",
                existing.root_sha256,
                projection.root_sha256
            );
            return Ok(SnapshotResult {
                schema: if request.raw_preimage {
                    RAW_SNAPSHOT_SCHEMA
                } else {
                    SNAPSHOT_SCHEMA
                },
                status: SnapshotStatus::Unchanged,
                catalog,
                output,
                entries: projection.entries(),
                root_sha256: projection.root_sha256,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect snapshot output {}", output.display()));
        }
    }

    let stage = tempfile::Builder::new()
        .prefix(".st2-catalog-snapshot-")
        .tempdir_in(&parent)
        .with_context(|| format!("create snapshot stage in {}", parent.display()))?;
    materialize_projection(&projection, stage.path())?;
    rename_noreplace(&stage.keep(), &output)
        .with_context(|| format!("publish snapshot {}", output.display()))?;
    sync_dir(&parent)?;

    Ok(SnapshotResult {
        schema: if request.raw_preimage {
            RAW_SNAPSHOT_SCHEMA
        } else {
            SNAPSHOT_SCHEMA
        },
        status: SnapshotStatus::Created,
        catalog,
        output,
        entries: projection.entries(),
        root_sha256: projection.root_sha256,
    })
}

/// Publish one complete prepared declaration plane as an absent catalog.
pub fn bootstrap(request: BootstrapRequest) -> Result<BootstrapResult> {
    validate_sha256(&request.input_sha256)?;
    let catalog = absolute_path(&request.catalog)?;
    let parent = catalog.parent().context("catalog root has no parent")?;
    let parent = canonical_real_dir_no_alias(parent, "catalog parent")?;
    let name = catalog
        .file_name()
        .context("catalog root has no final path component")?;
    ensure_safe_component(name, "catalog root")?;
    let catalog = parent.join(name);
    let parent_file = open_dir_beneath(&parent, &parent)?;

    let prepared = canonical_real_dir_no_alias(&request.prepared, "prepared catalog")?;
    anyhow::ensure!(
        !catalog.starts_with(&prepared),
        "catalog bootstrap target is contained by prepared source: {}",
        catalog.display()
    );
    let captured = tempfile::tempdir().context("create prepared-catalog capture root")?;
    capture_prepared_catalog(&prepared, captured.path())?;
    let desired = project(captured.path(), ProjectionSource::Prepared, &catalog)?;
    anyhow::ensure!(
        desired.root_sha256 == request.input_sha256,
        "prepared catalog input sha256 mismatch: expected {}, found {}",
        request.input_sha256,
        desired.root_sha256
    );

    let admission = tempfile::tempdir().context("create prepared-catalog admission root")?;
    materialize_projection(&desired, admission.path())?;
    validate_full_catalog(admission.path())?;
    let desired_config = crate::catalog::load(admission.path())?;
    validate_external_pty_root(
        &catalog,
        desired_config.pty_root.as_deref(),
        "catalog bootstrap v1",
    )?;

    match fs::symlink_metadata(&catalog) {
        Ok(_) => {
            return inspect_existing_bootstrap(&parent_file, name, &catalog, &prepared, &desired);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect catalog bootstrap target {}", catalog.display())
            });
        }
    }

    bootstrap_checkpoint("before-stage");
    let retained_parent = retained_dir_path(&parent_file)?;
    let stage = tempfile::Builder::new()
        .prefix(".st2-catalog-bootstrap-")
        .tempdir_in(&retained_parent)
        .with_context(|| format!("create catalog bootstrap stage in {}", parent.display()))?;
    let stage = stage.keep();
    let source_name = stage
        .file_name()
        .context("bootstrap stage has no name")?
        .to_os_string();
    let staged_lock = match (|| -> Result<File> {
        materialize_projection(&desired, &stage)?;
        let staged = project(&stage, ProjectionSource::Prepared, &catalog)?;
        anyhow::ensure!(
            staged.root_sha256 == desired.root_sha256,
            "catalog bootstrap stage verification failed: expected {}, found {}",
            desired.root_sha256,
            staged.root_sha256
        );
        validate_full_catalog(&stage)?;
        let lock = initialize_bootstrap_control(&stage)?;
        sync_tree_dirs(&stage)?;
        Ok(lock)
    })() {
        Ok(lock) => lock,
        Err(error) => {
            let _ = remove_tree_at(&parent_file, &source_name);
            return Err(error);
        }
    };
    bootstrap_checkpoint("before-publish");

    match renameat_noreplace(&parent_file, &source_name, &parent_file, name) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            drop(staged_lock);
            let _ = remove_tree_at(&parent_file, &source_name);
            return inspect_existing_bootstrap(&parent_file, name, &catalog, &prepared, &desired);
        }
        Err(error) => {
            drop(staged_lock);
            let _ = remove_tree_at(&parent_file, &source_name);
            return Err(error).context("publish catalog bootstrap transaction");
        }
    }
    bootstrap_checkpoint("after-publish-before-parent-sync");
    parent_file
        .sync_all()
        .context("sync catalog parent after bootstrap publication")?;
    bootstrap_checkpoint("after-parent-sync");
    drop(staged_lock);

    Ok(BootstrapResult {
        schema: BOOTSTRAP_SCHEMA,
        status: BootstrapStatus::Created,
        catalog,
        prepared,
        entries: desired.entries(),
        root_sha256: desired.root_sha256,
    })
}

fn inspect_existing_bootstrap(
    parent: &File,
    name: &std::ffi::OsStr,
    catalog: &Path,
    prepared: &Path,
    desired: &DeclarationProjection,
) -> Result<BootstrapResult> {
    let target = openat_dir_nofollow(parent, name)
        .with_context(|| format!("open existing catalog target {}", catalog.display()))?;
    let retained_target = retained_dir_path(&target)?.join(".");
    let lock = CatalogLock::shared_existing(&retained_target)
        .context("existing catalog is not a completed bootstrap transaction")?;
    let generation = lock.generation()?;
    anyhow::ensure!(
        generation.is_some_and(|generation| generation >= 1),
        "existing catalog is missing a valid durable generation"
    );
    let retained_catalog = retained_dir_path(lock.root())?.join(".");
    validate_live_workspace_facts(&retained_catalog, &desired.workspace_dirs)?;
    let current = project_excluding(
        &retained_catalog,
        ProjectionSource::Current,
        &catalog,
        &desired.workspace_dirs,
    )?;
    validate_full_catalog(&retained_catalog)?;
    anyhow::ensure!(
        current.root_sha256 == desired.root_sha256,
        "catalog bootstrap target already exists with root sha256 {}, expected {}",
        current.root_sha256,
        desired.root_sha256
    );
    let bound_target = openat_dir_nofollow(parent, name)
        .with_context(|| format!("reopen existing catalog target {}", catalog.display()))?;
    let locked_metadata = lock.root().metadata()?;
    let bound_metadata = bound_target.metadata()?;
    anyhow::ensure!(
        locked_metadata.dev() == bound_metadata.dev()
            && locked_metadata.ino() == bound_metadata.ino(),
        "catalog bootstrap target changed while replay was validating it: {}",
        catalog.display()
    );
    parent
        .sync_all()
        .context("sync catalog parent before completing bootstrap replay")?;
    bootstrap_checkpoint("after-replay-parent-sync");
    Ok(BootstrapResult {
        schema: BOOTSTRAP_SCHEMA,
        status: BootstrapStatus::Unchanged,
        catalog: catalog.to_path_buf(),
        prepared: prepared.to_path_buf(),
        entries: desired.entries(),
        root_sha256: desired.root_sha256.clone(),
    })
}

fn initialize_bootstrap_control(stage: &Path) -> Result<File> {
    let control = stage.join(CONTROL_DIR);
    fs::create_dir(&control)
        .with_context(|| format!("create bootstrap control directory {}", control.display()))?;
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700))?;
    let control_file = File::open(&control)?;
    let lock_path = control.join(crate::catalog_lock::LOCK_FILE);
    let lock = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .with_context(|| format!("create bootstrap authoring lock {}", lock_path.display()))?;
    let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("lock staged bootstrap catalog");
    }
    lock.sync_all()?;
    let generation_path = control.join(crate::catalog_lock::GENERATION_FILE);
    let mut generation = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&generation_path)
        .with_context(|| {
            format!(
                "create bootstrap catalog generation {}",
                generation_path.display()
            )
        })?;
    generation.write_all(b"1\n")?;
    generation.sync_all()?;
    control_file.sync_all()?;
    Ok(lock)
}

fn remove_tree_at(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<()> {
    let dir = openat_dir_nofollow(parent, name)?;
    remove_dir_contents(&dir)?;
    unlinkat(parent, name, libc::AT_REMOVEDIR)
}

fn remove_dir_contents(dir: &File) -> std::io::Result<()> {
    let entries = capability_dir_entries(dir).map_err(std::io::Error::other)?;
    for name in entries {
        let entry = openat_nofollow(dir, &name).map_err(std::io::Error::other)?;
        if entry.metadata()?.is_dir() {
            remove_dir_contents(&entry)?;
            unlinkat(dir, &name, libc::AT_REMOVEDIR)?;
        } else {
            unlinkat(dir, &name, 0)?;
        }
    }
    Ok(())
}

fn unlinkat(parent: &File, name: &std::ffi::OsStr, flags: libc::c_int) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Apply a complete prepared declaration plane under one exclusive transaction.
pub fn apply(request: ApplyRequest) -> Result<ApplyResult> {
    let catalog = canonical_real_dir(&request.catalog, "catalog")?;
    let prepared_input = match request.mode {
        ApplyMode::Prepared {
            prepared,
            input_sha256,
            expect_sha256,
        } => {
            validate_sha256(&input_sha256)?;
            validate_sha256(&expect_sha256)?;
            let prepared = canonical_real_dir_no_alias(&prepared, "prepared catalog")?;
            anyhow::ensure!(
                !prepared.starts_with(&catalog),
                "prepared catalog must be outside the live catalog: {}",
                prepared.display()
            );
            let captured = tempfile::tempdir().context("create prepared-catalog capture root")?;
            capture_prepared_catalog(&prepared, captured.path())?;
            let desired = project(captured.path(), ProjectionSource::Prepared, &catalog)?;
            anyhow::ensure!(
                desired.root_sha256 == input_sha256,
                "catalog apply input precondition failed: expected sha256 {}, captured {}",
                input_sha256,
                desired.root_sha256
            );
            Some((prepared, expect_sha256, desired, false))
        }
        ApplyMode::RawPreimage {
            prepared,
            input_sha256,
            expect_sha256,
        } => {
            validate_sha256(&input_sha256)?;
            validate_sha256(&expect_sha256)?;
            let prepared = canonical_real_dir_no_alias(&prepared, "prepared catalog")?;
            anyhow::ensure!(
                !prepared.starts_with(&catalog),
                "prepared catalog must be outside the live catalog: {}",
                prepared.display()
            );
            let captured = tempfile::tempdir().context("create prepared-catalog capture root")?;
            capture_prepared_catalog(&prepared, captured.path())?;
            let desired = project(captured.path(), ProjectionSource::Prepared, &catalog)?;
            anyhow::ensure!(
                desired.root_sha256 == input_sha256,
                "catalog apply input precondition failed: expected sha256 {}, captured {}",
                input_sha256,
                desired.root_sha256
            );
            Some((prepared, expect_sha256, desired, true))
        }
        ApplyMode::Resume => None,
    };

    let lock = CatalogLock::exclusive_for_catalog_apply(&catalog)?;
    let control = retained_dir_path(lock.control())?;
    let marker_path = control.join(APPLY_MARKER);
    let existing_marker = read_marker_optional(&marker_path)?;
    let recovered = existing_marker.is_some();
    let (prepared, expect_sha256, desired, marker, raw_preimage) =
        match (prepared_input, existing_marker) {
            (Some(_), Some(_)) => {
                anyhow::bail!(
                    "catalog apply is incomplete; recover only with `catalog apply --resume`"
                )
            }
            (Some((prepared, expect_sha256, desired, raw_preimage)), None) => {
                (Some(prepared), expect_sha256, desired, None, raw_preimage)
            }
            (None, Some(marker)) => {
                validate_marker(&marker)?;
                let stage_path = control.join(&marker.stage_name);
                let staged = project(&stage_path, ProjectionSource::Prepared, &catalog)
                    .context("validate durable recovery stage")?;
                anyhow::ensure!(
                    staged.root_sha256 == marker.prepared_root_sha256,
                    "durable recovery stage hash mismatch: expected {}, found {}",
                    marker.prepared_root_sha256,
                    staged.root_sha256
                );
                let raw_preimage = marker.schema == RAW_MARKER_SCHEMA;
                (
                    None,
                    marker.expected_root_sha256.clone(),
                    staged,
                    Some(marker),
                    raw_preimage,
                )
            }
            (None, None) => {
                anyhow::bail!("catalog apply --resume requires an incomplete apply marker")
            }
        };

    if recovered {
        cleanup_writer_temporaries(&catalog)?;
    }
    validate_live_workspace_facts(&catalog, &desired.workspace_dirs)?;
    // Admission reads exact durable/captured declaration bytes. Catalog-contained workspace facts
    // are mirrored as empty directories; their live content is never copied or hashed.
    let admission = tempfile::tempdir().context("create prepared-catalog admission root")?;
    materialize_projection(&desired, admission.path())?;
    validate_full_catalog(admission.path())?;
    let desired_config = crate::catalog::load(admission.path())?;
    validate_external_pty_root(
        &catalog,
        desired_config.pty_root.as_deref(),
        "catalog apply v1",
    )?;

    let stage_name = stage_name(&desired.root_sha256);
    let stage_path = control.join(&stage_name);
    let (before_sha256, original_paths, current) = if let Some(marker) = marker {
        anyhow::ensure!(
            marker.stage_name == stage_name,
            "incomplete catalog apply stage name does not match its prepared root"
        );
        (expect_sha256.clone(), marker.original_paths, None)
    } else {
        let same_pty_root = if raw_preimage {
            true
        } else {
            let live_config = crate::catalog::load(&catalog)?;
            effective_pty_root(&catalog, live_config.pty_root.as_deref())
                == effective_pty_root(&catalog, desired_config.pty_root.as_deref())
        };
        if !raw_preimage {
            cleanup_writer_temporaries(&catalog)?;
        }
        let current = if raw_preimage {
            let current = project_raw_current(&catalog)?;
            validate_projection_link_counts(&catalog, &current, "raw live catalog")?;
            current
        } else {
            project_excluding(
                &catalog,
                ProjectionSource::Current,
                &catalog,
                &desired.workspace_dirs,
            )?
        };
        let same_declarations = if raw_preimage {
            raw_declarations_match(&catalog, &current, &desired)
        } else {
            current.root_sha256 == desired.root_sha256
        };
        if same_declarations && same_pty_root {
            return Ok(ApplyResult {
                schema: if raw_preimage {
                    RAW_APPLY_SCHEMA
                } else {
                    APPLY_SCHEMA
                },
                status: ApplyStatus::Unchanged,
                catalog,
                prepared,
                before_sha256: current.root_sha256.clone(),
                entries: desired.entries(),
                after_sha256: desired.root_sha256,
                recovered: false,
            });
        }
        anyhow::ensure!(
            same_pty_root,
            "catalog apply v1 refuses an effective pty-root change"
        );
        anyhow::ensure!(
            current.root_sha256 == expect_sha256,
            "catalog apply precondition failed: expected sha256 {}, found {}",
            expect_sha256,
            current.root_sha256
        );
        if raw_preimage {
            cleanup_writer_temporaries(&catalog)?;
        }
        let original_paths = current.files.keys().cloned().collect::<Vec<_>>();
        ensure_durable_stage(lock.control(), &catalog, &stage_name, &desired)?;
        write_marker(
            lock.control(),
            &ApplyMarker {
                schema: if raw_preimage {
                    RAW_MARKER_SCHEMA
                } else {
                    MARKER_SCHEMA
                }
                .to_string(),
                stage_name: stage_name.clone(),
                expected_root_sha256: expect_sha256.clone(),
                prepared_root_sha256: desired.root_sha256.clone(),
                original_profile_modules: current.profile_modules.iter().cloned().collect(),
                original_paths: original_paths.clone(),
            },
        )?;
        test_checkpoint("marker-created");
        (current.root_sha256.clone(), original_paths, Some(current))
    };

    let staged = project(&stage_path, ProjectionSource::Prepared, &catalog)?;
    let generation = lock.begin_generation_commit()?;
    apply_projection(
        lock.control(),
        &catalog,
        &original_paths,
        current.as_ref(),
        &staged,
    )?;
    test_checkpoint("before-verify");
    let verified = project(&catalog, ProjectionSource::Current, &catalog)?;
    anyhow::ensure!(
        verified.root_sha256 == staged.root_sha256,
        "catalog apply verification failed: expected {}, found {}",
        staged.root_sha256,
        verified.root_sha256
    );
    validate_full_catalog(&catalog).context("validate applied live catalog")?;
    sync_dir(&catalog)?;
    generation.commit()?;
    test_checkpoint("before-clear");
    fs::remove_file(&marker_path)
        .with_context(|| format!("clear catalog apply marker {}", marker_path.display()))?;
    lock.control().sync_all()?;
    let _ = fs::remove_dir_all(&stage_path);
    let _ = lock.control().sync_all();

    Ok(ApplyResult {
        schema: if raw_preimage {
            RAW_APPLY_SCHEMA
        } else {
            APPLY_SCHEMA
        },
        status: ApplyStatus::Applied,
        catalog,
        prepared,
        before_sha256,
        entries: verified.entries(),
        after_sha256: verified.root_sha256,
        recovered,
    })
}

/// Full structural and host-scoped validation for a complete prospective catalog.
pub(crate) fn validate_full_catalog(root: &Path) -> Result<()> {
    let found = crate::discover(root);
    let mut hosts = BTreeSet::new();
    for spec in &found.specs {
        let host = spec
            .host
            .as_deref()
            .context("canonical declaration is missing explicit host")?;
        hosts.insert(host.to_string());
    }
    let mut errors = BTreeSet::new();
    let report = crate::validate::validate(root);
    errors.extend(
        report
            .issues
            .iter()
            .filter(|issue| issue.severity == crate::validate::Severity::Error)
            .map(format_issue),
    );
    for host in hosts {
        let report = crate::validate::validate_for_host(root, &host);
        errors.extend(
            report
                .issues
                .iter()
                .filter(|issue| issue.severity == crate::validate::Severity::Error)
                .map(format_issue),
        );
    }
    anyhow::ensure!(
        errors.is_empty(),
        "catalog fails full validation:\n{}",
        errors.into_iter().collect::<Vec<_>>().join("\n")
    );
    Ok(())
}

fn format_issue(issue: &crate::validate::Issue) -> String {
    format!("{} [{}]: {}", issue.path, issue.code, issue.message)
}
/// Project the current declaration plane without interpreting declaration bytes.
///
/// This exists solely to bind a repair transaction to an exact opaque preimage. It deliberately
/// has no policy for whether or why those bytes are invalid. Mutable agent state is excluded by the
/// same structural boundaries as the strict projection; a prospective catalog is never admitted
/// through this path.
///
/// In particular, `catalog.kdl` is captured as raw bytes but its profile module references are not
/// resolved here. Module presence, kind, and size remain prospective admission policy; requiring
/// incumbent bytes to satisfy that policy could make their own repair preimage unreachable.
fn project_raw_current(root: &Path) -> Result<DeclarationProjection> {
    let metadata = fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "raw projection root is not a real directory: {}",
        root.display()
    );
    let mut files = BTreeMap::new();
    add_optional_regular(root, &root.join(crate::catalog::CONFIG_FILE), &mut files)?;
    let spec_paths = collect_canonical_specs(root, ProjectionSource::Current, &mut files)?;
    // Invalid declarations cannot establish typed workspace facts; canonical runtime directories
    // are excluded structurally by the shared bundle collector instead.
    let workspace_dirs = BTreeSet::new();
    for spec in &spec_paths {
        let bundle = spec.parent().context("canonical spec has no bundle")?;
        collect_bundle_files(
            root,
            bundle,
            bundle,
            ProjectionSource::Current,
            &workspace_dirs,
            &mut files,
        )?;
    }
    collect_templates(root, ProjectionSource::Current, &mut files)?;
    let root_sha256 = hash_raw_projection(&files, &workspace_dirs);
    Ok(DeclarationProjection {
        files,
        profile_modules: BTreeSet::new(),
        workspace_dirs,
        root_sha256,
    })
}

fn project(
    root: &Path,
    source: ProjectionSource,
    logical_catalog: &Path,
) -> Result<DeclarationProjection> {
    project_excluding(root, source, logical_catalog, &BTreeSet::new())
}

fn project_excluding(
    root: &Path,
    source: ProjectionSource,
    logical_catalog: &Path,
    additional_workspace_dirs: &BTreeSet<String>,
) -> Result<DeclarationProjection> {
    let metadata = fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "projection root is not a real directory: {}",
        root.display()
    );
    let mut files = BTreeMap::new();
    let profile_modules = collect_profile_modules(root, logical_catalog, &mut files)?;
    add_optional_regular(root, &root.join(crate::catalog::CONFIG_FILE), &mut files)?;
    let spec_paths = collect_canonical_specs(root, source, &mut files)?;
    let discovered = crate::discover(root);
    let mut specs = Vec::new();
    for path in &spec_paths {
        let declared = crate::discovery::parse_declared(path)
            .with_context(|| format!("parse canonical declaration {}", path.display()))?;
        anyhow::ensure!(
            declared.len() == 1,
            "canonical declaration must contain exactly one agent: {}",
            path.display()
        );
        let relative = path.strip_prefix(root)?;
        let components = normal_components(relative)?;
        let expected_host = components[1].as_str();
        let expected_identity = components[2].as_str();
        anyhow::ensure!(
            declared[0].host.as_deref() == Some(expected_host)
                && declared[0].identity.as_deref() == Some(expected_identity),
            "canonical declaration needs explicit host '{}' and identity '{}': {}",
            expected_host,
            expected_identity,
            path.display()
        );
        let matching = discovered
            .specs
            .iter()
            .filter(|spec| spec.path == *path)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matching.len() == 1,
            "canonical declaration did not lower to exactly one agent: {}",
            path.display()
        );
        specs.push(matching[0].clone());
    }

    let workspace_dirs = catalog_workspace_dirs(root, logical_catalog, &specs)?;
    let mut scan_exclusions = workspace_dirs.clone();
    scan_exclusions.extend(additional_workspace_dirs.iter().cloned());
    for spec in &specs {
        let bundle = spec.path.parent().context("canonical spec has no bundle")?;
        collect_bundle_files(root, bundle, bundle, source, &scan_exclusions, &mut files)?;
    }
    collect_templates(root, source, &mut files)?;

    for spec in &specs {
        let host = spec.host.as_deref().context("explicit host disappeared")?;
        for input in crate::materialize::catalog_owned_render_inputs(root, spec, host)? {
            let relative = normalized_relative(root, &input)?;
            let in_bundle = spec
                .path
                .parent()
                .is_some_and(|parent| input.starts_with(parent));
            anyhow::ensure!(
                in_bundle || relative.starts_with("_templates/"),
                "catalog-owned render input must be inside its agent bundle or _templates: {}",
                input.display()
            );
            add_regular(root, &input, &mut files)?;
        }
    }

    if source == ProjectionSource::Prepared {
        validate_prepared_workspace_facts(root, &workspace_dirs)?;
        reject_unprojected_entries(root, &files, &workspace_dirs)?;
    }
    let root_sha256 = hash_projection(&files, &workspace_dirs);
    Ok(DeclarationProjection {
        files,
        profile_modules,
        workspace_dirs,
        root_sha256,
    })
}

fn collect_canonical_specs(
    root: &Path,
    source: ProjectionSource,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<Vec<PathBuf>> {
    let agents = root.join("agents");
    let Some(host_entries) = read_real_dir_optional(&agents)? else {
        return Ok(Vec::new());
    };
    let mut specs = Vec::new();
    for host_entry in host_entries {
        let host_path = host_entry.path();
        ensure_safe_component(&host_entry.file_name(), "host")?;
        ensure_real_dir(&host_path, "canonical host directory")?;
        for identity_entry in sorted_entries(&host_path)? {
            if source == ProjectionSource::Current
                && crate::harness_context::is_legacy_harness_context_staging_file(&identity_entry)?
            {
                continue;
            }
            let identity_path = identity_entry.path();
            ensure_safe_component(&identity_entry.file_name(), "identity")?;
            ensure_real_dir(&identity_path, "canonical identity directory")?;
            let spec = identity_path.join("agent.kdl");
            match fs::symlink_metadata(&spec) {
                Ok(metadata) => {
                    anyhow::ensure!(
                        metadata.is_file() && !metadata.file_type().is_symlink(),
                        "canonical spec is not a real regular file: {}",
                        spec.display()
                    );
                    add_regular(root, &spec, files)?;
                    specs.push(spec);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if source == ProjectionSource::Prepared {
                        reject_state_children(&identity_path)?;
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect canonical spec {}", spec.display()));
                }
            }
        }
    }
    specs.sort();
    Ok(specs)
}

fn collect_bundle_files(
    root: &Path,
    bundle_root: &Path,
    dir: &Path,
    source: ProjectionSource,
    workspace_dirs: &BTreeSet<String>,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<()> {
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let relative = normalized_relative(root, &path)?;
        let relative_to_bundle = path.strip_prefix(bundle_root)?;
        let first = relative_to_bundle.components().next();
        let name = entry.file_name();
        let name_text = name.to_str().context("bundle path is not UTF-8")?;
        if is_writer_temporary(name_text) {
            let metadata = fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "catalog writer temporary is not a real regular file: {}",
                path.display()
            );
            anyhow::ensure!(
                source == ProjectionSource::Current,
                "prepared catalog contains a reserved writer temporary: {}",
                path.display()
            );
            continue;
        }
        let canonical_workspace = first.is_some()
            && relative_to_bundle.components().count() == 1
            && name_text == ".workspace";
        if canonical_workspace || workspace_dirs.contains(&relative) {
            continue;
        }
        let state = matches!(
            name_text,
            "resources"
                | "archive"
                | "inbox"
                | "status"
                | "harness-context"
                | "harness-state"
                | ".harness-state.seq"
                | ".harness-state.lock"
                | ".harness-context.lock"
        ) || name_text.starts_with(".status.tmp-");
        if first.is_some() && relative_to_bundle.components().count() == 1 && state {
            if source == ProjectionSource::Prepared {
                anyhow::bail!(
                    "prepared catalog contains state-plane path: {}",
                    path.display()
                );
            }
            continue;
        }
        anyhow::ensure!(
            !matches!(name_text, ".git" | ".st2"),
            "bundle contains reserved control path: {}",
            path.display()
        );
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_bundle_files(root, bundle_root, &path, source, workspace_dirs, files)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            add_regular(root, &path, files)?;
        } else {
            anyhow::bail!(
                "catalog declaration plane contains symlink or special entry: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn catalog_workspace_dirs(
    root: &Path,
    logical_catalog: &Path,
    specs: &[agent_spec::spec::AgentSpec],
) -> Result<BTreeSet<String>> {
    let mut facts = BTreeSet::new();
    for spec in specs {
        let bundle = spec
            .path
            .parent()
            .context("canonical spec has no bundle directory")?;
        let bundle_relative = normalized_relative(root, bundle)?;
        let logical_bundle = logical_catalog.join(&bundle_relative);
        let expected = format!("{bundle_relative}/.workspace");
        let paths = spec
            .workspace
            .iter()
            .map(String::as_str)
            .chain(spec.tasks.iter().filter_map(|task| task.cwd.as_deref()));
        for raw in paths {
            let resolved = crate::expand::resolve_spec_path(raw, logical_catalog, &logical_bundle)?;
            if resolved.starts_with(logical_catalog) {
                let relative = normalized_relative(logical_catalog, &resolved)?;
                anyhow::ensure!(
                    relative == expected,
                    "catalog-contained workspace/cwd must use canonical {}: {}",
                    expected,
                    resolved.display()
                );
                facts.insert(relative);
            }
        }
    }
    Ok(facts)
}

/// Admit the prepared plane's canonical workspace facts.
///
/// `.workspace` is a runtime-only directory: the agent creates it, nothing declares its contents,
/// and a raw preimage deliberately captures none of them. A prepared plane built from such a
/// preimage therefore legitimately has no directory for a declared fact, and demanding one made
/// every repair transaction over a raw preimage unreachable. The fact stays in the projection
/// either way — `materialize_projection` publishes it as an empty directory — so the applied
/// catalog still satisfies the live invariant that a declared workspace fact is present.
///
/// A directory that IS there still has to be a real, empty one: prepared state may not smuggle
/// runtime bytes through the declaration plane.
fn validate_prepared_workspace_facts(root: &Path, facts: &BTreeSet<String>) -> Result<()> {
    for relative in facts {
        let path = root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                ensure_real_dir_chain_present(root, &path, "prepared workspace fact")?;
                anyhow::ensure!(
                    sorted_entries(&path)?.is_empty(),
                    "prepared workspace fact must be an empty directory: {}",
                    path.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect prepared workspace fact {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn collect_templates(
    root: &Path,
    source: ProjectionSource,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<()> {
    let templates = root.join("_templates");
    let Some(_) = read_real_dir_optional(&templates)? else {
        return Ok(());
    };
    let mut count = 0usize;
    let mut total = 0u64;
    collect_template_dir(
        root, &templates, &templates, source, 0, &mut count, &mut total, files,
    )
}

fn collect_template_dir(
    root: &Path,
    templates: &Path,
    dir: &Path,
    source: ProjectionSource,
    depth: usize,
    count: &mut usize,
    total: &mut u64,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<()> {
    anyhow::ensure!(
        depth <= TEMPLATE_MAX_DEPTH,
        "_templates exceeds maximum depth {TEMPLATE_MAX_DEPTH}"
    );
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().context("_templates path is not UTF-8")?;
        if is_writer_temporary(name) {
            let metadata = fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "catalog writer temporary is not a real regular file: {}",
                path.display()
            );
            anyhow::ensure!(
                source == ProjectionSource::Current,
                "prepared catalog contains a reserved writer temporary: {}",
                path.display()
            );
            continue;
        }
        anyhow::ensure!(
            !matches!(
                name,
                ".git" | ".st2" | "pty" | "resources" | "archive" | "inbox" | "status"
            ),
            "_templates contains reserved control/state path: {}",
            path.display()
        );
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_template_dir(
                root,
                templates,
                &path,
                source,
                depth + 1,
                count,
                total,
                files,
            )?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            let relative_depth = path.strip_prefix(templates)?.components().count();
            anyhow::ensure!(
                relative_depth <= TEMPLATE_MAX_DEPTH,
                "_templates file exceeds maximum depth {TEMPLATE_MAX_DEPTH}: {}",
                path.display()
            );
            anyhow::ensure!(
                metadata.nlink() == 1,
                "_templates file must not be hard-linked: {}",
                path.display()
            );
            anyhow::ensure!(
                metadata.len() <= TEMPLATE_MAX_FILE_BYTES,
                "_templates file exceeds {TEMPLATE_MAX_FILE_BYTES} bytes: {}",
                path.display()
            );
            *count += 1;
            *total = total.saturating_add(metadata.len());
            anyhow::ensure!(
                *count <= TEMPLATE_MAX_FILES,
                "_templates exceeds {TEMPLATE_MAX_FILES} files"
            );
            anyhow::ensure!(
                *total <= TEMPLATE_MAX_TOTAL_BYTES,
                "_templates exceeds {TEMPLATE_MAX_TOTAL_BYTES} total bytes"
            );
            add_regular(root, &path, files)?;
        } else {
            anyhow::bail!(
                "_templates contains a symlink or special entry: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn reject_state_children(identity_path: &Path) -> Result<()> {
    for entry in sorted_entries(identity_path)? {
        let name = entry.file_name();
        let name = name.to_str().context("identity path is not UTF-8")?;
        if matches!(name, "resources" | "archive" | "inbox" | "status")
            || name.starts_with(".status.tmp-")
        {
            anyhow::bail!(
                "prepared catalog contains state-plane path: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn add_optional_regular(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => add_regular(root, path, files),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}
pub(crate) fn validate_catalog_profile_modules(root: &Path) -> Result<()> {
    collect_profile_modules(root, root, &mut BTreeMap::new()).map(|_| ())
}

fn collect_profile_modules(
    root: &Path,
    logical_catalog: &Path,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<BTreeSet<String>> {
    let config = crate::catalog::load(root).context("parse catalog profile modules")?;
    let mut modules = BTreeSet::new();
    for profile in config.profiles {
        if let crate::catalog::ResolvedProfileModule::CatalogRelative(relative) =
            crate::catalog::resolve_profile_module(logical_catalog, &profile.wasm)?
        {
            let normalized = add_profile_module(root, &relative, files).with_context(|| {
                format!(
                    "admit catalog-relative profile module '{}' for scheme '{}'",
                    relative.display(),
                    profile.scheme
                )
            })?;
            modules.insert(normalized);
        }
        if let Some(runtime) = &profile.runtime {
            let relative =
                crate::catalog::resolve_provider_component(logical_catalog, &runtime.component)?;
            let normalized = add_profile_module(root, &relative, files).with_context(|| {
                format!(
                    "admit provider component '{}' for scheme '{}'",
                    relative.display(),
                    profile.scheme
                )
            })?;
            modules.insert(normalized);
        }
    }
    Ok(modules)
}

fn add_profile_module(
    root: &Path,
    relative: &Path,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<String> {
    let normalized = normal_components(relative)?.join("/");
    anyhow::ensure!(
        !normalized.is_empty(),
        "profile module cannot name the catalog root"
    );
    if files.contains_key(&normalized) {
        return Ok(normalized);
    }

    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = open_dir_beneath(root, &root.join(parent_relative))
        .with_context(|| format!("open profile module parent {}", parent_relative.display()))?;
    let name = relative
        .file_name()
        .context("profile module has no file name")?;
    let input = openat_nofollow(&parent, name)
        .with_context(|| format!("open profile module {}", relative.display()))?;
    let metadata = input.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "profile module is not a no-follow regular file: {}",
        relative.display()
    );
    anyhow::ensure!(
        metadata.nlink() == 1,
        "profile module is hard-linked: {}",
        relative.display()
    );
    let limit = agent_spec::profile::DEFAULT_MODULE_LIMIT_BYTES;
    anyhow::ensure!(
        metadata.len() <= limit as u64,
        "profile module exceeds the {limit}-byte limit: {}",
        relative.display()
    );
    let executable = metadata.permissions().mode() & 0o111 != 0;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read profile module {}", relative.display()))?;
    anyhow::ensure!(
        bytes.len() <= limit,
        "profile module exceeds the {limit}-byte limit: {}",
        relative.display()
    );
    files.insert(normalized.clone(), ProjectedFile { bytes, executable });
    Ok(normalized)
}

pub(crate) fn read_provider_component(root: &Path, relative: &Path) -> Result<Vec<u8>> {
    let mut files = BTreeMap::new();
    let normalized = add_profile_module(root, relative, &mut files)?;
    files
        .remove(&normalized)
        .map(|file| file.bytes)
        .context("admitted provider component disappeared")
}

fn add_regular(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, ProjectedFile>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "declaration input is not a real regular file: {}",
        path.display()
    );
    let relative = normalized_relative(root, path)?;
    let bytes =
        fs::read(path).with_context(|| format!("read declaration input {}", path.display()))?;
    let executable = metadata.permissions().mode() & 0o111 != 0;
    files.insert(relative, ProjectedFile { bytes, executable });
    Ok(())
}

fn reject_unprojected_entries(
    root: &Path,
    files: &BTreeMap<String, ProjectedFile>,
    workspace_dirs: &BTreeSet<String>,
) -> Result<()> {
    let mut allowed_dirs = BTreeSet::new();
    for path in files.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(value) = parent {
            if value.as_os_str().is_empty() {
                break;
            }
            allowed_dirs.insert(value.to_string_lossy().to_string());
            parent = value.parent();
        }
    }
    for path in workspace_dirs {
        let mut value = Some(Path::new(path));
        while let Some(dir) = value {
            if dir.as_os_str().is_empty() {
                break;
            }
            allowed_dirs.insert(dir.to_string_lossy().to_string());
            value = dir.parent();
        }
    }
    reject_unprojected_recursive(root, root, files, &allowed_dirs)
}

fn reject_unprojected_recursive(
    root: &Path,
    dir: &Path,
    files: &BTreeMap<String, ProjectedFile>,
    allowed_dirs: &BTreeSet<String>,
) -> Result<()> {
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let relative = normalized_relative(root, &path)?;
        let metadata = fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "prepared catalog contains a symlink: {}",
            path.display()
        );
        if metadata.is_dir() {
            anyhow::ensure!(
                allowed_dirs.contains(&relative),
                "prepared catalog contains unprojected directory: {}",
                path.display()
            );
            reject_unprojected_recursive(root, &path, files, allowed_dirs)?;
        } else if metadata.is_file() {
            anyhow::ensure!(
                files.contains_key(&relative),
                "prepared catalog contains unprojected file: {}",
                path.display()
            );
        } else {
            anyhow::bail!(
                "prepared catalog contains a special entry: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn hash_projection(
    files: &BTreeMap<String, ProjectedFile>,
    workspace_dirs: &BTreeSet<String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    for (path, file) in files {
        hasher.update([1]);
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update([u8::from(file.executable)]);
        hasher.update((file.bytes.len() as u64).to_be_bytes());
        hasher.update(&file.bytes);
    }
    for path in workspace_dirs {
        hasher.update([2]);
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn hash_raw_projection(
    files: &BTreeMap<String, ProjectedFile>,
    workspace_dirs: &BTreeSet<String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(RAW_HASH_DOMAIN);
    for (path, file) in files {
        hasher.update([1]);
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update([u8::from(file.executable)]);
        hasher.update((file.bytes.len() as u64).to_be_bytes());
        hasher.update(&file.bytes);
    }
    for path in workspace_dirs {
        hasher.update([2]);
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn materialize_projection(projection: &DeclarationProjection, root: &Path) -> Result<()> {
    for (relative, file) in &projection.files {
        let target = root.join(relative);
        let parent = target.parent().context("projected file has no parent")?;
        ensure_real_dir_chain(root, parent)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(if file.executable { 0o755 } else { 0o644 })
            .open(&target)
            .with_context(|| format!("create projected file {}", target.display()))?;
        output.write_all(&file.bytes)?;
        output.sync_all()?;
    }
    for relative in &projection.workspace_dirs {
        ensure_real_dir_chain(root, &root.join(relative))?;
    }
    sync_tree_dirs(root)
}

fn ensure_durable_stage(
    control_file: &File,
    catalog: &Path,
    stage_name: &str,
    desired: &DeclarationProjection,
) -> Result<()> {
    let control = retained_dir_path(control_file)?;
    let stage_path = control.join(stage_name);
    match fs::symlink_metadata(&stage_path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "catalog apply stage is not a real directory: {}",
                stage_path.display()
            );
            let existing = project(&stage_path, ProjectionSource::Prepared, catalog)?;
            anyhow::ensure!(
                existing.root_sha256 == desired.root_sha256
                    && existing.workspace_dirs == desired.workspace_dirs,
                "existing catalog apply stage has unexpected root sha256"
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let stage = tempfile::Builder::new()
        .prefix(".catalog-apply-stage-")
        .tempdir_in(&control)?;
    materialize_projection(desired, stage.path())?;
    let source = stage.keep();
    renameat_noreplace(
        control_file,
        source.file_name().context("stage has no name")?,
        control_file,
        std::ffi::OsStr::new(stage_name),
    )?;
    control_file.sync_all().map_err(Into::into)
}

fn is_writer_temporary(name: &str) -> bool {
    WRITER_TEMP_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn cleanup_writer_temporaries(catalog: &Path) -> Result<()> {
    cleanup_writer_temporaries_in(catalog, false, false)?;
    let agents = catalog.join("agents");
    if let Some(hosts) = read_real_dir_optional(&agents)? {
        for host in hosts {
            ensure_safe_component(&host.file_name(), "host")?;
            ensure_real_dir(&host.path(), "canonical host directory")?;
            for identity in sorted_entries(&host.path())? {
                if crate::harness_context::is_legacy_harness_context_staging_file(&identity)? {
                    continue;
                }
                ensure_safe_component(&identity.file_name(), "identity")?;
                ensure_real_dir(&identity.path(), "canonical identity directory")?;
                cleanup_writer_temporaries_in(&identity.path(), true, true)?;
            }
        }
    }
    let templates = catalog.join("_templates");
    if read_real_dir_optional(&templates)?.is_some() {
        cleanup_writer_temporaries_in(&templates, true, false)?;
    }
    Ok(())
}

fn cleanup_writer_temporaries_in(dir: &Path, recursive: bool, identity_root: bool) -> Result<()> {
    for entry in sorted_entries(dir)? {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("catalog path is not UTF-8: {}", path.display()))?;
        let metadata = fs::symlink_metadata(&path)?;
        if is_writer_temporary(&name) {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "catalog writer temporary is not a real regular file: {}",
                path.display()
            );
            fs::remove_file(&path).with_context(|| {
                format!("remove stale catalog writer temporary {}", path.display())
            })?;
            sync_dir(dir)?;
            continue;
        }
        if !recursive || !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        if identity_root
            && matches!(
                name.as_str(),
                ".workspace" | "resources" | "archive" | "inbox"
            )
        {
            continue;
        }
        cleanup_writer_temporaries_in(&path, true, false)?;
    }
    Ok(())
}

fn apply_projection(
    control: &File,
    catalog: &Path,
    original_paths: &[String],
    current: Option<&DeclarationProjection>,
    desired: &DeclarationProjection,
) -> Result<()> {
    let atomically_created = create_new_identity_bundles(control, catalog, desired)?;
    // Publish every referenced input before catalog.kdl. A crash can therefore leave an
    // unreferenced new module behind, but never a new catalog declaration pointing at a missing
    // module. Stale inputs are removed only after the declaration stops naming them.
    for publish_catalog_config in [false, true] {
        for (path, desired_file) in desired.files.iter().filter(|(path, _)| {
            (path.as_str() == crate::catalog::CONFIG_FILE) == publish_catalog_config
        }) {
            if current.is_some_and(|current| current.files.get(path) == Some(desired_file))
                || atomically_created
                    .iter()
                    .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
            {
                continue;
            }
            atomic_replace_file(control, catalog, path, desired_file)?;
            test_checkpoint("mid-write");
        }
    }

    let mut stale = original_paths
        .iter()
        .filter(|path| !desired.files.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    stale.sort_by_key(|path| {
        (
            path.as_str() != crate::catalog::CONFIG_FILE,
            is_canonical_agent_spec(path),
            path.clone(),
        )
    });
    for path in stale {
        let target = catalog.join(&path);
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "stale declaration is not a real regular file: {}",
            target.display()
        );
        remove_file_beneath(catalog, &target)
            .with_context(|| format!("remove stale declaration {}", target.display()))?;
        if is_canonical_agent_spec(&path) {
            test_checkpoint("deleted-spec");
        }
        test_checkpoint("mid-delete");
    }
    Ok(())
}

fn is_canonical_agent_spec(path: &str) -> bool {
    let components = path.split('/').collect::<Vec<_>>();
    components.len() == 4 && components[0] == "agents" && components[3] == "agent.kdl"
}

fn create_new_identity_bundles(
    control_file: &File,
    catalog: &Path,
    desired: &DeclarationProjection,
) -> Result<BTreeSet<String>> {
    let mut created = BTreeSet::new();
    let control = retained_dir_path(control_file)?;
    for path in desired.files.keys() {
        let components = path.split('/').collect::<Vec<_>>();
        if components.len() != 4 || components[0] != "agents" || components[3] != "agent.kdl" {
            continue;
        }
        let prefix = components[..3].join("/");
        let target = catalog.join(&prefix);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "canonical identity path is not a real directory: {}",
                    target.display()
                );
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let host = target
            .parent()
            .context("identity target has no host parent")?;
        ensure_real_dir_chain(catalog, host)?;
        let stage = tempfile::Builder::new()
            .prefix("catalog-apply-identity-")
            .tempdir_in(&control)?;
        fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o755))?;
        for (candidate, file) in &desired.files {
            let Some(relative) = candidate.strip_prefix(&format!("{prefix}/")) else {
                continue;
            };
            let destination = stage.path().join(relative);
            let parent = destination.parent().context("bundle file has no parent")?;
            ensure_real_dir_chain(stage.path(), parent)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(if file.executable { 0o755 } else { 0o644 })
                .open(&destination)?;
            output.write_all(&file.bytes)?;
            output.sync_all()?;
        }
        sync_tree_dirs(stage.path())?;
        test_checkpoint("identity-staged");
        test_forced_cross_device("identity-staged").map_err(control_plane_rename_error)?;
        rename_noreplace_between_dirs(control_file, catalog, stage.path(), &target)
            .map_err(control_plane_rename_error)
            .with_context(|| format!("publish canonical identity bundle {}", target.display()))?;
        sync_dir(host)?;
        created.insert(prefix);
        test_checkpoint("mid-write");
    }
    Ok(created)
}

fn atomic_replace_file(
    control_file: &File,
    catalog: &Path,
    relative: &str,
    file: &ProjectedFile,
) -> Result<()> {
    let target = catalog.join(relative);
    let parent = target
        .parent()
        .context("declaration target has no parent")?;
    ensure_real_dir_chain(catalog, parent)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "declaration target is not a real regular file: {}",
            target.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let control = retained_dir_path(control_file)?;
    let mut temp = tempfile::Builder::new()
        .prefix("catalog-apply-leaf-")
        .tempfile_in(&control)?;
    temp.as_file_mut()
        .set_permissions(fs::Permissions::from_mode(if file.executable {
            0o755
        } else {
            0o644
        }))?;
    temp.write_all(&file.bytes)?;
    temp.as_file().sync_all()?;
    test_checkpoint("leaf-staged");
    test_forced_cross_device("leaf-staged").map_err(control_plane_rename_error)?;
    persist_tempfile_from_control(control_file, catalog, temp, &target)
        .map_err(control_plane_rename_error)
        .with_context(|| format!("replace declaration {}", target.display()))?;
    open_dir_beneath(catalog, parent)?
        .sync_all()
        .map_err(Into::into)
}

fn remove_file_beneath(catalog: &Path, target: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let parent = open_dir_beneath(
        catalog,
        target.parent().context("target has no parent directory")?,
    )?;
    let name = CString::new(
        target
            .file_name()
            .context("target has no final component")?
            .as_bytes(),
    )?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    Ok(())
}

fn write_marker(control_file: &File, marker: &ApplyMarker) -> Result<()> {
    let control = retained_dir_path(control_file)?;
    let target = control.join(APPLY_MARKER);
    let mut bytes = serde_json::to_vec(marker)?;
    bytes.push(b'\n');
    let mut temp = tempfile::Builder::new()
        .prefix(".catalog-apply-incomplete-")
        .tempfile_in(&control)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    fs::hard_link(temp.path(), &target)
        .with_context(|| format!("publish catalog apply marker {}", target.display()))?;
    temp.close()?;
    control_file.sync_all().map_err(Into::into)
}

fn read_marker_optional(path: &Path) -> Result<Option<ApplyMarker>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "catalog apply marker is not a real regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let marker = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse catalog apply marker {}", path.display()))?;
    Ok(Some(marker))
}

fn validate_marker(marker: &ApplyMarker) -> Result<()> {
    anyhow::ensure!(
        matches!(marker.schema.as_str(), MARKER_SCHEMA | RAW_MARKER_SCHEMA),
        "unsupported catalog apply marker schema '{}'",
        marker.schema
    );
    validate_sha256(&marker.prepared_root_sha256)?;
    validate_sha256(&marker.expected_root_sha256)?;
    anyhow::ensure!(
        marker
            .original_paths
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "catalog apply marker originalPaths must be strictly sorted and unique"
    );
    anyhow::ensure!(
        marker
            .original_profile_modules
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "catalog apply marker originalProfileModules must be strictly sorted and unique"
    );
    let profile_modules = marker
        .original_profile_modules
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for path in &marker.original_profile_modules {
        let components = normal_components(Path::new(path))?;
        anyhow::ensure!(
            !components.is_empty() && components.join("/") == *path,
            "catalog apply marker contains a non-canonical profile module path"
        );
        crate::catalog::validate_catalog_relative_profile_module_path(Path::new(path))
            .context("catalog apply marker contains a reserved profile module path")?;
        anyhow::ensure!(
            marker.original_paths.binary_search(path).is_ok(),
            "catalog apply marker profile module is absent from originalPaths"
        );
    }
    for path in &marker.original_paths {
        if !profile_modules.contains(path.as_str()) {
            validate_declaration_leaf_path(path)?;
        }
    }
    let components = normal_components(Path::new(&marker.stage_name))?;
    anyhow::ensure!(
        components.len() == 1 && marker.stage_name == stage_name(&marker.prepared_root_sha256),
        "catalog apply marker has an unsafe or non-canonical stage name"
    );
    Ok(())
}

fn validate_declaration_leaf_path(path: &str) -> Result<()> {
    let components = path.split('/').collect::<Vec<_>>();
    anyhow::ensure!(
        !components.is_empty()
            && components
                .iter()
                .all(|component| !component.is_empty() && !matches!(*component, "." | "..")),
        "catalog apply marker contains an unsafe declaration path"
    );
    anyhow::ensure!(
        !components
            .iter()
            .any(|component| matches!(*component, ".git" | ".st2"))
            && components.first().copied() != Some("pty"),
        "catalog apply marker contains a reserved declaration path"
    );
    let canonical_bundle = components.len() >= 4 && components[0] == "agents";
    let template = components.len() >= 2 && components[0] == "_templates";
    anyhow::ensure!(
        path == crate::catalog::CONFIG_FILE || canonical_bundle || template,
        "catalog apply marker contains an unowned declaration path"
    );
    if canonical_bundle {
        anyhow::ensure!(
            !matches!(
                components[3],
                ".workspace" | "resources" | "archive" | "inbox" | "status"
            ) && !components[3].starts_with(".status.tmp-"),
            "catalog apply marker contains a workspace or state-plane path"
        );
    }
    Ok(())
}

fn effective_pty_root(live_catalog: &Path, declared_pty_root: Option<&str>) -> PathBuf {
    match declared_pty_root {
        Some(declared) => live_catalog.join(crate::expand::expand_catalog(declared, live_catalog)),
        None => live_catalog.join("pty"),
    }
}

fn validate_live_workspace_facts(catalog: &Path, facts: &BTreeSet<String>) -> Result<()> {
    for relative in facts {
        let path = catalog.join(relative);
        ensure_real_dir_chain_present(catalog, &path, "live workspace fact")?;
    }
    Ok(())
}

fn validate_external_pty_root(
    catalog: &Path,
    declared_pty_root: Option<&str>,
    operation: &str,
) -> Result<()> {
    anyhow::ensure!(
        declared_pty_root.is_some(),
        "{operation} requires an explicit external pty-root"
    );
    let pty_root = lexical_absolute(&effective_pty_root(catalog, declared_pty_root))?;
    anyhow::ensure!(
        !pty_root.starts_with(catalog),
        "{operation} requires pty-root outside the catalog: {}",
        pty_root.display()
    );
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "path is not absolute: {}",
        path.display()
    );
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
            Component::Prefix(_) => anyhow::bail!("unsupported path prefix: {}", path.display()),
        }
    }
    Ok(normalized)
}

fn stage_name(root_sha256: &str) -> String {
    format!("{STAGE_PREFIX}{root_sha256}")
}

fn validate_sha256(value: &str) -> Result<()> {
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected sha256 must be exactly 64 hexadecimal characters"
    );
    anyhow::ensure!(
        value.bytes().all(|byte| !byte.is_ascii_uppercase()),
        "expected sha256 must use lowercase hexadecimal"
    );
    Ok(())
}

fn canonical_real_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    ensure_real_dir(&canonical, label)?;
    Ok(canonical)
}

fn canonical_real_dir_no_alias(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} is not a real directory: {}",
        path.display()
    );
    canonical_real_dir(path, label)
}

fn ensure_real_dir(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} is not a real directory: {}",
        path.display()
    );
    Ok(())
}

fn read_real_dir_optional(path: &Path) -> Result<Option<Vec<fs::DirEntry>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "catalog path is not a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(sorted_entries(path)?))
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn ensure_safe_component(value: &std::ffi::OsStr, label: &str) -> Result<()> {
    let value = value
        .to_str()
        .with_context(|| format!("{label} is not UTF-8"))?;
    anyhow::ensure!(
        !value.is_empty() && !matches!(value, "." | ".." | ".git" | ".st2"),
        "{label} is not one safe path component"
    );
    Ok(())
}

fn normalized_relative(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("path escapes projection root: {}", path.display()))?;
    let components = normal_components(relative)?;
    anyhow::ensure!(!components.is_empty(), "projection cannot contain its root");
    Ok(components.join("/"))
}

fn normal_components(path: &Path) -> Result<Vec<String>> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(String::from)
                .context("catalog path is not UTF-8"),
            _ => anyhow::bail!("catalog path contains an unsafe component"),
        })
        .collect()
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn ensure_real_dir_chain(root: &Path, target: &Path) -> Result<()> {
    let relative = target
        .strip_prefix(root)
        .context("directory escapes root")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("directory contains an unsafe path component");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "path is not a real directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
                sync_dir(
                    current
                        .parent()
                        .context("created directory has no parent")?,
                )?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn ensure_real_dir_chain_present(root: &Path, target: &Path, label: &str) -> Result<()> {
    let relative = target
        .strip_prefix(root)
        .with_context(|| format!("{label} escapes catalog root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("{label} contains an unsafe path component");
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspect {label} {}", current.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{label} is not a real directory: {}",
            current.display()
        );
    }
    Ok(())
}

fn sync_tree_dirs(root: &Path) -> Result<()> {
    let mut dirs = Vec::new();
    collect_dirs(root, &mut dirs)?;
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        sync_dir(&dir)?;
    }
    Ok(())
}

fn collect_dirs(root: &Path, dirs: &mut Vec<PathBuf>) -> Result<()> {
    dirs.push(root.to_path_buf());
    for entry in sorted_entries(root)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_dirs(&path, dirs)?;
        }
    }
    Ok(())
}

/// Copy an exact directory capability without following a source symlink at any depth.
pub(crate) fn capture_real_tree(source: &Path, destination: &Path) -> Result<()> {
    capture_tree(source, destination, CaptureMode::General)
}

fn capture_prepared_catalog(source: &Path, destination: &Path) -> Result<()> {
    capture_tree(source, destination, CaptureMode::PreparedCatalog)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    General,
    PreparedCatalog,
}

fn capture_tree(source: &Path, destination: &Path, mode: CaptureMode) -> Result<()> {
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(source)
        .context("open retained source directory")?;
    if mode == CaptureMode::PreparedCatalog {
        prepared_capture_checkpoint("source-opened");
    }
    let source_root = retained_dir_path(&source)?
        .canonicalize()
        .context("resolve retained source directory")?;
    let destination_root = destination
        .canonicalize()
        .context("resolve capture destination directory")?;
    anyhow::ensure!(
        !destination_root.starts_with(&source_root),
        "capture destination {} is contained by source {}",
        destination_root.display(),
        source_root.display()
    );
    capture_dir_capability(&source, destination, destination, mode)?;
    sync_tree_dirs(destination)
}

#[cfg(debug_assertions)]
fn prepared_capture_checkpoint(point: &str) {
    if std::env::var("ST2_TEST_PREPARED_CAPTURE_PAUSE_AT").as_deref() == Ok(point)
        && let (Ok(ready), Ok(release)) = (
            std::env::var("ST2_TEST_PREPARED_CAPTURE_READY"),
            std::env::var("ST2_TEST_PREPARED_CAPTURE_RELEASE"),
        )
    {
        let _ = fs::write(&ready, point);
        while !Path::new(&release).exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

#[cfg(not(debug_assertions))]
fn prepared_capture_checkpoint(_point: &str) {}

/// A path naming the directory `dir` is open on, which callers may **join a child component onto**.
/// That last part is the contract — every call site appends to the result — and it is what makes the
/// two platforms need different mechanisms rather than symmetrical-looking strings.
pub(crate) fn retained_dir_path(dir: &File) -> Result<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // A symlink to the pathname, so a child component resolves through it.
        Ok(PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())))
    }
    #[cfg(target_os = "macos")]
    {
        // `/dev/fd/N` looks like the Linux form but is not the same kind of thing: it names the open
        // file itself, not a portal into the directory, so `/dev/fd/N/child` does not resolve and
        // every caller that joins gets ENOENT. `F_GETPATH` is the macOS counterpart to reading the
        // `/proc/self/fd` symlink — it recovers the descriptor's pathname, which can be joined onto.
        //
        // The pathname is resolved at call time rather than staying bound to the descriptor. That
        // difference is only observable if the directory is renamed between the open and the join;
        // the call site holding the result longest already `canonicalize()`s it, which discards the
        // descriptor binding on Linux too.
        let mut buffer = [0 as libc::c_char; libc::PATH_MAX as usize];
        // SAFETY: `buffer` is PATH_MAX bytes, which is what F_GETPATH requires, and `dir` owns a
        // valid descriptor for the duration of the call.
        let result = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
        if result == -1 {
            return Err(std::io::Error::last_os_error())
                .context("resolve the pathname of a retained directory descriptor");
        }
        // SAFETY: on success F_GETPATH wrote a NUL-terminated pathname into `buffer`.
        let path = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
        Ok(PathBuf::from(
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path.to_bytes()),
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = dir;
        anyhow::bail!("retained-directory capture is unsupported on this platform")
    }
}

fn capture_dir_capability(
    source: &File,
    destination: &Path,
    capture_root: &Path,
    mode: CaptureMode,
) -> Result<()> {
    for name in capability_dir_entries(source)? {
        let mut input = openat_nofollow(source, &name)?;
        let metadata = input.metadata()?;
        let target = destination.join(&name);
        if metadata.is_dir() {
            let relative = target.strip_prefix(capture_root)?;
            if mode == CaptureMode::PreparedCatalog && is_canonical_workspace_fact(relative) {
                anyhow::ensure!(
                    capability_dir_entries(&input)?.is_empty(),
                    "prepared workspace fact must be empty: {}",
                    relative.display()
                );
                fs::create_dir(&target)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                continue;
            }
            fs::create_dir(&target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
            capture_dir_capability(&input, &target, capture_root, mode)?;
        } else if metadata.is_file() {
            let relative = target.strip_prefix(capture_root)?;
            if mode == CaptureMode::PreparedCatalog {
                anyhow::ensure!(
                    metadata.nlink() == 1,
                    "prepared catalog contains a hard-linked file: {}",
                    target.display()
                );
            }
            if relative.components().next().and_then(|value| match value {
                Component::Normal(name) => name.to_str(),
                _ => None,
            }) == Some("_templates")
            {
                anyhow::ensure!(
                    metadata.nlink() == 1,
                    "_templates contains a hard-linked file: {}",
                    target.display()
                );
            }
            let executable = metadata.permissions().mode() & 0o111 != 0;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(if executable { 0o755 } else { 0o644 })
                .open(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        } else {
            anyhow::bail!(
                "source tree contains a symlink or special entry: {}",
                target.display()
            );
        }
    }
    Ok(())
}

fn is_canonical_workspace_fact(path: &Path) -> bool {
    let components = path.components().collect::<Vec<_>>();
    components.len() == 4
        && matches!(components[0], Component::Normal(name) if name == "agents")
        && matches!(components[3], Component::Normal(name) if name == ".workspace")
}

mod capability;
pub(crate) use capability::*;

#[cfg(debug_assertions)]
fn bootstrap_checkpoint(point: &str) {
    if std::env::var("ST2_TEST_CATALOG_BOOTSTRAP_PAUSE_AT").as_deref() == Ok(point)
        && let (Ok(ready), Ok(release)) = (
            std::env::var("ST2_TEST_CATALOG_BOOTSTRAP_READY"),
            std::env::var("ST2_TEST_CATALOG_BOOTSTRAP_RELEASE"),
        )
    {
        let _ = fs::write(&ready, point);
        while !Path::new(&release).exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    if std::env::var("ST2_TEST_CATALOG_BOOTSTRAP_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn bootstrap_checkpoint(_point: &str) {}

#[cfg(debug_assertions)]
fn test_checkpoint(point: &str) {
    if std::env::var("ST2_TEST_CATALOG_APPLY_PAUSE_AT").as_deref() == Ok(point)
        && let (Ok(ready), Ok(release)) = (
            std::env::var("ST2_TEST_CATALOG_APPLY_READY"),
            std::env::var("ST2_TEST_CATALOG_APPLY_RELEASE"),
        )
    {
        let _ = fs::write(&ready, point);
        while !Path::new(&release).exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    if std::env::var("ST2_TEST_CATALOG_APPLY_CRASH_AT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn test_checkpoint(_point: &str) {}

#[cfg(debug_assertions)]
fn test_forced_cross_device(point: &str) -> std::io::Result<()> {
    if std::env::var("ST2_TEST_CATALOG_APPLY_EXDEV_AT").as_deref() == Ok(point) {
        return Err(std::io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_forced_cross_device(_point: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_streams_participate_in_semantic_catalog_projection() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agents/host/worker");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.kdl"),
            "agent \"worker\" {\n  host \"host\"\n  command \"worker\"\n  stream \"webhook\" {}\n}\n",
        )
        .unwrap();
        let discovered = agent_spec::discover_strict(root.path());
        assert!(discovered.errors.is_empty(), "{:?}", discovered.errors);

        let fields = normalize_agent(&discovered.specs[0]).unwrap();

        let atom = fields
            .get("/agents/host/worker/streams/webhook/launch")
            .expect("external stream must affect semantic projection");
        assert_eq!(atom, &present_atom(SemanticType::String, "external"));
    }

    #[test]
    fn workspace_directory_facts_are_typed_into_the_projection_hash() {
        let files = BTreeMap::new();
        let no_facts = BTreeSet::new();
        let facts = BTreeSet::from(["agents/host/worker/.workspace".to_string()]);
        assert_ne!(
            hash_projection(&files, &no_facts),
            hash_projection(&files, &facts)
        );
    }

    /// Every caller of `retained_dir_path` appends a child component to the result, so returning a
    /// path that merely *names* the directory is not enough — a child has to resolve through it.
    /// The two platforms need different mechanisms to satisfy that, and nothing else in the suite
    /// states the requirement: the callers that break reach it through a catalog lock and surface as
    /// a lock error, which cannot distinguish "wrong path shape" from "lock genuinely unavailable".
    #[test]
    fn a_child_component_resolves_through_a_retained_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("child"), b"payload").unwrap();
        let handle = File::open(dir.path()).unwrap();

        let joined = retained_dir_path(&handle).unwrap().join("child");

        assert_eq!(
            std::fs::read(&joined).unwrap_or_default(),
            b"payload",
            "a child component must resolve through {}",
            joined.display()
        );
    }

    fn catalog_with_worker(root: &Path) -> PathBuf {
        let agent = root.join("agents/host/worker");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("agent.kdl"),
            "agent \"worker\" { host \"host\"; command \"true\" }\n",
        )
        .unwrap();
        agent
    }

    #[test]
    fn current_projection_ignores_and_preserves_an_exact_legacy_staging_file() {
        let root = tempfile::tempdir().unwrap();
        catalog_with_worker(root.path());
        let before = project(root.path(), ProjectionSource::Current, root.path())
            .unwrap()
            .root_sha256;
        let legacy = root
            .path()
            .join("agents/host/.harness-context.tmp-123-456");
        fs::write(&legacy, b"stale legacy staging bytes").unwrap();

        let after = project(root.path(), ProjectionSource::Current, root.path())
            .unwrap()
            .root_sha256;
        cleanup_writer_temporaries(root.path()).unwrap();

        assert_eq!(after, before, "runtime residue cannot alter the digest");
        assert_eq!(
            fs::read(&legacy).unwrap(),
            b"stale legacy staging bytes",
            "the compatibility reader must not clean another process's file"
        );
        assert!(
            project(root.path(), ProjectionSource::Prepared, root.path()).is_err(),
            "prepared topology remains strict"
        );
    }

    #[cfg(unix)]
    #[test]
    fn current_projection_rejects_legacy_shape_type_confusion_and_near_misses() {
        use std::os::unix::fs::symlink;

        for kind in ["directory", "symlink", "near-miss"] {
            let root = tempfile::tempdir().unwrap();
            catalog_with_worker(root.path());
            let host = root.path().join("agents/host");
            match kind {
                "directory" => {
                    fs::create_dir(host.join(".harness-context.tmp-123-456")).unwrap();
                }
                "symlink" => {
                    symlink(
                        host.join("worker"),
                        host.join(".harness-context.tmp-123-456"),
                    )
                    .unwrap();
                }
                "near-miss" => {
                    fs::write(host.join(".harness-context.tmp-123-nope"), b"stale").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                project(root.path(), ProjectionSource::Current, root.path()).is_err(),
                "{kind} must not enter the reserved compatibility exception"
            );
            assert!(
                cleanup_writer_temporaries(root.path()).is_err(),
                "{kind} must remain strict during writer-temporary cleanup"
            );
        }
    }

}
