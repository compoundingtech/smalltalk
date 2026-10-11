import { child, scenarioId } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { entry, thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { RawResource, Slice } from '../kit/slice.ts'
import { liveSync } from '../kit/variants.ts'
import type { WorldDefinition } from '../kit/world.ts'

/** A newer daemon publishes four independent forms of forward-compatible wire data. */
export const unknownFields: WorldDefinition = {
  id: 'unknown-fields',
  title: 'Unknown fields',
  narrative: 'An Atlas builder checks a newer daemon: an extra agent field, a reconciling state, a compatibility-probe resource and diagnostic-summary entries survive tolerant reads without hiding unrelated schema errors.',
  seed: 19,
  cast: {
    project: 'atlas', roles: ['builder', 'reviewer'], hosts: 2, people: 1,
    missions: [{ slug: 'forward-compatibility', title: 'Check forward-compatible client reads', steps: ['Preserve future daemon observations'] }],
  },
  slices: (ctx) => {
    const member = ctx.cast.agents[0]!
    const person = ctx.cast.people[0]!
    const mission = ctx.cast.missions[0]!
    const source = { _tag: 'synthetic' as const, seed: 19 }
    const common = { variant: 'default', source, loading: false }
    const built = agent(child(ctx, 'builder'), member, {
      state: 'running', sinceMs: -120_000, lastActivityMs: -2_000,
      harnessState: 'busy', mission, step: 0, workState: 'claimed',
    })
    const futureAgent = {
      ...built.agent,
      state: 'reconciling',
      protocol_features: { diagnostic_summaries: true, compatibility_probes: true },
    }
    const probe: RawResource = {
      id: scenarioId(ctx, 'compatibility-probe', 'builder'),
      kind: 'compatibility-probe', revision: '1', updated_at: ctx.t.at(-2_000),
      owner_id: member.id, mission_id: mission.id,
      payload: {
        status: 'running', preserved_fields: ['protocol_features', 'reconciling', 'diagnostic_summary'],
        // Opaque payload text is not a schema instant, even when it looks like one.
        sample_timestamp: '2035-01-02T03:04:05.000Z',
      },
    }
    const passedProbe: RawResource = {
      ...probe, revision: '2', updated_at: ctx.t.at(2_000),
      payload: { status: 'passed', preserved_fields: ['protocol_features', 'reconciling', 'diagnostic_summary'], sample_timestamp: '2035-01-02T03:04:05.000Z' },
    }
    const roster: Slice<'roster'> = {
      ...common, kind: 'roster', decode: 'tolerant',
      // Extra keys need deletion; open enums and future kinds need strict-clean repair witnesses.
      unknown: [
        { pointer: '/state/agents/0/protocol_features' },
        { pointer: '/state/agents/0/state', known_value: built.agent.state },
        { pointer: '/state/resources/0', known_value: built.agent },
        { pointer: '/timeline/0/upserts/0', known_value: built.agent },
      ],
      state: {
        agents: [futureAgent], runtimes: built.runtime === null ? [] : [built.runtime],
        machines: [resources.machine(ctx, member.host, [member.runtime], [mission.steps[0]!.work])],
        resources: [probe], order: [member.id],
      },
      timeline: [
        { _tag: 'changes', at_ms: 2_000, store: 0, upserts: [passedProbe], removes: [] },
        { _tag: 'changes', at_ms: 5_000, store: 0, upserts: [], removes: [probe.id] },
      ],
    }
    const cursor = thread(member)
    const past = turn(child(ctx, 'conversation'), cursor, {
      atMs: -120_000, from: { _tag: 'person', person },
      text: 'Check that the older client keeps the newer daemon observations without treating them as known states.',
      steps: [{ _tag: 'say', text: 'The daemon reports reconciling and a compatibility-probe resource. I am checking HTTP reads and collection frames before retiring the transient probe.' }],
    })
    const futureSummary = entry(ctx, cursor, {
      atMs: -2_000, role: 'system', type: 'diagnostic_summary',
      body: { probe_id: probe.id, status: 'running', checks: ['agent-extra-key', 'agent-state', 'resource-kind', 'timeline-type'] },
    })
    const knownSummary = {
      ...futureSummary, type: 'content' as const,
      body: { media_type: 'text/plain', text: 'Compatibility probe started; future fields are preserved.' },
    }
    const futureResult = entry(ctx, cursor, {
      atMs: 2_000, role: 'system', type: 'diagnostic_summary',
      body: { probe_id: probe.id, status: 'passed', checks: ['agent-extra-key', 'agent-state', 'resource-kind', 'timeline-type'] },
    })
    const knownResult = {
      ...futureResult, type: 'content' as const,
      body: { media_type: 'text/plain', text: 'Compatibility probe passed on HTTP and collection streams.' },
    }
    const conversation: Slice<'conversation'> = {
      ...common, kind: 'conversation', decode: 'tolerant',
      // Future entry bodies are opaque; replacing the complete entry repairs both type and body.
      unknown: [
        { pointer: `/state/threads/0/items/${past.length}`, known_value: knownSummary },
        { pointer: '/timeline/0/items/0', known_value: knownResult },
      ],
      state: { threads: [{ agent: member.id, session_id: member.session, items: [...past, futureSummary], page_size: 100, has_more: false }] },
      timeline: [{ _tag: 'entries', at_ms: 2_000, store: 0, agent: member.id, items: [futureResult] }],
    }
    const run = terminalRun(child(ctx, 'terminal'), member, {
      startedAtMs: -15_000, command: 'pnpm test client-forward-compatibility',
      lines: ['Agent extra fields: preserved', 'Future agent state: reconciling', 'Compatibility probe: watching HTTP and collection streams'],
    })
    return {
      roster, conversation,
      details: {
        ...common, kind: 'details', decode: 'strict',
        state: {
          missions: [resources.mission(ctx, mission, 'running', -120_000)],
          work: [resources.work(ctx, { mission, step: 0, state: 'claimed', updatedMs: -120_000, claimant: member, goals: ['Unknown values survive HTTP and socket replay', 'Only declared contaminants fail strict decoding'] })],
        },
        timeline: [],
      },
      attention: { ...common, kind: 'attention', decode: 'strict', state: { attention: [], messages: [] }, timeline: [] },
      terminal: { ...common, kind: 'terminal', decode: 'strict', state: { terminals: [terminalRecord(ctx, member, run, -15_000)] }, timeline: [] },
      sync: { ...liveSync(ctx, { conversation }), source },
    }
  },
}
