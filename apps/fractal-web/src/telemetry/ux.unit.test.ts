import { Option, Tracer } from 'effect'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { FIRST_FRAME_SPAN, FOLLOW_SUBSCRIBE_SPAN, makeFirstFrames } from '../../packages/st3-sdk/src/effect/firstFrame.ts'
import { makeSpanRing } from './spanRing.ts'
import { makeUxTelemetry } from './ux.ts'

const harness = () => {
  vi.useFakeTimers()
  const ring = makeSpanRing()
  const ended: Tracer.Span[] = []
  const tracer = Tracer.make({ span: (options) => {
    const span = Tracer.nativeTracer.span(options)
    const end = span.end.bind(span)
    span.end = (at, exit) => {
      end(at, exit)
      ended.push(span)
      ring.push({ name: span.name, label: String(span.attributes.get('span.label') ?? ''), startMs: Number(options.startTime) / 1e6, durationMs: Number(at - options.startTime) / 1e6, attributes: Object.fromEntries(span.attributes) })
    }
    return span
  } })
  let at = 0
  const paints = new Set<() => void>()
  const ready = vi.fn()
  const ux = makeUxTelemetry({ tracer: () => tracer, now: () => at, timeOrigin: 0, paint: (callback) => { paints.add(callback); return () => { paints.delete(callback) } }, onFirstPaint: ready })
  return { ux, tracer, ready, snapshot: () => { vi.advanceTimersByTime(250); return ring.getSnapshot() }, ended, time: (value: number) => { at = value }, paint: () => { const callbacks = [...paints]; paints.clear(); for (const callback of callbacks) callback() } }
}

afterEach(() => vi.useRealTimers())
describe('bounded perceived UX', () => {
  it.each(['shellCommitted', 'rosterCommitted'] as const)('does not release startup transport until %s has painted', (commit) => {
    const h = harness()
    expect(h.ux.activeSpan()).toBeDefined()
    expect(h.ux.traceContext()?.traceparent).toMatch(/^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/)
    expect(h.ready).not.toHaveBeenCalled()
    h.ux[commit]()
    expect(h.ready).not.toHaveBeenCalled()
    h.paint()
    expect(h.ready).toHaveBeenCalledOnce()
    h.ux.dispose()
  })
  it('cancels readiness with an abandoned commit or disposed page', () => {
    const h = harness()
    const cancel = h.ux.shellCommitted()
    cancel()
    h.paint()
    expect(h.ready).not.toHaveBeenCalled()
    h.ux.rosterCommitted()
    h.ux.dispose()
    h.paint()
    expect(h.ready).not.toHaveBeenCalled()
  })
  it('ends reload on visible roster, measuring the q96 shell-to-roster budget', () => {
    const h = harness()
    h.time(25); h.ux.shellCommitted(); h.paint()
    h.time(75); h.ux.rosterCommitted()
    expect(h.snapshot().spans).toHaveLength(0)
    h.time(80); h.paint()
    expect(h.snapshot().spans[0]).toMatchObject({ name: 'wf.ux.reload', durationMs: 80, label: 'roster', attributes: { 'wf.ux.shell_ms': 25, 'wf.ux.roster_after_shell_ms': 55, 'wf.ux.budget_ms': 100, 'wf.ux.budget_met': true, 'wf.ux.painted': true } })
    h.ux.dispose()
  })
  it('separates data-ready from committed paint and preserves root-child context', () => {
    const h = harness()
    h.time(10); h.ux.beginSwitch({ ref: 'agent/private-id', warm: false, slotCount: 3 })
    const root = h.ux.activeSpan()!
    h.time(30); h.ux.switchDataReady('agent/private-id')
    expect(h.snapshot().spans.map((span) => span.name)).toEqual(['wf.ux.switch.data_ready'])
    expect(h.ended[0]?.parent).toMatchObject({ value: { spanId: root.spanId } })
    expect(h.ux.traceContext()?.traceparent).toBe(`00-${root.traceId}-${root.spanId}-01`)
    h.time(35); h.ux.transcriptCommitted('agent/private-id')
    expect(h.snapshot().spans).toHaveLength(1)
    h.time(40); h.paint()
    expect(h.snapshot().spans[1]).toMatchObject({ name: 'wf.ux.switch', label: 'cold', durationMs: 30, attributes: { 'wf.ux.data_ready_ms': 20, 'wf.ux.slot_count': 3, 'wf.ux.painted': true } })
    expect(JSON.stringify(h.snapshot())).not.toContain('private-id')
    h.ux.dispose()
  })
  it('parents every switch subscription kind to the switch without independent first-frame roots', () => {
    const h = harness()
    const firstFrames = makeFirstFrames({ tracer: h.tracer, parentSpan: h.ux.activeSpan })
    h.ux.beginSwitch({ ref: 'session/selected', warm: false, slotCount: 3 })
    const root = h.ux.activeSpan()!
    for (const kind of ['window', 'conversation', 'terminal'] as const) {
      const attempt = firstFrames.begin({ kind, label: kind === 'window' ? 'agents' : kind })
      attempt.end('observed')
      attempt.end('disposed')
      const child = h.ended.at(-1)!
      expect(child.name).toBe(FOLLOW_SUBSCRIBE_SPAN)
      expect(child.traceId).toBe(root.traceId)
      expect(Option.getOrUndefined(child.parent)).toBe(root)
      expect(attempt.trace).toEqual({ traceparent: `00-${root.traceId}-${child.spanId}-01` })
      expect(child.attributes.get('wf.ux.outcome')).toBe('observed')
    }
    expect(h.ended).toHaveLength(3)
    expect(h.ended.some((span) => span.name === FIRST_FRAME_SPAN)).toBe(false)
    h.ux.dispose()
    h.snapshot()
    expect(vi.getTimerCount()).toBe(0)
  })
  it('falls back to independent first-frame roots once no UX parent is active', () => {
    const h = harness()
    const firstFrames = makeFirstFrames({ tracer: h.tracer, parentSpan: h.ux.activeSpan })
    h.ux.shellCommitted()
    h.ux.rosterCommitted()
    h.paint()
    expect(h.ux.activeSpan()).toBeUndefined()
    const traces = new Set<string>()
    for (const kind of ['window', 'conversation', 'terminal'] as const) {
      const attempt = firstFrames.begin({ kind, label: kind === 'window' ? 'agents' : kind })
      attempt.end('observed')
      const root = h.ended.at(-1)!
      expect(root.name).toBe(FIRST_FRAME_SPAN)
      expect(root.parent._tag).toBe('None')
      expect(attempt.trace).toEqual({ traceparent: `00-${root.traceId}-${root.spanId}-01` })
      expect(root.attributes.get('wf.ux.budget_ms')).toBe(30_000)
      expect(root.attributes.get('wf.ux.budget_phase')).toBe('subscribe-to-first-frame')
      expect(root.attributes.get('wf.ux.budget_met')).toBe(true)
      traces.add(root.traceId)
    }
    expect(traces.size).toBe(3)
    expect(h.ended.some((span) => span.name === FOLLOW_SUBSCRIBE_SPAN)).toBe(false)
    h.ux.dispose()
    h.snapshot()
    expect(vi.getTimerCount()).toBe(0)
  })
  it('does not complete a superseding selection on an old committed paint', () => {
    const h = harness()
    h.ux.beginSwitch({ ref: 'first', warm: true, slotCount: 1 })
    h.ux.transcriptCommitted('first')
    h.ux.beginSwitch({ ref: 'second', warm: false, slotCount: 2 })
    h.paint()
    expect(h.snapshot().spans.filter((span) => span.name === 'wf.ux.switch')).toEqual([expect.objectContaining({ attributes: expect.objectContaining({ 'wf.ux.outcome': 'superseded', 'wf.ux.painted': false }) })])
    h.ux.dispose()
  })
  it('deduplicates SyncStatus v2 stage progress and measures the full reconnect gap', () => {
    const h = harness(), key = {}
    h.time(5); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Live', since: 5 } })
    h.time(10); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: 100, issue: 'closed' } } })
    h.time(20); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Requested', since: 20 } })
    h.time(30); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Progress', stage: 'reading', elapsedMs: 10, stageSince: 20, reportedAt: 30 } })
    h.time(40); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Progress', stage: 'reading', elapsedMs: 20, stageSince: 20, reportedAt: 40 } })
    h.time(50); h.ux.observeSync({ key, kind: 'conversation', status: { _tag: 'Live', since: 50 } })
    expect(h.snapshot().spans.filter((span) => span.name === 'wf.sync.transition')).toHaveLength(5)
    expect(h.snapshot().spans.at(-1)?.attributes['wf.sync.reconnect_ms']).toBe(40)
    h.ux.dispose()
  })
  it('times an answered alert to the paint that no longer shows it, against st\'s 2 s alert_landed budget', () => {
    const h = harness()
    h.time(100)
    const fast = h.ux.beginAlertAnswer()
    h.time(900); fast.landed(); h.time(950); h.paint()
    const slow = h.ux.beginAlertAnswer()
    h.time(3500); slow.landed(); h.paint()
    h.ux.beginAlertAnswer().cancel()
    const spans = h.snapshot().spans.filter((span) => span.name === 'wf.ux.alert_landed')
    expect(spans.map((span) => [span.durationMs, span.attributes['wf.ux.budget_met'], span.attributes['wf.ux.outcome']])).toEqual([
      [850, true, 'painted'], [2550, false, 'painted'], [0, undefined, 'disposed'],
    ])
    expect(spans[0]?.attributes['wf.ux.budget_ms']).toBe(2000)
    h.ux.dispose()
  })
  it('bounds missing paints and send-echo hooks and cleans all operations on disposal', () => {
    const h = harness()
    h.ux.beginSwitch({ ref: 'unpainted', warm: false, slotCount: 0 })
    h.ux.beginSendEcho()
    h.time(30_000); vi.advanceTimersByTime(30_000)
    expect(h.snapshot().spans.every((span) => span.attributes['wf.ux.outcome'] === 'timeout')).toBe(true)
    expect(h.snapshot().spans).toHaveLength(4)
    h.ux.dispose()
    expect(vi.getTimerCount()).toBe(0)
  })
})
