import { RegistryContext, useAtomValue } from '@effect/atom-react'
import { Devbar } from '@overeng/devbar'
import type { DevbarPanel, DevbarSegment } from '@overeng/devbar'
import { darkDevbarTheme, lightDevbarTheme } from '@overeng/devbar/themes'
import { makeMeters, makeSeries } from '@overeng/meters'
import type { FpsValue } from '@overeng/meters'
import { darkMeterTheme, frameBlock, heapBlock, jankBlock, lightMeterTheme } from '@overeng/meters/canvas'
import { makeBrowserPlatform } from '@overeng/meters/platform/browser'
import { MetersProvider, MeterStrip } from '@overeng/meters/react'
import { frameSource } from '@overeng/meters/sources/frame'
import { longFramesSource } from '@overeng/meters/sources/long-frames'
import type { LongFrameValue } from '@overeng/meters/sources/long-frames'
import { heapSource } from '@overeng/meters/sources/memory'
import type { HeapMemory } from '@overeng/meters/sources/memory'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { buildIdentity, deploymentId } from 'virtual:build-identity'

import { counters } from './measurement/index.ts'
import { persistedAtom } from '../state/persistence.ts'
import { transportSnapshot } from './transport.ts'

const preferences = AtomRegistry.make()
let defaultPreference = import.meta.env?.DEV ?? false
const visibility = persistedAtom({
  key: 'devbar.visible',
  schema: Schema.NullOr(Schema.Boolean),
  defaultValue: null,
})

/** Shared command palette and keyboard action through the same persistence boundary. */
export const toggleDevBar = (): void => {
  preferences.set(visibility, !(preferences.get(visibility) ?? defaultPreference))
}

export const DevBar = (props: { readonly defaultVisible?: boolean }) => (
  <RegistryContext.Provider value={preferences}>
    <DevBarContent {...props} />
  </RegistryContext.Provider>
)

const styles = stylex.create({
  root: { zIndex: 1000 },
  segment: { whiteSpace: 'nowrap', fontVariantNumeric: 'tabular-nums', fontFamily: 'ui-monospace, monospace' },
  version: { maxWidth: 180, overflow: 'hidden', textOverflow: 'ellipsis' },
  error: { color: '#f87171' },
  panel: { padding: 12, color: 'inherit', fontFamily: 'ui-monospace, monospace' },
  description: { marginTop: 0, marginBottom: 10, lineHeight: 1.5 },
  counters: { whiteSpace: 'pre-wrap', lineHeight: 1.5, fontVariantNumeric: 'tabular-nums' },
})

// Definitions are inert; the one MetersProvider owns the kit's scoped collector lease.
const frames = makeSeries<FpsValue>({ id: 'wf.frames', label: 'Frame rate', unit: 'fps', capacity: 1200 })
const longFrames = makeSeries<LongFrameValue>({ id: 'wf.longFrames', label: 'Long frames', unit: 'ms', capacity: 256 })
const memory = makeSeries<HeapMemory>({ id: 'wf.memory', label: 'JS heap (approx.)', unit: 'bytes', capacity: 120 })
const meters = makeMeters({
  platform: makeBrowserPlatform(),
  sources: [
    frameSource({ id: 'frame', series: frames }),
    longFramesSource({ id: 'long-frames', series: longFrames }),
    heapSource({ id: 'memory', series: memory, everyMs: 1000 }),
  ],
})
const compactBlocks = [
  frameBlock({ id: 'frame', series: frames, widthPx: 80 }),
  jankBlock({ id: 'long-frames', series: longFrames, widthPx: 90 }),
  heapBlock({ id: 'memory', series: memory, widthPx: 110 }),
]
const detailBlocks = [
  frameBlock({ id: 'frame', series: frames, widthPx: 180 }),
  jankBlock({ id: 'long-frames', series: longFrames, widthPx: 180 }),
  heapBlock({ id: 'memory', series: memory, widthPx: 180 }),
]

/** The visibility shortcut outlives panel selection and the collector lease. */
const diagnosticLifetime = Atom.make((get) => {
  if (typeof window === 'undefined') return
  const keydown = (event: KeyboardEvent) => {
    if (!event.defaultPrevented && (event.metaKey || event.ctrlKey) && event.shiftKey && event.code === 'KeyB') {
      event.preventDefault()
      toggleDevBar()
    }
  }
  window.addEventListener('keydown', keydown)
  get.addFinalizer(() => window.removeEventListener('keydown', keydown))
})

const counterSnapshot = Atom.make((get) => {
  const timer = setInterval(() => get.setSelf(counters.snapshot()), 250)
  get.addFinalizer(() => clearInterval(timer))
  return counters.snapshot()
})

const CountersPanel = () => {
  const snapshot = useAtomValue(counterSnapshot)
  const counterText = Object.entries(snapshot)
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, value]) => `${key}  ${value}`)
    .join('\n') || 'No measurement counters recorded.'
  return (
    <div {...stylex.props(styles.panel)}>
      <p {...stylex.props(styles.description)}>
        Counter totals are process-local values recorded by the app measurement instrumentation.
        Transport latency uses completed SDK fetches through response headers.
      </p>
      <p {...stylex.props(styles.description)}>
        {buildIdentity.displayVersion ?? buildIdentity.machineVersion} · Machine: {buildIdentity.machineVersion} · Source:{' '}
        {buildIdentity.sourceKind} · Revision: {buildIdentity.rev ?? 'unavailable'} · Dirty:{' '}
        {buildIdentity.dirty ? 'yes' : 'no'} · Commit timestamp: {buildIdentity.commitTs ?? 'unavailable'} · Build timestamp:{' '}
        {buildIdentity.buildTs ?? 'unavailable'} · Deployment: {deploymentId ?? 'unavailable'}
      </p>
      <pre {...stylex.props(styles.counters)} aria-label="Measurement counters">{counterText}</pre>
    </div>
  )
}

/** Subscribe only the transport slot that consumes each sampled field. */
const WebSocketSegment = () => {
  const live = useAtomValue(transportSnapshot, (snapshot) => snapshot.socketLive)
  return <span {...stylex.props(styles.segment)}>WS {live ? 'live' : 'offline'}</span>
}
const HttpSegment = () => {
  const requests = useAtomValue(transportSnapshot, (snapshot) => snapshot.requests)
  const p50Ms = useAtomValue(transportSnapshot, (snapshot) => snapshot.p50Ms)
  const p95Ms = useAtomValue(transportSnapshot, (snapshot) => snapshot.p95Ms)
  const errors = useAtomValue(transportSnapshot, (snapshot) => snapshot.errors)
  return (
    <span {...stylex.props(styles.segment, errors > 0 && styles.error)} title="Completed fetch latency through response headers, latest 256 requests">
      HTTP {requests} active · p50 {p50Ms.toFixed(0)} · p95 {p95Ms.toFixed(0)}ms · errors {errors}
    </span>
  )
}
const SubscriptionsSegment = () => {
  const subscriptions = useAtomValue(transportSnapshot, (snapshot) => snapshot.subscriptions)
  const cap = useAtomValue(transportSnapshot, (snapshot) => snapshot.subscriptionCap)
  return <span {...stylex.props(styles.segment)}>Subs {subscriptions}/{cap}</span>
}
const MessageRateSegment = () => {
  const rate = useAtomValue(transportSnapshot, (snapshot) => snapshot.messagesPerSecond)
  return <span {...stylex.props(styles.segment)}>{rate.toFixed(1)} msg/s</span>
}
const segments: readonly DevbarSegment[] = [
  { id: 'version', render: () => (
    <span {...stylex.props(styles.segment, styles.version)} title={buildIdentity.displayVersion ?? buildIdentity.machineVersion}>
      {buildIdentity.displayVersion ?? buildIdentity.machineVersion}{buildIdentity.dirty ? ' · dirty' : ''}
    </span>
  ) },
  { id: 'websocket', render: () => <WebSocketSegment /> },
  { id: 'http', render: () => <HttpSegment /> },
  { id: 'subscriptions', render: () => <SubscriptionsSegment /> },
  { id: 'message-rate', render: () => <MessageRateSegment /> },
]

const subscribeScheme = (notify: () => void): (() => void) => {
  if (typeof document === 'undefined') return () => {}
  const observer = new MutationObserver(notify)
  observer.observe(document.documentElement, { attributes: true, subtree: true, attributeFilter: ['data-scheme'] })
  return () => observer.disconnect()
}
const readDarkScheme = (): boolean => {
  if (typeof document === 'undefined') return false
  // The live workbench owns its palette locally, overriding the document's system preference.
  const host = document.querySelector<HTMLElement>('[data-testid="live-agent-workspace"]')
  return (host?.dataset.scheme ?? document.documentElement.dataset.scheme) === 'dark'
}
const readServerScheme = (): boolean => false

const MeterView = ({ dark, expanded = false, onOpenDetail }: {
  readonly dark: boolean
  readonly expanded?: boolean
  readonly onOpenDetail: (selection: { readonly id: string }) => void
}) => {
  const [frozen, setFrozen] = React.useState(false)
  return <MeterStrip meters={meters} blocks={expanded ? detailBlocks : compactBlocks}
    theme={dark ? darkMeterTheme : lightMeterTheme} frozen={frozen} onFrozenChange={setFrozen}
    onOpenDetail={onOpenDetail} heightPx={expanded ? 64 : 28} />
}

const meterDescriptions: Readonly<Record<string, string>> = {
  frame: 'Frame rate is observed rAF timing. The kit calibrates skipped-frame evidence separately; pending or unsupported calibration is not treated as a measured zero.',
  'long-frames': 'Long-animation-frame durations use PerformanceObserver, with an explicitly tagged long-task fallback on browsers without LoAF support. NoSamples means no event has been observed, not a measured zero.',
  memory: 'JS heap is an approximate shared-heap measurement from performance.memory, not total app memory. Unsupported capabilities are shown as n/a, not zero.',
}
const MetersPanel = ({ dark }: { readonly dark: boolean }) => {
  const [detail, setDetail] = React.useState('frame')
  return (
    <div {...stylex.props(styles.panel)}>
      <p {...stylex.props(styles.description)}>Frame, long-frame, and memory histories share one scoped session. Click a meter for its measurement semantics; freeze each strip independently.</p>
      <MeterView dark={dark} expanded onOpenDetail={({ id }) => setDetail(id)} />
      <p {...stylex.props(styles.description)}>{meterDescriptions[detail]}</p>
    </div>
  )
}

const DevBarContent = ({ defaultVisible = import.meta.env?.DEV ?? false }: { readonly defaultVisible?: boolean }) => {
  defaultPreference = defaultVisible
  const shown = useAtomValue(visibility) ?? defaultVisible
  useAtomValue(diagnosticLifetime)
  const dark = React.useSyncExternalStore(subscribeScheme, readDarkScheme, readServerScheme)
  const [openPanel, setOpenPanel] = React.useState<string | undefined>(undefined)
  const panels = React.useMemo<readonly DevbarPanel[]>(() => [
    { id: 'counters', label: 'Counters', render: () => <CountersPanel /> },
    { id: 'meters', label: 'Meters', render: () => <MetersPanel dark={dark} /> },
  ], [dark])
  if (!shown) return null
  return (
    <div data-testid="wf-devbar" {...stylex.props(dark ? darkDevbarTheme : lightDevbarTheme)}>
      <MetersProvider meters={meters}>
        <Devbar panels={panels} segments={segments} openPanel={openPanel} onOpenPanelChange={setOpenPanel}
          strip={<MeterView dark={dark} onOpenDetail={() => setOpenPanel('meters')} />} style={styles.root} />
      </MetersProvider>
    </div>
  )
}
