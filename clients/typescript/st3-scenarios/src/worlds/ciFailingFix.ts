import type { Attention, TimelineEntry, Work } from '@smalltalk/st3-client'

import { child, type FactoryContext } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { diff } from '../kit/factories/diff.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { toolCall } from '../kit/factories/toolCall.ts'
import { thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import { screenAt } from '../kit/screen.ts'
import type { Slice } from '../kit/slice.ts'
import { parseTimestamp } from '../kit/time.ts'
import { liveSync } from '../kit/variants.ts'
import type { Slices, WorldDefinition } from '../kit/world.ts'

const SEED = 21
const command = 'pnpm --filter @atlas/api test'
const redOutput = [
  'FAIL packages/api/test/parseOptions.test.ts > accepts an omitted limit',
  'TypeError: Expected a number, received undefined',
  'at parseOptions (packages/api/src/options.ts:18)',
  'Tests: 1 failed, 47 passed; exit 1',
]
const greenOutput = ['PASS packages/api/test/parseOptions.test.ts', 'Tests: 48 passed, 0 failed', 'CI retry #2: typecheck, test, build all passed']

/** A dependency regression is bisected and repaired without reverting the bump. */
export const ciFailingFix: WorldDefinition = {
  id: 'ci-failing-fix',
  title: 'CI failing fix',
  narrative: 'main is red after option-schema 4.1 changes omitted-value handling. A bisect finds the bump, a one-line default restores the contract, and the CI retry turns green.',
  seed: SEED,
  cast: {
    project: 'atlas', roles: ['builder'], hosts: 1, people: 1,
    missions: [{ slug: 'repair-dependency-bump', title: 'Repair CI after the option-schema bump', steps: [
      'Check main after the dependency bump', 'Bisect the regression', 'Restore the omitted limit default', 'Rerun the CI matrix',
    ] }],
  },
  slices: (ctx) => build(ctx),
}

const build = (ctx: FactoryContext): Slices => {
  const builder = ctx.cast.agents[0]!
  const person = ctx.cast.people[0]!
  const mission = ctx.cast.missions[0]!
  const source = { _tag: 'synthetic' as const, seed: SEED }
  const initial = agent(child(ctx, 'builder/red'), builder, {
    state: 'running', sinceMs: -600_000, lastActivityMs: -30_000, harnessState: 'busy',
    mission, step: 1, workState: 'claimed', upcoming: [2, 3],
  })
  const patching = agent(child(ctx, 'builder/patching'), builder, {
    state: 'running', sinceMs: 10_000, harnessState: 'busy', mission, step: 2, workState: 'claimed', upcoming: [3],
  })
  const verifying = agent(child(ctx, 'builder/verifying'), builder, {
    state: 'running', sinceMs: 18_000, harnessState: 'busy', mission, step: 3, workState: 'verifying',
  })
  const finished = agent(child(ctx, 'builder/green'), builder, {
    state: 'waiting', sinceMs: 30_000, harnessState: 'idle', mission,
  })
  const roster: Slice<'roster'> = {
    kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
    state: {
      agents: [initial.agent], runtimes: [initial.runtime!], order: [builder.id],
      machines: [resources.machine(ctx, builder.host, [builder.runtime, builder.terminalRuntime], [mission.steps[1]!.work])],
    },
    timeline: [
      { _tag: 'changes', at_ms: 10_000, store: 0, upserts: [patching.agent, patching.runtime!], removes: [] },
      { _tag: 'changes', at_ms: 18_000, store: 0, upserts: [verifying.agent, verifying.runtime!], removes: [] },
      { _tag: 'changes', at_ms: 30_000, store: 0, upserts: [finished.agent, finished.runtime!, { ...resources.machine(ctx, builder.host, [builder.runtime, builder.terminalRuntime], []), revision: 'mh-green', updated_at: ctx.t.at(30_000) }], removes: [] },
    ],
  }
  const goals = ['Keep option-schema 4.1 installed', 'Omitted limit retains the default of 20', 'CI typecheck, test and build pass']
  const work = (step: number, state: Work['state'], updatedMs: number, revision: string): Work => ({
    ...resources.work(ctx, { mission, step, state, updatedMs, claimant: builder, goals }),
    revision, constraints: ['Do not revert the dependency bump or widen the patch'],
    ...(state === 'failed' ? { blocked_reason: redOutput[1] } : {}),
  })
  const details: Slice<'details'> = {
    kind: 'details', variant: 'default', source, decode: 'strict', loading: false,
    state: {
      missions: [resources.mission(ctx, mission, 'running', -30_000)],
      work: [work(0, 'failed', -30_000, 'w-red'), work(1, 'claimed', -25_000, 'w-bisect'), work(2, 'ready', -25_000, 'w-patch-ready'), work(3, 'ready', -25_000, 'w-retry-ready')],
    },
    timeline: [
      { _tag: 'changes', at_ms: 10_000, store: 0, upserts: [work(1, 'completed', 10_000, 'w-bisect-complete'), work(2, 'claimed', 10_000, 'w-patching')], removes: [] },
      { _tag: 'changes', at_ms: 18_000, store: 0, upserts: [work(2, 'completed', 18_000, 'w-patched'), work(3, 'verifying', 18_000, 'w-verifying')], removes: [] },
      { _tag: 'changes', at_ms: 30_000, store: 0, upserts: [
        { ...work(0, 'completed', 30_000, 'w-green'), attempt: 2 }, work(3, 'completed', 30_000, 'w-retry-green'),
        { ...resources.mission(ctx, mission, 'completed', 30_000), revision: 'mr-green' },
      ], removes: [] },
    ],
  }
  const card = resources.attention(ctx, {
    key: 'main-red', kind: 'fault', person, source: mission.steps[0]!.work, requester: builder, mission,
    title: 'main is red after the dependency bump', detail: 'option-schema 4.1 rejects an omitted limit. Bisect first; preserve the existing API default rather than reverting the bump.',
    priority: 'high', requestedMs: -30_000, actions: ['custom.reply'],
  })
  const resolved: Attention = { ...card, revision: 'a2', state: 'resolved', updated_at: ctx.t.at(30_000), detail: 'Bisect isolated option-schema 4.1. The default is explicit now; CI retry #2 passed typecheck, all 48 tests and build.' }
  const attention: Slice<'attention'> = {
    kind: 'attention', variant: 'default', source, decode: 'strict', loading: false,
    state: { attention: [card], messages: [] },
    timeline: [{ _tag: 'changes', at_ms: 30_000, store: 0, upserts: [resolved, resources.message(ctx, {
      key: 'ci-green', from: builder.id, to: person.id, title: 'CI is green again',
      content: 'Retry #2 passed with option-schema 4.1 retained. The only source change is the explicit limit default.', sentMs: 30_000,
    })], removes: [] }],
  }
  const c = child(ctx, 'conversation')
  const cursor = thread(builder)
  const history = turn(c, cursor, {
    atMs: -600_000, from: { _tag: 'person', person },
    text: 'main turned red after the option-schema 4.0 to 4.1 lockfile bump. Find the first bad change and make the smallest compatible fix.',
    steps: [
      { _tag: 'say', text: 'The bump installed cleanly, but parseOptions({}) no longer supplies the omitted limit. I will bisect before changing the schema.' },
      { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command }, outcome: 'error', output: redOutput.join('\n'), durationMs: 600 }) },
    ], stepMs: 2_000,
  })
  const future: TimelineEntry[] = [
    ...turn(c, cursor, {
      atMs: 2_000, from: { _tag: 'person', person }, text: 'Keep the dependency bump; bisect the red range.', stepMs: 2_000,
      steps: [
        { _tag: 'say', text: 'Testing the three candidates between the last green main and the red lockfile bump.' },
        { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command: 'git bisect run pnpm --filter @atlas/api test' }, outcome: 'ok', durationMs: 600,
          output: 'candidate 1: refactor request logging — good\ncandidate 2: update API fixtures — good\ncandidate 3: bump option-schema 4.0 to 4.1 — bad\nfirst bad change: option-schema now validates undefined before applying defaults' }) },
        { _tag: 'say', text: 'The dependency bump is the first bad change. Make our omitted-value default explicit at the schema boundary; leave the dependency and all callers alone.' },
      ], status: 'completed',
    }),
    ...diff(c, cursor, { atMs: 14_000, file: 'packages/api/src/options.ts', before: 'limit: number().optional(),', after: 'limit: number().default(20),' }),
    ...toolCall(c, cursor, { atMs: 20_000, name: 'shell', arguments: { command: 'pnpm -r typecheck && pnpm --filter @atlas/api test && pnpm -r build' }, outcome: 'ok', durationMs: 8_000, output: greenOutput.join('\n') }),
    ...turn(c, cursor, { atMs: 29_000, from: { _tag: 'person', person }, text: 'Is the retry green without a rollback?', stepMs: 500,
      steps: [{ _tag: 'say', text: 'Yes. Retry #2 passed all three CI jobs and all 48 tests. option-schema 4.1 remains installed; the patch changes exactly one line.' }], status: 'completed' }),
  ]
  const conversation: Slice<'conversation'> = {
    kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false,
    state: { threads: [{ agent: builder.id, session_id: builder.session, items: history, page_size: 50, has_more: false }] },
    timeline: future.map((item) => ({ _tag: 'entries', at_ms: parseTimestamp(item.timestamp) - ctx.t.now, store: 0, agent: builder.id, items: [item] })),
  }
  const red = terminalRun(child(ctx, 'terminal/red'), builder, { startedAtMs: -30_000, command, lines: redOutput.map((line) => `\u001b[31m${line}\u001b[0m`) })
  const green = terminalRun(child(ctx, 'terminal/green'), builder, { startedAtMs: 20_000, command: 'pnpm -r typecheck && pnpm --filter @atlas/api test && pnpm -r build', lines: greenOutput.map((line) => `\u001b[32m${line}\u001b[0m`) })
  // The first run already left a prompt; append the next command rather than a second prompt.
  const cast = { ...red.cast, events: [...red.cast.events, ...green.cast.events.map(([seconds, kind, data], index): [number, 'o', string] => [seconds + 50, kind, index === 0 ? data.slice(data.indexOf('$ ') + 2) : data])] }
  const record = terminalRecord(ctx, builder, { cast, screens: red.screens }, -30_000)
  const terminal: Slice<'terminal'> = {
    kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false,
    state: { terminals: [record] },
    timeline: green.screens.map(({ at_ms }) => ({ _tag: 'screen', at_ms, store: 0, terminal: builder.terminal,
      screen: screenAt(cast, (at_ms + 30_000) / 1_000, { terminalId: builder.terminal, incarnation: builder.terminalIncarnation }),
    })),
  }
  return { roster, details, attention, conversation, terminal, sync: { ...liveSync(ctx, { conversation }), variant: 'default', source } }
}
