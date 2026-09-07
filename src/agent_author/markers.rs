//! Ownership-marker authority: who may rewrite a declaration carrying `meta { managed-by }`.
//!
//! Moved verbatim out of `agent_author.rs`; the shared declaration-writer primitives (error
//! vocabulary, target resolution, KDL node location, span edits, atomic commit) stay there.

use super::*;

/// Every ownership marker this declaration carries, in source order.
///
/// A well-formed declaration carries at most one. Several is not a resolvable ownership claim, so
/// they are returned as-is and no assertion can match them.
pub(super) fn declared_markers(node: &KdlNode) -> Vec<&str> {
    node.children()
        .into_iter()
        .flat_map(|children| children.nodes())
        .filter(|child| child.name().value() == "meta")
        .filter_map(KdlNode::children)
        .flat_map(|meta| meta.nodes())
        .filter(|child| child.name().value() == "managed-by")
        .filter_map(|child| child.get(0).and_then(|value| value.as_string()))
        .collect()
}

pub(super) fn is_nix_managed(node: &KdlNode) -> bool {
    declared_markers(node).contains(&"nix")
}

/// Reject an ownership assertion no declaration could carry, before any lock or read.
///
/// A marker is compared byte-exactly against the declaration's own `meta { managed-by "..." }`
/// value, so an empty or padded assertion can only ever be a caller mistake.
pub(crate) fn validate_marker_assertion(asserted: Option<&str>) -> Result<(), AuthorError> {
    if let Some(marker) = asserted
        && (marker.is_empty() || marker.trim() != marker)
    {
        return Err(AuthorError::new(
            "invalid-managed-by",
            format!("asserted ownership marker {marker:?} is empty or padded"),
        ));
    }
    Ok(())
}

/// Every ownership marker the declaration source `bytes` carries, in source order.
///
/// `agent publish` replaces a whole declaration file rather than one node, so every marker any
/// `agent` node in the incumbent file declares is at stake in that publication and the union is
/// what an assertion has to resolve against (#486). Bytes that are not UTF-8 or do not parse as
/// KDL carry no discoverable claim and read as unmarked: a writer able to leave such bytes in a
/// declaration leaf already holds direct filesystem write authority over the file, which is
/// strictly stronger than any st2 write path this marker governs.
pub(crate) fn declaration_markers(bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let Ok(document) = text.parse::<KdlDocument>() else {
        return Vec::new();
    };
    document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "agent")
        .flat_map(|node| {
            declared_markers(node)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Decide whether `asserted` authorizes rewriting bytes carrying the `declared` markers.
///
/// `meta { managed-by "nix" }` says the Nix projection, not st2, is the writer of these bytes: an
/// edit made behind it is silently reverted on the next activation, which is why an unasserted
/// rewrite refuses (R25, decision 0003). Only that marker refuses; the others are labels on
/// declarations st2's own verbs and publishers are expected to rewrite.
///
/// The generator itself is the one writer that legitimately authors the declaration, and
/// `--managed-by` is how it says so. An assertion is admitted only when it names exactly the one
/// marker the declaration carries — a caller wrong about who owns the bytes is wrong about the
/// edit, so a mismatched marker, an unmarked declaration, and an unresolvable multi-marker
/// declaration all fail closed. Returns whether an assertion was matched.
pub(crate) fn authorize_asserted_marker(
    declared: &[&str],
    subject: &str,
    path: &Path,
    asserted: Option<&str>,
) -> Result<bool, AuthorError> {
    match (asserted, declared) {
        (None, _) if !declared.contains(&"nix") => Ok(false),
        (None, _) => Err(AuthorError::new(
            "nix-managed-declaration",
            format!(
                "agent {subject:?} is Nix-owned; edit its Nix source instead of {}, or pass --managed-by \"nix\" if you are that projection",
                path.display()
            ),
        )),
        (Some(asserted), [marker]) if *marker == asserted => Ok(true),
        (Some(asserted), []) => Err(AuthorError::new(
            "managed-by-unmarked",
            format!(
                "--managed-by {asserted:?} claims agent {subject:?}, whose declaration {} carries no `meta {{ managed-by }}` marker",
                path.display()
            ),
        )),
        (Some(asserted), markers) => Err(AuthorError::new(
            "managed-by-mismatch",
            format!(
                "--managed-by {asserted:?} does not own agent {subject:?}: {} declares owner {}",
                path.display(),
                markers
                    .iter()
                    .map(|marker| format!("{marker:?}"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        )),
    }
}
