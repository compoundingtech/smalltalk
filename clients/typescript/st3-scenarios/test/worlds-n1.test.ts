import { St3Client } from '@smalltalk/st3-client'
import { Envelope, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'

import { decodeSlice } from '../scripts/decode.ts'
import { sliceFile } from '../scripts/emit.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { SLICE_KINDS, type AnySlice, type SliceKind } from '../src/kit/slice.ts'
import { ANCHOR_MS, parseTimestamp } from '../src/kit/time.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { buildWorld, type World, type WorldDefinition } from '../src/kit/world.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { ciFailingFix } from '../src/worlds/ciFailingFix.ts'
import { longDebugRabbitHole } from '../src/worlds/longDebugRabbitHole.ts'

const definitions = [ciFailingFix, longDebugRabbitHole]
const screenText = (world: World, atMs: number) => foldSlice(world.slices.terminal, atMs).state.terminals[0]!.screens.at(-1)!.screen.lines.map(({ text }) => text).join('\n')
const storyCi = (world: World) => {
  const member = world.cast.agents[0]!
  const mission = world.cast.missions[0]!
  const before = world.slices.details.state.work.find(({ id }) => id === mission.steps[0]!.work)!
  const after = foldSlice(world.slices.details, 30_000).state
  expect(before.state).toBe('failed')
  expect(after.work.find(({ id }) => id === before.id)).toMatchObject({ state: 'completed', attempt: 2 })
  expect(after.work.every(({ state }) => state === 'completed')).toBe(true)
  expect(after.missions[0]!.state).toBe('completed')
  expect(world.slices.roster.state.agents[0]!.current_work?.[0]).toMatchObject({ id: mission.steps[1]!.stepRun, state: 'claimed' })
  expect(foldSlice(world.slices.roster, 18_000).state.agents[0]!.current_work?.[0]).toMatchObject({ id: mission.steps[3]!.stepRun, state: 'verifying' })
  expect(foldSlice(world.slices.roster, 30_000).state.agents[0]).toMatchObject({ state: 'waiting', harness_state: 'idle', active_work_count: 0 })
  expect(world.slices.attention.state.attention[0]).toMatchObject({ state: 'open', source_id: before.id, requester_id: member.id, mission_id: mission.id })
  expect(foldSlice(world.slices.attention, 30_000).state.attention[0]!.state).toBe('resolved')
  const conversation = JSON.stringify(foldSlice(world.slices.conversation, 30_000).state)
  expect(conversation).toContain('bump option-schema 4.0 to 4.1')
  expect(conversation).toContain('candidate 1:')
  expect(conversation).toContain('candidate 2:')
  expect(conversation).toContain('candidate 3:')
  expect(conversation).toContain('limit: number().default(20),')
  expect(conversation).toContain('48 passed, 0 failed')
  expect(screenText(world, 0)).toContain('1 failed, 47 passed')
  expect(screenText(world, 30_000)).toContain('48 passed, 0 failed')
  expect(screenText(world, 30_000)).not.toContain('atlas:builder $ atlas:builder $')
  const initialScreen = world.slices.terminal.state.terminals[0]!.screens.at(-1)!.screen
  const finalScreen = foldSlice(world.slices.terminal, 30_000).state.terminals[0]!.screens.at(-1)!.screen
  expect(initialScreen.lines.flatMap(({ runs }) => runs).some(({ fg }) => fg === 1)).toBe(true)
  expect(finalScreen.lines.flatMap(({ runs }) => runs).some(({ fg }) => fg === 2)).toBe(true)
}
const storyLong = (world: World) => {
  const thread = world.slices.conversation.state.threads[0]!
  const history = JSON.stringify(thread.items)
  expect(parseTimestamp(thread.items[0]!.timestamp) - world.now).toBe(-40 * 60_000)
  expect(thread.items.length).toBeGreaterThan(150)
  expect(thread.has_more).toBe(true)
  expect(Math.ceil(thread.items.length / thread.page_size)).toBeGreaterThan(5)
  for (const index of [1, 2, 3]) {
    expect(history).toContain(`Wrong hypothesis ${index}:`)
    expect(history).toContain(`Rejected hypothesis ${index}:`)
  }
  expect(history).toContain('PASS search before cancellation')
  expect(history).toContain('FAIL search after cancellation')
  expect(history).toContain('sharedController')
  const fixed = foldSlice(world.slices.conversation, 22_000).state.threads[0]!
  const edit = fixed.items.find((item) => item.type === 'tool_call' && item.body.name === 'edit')
  expect(edit?.type === 'tool_call' ? edit.body.arguments : undefined).toEqual({ path: 'packages/web/test/searchClient.test.ts', old_string: 'client = createClient(sharedController.signal);', new_string: 'client = createClient(new AbortController().signal);' })
  expect(JSON.stringify(fixed.items)).toContain('100 shuffled orders passed; 0 AbortError failures')
  const terminal = world.slices.terminal.state.terminals[0]!
  const scrollback = terminal.cast.events.map(([, , text]) => text).join('')
  expect(scrollback.split('\r\n').length).toBeGreaterThan(350)
  expect(terminal.screens.length).toBeLessThan(10)
  expect(terminal.screens.at(-1)!.screen.truncated).toBe(true)
  expect(screenText(world, 22_000)).toContain('100 shuffled orders passed')
  expect(foldSlice(world.slices.details, 22_000).state.work.every(({ state }) => state === 'completed')).toBe(true)
  expect(foldSlice(world.slices.attention, 22_000).state.attention[0]!.state).toBe('resolved')
  expect(foldSlice(world.slices.roster, 22_000).state.agents[0]!.active_work_count).toBe(0)
}

const verifySlices = (definition: WorldDefinition, now: number): World => {
  const world = buildWorld(definition, genericVariants, now)
  for (const kind of SLICE_KINDS) {
    const slice = world.slices[kind] as AnySlice
    expect(slice.decode).toBe('strict')
    expect(decodeSlice(world.id, slice), `${world.id}/${kind}/anchor`).toEqual([])
    let previous = 0
    for (const event of slice.timeline) {
      expect(event.at_ms).toBeGreaterThan(0)
      expect(event.at_ms).toBeGreaterThanOrEqual(previous)
      previous = event.at_ms
      expect(decodeSlice(world.id, foldSlice(slice, event.at_ms) as AnySlice), `${world.id}/${kind}/${event.at_ms}`).toEqual([])
    }
  }
  const owner = world.cast.agents[0]!
  expect(world.slices.conversation.state.threads[0]).toMatchObject({ agent: owner.id, session_id: owner.session })
  expect(world.slices.terminal.state.terminals[0]).toMatchObject({ owner: owner.id, terminal: owner.terminal, incarnation: owner.terminalIncarnation })
  expect(world.slices.roster.state.agents[0]!.id).toBe(owner.id)
  return world
}

describe('red and long narrative worlds', () => {
  for (const definition of definitions) {
    it(`${definition.id}: strict-decodes and folds every event at anchor and shifted now`, () => {
      for (const now of [ANCHOR_MS, ANCHOR_MS + 123_456_789]) {
        const world = verifySlices(definition, now)
        if (world.id === ciFailingFix.id) storyCi(world)
        else storyLong(world)
      }
    })
    it(`${definition.id}: remains deterministic and under a 1 MB budget per slice`, () => {
      const world = buildWorld(definition, genericVariants, ANCHOR_MS)
      expect(world.slices).toEqual(buildWorld(definition, genericVariants, ANCHOR_MS).slices)
      const sliceBytes: Partial<Record<SliceKind, number>> = {}
      const fixtureBytes: Partial<Record<SliceKind, number>> = {}
      for (const kind of SLICE_KINDS) {
        const slice = world.slices[kind] as AnySlice
        const bytes = new TextEncoder().encode(JSON.stringify(slice)).byteLength
        const fileBytes = new TextEncoder().encode(JSON.stringify(sliceFile(world, slice), null, 2) + '\n').byteLength
        sliceBytes[kind] = bytes
        fixtureBytes[kind] = fileBytes
        expect(bytes).toBeLessThan(1_000_000)
        expect(fileBytes).toBeLessThan(1_000_000)
      }
      console.info(`${definition.id} slice bytes: ${JSON.stringify(sliceBytes)}; fixture bytes: ${JSON.stringify(fixtureBytes)}`)
    })
  }

  it('ci-failing-fix: catches a planted missing green terminal screen', () => {
    const world = buildWorld(ciFailingFix, genericVariants, ANCHOR_MS)
    expect(() => storyCi(world.with({ terminal: { ...world.slices.terminal, timeline: [] } }))).toThrow()
  })

  it('long-debug-rabbit-hole: catches a planted edit that retains the shared signal', () => {
    const world = buildWorld(longDebugRabbitHole, genericVariants, ANCHOR_MS)
    const conversation = world.slices.conversation
    const broken = { ...conversation, timeline: conversation.timeline.map((event) => event._tag !== 'entries' ? event : {
      ...event, items: event.items.map((item) => item.type !== 'tool_call' || item.body.name !== 'edit' ? item : {
        ...item, body: { ...item.body, arguments: { path: 'packages/web/test/searchClient.test.ts', old_string: 'client = createClient(sharedController.signal);', new_string: 'client = createClient(sharedController.signal);' } },
      }),
    }) }
    expect(() => storyLong(world.with({ conversation: broken }))).toThrow()
  })

  it('long-debug-rabbit-hole: delivers every older page through the real client', async () => {
    const world = buildWorld(longDebugRabbitHole, genericVariants, ANCHOR_MS)
    const replay = createReplay(world, { clock: manualClock(world.now) })
    const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl: async (input, init) => {
      const response = await replay.fetch(input, init)
      decodeUnknownSync(Envelope, 'strict')(await response.clone().json())
      return response
    } })
    const thread = world.slices.conversation.state.threads[0]!
    const pages: string[][] = []
    let cursor: string | undefined
    try {
      do {
        const { value } = await client.timelineList(thread.session_id, { limit: thread.page_size, ...(cursor === undefined ? {} : { cursor }) })
        expect(value.items.length).toBeLessThanOrEqual(thread.page_size)
        pages.unshift(value.items.map(({ id }) => id))
        cursor = value.page.has_more ? value.page.next_cursor ?? undefined : undefined
        expect(pages.length).toBeLessThan(30)
      } while (cursor !== undefined)
      expect(pages.length).toBeGreaterThan(5)
      expect(pages.flat()).toEqual(thread.items.map(({ id }) => id))
      expect(new Set(pages.flat()).size).toBe(thread.items.length)
    } finally { replay.close() }
  })
})
