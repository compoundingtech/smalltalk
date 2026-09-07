//! The catalog's own declaration — `<catalog>/catalog.kdl`.
//!
//! Every other file in a catalog describes an agent; this one describes the folder. Three things
//! are declarable today: the session registry, how long a retired seat is kept in the live
//! catalog, and resource profiles —
//!
//! ```kdl
//! catalog {
//!   pty-root "/run/agents/pty"
//!   archive-after "7d"
//! }
//!
//! // One wasm resolver per URI scheme; `class` (optional, default coalesced) decides how
//! // carriers resolved through the profile notify. Paths anchor at the catalog root.
//! profile "dev.schickling.agent-goal" {
//!   wasm "resolvers/goal.wasm"
//!   class "immediate"
//! }
//! ```
//!
//! It is deliberately not a spec. `catalog` and `profile` are not `agent` nodes, so discovery
//! lowers nothing from them, and `eval_spec::parse_spec` rejects both as top-level nodes, so a
//! catalog that declares them is still dispatched as a catalog and never mistaken for a
//! single-file team spec.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "wasm-resolver")]
use agent_spec::profile::ProfileCapability;
use agent_spec::profile::{ProfileClass, ResourceProfile, ResourceProfileRegistry};
use anyhow::Context as _;
use kdl::KdlDocument;

/// The catalog-level declaration, read from the catalog root.
pub const CONFIG_FILE: &str = "catalog.kdl";
/// One declared resource profile with a closed component runtime and optional demand observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredProfile {
    /// The URI scheme this profile resolves.
    pub scheme: String,
    /// Path of the resolver `.wasm`, anchored at the catalog root when relative.
    pub wasm: String,
    /// How carriers resolved through this profile notify; defaults to coalesced.
    pub class: ProfileClass,
    /// Whether a binding through this profile also subscribes to its ancestors' same-scheme
    /// carriers; defaults to off.
    pub notify_chain: bool,
    /// Immutable provider component and its one typed host capability.
    pub runtime: Option<DeclaredProfileRuntime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredProfileRuntime {
    pub component: String,
    pub capability: DeclaredProviderCapability,
    /// Opt in to demand-driven observation through one atomic observation result.
    pub demand: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredProviderCapability {
    GitHubIssue {
        auth_executable: String,
        connect_timeout_ms: u64,
        total_timeout_ms: u64,
    },
    GitHubPr {
        auth_executable: String,
        connect_timeout_ms: u64,
        total_timeout_ms: u64,
    },
    PtyStats {
        executable: String,
        cwd: String,
        deadline_ms: u64,
    },
    Vista {
        executable: String,
        cwd: String,
        deadline_ms: u64,
    },
}

/// What `<catalog>/catalog.kdl` declares. An absent file leaves every field empty.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CatalogConfig {
    /// The `pty` session registry holding this catalog's tasks. Relative values anchor at the
    /// catalog root; `$VAR`/`$CATALOG` are expanded at use.
    pub pty_root: Option<String>,
    /// How long a retired seat stays in the live catalog before the supervisor archives it.
    /// Absent means [`DEFAULT_ARCHIVE_AFTER`]; `Duration::ZERO` disables auto-archive.
    pub archive_after: Option<Duration>,
    /// Resource profiles in declaration order.
    pub profiles: Vec<DeclaredProfile>,
}

/// The grace period an undeclared `archive-after` means: a week of hindsight before a retired seat
/// leaves the live catalog, which is long enough that un-retiring stays a normal edit.
pub const DEFAULT_ARCHIVE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The declared retirement grace period, or the default when `catalog.kdl` says nothing.
///
/// `Duration::ZERO` is the operator's off switch, so it is preserved rather than defaulted: an
/// explicit `archive-after "0"` disables auto-archive without disabling `st2 catalog archive`.
pub fn archive_after(config: &CatalogConfig) -> Duration {
    config.archive_after.unwrap_or(DEFAULT_ARCHIVE_AFTER)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedProfileModule {
    CatalogRelative(PathBuf),
    External(PathBuf),
}

/// `<catalog>/catalog.kdl`.
pub fn config_path(catalog_root: &Path) -> PathBuf {
    catalog_root.join(CONFIG_FILE)
}

/// Parse a catalog declaration.
///
/// An unknown child of `catalog{}` is an error rather than ignored: a typo'd `pty_root` would
/// silently resolve back to `<catalog>/pty` and reappear as a live agent whose task reads dead —
/// exactly the split registry this declaration exists to prevent. Its value set is closed, so the
/// lint cannot fire on a render-only field st2 ignores by design.
///
/// The top-level vocabulary is closed: `catalog` and `profile` belong to this parser, while a
/// colocated `agent` belongs to discovery. Rejecting every other node keeps misspelled profile
/// declarations from silently disappearing.
pub fn parse(text: &str) -> anyhow::Result<CatalogConfig> {
    let doc = KdlDocument::parse(text).map_err(|e| anyhow::anyhow!("KDL parse error: {e}"))?;
    let mut config = CatalogConfig::default();
    let mut seen_catalog = false;
    let mut seen_schemes = BTreeSet::new();

    for node in doc.nodes() {
        match node.name().value() {
            "catalog" => {
                if seen_catalog {
                    anyhow::bail!("catalog block declared more than once");
                }
                seen_catalog = true;
                parse_catalog_node(node, &mut config)?;
            }
            "profile" => {
                let profile = parse_profile(node)?;
                if !seen_schemes.insert(profile.scheme.clone()) {
                    anyhow::bail!("profile '{}' declared more than once", profile.scheme);
                }
                config.profiles.push(profile);
            }
            "agent" => {}
            other => anyhow::bail!(
                "unknown catalog.kdl top-level node '{other}' (expected catalog, profile, or agent)"
            ),
        }
    }
    Ok(config)
}

fn parse_catalog_node(node: &kdl::KdlNode, config: &mut CatalogConfig) -> anyhow::Result<()> {
    let Some(children) = node.children() else {
        return Ok(());
    };
    for child in children.nodes() {
        match child.name().value() {
            "pty-root" => {
                anyhow::ensure!(
                    config.pty_root.is_none(),
                    "pty-root declared more than once"
                );
                let value = child
                    .get(0)
                    .and_then(|v| v.as_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "pty-root needs a non-empty path, e.g. pty-root \"/run/agents/pty\""
                        )
                    })?;
                config.pty_root = Some(value.to_string());
            }
            // A malformed grace period refuses the whole declaration rather than falling back to
            // the default: silently archiving on a 7-day clock the operator did not write is the
            // one outcome this setting exists to prevent.
            "archive-after" => {
                anyhow::ensure!(
                    config.archive_after.is_none(),
                    "archive-after declared more than once"
                );
                let value = child
                    .get(0)
                    .and_then(|v| v.as_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "archive-after needs a quoted duration, e.g. archive-after \"7d\" (\"0\" disables auto-archive)"
                        )
                    })?;
                let parsed = agent_spec::spec::parse_duration(value)
                    .map_err(|error| anyhow::anyhow!("archive-after: {error}"))?;
                config.archive_after = Some(parsed);
            }
            other => anyhow::bail!(
                "unknown catalog field '{other}' (expected pty-root or archive-after)"
            ),
        }
    }
    Ok(())
}

fn parse_profile(node: &kdl::KdlNode) -> anyhow::Result<DeclaredProfile> {
    if node.entries().len() != 1 {
        anyhow::bail!("profile takes exactly one quoted URI scheme and no properties");
    }
    let scheme = node
        .get(0)
        .and_then(|v| v.as_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile needs a non-empty URI scheme, e.g. \
                 profile \"dev.example.goal\" {{ wasm \"resolvers/goal.wasm\" }}"
            )
        })?;
    let scheme_ok = scheme
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && !scheme.contains('/')
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok {
        anyhow::bail!("profile '{scheme}' is not a valid URI scheme");
    }

    let Some(children) = node.children() else {
        anyhow::bail!("profile '{scheme}' needs a wasm child naming its resolver module");
    };
    let mut wasm: Option<String> = None;
    let mut class = ProfileClass::Coalesced;
    let mut seen_class = false;
    let mut notify_chain = false;
    let mut seen_notify_chain = false;
    let mut runtime = None;
    for child in children.nodes() {
        if child.name().value() == "runtime" {
            if runtime.is_some() {
                anyhow::bail!("profile '{scheme}' declares runtime more than once");
            }
            if !child.entries().is_empty() {
                anyhow::bail!("profile '{scheme}': runtime takes no values or properties");
            }
            let runtime_children = child.children().ok_or_else(|| {
                anyhow::anyhow!(
                    "profile '{scheme}': runtime needs a component and one typed capability"
                )
            })?;
            let mut component = None;
            let mut capability = None;
            let mut demand = false;
            let mut seen_demand = false;
            for runtime_child in runtime_children.nodes() {
                match runtime_child.name().value() {
                    "component" => {
                        anyhow::ensure!(
                            component.is_none()
                                && runtime_child.children().is_none()
                                && runtime_child.entries().len() == 1,
                            "profile '{scheme}': runtime needs exactly one component path"
                        );
                        component = runtime_child
                            .get(0)
                            .and_then(|value| value.as_string())
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned);
                        anyhow::ensure!(
                            component.is_some(),
                            "profile '{scheme}': component path must be a non-empty quoted string"
                        );
                    }
                    "demand" => {
                        anyhow::ensure!(
                            !seen_demand,
                            "profile '{scheme}': demand is declared more than once"
                        );
                        anyhow::ensure!(
                            runtime_child.children().is_none()
                                && runtime_child.entries().len() == 1,
                            "profile '{scheme}': demand takes one boolean value"
                        );
                        demand = runtime_child
                            .get(0)
                            .and_then(|value| value.as_bool())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "profile '{scheme}': demand takes one boolean value"
                                )
                            })?;
                        seen_demand = true;
                    }
                    "github-issue" => {
                        anyhow::ensure!(
                            capability.is_none() && runtime_child.children().is_none(),
                            "profile '{scheme}': runtime declares more than one capability"
                        );
                        capability = Some(parse_github_issue_capability(scheme, runtime_child)?);
                    }
                    "github-pr" => {
                        anyhow::ensure!(
                            capability.is_none() && runtime_child.children().is_none(),
                            "profile '{scheme}': runtime declares more than one capability"
                        );
                        capability = Some(parse_github_pr_capability(scheme, runtime_child)?);
                    }
                    "pty-stats" => {
                        anyhow::ensure!(
                            capability.is_none() && runtime_child.children().is_none(),
                            "profile '{scheme}': runtime declares more than one capability"
                        );
                        capability = Some(parse_pty_stats_capability(scheme, runtime_child)?);
                    }
                    "vista" => {
                        anyhow::ensure!(
                            capability.is_none() && runtime_child.children().is_none(),
                            "profile '{scheme}': runtime declares more than one capability"
                        );
                        capability = Some(parse_vista_capability(scheme, runtime_child)?);
                    }
                    other => anyhow::bail!(
                        "profile '{scheme}': runtime field '{other}' is unknown \
                         (expected component, demand, github-issue, github-pr, pty-stats, or vista)"
                    ),
                }
            }
            runtime = Some(DeclaredProfileRuntime {
                component: component.ok_or_else(|| {
                    anyhow::anyhow!("profile '{scheme}': runtime needs exactly one component path")
                })?,
                capability: capability.ok_or_else(|| {
                    anyhow::anyhow!(
                        "profile '{scheme}': runtime needs exactly one typed capability"
                    )
                })?,
                demand,
            });
            continue;
        }
        if child.children().is_some() {
            anyhow::bail!(
                "profile '{scheme}': '{}' does not accept a child block",
                child.name().value()
            );
        }
        // KDL folds value fields written without separators into one node. Reject extra entries.
        if child.entries().len() != 1 {
            anyhow::bail!(
                "profile '{scheme}': '{}' takes exactly one quoted value",
                child.name().value()
            );
        }
        match child.name().value() {
            "wasm" => {
                if wasm.is_some() {
                    anyhow::bail!("profile '{scheme}' declares wasm more than once");
                }
                let value = child
                    .get(0)
                    .and_then(|v| v.as_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "profile '{scheme}' needs a non-empty module path, e.g. \
                             wasm \"resolvers/goal.wasm\""
                        )
                    })?;
                wasm = Some(value.to_string());
            }
            "class" => {
                if seen_class {
                    anyhow::bail!("profile '{scheme}' declares class more than once");
                }
                seen_class = true;
                let value = child.get(0).and_then(|v| v.as_string()).unwrap_or("");
                class = ProfileClass::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "profile '{scheme}': unknown class '{value}' (expected immediate, coalesced, or silent)"
                    )
                })?;
            }
            "notify-chain" => {
                if seen_notify_chain {
                    anyhow::bail!("profile '{scheme}' declares notify-chain more than once");
                }
                seen_notify_chain = true;
                notify_chain = child.get(0).and_then(|v| v.as_bool()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "profile '{scheme}': notify-chain takes a boolean, e.g. notify-chain #true"
                    )
                })?;
            }
            other => anyhow::bail!(
                "unknown profile field '{other}' in profile '{scheme}' \
                 (expected wasm, class, notify-chain, or runtime)"
            ),
        }
    }
    let Some(wasm) = wasm else {
        anyhow::bail!("profile '{scheme}' needs a wasm child naming its resolver module");
    };
    Ok(DeclaredProfile {
        scheme: scheme.to_owned(),
        wasm,
        class,
        notify_chain,
        runtime,
    })
}

fn parse_github_issue_capability(
    scheme: &str,
    node: &kdl::KdlNode,
) -> anyhow::Result<DeclaredProviderCapability> {
    anyhow::ensure!(
        node.entries().len() == 3 && node.entries().iter().all(|entry| entry.name().is_some()),
        "profile '{scheme}': github-issue requires auth-executable, connect-timeout-ms, and \
         total-timeout-ms properties"
    );
    let auth_executable = required_string_property(scheme, node, "auth-executable")?;
    anyhow::ensure!(
        Path::new(&auth_executable).is_absolute(),
        "profile '{scheme}': GitHub authentication executable must be absolute"
    );
    let connect_timeout_ms = required_u64_property(scheme, node, "connect-timeout-ms")?;
    let total_timeout_ms = required_u64_property(scheme, node, "total-timeout-ms")?;
    anyhow::ensure!(
        connect_timeout_ms > 0
            && connect_timeout_ms <= total_timeout_ms
            && total_timeout_ms <= 60_000,
        "profile '{scheme}': GitHub deadlines must be positive, ordered, and at most 60000ms"
    );
    Ok(DeclaredProviderCapability::GitHubIssue {
        auth_executable,
        connect_timeout_ms,
        total_timeout_ms,
    })
}

fn parse_github_pr_capability(
    scheme: &str,
    node: &kdl::KdlNode,
) -> anyhow::Result<DeclaredProviderCapability> {
    anyhow::ensure!(
        node.entries().len() == 3 && node.entries().iter().all(|entry| entry.name().is_some()),
        "profile '{scheme}': github-pr requires auth-executable, connect-timeout-ms, and \
         total-timeout-ms properties"
    );
    let auth_executable = required_string_property(scheme, node, "auth-executable")?;
    anyhow::ensure!(
        Path::new(&auth_executable).is_absolute(),
        "profile '{scheme}': GitHub authentication executable must be absolute"
    );
    let connect_timeout_ms = required_u64_property(scheme, node, "connect-timeout-ms")?;
    let total_timeout_ms = required_u64_property(scheme, node, "total-timeout-ms")?;
    anyhow::ensure!(
        connect_timeout_ms > 0
            && connect_timeout_ms <= total_timeout_ms
            && total_timeout_ms <= 60_000,
        "profile '{scheme}': GitHub deadlines must be positive, ordered, and at most 60000ms"
    );
    Ok(DeclaredProviderCapability::GitHubPr {
        auth_executable,
        connect_timeout_ms,
        total_timeout_ms,
    })
}

fn parse_pty_stats_capability(
    scheme: &str,
    node: &kdl::KdlNode,
) -> anyhow::Result<DeclaredProviderCapability> {
    anyhow::ensure!(
        node.entries().len() == 3 && node.entries().iter().all(|entry| entry.name().is_some()),
        "profile '{scheme}': pty-stats requires executable, cwd, and deadline-ms properties"
    );
    let executable = required_string_property(scheme, node, "executable")?;
    let cwd = required_string_property(scheme, node, "cwd")?;
    let deadline_ms = required_u64_property(scheme, node, "deadline-ms")?;
    anyhow::ensure!(
        deadline_ms > 0 && deadline_ms <= 60_000,
        "profile '{scheme}': PTY deadline must be between 1ms and 60000ms"
    );
    Ok(DeclaredProviderCapability::PtyStats {
        executable,
        cwd,
        deadline_ms,
    })
}

fn parse_vista_capability(
    scheme: &str,
    node: &kdl::KdlNode,
) -> anyhow::Result<DeclaredProviderCapability> {
    anyhow::ensure!(
        node.entries().len() == 3 && node.entries().iter().all(|entry| entry.name().is_some()),
        "profile '{scheme}': vista requires executable, cwd, and deadline-ms properties"
    );
    let executable = required_string_property(scheme, node, "executable")?;
    let cwd = required_string_property(scheme, node, "cwd")?;
    let deadline_ms = required_u64_property(scheme, node, "deadline-ms")?;
    anyhow::ensure!(
        deadline_ms > 0 && deadline_ms <= 60_000,
        "profile '{scheme}': Vista deadline must be between 1ms and 60000ms"
    );
    Ok(DeclaredProviderCapability::Vista {
        executable,
        cwd,
        deadline_ms,
    })
}

fn required_string_property(
    scheme: &str,
    node: &kdl::KdlNode,
    name: &str,
) -> anyhow::Result<String> {
    node.get(name)
        .and_then(|value| value.as_string())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile '{scheme}': '{}' property '{name}' must be a non-empty string",
                node.name().value()
            )
        })
}

fn required_u64_property(scheme: &str, node: &kdl::KdlNode, name: &str) -> anyhow::Result<u64> {
    node.get(name)
        .and_then(|value| value.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile '{scheme}': '{}' property '{name}' must be a non-negative integer",
                node.name().value()
            )
        })
}

/// Read `<catalog>/catalog.kdl`. A missing file is the default declaration, not an error.
pub fn load(catalog_root: &Path) -> anyhow::Result<CatalogConfig> {
    match std::fs::read_to_string(config_path(catalog_root)) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CatalogConfig::default()),
        Err(e) => Err(e.into()),
    }
}

/// Resolve one declared module using the same expansion as runtime registry construction while
/// preserving whether the module belongs to the catalog transaction.
pub(crate) fn resolve_profile_module(
    catalog_root: &Path,
    declared: &str,
) -> anyhow::Result<ResolvedProfileModule> {
    let catalog_root = if catalog_root.is_absolute() {
        lexical_absolute(catalog_root)?
    } else {
        lexical_absolute(&std::env::current_dir()?.join(catalog_root))?
    };
    let expanded = PathBuf::from(crate::expand::expand_catalog(declared, &catalog_root));
    if Path::new(declared).is_absolute() {
        return Ok(ResolvedProfileModule::External(expanded));
    }

    let resolved = lexical_absolute(&catalog_root.join(expanded))?;
    let relative = resolved.strip_prefix(&catalog_root).with_context(|| {
        format!("catalog-relative profile module escapes the catalog root: {declared}")
    })?;
    anyhow::ensure!(
        !relative.as_os_str().is_empty(),
        "catalog-relative profile module names the catalog root: {declared}"
    );
    validate_catalog_relative_profile_module_path(relative)
        .with_context(|| format!("profile module path is reserved: {declared}"))?;
    Ok(ResolvedProfileModule::CatalogRelative(
        relative.to_path_buf(),
    ))
}

/// Resolve a provider component as a catalog-owned transactional artifact.
///
/// Unlike legacy resolver modules, provider components may not be external: the component bytes
/// and `catalog.kdl` capability declaration must cross apply/generation boundaries together.
pub(crate) fn resolve_provider_component(
    catalog_root: &Path,
    declared: &str,
) -> anyhow::Result<PathBuf> {
    match resolve_profile_module(catalog_root, declared)? {
        ResolvedProfileModule::CatalogRelative(relative) => Ok(relative),
        ResolvedProfileModule::External(_) => {
            anyhow::bail!("provider component must be catalog-relative: {declared}")
        }
    }
}

pub(crate) fn validate_catalog_relative_profile_module_path(relative: &Path) -> anyhow::Result<()> {
    let components = relative
        .components()
        .map(|component| {
            let Component::Normal(name) = component else {
                anyhow::bail!("profile module path contains an unsafe component")
            };
            name.to_str()
                .context("catalog-relative profile module path is not UTF-8")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let first = components
        .first()
        .copied()
        .context("catalog-relative profile module path is empty")?;
    let reserved_control = components
        .iter()
        .any(|name| matches!(*name, ".git" | ".st2"));
    let reserved_root = matches!(
        first,
        "pty"
            | "workspace"
            | "workspaces"
            | ".workspace"
            | "resources"
            | "archive"
            | "inbox"
            | "status"
    );
    let reserved_agent_state = first == "agents"
        && components.get(3).is_some_and(|name| {
            matches!(
                *name,
                ".workspace" | "resources" | "archive" | "inbox" | "status"
            ) || name.starts_with(".status.tmp-")
        });
    let reserved_template_subtree = first == "_templates"
        && components.iter().skip(1).any(|name| {
            matches!(
                *name,
                ".git"
                    | ".st2"
                    | "pty"
                    | "workspace"
                    | "workspaces"
                    | ".workspace"
                    | "resources"
                    | "archive"
                    | "inbox"
                    | "status"
            ) || name.starts_with(".status.tmp-")
        });
    anyhow::ensure!(
        !(reserved_control || reserved_root || reserved_agent_state || reserved_template_subtree),
        "catalog-relative profile module targets a reserved control/state path: {}",
        relative.display()
    );
    Ok(())
}

fn lexical_absolute(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "catalog root is not absolute: {}",
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
            Component::Prefix(_) => {
                anyhow::bail!("unsupported profile module path prefix: {}", path.display())
            }
        }
    }
    Ok(normalized)
}

/// The session registry the CATALOG itself declares: `pty-root` if it declares one, else the native
/// `<catalog>/pty`. This is what `st2 env`/`st2 pty`/`st2 shell` hand to bus-aware tools, so those
/// describe the catalog rather than whatever registry the caller happens to be standing in.
///
/// A malformed declaration falls back to the default instead of failing: this runs on every spawn,
/// list, and kill, including teardown, and inventing a root there is worse than using the native
/// one. `st2 validate` is where a bad declaration is reported.
pub fn pty_root(catalog_root: &Path) -> PathBuf {
    match load(catalog_root).ok().and_then(|c| c.pty_root) {
        // Join, so a relative declaration anchors at the catalog instead of the caller's cwd.
        Some(declared) => catalog_root.join(crate::expand::expand_catalog(&declared, catalog_root)),
        None => catalog_root.join("pty"),
    }
}

/// Load the catalog profile declaration and construct its registry from one coherent caller-held
/// catalog read fence. Descriptor/runtime compatibility is checked here so `st2 up` cannot silently
/// start a profile with half of the observable contract.
pub fn declared_profile_catalog(
    catalog_root: &Path,
) -> anyhow::Result<(CatalogConfig, ResourceProfileRegistry)> {
    let config = load(catalog_root)?;
    let absolute_root = if catalog_root.is_absolute() {
        lexical_absolute(catalog_root)?
    } else {
        lexical_absolute(&std::env::current_dir()?.join(catalog_root))?
    };
    let registry = config.profiles.iter().try_fold(
        ResourceProfileRegistry::empty(),
        |registry, declared| -> anyhow::Result<ResourceProfileRegistry> {
            let profile = match resolve_profile_module(&absolute_root, &declared.wasm)? {
                ResolvedProfileModule::CatalogRelative(relative) => {
                    ResourceProfile::wasm_contained(
                        declared.scheme.clone(),
                        &absolute_root,
                        relative,
                        declared.class,
                    )
                    .with_notify_chain(declared.notify_chain)
                }
                ResolvedProfileModule::External(module) => {
                    ResourceProfile::wasm(declared.scheme.clone(), module, declared.class)
                        .with_notify_chain(declared.notify_chain)
                }
            };
            Ok(registry.with_profile(profile))
        },
    )?;
    validate_runtime_contracts(&config, &registry)?;
    Ok((config, registry))
}

/// The resource profiles declared by this catalog.
pub fn declared_profiles(catalog_root: &Path) -> anyhow::Result<ResourceProfileRegistry> {
    declared_profile_catalog(catalog_root).map(|(_, registry)| registry)
}

/// Build the registry passed to passive resync. Observable carriers are supervisor-authored
/// snapshots and must never also be watched as ordinary filesystem carriers.
pub fn passive_profiles(
    config: &CatalogConfig,
    registry: &ResourceProfileRegistry,
) -> anyhow::Result<ResourceProfileRegistry> {
    #[cfg(not(feature = "wasm-resolver"))]
    {
        let _ = config;
        return Ok(registry.clone());
    }
    #[cfg(feature = "wasm-resolver")]
    let refresh = registry.begin_refresh();
    #[cfg(feature = "wasm-resolver")]
    {
        config
            .profiles
            .iter()
            .try_fold(ResourceProfileRegistry::empty(), |passive, declared| {
                let observable = refresh
                    .try_descriptor(&declared.scheme)
                    .ok()
                    .flatten()
                    .is_some_and(|descriptor| {
                        descriptor
                            .capabilities
                            .contains(&ProfileCapability::Observe)
                    });
                if observable {
                    Ok(passive)
                } else {
                    Ok(passive.with_profile(
                        registry
                            .get(&declared.scheme)
                            .expect("registry was built from this declaration")
                            .clone(),
                    ))
                }
            })
    }
}

fn validate_runtime_contracts(
    config: &CatalogConfig,
    registry: &ResourceProfileRegistry,
) -> anyhow::Result<()> {
    #[cfg(not(feature = "wasm-resolver"))]
    {
        if let Some(profile) = config
            .profiles
            .iter()
            .find(|profile| profile.runtime.is_some())
        {
            anyhow::bail!(
                "profile '{}': observable runtime unavailable because st2 was built without the `wasm-resolver` feature",
                profile.scheme
            );
        }
        let _ = registry;
        return Ok(());
    }
    #[cfg(feature = "wasm-resolver")]
    {
        #[cfg(not(feature = "wasip2-provider-runtime"))]
        if let Some(profile) = config
            .profiles
            .iter()
            .find(|profile| profile.runtime.is_some())
        {
            anyhow::bail!(
                "profile '{}': component provider runtime unavailable because st2 was built \
                 without the `wasip2-provider-runtime` feature",
                profile.scheme
            );
        }
        let refresh = registry.begin_refresh();
        for profile in &config.profiles {
            let descriptor = match refresh.try_descriptor(&profile.scheme) {
                Ok(descriptor) => descriptor,
                Err(error) if profile.runtime.is_none() => {
                    // Passive resolver failures remain binding-local. Requiring every legacy
                    // module to instantiate during catalog admission would turn one unwatchable
                    // Resource into a catalog-wide supervisor outage.
                    let _ = error;
                    continue;
                }
                Err(error) => {
                    return Err(anyhow::Error::msg(error))
                        .with_context(|| format!("profile '{}': describe", profile.scheme));
                }
            };
            let observes = descriptor.as_ref().is_some_and(|descriptor| {
                descriptor
                    .capabilities
                    .contains(&ProfileCapability::Observe)
            });
            match (observes, profile.runtime.is_some()) {
                (true, false) => anyhow::bail!(
                    "profile '{}': descriptor declares observe but catalog runtime is missing",
                    profile.scheme
                ),
                (false, true) => anyhow::bail!(
                    "profile '{}': catalog runtime is forbidden unless descriptor declares observe",
                    profile.scheme
                ),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undeclared_catalog_keeps_the_native_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()).unwrap(), CatalogConfig::default());
        assert_eq!(pty_root(tmp.path()), tmp.path().join("pty"));

        // A file that declares other things, but no pty root.
        std::fs::write(
            config_path(tmp.path()),
            "agent \"a\" { command \"true\" }\n",
        )
        .unwrap();
        assert_eq!(pty_root(tmp.path()), tmp.path().join("pty"));
    }

    #[test]
    fn a_declared_root_is_expanded_and_anchored_at_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(tmp.path()),
            "catalog {\n  pty-root \"/run/agents/pty\"\n}\n",
        )
        .unwrap();
        assert_eq!(pty_root(tmp.path()), PathBuf::from("/run/agents/pty"));

        std::fs::write(
            config_path(tmp.path()),
            "catalog { pty-root \"$CATALOG/../shared\" }\n",
        )
        .unwrap();
        assert_eq!(pty_root(tmp.path()), tmp.path().join("../shared"));

        // A relative value belongs to the catalog, never to the caller's cwd.
        std::fs::write(
            config_path(tmp.path()),
            "catalog { pty-root \"registry\" }\n",
        )
        .unwrap();
        assert_eq!(pty_root(tmp.path()), tmp.path().join("registry"));
    }

    #[test]
    fn a_mistyped_declaration_is_an_error_not_a_silent_default() {
        assert!(parse("catalog { pty_root \"/run/agents/pty\" }").is_err());
        assert!(parse("catalog { pty-root }").is_err());
        assert!(parse("catalog { pty-root \"\" }").is_err());
        assert!(parse("catalog { pty-root \"/a\" }\ncatalog { pty-root \"/b\" }").is_err());
        assert!(parse("this is (not kdl").is_err());

        // Reported by `st2 validate`; the runtime path stays on the native root.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(tmp.path()),
            "catalog { pty_root \"/run/agents/pty\" }\n",
        )
        .unwrap();
        assert_eq!(pty_root(tmp.path()), tmp.path().join("pty"));
    }

    #[test]
    fn top_level_profile_typos_fail_without_rejecting_colocated_agents() {
        let error = parse(r#"profiel "dev.x" { wasm "x.wasm" }"#).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown catalog.kdl top-level node 'profiel'"),
            "{error:#}"
        );

        let config = parse(
            r#"
            agent "live" { command "true" }
            catalog { pty-root "registry" }
            profile "dev.x" { wasm "x.wasm" }
            "#,
        )
        .unwrap();
        assert_eq!(config.pty_root.as_deref(), Some("registry"));
        assert_eq!(config.profiles.len(), 1);
    }

    #[test]
    fn profile_blocks_parse_with_default_and_declared_classes() {
        let config = parse(
            r#"
            profile "dev.example.goal" { wasm "resolvers/goal.wasm" }
            profile "dev.example.tree" {
              wasm "/abs/resolvers/tree.wasm"
              class "silent"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.profiles,
            vec![
                DeclaredProfile {
                    scheme: "dev.example.goal".into(),
                    wasm: "resolvers/goal.wasm".into(),
                    class: ProfileClass::Coalesced,
                    notify_chain: false,
                    runtime: None,
                },
                DeclaredProfile {
                    scheme: "dev.example.tree".into(),
                    wasm: "/abs/resolvers/tree.wasm".into(),
                    class: ProfileClass::Silent,
                    notify_chain: false,
                    runtime: None,
                },
            ]
        );
    }

    #[test]
    fn runtime_grammar_has_one_immutable_component_and_one_typed_capability() {
        let config = parse(
            r#"
            profile "dev.example.observe" {
              wasm "observe.wasm"
              runtime {
                component "components/github-issue.wasm"
                demand #true
                github-issue auth-executable="/nix/store/example/bin/gh" connect-timeout-ms=3000 total-timeout-ms=10000
              }
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.profiles[0].runtime,
            Some(DeclaredProfileRuntime {
                component: "components/github-issue.wasm".into(),
                capability: DeclaredProviderCapability::GitHubIssue {
                    auth_executable: "/nix/store/example/bin/gh".into(),
                    connect_timeout_ms: 3000,
                    total_timeout_ms: 10000,
                },
                demand: true,
            })
        );
        for malformed in [
            r#"profile "dev.x" { wasm "x.wasm"; runtime }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime "shell" { component "x.wasm" } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component "x.wasm" } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component ""; pty-stats executable="pty" cwd="/" deadline-ms=1000 } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component "x"; component "y"; pty-stats executable="pty" cwd="/" deadline-ms=1000 } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { argv "x" } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component "x"; pty-stats executable="pty" cwd="/" deadline-ms=1000; github-issue auth-executable="/bin/gh" connect-timeout-ms=1 total-timeout-ms=2 } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component "x"; demand #true; demand #true; pty-stats executable="pty" cwd="/" deadline-ms=1000 } }"#,
            r#"profile "dev.x" { wasm "x.wasm"; runtime { component "x"; pty-stats executable="pty" cwd="/" deadline-ms=1000 extra="no" } }"#,
        ] {
            assert!(parse(malformed).is_err(), "expected error for: {malformed}");
        }
    }

    #[test]
    fn github_pr_runtime_capability_authorizes_dynamic_resources_with_bounded_transport() {
        let config = parse(
            r#"
            profile "github-pr" {
              wasm "github-pr-resolver.wasm"
              runtime {
                component "components/github-pr.component.wasm"
                demand #true
                github-pr auth-executable="/nix/store/example/bin/gh" connect-timeout-ms=3000 total-timeout-ms=10000
              }
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.profiles[0].runtime,
            Some(DeclaredProfileRuntime {
                component: "components/github-pr.component.wasm".into(),
                capability: DeclaredProviderCapability::GitHubPr {
                    auth_executable: "/nix/store/example/bin/gh".into(),
                    connect_timeout_ms: 3000,
                    total_timeout_ms: 10000,
                },
                demand: true,
            })
        );
        for malformed in [
            r#"profile "github-pr" { wasm "x"; runtime { component "x"; github-pr auth-executable="/bin/gh" owner="o" connect-timeout-ms=1 total-timeout-ms=2 } }"#,
            r#"profile "github-pr" { wasm "x"; runtime { component "x"; github-pr auth-executable="gh" connect-timeout-ms=1 total-timeout-ms=2 } }"#,
            r#"profile "github-pr" { wasm "x"; runtime { component "x"; github-pr auth-executable="/bin/gh" connect-timeout-ms=3 total-timeout-ms=2 } }"#,
            r#"profile "github-pr" { wasm "x"; runtime { component "x"; github-pr auth-executable="/bin/gh" connect-timeout-ms=1 total-timeout-ms=60001 } }"#,
        ] {
            assert!(parse(malformed).is_err(), "expected error for: {malformed}");
        }
    }

    #[test]
    fn vista_runtime_capability_authorizes_dynamic_artifacts_with_bounded_execution() {
        let config = parse(
            r#"
            profile "vista" {
              wasm "vista-resolver.wasm"
              runtime {
                component "components/vista.component.wasm"
                demand #true
                vista executable="/nix/store/example/bin/vista" cwd="/var/empty" deadline-ms=10000
              }
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.profiles[0].runtime,
            Some(DeclaredProfileRuntime {
                component: "components/vista.component.wasm".into(),
                capability: DeclaredProviderCapability::Vista {
                    executable: "/nix/store/example/bin/vista".into(),
                    cwd: "/var/empty".into(),
                    deadline_ms: 10000,
                },
                demand: true,
            })
        );
        for malformed in [
            r#"profile "vista" { wasm "x"; runtime { component "x"; vista executable="vista" cwd="/" deadline-ms=0 } }"#,
            r#"profile "vista" { wasm "x"; runtime { component "x"; vista executable="vista" cwd="/" deadline-ms=60001 } }"#,
            r#"profile "vista" { wasm "x"; runtime { component "x"; vista executable="vista" cwd="/" deadline-ms=1 extra="no" } }"#,
        ] {
            assert!(parse(malformed).is_err(), "expected error for: {malformed}");
        }
    }

    #[test]
    fn malformed_profile_blocks_fail_validation_loudly() {
        let loud = [
            // missing scheme / module
            r#"profile { wasm "x.wasm" }"#,
            r#"profile "dev.x" { class "immediate" }"#,
            r#"profile "dev.x""#,
            r#"profile "dev.x" "extra" { wasm "x.wasm" }"#,
            r#"profile "dev.x" class="silent" { wasm "x.wasm" }"#,
            // invalid class value and duplicate fields
            r#"profile "dev.x" { wasm "x.wasm" class "goal" }"#,
            r#"profile "dev.x" { wasm "a.wasm" wasm "b.wasm" }"#,
            r#"profile "dev.x" { wasm "x.wasm" class "immediate" class "silent" }"#,
            // unknown field, duplicate scheme, scheme that is not a scheme
            r#"profile "dev.x" { template "x.md" }"#,
            r#"profile "dev.a" { wasm "a.wasm" }
               profile "dev.a" { wasm "b.wasm" }"#,
            r#"profile "not a scheme" { wasm "x.wasm" }"#,
            r#"profile "1bad" { wasm "x.wasm" }"#,
        ];
        for text in loud {
            assert!(parse(text).is_err(), "expected error for: {text}");
        }
    }

    #[test]
    fn profile_value_fields_reject_nested_child_blocks() {
        for (field, text) in [
            (
                "wasm",
                r#"profile "dev.x" {
                  wasm "x.wasm" {
                    nested "value"
                  }
                }"#,
            ),
            (
                "class",
                r#"profile "dev.x" {
                  wasm "x.wasm"
                  class "immediate" {
                    nested "value"
                  }
                }"#,
            ),
        ] {
            let error = parse(text).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("'{field}' does not accept a child block")),
                "{field}: {error:#}"
            );
        }
    }

    #[test]
    fn declared_profiles_anchor_relative_modules_at_the_catalog_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(tmp.path()),
            "profile \"dev.example.goal\" {\n  wasm \"$CATALOG/resolvers/goal.wasm\"\n}\n",
        )
        .unwrap();
        let registry = declared_profiles(tmp.path()).unwrap();
        let profile = registry.get("dev.example.goal").unwrap();
        assert_eq!(
            profile.module(),
            Some(tmp.path().join("resolvers/goal.wasm").as_path())
        );

        // A malformed block propagates instead of falling back to an empty registry.
        std::fs::write(
            config_path(tmp.path()),
            "profile \"dev.example.goal\" { class \"bogus\" }\n",
        )
        .unwrap();
        assert!(declared_profiles(tmp.path()).is_err());
    }
}
