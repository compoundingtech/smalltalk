//! Agent-owned stream declarations.
//!
//! Moved verbatim out of `agent_author.rs`; the shared declaration-writer primitives (error
//! vocabulary, target resolution, KDL node location, span edits, atomic commit) stay there.

use crate::run::Runner as _;

use super::*;

/// Stable machine-readable receipt from adding one agent-owned stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamAddReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub name: String,
    pub launch: Option<StreamLaunch>,
}

/// Stable machine-readable receipt from removing one agent-owned stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamRemoveReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub name: String,
}

/// Add an agent-owned stream, or prove that the identical declaration already exists.
pub fn add_stream(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    name: &str,
    launch: Option<StreamLaunch>,
) -> Result<StreamAddReceipt, AuthorError> {
    author_stream(
        catalog_root,
        selector,
        this_host,
        actor,
        name,
        launch.as_ref(),
        false,
    )
    .map(|(result, identity)| StreamAddReceipt {
        result,
        identity,
        name: name.to_owned(),
        launch,
    })
}

/// Remove one agent-owned stream. An already absent stream is an idempotent success.
pub fn remove_stream(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    name: &str,
) -> Result<StreamRemoveReceipt, AuthorError> {
    author_stream(catalog_root, selector, this_host, actor, name, None, true).map(
        |(result, identity)| StreamRemoveReceipt {
            result,
            identity,
            name: name.to_owned(),
        },
    )
}

fn author_stream(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    name: &str,
    launch: Option<&StreamLaunch>,
    remove: bool,
) -> Result<(AuthorOutcome, String), AuthorError> {
    let catalog_lock = CatalogLock::exclusive(catalog_root).map_err(|error| {
        AuthorError::new(
            "catalog-lock-failed",
            format!("acquire catalog-authoring lock: {error:#}"),
        )
    })?;
    let found = crate::discover_strict(catalog_root);
    if let Some(error) = found.errors.first() {
        return Err(AuthorError::new(
            "catalog-malformed",
            format!(
                "cannot prove an exact stream target while {} is malformed: {}",
                error.path.display(),
                error.message
            ),
        ));
    }
    let target = resolve_target(&found.specs, selector, this_host)?;
    let actor = actor
        .map(|actor| resolve_target(&found.specs, actor, this_host).map(|target| target.identity))
        .transpose()?;
    authorize_actor(
        &found.specs,
        &target.identity,
        this_host,
        actor.as_deref(),
        "stream-not-authorized",
    )?;
    if remove {
        let spec = found
            .specs
            .iter()
            .find(|spec| spec.path == target.declaration)
            .ok_or_else(|| {
                AuthorError::new("stream-target-lost", "resolved stream target disappeared")
            })?;
        if spec
            .streams
            .iter()
            .find(|stream| stream.name == name)
            .is_some_and(|stream| stream.launch.is_some())
        {
            let task_name = format!("{}{}", agent_spec::STREAM_TASK_PREFIX, name);
            let task = spec
                .tasks
                .iter()
                .find(|task| task.name == task_name)
                .ok_or_else(|| {
                    AuthorError::new("stream-task-missing", "launched stream has no derived task")
                })?;
            let runtime_id = task
                .id
                .clone()
                .unwrap_or_else(|| format!("{}.{}", spec.bus_id(this_host), task.name));
            let runner = crate::run::SystemRunner::new(
                catalog_root.to_path_buf(),
                crate::run::exec_state_dir(this_host),
            );
            let live = runner
                .list_sessions()
                .map_err(|error| {
                    AuthorError::new("stream-runtime-observation-failed", error.to_string())
                })?
                .into_iter()
                .any(|session| session.alive && session.pty_id == runtime_id);
            if live {
                runner.retire(&runtime_id).map_err(|error| {
                    AuthorError::new(
                        "stream-runtime-retirement-failed",
                        format!("retire launched stream runtime {runtime_id}: {error:#}"),
                    )
                })?;
            }
        }
    }
    let result = edit_stream_declaration(
        &catalog_lock,
        catalog_root,
        &crate::catalog_transaction::retained_dir_path(catalog_lock.control())
            .map_err(|error| AuthorError::new("declaration-write-failed", error.to_string()))?,
        &target.declaration,
        &target.identity,
        &target.source_host,
        &target.source_identity,
        name,
        launch,
        remove,
        || {},
    )?;
    Ok((result, target.identity))
}

#[allow(clippy::too_many_arguments)]
fn edit_stream_declaration(
    catalog_lock: &CatalogLock,
    catalog: &Path,
    control: &Path,
    path: &Path,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    name: &str,
    launch: Option<&StreamLaunch>,
    remove: bool,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    if path.extension().and_then(|value| value.to_str()) != Some("kdl") {
        return Err(AuthorError::new(
            "unsupported-declaration-format",
            format!(
                "stream authoring requires canonical KDL, found {}",
                path.display()
            ),
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        AuthorError::new(
            "declaration-read-failed",
            format!("reading declaration {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(AuthorError::new(
            "unsafe-declaration-path",
            format!("refusing non-regular declaration path {}", path.display()),
        ));
    }
    let original = fs::read(path).map_err(|error| {
        AuthorError::new(
            "declaration-read-failed",
            format!("reading declaration {}: {error}", path.display()),
        )
    })?;
    let original_version = SourceVersion::from_metadata(&metadata);
    let text = std::str::from_utf8(&original).map_err(|error| {
        AuthorError::new(
            "malformed-declaration",
            format!("declaration {} is not UTF-8: {error}", path.display()),
        )
    })?;
    let document = KdlDocument::parse(text).map_err(|error| {
        AuthorError::new(
            "malformed-declaration",
            format!("parsing declaration {}: {error}", path.display()),
        )
    })?;
    let target = exact_agent_node(&document, expected_identity, expected_host, expected_agent)?;
    if is_nix_managed(target) {
        return Err(AuthorError::new(
            "nix-managed-declaration",
            format!(
                "agent {expected_identity:?} is Nix-owned; edit its Nix source instead of {}",
                path.display()
            ),
        ));
    }
    let replacement = stream_edit(text, target, name, launch, remove)?;
    let Some(replacement) = replacement else {
        return Ok(AuthorOutcome::Unchanged);
    };
    verify_stream_candidate(
        catalog,
        path,
        &replacement,
        expected_identity,
        expected_host,
        expected_agent,
        name,
        launch,
        remove,
    )?;
    atomic_replace_checked(
        catalog_lock,
        catalog,
        control,
        path,
        &original,
        original_version,
        replacement.as_bytes(),
        metadata.permissions().mode() & 0o7777,
        before_commit,
    )?;
    Ok(AuthorOutcome::Changed)
}

fn stream_edit(
    text: &str,
    target: &KdlNode,
    name: &str,
    launch: Option<&StreamLaunch>,
    remove: bool,
) -> Result<Option<String>, AuthorError> {
    let streams = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| {
            child.name().value() == "stream"
                && child.get(0).and_then(|entry| entry.as_string()) == Some(name)
        })
        .collect::<Vec<_>>();
    if streams.len() > 1 {
        return Err(AuthorError::new(
            "duplicate-stream",
            format!("target declares stream {name:?} more than once"),
        ));
    }
    if remove {
        return streams
            .first()
            .map(|node| remove_field(text, node).map(Some))
            .unwrap_or(Ok(None));
    }
    if let Some(existing) = streams.first() {
        if parsed_stream_launch(existing)? == launch.cloned() {
            return Ok(None);
        }
        return Err(AuthorError::new(
            "stream-already-exists",
            format!(
                "stream {name:?} already exists with a different launch; remove it before adding a replacement"
            ),
        ));
    }
    let authored = match launch {
        None => format!("stream {} {{}}", quoted(name)?),
        Some(StreamLaunch::Command(command)) => format!(
            "stream {} {{ command {} }}",
            quoted(name)?,
            quoted(command)?
        ),
        Some(StreamLaunch::Argv(argv)) => {
            let values = argv
                .iter()
                .map(|value| quoted(value))
                .collect::<Result<Vec<_>, _>>()?;
            format!("stream {} {{ argv {} }}", quoted(name)?, values.join(" "))
        }
    };
    insert_node(text, target, &authored).map(Some)
}

fn parsed_stream_launch(node: &KdlNode) -> Result<Option<StreamLaunch>, AuthorError> {
    let children = node
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .collect::<Vec<_>>();
    match children.as_slice() {
        [] => Ok(None),
        [child] if child.name().value() == "command" => child
            .get(0)
            .and_then(|entry| entry.as_string())
            .map(|value| Some(StreamLaunch::Command(value.to_owned())))
            .ok_or_else(|| {
                AuthorError::new("malformed-stream", "stream command must contain one string")
            }),
        [child] if child.name().value() == "argv" => {
            let argv = child
                .entries()
                .iter()
                .map(|entry| entry.value().as_string().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    AuthorError::new("malformed-stream", "stream argv values must be strings")
                })?;
            Ok(Some(StreamLaunch::Argv(argv)))
        }
        _ => Err(AuthorError::new(
            "malformed-stream",
            "stream must contain exactly one command or argv node, or be empty",
        )),
    }
}

fn verify_stream_candidate(
    catalog: &Path,
    path: &Path,
    candidate: &str,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    name: &str,
    launch: Option<&StreamLaunch>,
    removed: bool,
) -> Result<(), AuthorError> {
    let temporary = tempfile::tempdir()
        .map_err(|error| AuthorError::new("unsafe-source-edit", error.to_string()))?;
    let relative = path.strip_prefix(catalog).map_err(|_| {
        AuthorError::new(
            "unsafe-declaration-path",
            format!(
                "declaration {} is outside catalog {}",
                path.display(),
                catalog.display()
            ),
        )
    })?;
    let candidate_path = temporary.path().join(relative);
    fs::create_dir_all(
        candidate_path
            .parent()
            .expect("candidate declaration has a parent"),
    )
    .and_then(|()| fs::write(&candidate_path, candidate))
    .map_err(|error| {
        AuthorError::new(
            "unsafe-source-edit",
            format!("stage stream validation: {error}"),
        )
    })?;
    let (specs, _) = agent_spec::discover_file(temporary.path(), &candidate_path)
        .map_err(|error| AuthorError::new("invalid-stream", error.to_string()))?;
    let spec = specs
        .iter()
        .find(|spec| {
            spec.identity == expected_agent && spec.bus_id(expected_host) == expected_identity
        })
        .ok_or_else(|| {
            AuthorError::new(
                "unsafe-source-edit",
                "stream candidate lost the authored agent",
            )
        })?;
    let observed = spec.streams.iter().find(|stream| stream.name == name);
    if removed && observed.is_none()
        || !removed && observed.is_some_and(|stream| stream.launch.as_ref() == launch)
    {
        Ok(())
    } else {
        Err(AuthorError::new(
            "unsafe-source-edit",
            "stream candidate did not read back as the authored intent",
        ))
    }
}
