import { describe, expect, it } from 'vitest'
import { catalog, loadWorld } from '../src/index.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { ANCHOR_MS, parseTimestamp } from '../src/kit/time.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { buildWorld } from '../src/kit/world.ts'
import { syntheticFleetMidRefactor } from '../src/worlds/fleetMidRefactor.ts'
import { NARRATIVE_SPLICES } from '../src/worlds/narrativeAdoption.ts'
import { syntheticOfflineReconnectStorm } from '../src/worlds/offlineReconnectStorm.ts'
import { loadRecordedWorld } from './recorded-world.ts'

const narratives = ['fleet-mid-refactor', 'ci-failing-fix', 'long-debug-rabbit-hole', 'first-run-onboarding', 'merge-conflict-standoff', 'offline-reconnect-storm', 'release-candidate-polish'] as const

describe('production recording adoption and explicit synthetic splices', () => {
  it('retains the catalog and declares one honest source per slice', () => {
    expect(catalog).toHaveLength(24)
    expect(Object.keys(NARRATIVE_SPLICES)).toEqual(['fleet-mid-refactor', 'offline-reconnect-storm'])
    for (const id of narratives) {
      const world = loadWorld(id, { now: ANCHOR_MS })
      expect(world.id).toBe(id)
      expect(world.recording).toBeUndefined()
      for (const kind of ['roster', 'details', 'attention', 'sync'] as const) expect(world.slices[kind].source._tag).toBe('synthetic')
      expect(world.slices.terminal.source).toEqual({ _tag: 'recorded', recording: id })
      expect(world.slices.conversation.source._tag).toBe(id === 'fleet-mid-refactor' || id === 'offline-reconnect-storm' ? 'synthetic' : 'recorded')
    }
  })

  it('bounded clock reuse never shifts the anchor or opaque entry bodies', () => {
    const initial = loadWorld('ci-failing-fix', { now: ANCHOR_MS }).slices.conversation
    const anchored = structuredClone(initial)
    for (const offset of [123_456, 987_654, 0, 123_456]) {
      const shifted = loadWorld('ci-failing-fix', { now: ANCHOR_MS + offset }).slices.conversation
      const originalItems = initial.state.threads[0]!.items
      const shiftedItems = shifted.state.threads[0]!.items
      expect(shiftedItems.map((item) => item.body)).toEqual(originalItems.map((item) => item.body))
      expect(shiftedItems.map((item) => parseTimestamp(item.timestamp))).toEqual(originalItems.map((item) => parseTimestamp(item.timestamp) + offset))
      expect(shifted.timeline.map((event) => event.at_ms)).toEqual(initial.timeline.map((event) => event.at_ms))
    }
    expect(initial).toEqual(anchored)
    expect(loadWorld('ci-failing-fix', { now: ANCHOR_MS }).slices.conversation).toEqual(anchored)
  })

  for (const id of narratives) {
    it(`${id}: uses captured terminal values and offsets without authoring new recorded screens`, () => {
      const production = loadWorld(id, { now: ANCHOR_MS })
      const { world: recorded, rows } = loadRecordedWorld(id)
      expect(production.slices.terminal).toEqual({
        ...recorded.slices.terminal, state: { terminals: recorded.slices.terminal.state.terminals },
      })
      const terminals = [...production.slices.terminal.state.terminals, ...production.slices.terminal.timeline.flatMap((event) => event._tag === 'terminal-create' ? [event.record] : [])]
      for (const terminal of terminals) {
        const owner = production.cast.agents.find((member) => member.id === terminal.owner)
        expect(owner).toBeDefined()
        expect(terminal.runtime.id).toBe(owner?.terminalRuntime)
        expect(terminal.runtime.runtime_id).toBe(owner?.terminalRuntime.slice('runtime/'.length))
        const output = rows.filter((row) => row.kind === 'pty_output' && row.value.terminal === terminal.terminal)
        expect(terminal.cast.events).toHaveLength(output.length)
        for (const [index, [seconds, , text]] of terminal.cast.events.entries()) {
          const captured = output[index]!
          expect(terminal.cast.started_at_ms + seconds * 1000).toBeCloseTo(captured.receive_ms, 6)
          expect(text).toBe(captured.value.text)
        }
      }
      if (id !== 'fleet-mid-refactor' && id !== 'offline-reconnect-storm') {
        expect(production.slices.conversation).toEqual({
          ...recorded.slices.conversation,
          state: { threads: recorded.slices.conversation.state.threads.map((thread) => ({
            ...thread, has_more: thread.has_more || thread.items.length > thread.page_size,
          })) },
        })
      }
    })
  }

  it('fleet splices by agent ownership, retaining the migrator revision chain verbatim', () => {
    const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
    const legacy = buildWorld(syntheticFleetMidRefactor, genericVariants, ANCHOR_MS)
    const recorded = loadRecordedWorld(world.id).world
    const builder = world.cast.agents.find((member) => member.key === 'builder')!.id
    const migrator = world.cast.agents.find((member) => member.key === 'migrator')!.id
    expect(world.slices.conversation.state.threads.find((thread) => thread.agent === builder)).toEqual(recorded.slices.conversation.state.threads.find((thread) => thread.agent === builder))
    expect(world.slices.conversation.state.threads.find((thread) => thread.agent === migrator)).toEqual(legacy.slices.conversation.state.threads.find((thread) => thread.agent === migrator))
    const chain = world.slices.conversation.timeline.filter((event) => event._tag === 'entries' && event.agent === migrator)
    expect(chain.map((event) => event.at_ms)).toEqual([2_000, 3_000, 5_000])
    const first = chain[0]!
    const second = chain[1]!
    if (first._tag !== 'entries' || second._tag !== 'entries') throw new Error('Missing declared migrator splice events')
    expect(first.items[0]).toMatchObject({ revision: 1, final: false })
    expect(second.items[0]).toMatchObject({ id: first.items[0]!.id, revision: 2, final: true })
    expect(JSON.stringify(foldSlice(world.slices.conversation, 5_000).state)).toContain('packages/api/src/users/index.ts')
  })

  it('offline splices before the report, not by relabeling captured revision-one content', () => {
    const world = loadWorld('offline-reconnect-storm', { now: ANCHOR_MS })
    const legacy = buildWorld(syntheticOfflineReconnectStorm, genericVariants, ANCHOR_MS)
    const recorded = loadRecordedWorld(world.id).world
    const imported = recorded.slices.conversation.state.threads[0]!
    const boundary = imported.items.find((item) => item.type === 'content' &&
      typeof item.body === 'object' && item.body !== null && 'text' in item.body &&
      item.body.text === 'Readiness: 2 of 3 replicas')!
    const prefix = imported.items.filter((item) => item.sequence < boundary.sequence)
    const thread = world.slices.conversation.state.threads[0]!
    expect(thread.items.slice(0, -1)).toEqual(prefix)
    const report = thread.items.at(-1)!
    expect(report.id).toBe(legacy.slices.conversation.state.threads[0]!.items.at(-1)!.id)
    expect(report.id).not.toBe(boundary.id)
    expect(report).toMatchObject({ revision: 1, final: false })
    expect(report.sequence).toBeGreaterThan(Math.max(...prefix.map((item) => item.sequence)))
    const snapshots = world.slices.conversation.timeline.filter((event) => event._tag === 'replace')
    expect(snapshots.map((event) => event.at_ms)).toEqual([6_000, 13_000, 22_000])
    expect(snapshots.map((event) => event.items.at(-1)?.revision)).toEqual([3, 4, 5])
    for (const event of snapshots) expect(event.items.slice(0, -1)).toEqual(prefix)
    expect(foldSlice(world.slices.conversation, Infinity).state.threads[0]!.items.at(-1)).toMatchObject({ id: report.id, revision: 5, final: true })
  })
})
