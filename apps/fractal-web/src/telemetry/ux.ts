import { Context, Exit, Option, Tracer } from 'effect'
import type { SyncStatus } from '@st3/sdk/effect'

export const uxSpanNames = ['wf.ux.reload', 'wf.ux.switch', 'wf.ux.switch.data_ready', 'wf.ux.first_frame', 'wf.sync.transition', 'wf.ux.send_echo'] as const
export const uxAttributeNames = ['wf.ux.budget_ms', 'wf.ux.budget_phase', 'wf.ux.budget_met', 'wf.ux.shell_ms', 'wf.ux.roster_after_shell_ms', 'wf.ux.cache', 'wf.ux.slot_count', 'wf.ux.data_ready_ms', 'wf.ux.painted', 'wf.ux.outcome', 'wf.subscription.kind', 'wf.sync.from', 'wf.sync.to', 'wf.sync.stage', 'wf.sync.previous_ms', 'wf.sync.reconnect_ms'] as const

/** Browser-visible completion, not a React render or a data-arrival timestamp. */
export const afterNextPaint = (callback: () => void): (() => void) => {
  let channel: MessageChannel | undefined
  const frame = requestAnimationFrame(() => {
    channel = new MessageChannel()
    channel.port1.addEventListener('message', () => {
      channel?.port1.close()
      channel?.port2.close()
      callback()
    }, { once: true })
    channel.port1.start()
    channel.port2.postMessage(undefined)
  })
  return () => {
    cancelAnimationFrame(frame)
    channel?.port1.close()
    channel?.port2.close()
  }
}

interface Operation {
  readonly span: Tracer.Span
  readonly start: number
  readonly finish: (outcome: 'painted' | 'timeout' | 'superseded' | 'disposed' | 'observed') => void
}
export interface UxTelemetry {
  readonly activeSpan: () => Tracer.Span | undefined
  readonly traceContext: () => { readonly traceparent: string } | undefined
  readonly shellCommitted: () => () => void
  readonly rosterCommitted: () => () => void
  readonly beginSwitch: (options: { readonly ref: string; readonly warm: boolean; readonly slotCount: number }) => void
  readonly switchDataReady: (ref: string) => void
  /** Call from a ref/layout commit only when this transcript's actual content is committed. */
  readonly transcriptCommitted: (ref: string) => () => void
  /** Feature layer hook: invoke at send, complete only when the verified echo DOM commits. */
  readonly beginSendEcho: () => { readonly committed: () => () => void; readonly cancel: () => void }
  readonly observeSync: (options: { readonly key: object; readonly kind: 'window' | 'conversation' | 'terminal' | 'gateway'; readonly status: SyncStatus }) => void
  readonly dispose: () => void
}

/** Always-sampled, bounded roots. Identifiers remain only in local matching state, never attributes. */
export const makeUxTelemetry = ({ tracer, now = () => performance.now(), timeOrigin = performance.timeOrigin, paint = afterNextPaint, deadlineMs = 30_000 }: {
  readonly tracer: () => Tracer.Tracer
  readonly now?: () => number
  readonly timeOrigin?: number
  readonly paint?: (callback: () => void) => () => void
  readonly deadlineMs?: number
}): UxTelemetry => {
  const nanos = (ms: number) => BigInt(Math.round((timeOrigin + ms) * 1_000_000))
  const operations = new Set<Operation>()
  const paints = new Set<() => void>()
  const start = (name: string, label: string, attributes: Readonly<Record<string, unknown>>, startMs = now(), parent?: Tracer.Span): Operation => {
    const span = tracer().span({ name, parent: Option.fromUndefinedOr(parent), annotations: Context.empty(), links: [], startTime: nanos(startMs), kind: 'internal', root: parent === undefined, sampled: true })
    span.attribute('span.label', label)
    for (const [key, value] of Object.entries(attributes)) span.attribute(key, value)
    let ended = false
    const operation: Operation = { span, start: startMs, finish: (outcome) => {
      if (ended) return
      ended = true
      clearTimeout(timer)
      operations.delete(operation)
      span.attribute('wf.ux.outcome', outcome)
      span.end(nanos(now()), Exit.void)
    } }
    const timer = setTimeout(() => operation.finish('timeout'), deadlineMs)
    operations.add(operation)
    return operation
  }
  const onPaint = (callback: () => void) => {
    let active = true
    const cancelPaint = paint(() => {
      if (!active) return
      active = false
      paints.delete(cancel)
      callback()
    })
    const cancel = () => { active = false; cancelPaint(); paints.delete(cancel) }
    paints.add(cancel)
    return cancel
  }
  const reload = start('wf.ux.reload', 'roster', { 'wf.ux.budget_ms': 100, 'wf.ux.budget_phase': 'roster-after-shell', 'wf.ux.painted': false }, 0)
  let shellAt: number | undefined
  let rosterAt: number | undefined
  const finishReload = () => {
    if (shellAt === undefined || rosterAt === undefined || !operations.has(reload)) return
    const elapsed = Math.max(0, rosterAt - shellAt)
    reload.span.attribute('wf.ux.roster_after_shell_ms', elapsed)
    reload.span.attribute('wf.ux.budget_met', elapsed < 100)
    reload.span.attribute('wf.ux.painted', true)
    reload.finish('painted')
  }
  let switching: { readonly ref: string; readonly root: Operation; readonly data: Operation } | undefined
  const sync = new WeakMap<object, { readonly status: string; readonly since: number; readonly reconnectSince?: number }>()
  const current = () => switching !== undefined && operations.has(switching.root) ? switching.root.span : operations.has(reload) ? reload.span : undefined
  return {
    activeSpan: current,
    traceContext: () => { const span = current(); return span === undefined ? undefined : { traceparent: `00-${span.traceId}-${span.spanId}-${span.sampled ? '01' : '00'}` } },
    shellCommitted: () => onPaint(() => { shellAt ??= now(); reload.span.attribute('wf.ux.shell_ms', shellAt); finishReload() }),
    rosterCommitted: () => onPaint(() => { rosterAt ??= now(); finishReload() }),
    beginSwitch: ({ ref, warm, slotCount }) => {
      switching?.data.finish('superseded')
      switching?.root.finish('superseded')
      const root = start('wf.ux.switch', warm ? 'warm' : 'cold', { 'wf.ux.cache': warm ? 'warm' : 'cold', 'wf.ux.slot_count': slotCount, 'wf.ux.budget_ms': 100, 'wf.ux.budget_phase': 'selection-to-paint', 'wf.ux.painted': false })
      const data = start('wf.ux.switch.data_ready', 'first page', {}, root.start, root.span)
      switching = { ref, root, data }
    },
    switchDataReady: (ref) => {
      if (switching?.ref !== ref || !operations.has(switching.root) || !operations.has(switching.data)) return
      switching.root.span.attribute('wf.ux.data_ready_ms', now() - switching.root.start)
      switching.data.finish('observed')
    },
    transcriptCommitted: (ref) => {
      const selected = switching
      return onPaint(() => {
        if (selected === undefined || selected !== switching || selected.ref !== ref || !operations.has(selected.root)) return
        selected.root.span.attribute('wf.ux.painted', true)
        selected.root.span.attribute('wf.ux.budget_met', now() - selected.root.start < 100)
        selected.root.finish('painted')
      })
    },
    beginSendEcho: () => {
      const operation = start('wf.ux.send_echo', 'echo', { 'wf.ux.budget_ms': 100, 'wf.ux.budget_phase': 'send-to-echo-paint', 'wf.ux.painted': false })
      return {
        committed: () => onPaint(() => {
          if (!operations.has(operation)) return
          operation.span.attribute('wf.ux.painted', true)
          operation.span.attribute('wf.ux.budget_met', now() - operation.start < 100)
          operation.finish('painted')
        }),
        cancel: () => operation.finish('disposed'),
      }
    },
    observeSync: ({ key, kind, status }) => {
      const stage = status._tag === 'Progress' ? status.stage : undefined
      const identity = `${status._tag}:${stage ?? (status._tag === 'Stale' ? status.reason._tag : status._tag === 'Failed' ? status.cause._tag : '')}`
      const previous = sync.get(key)
      if (previous?.status === identity) return
      const at = now()
      const reconnecting = status._tag === 'Stale' && status.reason._tag === 'Reconnecting'
      const reconnectSince = previous?.reconnectSince ?? (reconnecting ? at : undefined)
      const elapsed = at - (previous?.since ?? at)
      const operation = start('wf.sync.transition', kind, {
        'wf.subscription.kind': kind,
        'wf.sync.from': previous?.status ?? 'initial',
        'wf.sync.to': status._tag,
        'wf.sync.previous_ms': elapsed,
        'wf.ux.budget_ms': deadlineMs,
        'wf.ux.budget_phase': 'transition-bound',
        'wf.ux.budget_met': elapsed <= deadlineMs,
        ...(stage === undefined ? {} : { 'wf.sync.stage': stage }),
        ...(status._tag === 'Live' && reconnectSince !== undefined ? { 'wf.sync.reconnect_ms': at - reconnectSince } : {}),
      }, Math.max(previous?.since ?? at, at - deadlineMs))
      operation.finish('observed')
      sync.set(key, { status: identity, since: at, ...(status._tag === 'Live' || reconnectSince === undefined ? {} : { reconnectSince }) })
    },
    dispose: () => { for (const cancel of paints) cancel(); for (const operation of operations) operation.finish('disposed'); switching = undefined },
  }
}
