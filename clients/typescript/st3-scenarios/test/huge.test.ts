import { ClientError, St3Client, type TimelineEntry } from '@smalltalk/st3-client'
import { afterAll, describe, expect, it } from 'vitest'

import { decodeSlice } from '../scripts/decode.ts'
import { canonicalJson, sliceFile } from '../scripts/emit.ts'
import {
  ENTRIES_PER_HISTORY_TURN, generateConversationRange, HUGE_COMMITTED_TURNS, HUGE_TURNS,
} from '../src/kit/conversationHistory.ts'
import type { FactoryContext } from '../src/kit/context.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { fork, rngFromSeed } from '../src/kit/rng.ts'
import { SLICE_KINDS, type SliceKind, type TimelineEvent } from '../src/kit/slice.ts'
import { ANCHOR, ANCHOR_MS, parseTimestamp, rebase, timeContext } from '../src/kit/time.ts'
import { catalog, loadWorld } from '../src/index.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'

const SHIFT_MS = 123_456_789
const TOTAL = HUGE_TURNS * ENTRIES_PER_HISTORY_TURN
const COMMITTED = HUGE_COMMITTED_TURNS * ENTRIES_PER_HISTORY_TURN
const FIRST_COMMITTED = TOTAL - COMMITTED + 1
// The whole file shares one generated world; every test below only folds or pages it.
const world = loadWorld('huge', { now: ANCHOR_MS })
const roster = world.slices.roster.state
const details = world.slices.details.state
const thread = world.slices.conversation.state.threads[0]!
const history = thread.history!
const member = world.cast.agents.find(({ id }) => id === thread.agent)!
const generatorCtx: FactoryContext = { world: history.world, rng: rngFromSeed(history.seed), cast: world.cast, t: timeContext(ANCHOR_MS) }
const range = (firstSequence: number, lastSequence: number, seed = history.seed): TimelineEntry[] =>
  generateConversationRange(generatorCtx, member, { ...history, seed }, firstSequence, lastSequence)

describe('huge fleet', () => {
  it('catalogs huge immediately before unicode', () => {
    const ids = catalog.map(({ id }) => id)
    expect(ids.indexOf('huge')).toBe(ids.indexOf('unicode') - 1)
    expect(world.title).toBe('Huge fleet and conversation')
  })

  it('commits one thousand unique agents across six hosts with joined runtimes and order', () => {
    expect(roster.agents).toHaveLength(1_000)
    expect(new Set(roster.agents.map(({ id }) => id)).size).toBe(1_000)
    expect(roster.order).toEqual(roster.agents.map(({ id }) => id))
    expect(world.cast.agents).toHaveLength(1_000)
    expect(new Set(world.cast.agents.map(({ key }) => key)).size).toBe(1_000)
    for (const field of ['id', 'session', 'terminal', 'runtime', 'incarnation'] as const) {
      expect(new Set(world.cast.agents.map((agent) => agent[field])).size, field).toBe(1_000)
    }
    expect(world.cast.hosts).toHaveLength(6)
    const hostIds = new Set(world.cast.hosts.map(({ id }) => id))
    expect(roster.agents.every(({ host_id }) => typeof host_id === 'string' && hostIds.has(host_id))).toBe(true)
    expect(new Set(roster.agents.map(({ host_id }) => host_id)).size).toBe(6)
    expect(roster.machines.map(({ id }) => id)).toEqual(world.cast.hosts.map(({ machine }) => machine))
    expect(roster.runtimes).toHaveLength(1_000)
    expect(new Set(roster.runtimes.map(({ id }) => id)).size).toBe(1_000)
    const runtimeIds = new Set(roster.runtimes.map(({ id }) => id))
    for (const agent of roster.agents) {
      expect(agent.runtime_ids.length, agent.id).toBeLessThanOrEqual(1)
      for (const id of agent.runtime_ids) expect(runtimeIds.has(id), `${agent.id} runtime ${id}`).toBe(true)
    }
    expect(roster.runtimes.every(({ owner_id, owner_host_id }) => hostIds.has(owner_host_id) && roster.agents.some(({ id }) => id === owner_id))).toBe(true)
    const states = roster.agents.reduce<Record<string, number>>((counts, { state }) => ({ ...counts, [state]: (counts[state] ?? 0) + 1 }), {})
    expect(states).toEqual({ waiting: 200, running: 800 })
    expect(decodeSlice(world.id, world.slices.roster)).toEqual([])
  })

  it('joins every claimed work item to one agent and one of twenty fifty-step missions', () => {
    expect(details.missions).toHaveLength(20)
    expect(new Set(details.missions.map(({ id }) => id)).size).toBe(20)
    expect(details.missions.every(({ state }) => state === 'running')).toBe(true)
    expect(details.work).toHaveLength(1_000)
    expect(new Set(details.work.map(({ id }) => id)).size).toBe(1_000)
    const missionIds = new Set(details.missions.map(({ id }) => id))
    expect(details.work.every(({ mission_id }) => typeof mission_id === 'string' && missionIds.has(mission_id))).toBe(true)
    const perMission = new Map<string, number>()
    for (const { mission_id } of details.work) {
      if (typeof mission_id !== 'string') throw new Error('huge work requires a mission')
      perMission.set(mission_id, (perMission.get(mission_id) ?? 0) + 1)
    }
    expect([...perMission.values()].every((count) => count === 50)).toBe(true)
    expect(details.work.every(({ state }) => state === 'claimed')).toBe(true)
    const workByClaimant = new Map(details.work.map((item) => [item.claimant!, item]))
    expect(workByClaimant.size).toBe(1_000)
    expect(new Set(workByClaimant.keys())).toEqual(new Set(roster.agents.map(({ id }) => id)))
    for (const [index, agent] of roster.agents.entries()) {
      const mission = world.cast.missions[index % world.cast.missions.length]!
      const step = Math.floor(index / world.cast.missions.length) % mission.steps.length
      const work = workByClaimant.get(agent.id)!
      expect(agent.current_work_ids, agent.id).toEqual([mission.steps[step]!.stepRun])
      expect(agent.current_work?.[0]).toMatchObject({ id: mission.steps[step]!.stepRun, mission_id: mission.id, state: 'claimed' })
      expect(work.id).toBe(mission.steps[step]!.work)
      expect(work.mission_id).toBe(mission.id)
      expect(work.claimant).toBe(agent.id)
      expect(work.claim_incarnation).toBe(agent.incarnation_id)
    }
    expect(decodeSlice(world.id, world.slices.details)).toEqual([])
  })
})

describe('huge conversation window', () => {
  it('carries exact seeded-turns metadata and a contiguous committed window', () => {
    expect(ENTRIES_PER_HISTORY_TURN).toBe(4)
    expect(HUGE_TURNS).toBe(10_000)
    expect(HUGE_COMMITTED_TURNS).toBe(400)
    expect(world.slices.conversation.state.threads).toHaveLength(1)
    expect(thread.agent).toBe(world.cast.agents[0]!.id)
    expect(thread.session_id).toBe(member.session)
    expect(history).toEqual({
      kind: 'seeded-turns',
      seed: world.seed,
      world: 'huge',
      total_turns: HUGE_TURNS,
      total_entries: TOTAL,
      committed_from_sequence: FIRST_COMMITTED,
      next_cursor: `scenario-cursor/${COMMITTED}`,
    })
    expect(thread.page_size).toBe(50)
    expect(thread.has_more).toBe(true)
    expect(thread.items).toHaveLength(COMMITTED)
    expect(thread.items[0]!.sequence).toBe(FIRST_COMMITTED)
    expect(thread.items.at(-1)!.sequence).toBe(TOTAL)
    for (let index = 1; index < thread.items.length; index += 1) {
      expect(thread.items[index]!.sequence, `sequence at ${index}`).toBe(thread.items[index - 1]!.sequence + 1)
    }
    const instants = thread.items.map(({ timestamp }) => parseTimestamp(timestamp))
    expect(instants).toEqual([...instants].sort((a, b) => a - b))
    expect(instants.every((instant) => instant <= ANCHOR_MS)).toBe(true)
    expect(instants[0]).toBe(ANCHOR_MS - HUGE_COMMITTED_TURNS * 30_000)
    for (let exchange = 0; exchange < HUGE_COMMITTED_TURNS; exchange += 1) {
      const block = thread.items.slice(exchange * ENTRIES_PER_HISTORY_TURN, (exchange + 1) * ENTRIES_PER_HISTORY_TURN)
      expect(block.map(({ role, type }) => `${role}/${type}`)).toEqual(['user/message', 'user/content', 'assistant/content', 'system/status'])
      expect(block[0]!.body).toMatchObject({ from: world.cast.people[0]!.id, to: member.id })
      expect(block[3]!.body).toMatchObject({ status: 'completed' })
    }
  })

  it('keeps the newest assistant entry partial and revises it twice after now', () => {
    const partial = thread.items.find(({ sequence }) => sequence === TOTAL - 1)!
    expect(partial.final).toBe(false)
    expect(partial.revision).toBe(1)
    expect(partial.body).toMatchObject({ text: expect.stringContaining('Batch 10000:') })
    const before = foldSlice(world.slices.conversation, 999).state.threads[0]!.items.find(({ sequence }) => sequence === TOTAL - 1)!
    expect(before).toEqual(partial)
    const second = foldSlice(world.slices.conversation, 1_000).state.threads[0]!.items.find(({ sequence }) => sequence === TOTAL - 1)!
    expect(second).toMatchObject({ id: partial.id, sequence: TOTAL - 1, revision: 2, final: false })
    expect(second.body).toMatchObject({ text: 'The final caller batch is validated; I am checking its migration handoff.' })
    const final = foldSlice(world.slices.conversation, 2_000).state.threads[0]!.items.find(({ sequence }) => sequence === TOTAL - 1)!
    expect(final).toMatchObject({ id: partial.id, sequence: TOTAL - 1, revision: 3, final: true })
    expect(final.body).toMatchObject({ text: 'The caller tests and migration handoff are checked. The fleet can continue with the next group.' })
    expect(world.slices.conversation.timeline).toEqual([
      expect.objectContaining({ _tag: 'entries', at_ms: 1_000, agent: thread.agent, items: [second] }),
      expect.objectContaining({ _tag: 'entries', at_ms: 2_000, agent: thread.agent, items: [final] }),
    ])
    expect(decodeSlice(world.id, world.slices.conversation)).toEqual([])
  })
})

describe('seeded history generator', () => {
  it('regenerates the committed window exactly except the live partial entry', () => {
    const regenerated = range(FIRST_COMMITTED, TOTAL)
    expect(regenerated).toHaveLength(COMMITTED)
    for (const [index, entry] of thread.items.entries()) {
      const candidate = regenerated[index]!
      if (entry.sequence === TOTAL - 1) {
        expect(candidate.id).toBe(entry.id)
        expect(candidate.sequence).toBe(entry.sequence)
        expect(candidate.revision).toBe(entry.revision)
        expect(candidate.timestamp).toBe(entry.timestamp)
        expect(candidate.final).toBe(true)
        expect(entry.final).toBe(false)
      } else expect(candidate).toEqual(entry)
    }
  })

  it('seeds each turn independently of the caller generator, window edges and call order', () => {
    const aligned = range(12_345, 12_352)
    expect(aligned.map(({ sequence }) => sequence)).toEqual([12_345, 12_346, 12_347, 12_348, 12_349, 12_350, 12_351, 12_352])
    expect(range(12_345, 12_352)).toEqual(aligned)
    // A different caller rng must not move any byte: exchanges derive from the history seed alone.
    const otherCtx: FactoryContext = { ...generatorCtx, rng: fork(generatorCtx.rng, 'a-different-caller') }
    expect(generateConversationRange(otherCtx, member, history, 12_345, 12_352)).toEqual(aligned)
    // Unaligned edges return exactly the entries of the aligned window they cut into.
    expect(range(12_346, 12_349)).toEqual(aligned.filter(({ sequence }) => sequence >= 12_346 && sequence <= 12_349))
    expect(range(0, 10).map(({ sequence }) => sequence)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
    expect(range(TOTAL - 1, TOTAL * 2).map(({ sequence }) => sequence)).toEqual([TOTAL - 1, TOTAL])
    // A different seed is a different history: the assertion has teeth.
    expect(range(12_345, 12_352, history.seed + 1)).not.toEqual(aligned)
  })

  it('serves generated older pages as final revision-one entries strictly before the window', () => {
    const older = range(FIRST_COMMITTED - 200, FIRST_COMMITTED - 1)
    expect(older).toHaveLength(200)
    expect(older[0]!.sequence).toBe(FIRST_COMMITTED - 200)
    expect(older.at(-1)!.sequence).toBe(FIRST_COMMITTED - 1)
    for (const entry of older) {
      expect(entry.revision, `sequence ${entry.sequence}`).toBe(1)
      expect(entry.final, `sequence ${entry.sequence}`).toBe(true)
    }
    const committedIds = new Set(thread.items.map(({ id }) => id))
    expect(older.every(({ id }) => !committedIds.has(id))).toBe(true)
    expect(parseTimestamp(older.at(-1)!.timestamp)).toBeLessThan(parseTimestamp(thread.items[0]!.timestamp))
    const instants = older.map(({ timestamp }) => parseTimestamp(timestamp))
    expect(instants).toEqual([...instants].sort((a, b) => a - b))
    expect(instants.every((instant) => instant <= ANCHOR_MS)).toBe(true)
    const deep = range(12_301, 12_500)
    expect(deep).toHaveLength(200)
    expect(deep.map(({ sequence }) => sequence)[0]).toBe(12_301)
    expect(deep.map(({ sequence }) => sequence).at(-1)).toBe(12_500)
    expect(deep).toEqual(range(12_301, 12_500))
  })

  it('rebuilds identical committed bytes at another now, shifting only instants', () => {
    const shifted = loadWorld('huge', { now: ANCHOR_MS + SHIFT_MS })
    const shiftedThread = shifted.slices.conversation.state.threads[0]!
    expect(shiftedThread.history).toEqual(history)
    expect(shiftedThread.items.map(({ sequence, id }) => ({ sequence, id }))).toEqual(thread.items.map(({ sequence, id }) => ({ sequence, id })))
    const partial = shiftedThread.items.find(({ sequence }) => sequence === TOTAL - 1)!
    expect(partial.final).toBe(false)
    for (const [index, entry] of thread.items.entries()) {
      expect(parseTimestamp(shiftedThread.items[index]!.timestamp) - parseTimestamp(entry.timestamp), `entry ${entry.sequence}`).toBe(SHIFT_MS)
    }
    for (const kind of SLICE_KINDS) {
      const file = sliceFile(world, world.slices[kind])
      const rebased = rebase({ state: file.state, timeline: file.timeline }, file.times, ANCHOR, shifted.now)
      const direct = sliceFile(shifted, shifted.slices[kind])
      expect(rebased, `${kind} full native-file rebase`).toEqual({ state: direct.state, timeline: direct.timeline })
    }
  })
})

/** Sequence problems of a paged walk: every page boundary must continue the previous sequence exactly. */
const continuityProblems = (pages: readonly TimelineEntry[][]): string[] => {
  const flat = pages.flat()
  const problems: string[] = []
  for (let index = 1; index < flat.length; index += 1) {
    const before = flat[index - 1]!
    const after = flat[index]!
    if (after.sequence !== before.sequence + 1) problems.push(`${before.sequence} -> ${after.sequence}`)
  }
  return problems
}

describe('real client paging', () => {
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  let timelineRequests = 0
  const client = new St3Client({
    baseUrl: 'http://scenario.invalid',
    fetchImpl: async (input, init) => {
      const response = await replay.fetch(input, init)
      if (new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url).pathname.endsWith('/timeline')) timelineRequests += 1
      return response
    },
  })

  it('walks the whole committed window in eight pages and stops at the history cursor', async () => {
    const pages: TimelineEntry[][] = []
    const cursors = new Set<string>()
    let cursor: string | undefined
    do {
      const result = (await client.timelineList(thread.session_id, { limit: 200, ...(cursor === undefined ? {} : { cursor }) })).value
      pages.unshift(result.items)
      cursor = result.page.has_more ? result.page.next_cursor ?? undefined : undefined
      expect(result.page.limit).toBe(200)
      if (result.page.has_more) expect(cursor, 'has_more requires a next cursor').toBeDefined()
      if (cursor !== undefined) { expect(cursors.has(cursor), `repeated cursor ${cursor}`).toBe(false); cursors.add(cursor) }
    } while (cursor !== undefined && cursor !== history.next_cursor)
    expect(pages).toHaveLength(8)
    expect(cursor).toBe(history.next_cursor)
    expect(timelineRequests).toBe(8)
    expect(continuityProblems(pages)).toEqual([])
    expect(pages.flat()).toEqual(thread.items)
  })

  it('returns generated older pages across the committed boundary without gaps or duplicates', async () => {
    const boundary = (await client.timelineList(thread.session_id, { limit: 200, cursor: history.next_cursor })).value
    expect(boundary.items).toEqual(range(FIRST_COMMITTED - 200, FIRST_COMMITTED - 1))
    expect(boundary.items).toHaveLength(200)
    expect(boundary.items[0]!.sequence).toBe(FIRST_COMMITTED - 200)
    expect(boundary.items.at(-1)!.sequence).toBe(FIRST_COMMITTED - 1)
    expect(continuityProblems([boundary.items, [thread.items[0]!]])).toEqual([])
    expect(boundary.page.has_more).toBe(true)
    expect(boundary.page.next_cursor).toBe(`scenario-cursor/${COMMITTED + 200}`)
    // A page-size-aligned cursor lands exactly on the committed boundary from below.
    const sharp = (await client.timelineList(thread.session_id, { limit: 50, cursor: `scenario-cursor/${COMMITTED - 50}` })).value
    expect(sharp.items.map(({ sequence }) => sequence)).toEqual(Array.from({ length: 50 }, (_, index) => FIRST_COMMITTED + index))
    expect(sharp.page.next_cursor).toBe(history.next_cursor)
    // Planted-break evidence: the continuity oracle catches a dropped, a duplicated and a misordered page seam.
    expect(continuityProblems([thread.items.slice(0, 200), thread.items.slice(201, 400)])).not.toEqual([])
    expect(continuityProblems([thread.items.slice(0, 200), thread.items.slice(199, 400)])).not.toEqual([])
    expect(continuityProblems([thread.items.slice(0, 200), boundary.items])).not.toEqual([])
  })

  it('serves any deep older range deterministically, byte for byte', async () => {
    const first = (await client.timelineList(thread.session_id, { limit: 200, cursor: 'scenario-cursor/27500' })).value
    const page = (await client.timelineList(thread.session_id, { limit: 200, cursor: 'scenario-cursor/27500' })).value
    // Envelope request IDs advance, but the timeline value's bytes remain identical.
    expect(canonicalJson(page)).toBe(canonicalJson(first))
    expect(page.items).toEqual(range(12_301, 12_500))
    expect(page.page.next_cursor).toBe('scenario-cursor/27700')
    expect(page.items.every(({ revision, final }) => revision === 1 && final)).toBe(true)
  })

  it('rejects malformed cursors with the page-cursor-expired contract', async () => {
    const error = await client.timelineList(thread.session_id, { limit: 50, cursor: 'scenario-cursor/not-a-number' }).then(
      () => undefined,
      (cause: unknown) => cause,
    )
    expect(error).toBeInstanceOf(ClientError)
    if (!(error instanceof ClientError)) throw error
    expect(error.status).toBe(410)
    expect(error.response).toMatchObject({ code: 'page-cursor-expired' })
  })

  it('streams revisions into the newest committed page but never into generated pages', async () => {
    const at = async () => {
      const page = (await client.timelineList(thread.session_id, { limit: 200 })).value
      return page.items.find(({ sequence }) => sequence === TOTAL - 1)!
    }
    expect(await at()).toMatchObject({ revision: 1, final: false })
    clock.advance(world.now + 1_000 - clock.now())
    expect(await at()).toMatchObject({ revision: 2, final: false })
    clock.advance(world.now + 2_000 - clock.now())
    const revised = await at()
    expect(revised).toMatchObject({ revision: 3, final: true })
    const older = (await client.timelineList(thread.session_id, { limit: 200, cursor: 'scenario-cursor/27500' })).value
    expect(older.items).toEqual(range(12_301, 12_500))
  })

  it('shows native socket readers only the committed live window, never the history metadata', () => {
    const frames: unknown[] = []
    const isConversationFrame = (value: unknown): value is { readonly kind: string; readonly items: TimelineEntry[]; readonly has_more: boolean } =>
      typeof value === 'object' && value !== null && 'kind' in value && 'items' in value && 'has_more' in value
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = (event) => {
      if (typeof event.data !== 'string') throw new Error('expected encoded replay frame')
      frames.push(JSON.parse(event.data))
    }
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'conversation', id: 'live', conversation: thread.agent }))
    expect(frames).toHaveLength(1)
    if (!isConversationFrame(frames[0])) throw new Error('expected a conversation frame')
    const frame = frames[0]
    expect(frame.kind).toBe('conversation')
    const live = foldSlice(world.slices.conversation, clock.now() - world.now).state.threads[0]!
    expect(frame.items).toEqual(live.items.slice(-live.page_size))
    expect(frame.has_more).toBe(true)
    expect(JSON.stringify(frames)).not.toContain('seeded-turns')
    socket.close()
  })

  it('keeps serialized slice files inside the committed tree budget', () => {
    const sizes = SLICE_KINDS.map((kind) => {
      const bytes = new TextEncoder().encode(canonicalJson(sliceFile(world, world.slices[kind]))).byteLength
      expect(bytes, `${kind} file bytes`).toBeLessThanOrEqual(8 * 1024 * 1024)
      return bytes
    })
    expect(sizes.reduce((a, b) => a + b, 0), 'huge tree share of the 24 MiB budget').toBeLessThanOrEqual(12 * 1024 * 1024)
    expect(sizes.reduce((a, b) => a + b, 0)).toBeGreaterThan(0)
  })

  afterAll(() => {
    replay.close()
  })
})

describe('huge variant coherence', () => {
  // Default files are covered above; CLI decode exhausts every variant. Keep this consumer-scale
  // matrix focused on huge overrides and the empty/streaming window transitions.
  const boundedVariants: Record<SliceKind, readonly string[]> = {
    roster: ['huge', 'empty', 'one-agent'],
    details: [],
    attention: [],
    conversation: ['huge', 'empty', 'streaming'],
    terminal: [],
    sync: [],
  }

  it('offers all generic variants and keeps huge window overrides coherent', () => {
    expect(world.available.roster).toContain('huge')
    expect(world.available.conversation).toContain('huge')
    expect(world.available.conversation).toEqual(expect.arrayContaining(['long', 'tool-heavy', 'failed-tools', 'remote-only-mail']))
    const declaredCast = new Set([...world.cast.agents.map(({ id }) => id), ...world.cast.people.map(({ id }) => id), ...world.cast.hosts.map(({ id }) => id)])
    const checkCast = (value: unknown): void => {
      if (typeof value === 'string') {
        if (/^(?:agent|person|host)\//.test(value)) {
          const identity = value.startsWith('person/') ? value.replace(/\/session\/.*$/u, '') : value
          expect(declaredCast.has(identity), value).toBe(true)
        }
      } else if (Array.isArray(value)) value.forEach(checkCast)
      else if (value !== null && typeof value === 'object') Object.values(value).forEach(checkCast)
    }
    for (const kind of SLICE_KINDS) for (const variant of boundedVariants[kind]) {
      const variantWorld = world.with({ [kind]: variant })
      const slice = variantWorld.slices[kind]
      expect(decodeSlice(world.id, slice), `${kind}:${variant}`).toEqual([])
      checkCast(slice.state)
      const offsets = slice.timeline.map((event) => event.at_ms)
      expect(offsets, `${kind}:${variant} timeline order`).toEqual([...offsets].sort((a, b) => a - b))
      const events = SLICE_KINDS.flatMap<TimelineEvent>((sliceKind) => variantWorld.slices[sliceKind].timeline).sort((a, b) => a.at_ms - b.at_ms)
      let store = 1
      for (const event of events) {
        if (['changes', 'thread-create', 'thread-remove', 'terminal-create', 'terminal-remove', 'entries', 'replace', 'incarnation', 'end'].includes(event._tag)) store += 1
        expect(event.store, `${kind}:${variant} store of ${event._tag}`).toBe(store)
      }
      if (kind === 'roster') {
        expect(variantWorld.slices.roster.state.agents.map(({ id }) => id)).toEqual(variant === 'empty' ? [] : variant === 'one-agent' ? [world.cast.agents[0]!.id] : roster.agents.map(({ id }) => id))
      }
      if (kind === 'conversation' && variant !== 'default' && variant !== 'loading') {
        for (const variantThread of variantWorld.slices.conversation.state.threads) {
          const instants = variantThread.items.map(({ timestamp }) => parseTimestamp(timestamp))
          expect(instants).toEqual([...instants].sort((a, b) => a - b))
          expect(instants.every((instant) => instant <= world.now)).toBe(true)
          const sequences = variantThread.items.map(({ sequence }) => sequence)
          if (variantThread.history !== undefined) {
            for (let index = 1; index < sequences.length; index += 1) expect(sequences[index], `${variant} sequence at ${index}`).toBe(sequences[index - 1]! + 1)
            expect(sequences.at(-1)).toBe(variantThread.history.total_entries)
          } else expect(sequences).toEqual(sequences.map((_sequence, index) => index + 1))
        }
      }
    }
  })

  it('reselecting the huge conversation variant regenerates the window and drops only the live revision script', () => {
    const reselected = world.with({ conversation: 'huge' }).slices.conversation
    expect(reselected.timeline).toEqual([])
    expect(reselected.state.threads).toHaveLength(1)
    const variantThread = reselected.state.threads[0]!
    expect(variantThread.history).toEqual(history)
    expect(variantThread.page_size).toBe(thread.page_size)
    expect(variantThread.has_more).toBe(thread.has_more)
    expect(variantThread.items).toEqual(range(FIRST_COMMITTED, TOTAL))
    for (const [index, entry] of variantThread.items.entries()) {
      if (entry.sequence === TOTAL - 1) expect(thread.items[index]!.final).toBe(false)
      else expect(thread.items[index]!).toEqual(entry)
    }
  })
})
