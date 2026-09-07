//! Discovery — slurp a unified catalog+inbox folder and lower every agent spec it contains.
//!
//! The folder is *both* the catalog and the inboxes: one directory per agent holding its spec plus
//! its `inbox/`/`archive/`. Discovery walks `<root>` recursively, parses every `*.{toml,json,kdl}`
//! that looks like a spec, and resolves each spec's `identity`/`host` with the gist's precedence:
//! an explicit identity+host pair is path-independent; otherwise **content wins, the path supplies
//! defaults, and a mismatch is a warning**. Malformed files are collected as errors rather than
//! halting the walk — one bad edit must not wedge the whole reconcile.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::declared::{
    DeclaredDiagnosticCode, DeclaredParse, parse_declared_document,
};
use crate::spec::{AgentSpec, RawSpec};

/// The result of walking a catalog folder. Sorted + deterministic.
#[derive(Debug, Default)]
pub struct Discovered {
    /// Every spec that parsed and resolved an identity, sorted by path.
    pub specs: Vec<AgentSpec>,
    /// Non-fatal notes (identity/host path↔content mismatches, formats not yet supported).
    pub warnings: Vec<String>,
    /// Files that looked like specs but failed to parse/resolve — surfaced, never silently dropped.
    pub errors: Vec<SpecError>,
    /// Declaration parses retained so runner lowering and validation consume one immutable result.
    pub declarations: Vec<DiscoveredDeclaration>,
}

/// One declaration candidate parsed during discovery.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredDeclaration {
    pub path: PathBuf,
    /// Pre-lowering runner-significant fields for every declaration in the file.
    pub agents: Vec<Declared>,
    /// Full canonical KDL parse. `None` for legacy TOML/JSON declarations.
    pub parse: Option<DeclaredParse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError {
    pub path: PathBuf,
    pub message: String,
}

/// Extensions discovery will attempt to parse as specs.
const SPEC_EXTS: [&str; 3] = ["toml", "json", "kdl"];

thread_local! {
    /// This observer cannot see walks performed on another thread. If discovery gains a spawn
    /// boundary, dependent structural tests can silently lose coverage and must be reassessed.
    static DISCOVERY_WALK_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reset the current thread's discovery-walk count for structural tests in dependent crates.
#[doc(hidden)]
pub fn reset_discovery_walk_count_for_test() {
    DISCOVERY_WALK_COUNT.set(0);
}

/// Return the current thread's discovery-walk count for structural tests in dependent crates.
#[doc(hidden)]
pub fn discovery_walk_count_for_test() -> usize {
    DISCOVERY_WALK_COUNT.get()
}

/// Walk `root` recursively and lower every agent spec found. Returns empty (no error) when `root`
/// does not exist yet — a fresh, un-seeded folder is a valid state.
pub fn discover(root: &Path) -> Discovered {
    discover_impl(root, false)
}

/// Discover a catalog while treating directory traversal failures and entries that could conceal
/// declarations as uncertainty. Consumers that must prove a global property such as identity
/// uniqueness should use this mode.
pub fn discover_strict(root: &Path) -> Discovered {
    discover_impl(root, true)
}

/// Parse and lower one declaration file without walking its surrounding catalog.
///
/// `root` supplies the same path defaults as [`discover`]. The returned warnings describe only
/// this file.
pub fn discover_file(root: &Path, path: &Path) -> anyhow::Result<(Vec<AgentSpec>, Vec<String>)> {
    let raws = parse_raw_file(Some(root), path)?;
    load_specs(root, path, raws)
}

fn discover_impl(root: &Path, strict: bool) -> Discovered {
    DISCOVERY_WALK_COUNT.set(DISCOVERY_WALK_COUNT.get() + 1);
    let mut out = Discovered::default();
    let mut files = Vec::new();
    collect_spec_files(root, root, &mut files, strict, &mut out.errors);
    files.sort();
    for path in files {
        let ParsedRawFile { raws, declaration } =
            parse_raw_file_with_declaration(Some(root), &path);
        let agents = raws
            .as_ref()
            .map(|raws| raws.iter().map(Declared::from).collect())
            .unwrap_or_default();
        let is_generic_agent = path.file_stem().and_then(|stem| stem.to_str()) == Some("agent");
        let is_declaration = is_generic_agent
            || raws
                .as_ref()
                .is_ok_and(|raws| raws.iter().any(RawSpec::looks_like_spec));
        if is_declaration {
            out.declarations.push(DiscoveredDeclaration {
                path: path.clone(),
                agents,
                parse: declaration,
            });
        }
        match raws.and_then(|raws| load_specs(root, &path, raws)) {
            Ok((specs, warnings)) => {
                out.specs.extend(specs);
                out.warnings.extend(warnings);
            }
            Err(e) => out.errors.push(SpecError {
                path,
                message: e.to_string(),
            }),
        }
    }
    out
}

/// Whether `path` is in catalog declaration space rather than a known control/runtime namespace.
///
/// A leading dot has no generic meaning: organizational directories such as `.managed` and
/// `.retired` remain visible. `.git` and `.st2` control directories at any depth, the catalog root's
/// `pty` child, and an actual declaration parent's `resources`, `archive`, and `inbox` children have
/// explicit non-catalog meaning.
pub fn is_catalog_path(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let components: Vec<_> = rel
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();

    if components
        .iter()
        .any(|name| matches!(name.to_str(), Some(".git" | ".st2")))
        || components.first().and_then(|name| name.to_str()) == Some("pty")
    {
        return false;
    }

    // Canonical state has a stable address independent of whether a declaration is currently
    // present. This keeps orphan state after retirement/removal out of discovery and out of a
    // whole-catalog transaction's declaration identity.
    if components.first().and_then(|name| name.to_str()) == Some("agents")
        && components.len() >= 4
        && matches!(
            components[3].to_str(),
            Some("resources" | "archive" | "inbox" | "status")
        )
    {
        return false;
    }

    let mut parent = root.to_path_buf();
    for name in components {
        if matches!(name.to_str(), Some("resources" | "archive" | "inbox"))
            && is_declaration_parent(root, &parent)
        {
            return false;
        }
        parent.push(name);
    }
    true
}

/// Whether `dir` anchors at least one declaration whose adjacent state directories are not
/// recursively discoverable catalog input.
///
/// Generic `agent.*` filenames reserve the namespace even while malformed so a broken declaration
/// cannot suddenly expose its inbox as candidate specs. Named declaration files are recognized
/// only when they parse as an agent spec, which keeps ordinary project JSON/TOML/KDL from claiming
/// an unrelated `resources` directory.
fn is_declaration_parent(catalog_root: &Path, dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| SPEC_EXTS.contains(&extension))
        {
            return false;
        }
        if path.file_stem().and_then(|stem| stem.to_str()) == Some("agent") {
            return true;
        }
        parse_raw_file(Some(catalog_root), &path)
            .is_ok_and(|raws| raws.iter().any(RawSpec::looks_like_spec))
    })
}

/// Recursively gather candidate spec files, skipping explicit control/runtime namespaces, static
/// descendants beneath a canonical `agents/<host>/<identity>` bundle, and anything that isn't one
/// of [`SPEC_EXTS`]. `pty` session metadata includes JSON that can resemble an agent spec; it is
/// runner state, never catalog input. Unreadable directories are skipped, not fatal in ordinary
/// discovery. Strict discovery records them as uncertainty.
fn collect_spec_files(
    root: &Path,
    dir: &Path,
    acc: &mut Vec<PathBuf>,
    strict: bool,
    errors: &mut Vec<SpecError>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(error) => {
            if strict && !(dir == root && error.kind() == std::io::ErrorKind::NotFound) {
                errors.push(SpecError {
                    path: dir.to_path_buf(),
                    message: format!("catalog directory traversal failed: {error}"),
                });
            }
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if strict {
                    errors.push(SpecError {
                        path: dir.to_path_buf(),
                        message: format!("catalog directory entry traversal failed: {error}"),
                    });
                }
                continue;
            }
        };
        let path = entry.path();
        if !is_catalog_path(root, &path) {
            continue;
        }
        if is_canonical_bundle_descendant(root, &path) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(error) => {
                if strict {
                    errors.push(SpecError {
                        path,
                        message: format!("catalog entry type inspection failed: {error}"),
                    });
                }
                continue;
            }
        };
        if ft.is_dir() {
            collect_spec_files(root, &path, acc, strict, errors);
        } else if ft.is_file() && has_spec_extension(&path) {
            acc.push(path);
        } else if strict && unobservable_entry_may_hide_declaration(root, &path, ft) {
            errors.push(SpecError {
                path,
                message: "unobservable declaration entry: neither a regular file nor an independently traversed catalog directory".to_string(),
            });
        }
    }
}

/// Files below the canonical agent bundle root are resource/static payload, even when a nested
/// payload happens to use a generic declaration filename such as `docs/agent.kdl`.
fn is_canonical_bundle_descendant(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components().filter_map(|component| match component {
        Component::Normal(name) => Some(name),
        _ => None,
    });
    components.next().and_then(|name| name.to_str()) == Some("agents")
        && components.count() >= 4
}

fn has_spec_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| SPEC_EXTS.contains(&extension))
}

/// A directory symlink is safe only when its real target is already inside the catalog walk.
/// Declaration-shaped special files and links whose targets cannot be proven independently
/// observable keep strict consumers from claiming a complete catalog.
fn unobservable_entry_may_hide_declaration(
    root: &Path,
    path: &Path,
    file_type: fs::FileType,
) -> bool {
    if has_spec_extension(path) {
        return true;
    }
    if !file_type.is_symlink() {
        return false;
    }

    let Ok(target) = path.canonicalize() else {
        return true;
    };
    if !target.is_dir() {
        return false;
    }

    let Ok(canonical_root) = root.canonicalize() else {
        return true;
    };
    !target.starts_with(&canonical_root) || !is_catalog_path(&canonical_root, &target)
}

/// What a declaration literally *says*, before lowering normalizes it away.
///
/// Lowering is lossy by design: a typo'd `type = "srvice"` becomes `JobType::Service`, and an
/// identity omitted from the content is filled in from the path. Both are invisible in the resolved
/// [`AgentSpec`], so a linter that wants to fault them has to see the declared form. This is that
/// view — deliberately narrow, so the permissive on-disk shape itself stays private and is free to
/// gain fields without breaking readers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declared {
    /// `id` as written in the file. `None` on a legacy declaration with no explicit agent ID.
    pub id: Option<String>,
    /// `address` as written in the file. `None` when `identity` is the effective legacy address.
    pub address: Option<String>,
    /// `identity` as written in the file. `None` when the file relies on [`path_defaults`].
    pub identity: Option<String>,
    /// `host` as written in the file. `None` when the file relies on [`path_defaults`].
    pub host: Option<String>,
    /// `type` as written, before it is normalized to `JobType::Service`. `None` when unset.
    pub job_type: Option<String>,
}

impl From<&RawSpec> for Declared {
    fn from(raw: &RawSpec) -> Self {
        Self {
            id: raw.id.clone(),
            address: raw.address.clone(),
            identity: raw.identity.clone(),
            host: raw.host.clone(),
            job_type: raw.job_type.clone(),
        }
    }
}

/// Read the declared (pre-lowering) values of every agent in a file — one per `agent` node for KDL,
/// 0-or-1 for TOML/JSON, empty for a non-spec extension.
///
/// One entry per parsed node, *including* nodes [`discover`] skips as non-specs, so this is not
/// positionally paired with that file's [`Discovered::specs`].
pub fn parse_declared(path: &Path) -> anyhow::Result<Vec<Declared>> {
    Ok(parse_raw_file(None, path)?
        .iter()
        .map(Declared::from)
        .collect())
}

/// Parse a spec file into its raw (pre-resolution) shape — one per `agent` node for KDL, 0-or-1 for
/// TOML/JSON. Non-spec extensions yield an empty vec. Shared by discovery and [`parse_declared`]
/// (which exposes the *raw* `type` and `identity` before normalization, without leaking [`RawSpec`]).
fn parse_raw_file(catalog_root: Option<&Path>, path: &Path) -> anyhow::Result<Vec<RawSpec>> {
    parse_raw_file_with_declaration(catalog_root, path).raws
}

struct ParsedRawFile {
    raws: anyhow::Result<Vec<RawSpec>>,
    declaration: Option<DeclaredParse>,
}

fn parse_raw_file_with_declaration(catalog_root: Option<&Path>, path: &Path) -> ParsedRawFile {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return ParsedRawFile {
                raws: Err(error.into()),
                declaration: None,
            };
        }
    };
    if ext == "kdl" {
        let mut declaration = parse_declared_document(path, &text);
        admit_catalog_envelope_nodes(catalog_root, path, &mut declaration);
        let is_adjacent_kdl = declaration
            .document
            .as_ref()
            .is_some_and(|document| document.agents.is_empty())
            && path.file_stem().and_then(|stem| stem.to_str()) != Some("agent");
        let raws = if declaration.is_valid() || is_adjacent_kdl {
            crate::kdl_format::lower_declared_document(
                declaration
                    .document
                    .as_ref()
                    .expect("valid or adjacent KDL has a document"),
            )
        } else if declaration.document.is_none() {
            let detail = declaration
                .diagnostics
                .first()
                .map(|diagnostic| diagnostic.message.as_str())
                .unwrap_or("invalid KDL syntax");
            Err(anyhow::anyhow!("KDL parse error: {detail}"))
        } else {
            let detail = declaration
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == crate::DeclaredSeverity::Error)
                .map(|diagnostic| format!("[{}]: {}", diagnostic.code, diagnostic.message))
                .collect::<Vec<_>>()
                .join("\n");
            Err(anyhow::anyhow!("KDL declaration error: {detail}"))
        };
        return ParsedRawFile {
            raws,
            declaration: Some(declaration),
        };
    }
    let raws = match ext {
        "toml" => toml::from_str(&text)
            .map(|raw| vec![raw])
            .map_err(|error| anyhow::anyhow!("TOML parse error: {error}")),
        "json" => serde_json::from_str(&text)
            .map(|raw| vec![raw])
            .map_err(|error| anyhow::anyhow!("JSON parse error: {error}")),
        _ => Ok(Vec::new()),
    };
    ParsedRawFile {
        raws,
        declaration: None,
    }
}

/// The catalog root's `catalog.kdl` is a shared envelope: st2 owns its `catalog`/`profile` nodes
/// while Agent Spec discovery owns any colocated `agent` nodes. Suppress only the top-level
/// diagnostics attached to those two explicitly admitted envelope node kinds; a nested file that
/// merely shares the basename has no catalog-control-plane authority.
fn admit_catalog_envelope_nodes(
    catalog_root: Option<&Path>,
    path: &Path,
    declaration: &mut DeclaredParse,
) {
    let Some(catalog_root) = catalog_root else {
        return;
    };
    if path.strip_prefix(catalog_root).ok() != Some(Path::new("catalog.kdl")) {
        return;
    }
    let Some(document) = declaration.document.as_ref() else {
        return;
    };
    let admitted_spans = document
        .nodes
        .iter()
        .filter(|node| matches!(node.name.as_str(), "catalog" | "profile"))
        .map(|node| node.span)
        .collect::<Vec<_>>();
    declaration.diagnostics.retain(|diagnostic| {
        diagnostic.code != DeclaredDiagnosticCode::UnexpectedTopLevelNode
            || !admitted_spans.contains(&diagnostic.span)
    });
}

/// Parse one file into `(specs, warnings)`. TOML/JSON yield 0-or-1 spec; KDL yields one per `agent`
/// node. Non-spec files yield an empty vec; a malformed file is an `Err` (collected, never fatal).
fn load_specs(
    root: &Path,
    path: &Path,
    raws: Vec<RawSpec>,
) -> anyhow::Result<(Vec<AgentSpec>, Vec<String>)> {
    let mut specs = Vec::new();
    let mut warnings = Vec::new();
    for raw in raws {
        if let Some((spec, mut warns)) = resolve_spec(root, path, raw)? {
            specs.push(spec);
            warnings.append(&mut warns);
        }
    }
    Ok((specs, warnings))
}

/// Apply identity/host precedence to one raw spec. An explicit pair is authoritative and
/// path-independent. When either is omitted, content still wins over path-derived defaults and a
/// mismatch warns. Returns `None` for a non-spec (no agent signal); `Err` when it looks like a spec
/// but no identity can be resolved from content or path.
fn resolve_spec(
    root: &Path,
    path: &Path,
    raw: RawSpec,
) -> anyhow::Result<Option<(AgentSpec, Vec<String>)>> {
    if !raw.looks_like_spec() {
        return Ok(None); // random config in the tree — not an agent spec
    }

    let (path_identity, path_host) = path_defaults(root, path);
    let explicit_placement = raw.identity.is_some() && raw.host.is_some();
    let mut warnings = Vec::new();

    let identity = match (raw.identity.clone(), path_identity) {
        (Some(c), Some(p)) if c != p && !explicit_placement => {
            warnings.push(format!(
                "{}: identity mismatch — content '{c}' vs path '{p}'; using content",
                path.display()
            ));
            c
        }
        (Some(c), _) => c,
        (None, Some(p)) => p,
        (None, None) => {
            anyhow::bail!(
                "{}: spec has no identity in content or path",
                path.display()
            )
        }
    };

    let host = match (raw.host.clone(), path_host) {
        (Some(c), Some(p)) if c != p && !explicit_placement => {
            warnings.push(format!(
                "{}: host mismatch — content '{c}' vs path '{p}'; using content",
                path.display()
            ));
            Some(c)
        }
        (Some(c), _) => Some(c),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    };

    let spec = raw.into_agent_spec(identity, host, path.to_path_buf())?;
    Ok(Some((spec, warnings)))
}

/// Derive `(identity, host)` defaults from the file's path under `root`.
///
/// The canonical layout is `<root>/<host>/<identity>/agent.<ext>` (a generic `agent` filename), but
/// a file named for the agent (`<root>/[host/]<identity>.<ext>`) works too. The rule: a generic
/// `agent` stem takes identity from its parent dir and host from its grandparent; any other stem is
/// itself the identity and takes host from its parent dir. Content still overrides both.
///
/// Exposed for `validate`, which re-derives the path defaults to reconstruct an identity/host
/// path↔content mismatch structurally (the resolved [`AgentSpec`] no longer records which source won).
pub fn path_defaults(root: &Path, file: &Path) -> (Option<String>, Option<String>) {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dirs: Vec<String> = rel
        .parent()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => s.to_str().map(|s| s.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    if stem == "agent" {
        // Generic filename: identity = last dir, host = second-to-last dir.
        let identity = dirs.last().cloned();
        let host = if dirs.len() >= 2 {
            Some(dirs[dirs.len() - 2].clone())
        } else {
            None
        };
        (identity, host)
    } else {
        // File named for the agent: identity = stem, host = its parent dir (if any).
        let identity = if stem.is_empty() {
            None
        } else {
            Some(stem.to_string())
        };
        let host = dirs.last().cloned();
        (identity, host)
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn pd(root: &str, file: &str) -> (Option<String>, Option<String>) {
        path_defaults(Path::new(root), Path::new(file))
    }

    #[test]
    fn canonical_layout_host_identity_agent() {
        assert_eq!(
            pd("/cat", "/cat/hetz/st2-claude/agent.kdl"),
            (Some("st2-claude".into()), Some("hetz".into()))
        );
    }

    #[test]
    fn identity_folder_generic_name_no_host() {
        assert_eq!(
            pd("/cat", "/cat/fabric-claude/agent.toml"),
            (Some("fabric-claude".into()), None)
        );
    }

    #[test]
    fn flat_file_named_for_agent() {
        assert_eq!(
            pd("/cat", "/cat/fabric-claude.toml"),
            (Some("fabric-claude".into()), None)
        );
    }

    #[test]
    fn agent_named_file_inside_host_folder() {
        assert_eq!(
            pd("/cat", "/cat/hetz/fabric-claude.toml"),
            (Some("fabric-claude".into()), Some("hetz".into()))
        );
    }
}
