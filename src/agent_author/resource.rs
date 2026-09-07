//! Typed Resource bindings: add, remove, rename.
//!
//! Moved verbatim out of `agent_author.rs`; the shared declaration-writer primitives (error
//! vocabulary, target resolution, KDL node location, span edits, atomic commit) stay there.

use super::*;

/// Stable machine-readable receipt from adding or updating one Resource binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceAddReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub name: String,
    pub uri: String,
    pub reason: String,
    pub inactive_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<serde_json::Value>,
}

/// Stable machine-readable receipt from removing one Resource binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRemoveReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub name: String,
}

/// Stable machine-readable receipt from relabelling one Resource binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRenameReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub old: String,
    pub new: String,
}

/// Declare one Resource binding, or update the binding that already carries `name`.
///
/// st2 preserves the binding for readers; it resolves nothing and grants nothing. `uri` is the
/// exact absolute identity and is stored byte for byte with no normalization.
#[allow(clippy::too_many_arguments)]
pub fn add_resource_with_selector(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    name: &str,
    uri: &str,
    reason: &str,
    inactive_reason: Option<&str>,
    resource_selector: Option<&serde_json::Value>,
) -> Result<ResourceAddReceipt, AuthorError> {
    author_resource(
        catalog_root,
        selector,
        this_host,
        actor,
        ResourceIntent::Upsert {
            name,
            uri,
            reason,
            inactive_reason,
            selector: resource_selector,
        },
    )
    .map(|(result, identity)| ResourceAddReceipt {
        result,
        identity,
        name: name.to_owned(),
        uri: uri.to_owned(),
        reason: reason.to_owned(),
        inactive_reason: inactive_reason.map(str::to_owned),
        selector: resource_selector.cloned(),
    })
}

/// Remove one Resource binding. An already absent binding is an idempotent success.
pub fn remove_resource(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    name: &str,
) -> Result<ResourceRemoveReceipt, AuthorError> {
    author_resource(
        catalog_root,
        selector,
        this_host,
        actor,
        ResourceIntent::Remove { name },
    )
    .map(|(result, identity)| ResourceRemoveReceipt {
        result,
        identity,
        name: name.to_owned(),
    })
}

/// Relabel one Resource binding, carrying its `uri`, `reason`, and `inactive-reason` unchanged.
///
/// An absent `old` and an already declared `new` both refuse: binding names are unique within one
/// agent, so neither request has an outcome that preserves the caller's intent.
pub fn rename_resource(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    old: &str,
    new: &str,
) -> Result<ResourceRenameReceipt, AuthorError> {
    author_resource(
        catalog_root,
        selector,
        this_host,
        actor,
        ResourceIntent::Rename { old, new },
    )
    .map(|(result, identity)| ResourceRenameReceipt {
        result,
        identity,
        old: old.to_owned(),
        new: new.to_owned(),
    })
}

/// One requested Resource-binding mutation, resolved against the declaration under the lock.
#[derive(Debug, Clone, Copy)]
enum ResourceIntent<'a> {
    Upsert {
        name: &'a str,
        uri: &'a str,
        reason: &'a str,
        inactive_reason: Option<&'a str>,
        selector: Option<&'a serde_json::Value>,
    },
    Remove {
        name: &'a str,
    },
    Rename {
        old: &'a str,
        new: &'a str,
    },
}

/// The binding state a candidate must read back as before it may be committed.
#[derive(Debug)]
struct ResourceExpectation {
    absent: Option<String>,
    present: Option<Resource>,
}

fn author_resource(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    intent: ResourceIntent<'_>,
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
                "cannot prove an exact resource target while {} is malformed: {}",
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
        "resource-not-authorized",
    )?;
    let result = edit_resource_declaration(
        &catalog_lock,
        catalog_root,
        &crate::catalog_transaction::retained_dir_path(catalog_lock.control())
            .map_err(|error| AuthorError::new("declaration-write-failed", error.to_string()))?,
        &target.declaration,
        &target.identity,
        &target.source_host,
        &target.source_identity,
        intent,
        || {},
    )?;
    Ok((result, target.identity))
}

#[allow(clippy::too_many_arguments)]
fn edit_resource_declaration(
    catalog_lock: &CatalogLock,
    catalog: &Path,
    control: &Path,
    path: &Path,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    intent: ResourceIntent<'_>,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    if path.extension().and_then(|value| value.to_str()) != Some("kdl") {
        return Err(AuthorError::new(
            "unsupported-declaration-format",
            format!(
                "resource authoring requires canonical KDL, found {}",
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
    let Some((replacement, expectation)) = resource_edit(text, target, intent)? else {
        return Ok(AuthorOutcome::Unchanged);
    };
    verify_resource_candidate(
        catalog,
        path,
        &replacement,
        expected_identity,
        expected_host,
        expected_agent,
        &expectation,
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

/// Resolve one intent against the declared bindings, preserving every unrelated byte.
///
/// `Ok(None)` is the proven no-op: an unchanged upsert, an absent removal, or a self-rename. A
/// changed upsert rewrites exactly the one binding node in place, so its position, its leading
/// trivia, and every sibling binding survive.
fn resource_edit(
    text: &str,
    target: &KdlNode,
    intent: ResourceIntent<'_>,
) -> Result<Option<(String, ResourceExpectation)>, AuthorError> {
    let declared = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| child.name().value() == "resource")
        .collect::<Vec<_>>();
    let declaring = |name: &str| -> Result<Option<&KdlNode>, AuthorError> {
        let matches = declared
            .iter()
            .copied()
            .filter(|child| child.get(0).and_then(|entry| entry.as_string()) == Some(name))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(AuthorError::new(
                "duplicate-resource",
                format!("target declares resource {name:?} more than once"),
            ));
        }
        Ok(matches.first().copied())
    };
    match intent {
        ResourceIntent::Upsert {
            name,
            uri,
            reason,
            inactive_reason,
            selector,
        } => {
            let authored = declared_resource(name, uri, reason, inactive_reason, selector)?;
            let replacement = match declaring(name)? {
                Some(node) if parsed_resource(node)? == authored => return Ok(None),
                Some(node) => replace_node(text, node, &render_resource(&authored)?)?,
                None => insert_node(text, target, &render_resource(&authored)?)?,
            };
            Ok(Some((
                replacement,
                ResourceExpectation {
                    absent: None,
                    present: Some(authored),
                },
            )))
        }
        ResourceIntent::Remove { name } => {
            let Some(node) = declaring(name)? else {
                return Ok(None);
            };
            Ok(Some((
                remove_field(text, node)?,
                ResourceExpectation {
                    absent: Some(name.to_owned()),
                    present: None,
                },
            )))
        }
        ResourceIntent::Rename { old, new } => {
            let Some(node) = declaring(old)? else {
                return Err(AuthorError::new(
                    "resource-not-found",
                    format!("target declares no resource {old:?}"),
                ));
            };
            if old == new {
                return Ok(None);
            }
            if declaring(new)?.is_some() {
                return Err(AuthorError::new(
                    "resource-already-exists",
                    format!(
                        "target already declares resource {new:?}; binding names are unique within one agent"
                    ),
                ));
            }
            let carried = parsed_resource(node)?;
            let renamed = declared_resource(
                new,
                carried.uri(),
                carried.reason(),
                carried.inactive_reason(),
                carried.selector(),
            )?;
            Ok(Some((
                replace_node(text, node, &render_resource(&renamed)?)?,
                ResourceExpectation {
                    absent: Some(old.to_owned()),
                    present: Some(renamed),
                },
            )))
        }
    }
}

/// Enforce the canonical binding invariants — `agent_spec` owns them; this mints no new rule.
fn declared_resource(
    name: &str,
    uri: &str,
    reason: &str,
    inactive_reason: Option<&str>,
    selector: Option<&serde_json::Value>,
) -> Result<Resource, AuthorError> {
    let resource = match inactive_reason {
        None => Resource::new(name.to_owned(), uri.to_owned(), reason.to_owned()),
        Some(inactive_reason) => Resource::new_inactive(
            name.to_owned(),
            uri.to_owned(),
            reason.to_owned(),
            inactive_reason.to_owned(),
        ),
    }
    .map_err(|error| AuthorError::new("invalid-resource", error))?;
    Ok(match selector {
        Some(selector) => resource.with_selector(selector.clone()),
        None => resource,
    })
}

fn parsed_resource(node: &KdlNode) -> Result<Resource, AuthorError> {
    let malformed =
        |detail: &str| AuthorError::new("malformed-resource", format!("resource binding {detail}"));
    if node.children().is_some() {
        return Err(malformed("cannot have children"));
    }
    let mut name = None;
    let mut uri = None;
    let mut reason = None;
    let mut inactive_reason = None;
    let mut selector = None;
    for entry in node.entries() {
        let value = entry
            .value()
            .as_string()
            .ok_or_else(|| malformed("accepts only string values"))?;
        match entry.name().map(|name| name.value()) {
            None => {
                if name.replace(value).is_some() {
                    return Err(malformed("declares one of its fields more than once"));
                }
            }
            Some("uri") => {
                if uri.replace(value).is_some() {
                    return Err(malformed("declares one of its fields more than once"));
                }
            }
            Some("reason") => {
                if reason.replace(value).is_some() {
                    return Err(malformed("declares one of its fields more than once"));
                }
            }
            Some("inactive-reason") => {
                if inactive_reason.replace(value).is_some() {
                    return Err(malformed("declares one of its fields more than once"));
                }
            }
            Some("selector") => {
                if selector.is_some() {
                    return Err(malformed("declares one of its fields more than once"));
                }
                selector = Some(serde_json::from_str(value).map_err(|error| {
                    malformed(&format!("has invalid JSON `selector`: {error}"))
                })?);
            }
            Some(other) => return Err(malformed(&format!("has unsupported property `{other}`"))),
        }
    }
    let (Some(name), Some(uri), Some(reason)) = (name, uri, reason) else {
        return Err(malformed("needs a name, a `uri`, and a `reason`"));
    };
    declared_resource(name, uri, reason, inactive_reason, selector.as_ref())
}

fn render_resource(resource: &Resource) -> Result<String, AuthorError> {
    let mut authored = format!(
        "resource {} uri={} reason={}",
        quoted(resource.name())?,
        quoted(resource.uri())?,
        quoted(resource.reason())?
    );
    if let Some(inactive_reason) = resource.inactive_reason() {
        authored.push_str(&format!(" inactive-reason={}", quoted(inactive_reason)?));
    }
    if let Some(selector) = resource.selector() {
        authored.push_str(" selector=");
        authored.push_str(&raw_json(selector)?);
    }
    Ok(authored)
}

fn raw_json(value: &serde_json::Value) -> Result<String, AuthorError> {
    let json = serde_json::to_string(value).map_err(|error| {
        AuthorError::new(
            "invalid-resource",
            format!("serialize Resource selector as canonical JSON: {error}"),
        )
    })?;
    for hashes in 1..=json.len() + 1 {
        let fence = "#".repeat(hashes);
        if !json.contains(&format!("\"{fence}")) {
            return Ok(format!("{fence}\"{json}\"{fence}"));
        }
    }
    unreachable!("a delimiter longer than the JSON payload cannot occur in the payload")
}

/// Replace exactly one node's source span. A KDL node span carries neither the leading trivia nor
/// the trailing terminator, so the surrounding line survives untouched.
fn replace_node(text: &str, node: &KdlNode, authored: &str) -> Result<String, AuthorError> {
    let span = node.span();
    let range = span.offset()..span.offset() + span.len();
    text.get(range.clone()).ok_or_else(|| {
        AuthorError::new(
            "malformed-declaration",
            "resource binding span falls outside the declaration",
        )
    })?;
    // The span can run to the start of trailing trivia, so replacing it verbatim would glue the
    // rendered node onto a following `// comment`. Leave that separator in the source.
    let kept = text[range.clone()].trim_end_matches([' ', '\t']).len();
    let mut replacement = text.to_owned();
    replacement.replace_range(range.start..range.start + kept, authored);
    Ok(replacement)
}

fn verify_resource_candidate(
    catalog: &Path,
    path: &Path,
    candidate: &str,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    expectation: &ResourceExpectation,
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
            format!("stage resource validation: {error}"),
        )
    })?;
    let (specs, _) = agent_spec::discover_file(temporary.path(), &candidate_path)
        .map_err(|error| AuthorError::new("invalid-resource", error.to_string()))?;
    let spec = specs
        .iter()
        .find(|spec| {
            spec.identity == expected_agent && spec.bus_id(expected_host) == expected_identity
        })
        .ok_or_else(|| {
            AuthorError::new(
                "unsafe-source-edit",
                "resource candidate lost the authored agent",
            )
        })?;
    let declares = |name: &str| {
        spec.resources
            .iter()
            .find(|resource| resource.name() == name)
    };
    if expectation
        .absent
        .as_deref()
        .is_some_and(|name| declares(name).is_some())
        || expectation
            .present
            .as_ref()
            .is_some_and(|expected| declares(expected.name()) != Some(expected))
    {
        return Err(AuthorError::new(
            "unsafe-source-edit",
            "resource candidate did not read back as the authored intent",
        ));
    }
    Ok(())
}
