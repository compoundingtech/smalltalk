import { availableParallelism, loadavg } from 'node:os'

import type { CollectionFrame } from '@smalltalk/st3-client'
import { decodeUnknownSync, Resource as ResourceCodec } from '@smalltalk/st3-client/schema'
import { expect, it } from 'vitest'

import { agents, frame, snapshot } from './windowIngest.fixture.ts'
import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'

it('keeps warmed burst tasks and one-row deltas below the frame CPU budget', () => {
  const wire = JSON.stringify(frame(agents))
  expect(wire.length).toBeGreaterThan(150_000)
  const decode = decodeUnknownSync(ResourceCodec)
  const sample = () => {
    const tasks: (() => void)[] = []
    const values: ProcessedWindow<unknown>[] = []
    const ingest = makeWindowIngest({
      decode,
      publish: (value) => values.push(value),
      schedule: (work) => {
        tasks.push(work)
        return () => tasks.splice(tasks.indexOf(work), 1)
      },
    })
    ingest.accept(JSON.parse(wire))
    let longestCpuMs = 0
    let totalCpuMs = 0
    let slices = 0
    while (tasks.length > 0) {
      const work = tasks.shift()!
      // Calling-thread CPU excludes descheduling and unrelated Node worker/GC threads.
      const started = process.threadCpuUsage()
      work()
      const cpu = process.threadCpuUsage(started)
      const elapsed = (cpu.user + cpu.system) / 1000
      longestCpuMs = Math.max(longestCpuMs, elapsed)
      totalCpuMs += elapsed
      slices++
    }
    expect(values[0]?.items).toHaveLength(200)
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
    const started = process.threadCpuUsage()
    ingest.accept(delta)
    while (tasks.length > 0) tasks.shift()!()
    const cpu = process.threadCpuUsage(started)
    expect(values[1]?.items).toHaveLength(200)
    return { longestCpuMs, totalCpuMs, deltaCpuMs: (cpu.user + cpu.system) / 1000, slices }
  }
  const loadBefore = loadavg()
  // Compile schemas and warm the complete path, then collect a fixed sample set.
  // Seven observations are not retries: every run contributes regardless of its result.
  sample()
  const samples = Array.from({ length: 7 }, sample)
  const median = (values: number[]) => values.toSorted((a, b) => a - b)[3]!
  const longestCpuMs = median(samples.map((value) => value.longestCpuMs))
  const totalCpuMs = median(samples.map((value) => value.totalCpuMs))
  const deltaCpuMs = median(samples.map((value) => value.deltaCpuMs))
  // Load is diagnostic only: never skip or adjust the budgets on a busy runner.
  console.log(`[wf-roster-perf] ${JSON.stringify({
    wireBytes: wire.length,
    availableParallelism: availableParallelism(),
    loadBefore,
    loadAfter: loadavg(),
    samples,
    median: { longestCpuMs, totalCpuMs, deltaCpuMs },
  })}`)
  expect(longestCpuMs).toBeLessThan(16)
  expect(totalCpuMs).toBeLessThan(50)
  expect(deltaCpuMs).toBeLessThan(16)
})
