//! What a declaration makes this host watch, and how completely it is covered.
//!
//! Moved verbatim out of the parent module: carrier classification, the coalescing windows, the
//! per-agent watch set, and resource coverage.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

pub(super) use agent_spec::profile::ProfileClass;
use agent_spec::profile::{ResourceProfileRefresh, ResourceProfileRegistry};
use agent_spec::spec::{AgentSpec, Resource, decode_percent_path};

use crate::resource_profile::{MAX_FACTS, MAX_FACT_KEY_BYTES};


/// Provisional coalescing windows (`RESYNC-T02`): tuned by observed notification volume.
pub(super) const IMMEDIATE_WINDOW: Duration = Duration::from_millis(500);
pub(super) const COALESCED_WINDOW: Duration = Duration::from_secs(5);

/// How a carrier notifies (`RESYNC-R04`). Silent carriers never reach the watch set:
/// [`classify`] excludes them, so nothing about them is observed or emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CarrierClass {
    Immediate,
    Coalesced,
}

impl CarrierClass {
    pub(super) fn window(self) -> Duration {
        match self {
            CarrierClass::Immediate => IMMEDIATE_WINDOW,
            CarrierClass::Coalesced => COALESCED_WINDOW,
        }
    }
}
/// Catalog-observable resync coverage for one declared Resource binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncCoverage {
    Immediate,
    Coalesced,
    Silent,
    Unsupported,
    Inactive,
}

impl ResyncCoverage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Coalesced => "coalesced",
            Self::Silent => "silent",
            Self::Unsupported => "unsupported",
            Self::Inactive => "inactive",
        }
    }

    pub(super) fn carrier_class(self) -> Option<CarrierClass> {
        match self {
            Self::Immediate => Some(CarrierClass::Immediate),
            Self::Coalesced => Some(CarrierClass::Coalesced),
            Self::Silent | Self::Unsupported | Self::Inactive => None,
        }
    }
}


/// One watchable local carrier: binding label, absolute path, notification class, and an optional
/// host root that must confine every read of a resolver-selected path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchableCarrier {
    pub label: String,
    pub path: PathBuf,
    pub class: CarrierClass,
    pub containment_root: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeclarationSummary {
    pub(super) bindings: BTreeMap<String, String>,
    pub(super) complete: bool,
}

pub(super) fn declaration_summary(spec: &AgentSpec) -> DeclarationSummary {
    let mut bindings = spec
        .resources
        .iter()
        .map(|resource| {
            (
                resource.name().to_owned(),
                declaration_resource_digest(resource),
            )
        })
        .collect::<Vec<_>>();
    bindings.sort_by(|left, right| left.0.cmp(&right.0));
    let complete = bindings.len() <= MAX_FACTS
        && bindings
            .iter()
            .all(|(label, _)| label.len() <= MAX_FACT_KEY_BYTES);
    DeclarationSummary {
        bindings: bindings.into_iter().take(MAX_FACTS).collect(),
        complete,
    }
}

fn declaration_resource_digest(resource: &Resource) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st2.resync.resource-declaration.v1\0");
    update_digest_field(&mut digest, resource.uri().as_bytes());
    update_digest_field(&mut digest, resource.reason().as_bytes());
    match resource.inactive_reason() {
        Some(reason) => {
            digest.update([1]);
            update_digest_field(&mut digest, reason.as_bytes());
        }
        None => digest.update([0]),
    }
    let selector = serde_json::to_vec(&resource.selector())
        .expect("a parsed JSON selector always serializes");
    update_digest_field(&mut digest, &selector);
    format!("{:x}", digest.finalize())
}

fn update_digest_field(digest: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("declaration fields fit in u64");
    digest.update(length.to_be_bytes());
    digest.update(value);
}

/// The watchable carriers of one agent, keyed by its declaration path with current routing IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWatchSet {
    pub declaration_path: PathBuf,
    pub bus_id: String,
    pub seat_id: Option<String>,
    pub carriers: Vec<WatchableCarrier>,
    pub(super) declaration_summary: Option<DeclarationSummary>,
}

/// Resolve one spec's watch set: the declaration file plus every active resource binding whose
/// URI denotes a local file (`RESYNC-R01`) — directly (`file://`, catalog-relative) or through a
/// declared resource profile for a scheme URI. Bindings with an inactive reason are skipped;
/// schemes without a local denotation and silent carriers are simply absent. A failing profile
/// resolver is contained: its binding is skipped, the rest of the set survives.
pub fn watch_set_for(
    spec: &AgentSpec,
    this_host: &str,
    profiles: &ResourceProfileRegistry,
) -> AgentWatchSet {
    watch_set_for_in_catalog(spec, std::slice::from_ref(spec), this_host, profiles)
}

/// [`watch_set_for`] with the catalog view a `notify-chain` profile needs to reach the carriers
/// this agent's `supervisor` ancestors declare. Without the other specs, chain carriers cannot be
/// resolved and only the agent's own carriers are produced.
pub fn watch_set_for_in_catalog(
    spec: &AgentSpec,
    specs: &[AgentSpec],
    this_host: &str,
    profiles: &ResourceProfileRegistry,
) -> AgentWatchSet {
    let refresh = profiles.begin_refresh();
    resolve_watch_set(spec, specs, this_host, &refresh).0
}

pub(super) fn resolve_watch_set(
    spec: &AgentSpec,
    specs: &[AgentSpec],
    this_host: &str,
    profiles: &ResourceProfileRefresh<'_>,
) -> (AgentWatchSet, Vec<String>) {
    let declaration_path = lexical_clean(&spec.path);
    let agent_dir = declaration_path.parent().unwrap_or(Path::new("."));
    let mut carriers = vec![WatchableCarrier {
        label: "declaration".to_owned(),
        path: declaration_path.clone(),
        class: CarrierClass::Immediate,
        containment_root: None,
    }];
    let mut diagnostics = Vec::new();
    for resource in &spec.resources {
        if resource.inactive_reason().is_some() {
            continue;
        }
        // Silent profiles carry no observable transition, so never compile or execute their
        // untrusted resolver merely to discard its result.
        let registered_profile = resource
            .uri()
            .split_once(':')
            .and_then(|(scheme, _)| profiles.get(scheme));
        if registered_profile.is_some_and(|profile| profile.class() == ProfileClass::Silent) {
            continue;
        }
        // Declared profile schemes resolve through their wasm module; the declared class governs
        // notification instead of the local-path defaults.
        match profiles.try_resolve(agent_dir, resource.uri()) {
            Ok(Some(resolution)) => {
                let Some(class) = carrier_class(resolution.class) else {
                    continue;
                };
                carriers.push(WatchableCarrier {
                    label: resource.name().to_owned(),
                    path: resolution.path,
                    class,
                    containment_root: Some(resolution.containment_root),
                });
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                diagnostics.push(format!(
                    "resync profile for {} resource '{}': {error}; binding is unwatchable",
                    spec.bus_id(this_host),
                    resource.name()
                ));
                continue;
            }
        }
        let Some(class) = resource_coverage(agent_dir, resource).carrier_class() else {
            continue;
        };
        let path = resolve_local_path(agent_dir, resource.uri())
            .expect("watchable coverage must have a local path");
        carriers.push(WatchableCarrier {
            label: resource.name().to_owned(),
            path,
            class,
            containment_root: None,
        });
    }
    append_chain_carriers(
        spec,
        specs,
        this_host,
        profiles,
        &mut carriers,
        &mut diagnostics,
    );
    // The supervisor's resolved logical host — not the OS hostname — decides the bus id, so an
    // agent supervised under `st2 up --host <alias>` without an explicit declaration host still
    // produces a recipient `resolve_stream` can resolve.
    (
        AgentWatchSet {
            declaration_path,
            bus_id: spec.bus_id(this_host),
            seat_id: spec.tasks.iter().find(|task| task.name == "agent").map(|task| {
                task.id
                    .clone()
                    .unwrap_or_else(|| format!("{}.{}", spec.bus_id(this_host), task.name))
            }),
            carriers,
            declaration_summary: Some(declaration_summary(spec)),
        },
        diagnostics,
    )
}

/// A URI denotes a local file when it is an absolute, authority-free `file://` URI or a
/// scheme-less catalog-relative path resolved against the agent directory. URI scheme syntax is
/// parsed before the `file` scheme name is matched ASCII-case-insensitively, as required by RFC
/// 3986. Unsupported file URI authorities, query/fragment components,
/// malformed escapes, encoded path separators, and encoded parent components have no local
/// denotation.
/// Carriers this agent's `supervisor` ancestors declare through a `notify-chain` profile.
///
/// A profile whose layers compose along the supervisor edge leaves every descendant's effective
/// view dependent on carriers the descendant does not own. Resync notifies a carrier's owner, so
/// without this the descendant is never told its view changed.
///
/// The walk deliberately reuses each ancestor's OWN declared URI rather than synthesizing one:
/// st2 does not own any profile's URI grammar, and a resolver is free to ignore the authority
/// component entirely, so a synthesized subject would be a guess. Resolving the ancestor's
/// declaration against the ancestor's directory is the identical call the ancestor's own
/// subscription makes, which is what keeps containment unchanged — the guest is still only ever
/// asked to resolve one agent's URI against that agent's own directory.
///
/// Matching is by profile scheme, never by binding label: labels are agent-local and replaceable,
/// so keying on them would silently drop a layer whose owner renamed its binding.
fn append_chain_carriers(
    spec: &AgentSpec,
    specs: &[AgentSpec],
    this_host: &str,
    profiles: &ResourceProfileRefresh<'_>,
    carriers: &mut Vec<WatchableCarrier>,
    diagnostics: &mut Vec<String>,
) {
    let chain_schemes: Vec<&str> = spec
        .resources
        .iter()
        .filter(|resource| resource.inactive_reason().is_none())
        .filter_map(|resource| resource.uri().split_once(':').map(|(scheme, _)| scheme))
        .filter(|scheme| profiles.get(scheme).is_some_and(|p| p.notify_chain()))
        .collect();
    if chain_schemes.is_empty() {
        return;
    }

    let ancestors = match crate::supervisor_chain::ancestors(specs, spec, this_host) {
        Ok(ancestors) => ancestors,
        Err(error) => {
            diagnostics.push(format!(
                "resync notify-chain for {}: supervisor chain is unwalkable ({error:?}); \
                 ancestor carriers are unwatchable",
                spec.bus_id(this_host)
            ));
            return;
        }
    };

    for ancestor in ancestors {
        // Skip and continue, never sever: a retired ancestor contributes no layer, but its own
        // ancestors still do. `is_retired` normalizes both declaration spellings.
        if ancestor.desired_state.is_retired() {
            continue;
        }
        let ancestor_declaration = lexical_clean(&ancestor.path);
        let ancestor_dir = ancestor_declaration.parent().unwrap_or(Path::new("."));
        let ancestor_bus_id = ancestor.bus_id(this_host);
        for resource in &ancestor.resources {
            if resource.inactive_reason().is_some() {
                continue;
            }
            let Some((scheme, _)) = resource.uri().split_once(':') else {
                continue;
            };
            if !chain_schemes.contains(&scheme) {
                continue;
            }
            match profiles.try_resolve(ancestor_dir, resource.uri()) {
                Ok(Some(resolution)) => {
                    let Some(class) = carrier_class(resolution.class) else {
                        continue;
                    };
                    carriers.push(WatchableCarrier {
                        // Qualifying by owner keeps each ancestor's layer on its own supersession
                        // key, so a burst on one ancestor cannot collapse another's event.
                        label: format!("{}@{ancestor_bus_id}", resource.name()),
                        path: resolution.path,
                        class,
                        containment_root: Some(resolution.containment_root),
                    });
                }
                Ok(None) => {}
                Err(error) => diagnostics.push(format!(
                    "resync notify-chain for {}: ancestor {ancestor_bus_id} resource '{}': \
                     {error}; that ancestor layer is unwatchable",
                    spec.bus_id(this_host),
                    resource.name()
                )),
            }
        }
    }
}

pub(super) fn resolve_local_path(agent_dir: &Path, uri: &str) -> Option<PathBuf> {
    if let Some((scheme, scheme_specific)) = uri.split_once(':').filter(|(scheme, _)| {
        !scheme.is_empty()
            && !scheme.contains('/')
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    }) {
        if !scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let encoded_path = scheme_specific.strip_prefix("//")?;
        if !encoded_path.starts_with('/')
            || encoded_path.starts_with("//")
            || encoded_path.contains(['?', '#'])
        {
            return None;
        }
        let path = PathBuf::from(decode_percent_path(encoded_path).ok()?);
        return path.is_absolute().then(|| lexical_clean(&path));
    }
    let path = PathBuf::from(decode_percent_path(uri).ok()?);
    Some(lexical_clean(&agent_dir.join(path)))
}
/// Resolve the externally visible resync coverage for one Resource binding.
pub fn resource_coverage(agent_dir: &Path, resource: &agent_spec::spec::Resource) -> ResyncCoverage {
    if resource.inactive_reason().is_some() {
        return ResyncCoverage::Inactive;
    }
    let Some(path) = resolve_local_path(agent_dir, resource.uri()) else {
        return ResyncCoverage::Unsupported;
    };
    match classify(agent_dir, resource.name(), &path) {
        Some(CarrierClass::Immediate) => ResyncCoverage::Immediate,
        Some(CarrierClass::Coalesced) => ResyncCoverage::Coalesced,
        None => ResyncCoverage::Silent,
    }
}


/// Remove `.` and `..` components lexically. This deliberately does not inspect the filesystem:
/// classification follows the authored path structure without resolving symlinks.
pub(super) fn lexical_clean(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                clean.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(clean.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    clean.pop();
                } else if !clean.has_root() {
                    clean.push(component.as_os_str());
                }
            }
        }
    }
    clean
}

/// Class defaults for carriers resolved WITHOUT a declared profile (`RESYNC-R04`): goal carriers
/// are immediate; stores the agent itself authors are silent (None); everything else is coalesced.
/// The declaration carrier is immediate by construction in [`watch_set_for`]. Profile-resolved
/// carriers skip this sniffing entirely — their class is what the catalog declares.
pub(super) fn classify(agent_dir: &Path, binding_name: &str, normalized_path: &Path) -> Option<CarrierClass> {
    let agent_relative = normalized_path.strip_prefix(agent_dir).ok();
    let authored_store = agent_relative.is_some_and(|rel| {
        rel.starts_with("resources/context")
            || rel.starts_with("resources/decisions")
            || rel.starts_with("resources/friction")
    });
    if authored_store {
        return None;
    }
    let goal =
        binding_name == "goal" || normalized_path.file_name().is_some_and(|n| n == "goal.md");
    Some(if goal {
        CarrierClass::Immediate
    } else {
        CarrierClass::Coalesced
    })
}

/// A declared profile class maps onto carrier notification: silent profiles are excluded from
/// the watch set exactly like sniffed agent-authored stores.
pub(super) fn carrier_class(class: ProfileClass) -> Option<CarrierClass> {
    match class {
        ProfileClass::Immediate => Some(CarrierClass::Immediate),
        ProfileClass::Coalesced => Some(CarrierClass::Coalesced),
        ProfileClass::Silent => None,
    }
}

/// Resolve coverage with the catalog's declared profile registry. Silent profiles report their
/// declared class without executing a guest; other registered schemes are watchable only when
/// their resolver succeeds.
pub fn resource_coverage_with_profiles(
    agent_dir: &Path,
    resource: &agent_spec::spec::Resource,
    profiles: &ResourceProfileRefresh<'_>,
) -> ResyncCoverage {
    if resource.inactive_reason().is_some() {
        return ResyncCoverage::Inactive;
    }
    let registered = resource
        .uri()
        .split_once(':')
        .and_then(|(scheme, _)| profiles.get(scheme));
    let Some(profile) = registered else {
        return resource_coverage(agent_dir, resource);
    };
    if profile.class() == ProfileClass::Silent {
        return ResyncCoverage::Silent;
    }
    match profiles.try_resolve(agent_dir, resource.uri()) {
        Ok(Some(resolution)) => match resolution.class {
            ProfileClass::Immediate => ResyncCoverage::Immediate,
            ProfileClass::Coalesced => ResyncCoverage::Coalesced,
            ProfileClass::Silent => ResyncCoverage::Silent,
        },
        Ok(None) | Err(_) => ResyncCoverage::Unsupported,
    }
}
