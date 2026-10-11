import type { TimelineEntry, Work } from '@smalltalk/st3-client'

import type { Asciicast } from '../kit/asciicast.ts'
import { child, type FactoryContext } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { diff } from '../kit/factories/diff.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { toolCall } from '../kit/factories/toolCall.ts'
import { entry, thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import { screenAt } from '../kit/screen.ts'
import type { Slice } from '../kit/slice.ts'
import { parseTimestamp } from '../kit/time.ts'
import { liveSync } from '../kit/variants.ts'
import type { Slices, WorldDefinition } from '../kit/world.ts'

const SEED = 22
const START_MS = -40 * 60_000
const file = 'packages/web/test/searchClient.test.ts'
const before = 'client = createClient(sharedController.signal);'
const after = 'client = createClient(new AbortController().signal);'
type HypothesisKey = 'network' | 'timers' | 'cache'
interface Hypothesis {
  readonly title: string
  readonly command: string
  readonly evidence: string
  readonly rejection: string
}
const hypotheses: Record<HypothesisKey, Hypothesis> = {
  network: {
    title: 'Wrong hypothesis 1: network retry latency',
    command: 'pnpm --filter @atlas/web test searchClient -- --network=stub --sequence.shuffle',
    evidence: 'All responses come from the in-memory transport; failing seeds make zero network requests.',
    rejection: 'Rejected hypothesis 1: the same AbortError occurs with a stubbed network. Increasing the retry timeout changes nothing.',
  },
  timers: {
    title: 'Wrong hypothesis 2: fake timers leave a pending microtask',
    command: 'pnpm --filter @atlas/web test searchClient -- --timers=real --sequence.shuffle',
    evidence: 'The test still fails with real timers and a drained microtask queue; no timer remains pending.',
    rejection: 'Rejected hypothesis 2: real timers fail with the same seeds. The abort is already set before the search starts, not during a timer callback.',
  },
  cache: {
    title: 'Wrong hypothesis 3: stale search cache',
    command: 'pnpm --filter @atlas/web test searchClient -- --cache=off --sequence.shuffle',
    evidence: 'A new empty cache for every trial still fails when cancellation runs before search.',
    rejection: 'Rejected hypothesis 3: cache disabled, same failure. Test ordering, not cached results, controls the AbortError.',
  },
}
const phaseKeys: readonly HypothesisKey[] = ['network', 'timers', 'cache']

/** Forty minutes of evidence, three discarded explanations, one isolated test signal. */
export const longDebugRabbitHole: WorldDefinition = {
  id: 'long-debug-rabbit-hole',
  title: 'Long debug rabbit hole',
  narrative: 'A forty-minute hunt through network retries, fake timers and cache state finds an order-dependent shared AbortController. Hundreds of terminal lines and multiple conversation pages end in a one-line fixture fix.',
  seed: SEED,
  cast: {
    project: 'atlas', roles: ['builder'], hosts: 1, people: 1,
    missions: [{ slug: 'search-test-flake', title: 'Find the order-dependent search test flake', steps: [
      'Reproduce and reject the three hypotheses', 'Isolate the test abort signal', 'Verify shuffled test orders',
    ] }],
  },
  slices: (ctx) => build(ctx),
}

const build = (ctx: FactoryContext): Slices => {
  const builder = ctx.cast.agents[0]!
  const person = ctx.cast.people[0]!
  const mission = ctx.cast.missions[0]!
  const source = { _tag: 'synthetic' as const, seed: SEED }
  const initial = agent(child(ctx, 'builder/debugging'), builder, {
    state: 'running', sinceMs: START_MS, lastActivityMs: -8_000, harnessState: 'busy', mission, step: 1, workState: 'claimed', upcoming: [2],
  })
  const verifying = agent(child(ctx, 'builder/verifying'), builder, {
    state: 'running', sinceMs: 6_000, harnessState: 'busy', mission, step: 2, workState: 'verifying',
  })
  const finished = agent(child(ctx, 'builder/finished'), builder, { state: 'waiting', sinceMs: 22_000, harnessState: 'idle', mission })
  const roster: Slice<'roster'> = {
    kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
    state: { agents: [initial.agent], runtimes: [initial.runtime!], order: [builder.id],
      machines: [resources.machine(ctx, builder.host, [builder.runtime, builder.terminalRuntime], [mission.steps[1]!.work])] },
    timeline: [
      { _tag: 'changes', at_ms: 6_000, store: 0, upserts: [verifying.agent, verifying.runtime!], removes: [] },
      { _tag: 'changes', at_ms: 22_000, store: 0, upserts: [finished.agent, finished.runtime!, { ...resources.machine(ctx, builder.host, [builder.runtime, builder.terminalRuntime], []), revision: 'mh-verified', updated_at: ctx.t.at(22_000) }], removes: [] },
    ],
  }
  const work = (step: number, state: Work['state'], updatedMs: number, revision: string): Work => ({
    ...resources.work(ctx, { mission, step, state, updatedMs, claimant: builder, goals: ['No AbortError in 100 shuffled orders', 'Change only the test fixture, not production retry behavior'] }),
    revision, constraints: ['Keep the patch to one line in the test fixture'],
  })
  const details: Slice<'details'> = {
    kind: 'details', variant: 'default', source, decode: 'strict', loading: false,
    state: { missions: [resources.mission(ctx, mission, 'running', -8_000)],
      work: [work(0, 'completed', -8_000, 'w-hypotheses-rejected'), work(1, 'claimed', -8_000, 'w-fix'), work(2, 'ready', -8_000, 'w-repeat-ready')] },
    timeline: [
      { _tag: 'changes', at_ms: 6_000, store: 0, upserts: [work(1, 'completed', 6_000, 'w-isolated'), work(2, 'verifying', 6_000, 'w-shuffled')], removes: [] },
      { _tag: 'changes', at_ms: 22_000, store: 0, upserts: [work(2, 'completed', 22_000, 'w-repeat-green'), { ...resources.mission(ctx, mission, 'completed', 22_000), revision: 'mr-complete' }], removes: [] },
    ],
  }
  const card = resources.attention(ctx, {
    key: 'search-flake', kind: 'fault', person, source: mission.steps[0]!.work, requester: builder, mission,
    title: 'Search test fails only in some shuffled orders', detail: 'Single-test runs pass, but the suite intermittently raises AbortError. Preserve the evidence from all three discarded hypotheses.',
    priority: 'normal', requestedMs: START_MS, actions: ['custom.reply'],
  })
  const attention: Slice<'attention'> = {
    kind: 'attention', variant: 'default', source, decode: 'strict', loading: false,
    state: { attention: [card], messages: [] },
    timeline: [{ _tag: 'changes', at_ms: 22_000, store: 0, upserts: [
      { ...card, revision: 'a2', state: 'resolved', updated_at: ctx.t.at(22_000), detail: 'One-line test-fixture isolation fixed the shared aborted signal. All 100 shuffled orders passed; production code is unchanged.' },
      resources.message(ctx, { key: 'flake-fixed', from: builder.id, to: person.id, title: 'Flake fixed without changing retry behavior', content: 'The cancellation test poisoned a module-shared AbortController. Each test now creates its own signal; all 100 shuffled orders pass.', sentMs: 22_000 }),
    ], removes: [] }],
  }
  const c = child(ctx, 'conversation')
  const cursor = thread(builder)
  const history: TimelineEntry[] = []
  const events: [number, 'o', string][] = []
  for (const [phaseIndex, key] of phaseKeys.entries()) {
    const hypothesis = hypotheses[key]
    for (let batch = 0; batch < 8; batch += 1) {
      const index = phaseIndex * 8 + batch
      const atMs = START_MS + index * 100_000
      const trials = Array.from({ length: 12 }, (_, trial) => {
        const seed = batch * 12 + trial + 1
        const fails = seed % 7 === 0 || seed % 11 === 0
        return `seed ${String(seed).padStart(3, '0')}: ${fails ? 'FAIL search after cancellation: AbortError, signal.aborted=true' : 'PASS search before cancellation: 12 assertions'}`
      })
      const failed = trials.filter((line) => line.includes('FAIL')).length
      const output = [...trials, `batch ${batch + 1}: ${12 - failed} passed, ${failed} failed`, hypothesis.evidence].join('\n')
      history.push(...turn(c, cursor, {
        atMs, from: { _tag: 'person', person },
        text: batch === 0 ? `Investigate ${hypothesis.title.toLowerCase()}. Keep the failing shuffle seeds.` : `Run the next shuffled batch for ${key}; compare it with batch ${batch}.`,
        steps: [
          { _tag: 'say', text: batch === 0 ? hypothesis.title : `Batch ${batch + 1}/8: preserve the seed and whether cancellation precedes search.` },
          { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command: `${hypothesis.command} --seed-start=${batch * 12 + 1} --repeat=12` }, outcome: 'error', output, durationMs: 1_000 }) },
          { _tag: 'say', text: batch === 7 ? hypothesis.rejection : `${failed} of 12 orders still fail. ${hypothesis.evidence} Next batch narrows the ordering evidence; this is not a green rerun.` },
        ], stepMs: 2_000, status: 'waiting',
      }))
      events.push([(atMs - START_MS) / 1_000, 'o', `atlas:builder $ ${hypothesis.command} --seed-start=${batch * 12 + 1} --repeat=12\r\n`])
      events.push([(atMs + 5_000 - START_MS) / 1_000, 'o', `${output.replace(/\n/g, '\r\n')}\r\n${batch === 7 ? hypothesis.rejection + '\r\n' : ''}`])
    }
  }
  history.push(entry(c, cursor, {
    atMs: -8_000, role: 'assistant', type: 'content', body: { media_type: 'text/markdown',
      text: 'Forty minutes of evidence: network, fake timers and cache are ruled out. The cancellation test aborts sharedController; shuffled search tests inherit its already-aborted signal. A fresh AbortController in beforeEach is the one-line fix.' },
  }))
  const future: TimelineEntry[] = [
    ...diff(c, cursor, { atMs: 4_000, file, before, after }),
    ...toolCall(c, cursor, { atMs: 12_000, name: 'shell', arguments: { command: 'pnpm --filter @atlas/web test searchClient -- --sequence.shuffle --repeat=100' }, outcome: 'ok', durationMs: 8_000,
      output: '100 shuffled orders passed; 0 AbortError failures\nCancellation still aborts its own request; search uses a fresh signal.\n1 file changed, 1 insertion(+), 1 deletion(-)' }),
    entry(c, cursor, { atMs: 22_000, role: 'assistant', type: 'content', body: { media_type: 'text/markdown', text: 'Fixed: each test gets a fresh abort signal. All 100 shuffled orders pass, cancellation coverage is intact, and no production code changed.' } }),
    entry(c, cursor, { atMs: 22_000, role: 'system', type: 'status', body: { status: 'completed' } }),
  ]
  const pageSize = 25
  const conversation: Slice<'conversation'> = {
    kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false,
    state: { threads: [{ agent: builder.id, session_id: builder.session, items: history, page_size: pageSize, has_more: history.length > pageSize }] },
    timeline: future.map((item) => ({ _tag: 'entries', at_ms: parseTimestamp(item.timestamp) - ctx.t.now, store: 0, agent: builder.id, items: [item] })),
  }
  events.push([(-8_000 - START_MS) / 1_000, 'o', 'Diagnosis: cancellation poisoned sharedController; fresh signal required per test.\r\n'])
  events.push([(4_300 - START_MS) / 1_000, 'o', `atlas:builder $ git diff --stat\r\n${file} | 2 +-\r\n1 file changed, 1 insertion(+), 1 deletion(-)\r\n`])
  events.push([(12_000 - START_MS) / 1_000, 'o', 'atlas:builder $ pnpm --filter @atlas/web test searchClient -- --sequence.shuffle --repeat=100\r\n'])
  events.push([(20_000 - START_MS) / 1_000, 'o', '\u001b[32m100 shuffled orders passed; 0 AbortError failures\u001b[0m\r\nCancellation coverage intact; no production changes.\r\natlas:builder $ '])
  const cast: Asciicast = { header: { version: 2, width: 110, height: 16, title: 'Forty-minute search test investigation' }, started_at_ms: START_MS, events }
  // Preserve every output byte in the cast, but sample screens at hypothesis boundaries rather
  // than copying the viewport once for every historical line.
  const screenOffsets = [-1_695_000, -895_000, -95_000, -8_000, 4_300, 12_000, 20_000]
  const screens = screenOffsets.map((at_ms) => ({ at_ms, screen: screenAt(cast, (at_ms - START_MS) / 1_000, { terminalId: builder.terminal, incarnation: builder.terminalIncarnation }) }))
  const record = terminalRecord(ctx, builder, { cast, screens }, START_MS)
  const terminal: Slice<'terminal'> = {
    kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false,
    state: { terminals: [record] },
    timeline: screens.filter(({ at_ms }) => at_ms > 0).map(({ at_ms, screen }) => ({ _tag: 'screen', at_ms, store: 0, terminal: builder.terminal, screen })),
  }
  return { roster, details, attention, conversation, terminal, sync: { ...liveSync(ctx, { conversation }), variant: 'default', source } }
}
