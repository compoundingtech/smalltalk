import type { CollectionFrame, Resource } from '@smalltalk/st3-client'
import { decodeUnknownSync, Resource as ResourceCodec } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'

import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'
import { agents, frame, snapshot } from './windowIngest.fixture.ts'

describe('bounded ordered collection ingestion', () => {
  it('bounds rejected-row reporting to the present latest revision and clears departed/recovered rows', () => {
    const tasks: (() => void)[] = []
    const reports: string[] = []
    const row = agents[0]!
    const ingest = makeWindowIngest({
      decode: (value) => value.revision === 'recovered' ? value : undefined,
      onRejected: (value) => reports.push(value.revision),
      publish: () => {},
      now: () => 0,
      schedule: (work) => { tasks.push(work); return () => {} },
    })
    const send = (items: Resource[]) => {
      ingest.accept(frame(items))
      while (tasks.length > 0) tasks.shift()!()
    }
    send([row])
    send([{ ...row, name: 'Changed contents' }])
    expect(reports).toEqual(['1'])
    expect(ingest.rejectedRowCount).toBe(1)
    send([{ ...row, revision: '2' }])
    expect(reports).toEqual(['1', '2'])
    expect(ingest.rejectedRowCount).toBe(1)
    send([])
    expect(ingest.rejectedRowCount).toBe(0)
    send([row])
    expect(reports).toEqual(['1', '2', '1'])
    send([{ ...row, revision: 'recovered' }])
    expect(ingest.rejectedRowCount).toBe(0)
    send([row])
    expect(reports).toEqual(['1', '2', '1', '1'])
    expect(ingest.rejectedRowCount).toBe(1)
  })

  it('yields between bounded decode and ordering slices for a >150KB burst', () => {
    const wire = JSON.stringify(frame(agents))
    expect(wire.length).toBeGreaterThan(150_000)
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<unknown>[] = []
    const decode = decodeUnknownSync(ResourceCodec)
    let clock = 0
    let decoded = 0
    const ingest = makeWindowIngest({
      decode: (row) => {
        decoded++
        return decode(row)
      },
      publish: (value) => values.push(value),
      // Each generator step costs one virtual millisecond, independent of host load.
      now: () => clock++,
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(JSON.parse(wire))
    expect(decoded).toBe(0)
    expect(values).toHaveLength(0)
    let slices = 0
    while (tasks.length > 0) {
      // Exactly one continuation is pending; accept and each slice return to the task queue.
      expect(tasks).toHaveLength(1)
      const before = decoded
      tasks.shift()!()
      expect(decoded - before).toBeLessThanOrEqual(4)
      slices++
      if (tasks.length > 0) expect(values).toHaveLength(0)
    }
    expect(decoded).toBe(200)
    // 200 decode yields + 200 ordering yields, four per task, then publication.
    // Removing either phase's yields must fail, even if decoding remains bounded.
    expect(slices).toBe(101)
    expect(values).toHaveLength(1)
    expect(values[0]?.items).toHaveLength(200)
    expect(values[0]?.rawItems.map((row) => row.id)).toEqual(agents.map((row) => row.id))
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
    ingest.accept(delta)
    while (tasks.length > 0) tasks.shift()!()
    expect(decoded).toBe(1)
    for (let i = 0; i < agents.length; i++)
      if (i === 17) expect(values[1]!.items[i]).not.toBe(values[0]!.items[i])
      else expect(values[1]!.items[i]).toBe(values[0]!.items[i])
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
