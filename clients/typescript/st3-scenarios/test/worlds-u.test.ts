import { describe, expect, it } from 'vitest'
import { applyWindow, St3Client, type CollectionFrame, type CollectionWindow } from '@smalltalk/st3-client'
import * as Schema from '@smalltalk/st3-client/schema'

import { decodeSlice, everyVariant } from '../scripts/decode.ts'
import { sliceTimes } from '../scripts/emit.ts'
import { ANCHOR_MS } from '../src/kit/time.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { SLICE_KINDS, type AnySlice } from '../src/kit/slice.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { wireValues } from '../src/kit/wire.ts'
import { buildWorld } from '../src/kit/world.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { unknownFields } from '../src/worlds/unknownFields.ts'

const SHIFT_MS = 123_456_789
const build = (now = ANCHOR_MS) => buildWorld(unknownFields, genericVariants, now)

/** The decode gate repairs all declarations, then isolates each declared strict failure. */
describe('unknown-fields catalog world', () => {
  it.each([ANCHOR_MS, ANCHOR_MS + SHIFT_MS])('has tolerant success and exactly declared strict negatives at now=%s', (now) => {
    const world = build(now)
    for (const kind of SLICE_KINDS) {
      const slice = world.slices[kind]
      expect(decodeSlice(world.id, slice), kind).toEqual([])
      for (const wire of wireValues(slice)) {
        expect(() => Schema.decodeUnknownSync(Schema[wire.definition] as never, 'tolerant')(wire.value), `${kind}${wire.pointer}`).not.toThrow()
        const declared = (slice.unknown ?? []).some(({ pointer }) => pointer === wire.pointer || pointer.startsWith(`${wire.pointer}/`))
        const strict = () => Schema.decodeUnknownSync(Schema[wire.definition] as never, 'strict')(wire.value)
        if (declared) expect(strict, `${kind}${wire.pointer}`).toThrow()
        else expect(strict, `${kind}${wire.pointer}`).not.toThrow()
      }
    }
    const { roster, conversation } = world.slices
    expect(roster.unknown?.map(({ pointer }) => pointer)).toEqual([
      '/state/agents/0/protocol_features', '/state/agents/0/state', '/state/resources/0', '/timeline/0/upserts/0',
    ])
    expect(conversation.unknown?.map(({ pointer }) => pointer)).toEqual(['/state/threads/0/items/3', '/timeline/0/items/0'])
    expect(roster.state.agents[0]).toMatchObject({ state: 'reconciling', protocol_features: { diagnostic_summaries: true, compatibility_probes: true } })
    expect(roster.state.resources![0]).toMatchObject({ kind: 'compatibility-probe', owner_id: world.cast.agents[0]!.id })
    expect(conversation.state.threads[0]!.items.at(-1)).toMatchObject({ type: 'diagnostic_summary', body: { status: 'running' } })
  })

  it('rejects every planted missing declaration rather than permitting arbitrary tolerant contamination', () => {
    const world = build()
    for (const slice of [world.slices.roster, world.slices.conversation]) {
      for (const declaration of slice.unknown ?? []) {
        const planted = { ...slice, unknown: slice.unknown?.filter((path) => path !== declaration) }
        expect(decodeSlice(world.id, planted), `missing ${declaration.pointer}`).not.toEqual([])
      }
    }
    const roster = world.slices.roster
    const known = roster.state.agents[0]!
    const undeclared = { ...known, reachability: 'future-reachability' }
    expect(decodeSlice(world.id, { ...roster, state: { ...roster.state, agents: [undeclared] } })).not.toEqual([])
  })

  it('folds every event, keeps future resources out of known-kind dispatch and retires the probe', () => {
    const world = build()
    const offsets = [...new Set([0, ...SLICE_KINDS.flatMap((kind) => world.slices[kind].timeline.map(({ at_ms }) => at_ms))])].sort((a, b) => a - b)
    for (const offset of offsets) for (const kind of SLICE_KINDS) {
      const folded = foldSlice(world.slices[kind], offset) as AnySlice
      expect(folded.timeline.every(({ at_ms }) => at_ms > offset)).toBe(true)
      for (const wire of wireValues(folded)) expect(() => Schema.decodeUnknownSync(Schema[wire.definition] as never, 'tolerant')(wire.value)).not.toThrow()
    }
    const initial = world.slices.roster.state.resources![0]!
    expect(foldSlice(world.slices.roster, 1_999).state.resources).toEqual([initial])
    expect(foldSlice(world.slices.roster, 2_000).state.resources).toEqual([expect.objectContaining({ id: initial.id, revision: '2', payload: expect.objectContaining({ status: 'passed' }) })])
    expect(foldSlice(world.slices.roster, 5_000).state.resources).toEqual([])
    expect(foldSlice(world.slices.roster, 5_000).state.agents).toEqual(world.slices.roster.state.agents)
    const entries = foldSlice(world.slices.conversation, 2_000).state.threads[0]!.items
    expect(entries.at(-1)).toMatchObject({ type: 'diagnostic_summary', body: { probe_id: initial.id, status: 'passed' } })
    expect(entries.map(({ sequence }) => sequence)).toEqual([1, 2, 3, 4, 5])
    expect(new Set(entries.map(({ id }) => id)).size).toBe(entries.length)
    expect(world.slices.roster.timeline[0]!.store).toBeLessThan(world.slices.conversation.timeline[0]!.store)
  })

  it('rebases future-kind headers but leaves opaque timestamp-like payload text alone', () => {
    const anchor = build()
    const shifted = build(ANCHOR_MS + SHIFT_MS)
    const originalProbe = anchor.slices.roster.state.resources![0]!
    const shiftedProbe = shifted.slices.roster.state.resources![0]!
    expect(Date.parse(shiftedProbe.updated_at) - Date.parse(originalProbe.updated_at)).toBe(SHIFT_MS)
    expect(shiftedProbe.payload).toEqual(originalProbe.payload)
    expect(Date.parse(shifted.slices.conversation.state.threads[0]!.items.at(-1)!.timestamp) - Date.parse(anchor.slices.conversation.state.threads[0]!.items.at(-1)!.timestamp)).toBe(SHIFT_MS)
    expect(sliceTimes(anchor.slices.roster)).toEqual(expect.arrayContaining([
      { pointer: '/state/resources/0/updated_at', codec: 'timestamp' },
      { pointer: '/timeline/0/upserts/0/updated_at', codec: 'timestamp' },
    ]))
    expect(sliceTimes(anchor.slices.roster).some(({ pointer }) => pointer.includes('sample_timestamp'))).toBe(false)
    expect(sliceTimes(anchor.slices.conversation)).toEqual(expect.arrayContaining([
      { pointer: '/state/threads/0/items/3/timestamp', codec: 'timestamp' },
      { pointer: '/timeline/0/items/0/timestamp', codec: 'timestamp' },
    ]))
  })

  it.each([ANCHOR_MS, ANCHOR_MS + SHIFT_MS])('preserves unknown values through real St3Client HTTP and sockets at now=%s', async (now) => {
    const world = build(now)
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl: async (input, init) => {
      const response = await replay.fetch(input, init)
      Schema.decodeUnknownSync(Schema.Envelope, 'tolerant')(await response.clone().json())
      return response
    } })
    let window: CollectionWindow | undefined
    const frames: CollectionFrame[] = []
    const endings: (Error | undefined)[] = []
    try {
      await client.discover()
      const probe = world.slices.roster.state.resources![0]!
      const initialAgents = await client.agentsList()
      expect(initialAgents.value.items).toEqual([...world.slices.roster.state.agents, probe])
      const initialResources = await client.resourcesList({ kind: probe.kind })
      expect(initialResources.value.items.map(({ facts }) => facts)).toEqual([probe])
      const session = world.slices.conversation.state.threads[0]!.session_id
      expect((await client.timelineList(session)).value.items).toEqual(world.slices.conversation.state.threads[0]!.items)
      const stream = await client.collectionStream({ socket: replay.socket, onEnd: (error) => endings.push(error), onFrame: (frame) => {
        Schema.decodeUnknownSync(Schema.CollectionFrame, 'tolerant')(frame)
        frames.push(frame)
        if (frame.id === 'agents') window = applyWindow(window, frame)
      } })
      stream.subscribe('agents', 'agents', 200)
      stream.subscribeConversation('conversation', world.cast.agents[0]!.id)
      clock.advance(0)
      expect(window?.items).toEqual(initialAgents.value.items)
      expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'conversation', items: world.slices.conversation.state.threads[0]!.items })]))
      clock.advance(2_000)
      const passed = foldSlice(world.slices.roster, 2_000).state.resources![0]!
      expect(window?.items).toEqual([...world.slices.roster.state.agents, passed])
      expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'changes', upserts: [passed] })]))
      expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({
        kind: 'conversation', items: [foldSlice(world.slices.conversation, 2_000).state.threads[0]!.items.at(-1)],
      })]))
      expect((await client.resourcesList({ kind: probe.kind })).value.items.map(({ facts }) => facts)).toEqual([passed])
      expect((await client.agentsList()).value.items).toEqual(window?.items)
      expect((await client.timelineList(session)).value.items.at(-1)).toMatchObject({ type: 'diagnostic_summary', body: { status: 'passed' } })
      clock.advance(3_000)
      expect(window?.items).toEqual(world.slices.roster.state.agents)
      expect((await client.resourcesList({ kind: probe.kind })).value.items).toEqual([])
      expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'changes', removes: [probe.id] })]))
      expect(endings).toEqual([])
      stream.close()
    } finally { replay.close() }
  })

  it('keeps declarations valid for each named variant', () => {
    const world = build()
    for (const slice of everyVariant(world)) expect(decodeSlice(world.id, slice), `${slice.kind}/${slice.variant}`).toEqual([])
  })
})
