//! Constrained, source-preserving authoring of Agent Spec presentation and desired state.
//!
//! Presentation is declaration state, not runtime identity. Every edit holds the shared persistent
//! catalog-authoring lock, rechecks the original bytes, and atomically replaces exactly one
//! canonical KDL declaration. TOML, JSON, and callers outside the supplied actor relationship fail
//! closed. A declaration marked `meta { managed-by "nix" }` fails closed too, except on the
//! lifecycle verb, where the projection may assert that marker and author the one transition its
//! own source can no longer express (#473). `ST_AGENT` is a trusted-fleet guardrail rather than
//! authentication. The lock serializes cooperating local st2 writers; it is not a cross-host lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use agent_spec::spec::{
    AGENT_DESCRIPTION_MAX_CHARS, AGENT_NAME_MAX_CHARS, Resource, StreamLaunch,
    validate_desired_state_reason, validate_presentation,
};
use kdl::{KdlDocument, KdlNode};
use serde::Serialize;

use crate::catalog_lock::CatalogLock;

mod declared_field;
mod desired_state;
mod markers;
mod resource;
mod stream;

// Re-exported at each item's own visibility so every existing path — `st2::agent_author::*` for
// the binary and the integration tests, `crate::agent_author::*` for `agent_publish`, and the
// inline test module's `use super::*` — resolves unchanged.
pub use declared_field::*;
pub use desired_state::*;
pub(crate) use markers::*;
pub use resource::*;
pub use stream::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceVersion {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl SourceVersion {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}


/// Whether a request changed declaration bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthorOutcome {
    Changed,
    Unchanged,
}


/// A classified authoring refusal. `code` is stable for machine consumers.
#[derive(Debug)]
pub struct AuthorError {
    code: &'static str,
    message: String,
}

impl AuthorError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for AuthorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuthorError {}

#[derive(Debug)]
struct AgentTarget {
    identity: String,
    /// The subject's immutable catalog-global agent ID (R24): the explicit `id`, else the legacy
    /// `<host>.<identity>` bus identity that migration freezes as this subject's ID.
    agent_id: String,
    source_host: String,
    source_identity: String,
    declaration: PathBuf,
    retired: bool,
}


fn resolve_target(
    specs: &[crate::AgentSpec],
    selector: &str,
    this_host: &str,
) -> Result<AgentTarget, AuthorError> {
    let exact = specs
        .iter()
        .filter(|spec| spec.bus_id(this_host) == selector)
        .collect::<Vec<_>>();
    let matches = if exact.is_empty() {
        specs
            .iter()
            .filter(|spec| spec.identity == selector)
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match matches.as_slice() {
        [] => Err(AuthorError::new(
            "target-not-found",
            format!("no agent {selector:?} found in the selected catalog"),
        )),
        [spec] => Ok(AgentTarget {
            identity: spec.bus_id(this_host),
            agent_id: spec.effective_id(this_host),
            source_host: spec.resolved_host(this_host).to_owned(),
            source_identity: spec.identity.clone(),
            declaration: spec.path.clone(),
            retired: spec.desired_state.is_retired(),
        }),
        many => {
            let mut candidates = many
                .iter()
                .map(|spec| format!("{} ({})", spec.bus_id(this_host), spec.path.display()))
                .collect::<Vec<_>>();
            candidates.sort();
            Err(AuthorError::new(
                "target-ambiguous",
                format!(
                    "agent selector {selector:?} is ambiguous: {}",
                    candidates.join(", ")
                ),
            ))
        }
    }
}

fn authorize_actor(
    specs: &[crate::AgentSpec],
    target: &str,
    this_host: &str,
    actor: Option<&str>,
    refusal_code: &'static str,
) -> Result<(), AuthorError> {
    let Some(actor) = actor else {
        return Ok(());
    };
    if actor == target {
        return Ok(());
    }
    let by_identity = specs
        .iter()
        .map(|spec| (spec.bus_id(this_host), spec))
        .collect::<BTreeMap<_, _>>();
    let mut current = target.to_owned();
    let mut visited = BTreeSet::new();
    while visited.insert(current.clone()) {
        let Some(spec) = by_identity.get(&current) else {
            break;
        };
        let Some(supervisor) = spec.supervisor.as_deref() else {
            break;
        };
        if supervisor == actor {
            return Ok(());
        }
        let same_host = format!("{}.{}", spec.resolved_host(this_host), supervisor);
        let qualified = if by_identity.contains_key(supervisor) {
            supervisor.to_owned()
        } else if by_identity.contains_key(&same_host) {
            same_host
        } else {
            supervisor.to_owned()
        };
        if qualified == actor {
            return Ok(());
        }
        current = qualified;
    }
    Err(AuthorError::new(
        refusal_code,
        format!("agent {actor:?} may edit only itself or a declared descendant, not {target:?}"),
    ))
}

#[cfg(test)]
fn edit_declaration_for_test(
    path: &Path,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
    field: PresentationField,
    requested: Option<&str>,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    let control = path
        .parent()
        .expect("test declaration has a parent")
        .join(crate::catalog_lock::CONTROL_DIR);
    fs::create_dir_all(&control).expect("create test catalog control directory");
    let catalog_lock = CatalogLock::exclusive(path.parent().expect("test catalog has a parent"))
        .expect("acquire test catalog lock");
    edit_declaration(
        &catalog_lock,
        path.parent().expect("test catalog has a parent"),
        &control,
        path,
        expected_identity,
        expected_host,
        expected_agent,
        field.into(),
        requested,
        before_commit,
    )
}

#[cfg(test)]
fn edit_desired_state_for_test(
    path: &Path,
    state: DesiredStateValue,
    reason: Option<&str>,
    before_commit: impl FnOnce(),
) -> Result<AuthorOutcome, AuthorError> {
    let control = path
        .parent()
        .expect("test declaration has a parent")
        .join(crate::catalog_lock::CONTROL_DIR);
    fs::create_dir_all(&control).expect("create test catalog control directory");
    let catalog_lock = CatalogLock::exclusive(path.parent().expect("test catalog has a parent"))
        .expect("acquire test catalog lock");
    edit_desired_state_declaration(
        &catalog_lock,
        path.parent().expect("test catalog has a parent"),
        &control,
        path,
        "h.worker",
        "h",
        "worker",
        state,
        reason,
        None,
        before_commit,
    )
}


fn exact_agent_node<'a>(
    document: &'a KdlDocument,
    expected_identity: &str,
    expected_host: &str,
    expected_agent: &str,
) -> Result<&'a KdlNode, AuthorError> {
    let agents = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "agent")
        .collect::<Vec<_>>();
    let explicit = agents
        .iter()
        .copied()
        .filter(|node| {
            let (host, identity) = agent_identity_parts(node);
            identity.as_deref() == Some(expected_agent)
                && host.as_deref().is_none_or(|host| host == expected_host)
        })
        .collect::<Vec<_>>();
    let matches = if explicit.is_empty() {
        agents
            .into_iter()
            .filter(|node| {
                let (host, identity) = agent_identity_parts(node);
                identity.is_none() && host.as_deref().is_none_or(|host| host == expected_host)
            })
            .collect::<Vec<_>>()
    } else {
        explicit
    };
    match matches.as_slice() {
        [target] => Ok(*target),
        [] => Err(AuthorError::new(
            "target-changed",
            format!("declaration no longer contains agent {expected_identity:?}"),
        )),
        _ => Err(AuthorError::new(
            "target-ambiguous",
            format!("declaration contains more than one agent {expected_identity:?}"),
        )),
    }
}

fn agent_identity_parts(node: &KdlNode) -> (Option<String>, Option<String>) {
    let mut identity = node
        .get(0)
        .and_then(|value| value.as_string())
        .map(str::to_owned);
    let mut host = None;
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "identity" => {
                    identity = child
                        .get(0)
                        .and_then(|value| value.as_string())
                        .map(str::to_owned)
                        .or(identity);
                }
                "host" => {
                    host = child
                        .get(0)
                        .and_then(|value| value.as_string())
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
    }
    (host, identity)
}


fn quoted(value: &str) -> Result<String, AuthorError> {
    serde_json::to_string(value).map_err(|error| {
        AuthorError::new(
            "unsafe-source-edit",
            format!("encode presentation string for canonical KDL: {error}"),
        )
    })
}


fn insert_node(text: &str, target: &KdlNode, authored: &str) -> Result<String, AuthorError> {
    let span = target.span();
    let start = span.offset();
    let end = start + span.len();
    let source = text.get(start..end).ok_or_else(|| {
        AuthorError::new(
            "malformed-declaration",
            "agent span falls outside the declaration",
        )
    })?;
    let mut replacement = text.to_owned();
    if target.children().is_none() {
        replacement.insert_str(end, &format!(" {{ {authored} }}"));
        return Ok(replacement);
    }
    if !source.ends_with('}') {
        return Err(AuthorError::new(
            "unsafe-source-shape",
            "agent child block does not end at a source-preserving insertion point",
        ));
    }
    let close = source.len() - 1;
    if let Some(newline) = source[..close].rfind('\n') {
        let closing_indent = &source[newline + 1..close];
        if !closing_indent
            .chars()
            .all(|value| matches!(value, ' ' | '\t'))
        {
            return Err(AuthorError::new(
                "unsafe-source-shape",
                "cannot preserve a non-whitespace closing-brace prefix",
            ));
        }
        let child_indent = target
            .children()
            .and_then(|children| children.nodes().first())
            .and_then(|child| line_indent(text, child.span().offset()))
            .unwrap_or_else(|| format!("{closing_indent}  "));
        replacement.insert_str(start + newline + 1, &format!("{child_indent}{authored}\n"));
        return Ok(replacement);
    }
    let before_close = &source[..close];
    let trimmed = before_close.trim_end();
    let insertion = if trimmed.ends_with('{') {
        format!(" {authored}")
    } else if trimmed.ends_with(';') {
        format!(" {authored};")
    } else {
        format!("; {authored}")
    };
    replacement.insert_str(start + trimmed.len(), &insertion);
    Ok(replacement)
}

/// Whether the remainder of a node's own line is removable trivia: blanks, optionally followed by
/// a `//` line comment. A hand-authored `resource "work" uri="…" reason="…" // why` owns that
/// comment, so deleting the binding deletes its explanation with it. `/*` is deliberately not
/// accepted — a block comment can span lines, and this only ever sees one.
fn is_line_tail_trivia(tail: &str) -> bool {
    let rest = tail.trim_start_matches([' ', '\t', '\r']);
    rest.is_empty() || rest.starts_with("//")
}

fn remove_field(text: &str, node: &KdlNode) -> Result<String, AuthorError> {
    let span = node.span();
    let start = span.offset();
    let end = start + span.len();
    text.get(start..end).ok_or_else(|| {
        AuthorError::new(
            "malformed-declaration",
            "presentation field span falls outside the declaration",
        )
    })?;
    let line_start = text[..start].rfind('\n').map_or(0, |newline| newline + 1);
    let line_end = text[end..]
        .find('\n')
        .map_or(text.len(), |newline| end + newline);
    if text[line_start..start]
        .chars()
        .all(|value| matches!(value, ' ' | '\t'))
        && is_line_tail_trivia(&text[end..line_end])
    {
        let mut replacement = text.to_owned();
        let remove_end = usize::min(line_end + usize::from(line_end < text.len()), text.len());
        replacement.replace_range(line_start..remove_end, "");
        return Ok(replacement);
    }

    // A KDL node's span excludes leading trivia. Preserve an admitted inline
    // block comment by leaving that trivia on its line while removing only the
    // lifecycle declaration and the whitespace around it. Candidate parsing
    // below remains the final guard against accepting some other unsafe prefix.
    if text[end..line_end]
        .chars()
        .all(|value| matches!(value, ' ' | '\t' | '\r'))
    {
        let before = &text[line_start..start];
        let remove_start = line_start + before.trim_end_matches([' ', '\t']).len();
        let mut replacement = text.to_owned();
        replacement.replace_range(remove_start..line_end, "");
        return Ok(replacement);
    }

    let after = &text[end..line_end];
    let after_indent = after.len() - after.trim_start_matches([' ', '\t']).len();
    let after_content = end + after_indent;
    if text[after_content..line_end].starts_with(';') {
        let mut remove_end = after_content + 1;
        while remove_end < line_end
            && text.as_bytes()[remove_end].is_ascii_whitespace()
            && text.as_bytes()[remove_end] != b'\n'
            && text.as_bytes()[remove_end] != b'\r'
        {
            remove_end += 1;
        }
        let mut replacement = text.to_owned();
        replacement.replace_range(start..remove_end, "");
        return Ok(replacement);
    }

    let before = &text[line_start..start];
    let before_content = line_start + before.trim_end_matches([' ', '\t']).len();
    let preceding = text[..before_content].chars().next_back();
    let remove_start = match preceding {
        Some(';') => before_content - 1,
        Some('{') => start,
        _ => {
            return Err(AuthorError::new(
                "unsafe-source-shape",
                "compact presentation metadata has no adjacent KDL separator",
            ));
        }
    };
    let mut replacement = text.to_owned();
    replacement.replace_range(remove_start..end, "");
    Ok(replacement)
}

fn line_indent(text: &str, offset: usize) -> Option<String> {
    let prefix = text.get(..offset)?;
    let start = prefix.rfind('\n').map_or(0, |newline| newline + 1);
    let indent = prefix.get(start..)?;
    indent
        .chars()
        .all(|value| matches!(value, ' ' | '\t'))
        .then(|| indent.to_owned())
}


fn atomic_replace_checked(
    catalog_lock: &CatalogLock,
    catalog: &Path,
    control: &Path,
    path: &Path,
    original: &[u8],
    original_version: SourceVersion,
    replacement: &[u8],
    mode: u32,
    before_commit: impl FnOnce(),
) -> Result<(), AuthorError> {
    let directory = path.parent().ok_or_else(|| {
        AuthorError::new(
            "invalid-target",
            format!("declaration path {} has no parent", path.display()),
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix("agent-author-")
        .tempfile_in(control)
        .map_err(|error| {
            AuthorError::new(
                "declaration-write-failed",
                format!("staging declaration {}: {error}", path.display()),
            )
        })?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(mode))
        .and_then(|()| temporary.write_all(replacement))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| {
            AuthorError::new(
                "declaration-write-failed",
                format!("staging declaration {}: {error}", path.display()),
            )
        })?;
    test_crash_after_temporary_write();
    before_commit();
    let current = fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| (SourceVersion::from_metadata(&metadata), fs::read(path).ok()));
    if !matches!(current, Some((version, Some(bytes))) if version == original_version && bytes == original)
    {
        return Err(AuthorError::new(
            "source-changed",
            format!(
                "declaration {} changed while the edit was authored",
                path.display()
            ),
        ));
    }
    #[cfg(debug_assertions)]
    if std::env::var_os("ST2_TEST_AGENT_AUTHOR_FAIL_BEFORE_PUBLISH").is_some() {
        return Err(AuthorError::new(
            "declaration-write-failed",
            "injected declaration publication failure",
        ));
    }
    let generation = catalog_lock.begin_generation_commit().map_err(|error| {
        AuthorError::new(
            "declaration-write-failed",
            format!("prepare catalog generation: {error:#}"),
        )
    })?;
    if let Err(error) = crate::catalog_transaction::persist_tempfile_from_control(
        catalog_lock.control(),
        catalog,
        temporary,
        path,
    ) {
        return Err(AuthorError::new(
            "declaration-write-failed",
            format!(
                "atomically publishing declaration {}: {error}",
                path.display()
            ),
        ));
    }
    crate::catalog_transaction::open_dir_beneath(catalog, directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            AuthorError::new(
                "declaration-write-failed",
                format!(
                    "syncing declaration directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
    generation.commit().map_err(|error| {
        AuthorError::new(
            "declaration-write-failed",
            format!("advance catalog generation: {error:#}"),
        )
    })?;
    Ok(())
}

#[cfg(debug_assertions)]
fn test_crash_after_temporary_write() {
    if std::env::var_os("ST2_TEST_AGENT_AUTHOR_CRASH_AFTER_TEMP").is_some() {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn test_crash_after_temporary_write() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn declaration(
        identity: &str,
        host: &str,
        supervisor: Option<&str>,
        managed_by: &str,
    ) -> String {
        let supervisor = supervisor
            .map(|value| format!("  supervisor {value:?}\n"))
            .unwrap_or_default();
        format!(
            "// keep this comment\nagent {identity:?} {{\n  host {host:?}\n  meta {{ managed-by {managed_by:?}; keep \"exact\" }}\n{supervisor}  command \"sleep 60\"\n}}\n"
        )
    }

    #[test]
    fn source_preserving_set_replace_idempotent_and_clear() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        let before = fs::read_to_string(&path).unwrap();

        let set = set_presentation(
            root,
            "h.worker",
            "h",
            None,
            PresentationField::Name,
            Some("Build owner"),
        )
        .unwrap();
        assert_eq!(set.result, AuthorOutcome::Changed);
        let after_set = fs::read_to_string(&path).unwrap();
        assert_eq!(after_set.matches("name \"Build owner\"").count(), 1);
        assert_eq!(after_set.replace("  name \"Build owner\"\n", ""), before);

        assert_eq!(
            set_presentation(
                root,
                "worker",
                "h",
                None,
                PresentationField::Name,
                Some("Build owner")
            )
            .unwrap()
            .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(
            set_presentation(
                root,
                "worker",
                "h",
                None,
                PresentationField::Name,
                Some("Release owner")
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            set_presentation(root, "worker", "h", None, PresentationField::Name, None)
                .unwrap()
                .result,
            AuthorOutcome::Changed
        );
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn source_preserving_edit_accepts_a_dotted_host_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "us.east/worker/agent.kdl",
            &declaration("worker", "us.east", None, "catalog"),
        );

        let receipt = set_presentation(
            root,
            "us.east.worker",
            "elsewhere",
            None,
            PresentationField::Name,
            Some("Build owner"),
        )
        .unwrap();

        assert_eq!(receipt.identity, "us.east.worker");
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("name \"Build owner\"")
        );
    }

    #[test]
    fn source_preserving_clear_accepts_a_crlf_dedicated_field() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let original = declaration("worker", "h", None, "catalog").replace('\n', "\r\n");
        let with_name = original.replace(
            "  command \"sleep 60\"",
            "  name \"Build owner\"\r\n  command \"sleep 60\"",
        );
        let path = write(root, "h/worker/agent.kdl", &with_name);

        let receipt =
            set_presentation(root, "h.worker", "h", None, PresentationField::Name, None).unwrap();

        assert_eq!(receipt.result, AuthorOutcome::Changed);
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn self_and_supervisor_can_edit_but_sibling_and_nix_owner_cannot() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write(
            root,
            "h/root/agent.kdl",
            &declaration("root", "h", None, "catalog"),
        );
        write(
            root,
            "h/child/agent.kdl",
            &declaration("child", "h", Some("root"), "catalog"),
        );
        write(
            root,
            "h/sibling/agent.kdl",
            &declaration("sibling", "h", Some("root"), "catalog"),
        );
        write(
            root,
            "h/nix/agent.kdl",
            &declaration("nix", "h", Some("root"), "nix"),
        );

        set_presentation(
            root,
            "h.child",
            "h",
            Some("h.child"),
            PresentationField::Name,
            Some("self"),
        )
        .unwrap();
        set_presentation(
            root,
            "h.child",
            "h",
            Some("h.root"),
            PresentationField::Description,
            Some("supervised"),
        )
        .unwrap();
        assert_eq!(
            set_presentation(
                root,
                "h.sibling",
                "h",
                Some("h.child"),
                PresentationField::Name,
                Some("no")
            )
            .unwrap_err()
            .code(),
            "presentation-not-authorized"
        );
        assert_eq!(
            set_presentation(
                root,
                "h.nix",
                "h",
                Some("h.root"),
                PresentationField::Name,
                Some("no")
            )
            .unwrap_err()
            .code(),
            "nix-managed-declaration"
        );
    }

    #[test]
    fn stale_source_refuses_atomic_replace() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write(
            temporary.path(),
            "agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        let changed = declaration("worker", "h", None, "external");
        let error = edit_declaration_for_test(
            &path,
            "h.worker",
            "h",
            "worker",
            PresentationField::Name,
            Some("Owner"),
            || fs::write(&path, &changed).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "source-changed");
        assert_eq!(fs::read_to_string(path).unwrap(), changed);
    }

    #[test]
    fn source_version_rejects_byte_identical_aba_rewrite() {
        let temporary = tempfile::tempdir().unwrap();
        let original = declaration("worker", "h", None, "catalog");
        let path = write(temporary.path(), "agent.kdl", &original);
        let error = edit_declaration_for_test(
            &path,
            "h.worker",
            "h",
            "worker",
            PresentationField::Name,
            Some("Owner"),
            || {
                fs::write(&path, "temporary competing bytes").unwrap();
                fs::write(&path, &original).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "source-changed");
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn desired_state_authoring_refuses_a_stale_source() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write(
            temporary.path(),
            "agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        let changed = declaration("worker", "h", None, "external");
        let error = edit_desired_state_for_test(
            &path,
            DesiredStateValue::Suspended,
            Some("Waiting for capacity"),
            || fs::write(&path, &changed).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "source-changed");
        assert_eq!(fs::read_to_string(path).unwrap(), changed);
    }

    #[test]
    fn desired_state_authoring_prefers_an_explicit_target_over_an_anonymous_sibling() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write(
            temporary.path(),
            "agent.kdl",
            "agent \"worker\" { host \"h\"; command \"sleep 60\" }\nagent { host \"h\"; command \"sleep 60\" }\n",
        );

        let result = edit_desired_state_for_test(
            &path,
            DesiredStateValue::Suspended,
            Some("Waiting for capacity"),
            || {},
        )
        .unwrap();

        assert_eq!(result, AuthorOutcome::Changed);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "agent \"worker\" { host \"h\"; command \"sleep 60\"; desired-state \"suspended\" reason=\"Waiting for capacity\" }\nagent { host \"h\"; command \"sleep 60\" }\n"
        );
    }

    /// #473: a generator-owned declaration refuses ordinary authoring because the generator is the
    /// writer of those bytes — but the generator has exactly one transition it cannot express in
    /// its own source, since the source change being projected is the seat's removal. The
    /// assertion is the authority, and only an exact marker match is one.
    #[test]
    fn marker_matched_lifecycle_authority_is_exact_and_source_preserving() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write(
            root,
            "h/root/agent.kdl",
            &declaration("root", "h", None, "catalog"),
        );
        let projected = write(
            root,
            "h/nix/agent.kdl",
            &declaration("nix", "h", Some("root"), "nix"),
        );
        let plain = "agent \"plain\" {\n  host \"h\"\n  supervisor \"root\"\n  command \"sleep 60\"\n}\n";
        let unmarked = write(root, "h/plain/agent.kdl", plain);
        let original = fs::read_to_string(&projected).unwrap();
        let retire = |marker: Option<&str>, selector: &str| {
            set_desired_state(
                root,
                selector,
                "h",
                None,
                DesiredStateValue::Retired,
                Some("nix: no longer declared"),
                marker,
            )
        };

        // Unasserted authoring still refuses, and now names the assertion that would carry it.
        let refused = retire(None, "h.nix").unwrap_err();
        assert_eq!(refused.code(), "nix-managed-declaration");
        assert!(
            refused.to_string().contains("--managed-by \"nix\""),
            "{refused}"
        );

        // Every inexact assertion fails closed: wrong marker, unmarked subject, unusable marker.
        assert_eq!(
            retire(Some("catalog"), "h.nix").unwrap_err().code(),
            "managed-by-mismatch"
        );
        assert_eq!(
            retire(Some("nix"), "h.root").unwrap_err().code(),
            "managed-by-mismatch"
        );
        assert_eq!(
            retire(Some("nix"), "h.plain").unwrap_err().code(),
            "managed-by-unmarked"
        );
        assert_eq!(
            retire(Some(""), "h.nix").unwrap_err().code(),
            "invalid-managed-by"
        );
        assert_eq!(fs::read_to_string(&projected).unwrap(), original);

        // The matched assertion authors exactly the lifecycle line and nothing else.
        let receipt = retire(Some("nix"), "h.nix").unwrap();
        assert_eq!(receipt.result, AuthorOutcome::Changed);
        assert_eq!(receipt.managed_by.as_deref(), Some("nix"));
        let authored = fs::read_to_string(&projected).unwrap();
        assert_eq!(
            authored.replace(
                "  desired-state \"retired\" reason=\"nix: no longer declared\"\n",
                ""
            ),
            original,
            "only the lifecycle line may differ"
        );
        assert_eq!(
            retire(Some("nix"), "h.nix").unwrap().result,
            AuthorOutcome::Unchanged
        );

        // The same authority reverses it, restoring the projected bytes exactly.
        assert_eq!(
            set_desired_state(
                root,
                "h.nix",
                "h",
                None,
                DesiredStateValue::Running,
                None,
                Some("nix"),
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(fs::read_to_string(&projected).unwrap(), original);
        assert_eq!(fs::read_to_string(&unmarked).unwrap(), plain);
    }

    /// The marker-matched arm stands in for the CAS `agent publish` the projection would otherwise
    /// run, so it refuses what that publication refuses: a retirement leaving an active agent
    /// descended from a tombstone root is rejected by admission before any byte is written.
    #[test]
    fn marker_matched_retirement_refuses_a_candidate_admission_would_reject() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let projected = write(
            root,
            "h/root/agent.kdl",
            &declaration("root", "h", None, "nix"),
        );
        write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", Some("root"), "catalog"),
        );
        let original = fs::read_to_string(&projected).unwrap();

        let error = set_desired_state(
            root,
            "h.root",
            "h",
            None,
            DesiredStateValue::Retired,
            Some("nix: no longer declared"),
            Some("nix"),
        )
        .unwrap_err();

        assert_eq!(error.code(), "candidate-not-admissible");
        assert!(error.to_string().contains("[retired-root]"), "{error}");
        assert_eq!(fs::read_to_string(&projected).unwrap(), original);
    }

    #[test]
    fn stream_add_supports_external_command_and_argv_and_external_remove_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        let original = fs::read_to_string(&path).unwrap();

        assert_eq!(
            add_stream(root, "h.worker", "h", Some("h.worker"), "webhook", None)
                .unwrap()
                .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            add_stream(
                root,
                "h.worker",
                "h",
                Some("h.worker"),
                "github-ci",
                Some(StreamLaunch::Command("gh watch --repo st2".to_owned())),
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            add_stream(
                root,
                "h.worker",
                "h",
                Some("h.worker"),
                "tick",
                Some(StreamLaunch::Argv(vec![
                    "tick-source".to_owned(),
                    "--daily".to_owned()
                ])),
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            add_stream(root, "h.worker", "h", Some("h.worker"), "webhook", None)
                .unwrap()
                .result,
            AuthorOutcome::Unchanged
        );
        let authored = fs::read_to_string(&path).unwrap();
        assert!(authored.contains("stream \"webhook\" {}"));
        assert!(authored.contains("stream \"github-ci\" { command \"gh watch --repo st2\" }"));
        assert!(authored.contains("stream \"tick\" { argv \"tick-source\" \"--daily\" }"));

        assert_eq!(
            remove_stream(root, "h.worker", "h", None, "webhook")
                .unwrap()
                .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            remove_stream(root, "h.worker", "h", None, "webhook")
                .unwrap()
                .result,
            AuthorOutcome::Unchanged
        );
        let remaining = fs::read_to_string(path).unwrap();
        assert!(!remaining.contains("stream \"webhook\""));
        assert!(remaining.contains("stream \"github-ci\""));
        assert!(remaining.contains("stream \"tick\""));
        assert_ne!(remaining, original);
    }

    #[test]
    fn stream_candidate_verification_matches_the_exact_host_agent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "agents.kdl",
            "agent \"worker\" { host \"alpha\"; command \"sleep 60\"; stream \"existing\" {} }\nagent \"worker\" { host \"beta\"; command \"sleep 60\"; stream \"existing\" {} }\n",
        );

        assert_eq!(
            add_stream(
                root,
                "beta.worker",
                "beta",
                Some("beta.worker"),
                "webhook",
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(
            remove_stream(root, "beta.worker", "beta", Some("beta.worker"), "existing",)
                .unwrap()
                .result,
            AuthorOutcome::Changed
        );

        let authored = fs::read_to_string(path).unwrap();
        let document = KdlDocument::parse(&authored).unwrap();
        let agents = document.nodes();
        assert!(agents[0].to_string().contains("stream \"existing\""));
        assert!(!agents[0].to_string().contains("stream \"webhook\""));
        assert!(!agents[1].to_string().contains("stream \"existing\""));
        assert!(agents[1].to_string().contains("stream \"webhook\""));
    }

    #[test]
    fn stream_authoring_enforces_authority_nix_ownership_and_canonical_validation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write(
            root,
            "h/root/agent.kdl",
            &declaration("root", "h", None, "catalog"),
        );
        write(
            root,
            "h/child/agent.kdl",
            &declaration("child", "h", Some("root"), "catalog"),
        );
        write(
            root,
            "h/sibling/agent.kdl",
            &declaration("sibling", "h", Some("root"), "catalog"),
        );
        write(
            root,
            "h/nix/agent.kdl",
            &declaration("nix", "h", Some("root"), "nix"),
        );

        add_stream(root, "h.child", "h", Some("h.root"), "events", None).unwrap();
        assert_eq!(
            add_stream(root, "h.sibling", "h", Some("h.child"), "events", None)
                .unwrap_err()
                .code(),
            "stream-not-authorized"
        );
        assert_eq!(
            add_stream(root, "h.nix", "h", Some("h.root"), "events", None)
                .unwrap_err()
                .code(),
            "nix-managed-declaration"
        );
        assert_eq!(
            add_stream(root, "h.child", "h", None, "Bad Name", None)
                .unwrap_err()
                .code(),
            "invalid-stream"
        );
        assert_eq!(
            add_stream(
                root,
                "h.child",
                "h",
                None,
                "empty-argv",
                Some(StreamLaunch::Argv(Vec::new())),
            )
            .unwrap_err()
            .code(),
            "invalid-stream"
        );
    }

    #[test]
    fn stream_authoring_refuses_catalogs_with_concealed_declarations() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("catalog");
        let concealed = temporary.path().join("concealed");
        let declaration_path = write(
            &root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        write(
            &concealed,
            "agent.kdl",
            &declaration("shadow", "h", None, "catalog"),
        );
        symlink(&concealed, root.join("concealed-link")).unwrap();
        let original = fs::read(&declaration_path).unwrap();

        let error = add_stream(&root, "h.worker", "h", None, "events", None).unwrap_err();

        assert_eq!(error.code(), "catalog-malformed");
        assert!(
            error.to_string().contains("unobservable declaration entry"),
            "{error}"
        );
        assert_eq!(fs::read(declaration_path).unwrap(), original);
    }

    fn bound(root: &Path, identity: &str, name: &str) -> Resource {
        crate::discover(root)
            .specs
            .into_iter()
            .find(|spec| spec.identity == identity)
            .expect("catalog declares the agent")
            .resources
            .into_iter()
            .find(|resource| resource.name() == name)
            .expect("agent declares the binding")
    }

    #[test]
    fn resource_add_declares_updates_in_place_and_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );

        let added = add_resource_with_selector(
            root,
            "h.worker",
            "h",
            Some("h.worker"),
            "work",
            "github-issue://example/project/123",
            "release work item",
            None,
            None,
        )
        .unwrap();
        assert_eq!(added.result, AuthorOutcome::Changed);
        assert_eq!(added.identity, "h.worker");
        assert_eq!(added.inactive_reason, None);

        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "source",
            "worktree://github.com/example/project/change",
            "primary checkout",
            None,
            None,
        )
        .unwrap();
        let two_bindings = fs::read_to_string(&path).unwrap();

        // An identical request proves the binding rather than rewriting the declaration.
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.worker",
                "h",
                None,
                "work",
                "github-issue://example/project/123",
                "release work item",
                None,
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), two_bindings);

        // Re-declaring an existing name updates it in place, keeping its position and siblings.
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.worker",
                "h",
                None,
                "work",
                "github-issue://example/project/456",
                "follow-up work item",
                Some("superseded by the follow-up"),
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        let updated = fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("resource \"work\"").count(), 1);
        assert!(
            updated.find("resource \"work\"").unwrap()
                < updated.find("resource \"source\"").unwrap()
        );
        assert!(updated.contains("// keep this comment"));
        assert!(updated.contains("keep \"exact\""));

        let work = bound(root, "worker", "work");
        assert_eq!(work.uri(), "github-issue://example/project/456");
        assert_eq!(work.reason(), "follow-up work item");
        assert_eq!(work.inactive_reason(), Some("superseded by the follow-up"));
        assert_eq!(
            bound(root, "worker", "source").uri(),
            "worktree://github.com/example/project/change"
        );

        // The request declares the complete binding, so an omitted inactive-reason clears it.
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.worker",
                "h",
                None,
                "work",
                "github-issue://example/project/456",
                "follow-up work item",
                None,
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Changed
        );
        assert_eq!(bound(root, "worker", "work").inactive_reason(), None);
        let cleared = fs::read_to_string(&path).unwrap();
        assert!(!cleared.contains("inactive-reason"));
        assert_eq!(cleared.matches("resource \"work\"").count(), 1);
    }

    #[test]
    fn resource_add_proves_a_hand_authored_binding_without_rewriting_it() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            "agent \"worker\" {\n  host \"h\"\n  command \"sleep 60\"\n  \
             resource \"work\" reason=\"release work item\" uri=\"github-issue://example/project/123\"\n}\n",
        );
        let original = fs::read_to_string(&path).unwrap();

        // Hand-authored property order and spacing are proven, not re-rendered.
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.worker",
                "h",
                None,
                "work",
                "github-issue://example/project/123",
                "release work item",
                None,
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn resource_remove_is_idempotent_and_keeps_unrelated_bindings() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "work",
            "github-issue://example/project/123",
            "release work item",
            None,
            None,
        )
        .unwrap();
        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "source",
            "worktree://github.com/example/project/change",
            "primary checkout",
            None,
            None,
        )
        .unwrap();

        let removed = remove_resource(root, "h.worker", "h", Some("h.worker"), "work").unwrap();
        assert_eq!(removed.result, AuthorOutcome::Changed);
        assert_eq!(removed.name, "work");
        let after_remove = fs::read_to_string(&path).unwrap();

        assert_eq!(
            remove_resource(root, "h.worker", "h", None, "work")
                .unwrap()
                .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(
            remove_resource(root, "h.worker", "h", None, "never-declared")
                .unwrap()
                .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), after_remove);
        assert!(!after_remove.contains("resource \"work\""));
        assert!(after_remove.contains("resource \"source\""));
        assert!(after_remove.contains("// keep this comment"));
        assert_eq!(bound(root, "worker", "source").reason(), "primary checkout");
    }

    #[test]
    fn resource_rename_carries_the_binding_and_refuses_absent_or_colliding_names() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "work",
            "github-issue://example/project/123",
            "release work item",
            Some("merged and retained for traceability"),
            None,
        )
        .unwrap();
        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "source",
            "worktree://github.com/example/project/change",
            "primary checkout",
            None,
            None,
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();

        assert_eq!(
            rename_resource(root, "h.worker", "h", None, "work", "work")
                .unwrap()
                .result,
            AuthorOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);

        let renamed =
            rename_resource(root, "h.worker", "h", Some("h.worker"), "work", "task").unwrap();
        assert_eq!(renamed.result, AuthorOutcome::Changed);
        assert_eq!(renamed.old, "work");
        assert_eq!(renamed.new, "task");

        let task = bound(root, "worker", "task");
        assert_eq!(task.uri(), "github-issue://example/project/123");
        assert_eq!(task.reason(), "release work item");
        assert_eq!(
            task.inactive_reason(),
            Some("merged and retained for traceability")
        );
        let authored = fs::read_to_string(&path).unwrap();
        assert!(!authored.contains("resource \"work\""));
        assert!(
            authored.find("resource \"task\"").unwrap()
                < authored.find("resource \"source\"").unwrap()
        );

        assert_eq!(
            rename_resource(root, "h.worker", "h", None, "work", "elsewhere")
                .unwrap_err()
                .code(),
            "resource-not-found"
        );
        // An absent `old` refuses even when the rename would otherwise be a self-rename no-op.
        assert_eq!(
            rename_resource(root, "h.worker", "h", None, "absent", "absent")
                .unwrap_err()
                .code(),
            "resource-not-found"
        );
        assert_eq!(
            rename_resource(root, "h.worker", "h", None, "task", "source")
                .unwrap_err()
                .code(),
            "resource-already-exists"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), authored);
    }

    #[test]
    fn resource_authoring_enforces_validation_authority_and_nix_ownership() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write(
            root,
            "h/root/agent.kdl",
            &declaration("root", "h", None, "catalog"),
        );
        let child = write(
            root,
            "h/child/agent.kdl",
            &declaration("child", "h", Some("root"), "catalog"),
        );
        write(
            root,
            "h/sibling/agent.kdl",
            &declaration("sibling", "h", Some("root"), "catalog"),
        );
        let nix_owned = write(
            root,
            "h/nix/agent.kdl",
            &declaration("nix", "h", Some("root"), "nix"),
        );
        let untouched = fs::read_to_string(&nix_owned).unwrap();

        add_resource_with_selector(
            root,
            "h.child",
            "h",
            Some("h.root"),
            "work",
            "github-issue://example/project/1",
            "supervised work item",
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            add_resource_with_selector(
                root,
                "h.sibling",
                "h",
                Some("h.child"),
                "work",
                "github-issue://example/project/1",
                "reaching across the fleet",
                None,
                None,
            )
            .unwrap_err()
            .code(),
            "resource-not-authorized"
        );
        assert_eq!(
            remove_resource(root, "h.sibling", "h", Some("h.child"), "work")
                .unwrap_err()
                .code(),
            "resource-not-authorized"
        );
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.nix",
                "h",
                Some("h.root"),
                "work",
                "github-issue://example/project/1",
                "Nix owns this declaration",
                None,
                None,
            )
            .unwrap_err()
            .code(),
            "nix-managed-declaration"
        );

        // A catalog-relative carrier path is admitted since #345, so the refusals worth pinning are
        // the ones that escape the catalog, plus empty names and empty explanations.
        for (name, uri, reason, inactive_reason) in [
            ("absolute-path", "/etc/passwd", "escapes the catalog", None),
            ("parent-escape", "../outside", "escapes the catalog", None),
            ("spaced", "issue://example/a b", "unencoded space", None),
            ("", "issue://example/1", "empty name", None),
            ("blank-reason", "issue://example/1", "", None),
            (
                "blank-inactive",
                "issue://example/1",
                "still explained",
                Some(""),
            ),
        ] {
            let error = add_resource_with_selector(
                root,
                "h.child",
                "h",
                None,
                name,
                uri,
                reason,
                inactive_reason,
                None,
            )
            .unwrap_err();
            assert_eq!(error.code(), "invalid-resource", "{name}: {error}");
        }
        assert_eq!(
            fs::read_to_string(&child)
                .unwrap()
                .matches("resource ")
                .count(),
            1
        );

        // #345 widened the envelope: a catalog-relative carrier path is a valid binding uri.
        add_resource_with_selector(
            root,
            "h.child",
            "h",
            None,
            "carrier",
            "carriers/goal.md",
            "Catalog-relative carrier.",
            None,
            None,
        )
        .expect("a catalog-relative carrier path is admitted");
        assert_eq!(fs::read_to_string(&nix_owned).unwrap(), untouched);
    }

    #[test]
    fn resource_uri_is_preserved_byte_for_byte() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        write(
            root,
            "h/worker/agent.kdl",
            &declaration("worker", "h", None, "catalog"),
        );
        let exact = "vendor+Thing://Authority.Example/Exact%20Identity?Query=A%2Fb#Frag%20Ment";

        add_resource_with_selector(
            root,
            "h.worker",
            "h",
            None,
            "subject",
            exact,
            "exact vendor identity",
            None,
            None,
        )
        .unwrap();
        assert_eq!(bound(root, "worker", "subject").uri(), exact);

        // The rename path carries the identity across without normalizing it either.
        rename_resource(root, "h.worker", "h", None, "subject", "carried").unwrap();
        assert_eq!(bound(root, "worker", "carried").uri(), exact);

        // A byte-identical re-declaration is a proven no-op, not a rewrite.
        assert_eq!(
            add_resource_with_selector(
                root,
                "h.worker",
                "h",
                None,
                "carried",
                exact,
                "exact vendor identity",
                None,
                None,
            )
            .unwrap()
            .result,
            AuthorOutcome::Unchanged
        );
    }

    /// Direct ID mutation is the one thing no span-bounded field edit may do. `id` is what makes
    /// a subject the same subject across an address, name, description, host, or graph change, so
    /// the read-back gate compares it rather than trusting the edit that produced the candidate.
    #[test]
    fn no_field_edit_may_rewrite_the_immutable_agent_id() {
        let tampered =
            "agent \"worker\" { id \"b\"; host \"h\"; command \"true\"; name \"Owner\" }\n";
        let error = verify_candidate(
            tampered,
            "h.worker",
            "h",
            "worker",
            DeclaredField::Presentation(PresentationField::Name),
            Some("Owner"),
            Some("a"),
        )
        .unwrap_err();
        assert_eq!(error.code(), "agent-id-immutable");

        // Dropping the ID entirely is the same refusal: an unmigrated declaration is not a place
        // to park a subject whose ID the catalog already froze.
        let dropped = "agent \"worker\" { host \"h\"; command \"true\"; address \"ops\" }\n";
        assert_eq!(
            verify_candidate(
                dropped,
                "h.worker",
                "h",
                "worker",
                DeclaredField::Address,
                Some("ops"),
                Some("a"),
            )
            .unwrap_err()
            .code(),
            "agent-id-immutable"
        );

        // The identical edit with the ID carried through is admitted.
        let honest =
            "agent \"worker\" { id \"a\"; host \"h\"; command \"true\"; address \"ops\" }\n";
        verify_candidate(
            honest,
            "h.worker",
            "h",
            "worker",
            DeclaredField::Address,
            Some("ops"),
            Some("a"),
        )
        .unwrap();
    }
}
