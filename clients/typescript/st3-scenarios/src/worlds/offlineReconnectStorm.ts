import { child } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { entry, revise, thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { Slice, SyncExpectation } from '../kit/slice.ts'
import type { WorldDefinition } from '../kit/world.ts'

/** Offline edits accumulate on the server; each reconnect replaces the stale client window. */
export const offlineReconnectStorm: WorldDefinition = {
  id: 'offline-reconnect-storm',
  title: 'Offline reconnect storm',
  narrative: 'A deployment summary streams through three socket drops. Backoff preserves stale content until authoritative snapshots restore the final revisions.',
  seed: 6,
  cast: { project: 'harbor', roles: ['builder', 'reviewer'], hosts: 2, people: 1,
    missions: [{ slug: 'deploy-summary', title: 'Verify deployment recovery', steps: ['Stream the deployment verification report'] }] },
  slices: (ctx) => {
    const source = { _tag: 'synthetic' as const, seed: 6 }
    const builder = ctx.cast.agents[0]!
    const mission = ctx.cast.missions[0]!
    const generated = ctx.cast.agents.map((member, index) => agent(child(ctx, member.key), member,
      index === 0 ? { state: 'running', sinceMs: -120_000, harnessState: 'busy', mission, step: 0, workState: 'claimed' }
        : { state: 'waiting', sinceMs: -180_000, harnessState: 'idle' }))
    const runtimes = generated.flatMap((value) => value.runtime ?? [])
    const c = child(ctx, 'conversation')
    const cursor = thread(builder)
    const history = turn(c, cursor, { atMs: -60_000, from: { _tag: 'person', person: ctx.cast.people[0]! },
      text: 'Verify the deployment and stream the health report. Keep the report authoritative after reconnects.',
      steps: [{ _tag: 'say', text: 'Deployment is running. Checking readiness, then the canary error budget.' }] })
    const first = entry(c, cursor, { atMs: -1_000, role: 'assistant', type: 'content', final: false,
      body: { media_type: 'text/markdown', text: 'Readiness: 2 of 3 replicas' } })
    const revision2 = revise(c, first, 1_000, { media_type: 'text/markdown', text: 'Readiness: 3 of 3 replicas. Canary checks pending.' }, false)
    const revision3 = revise(c, revision2, 3_000, { media_type: 'text/markdown', text: 'Readiness: 3 of 3 replicas. Canary checks: 8 of 12 passed.' }, false)
    const revision4 = revise(c, revision3, 9_000, { media_type: 'text/markdown', text: 'Readiness: 3 of 3 replicas. Canary checks: 12 of 12 passed. Observing error budget.' }, false)
    const final = revise(c, revision4, 17_000, { media_type: 'text/markdown', text: 'Deployment verified: 3 of 3 replicas ready, 12 of 12 canary checks passed, error budget unchanged. Safe to complete the rollout.' }, true)
    const transient = entry(c, cursor, { atMs: 7_000, role: 'assistant', type: 'content', final: false,
      body: { media_type: 'text/markdown', text: 'Temporary observation: canary counters are still settling.' } })
    const conversation: Slice<'conversation'> = { kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false,
      state: { threads: [{ agent: builder.id, session_id: builder.session, items: [...history, first], page_size: 50, has_more: false }] },
      timeline: [
        { _tag: 'entries', at_ms: 1_000, store: 0, agent: builder.id, items: [revision2] },
        { _tag: 'entries', at_ms: 3_000, store: 0, agent: builder.id, items: [revision3] },
        { _tag: 'replace', at_ms: 6_000, store: 0, agent: builder.id, session_id: builder.session, items: [...history, revision3], has_more: false },
        { _tag: 'entries', at_ms: 7_000, store: 0, agent: builder.id, items: [transient] },
        { _tag: 'entries', at_ms: 9_000, store: 0, agent: builder.id, items: [revision4] },
        { _tag: 'replace', at_ms: 13_000, store: 0, agent: builder.id, session_id: builder.session, items: [...history, revision4], has_more: false },
        { _tag: 'entries', at_ms: 17_000, store: 0, agent: builder.id, items: [final] },
        { _tag: 'replace', at_ms: 22_000, store: 0, agent: builder.id, session_id: builder.session, items: [...history, final], has_more: false },
      ] }
    const surfaces = ['agents', 'missions', 'work', 'attention', `conversation:${builder.id}`]
    const expected: SyncExpectation[] = surfaces.flatMap((surface): SyncExpectation[] => [
      { surface, at_ms: 0, status: { _tag: 'Live', since: 0 }, compare: 'shape' },
      ...[2_000, 8_000, 16_000].map((at_ms): SyncExpectation => ({ surface, at_ms,
        status: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: at_ms + 3_000, issue: 'collections socket ended' }, lastLiveAt: at_ms === 2_000 ? 0 : at_ms === 8_000 ? 6_000 : 13_000 }, compare: 'shape' })),
      ...[6_000, 13_000, 22_000].map((at_ms): SyncExpectation => ({ surface, at_ms, status: { _tag: 'Live', since: at_ms }, compare: 'shape' })),
    ]).sort((a, b) => a.at_ms - b.at_ms)
    return {
      roster: { kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
        state: { agents: generated.map((value) => value.agent), runtimes,
          machines: ctx.cast.hosts.map((host) => resources.machine(ctx, host, runtimes.filter((runtime) => runtime.owner_host_id === host.id).map((runtime) => runtime.id), [])), order: ctx.cast.agents.map((member) => member.id) }, timeline: [] },
      details: { kind: 'details', variant: 'default', source, decode: 'strict', loading: false,
        state: { missions: [resources.mission(ctx, mission, 'running', -120_000)], work: [resources.work(ctx, { mission, step: 0, state: 'claimed', updatedMs: -120_000, claimant: builder, goals: ['Verify readiness, canary checks and error budget'] })] }, timeline: [] },
      attention: { kind: 'attention', variant: 'default', source, decode: 'strict', loading: false, state: { attention: [], messages: [] }, timeline: [] },
      conversation,
      terminal: { kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false, state: { terminals: [] }, timeline: [] },
      sync: { kind: 'sync', variant: 'default', source, decode: 'strict', loading: false, state: { capabilities: resources.capabilities(ctx), expected },
        timeline: [2_000, 8_000, 16_000].flatMap((at_ms) => [
          { _tag: 'close' as const, at_ms, store: 0, code: 1006, reason: '' },
          { _tag: 'reopen' as const, at_ms, store: 0, after_ms: at_ms === 16_000 ? 5_000 : 3_000 },
        ]) },
    }
  },
}
