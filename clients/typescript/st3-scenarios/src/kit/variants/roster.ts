import { child, type FactoryContext } from '../context.ts'
import { agent, type AgentOptions } from '../factories/agent.ts'
import { machine } from '../resources.ts'
import type { RosterState } from '../slice.ts'
import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

const states: readonly AgentOptions['state'][] = ['desired', 'starting', 'running', 'waiting', 'stopped', 'failed', 'suspended']
const rosterAt = (ctx: FactoryContext, phase: number): RosterState => {
  const built = ctx.cast.agents.map((member, index) => agent(child(ctx, `states/${phase}/${member.key}`), member, {
    state: states[(phase + index) % states.length] ?? 'desired', sinceMs: phase === 0 ? -60_000 : phase * 2_000,
    harnessState: (['idle', 'ready', 'busy', 'blocked'] as const)[(phase + index) % 4],
    reachability: (['local', 'remote', 'unreachable', 'unknown', 'reachable', 'indeterminate'] as const)[(phase + index) % 6],
    blockedOn: (phase + index) % 7 === 3 ? 'Awaiting review of the session argument' : undefined,
    fault: (phase + index) % 7 === 5 ? 'Typecheck failed: session argument missing' : undefined,
  }))
  const runtimes = built.flatMap((value) => value.runtime ?? [])
  return { agents: built.map((value) => value.agent), runtimes, machines: ctx.cast.hosts.map((host) => machine(ctx, host, runtimes.filter((runtime) => runtime.owner_host_id === host.id).map((runtime) => runtime.id), [])), order: ctx.cast.agents.map((member) => member.id) }
}

export const rosterVariants: VariantTable['roster'] = {
  empty: (_ctx, base) => cleared(base.roster, { agents: [], runtimes: [], machines: [], order: [] }),
  loading: (_ctx, base) => ({ ...base.roster, loading: true }),
  'one-agent': (ctx, base) => {
    const member = ctx.cast.agents[0]
    if (member === undefined) throw new Error('one-agent variant requires a cast agent')
    const built = agent(child(ctx, 'one-agent'), member, { state: 'stopped', sinceMs: -60_000 })
    return cleared(base.roster, { agents: [built.agent], runtimes: [], machines: [machine(ctx, member.host, [], [])], order: [member.id] })
  },
  'all-states': (ctx, base) => {
    const initial = rosterAt(ctx, 0)
    return { ...cleared(base.roster, initial), timeline: states.slice(1).map((_state, index) => {
      const phase = index + 1
      const next = rosterAt(ctx, phase)
      return { _tag: 'changes' as const, at_ms: phase * 2_000, store: 0, upserts: [...next.agents, ...next.runtimes, ...next.machines], removes: ctx.cast.agents.filter((member) => !next.runtimes.some((runtime) => runtime.id === member.runtime)).map((member) => member.runtime), order: next.order }
    }) }
  },
  // TODO(Axe 0crzkm): roster.huge awaits the approved storage decision.
}
