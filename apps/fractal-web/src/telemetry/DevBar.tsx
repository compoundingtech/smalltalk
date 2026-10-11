import { RegistryContext, useAtomValue } from '@effect/atom-react'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { Button } from 'react-aria-components'
import { buildIdentity, deploymentId } from 'virtual:build-identity'

import { counters } from './measurement/index.ts'
import { acquireMeasurementEngine } from './meters.tsx'
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
  root: {
    position: 'fixed',
    bottom: 0,
    left: 0,
    right: 0,
    zIndex: 1000,
    color: '#e5e7eb',
    backgroundColor: '#111827',
    borderTopWidth: 1,
    borderTopStyle: 'solid',
    borderTopColor: '#374151',
    fontFamily: 'ui-monospace, monospace',
    fontSize: 10,
  },
  row: {
    display: 'flex',
    alignItems: 'center',
    height: 24,
    gap: 8,
    paddingInline: 8,
    overflowX: 'auto',
  },
  transport: {
    display: 'flex',
    gap: 12,
    whiteSpace: 'nowrap',
    flexShrink: 0,
    fontVariantNumeric: 'tabular-nums',
  },
  version: {
    flexShrink: 0,
    whiteSpace: 'nowrap',
    maxWidth: 180,
    overflow: 'hidden',
    textOverflow: 'ellipsis',
  },
  error: { color: '#f87171' },
  details: {
    display: 'none',
    padding: 8,
    color: '#ededed',
    backgroundColor: '#030712',
    borderBottom: '1px solid #374151',
    overflowX: 'auto',
  },
  expanded: { display: 'block' },
  trigger: {
    minHeight: 20,
    height: 20,
    paddingBlock: 0,
    paddingInline: 4,
    fontSize: 10,
    flexShrink: 0,
    color: 'inherit',
    backgroundColor: 'transparent',
    borderWidth: 0,
    borderRadius: 4,
    cursor: 'pointer',
  },
  hidden: { display: 'none' },
  description: { marginBottom: 6 },
  counters: { whiteSpace: 'pre-wrap', lineHeight: 1.5, fontVariantNumeric: 'tabular-nums' },
})

const keyboardShortcut = Atom.make((get) => {
  if (typeof window === 'undefined') return
  const keydown = (event: KeyboardEvent) => {
    if (
      !event.defaultPrevented &&
      (event.metaKey || event.ctrlKey) &&
      event.shiftKey &&
      event.code === 'KeyB'
    ) {
      event.preventDefault()
      toggleDevBar()
    }
  }
  window.addEventListener('keydown', keydown)
  get.addFinalizer(() => window.removeEventListener('keydown', keydown))
})

/** Sampled transport diagnostics stay outside the workbench render tree. */
const TransportStatus = () => {
  const transport = useAtomValue(transportSnapshot)
  return (
    <div {...stylex.props(styles.transport)} aria-label="st3 SDK transport">
      <span>st3 SDK · WS {transport.socketLive ? 'live' : 'offline'}</span>
      <span>HTTP {transport.requests} active</span>
      <span title="Completed fetch latency through response headers, latest 256 requests">
        p50 {transport.p50Ms.toFixed(0)} · p95 {transport.p95Ms.toFixed(0)}ms
      </span>
      <span {...stylex.props(transport.errors > 0 && styles.error)}>
        HTTP errors {transport.errors}
      </span>
      <span>Subs {transport.subscriptions}/{transport.subscriptionCap}</span>
      <span>{transport.messagesPerSecond.toFixed(1)} messages/s</span>
    </div>
  )
}

/** Compact status row with a counter snapshot in the expanded panel. */
const DevBarContent = ({
  defaultVisible = import.meta.env?.DEV ?? false,
}: {
  readonly defaultVisible?: boolean
}) => {
  defaultPreference = defaultVisible
  const shown = useAtomValue(visibility) ?? defaultVisible
  const [hovered, setHovered] = React.useState(false)
  const [pinned, setPinned] = React.useState(false)
  const [snapshot, setSnapshot] = React.useState<Readonly<Record<string, number>>>(() => counters.snapshot())
  useAtomValue(keyboardShortcut)
  React.useEffect(() => acquireMeasurementEngine(), [])
  React.useEffect(() => {
    const timer = window.setInterval(() => setSnapshot(counters.snapshot()), 250)
    return () => window.clearInterval(timer)
  }, [])
  const expanded = hovered || pinned
  const counterText = Object.entries(snapshot)
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, value]) => `${key}  ${value}`)
    .join('\n') || 'No measurement counters recorded.'

  return (
    <aside
      hidden={!shown}
      {...stylex.props(styles.root, !shown && styles.hidden)}
      aria-label="Developer transport and performance"
      data-testid="wf-devbar"
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
    >
      <div id="wf-devbar-details" {...stylex.props(styles.details, expanded && styles.expanded)}>
        <p {...stylex.props(styles.description)}>
          Counter totals are process-local values recorded by the app measurement instrumentation.
          Transport latency uses completed SDK fetches through response headers.
        </p>
        <p {...stylex.props(styles.description)}>
          {buildIdentity.displayVersion ?? buildIdentity.machineVersion} · Machine: {buildIdentity.machineVersion} · Source:{' '}
          {buildIdentity.sourceKind} · Revision: {buildIdentity.rev ?? 'unavailable'} · Dirty:{' '}
          {buildIdentity.dirty ? 'yes' : 'no'} · Commit timestamp:{' '}
          {buildIdentity.commitTs ?? 'unavailable'} · Build timestamp:{' '}
          {buildIdentity.buildTs ?? 'unavailable'} · Deployment: {deploymentId ?? 'unavailable'}
        </p>
        <pre {...stylex.props(styles.counters)} aria-label="Measurement counters">{counterText}</pre>
      </div>
      <div {...stylex.props(styles.row)}>
        <Button
          {...stylex.props(styles.trigger)}
          aria-label="Expand developer transport details"
          aria-expanded={expanded}
          aria-controls="wf-devbar-details"
          onPress={() => setPinned(!pinned)}
        >
          wf · {pinned ? '−' : '+'}
        </Button>
        <span {...stylex.props(styles.version)} title={buildIdentity.displayVersion ?? buildIdentity.machineVersion}>
          {buildIdentity.displayVersion ?? buildIdentity.machineVersion}
        </span>
        {buildIdentity.dirty && <span {...stylex.props(styles.transport)}>dirty</span>}
        <TransportStatus />
      </div>
    </aside>
  )
}
