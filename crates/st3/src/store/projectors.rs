//! The projector registry: which domain projections the claim fold calls, and in what order.
//!
//! The fold (append, replication, replay) lives in the store's core. The tables a domain derives
//! from claims are kept by that domain's projector, and the core reaches them only through the
//! tables below, never by name, so the store can be cut into domain crates without the core naming
//! any of them. Order is part of the storage contract: every phase runs its entries in the order
//! listed, and `projectors_tests` pins that order per phase and per claim kind.
use super::*;

/// The claim kinds an append projector takes.
#[derive(Clone, Copy, Debug)]
pub(super) enum Takes {
    /// Every appended claim.
    Every,
    /// Only claims of these kinds.
    Kinds(&'static [&'static str]),
}

impl Takes {
    pub(super) fn takes(self, kind: &str) -> bool {
        match self {
            Takes::Every => true,
            Takes::Kinds(kinds) => kinds.contains(&kind),
        }
    }
}

/// A domain hook that writes only its own tables.
pub(super) struct Hook {
    #[cfg_attr(not(test), expect(dead_code, reason = "Only the order tests read the names."))]
    pub(super) name: &'static str,
    pub(super) run: fn(&Transaction<'_>) -> Result<()>,
}

/// A domain hook for one claim, after its event row is written. In `REPLICATED` the name is also
/// the projection stage an error names.
pub(super) struct AppendProjector {
    pub(super) name: &'static str,
    pub(super) takes: Takes,
    pub(super) project: fn(&Transaction<'_>, &ClaimRecord) -> Result<()>,
}

/// One stage of the full graph replay, with the progress phase it reports.
pub(super) struct ReplayStage {
    pub(super) phase: &'static str,
    pub(super) run: fn(&Transaction<'_>) -> Result<()>,
}

/// Installs each domain's tables and triggers when the store opens its projections.
pub(super) const OPEN: &[Hook] = &[
    Hook { name: "publication-facts", run: publication_facts::initialize },
    Hook { name: "custom", run: custom::open },
    Hook { name: "resources", run: resources::open },
    Hook { name: "glass-heads", run: glass_heads::open },
    Hook { name: "directive-notes", run: directive_notes::flush },
    Hook { name: "agent-messages", run: agent_messages::open },
    Hook { name: "arrangements", run: arrangements::open },
    Hook { name: "limits", run: limits::open_limits },
];

/// Projects a locally appended claim, in this order, for the kinds each entry takes.
pub(super) const APPEND: &[AppendProjector] = &[
    AppendProjector {
        name: "custom",
        takes: Takes::Every,
        project: |transaction, _| custom::flush(transaction),
    },
    AppendProjector {
        name: "resources",
        takes: Takes::Kinds(&["resource.observed"]),
        project: |transaction, claim| resources::refresh(transaction, &claim.subject),
    },
    AppendProjector {
        name: "glass-heads",
        takes: Takes::Kinds(&["glass.upserted", "glass.deleted"]),
        project: |transaction, _| glass_heads::flush(transaction),
    },
    AppendProjector {
        name: "retirements",
        takes: Takes::Kinds(&[retirements::KIND]),
        project: |transaction, claim| {
            retirements::project_subject(transaction, &claim.subject).map_err(anyhow::Error::new)
        },
    },
    AppendProjector {
        name: "directive-notes",
        takes: Takes::Kinds(&[st3_schema::directive_notes::KIND]),
        project: |transaction, _| directive_notes::flush(transaction),
    },
    AppendProjector {
        name: "agent-messages",
        takes: Takes::Kinds(&["message.sent"]),
        project: |transaction, _| agent_messages::flush(transaction),
    },
    AppendProjector {
        name: "limits",
        takes: Takes::Kinds(&["harness.limits"]),
        project: |transaction, _| limits::flush_limits(transaction).map(drop),
    },
    AppendProjector {
        name: "arrangements",
        takes: Takes::Kinds(&["arrangement.edited"]),
        project: arrangements::project,
    },
];

/// Projects a replicated claim the incremental projection extends the graph with, after every
/// claim of the range has its event row.
pub(super) const REPLICATED: &[AppendProjector] = &[AppendProjector {
    name: "arrangement-project",
    takes: Takes::Kinds(&["arrangement.edited"]),
    project: arrangements::project,
}];

/// Catches each domain up with the claims a projection pass admitted.
pub(super) const FLUSH: &[Hook] = &[
    Hook { name: "custom", run: custom::flush },
    Hook { name: "resources", run: resources::flush },
    Hook { name: "glass-heads", run: glass_heads::flush },
    Hook { name: "directive-notes", run: directive_notes::flush },
    Hook { name: "agent-messages", run: agent_messages::flush },
    Hook { name: "limits", run: |transaction| limits::flush_limits(transaction).map(drop) },
];

/// Rebuilds the claim-derived domain tables from the claims, after the operations: the explicit
/// projection rebuild and the checkpoint proof's replay. Custom projections keep their tables
/// here; only the full graph replay rebuilds them.
pub(super) const REBUILD: &[Hook] = &[
    Hook { name: "planning", run: rebuild_planning_tx },
    Hook { name: "resources", run: resources::rebuild },
    Hook { name: "glass-heads", run: glass_heads::rebuild },
    Hook { name: "directive-notes", run: directive_notes::rebuild },
    Hook { name: "arrangements", run: arrangements::rebuild },
];

/// The domain stages of the full graph replay, after the mission runs are replayed.
pub(super) const GRAPH_REPLAY: &[ReplayStage] = &[
    ReplayStage { phase: "full-replay/planning", run: rebuild_planning_tx },
    ReplayStage { phase: "full-replay/resources", run: resources::rebuild },
    ReplayStage { phase: "full-replay/glass-heads", run: glass_heads::rebuild },
    ReplayStage { phase: "full-replay/directive-notes", run: directive_notes::rebuild },
    ReplayStage { phase: "full-replay/custom", run: custom::rebuild },
    ReplayStage { phase: "full-replay/arrangements", run: arrangements::rebuild },
    ReplayStage { phase: "full-replay/flush", run: publication_facts::reset },
];

pub(super) fn run_hooks(hooks: &[Hook], transaction: &Transaction<'_>) -> Result<()> {
    hooks.iter().try_for_each(|hook| (hook.run)(transaction))
}

/// Run each projector that takes `claim`'s kind, in the registry's order.
pub(super) fn project_claim(
    projectors: &[AppendProjector],
    transaction: &Transaction<'_>,
    claim: &ClaimRecord,
) -> Result<()> {
    projectors
        .iter()
        .filter(|projector| projector.takes.takes(&claim.kind))
        .try_for_each(|projector| (projector.project)(transaction, claim))
}
