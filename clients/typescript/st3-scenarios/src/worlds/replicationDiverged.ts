import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** Replication divergence is a notice, not a transport failure. */
export const replicationDiverged: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'replication-diverged',
  title: 'Replication diverged',
  narrative: 'One peer has claims on both sides of a divergence. Reads remain live while the peer is repaired.',
  slices: (ctx) => {
    const base = fleetMidRefactor.slices(ctx)
    return {
      ...base,
      sync: {
        ...base.sync,
        timeline: [
          { _tag: 'notice', at_ms: 0, store: 0, peers: [{
            host_id: ctx.cast.hosts[1]!.id,
            diverged_since: ctx.t.at(-120_000),
            last_exchange_at: ctx.t.at(-5_000),
            local_only_envelopes: 17,
            peer_only_envelopes: 9,
          }] },
          { _tag: 'notice-clear', at_ms: 30_000, store: 0 },
        ],
      },
    }
  },
}
