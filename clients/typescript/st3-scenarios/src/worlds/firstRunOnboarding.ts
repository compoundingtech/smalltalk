import { child } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { toolCall } from '../kit/factories/toolCall.ts'
import { thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { Slice } from '../kit/slice.ts'
import { liveSync } from '../kit/variants.ts'
import type { WorldDefinition } from '../kit/world.ts'

/** A genuinely empty daemon acquires its first resources and completes its first run. */
export const firstRunOnboarding: WorldDefinition = {
  id: 'first-run-onboarding', title: 'First run onboarding', seed: 4,
  narrative: 'An empty daemon joins its first machine, launches its first agent and mission, and finishes a green smoke run.',
  cast: { project: 'sprout', roles: ['builder'], hosts: 1, people: 1, missions: [{ slug: 'first-smoke', title: 'Prove the first run works', steps: ['Run the project smoke check'] }] },
  slices: (ctx) => {
    const member = ctx.cast.agents[0]!
    const person = ctx.cast.people[0]!
    const mission = ctx.cast.missions[0]!
    const source = { _tag: 'synthetic' as const, seed: 4 }
    const starting = agent(child(ctx, 'start'), member, { state: 'running', sinceMs: 2_000, harnessState: 'idle' })
    const working = agent(child(ctx, 'work'), member, { state: 'running', sinceMs: 2_000, lastActivityMs: 5_000, harnessState: 'busy', mission, step: 0, workState: 'claimed', workSinceMs: 5_000 })
    const finished = agent(child(ctx, 'finish'), member, { state: 'waiting', sinceMs: 16_000, harnessState: 'idle', mission, step: 0, workState: 'completed' })
    const machine = resources.machine(ctx, member.host, [], [])
    const joined = { ...machine, updated_at: ctx.t.at(1_000), transports: machine.transports.map((transport) => ({ ...transport, last_success_at: ctx.t.at(1_000) })) }
    const launch = resources.attention(ctx, { key: 'first-launch', kind: 'human-gate', person, source: member.id, requester: member, title: 'Launch your first mission', detail: 'Run the sprout smoke check on the newly connected machine.', priority: 'normal', requestedMs: 3_000, actions: ['custom.reply'] })
    const roster: Slice<'roster'> = {
      kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
      state: { agents: [], runtimes: [], machines: [], order: [] },
      timeline: [
        { _tag: 'changes', at_ms: 1_000, store: 0, upserts: [joined], removes: [] },
        { _tag: 'changes', at_ms: 2_000, store: 0, upserts: [starting.agent, starting.runtime!, { ...joined, revision: 'mh-start', updated_at: ctx.t.at(2_000), runtime_ids: [starting.runtime!.id], occupancy: { running_runtimes: 1 } }], removes: [], order: [member.id] },
        { _tag: 'changes', at_ms: 5_000, store: 0, upserts: [working.agent, working.runtime!], removes: [] },
        { _tag: 'changes', at_ms: 6_000, store: 0, upserts: [{ ...joined, revision: 'mh-terminal', updated_at: ctx.t.at(6_000), runtime_ids: [starting.runtime!.id, member.terminalRuntime], occupancy: { running_runtimes: 2 }, work: [mission.steps[0]!.work] }], removes: [] },
        { _tag: 'changes', at_ms: 16_000, store: 0, upserts: [finished.agent, finished.runtime!], removes: [] },
      ],
    }
    const details: Slice<'details'> = {
      kind: 'details', variant: 'default', source, decode: 'strict', loading: false, state: { missions: [], work: [] },
      timeline: [5_000, 16_000].map((at_ms) => ({ _tag: 'changes', at_ms, store: 0, upserts: [resources.mission(ctx, mission, at_ms === 5_000 ? 'running' : 'completed', at_ms), resources.work(ctx, { mission, step: 0, claimant: member, state: at_ms === 5_000 ? 'claimed' : 'completed', updatedMs: at_ms, goals: ['pnpm test smoke exits successfully'] })], removes: [] })),
    }
    const attention: Slice<'attention'> = {
      kind: 'attention', variant: 'default', source, decode: 'strict', loading: false, state: { attention: [], messages: [] },
      timeline: [
        { _tag: 'changes', at_ms: 3_000, store: 0, upserts: [launch], removes: [] },
        { _tag: 'changes', at_ms: 5_000, store: 0, upserts: [resources.message(ctx, { key: 'launch-answer', from: person.id, to: member.id, title: 'First mission approved', content: 'Launch the smoke check.', sentMs: 5_000 })], removes: [launch.id] },
        { _tag: 'changes', at_ms: 16_000, store: 0, upserts: [resources.message(ctx, { key: 'first-green', from: member.id, to: person.id, title: 'First run is green', content: 'The smoke check passed: 3 tests, 0 failures.', sentMs: 16_000 })], removes: [] },
      ],
    }
    const c = child(ctx, 'conversation')
    const cursor = thread(member)
    const items = turn(c, cursor, { atMs: 5_000, from: { _tag: 'person', person }, text: 'Launch the smoke check.', stepMs: 2_000, steps: [{ _tag: 'say', text: 'The first mission is running. Checking sprout now.' }, { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command: 'pnpm test smoke' }, outcome: 'ok', output: '3 tests passed; 0 failures', durationMs: 2_000 }) }, { _tag: 'say', text: 'The first run is green: 3 tests passed, 0 failures.' }], status: 'completed' })
    const conversation: Slice<'conversation'> = { kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false, state: { threads: [] }, timeline: [{ _tag: 'thread-create', at_ms: 5_000, store: 0, thread: { agent: member.id, session_id: member.session, items: [], page_size: 50, has_more: false } }, ...items.map((item) => ({ _tag: 'entries' as const, at_ms: Date.parse(item.timestamp) - ctx.t.now, store: 0, agent: member.id, items: [item] }))] }
    const run = terminalRun(child(ctx, 'terminal'), member, { startedAtMs: 6_000, command: 'pnpm test smoke', lines: ['Running sprout smoke check', '\u001b[32m3 tests passed; 0 failures\u001b[0m'], exit: 0 })
    const record = terminalRecord(ctx, member, run, 6_000)
    const terminal: Slice<'terminal'> = { kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false, state: { terminals: [] }, timeline: [{ _tag: 'terminal-create', at_ms: 6_000, store: 0, record }, ...run.screens.map(({ at_ms, screen }) => ({ _tag: 'screen' as const, at_ms, store: 0, terminal: member.terminal, screen }))] }
    return { roster, details, attention, conversation, terminal, sync: liveSync(ctx, { conversation }) }
  },
}
