import { describe, expect, it } from 'vitest'
import { St3Client, type CollectionFrame } from '@smalltalk/st3-client'
import { decodeSlice } from '../scripts/decode.ts'
import { buildWorld } from '../src/kit/world.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { SLICE_KINDS, type AnySlice } from '../src/kit/slice.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { ANCHOR_MS } from '../src/kit/time.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { fleetMidRefactor } from '../src/worlds/fleetMidRefactor.ts'
import { failedSyncOpenFail } from '../src/worlds/failedSyncOpenFail.ts'
import { failedSyncResyncCoded } from '../src/worlds/failedSyncResyncCoded.ts'
import { failedSyncForbidden } from '../src/worlds/failedSyncForbidden.ts'
import { failedSyncNonClientResponse } from '../src/worlds/failedSyncNonClientResponse.ts'
import { failedSyncSubscriptionError } from '../src/worlds/failedSyncSubscriptionError.ts'
import { failedSyncSubscriptionLimitLegacy } from '../src/worlds/failedSyncSubscriptionLimitLegacy.ts'
import { failedSyncCapabilityAbsent } from '../src/worlds/failedSyncCapabilityAbsent.ts'
import { failedSyncRateLimited } from '../src/worlds/failedSyncRateLimited.ts'
import { failedSyncCursorGap } from '../src/worlds/failedSyncCursorGap.ts'
import { failedSyncPageCursorExpired } from '../src/worlds/failedSyncPageCursorExpired.ts'
const definitions = [failedSyncOpenFail, failedSyncResyncCoded, failedSyncForbidden, failedSyncNonClientResponse, failedSyncSubscriptionError, failedSyncSubscriptionLimitLegacy, failedSyncCapabilityAbsent, failedSyncRateLimited, failedSyncCursorGap, failedSyncPageCursorExpired]
const NOWS = [ANCHOR_MS, ANCHOR_MS + 123_456_789]
/** The reference fleet each failed-sync world must preserve, built once per clock. */
const fleets = NOWS.map((now) => buildWorld(fleetMidRefactor, genericVariants, now))

describe.each(definitions)('$id', (definition) => {
  it('strict-decodes both clocks, folds every event and preserves the fleet', () => {
    for (const [index, now] of NOWS.entries()) {
      const world = buildWorld(definition, genericVariants, now)
      const fleet = fleets[index]!
      expect(world.cast).toEqual(fleet.cast)
      for (const kind of SLICE_KINDS) {
        const slice = world.slices[kind]
        expect(decodeSlice(world.id, slice)).toEqual([])
        for (const event of slice.timeline) expect(decodeSlice(world.id, foldSlice(slice, event.at_ms) as AnySlice)).toEqual([])
        if (kind !== 'sync') expect(slice).toEqual(fleet.slices[kind])
      }
      expect(world.slices.sync.variant).toBe(definition.selected?.sync)
      expect(world.slices.sync.state.expected.length).toBeGreaterThan(0)
    }
  })
  it('delivers the intended fault through the real client without contaminating other reads', async () => {
    const world = buildWorld(definition, genericVariants, ANCHOR_MS)
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl: replay.fetch })
    const cause = definition.selected?.sync
    const thread = world.slices.conversation.state.threads[0]!
    try {
      const http = world.slices.sync.timeline.find((event) => event._tag === 'http-error')
      if (http?._tag === 'http-error') {
        const read = () => http.route === 'capabilities' ? client.discover()
          : http.route === 'resources' ? client.resourcesList({})
          : http.route === 'events' ? client.eventsList()
          : client.timelineList(thread.session_id, { limit: 1, cursor: olderCursor })
        let olderCursor: string | undefined
        if (http.route === 'timeline') {
          const first = await client.timelineList(thread.session_id, { limit: 1 })
          expect(first.value.items.length).toBe(1)
          olderCursor = first.value.page.next_cursor ?? undefined
          expect(olderCursor).toBeDefined()
        }
        await expect(read()).rejects.toMatchObject({ status: http.status, response: http.envelope })
        for (const expected of world.slices.sync.state.expected) if (expected.status._tag === 'Failed' && expected.status.cause._tag === 'Server') {
          expect(expected.status.cause.code).toBe(http.envelope.code)
          expect(expected.status.cause.message).toBe(http.envelope.message)
        }
        if (http.route !== 'capabilities') expect((await client.discover()).value.kind).toBe('capabilities')
      } else if (cause === 'non-client-response') {
        await expect(client.discover()).rejects.toBeInstanceOf(Error)
      } else {
        const capabilities = await client.discover()
        if (cause === 'capability-absent') {
          expect(capabilities.value.capabilities.some((row) => row.id === 'terminal.attach')).toBe(false)
          const terminal = world.slices.terminal.state.terminals[0]!
          const runtime = await client.runtimesGet(terminal.runtime.id)
          const result = await client.terminalAttach({ id: 'action/scenario-attach', idempotency_key: 'scenario-attach', parameters: { target_id: terminal.terminal }, fence: { snapshot_id: runtime.snapshot.id, subject_revisions: {}, runtime_incarnation: terminal.incarnation } })
          expect(result.value).toBeDefined()
          expect(world.slices.sync.state.expected[0]?.compare).toBe('shape')
        }
      }
      if (cause === 'forbidden' || cause === 'non-client-response') {
        expect((await replay.fetch('http://scenario.invalid/v1/client/agents')).status).toBe(200)
        const frames: CollectionFrame[] = []
        const stream = await client.collectionStream({ socket: replay.socket, onFrame: (frame) => frames.push(frame) })
        stream.subscribe('agents', 'agents', 200)
        clock.advance(0)
        expect(frames.some((frame) => frame.kind === 'snapshot')).toBe(true)
        stream.close()
        await expect(client.discover()).rejects.toBeInstanceOf(Error)
        return
      }
      if (cause !== 'rate-limited') expect((await client.agentsList()).value.items.length).toBeGreaterThan(0)
      if (cause !== 'page-cursor-expired') expect((await client.timelineList(thread.session_id, { limit: 1 })).value.items.length).toBe(1)
      const frames: CollectionFrame[] = []
      let ended = false
      const stream = await client.collectionStream({ socket: replay.socket, onFrame: (frame) => frames.push(frame), onEnd: () => { ended = true } })
      stream.subscribe('agents', 'agents', 200)
      stream.subscribeConversation('conversation', thread.agent)
      clock.advance(0)
      if (cause === 'open-fail') {
        expect(ended).toBe(true)
        expect(frames).toHaveLength(0)
        let reopenedEnded = false
        await client.collectionStream({ socket: replay.socket, onFrame: (frame) => frames.push(frame), onEnd: () => { reopenedEnded = true } })
        clock.advance(0)
        expect(reopenedEnded).toBe(true)
      } else {
        if (cause === 'subscription-error') {
          expect(frames.find((frame) => frame.kind === 'error')).toMatchObject({ code: 'not-found', message: 'The agent collection is unavailable' })
          expect(frames.some((frame) => frame.kind === 'conversation')).toBe(true)
        } else if (cause === 'subscription-limit-legacy') {
          expect(frames.find((frame) => frame.kind === 'error')).toMatchObject({ message: 'Subscription limit reached' })
          expect(frames.find((frame) => frame.kind === 'error')).not.toHaveProperty('code')
        } else {
          expect(frames.some((frame) => frame.kind === 'snapshot')).toBe(true)
          expect(frames.some((frame) => frame.kind === 'conversation')).toBe(true)
        }
        if (cause === 'subscription-error' || cause === 'subscription-limit-legacy') {
          const failures = frames.filter((frame) => frame.kind === 'error').length
          stream.unsubscribe('agents')
          stream.subscribe('agents', 'agents', 200)
          expect(frames.filter((frame) => frame.kind === 'error').length).toBeGreaterThan(failures)
        }
        if (cause === 'resync-coded') {
          const initial = frames.find((frame) => frame.kind === 'conversation')
          clock.advance(4_000)
          expect(frames.find((frame) => frame.kind === 'resync')).toMatchObject({ code: 'remote-unavailable', message: 'The conversation peer is temporarily unavailable' })
          expect(initial).toBeDefined()
          expect(frames.some((frame) => frame.kind === 'snapshot' && frame.id === 'agents')).toBe(true)
        }
      }
      stream.close()
    } finally { replay.close() }
  })
})

it('covers every active sync matrix row and excludes pending rows', () => {
  const world = buildWorld(fleetMidRefactor, genericVariants, ANCHOR_MS)
  const active = ['live', 'connecting', 'requested', 'socket-dropped', 'reconnected', 'closed-unknown', 'resync-coded', 'resync-uncoded', 'forbidden', 'non-client-response', 'open-fail', 'subscription-error', 'subscription-limit-local', 'subscription-limit-legacy', 'evicted', 'capability-absent', 'rate-limited', 'cursor-gap', 'page-cursor-expired', 'progress-absent']
  for (const variant of active) {
    const slice = world.with({ sync: variant }).slices.sync
    expect(decodeSlice(world.id, slice)).toEqual([])
    expect(slice.state.expected.length).toBeGreaterThan(0)
  }
  for (const pending of ['subscription-limit-coded', 'quiet', 'progress']) expect(world.available.sync).not.toContain(pending)
  expect(world.with({ sync: 'evicted' }).slices.sync.state.local?._tag).toBe('hidden-follow')
  expect(world.with({ sync: 'subscription-limit-local' }).slices.sync.state.local).toEqual({ _tag: 'visible-follows', count: 9 })
})

it('withholds requested snapshots, then releases progress-absent without invented stages', async () => {
  for (const variant of ['requested', 'progress-absent']) {
    const world = buildWorld(fleetMidRefactor, genericVariants, ANCHOR_MS, { sync: variant })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    const frames: CollectionFrame[] = []
    try {
      const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl: replay.fetch })
      const stream = await client.collectionStream({ socket: replay.socket, onFrame: (frame) => frames.push(frame) })
      stream.subscribe('agents', 'agents', 200)
      clock.advance(0)
      expect(frames).toHaveLength(0)
      clock.advance(6_000)
      expect(frames.some((frame) => frame.kind === 'snapshot')).toBe(variant === 'progress-absent')
      expect(world.slices.sync.state.expected.some(({ status }) => status._tag === 'Progress')).toBe(false)
      stream.close()
    } finally { replay.close() }
  }
})

it('holds connecting socket opens without reporting transport failure', async () => {
  const world = buildWorld(fleetMidRefactor, genericVariants, ANCHOR_MS, { sync: 'connecting' })
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl: replay.fetch })
  const frames: CollectionFrame[] = []
  let ended = false
  try {
    const stream = await client.collectionStream({ socket: replay.socket, onFrame: (frame) => frames.push(frame), onEnd: () => { ended = true } })
    stream.subscribe('agents', 'agents', 200)
    clock.advance(20_000)
    expect(frames).toHaveLength(0)
    expect(ended).toBe(false)
    expect(world.slices.sync.state.expected[0]?.status._tag).toBe('Connecting')
    stream.close()
  } finally { replay.close() }
})
