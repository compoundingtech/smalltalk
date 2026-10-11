import { describe, expect, it } from 'vitest'
import { decodeSlice } from '../scripts/decode.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { SLICE_KINDS, type AnySlice } from '../src/kit/slice.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { buildWorld } from '../src/kit/world.ts'
import { offlineReconnectStorm } from '../src/worlds/offlineReconnectStorm.ts'
import { releaseCandidatePolish } from '../src/worlds/releaseCandidatePolish.ts'

const anchor = 1_780_000_000_000

describe('n3 narrative worlds', () => {
  for (const definition of [offlineReconnectStorm, releaseCandidatePolish]) {
    for (const now of [anchor, anchor + 123_456_789]) {
      it(definition.id + ' strictly decodes and folds at ' + now, () => {
        const world = buildWorld(definition, genericVariants, now)
        for (const kind of SLICE_KINDS) {
          const slice: AnySlice = world.slices[kind]
          expect(decodeSlice(world.id, slice)).toEqual([])
          for (const event of slice.timeline) {
            // Folding preserves this slice's kind/state pairing; the generic union loses it.
            expect(decodeSlice(world.id, foldSlice(slice, event.at_ms) as AnySlice)).toEqual([])
          }
          expect(foldSlice(slice, Infinity).timeline).toEqual([])
        }
      })
    }
  }

  it('replaces stale content at all three storm recovery checkpoints', () => {
    const world = buildWorld(offlineReconnectStorm, genericVariants, anchor)
    const conversation = world.slices.conversation
    const snapshots = conversation.timeline.filter((event) => event._tag === 'replace')
    expect(snapshots.map((event) => event.at_ms)).toEqual([6_000, 13_000, 22_000])
    for (const snapshot of snapshots) {
      const recovered = foldSlice(conversation, snapshot.at_ms).state.threads[0]!
      expect(recovered.items).toEqual(snapshot.items)
      expect(recovered.session_id).toEqual(snapshot.session_id)
    }
    const beforeReplacement = foldSlice(conversation, 12_000).state.threads[0]!.items
    expect(beforeReplacement.some((item) => JSON.stringify(item.body).includes('Temporary observation'))).toBe(true)
    const final = foldSlice(conversation, Infinity).state.threads[0]!.items
    expect(final).toEqual(snapshots[2]!.items)
    expect(final.some((item) => JSON.stringify(item.body).includes('Temporary observation'))).toBe(false)
    const report = final.find((item) => JSON.stringify(item.body).includes('Deployment verified'))!
    expect(report).toMatchObject({ revision: 5, final: true })
    expect(report.body).toMatchObject({ text: 'Deployment verified: 3 of 3 replicas ready, 12 of 12 canary checks passed, error budget unchanged. Safe to complete the rollout.' })
    expect(report.id).toBe(conversation.state.threads[0]!.items.at(-1)!.id)
    expect(world.slices.sync.timeline.filter((event) => event._tag === 'close')).toHaveLength(3)
    expect(world.slices.sync.timeline.filter((event) => event._tag === 'reopen')).toHaveLength(3)
    for (const surface of ['agents', 'missions', 'work', 'attention', 'conversation:' + world.cast.agents[0]!.id]) {
      const expected = world.slices.sync.state.expected.filter((value) => value.surface === surface)
      expect(expected.map((value) => [value.at_ms, value.status._tag])).toEqual([
        [0, 'Live'], [2_000, 'Stale'], [6_000, 'Live'], [8_000, 'Stale'], [13_000, 'Live'], [16_000, 'Stale'], [22_000, 'Live'],
      ])
      expect(expected.every((value) => value.compare === 'shape')).toBe(true)
    }
    // Planted regression: omitting the authoritative snapshot leaves the transient stale entry.
    const corrupted = { ...conversation, timeline: conversation.timeline.filter((event) => event._tag !== 'replace') }
    expect(foldSlice(corrupted, Infinity).state.threads[0]!.items).not.toEqual(final)
  })

  it('is a completed, green and idle release rather than a refactor', () => {
    const world = buildWorld(releaseCandidatePolish, genericVariants, anchor)
    expect(world.slices.roster.state.agents.every((value) => value.harness_state === 'idle')).toBe(true)
    expect(world.slices.roster.state.agents.every((value) => value.active_work_count === 0)).toBe(true)
    expect(world.slices.details.state.missions[0]!.state).toBe('completed')
    expect(world.slices.details.state.work).toHaveLength(3)
    expect(world.slices.details.state.work.every((value) => value.state === 'completed' && value.goals.length > 0)).toBe(true)
    expect(world.slices.attention.state.attention).toEqual([])
    const text = JSON.stringify(world.slices.conversation.state)
    expect(text).toContain('0.8.3')
    expect(text).toContain('0.8.4')
    expect(text).toContain('CHANGELOG.md')
    expect(text).toContain('Preserve selected tabs')
    expect(text).not.toContain('fetchUser')
    const terminal = world.slices.terminal.state.terminals[0]!
    expect(JSON.stringify(terminal.cast)).toContain('Matrix: 6/6 checks passed')
    expect(terminal.runtime.state).toBe('exited')
    // Planted regression: the calm story guard rejects even one busy harness.
    const busy = world.slices.roster.state.agents.map((value, index) => index === 0 ? { ...value, harness_state: 'busy' } : value)
    expect(busy.every((value) => value.harness_state === 'idle')).toBe(false)
  })
})
