import type { Agent, CollectionFrame, Resource, Snapshot } from '@smalltalk/st3-client'
import { decodeUnknownSync, Resource as ResourceCodec } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'

import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'

const snapshot: Snapshot = {
  id: 'snapshot/roster-perf',
  created_at: '2026-10-06T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: 1,
}
const agents: Agent[] = Array.from({ length: 200 }, (_, i) => ({
  id: `agent/perf-${i}`,
  kind: 'agent',
  revision: '1',
  updated_at: snapshot.created_at,
  name: `Agent ${i}`,
  state: 'running',
  reachability: 'local',
  runtime_ids: [],
  description: 'Realistic retained roster description. '.repeat(16),
}))
const frame = (items: Resource[]): CollectionFrame => ({
  kind: 'snapshot',
  id: 'f1',
  collection: 'agents',
  items,
  order: items.map((row) => row.id),
  has_more: false,
  snapshot,
})

describe('bounded ordered collection ingestion', () => {
  it('processes a >150KB warmed decoded burst below a frame of CPU per task', () => {
    const wire = JSON.stringify(frame(agents))
    expect(wire.length).toBeGreaterThan(150_000)
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<unknown>[] = []
    const decode = decodeUnknownSync(ResourceCodec)
    // Steady-state ingress excludes schema compilation; CPU accounting excludes host descheduling.
    for (const row of agents) decode(row)
    const ingest = makeWindowIngest({
      decode,
      publish: (value) => values.push(value),
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(JSON.parse(wire))
    expect(values).toHaveLength(0)
    let longestMs = 0
    let totalMs = 0
    let slices = 0
    while (tasks.length > 0) {
      const work = tasks.shift()!
      const started = process.cpuUsage()
      work()
      const cpu = process.cpuUsage(started)
      const elapsed = (cpu.user + cpu.system) / 1000
      longestMs = Math.max(longestMs, elapsed)
      totalMs += elapsed
      slices++
    }
    expect(values[0]?.items).toHaveLength(200)
    expect(longestMs).toBeLessThan(16)
    expect(totalMs).toBeLessThan(50)
    console.log(
      `[wf-roster-perf] ${JSON.stringify({ wireBytes: wire.length, longestCpuMs: longestMs, totalCpuMs: totalMs, slices })}`,
    )
  })

  it('yields after four work units with the default four-millisecond slice budget', () => {
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<Resource>[] = []
    let clock = 0
    let decoded = 0
    const ingest = makeWindowIngest({
      decode: (row) => {
        decoded++
        return row
      },
      publish: (value) => values.push(value),
      now: () => clock++,
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(frame(agents))
    let slices = 0
    while (tasks.length > 0) {
      const before = decoded
      tasks.shift()!()
      expect(decoded - before).toBeLessThanOrEqual(4)
      slices++
    }
    expect(decoded).toBe(200)
    expect(slices).toBeGreaterThanOrEqual(100)
    expect(values[0]?.items).toHaveLength(200)
  })

  it('preserves unchanged references, orders queued deltas and publishes exact frame metadata', () => {
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<Resource>[] = []
    let clock = 0
    const ingest = makeWindowIngest({
      decode: (row) => row,
      publish: (value) => values.push(value),
      now: () => clock++,
      sliceMs: 2,
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(frame(agents.slice(0, 2)))
    const changed = { ...agents[1]!, name: 'Only changed row' }
    ingest.accept({
      kind: 'changes',
      id: 'f1',
      collection: 'agents',
      upserts: [changed],
      removes: [],
      order: agents.slice(0, 2).map((row) => row.id),
      has_more: false,
      snapshot: { ...snapshot, store_index: 2 },
    })
    ingest.accept(frame(agents.slice(0, 2).map((row) => Object.assign({}, row))))
    while (tasks.length > 0) tasks.shift()!()
    expect(values).toHaveLength(3)
    expect(values[1]?.items[0]).toBe(values[0]?.items[0])
    expect(values[1]?.items[1]).not.toBe(values[0]?.items[1])
    expect(values[1]?.items[1]).toMatchObject({ name: 'Only changed row' })
    expect(values[1]?.snapshot.store_index).toBe(2)
    expect(values[2]?.snapshot.store_index).toBe(1)
    expect(values[2]?.items[0]).toBe(values[0]?.items[0])
    expect(values[2]?.rawItems[0]).toBe(values[0]?.rawItems[0])
  })

  it('a one-item delta decodes one row and retains the other 199', () => {
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<unknown>[] = []
    const decode = decodeUnknownSync(ResourceCodec)
    let decoded = 0
    const ingest = makeWindowIngest({
      decode: (row) => {
        decoded++
        return decode(row)
      },
      publish: (value) => values.push(value),
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(frame(agents))
    while (tasks.length > 0) tasks.shift()!()
    expect(decoded).toBe(200)
    decoded = 0
    const delta: CollectionFrame = {
      kind: 'changes',
      id: 'f1',
      collection: 'agents',
      upserts: [{ ...agents[17]!, name: 'The only changed row', revision: '2' }],
      removes: [],
      order: agents.map((row) => row.id),
      has_more: false,
      snapshot: { ...snapshot, store_index: 2 },
    }
    const started = performance.now()
    ingest.accept(delta)
    while (tasks.length > 0) tasks.shift()!()
    const elapsedMs = performance.now() - started
    expect(elapsedMs).toBeLessThan(16)
    expect(decoded).toBe(1)
    for (let i = 0; i < agents.length; i++)
      if (i === 17) expect(values[1]!.items[i]).not.toBe(values[0]!.items[i])
      else expect(values[1]!.items[i]).toBe(values[0]!.items[i])
    console.log(
      `[wf-roster-perf] ${JSON.stringify({ changedRows: decoded, retainedRows: agents.length - decoded, elapsedMs })}`,
    )
    ingest.accept({ ...delta, snapshot: { ...snapshot, store_index: 3 } })
    while (tasks.length > 0) tasks.shift()!()
    expect(decoded).toBe(1)
    expect(values[2]!.items).toBe(values[1]!.items)
  })

  it('cancels partially decoded old generations and ignores changes without a snapshot', () => {
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<Resource>[] = []
    let clock = 0
    const ingest = makeWindowIngest({
      decode: (row) => row,
      publish: (value) => values.push(value),
      now: () => clock++,
      sliceMs: 2,
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(frame(agents))
    tasks.shift()!()
    expect(values).toHaveLength(0)
    ingest.reset()
    expect(tasks).toHaveLength(0)
    ingest.accept({
      kind: 'changes',
      id: 'f1',
      collection: 'agents',
      upserts: [agents[0]!],
      removes: [],
      order: [agents[0]!.id],
      has_more: false,
      snapshot,
    })
    ingest.accept(frame([agents[1]!]))
    while (tasks.length > 0) tasks.shift()!()
    expect(values).toHaveLength(1)
    expect(values[0]?.items).toEqual([agents[1]])
  })
})
