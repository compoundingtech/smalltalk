//! Whole-agent lifecycle desired state.
//!
//! Moved verbatim out of `agent_author.rs`; the shared declaration-writer primitives (error
//! vocabulary, target resolution, KDL node location, span edits, atomic commit) stay there.

use super::*;

/// Stable authored desired-state selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredStateValue {
    Running,
    Suspended,
    Retired,
}

impl DesiredStateValue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Retired => "retired",
        }
    }
}

/// Stable machine-readable receipt from one desired-state edit.
///
/// `managed_by` is the ownership marker the caller asserted and the declaration confirmed, so the
/// receipt records which authority admitted the edit; `null` is the ordinary unmarked path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesiredStateReceipt {
    pub result: AuthorOutcome,
    pub identity: String,
    pub desired_state: DesiredStateValue,
    pub reason: Option<String>,
    pub managed_by: Option<String>,
}

/// Author one whole-agent desired state without claiming runtime convergence.
///
/// `managed_by` is the ownership marker the caller asserts owns the declaration. `None` is the
/// ordinary path and refuses a Nix-owned declaration, exactly as presentation, address, stream, and
/// Resource authoring do. `Some(marker)` is a generator saying "I am the writer of these bytes",
/// and is admitted only when the declaration's own `meta { managed-by "..." }` names exactly that
/// marker: it is the projection's typed route to the one transition its own source can no longer
/// express, because the source edit that has to be projected is the seat's removal (#473).
pub fn set_desired_state(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    actor: Option<&str>,
    state: DesiredStateValue,
    reason: Option<&str>,
    managed_by: Option<&str>,
) -> Result<DesiredStateReceipt, AuthorError> {
    match state {
        DesiredStateValue::Running if reason.is_some() => {
            return Err(AuthorError::new(
                "invalid-desired-state",
                "running desired state forbids --reason",
            ));
        }
        DesiredStateValue::Suspended | DesiredStateValue::Retired if reason.is_none() => {
            return Err(AuthorError::new(
                "invalid-desired-state",
                format!("{} desired state requires --reason", state.as_str()),
            ));
        }
        _ => {}
    }
    if let Some(reason) = reason {
        validate_desired_state_reason(reason)
            .map_err(|error| AuthorError::new("invalid-desired-state", error.to_string()))?;
    }
    validate_marker_assertion(managed_by)?;
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
                "cannot prove an exact desired-state target while {} is malformed: {}",
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
        "desired-state-not-authorized",
    )?;
    let result = edit_desired_state_declaration(
        &catalog_lock,
        catalog_root,
        &crate::catalog_transaction::retained_dir_path(catalog_lock.control())
            .map_err(|error| AuthorError::new("declaration-write-failed", error.to_string()))?,
        &target.declaration,
        &target.identity,
        &target.source_host,
        &target.source_identity,
        state,
        reason,
        managed_by,
        || {},
    )?;
    Ok(DesiredStateReceipt {
        result,
        identity: target.identity,
        desired_state: state,
        reason: reason.map(str::to_owned),
        managed_by: managed_by.map(str::to_owned),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn edit_desired_state_declaration(
    catalog_lock: &CatalogLock,
    catalog: &Path,
    control: &Path,
    path: &Path,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    state: DesiredStateValue,
    reason: Option<&str>,
    managed_by: Option<&str>,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    if path.extension().and_then(|value| value.to_str()) != Some("kdl") {
        return Err(AuthorError::new(
            "unsupported-declaration-format",
            format!(
                "desired-state authoring requires canonical KDL, found {}",
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
    let marker_matched = authorize_marker(target, expected_identity, path, managed_by)?;
    let Some(replacement) = desired_state_edit(text, target, state, reason)? else {
        return Ok(AuthorOutcome::Unchanged);
    };
    verify_desired_state_candidate(
        &replacement,
        expected_identity,
        expected_host,
        expected_agent,
        state,
        reason,
    )?;
    // A marker-matched edit stands in for the CAS `agent publish` the projection would otherwise
    // have to perform, so it inherits that path's admission gate rather than only the local
    // candidate reparse: the whole prospective catalog must still be admissible. That is what
    // makes retiring a supervisor with a live descendant refuse (`retired-root`) instead of
    // committing bytes the next reconcile pass rejects (#434).
    if marker_matched
        && let Err(error) = crate::agent_publish::admit_declaration_rewrite(
            catalog,
            control,
            path,
            replacement.as_bytes(),
        )
    {
        return Err(AuthorError::new(
            "candidate-not-admissible",
            format!("{error:#}"),
        ));
    }
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

fn desired_state_edit(
    text: &str,
    target: &KdlNode,
    state: DesiredStateValue,
    reason: Option<&str>,
) -> Result<Option<String>, AuthorError> {
    let lifecycle = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| matches!(child.name().value(), "desired-state" | "retired"))
        .collect::<Vec<_>>();
    if lifecycle.len() > 1 {
        return Err(AuthorError::new(
            "duplicate-lifecycle-field",
            "target declares more than one lifecycle field",
        ));
    }
    if state == DesiredStateValue::Running {
        return lifecycle
            .first()
            .map(|node| remove_field(text, node).map(Some))
            .unwrap_or(Ok(None));
    }
    let authored = format!(
        "desired-state {} reason={}",
        quoted(state.as_str())?,
        quoted(reason.expect("validated by set_desired_state"))?
    );
    match lifecycle.as_slice() {
        [] => insert_node(text, target, &authored).map(Some),
        [node] => {
            let span = node.span();
            let range = span.offset()..span.offset() + span.len();
            if text.get(range.clone()) == Some(authored.as_str()) {
                return Ok(None);
            }
            let mut replacement = text.to_owned();
            replacement.replace_range(range, &authored);
            Ok(Some(replacement))
        }
        _ => unreachable!(),
    }
}

fn verify_desired_state_candidate(
    candidate: &str,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    state: DesiredStateValue,
    reason: Option<&str>,
) -> Result<(), AuthorError> {
    let document = KdlDocument::parse(candidate).map_err(|error| {
        AuthorError::new(
            "unsafe-source-edit",
            format!("desired-state edit did not produce valid KDL: {error}"),
        )
    })?;
    let target = exact_agent_node(&document, expected_identity, expected_host, expected_agent)?;
    let lifecycle = target
        .children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| matches!(child.name().value(), "desired-state" | "retired"))
        .collect::<Vec<_>>();
    if state == DesiredStateValue::Running {
        if lifecycle.is_empty() {
            return Ok(());
        }
    } else if let [node] = lifecycle.as_slice()
        && node.name().value() == "desired-state"
        && node.get(0).and_then(|entry| entry.as_string()) == Some(state.as_str())
        && node.get("reason").and_then(|entry| entry.as_string()) == reason
    {
        return Ok(());
    }
    Err(AuthorError::new(
        "unsafe-source-edit",
        "desired-state candidate did not read back as the authored intent",
    ))
}

/// The lifecycle verb's marker authority, read from the exact declaration node it edits.
fn authorize_marker(
    target: &KdlNode,
    expected_identity: &str,
    path: &Path,
    asserted: Option<&str>,
) -> Result<bool, AuthorError> {
    authorize_asserted_marker(&declared_markers(target), expected_identity, path, asserted)
}
