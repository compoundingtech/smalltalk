//! One walk of the declared `supervisor` edge, shared by every consumer that needs it.
//!
//! DING walks it to render a relationship marker; resync walks it to reach the carriers an agent's
//! ancestors declare. Both need the same cycle guard and the same depth bound, so the traversal
//! lives here once rather than being reimplemented per consumer.

use std::collections::HashSet;

use crate::AgentSpec;

/// A declaration graph deeper than this is a declaration fault, not a chain worth walking. The
/// bound is what keeps a malformed catalog from turning a walk into a hang; validation rejecting
/// cycles is not relied on here.
pub const SUPERVISOR_CHAIN_LIMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorChainError {
    Cycle,
    MissingSupervisor,
    DepthLimit,
}

/// Resolve one declared supervisor reference to its spec. A reference matches either a fully
/// qualified bus id or a bare identity on the local host.
pub fn resolve_spec<'a>(
    specs: &'a [AgentSpec],
    identity: &str,
    local_host: &str,
) -> Option<&'a AgentSpec> {
    let mut matches = specs.iter().filter(|spec| {
        spec.bus_id(local_host) == identity
            || (spec.resolved_host(local_host) == local_host && spec.identity == identity)
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Whether a declaration occupies its host's root slot in the org chart: no supervisor, and not
/// retired. Retirement — either spelling; the folded `AgentDesiredState` normalizes legacy
/// `retired #true` and `desired-state "retired"` — removes a declaration from the org chart, so a
/// retired root does not hold the slot. Suspension keeps the declaration in the chart, so a
/// suspended root still counts (#402).
pub fn is_counted_root(spec: &AgentSpec) -> bool {
    spec.supervisor.is_none() && !spec.desired_state.is_retired()
}

/// Every spec from `start` to the root inclusive, `start` first.
pub fn chain<'a>(
    specs: &'a [AgentSpec],
    start: &'a AgentSpec,
    this_host: &str,
) -> Result<Vec<&'a AgentSpec>, SupervisorChainError> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current = start;

    for _ in 0..SUPERVISOR_CHAIN_LIMIT {
        if !visited.insert(current.bus_id(this_host)) {
            return Err(SupervisorChainError::Cycle);
        }
        chain.push(current);
        let Some(supervisor) = current.supervisor.as_deref() else {
            return Ok(chain);
        };
        current = resolve_spec(specs, supervisor, current.resolved_host(this_host))
            .ok_or(SupervisorChainError::MissingSupervisor)?;
    }

    Err(SupervisorChainError::DepthLimit)
}

/// The bus ids of [`chain`], `start` first. What DING's relationship marker keys on.
pub fn chain_bus_ids(
    specs: &[AgentSpec],
    start: &AgentSpec,
    this_host: &str,
) -> Result<Vec<String>, SupervisorChainError> {
    Ok(chain(specs, start, this_host)?
        .into_iter()
        .map(|spec| spec.bus_id(this_host))
        .collect())
}

/// Every ancestor of `start`, nearest first, excluding `start` itself.
pub fn ancestors<'a>(
    specs: &'a [AgentSpec],
    start: &'a AgentSpec,
    this_host: &str,
) -> Result<Vec<&'a AgentSpec>, SupervisorChainError> {
    let mut chain = chain(specs, start, this_host)?;
    chain.remove(0);
    Ok(chain)
}
