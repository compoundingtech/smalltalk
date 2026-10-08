import { child } from '../context.ts'
import { agent } from '../factories/agent.ts'
import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

export const rosterVariants: VariantTable['roster'] = {
  empty: (_ctx, base) => cleared(base.roster, { agents: [], runtimes: [], machines: [], order: [] }),
  loading: (_ctx, base) => ({ ...base.roster, loading: true }),
  'one-agent': (ctx, base) => {
    if (base.roster.state.agents.length === 0) {
      const member = ctx.cast.agents[0]
      if (member === undefined) throw new Error('one-agent variant requires a cast agent')
      const built = agent(child(ctx, 'one-agent'), member, { state: 'stopped', sinceMs: 0 })
      return cleared(base.roster, { agents: [built.agent], runtimes: [], machines: [], order: [member.id] })
    }
    const [first] = base.roster.state.order
    const agents = base.roster.state.agents.filter((value) => value.id === first)
    const runtimes = base.roster.state.runtimes.filter((runtime) => runtime.owner_id === first)
    const hosts = new Set(runtimes.map((runtime) => runtime.owner_host_id))
    return {
      ...base.roster,
      state: {
        agents,
        runtimes,
        machines: base.roster.state.machines
          .filter((machine) => hosts.has(machine.host_id))
          .map((machine) => ({ ...machine, runtime_ids: runtimes.map((runtime) => runtime.id), occupancy: { running_runtimes: runtimes.length } })),
        order: first === undefined ? [] : [first],
      },
      timeline: base.roster.timeline.filter((event) => event.upserts.every((value) => value.id === first)),
    }
  },
}
