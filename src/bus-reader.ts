// bus-reader.ts — a minimal, read-only programmatic view of the bus.
//
// Purpose: let another tool (e.g. convoy) IMPORT the bus reads it needs
// instead of shelling out to the `st` CLI and parsing JSON. It runs the
// exact same core as `st agents`, minus the process spawn + text parsing.
//
// READ-ONLY BY CONSTRUCTION: this surface exposes only reads, so a consumer
// cannot accidentally write the bus. Writes (send / setStatus / archive /
// sweep) live on the full createSt() handle, deliberately not here.
//
// PURELY ADDITIVE: the CLI (bin/st) and the createSt() handle are unchanged;
// this only adds a new export. Today the surface is exactly what convoy's
// bus.ts consumes — a single `agents()` read — and is kept minimal on
// purpose; grow it only when a real consumer needs a specific read.

import {
  getAgents,
  type AgentSummary,
  type AgentSummaryEnriched,
} from './commands/agents.ts';

// `AgentSummary` / `AgentSummaryEnriched` are already public via the package
// index (exported from ./commands/agents.ts), so consumers can type against
// them; we only reference them here.

export interface BusReaderOptions {
  /** The state root to read from (ST_ROOT). */
  root: string;
}

export interface BusReaderAgentsOptions {
  /**
   * Include `lastActivity` (newest mtime across inbox/archive/status) and
   * `inbox` (unread count) per agent. Mirrors `st agents --enrich`.
   */
  enrich?: boolean;
  /** Filter to agents whose effective status matches this state. */
  status?: string;
}

/**
 * Read-only view of the bus. Create one with {@link createBusReader}.
 */
export interface BusReader {
  /**
   * List agents. Without `enrich`, each is `{ identity, status, name }`;
   * with `enrich: true`, also `{ lastActivity, inbox }`. Same data as
   * `st agents --json [--enrich]`.
   */
  agents(opts?: BusReaderAgentsOptions & { enrich?: false }): AgentSummary[];
  agents(opts: BusReaderAgentsOptions & { enrich: true }): AgentSummaryEnriched[];
}

/**
 * Create a read-only bus reader rooted at `opts.root`. It walks the state
 * tree on each call (no caching, no watchers) and never writes.
 */
export function createBusReader(opts: BusReaderOptions): BusReader {
  const { root } = opts;
  return {
    agents(o: BusReaderAgentsOptions = {}) {
      return getAgents(root, {
        ...(o.enrich !== undefined && { enrich: o.enrich }),
        ...(o.status !== undefined && { status: o.status }),
      });
    },
  } as BusReader;
}
