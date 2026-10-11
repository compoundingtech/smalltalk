import { describe, expect, it } from 'vitest'
import { decodeSlice } from '../scripts/decode.ts'
import { canonicalJson } from '../scripts/emit.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { SLICE_KINDS, type AnySlice } from '../src/kit/slice.ts'
import { ANCHOR_MS } from '../src/kit/time.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { buildWorld } from '../src/kit/world.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { empty } from '../src/worlds/empty.ts'
import { loading } from '../src/worlds/loading.ts'
import { replicationDiverged } from '../src/worlds/replicationDiverged.ts'
import { unicode, UNICODE_LONG_NAME, UNICODE_TEXT } from '../src/worlds/unicode.ts'

for (const definition of [empty, loading, replicationDiverged, unicode]) {
  describe(definition.id, () => {
    for (const now of [ANCHOR_MS, ANCHOR_MS + 123_456_789]) {
      it(`strict-decodes every slice and timeline fold at ${now}`, () => {
        const world = buildWorld(definition, genericVariants, now)
        for (const kind of SLICE_KINDS) {
          const slice = world.slices[kind]
          expect(decodeSlice(world.id, slice)).toEqual([])
          // Folding preserves the input discriminant, including when iterating a union.
          for (const event of slice.timeline) expect(decodeSlice(world.id, foldSlice(slice, event.at_ms) as AnySlice)).toEqual([])
        }
      })
    }
  })
}

it('empty retains override cast but returns successful empty resource pages', async () => {
  const world = buildWorld(empty, genericVariants, ANCHOR_MS)
  for (const kind of SLICE_KINDS.filter((kind) => kind !== 'sync')) {
    expect(Object.values(world.slices[kind].state).every((rows) => Array.isArray(rows) && rows.length === 0)).toBe(true)
  }
  expect(world.cast.agents.length).toBeGreaterThan(0)
  expect(world.with({ roster: 'one-agent' }).slices.roster.state.agents).toHaveLength(1)
  const replay = createReplay(world, { clock: manualClock(world.now) })
  try {
    expect((await replay.fetch('http://scenario.invalid/v1/client/capabilities')).ok).toBe(true)
    for (const route of ['agents', 'runtimes', 'machines', 'missions', 'work', 'attention', 'messages']) {
      const response = await replay.fetch(`http://scenario.invalid/v1/client/${route}`)
      expect(response.ok).toBe(true)
      const body = await response.json()
      expect(body.value.items).toEqual([])
    }
  } finally { replay.close() }
})

it('loading holds replies until cancelled without a fabricated progress stage', async () => {
  const world = buildWorld(loading, genericVariants, ANCHOR_MS)
  expect(SLICE_KINDS.filter((kind) => kind !== 'sync').every((kind) => world.slices[kind].loading)).toBe(true)
  expect(world.slices.sync.state.expected.every(({ status }) => status._tag === 'Requested')).toBe(true)
  expect(world.slices.sync.timeline.every((event) => event._tag === 'hold')).toBe(true)
  const replay = createReplay(world, { clock: manualClock(world.now) })
  const abort = new AbortController()
  try {
    expect((await replay.fetch('http://scenario.invalid/v1/client/capabilities')).ok).toBe(true)
    let settled = false
    const pending = replay.fetch('http://scenario.invalid/v1/client/agents', { signal: abort.signal }).finally(() => { settled = true })
    await Promise.resolve()
    expect(settled).toBe(false)
    abort.abort()
    await expect(pending).rejects.toThrow()
  } finally { abort.abort(); replay.close() }
})

it('divergence has a dated peer notice and clear while all statuses stay Live', async () => {
  const world = buildWorld(replicationDiverged, genericVariants, ANCHOR_MS)
  expect(world.slices.sync.state.expected.every(({ status }) => status._tag === 'Live')).toBe(true)
  const [notice, clear] = world.slices.sync.timeline
  expect(notice?._tag).toBe('notice')
  if (notice?._tag !== 'notice') throw new Error('missing divergence notice')
  expect(notice.peers).toHaveLength(1)
  expect(notice.peers[0]?.diverged_since).toBe(new Date(ANCHOR_MS - 120_000).toISOString())
  expect(notice.peers[0]?.local_only_envelopes).toBe(17)
  expect(notice.peers[0]?.peer_only_envelopes).toBe(9)
  expect(clear?._tag).toBe('notice-clear')
  expect(world.slices.sync.timeline.some((event) => ['close', 'error', 'open-fail', 'http-error'].includes(event._tag))).toBe(false)
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  try {
    const before = await (await replay.fetch('http://scenario.invalid/v1/client/agents')).json()
    expect(before.value.sync.state).toBe('diverged')
    expect(before.value.sync.peers).toEqual(notice.peers)
    clock.advance(30_000)
    const after = await (await replay.fetch('http://scenario.invalid/v1/client/agents')).json()
    expect(after.value.sync).toBeUndefined()
  } finally { replay.close() }
})

it('Unicode code points survive canonical JSON in all six slices and identities stay ASCII', () => {
  const world = buildWorld(unicode, genericVariants, ANCHOR_MS)
  for (const kind of SLICE_KINDS) {
    const json = canonicalJson(world.slices[kind])
    expect(json).toContain(UNICODE_TEXT)
    expect(canonicalJson(JSON.parse(json))).toBe(json)
    expect([...JSON.stringify(JSON.parse(json))]).toContain('\u0301')
  }
  expect([...world.slices.roster.state.agents[0]!.name]).toHaveLength(200)
  expect(world.slices.roster.state.agents[0]!.name).toBe(UNICODE_LONG_NAME)
  expect(world.cast.agents.every(({ id }) => /^[\x00-\x7f]+$/.test(id))).toBe(true)
  expect(UNICODE_TEXT.normalize('NFC')).not.toBe(UNICODE_TEXT)
})
