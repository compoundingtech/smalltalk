//! Agent selection: immutable IDs and mutable addresses.
//!
//! Decision 0015-immutable-agent-id-and-mutable-address splits one overloaded string into two typed namespaces. The immutable **agent ID**
//! is catalog-global and never routes for humans; the mutable **agent address** is unique per
//! logical host and is the only thing an ordinary human reference resolves against. Equal bytes in
//! the two namespaces do not collide, so an exact-ID selector performs only ID lookup and never
//! falls through to address lookup — that is what keeps an existing semantic ID from silently
//! staying alive as a route after a rename.
//!
//! Ordinary references are decided by a fail-closed candidate set rather than a precedence rule,
//! because a dotted semantic address and a host-qualified bus address are indistinguishable by
//! shape: `dotfiles.fractal.chat` is a legal bare address and a legal `<host>.<address>` split.
//! Collecting both readings and requiring exactly one surviving subject makes the question
//! decidable without guessing which dot is the separator.
//!
//! Every subject's ID is its effective ID: the explicit `id` a declaration carries, else the
//! `<host>.<identity>` bus identity, which is what an unmigrated catalog answers with. Nothing
//! here reads an activation gate — the address model is normative on any catalog.

use std::collections::{BTreeMap, BTreeSet};

/// How a caller named one agent.
///
/// The two forms are mutually exclusive by construction. Every agent-selecting command exposes
/// both, and a command that defaults from `ST_AGENT` consumes it through [`Self::Id`] — an ambient
/// actor is an exact subject, never a route to re-resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSelector {
    /// Exact subject lookup on the two immutable keys. Never falls through to address lookup.
    Id(String),
    /// An ordinary human reference, resolved by [`resolve_address`] on every plane that routes:
    /// messages, events, inboxes, status, context. Authoring commands must select exactly one
    /// declaration to edit, so they carry these bytes through to their own declaration-key
    /// resolver (`agent_author::resolve_target`: an exact bus identity, or a bare stable identity
    /// unique in the catalog) and never consult the address book.
    Address(String),
}

/// One routable subject in the address book.
///
/// Retired subjects are absent: retirement releases the address for *claiming*, so a retired
/// subject never occupies the namespace and never makes a live claimant's reference ambiguous.
/// It still answers on its own declared address when no routable subject does, which is what
/// keeps its retained state — status, context, message boxes — reachable by name (Q3: retirement
/// keeps the bytes). Suspended subjects are fully present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBookEntry {
    /// The immutable catalog-global agent ID: the explicit `id`, else the positional bus identity
    /// a later ID migration freezes into it.
    pub id: String,
    /// The positional `<host>.<identity>` declaration key. Immutable in practice — nothing in st2
    /// rewrites it — and the value every durable surface is keyed on today, so an exact selector
    /// answers to it as well as to `id`.
    pub bus_identity: String,
    /// The resolved logical host.
    pub host: String,
    /// The effective address: explicit `address`, else the positional `identity` fallback.
    pub address: String,
}

impl AddressBookEntry {
    /// The one mapping from a declaration to its routable subject.
    ///
    /// Every caller that hand-built this struct was re-deriving the same four fields from the
    /// same spec, so the derivation lives here and the address book itself uses it too.
    pub fn of(spec: &agent_spec::AgentSpec, this_host: &str) -> Self {
        Self {
            id: spec.effective_id(this_host),
            bus_identity: spec.bus_id(this_host),
            host: spec.resolved_host(this_host).to_owned(),
            address: spec.effective_address().to_owned(),
        }
    }

}

/// Why an ordinary reference did not name exactly one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No routable subject carries this address, in any admitted reading.
    Unknown { reference: String },
    /// More than one subject survives, so the reference is undecidable.
    Ambiguous {
        reference: String,
        /// One key per surviving subject, sorted, so a diagnostic can name and count them: the
        /// agent ID, or the declaration key where two declarations share one ID and the ID would
        /// name the same bytes twice.
        ids: Vec<String>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown { reference } => write!(
                formatter,
                "no routable agent has the address '{reference}'; a retired subject releases its address for claiming and answers only when nothing routable does"
            ),
            Self::Ambiguous { reference, ids } => {
                let count = ids.len();
                let subjects = if count == 1 { "subject" } else { "subjects" };
                write!(
                    formatter,
                    "the reference '{reference}' is ambiguous: it names {count} {subjects} ({}); qualify it with a host or select the subject by its exact id",
                    ids.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve one exact agent selector, on either immutable key: the catalog-global agent ID, or the
/// positional bus identity that a later ID migration freezes into it.
///
/// Never an address lookup. Equal bytes in the address namespace do not answer here, which is what
/// keeps a renamed subject's released semantic address from silently staying alive as a selector;
/// and because neither key moves when an address does, `ST_AGENT`, a task ID, or a durable
/// record's endpoint still names its own subject after a cutover.
///
/// Both keys are unique per catalog by admission (`dup-id`), but this does not assume admission
/// ran: a key that names more than one subject is ambiguous, not a first match.
pub fn resolve_id<'a>(
    entries: &'a [AddressBookEntry],
    id: &str,
) -> std::result::Result<&'a AddressBookEntry, ResolveError> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.id == id || entry.bus_identity == id);
    let selected = matches.next().ok_or_else(|| ResolveError::Unknown {
        reference: id.to_owned(),
    })?;
    let mut ids = std::iter::once(selected)
        .chain(matches)
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    if ids.len() == 1 {
        return Ok(selected);
    }
    // Not deduplicated: two declarations sharing one effective ID are two subjects, and reporting
    // one name for them would say "names 1 subject" about an undecidable reference.
    ids.sort();
    Err(ResolveError::Ambiguous {
        reference: id.to_owned(),
        ids,
    })
}

/// Resolve one ordinary human reference through the fail-closed candidate set (R24).
///
/// 1. When the caller pins a host, treat the complete input as an address in that host, and also
///    try the qualified split whose prefix equals the pinned host.
/// 2. Otherwise treat the complete input as a bare address across the selected catalog, and also
///    try every dotted split whose prefix is an admitted logical host and whose suffix is an
///    effective address in that host.
/// 3. Deduplicate by agent ID and succeed only when exactly one subject remains.
fn resolve_address<'a>(
    entries: &'a [AddressBookEntry],
    reference: &str,
    pinned_host: Option<&str>,
) -> std::result::Result<&'a AddressBookEntry, ResolveError> {
    let hosts: BTreeSet<&str> = entries.iter().map(|entry| entry.host.as_str()).collect();
    let mut candidates: BTreeMap<&str, &AddressBookEntry> = BTreeMap::new();

    let mut consider = |host: &str, address: &str| {
        for entry in entries {
            if entry.host == host && entry.address == address {
                candidates.insert(entry.id.as_str(), entry);
            }
        }
    };

    match pinned_host {
        Some(pinned) => {
            consider(pinned, reference);
            // Only the pinned host's own prefix is an admitted split: a caller that pinned a host
            // cannot reach another one by spelling it into the reference.
            if let Some(suffix) = reference
                .strip_prefix(pinned)
                .and_then(|rest| rest.strip_prefix('.'))
            {
                consider(pinned, suffix);
            }
        }
        None => {
            for host in &hosts {
                consider(host, reference);
            }
            for (index, _) in reference.match_indices('.') {
                let (prefix, suffix) = (&reference[..index], &reference[index + 1..]);
                if hosts.contains(prefix) {
                    consider(prefix, suffix);
                }
            }
        }
    }

    match candidates.len() {
        1 => Ok(candidates.into_values().next().expect("one candidate")),
        0 => Err(ResolveError::Unknown {
            reference: reference.to_owned(),
        }),
        _ => Err(ResolveError::Ambiguous {
            reference: reference.to_owned(),
            ids: candidates.into_keys().map(str::to_owned).collect(),
        }),
    }
}

/// Resolve either selector form against one coherent address book.
fn resolve<'a>(
    entries: &'a [AddressBookEntry],
    selector: &AgentSelector,
    pinned_host: Option<&str>,
) -> std::result::Result<&'a AddressBookEntry, ResolveError> {
    match selector {
        AgentSelector::Id(id) => resolve_id(entries, id),
        AgentSelector::Address(reference) => resolve_address(entries, reference, pinned_host),
    }
}

/// Resolve a reference on the local host first, then across the whole catalog.
///
/// A bare address names this host's subject even when another host declares the same address —
/// today's behavior — while a reference only a foreign host answers still resolves, so a caller
/// that owns only local subjects can refuse it by name instead of reporting an absence. Every
/// plane that routes uses this, so `st2 message send chat` and `st2 event emit chat` decide one
/// reference the same way.
pub fn resolve_local_first<'a>(
    entries: &'a [AddressBookEntry],
    selector: &AgentSelector,
    this_host: &str,
) -> std::result::Result<&'a AddressBookEntry, ResolveError> {
    match resolve(entries, selector, Some(this_host)) {
        Err(ResolveError::Unknown { .. }) => resolve(entries, selector, None),
        other => other,
    }
}

/// The routable address book of a discovered catalog.
///
/// A subject with no explicit `id` contributes its effective ID — the legacy bus identity migration
/// freezes — so resolution works identically before and after migration.
pub fn address_book(specs: &[agent_spec::AgentSpec], this_host: &str) -> Vec<AddressBookEntry> {
    specs
        .iter()
        .filter(|spec| !spec.desired_state.is_retired())
        .map(|spec| AddressBookEntry::of(spec, this_host))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, host: &str, address: &str) -> AddressBookEntry {
        AddressBookEntry {
            id: id.to_owned(),
            bus_identity: format!("{host}.{address}"),
            host: host.to_owned(),
            address: address.to_owned(),
        }
    }

    #[test]
    fn a_bare_address_resolves_within_the_selected_catalog() {
        let entries = vec![entry("id-1", "dev3", "chat"), entry("id-2", "dev4", "notes")];
        assert_eq!(resolve_address(&entries, "chat", None).unwrap().id, "id-1");
        assert_eq!(resolve_address(&entries, "notes", None).unwrap().id, "id-2");
    }

    /// A host-qualified bus address and a dotted semantic address are the same shape, so both
    /// readings are collected and the answer is the surviving subject.
    #[test]
    fn a_dotted_reference_is_decided_by_the_candidate_set_not_by_precedence() {
        let entries = vec![
            entry("id-1", "dev3", "fractal.chat"),
            entry("id-2", "dev4", "notes"),
        ];
        // Only the bare reading exists.
        assert_eq!(
            resolve_address(&entries, "fractal.chat", None).unwrap().id,
            "id-1"
        );
        // Only the qualified reading exists.
        assert_eq!(
            resolve_address(&entries, "dev4.notes", None).unwrap().id,
            "id-2"
        );
        // The qualified reading of a dotted address also resolves, because `dev3` is an admitted
        // host and `fractal.chat` is an address in it.
        assert_eq!(
            resolve_address(&entries, "dev3.fractal.chat", None)
                .unwrap()
                .id,
            "id-1"
        );
    }

    /// Both readings naming different subjects is undecidable, not first-wins.
    #[test]
    fn two_readings_of_one_reference_fail_closed() {
        let entries = vec![
            // The bare reading: host `dev3` has the dotted address `dev4.notes`.
            entry("id-1", "dev3", "dev4.notes"),
            // The qualified reading: host `dev4` has the address `notes`.
            entry("id-2", "dev4", "notes"),
        ];
        let error = resolve_address(&entries, "dev4.notes", None).unwrap_err();
        match error {
            ResolveError::Ambiguous { ids, .. } => assert_eq!(ids, vec!["id-1", "id-2"]),
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    /// The same address on two hosts is legal, so an unqualified reference to it is ambiguous and a
    /// qualified one is exact.
    #[test]
    fn one_address_on_two_hosts_needs_a_host_to_be_decidable() {
        let entries = vec![entry("id-1", "dev3", "chat"), entry("id-2", "dev4", "chat")];
        assert!(matches!(
            resolve_address(&entries, "chat", None),
            Err(ResolveError::Ambiguous { .. })
        ));
        assert_eq!(
            resolve_address(&entries, "dev3.chat", None).unwrap().id,
            "id-1"
        );
        assert_eq!(
            resolve_address(&entries, "chat", Some("dev4")).unwrap().id,
            "id-2"
        );
    }

    /// A pinned host is a boundary: a reference cannot reach another host by spelling it.
    #[test]
    fn a_pinned_host_admits_only_its_own_qualified_split() {
        let entries = vec![entry("id-1", "dev3", "chat"), entry("id-2", "dev4", "chat")];
        assert_eq!(
            resolve_address(&entries, "dev3.chat", Some("dev3"))
                .unwrap()
                .id,
            "id-1"
        );
        assert!(matches!(
            resolve_address(&entries, "dev4.chat", Some("dev3")),
            Err(ResolveError::Unknown { .. })
        ));
    }

    /// Deduplication is by agent ID, so one subject reachable through both readings is not
    /// ambiguous with itself.
    #[test]
    fn one_subject_reached_twice_is_not_ambiguous() {
        // `dev3` has an address that literally reads `dev3.chat`, so the bare reading and the
        // qualified reading are the same subject only if the host also has `chat`.
        let entries = vec![entry("id-1", "dev3", "chat")];
        assert_eq!(
            resolve_address(&entries, "dev3.chat", None).unwrap().id,
            "id-1"
        );
        assert_eq!(
            resolve_address(&entries, "dev3.chat", Some("dev3"))
                .unwrap()
                .id,
            "id-1"
        );
    }

    #[test]
    fn an_unknown_address_fails_with_an_address_specific_diagnostic() {
        let entries = vec![entry("id-1", "dev3", "chat")];
        let error = resolve_address(&entries, "ghost", None).unwrap_err();
        assert!(
            format!("{error}").contains("no routable agent has the address 'ghost'"),
            "{error}"
        );
    }

    /// Exact ID selection never falls through to address lookup, and equal bytes across the two
    /// namespaces do not collide.
    #[test]
    fn exact_id_selection_never_falls_through_to_address_lookup() {
        let entries = vec![
            entry("0199b8f4-8d3a-7c21-9a44-6f85b7320ea1", "dev3", "chat"),
            entry("id-2", "dev4", "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1"),
        ];
        // The ID namespace answers with the subject that owns the ID.
        assert_eq!(
            resolve_id(&entries, "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1")
                .unwrap()
                .host,
            "dev3"
        );
        // The address namespace answers with the subject that owns the address.
        assert_eq!(
            resolve_address(&entries, "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1", None)
                .unwrap()
                .host,
            "dev4"
        );
        // An ID that is not an ID does not become an address.
        assert!(matches!(
            resolve_id(&entries, "chat"),
            Err(ResolveError::Unknown { .. })
        ));
    }

    /// An exact selector answers to either immutable key, so a cutover cannot orphan the values
    /// every durable surface is keyed on: `ST_AGENT`, a task ID, a record endpoint.
    #[test]
    fn an_exact_selector_answers_to_the_positional_key_after_an_address_moves() {
        let moved = AddressBookEntry {
            id: "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1".to_owned(),
            bus_identity: "dev3.verifier".to_owned(),
            host: "dev3".to_owned(),
            address: "keymap.verifier".to_owned(),
        };
        let entries = vec![moved];
        // The route moved, so the old spelling is not an address any more.
        assert!(matches!(
            resolve_address(&entries, "dev3.verifier", None),
            Err(ResolveError::Unknown { .. })
        ));
        // Both immutable keys still name the subject exactly.
        for key in [
            "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1",
            "dev3.verifier",
        ] {
            assert_eq!(
                resolve(&entries, &AgentSelector::Id(key.to_owned()), None)
                    .unwrap()
                    .address,
                "keymap.verifier"
            );
        }
        // The new route resolves, bare and host-qualified.
        for reference in ["keymap.verifier", "dev3.keymap.verifier"] {
            assert_eq!(
                resolve_address(&entries, reference, None).unwrap().id,
                "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1"
            );
        }
    }

    #[test]
    fn the_selector_forms_route_to_their_own_namespace() {
        let entries = vec![entry("id-1", "dev3", "chat")];
        assert_eq!(
            resolve(&entries, &AgentSelector::Id("id-1".to_owned()), None)
                .unwrap()
                .address,
            "chat"
        );
        assert_eq!(
            resolve(&entries, &AgentSelector::Address("chat".to_owned()), None)
                .unwrap()
                .id,
            "id-1"
        );
        assert!(resolve(&entries, &AgentSelector::Id("chat".to_owned()), None).is_err());
    }

}
