import { Agent as NativeAgent, decodeUnknownSync, UsageSummary, type UsageSummaryEncoded } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'
import * as Option from 'effect/Option'
import { contextOccupancy } from './contextOccupancy.ts'
import { createProjections } from './projections.ts'
import { sidebarRow } from '../web/sidebarRow.ts'

const usage = (context?: UsageSummaryEncoded['context']) => ({ _tag: 'Known' as const, value: decodeUnknownSync(UsageSummary)({
  aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1,
  input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214,
  ...(context === undefined ? {} : { context }),
}) })
const reading = { used_tokens: 61_206, window_tokens: 1_000_000, used_percent: 6.1206, observed_at_unix_ms: 3000 }
const native = decodeUnknownSync(NativeAgent)({ kind: 'agent', id: 'agent/example', name: 'Example', host_id: 'host/example', runtime_ids: [], reachability: 'reachable', state: 'running', revision: '1', updated_at: '1970-01-01T00:00:04.000Z', incarnation_id: 'first', usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 100, output_tokens: 200, cached_tokens: 300, total_tokens: 480_035_214, context: reading } })

describe('reported context occupancy', () => {
  it('uses the occupancy numerator/window, never cumulative tokens', () => {
    const projected = contextOccupancy({ usage: usage(reading), now: 10_000 })
    expect(projected).toMatchObject({ _tag: 'Known', usedTokens: 61_206, windowTokens: 1_000_000, observedAt: 3000 })
    expect(projected._tag === 'Known' ? projected.usedPercent : undefined).toBeCloseTo(6.1206, 6)
    // A cumulative-token substitution must fail the same criterion.
    expect(() => expect({ ...projected, usedPercent: 480_035_214 / 1_000_000 * 100 }).toEqual(projected)).toThrow()
  })
  it('does not turn missing occupancy or response totals into a percentage', () => {
    expect(contextOccupancy({ usage: { _tag: 'Unknown' }, now: 10_000 })).toEqual({ _tag: 'Unknown' })
    expect(contextOccupancy({ usage: usage(), now: 10_000 })).toEqual({ _tag: 'Unknown' })
    expect(contextOccupancy({ usage: usage({ observed_at_unix_ms: 3000 }), now: 10_000 })).toEqual({ _tag: 'Unknown' })
  })
  it('preserves zero and percentage-only reports without inventing token capacity', () => {
    expect(contextOccupancy({ usage: usage({ ...reading, used_tokens: 0, used_percent: 0 }), now: 10_000 })).toMatchObject({ _tag: 'Known', usedPercent: 0, usedTokens: 0 })
    expect(contextOccupancy({ usage: usage({ used_percent: 42, observed_at_unix_ms: 3000 }), now: 10_000 })).toEqual({ _tag: 'Known', usedPercent: 42, observedAt: 3000 })
    expect(contextOccupancy({ usage: usage({ used_tokens: 50, window_tokens: 0, observed_at_unix_ms: 3000 }), now: 10_000 })).toEqual({ _tag: 'Unknown' })
  })
  it('derives the precise ratio when the provider also reports a rounded percentage', () => {
    const projected = contextOccupancy({ usage: usage({ ...reading, used_percent: 6 }), now: 10_000 })
    expect(projected._tag === 'Known' ? projected.usedPercent : undefined).toBeCloseTo(6.1206, 6)
  })
  it('rejects future readings and retains the strict incarnation fence', () => {
    expect(contextOccupancy({ usage: usage(reading), now: 2000 })).toEqual({ _tag: 'Unknown' })
    expect(contextOccupancy({ usage: usage(reading), now: 10_000, observedAfter: 3000 })).toEqual({ _tag: 'Unknown' })
    expect(contextOccupancy({ usage: usage(reading), now: 10_000, observedAfter: 2999 })).toMatchObject({ _tag: 'Known' })
  })
  it('clears on an incarnation change, stays cleared on equal readings, and restores only on newer evidence', () => {
    const project = createProjections()
    const first = project.fleetFromAgents([native]).agents[0]!
    expect(sidebarRow({ agent: first, now: 10_000, stale: false }).context?._tag).toBe('Known')
    const restarted = { ...native, incarnation_id: Option.some('second') }
    const second = project.fleetFromAgents([restarted]).agents[0]!
    expect(sidebarRow({ agent: second, now: 10_000, stale: false }).context).toEqual({ _tag: 'Unknown' })
    // Reusing the same native DTO cannot bypass the transition fence in the WeakMap cache.
    const repeated = project.fleetFromAgents([restarted]).agents[0]!
    expect(repeated).toBe(second)
    expect(sidebarRow({ agent: repeated, now: 10_000, stale: false }).context).toEqual({ _tag: 'Unknown' })
    const newer = project.fleetFromAgents([{ ...restarted, usage: Option.some(usage({ ...reading, observed_at_unix_ms: 5000 }).value) }]).agents[0]!
    expect(sidebarRow({ agent: newer, now: 10_000, stale: false }).context).toMatchObject({ _tag: 'Known', observedAt: 5000 })
    expect(sidebarRow({ agent: newer, now: 10_000, stale: true }).context).toMatchObject({ _tag: 'Known', observedAt: 5000 })
    expect(() => expect(sidebarRow({ agent: second, now: 10_000, stale: false }).context?._tag).toBe('Known')).toThrow()
  })
  it('does not restore an old reading if occupancy was omitted at the transition', () => {
    const project = createProjections()
    const previous = { ...native, usage: Option.some(usage({ ...reading, observed_at_unix_ms: 6000 }).value) }
    project.fleetFromAgents([previous])
    project.fleetFromAgents([{ ...native, incarnation_id: Option.some('second'), usage: Option.none() }])
    const retained = project.fleetFromAgents([{ ...previous, incarnation_id: Option.some('second') }]).agents[0]!
    expect(sidebarRow({ agent: retained, now: 10_000, stale: false }).context).toEqual({ _tag: 'Unknown' })
  })
  it('keeps the incarnation fence when the ref leaves a bounded roster window and returns', () => {
    for (const returning of ['same', 'cloned'] as const) {
      const project = createProjections()
      project.fleetFromAgents([native])
      const restarted = { ...native, incarnation_id: Option.some('second') }
      expect(sidebarRow({ agent: project.fleetFromAgents([restarted]).agents[0]!, now: 10_000, stale: false }).context).toEqual({ _tag: 'Unknown' })
      // The ref is evicted from the window (e.g. liveSource's 100-row limit), then reappears.
      project.fleetFromAgents([])
      const back = project.fleetFromAgents([returning === 'same' ? restarted : { ...restarted }]).agents[0]!
      expect(back.contextObservedAfter).toBe(4000)
      expect(sidebarRow({ agent: back, now: 10_000, stale: false }).context).toEqual({ _tag: 'Unknown' })
      project.fleetFromAgents([])
      const fresh = project.fleetFromAgents([{ ...restarted, usage: Option.some(usage({ ...reading, observed_at_unix_ms: 5000 }).value) }]).agents[0]!
      expect(sidebarRow({ agent: fresh, now: 10_000, stale: false }).context).toMatchObject({ _tag: 'Known', observedAt: 5000 })
    }
  })
})
