//! Single-positional-string declaration fields: presentation (name, description) and address.
//!
//! Moved verbatim out of `agent_author.rs`; the shared declaration-writer primitives (error
//! vocabulary, target resolution, KDL node location, span edits, atomic commit) stay there.

use super::*;

/// A mutable presentation field with no routing or lifecycle authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresentationField {
    Name,
    Description,
}

impl PresentationField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
        }
    }

    fn max_chars(self) -> usize {
        match self {
            Self::Name => AGENT_NAME_MAX_CHARS,
            Self::Description => AGENT_DESCRIPTION_MAX_CHARS,
        }
    }
}

/// One single-positional-string child node these source-preserving edits may rewrite.
///
/// Address is not presentation — it is the mutable route (R24/R25), and it carries authority
/// presentation never has — but it is edited by exactly the same span-bounded machinery: find,
/// replace, insert, or remove one child node while every other byte of the declaration survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeclaredField {
    Presentation(PresentationField),
    Address,
}

impl DeclaredField {
    fn as_str(self) -> &'static str {
        match self {
            Self::Presentation(field) => field.as_str(),
            Self::Address => "address",
        }
    }
}

impl From<PresentationField> for DeclaredField {
    fn from(field: PresentationField) -> Self {
        Self::Presentation(field)
    }
}

/// Stable machine-readable receipt from one presentation edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresentationReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub field: PresentationField,
    pub value: Option<String>,
    pub retired: bool,
}

/// Stable machine-readable receipt from one agent-address cutover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressReceipt {
    pub result: AuthorOutcome,
    /// The subject's immutable agent ID (R24) — the value an address cutover must not touch.
    pub id: String,
    /// The positional declaration key, also unchanged: it stays the legacy address fallback.
    pub identity: String,
    /// The declared `address` after the edit. `None` means the positional fallback is effective.
    pub address: Option<String>,
    /// `<host>.<effective address>` after the cutover. `None` for a retired subject, which is
    /// non-routable and released its address.
    pub bus_address: Option<String>,
    pub retired: bool,
}

/// Set or clear one presentation field for one stable Agent Spec identity.
///
/// `actor` is the caller-supplied `ST_AGENT` identity. An absent actor is the explicit operator
/// path. Within the trusted-fleet model, the guardrail limits a catalog-managed caller to itself or
/// a descendant reached through declared supervisor edges; no presentation field expands it.
pub fn set_presentation(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    field: PresentationField,
    requested: Option<&str>,
) -> Result<PresentationReceipt, AuthorError> {
    let catalog_lock = CatalogLock::exclusive(catalog_root).map_err(|error| {
        AuthorError::new(
            "catalog-lock-failed",
            format!("acquire catalog-authoring lock: {error:#}"),
        )
    })?;
    let found = crate::discover(catalog_root);
    if let Some(error) = found.errors.first() {
        return Err(AuthorError::new(
            "catalog-malformed",
            format!(
                "cannot prove an exact presentation target while {} is malformed: {}",
                error.path.display(),
                error.message
            ),
        ));
    }
    let target = resolve_target(&found.specs, selector, this_host)?;
    authorize_actor(
        &found.specs,
        &target.identity,
        this_host,
        actor,
        "presentation-not-authorized",
    )?;
    let requested = requested
        .map(|value| {
            validate_presentation(field.as_str(), Some(value), field.max_chars())
                .map(|()| value.to_owned())
                .map_err(|error| AuthorError::new("invalid-presentation", error.to_string()))
        })
        .transpose()?;
    let result = edit_declaration(
        &catalog_lock,
        catalog_root,
        &crate::catalog_transaction::retained_dir_path(catalog_lock.control())
            .map_err(|error| AuthorError::new("declaration-write-failed", error.to_string()))?,
        &target.declaration,
        &target.identity,
        &target.source_host,
        &target.source_identity,
        field.into(),
        requested.as_deref(),
        || {},
    )?;
    Ok(PresentationReceipt {
        result,
        identity: target.identity,
        field,
        value: requested,
        retired: target.retired,
    })
}

/// Assign or clear one subject's mutable agent address — one atomic address-book cutover (R25).
///
/// The old address stops resolving as soon as the new catalog generation is visible; st2 stores no
/// rename history, redirect, implicit alias, or time-bounded compatibility route, so a stale
/// caller fails loudly and refreshes the roster. The edit rewrites exactly the `address` child
/// node, which is what makes the cutover nondisruptive by construction: the declaration-parent
/// state anchor, ID-keyed supervisor edges, task IDs, launch fingerprints, workspace, inbox,
/// archive, context, Resource state, and runtime ownership are all keyed off values this edit
/// never touches. `None` restores the positional `identity` fallback and is admitted only while
/// that fallback address is itself still unique on the resolved host.
pub fn set_address(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    requested: Option<&str>,
) -> Result<AddressReceipt, AuthorError> {
    let catalog_lock = CatalogLock::exclusive(catalog_root).map_err(|error| {
        AuthorError::new(
            "catalog-lock-failed",
            format!("acquire catalog-authoring lock: {error:#}"),
        )
    })?;
    let found = crate::discover(catalog_root);
    if let Some(error) = found.errors.first() {
        return Err(AuthorError::new(
            "catalog-malformed",
            format!(
                "cannot prove an exact address target while {} is malformed: {}",
                error.path.display(),
                error.message
            ),
        ));
    }
    let target = resolve_target(&found.specs, selector, this_host)?;
    authorize_actor(
        &found.specs,
        &target.identity,
        this_host,
        actor,
        "address-not-authorized",
    )?;
    if let Some(value) = requested {
        agent_spec::validate_agent_address(value)
            .map_err(|error| AuthorError::new("invalid-address", error.to_string()))?;
    }
    refuse_address_collision(catalog_root, &found.specs, this_host, &target, requested)?;
    let result = edit_declaration(
        &catalog_lock,
        catalog_root,
        &crate::catalog_transaction::retained_dir_path(catalog_lock.control())
            .map_err(|error| AuthorError::new("declaration-write-failed", error.to_string()))?,
        &target.declaration,
        &target.identity,
        &target.source_host,
        &target.source_identity,
        DeclaredField::Address,
        requested,
        || {},
    )?;
    let effective = requested.unwrap_or(&target.source_identity);
    Ok(AddressReceipt {
        result,
        // A retired subject is non-routable and released its address, so null is the honest bus
        // address here — exactly what the roster projects for the same subject.
        bus_address: (!target.retired).then(|| format!("{}.{effective}", target.source_host)),
        address: requested.map(str::to_owned),
        id: target.agent_id,
        identity: target.identity,
        retired: target.retired,
    })
}

/// Refuse an effective address that would not be unique on the target's resolved logical host.
///
/// The prospective catalog is the discovery this command already holds with exactly this subject's
/// `address` replaced, so `validate.rs`'s `dup-address` rule — the same rule whole-catalog
/// validation enforces — decides explicit/explicit and explicit/identity-fallback collisions
/// alike, including the `--clear` case where the restored fallback is the candidate address. Any
/// duplicate address in the prospective catalog refuses: an address book with two claims on one
/// route cannot answer an ordinary reference, so there is no cutover to admit.
fn refuse_address_collision(
    catalog_root: &Path,
    specs: &[crate::AgentSpec],
    this_host: &str,
    target: &AgentTarget,
    requested: Option<&str>,
) -> Result<(), AuthorError> {
    let mut prospective = crate::Discovered {
        specs: specs.to_vec(),
        ..Default::default()
    };
    for spec in &mut prospective.specs {
        if spec.bus_id(this_host) == target.identity {
            spec.address = requested.map(str::to_owned);
        }
    }
    let report = crate::validate::validate_discovered(catalog_root, Some(this_host), &prospective);
    if !report
        .issues
        .iter()
        .any(|issue| issue.code == "dup-address")
    {
        return Ok(());
    }
    // Name the incumbent, not the first declaration in path order: the forwarded diagnostic often
    // pointed at the candidate's own file, because that is where `dup-address` first saw the
    // address. The claimant is the *other* subject reading the same effective address on this
    // host.
    let candidate = requested.unwrap_or(&target.source_identity);
    let claimant = prospective
        .specs
        .iter()
        .find(|spec| {
            spec.bus_id(this_host) != target.identity
                && !spec.desired_state.is_retired()
                && spec.resolved_host(this_host) == target.source_host
                && spec.effective_address() == candidate
        })
        .map(|spec| {
            format!(
                "{} declared in {}",
                spec.bus_id(this_host),
                spec.path
                    .strip_prefix(catalog_root)
                    .unwrap_or(&spec.path)
                    .display()
            )
        });
    Err(AuthorError::new(
        "address-conflict",
        format!(
            "{} is not unique on host {:?}: already claimed by {}",
            requested.map_or_else(
                || format!("identity fallback address {:?}", target.source_identity),
                |value| format!("address {value:?}")
            ),
            target.source_host,
            claimant.unwrap_or_else(|| "another declaration in this catalog".to_owned())
        ),
    ))
}

pub(super) fn edit_declaration(
    catalog_lock: &CatalogLock,
    catalog: &Path,
    control: &Path,
    path: &Path,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    field: DeclaredField,
    requested: Option<&str>,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    if path.extension().and_then(|value| value.to_str()) != Some("kdl") {
        return Err(AuthorError::new(
            "unsupported-declaration-format",
            format!(
                "presentation authoring requires canonical KDL, found {}",
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
    // No span-bounded field edit may change the subject's immutable agent ID (R24). Address, name,
    // and description are all mutable; `id` is the one value that identifies the subject across
    // every one of those changes, so the candidate must read back with the exact same bytes — or
    // with none, on a declaration ID migration has not reached yet.
    let expected_id = declared_id(target);
    let Some(replacement) = presentation_edit(text, target, field, requested)? else {
        return Ok(AuthorOutcome::Unchanged);
    };
    verify_candidate(
        &replacement,
        expected_identity,
        expected_host,
        expected_agent,
        field,
        requested,
        expected_id.as_deref(),
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

/// The declaration's explicit immutable `id`, if it carries one.
fn declared_id(node: &KdlNode) -> Option<String> {
    node.children()?
        .nodes()
        .iter()
        .find(|child| child.name().value() == "id")
        .and_then(|child| child.get(0))
        .and_then(|value| value.as_string())
        .map(str::to_owned)
}

fn presentation_edit(
    text: &str,
    target: &KdlNode,
    field: DeclaredField,
    requested: Option<&str>,
) -> Result<Option<String>, AuthorError> {
    let fields = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| child.name().value() == field.as_str())
        .collect::<Vec<_>>();
    match fields.as_slice() {
        [] => match requested {
            Some(value) => insert_field(text, target, field, value).map(Some),
            None => Ok(None),
        },
        [node] => match requested {
            Some(value) => replace_field(text, node, field, value),
            None => remove_field(text, node).map(Some),
        },
        _ => Err(AuthorError::new(
            "duplicate-presentation-field",
            format!("target declares `{}` more than once", field.as_str()),
        )),
    }
}

fn parse_field_value(node: &KdlNode, field: DeclaredField) -> Result<&str, AuthorError> {
    if node.children().is_some() || node.entries().len() != 1 || node.entries()[0].name().is_some()
    {
        return Err(AuthorError::new(
            "malformed-presentation-field",
            format!(
                "`{}` must contain exactly one positional string",
                field.as_str()
            ),
        ));
    }
    node.get(0)
        .and_then(|value| value.as_string())
        .ok_or_else(|| {
            AuthorError::new(
                "malformed-presentation-field",
                format!("`{}` must contain a string", field.as_str()),
            )
        })
}

fn replace_field(
    text: &str,
    node: &KdlNode,
    field: DeclaredField,
    value: &str,
) -> Result<Option<String>, AuthorError> {
    if parse_field_value(node, field)? == value {
        return Ok(None);
    }
    let entry = &node.entries()[0];
    let span = entry.span();
    let range = span.offset()..span.offset() + span.len();
    text.get(range.clone()).ok_or_else(|| {
        AuthorError::new(
            "malformed-declaration",
            "presentation value span falls outside the declaration",
        )
    })?;
    let mut replacement = text.to_owned();
    replacement.replace_range(range, &quoted(value)?);
    Ok(Some(replacement))
}

fn insert_field(
    text: &str,
    target: &KdlNode,
    field: DeclaredField,
    value: &str,
) -> Result<String, AuthorError> {
    insert_node(
        text,
        target,
        &format!("{} {}", field.as_str(), quoted(value)?),
    )
}

pub(super) fn verify_candidate(
    candidate: &str,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    field: DeclaredField,
    expected: Option<&str>,
    expected_id: Option<&str>,
) -> Result<(), AuthorError> {
    let document = KdlDocument::parse(candidate).map_err(|error| {
        AuthorError::new(
            "unsafe-source-edit",
            format!("field edit did not produce valid KDL: {error}"),
        )
    })?;
    let target = exact_agent_node(&document, expected_identity, expected_host, expected_agent)?;
    if declared_id(target).as_deref() != expected_id {
        return Err(AuthorError::new(
            "agent-id-immutable",
            format!(
                "edit would change the immutable agent id of {expected_identity:?}; `id` is the \
                 one declared value no authoring command may rewrite"
            ),
        ));
    }
    let fields = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| child.name().value() == field.as_str())
        .collect::<Vec<_>>();
    let observed = match fields.as_slice() {
        [] => None,
        [node] => Some(parse_field_value(node, field)?),
        _ => {
            return Err(AuthorError::new(
                "unsafe-source-edit",
                format!(
                    "presentation edit produced duplicate `{}` fields",
                    field.as_str()
                ),
            ));
        }
    };
    if observed != expected {
        return Err(AuthorError::new(
            "unsafe-source-edit",
            format!(
                "presentation edit did not produce the requested `{}`",
                field.as_str()
            ),
        ));
    }
    Ok(())
}
