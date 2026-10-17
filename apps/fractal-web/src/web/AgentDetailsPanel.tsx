import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Dialog, Heading, Modal, ModalOverlay } from 'react-aria-components'
import { RegistryContext } from '@effect/atom-react'
import { DetailsPane, Workbench } from '@smalltalk/fractal-ui/assistant-ui'
import type { DetailsLayoutIntent, DetailsVariants, WorkbenchLayout, WorkbenchResources } from '@smalltalk/fractal-ui/assistant-ui'
import type { SyncStatus } from '../../../../packages/fractal-ui/src/assistant-ui/st3-views/sync-status.ts'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useGatewaySync } from '../data/react.tsx'
import { useClockTime, useClockWake } from '@smalltalk/fractal-ui/human-time/clock'
import { syncLineNextChange } from '@smalltalk/fractal-ui/assistant-ui/sync-status'
import { earliestChange } from '@smalltalk/fractal-ui/human-time/format'
import { useDetailsSections, useDetailsSupplement } from '../extensions/build.ts'
import { useNativeDetails } from './useNativeDetails.ts'
import { detailsWithSupplement } from './detailsSupplement.ts'
import { ChangesInspector } from './ChangesInspector.tsx'

/** Deployed defaults, individually replaceable without ratifying exploration alternatives. */
export const liveDetailsVariants: DetailsVariants = {
  overview: 'O2', context: 'C3', spend: 'S2', resources: 'R1', empty: 'E3',
}
const resources: WorkbenchResources = {
  threads: new Map(), diffs: new Map(), terminals: { state: 'unknown' },
}

export interface AgentDetailsPanelProps {
  readonly agentRef: string
  readonly width: number
  readonly scheme: 'dark' | 'light'
  readonly onClose: () => void
  readonly onOpen: (uri: string, split: boolean) => void
  readonly variants?: DetailsVariants
}

/** The real kit tab-group owns Details' status/action slot; hidden tabs release reader demand. */
export function AgentDetailsPanel({ agentRef, width, scheme, onClose, onOpen, variants = liveDetailsVariants }: AgentDetailsPanelProps) {
  const sourceRegistry = React.useContext(RegistryContext)
  const layout = React.useMemo<WorkbenchLayout>(() => ({ kind: 'group', tabs: [
    { uri: `details:${agentRef}`, view: 'details' },
    { uri: `changes:${agentRef}` },
  ] }), [agentRef])
  const detailsKey = `details:${agentRef} view=details`
  const [selected, setSelected] = React.useState(detailsKey)
  const [intent, setIntent] = React.useState<DetailsLayoutIntent>({ hiddenSections: [] })
  return <aside aria-label="Agent inspector" data-testid="agent-details-panel" style={{ width }} {...stylex.props(styles.root)}>
    <div {...stylex.props(styles.closeBar)}><Button aria-label="Close agent inspector" onPress={onClose} {...stylex.props(styles.close)}>Close</Button></div>
    <Workbench layout={layout} resources={resources} workspaceId={`live-inspector:${agentRef}`} scheme={scheme}
      appearance={{ width: 'W1', header: 'H3', chrome: 'P1', dropZones: 'D3' }}
      landmarkContext="Agent inspector" focusedPaneKey={selected} onPaneSelect={setSelected}
      describePane={pane => ({ title: pane.view === 'details' ? 'Details' : 'Changes' })}
      renderPane={pane => <RegistryContext.Provider value={sourceRegistry}>
        {pane.view === 'details'
          ? <NativeDetailsPane agentRef={agentRef} variants={variants} intent={intent} onIntentChange={setIntent} onOpen={onOpen} />
          : <ChangesInspector agentRef={agentRef} width={width} />}
      </RegistryContext.Provider>}
    />
  </aside>
}

function NativeDetailsPane({ agentRef, variants, intent, onIntentChange, onOpen }: {
  readonly agentRef: string
  readonly variants: DetailsVariants
  readonly intent: DetailsLayoutIntent
  readonly onIntentChange: (intent: DetailsLayoutIntent) => void
  readonly onOpen: (uri: string, split: boolean) => void
}) {
  const native = useNativeDetails({ agentRef })
  const protectedSections = useDetailsSections({ agentRef })
  const suppliedSections = native.sections.map(section => protectedSections.find(contribution => contribution.title === section.title) ?? section)
  const [inspectedUri, setInspectedUri] = React.useState<string>()
  const portalContainer = React.useRef<HTMLDivElement>(null)
  const resource = suppliedSections.flatMap(section => section.items._tag === 'Known' ? section.items.value : []).find(item => item.uri === inspectedUri)
  const now = useClockTime()
  const [requestedAt] = React.useState(now)
  const gateway = useGatewaySync()
  const scopedRoot = native.trace.nativeSessionId._tag === 'Known' ? native.trace.nativeSessionId.value : undefined
  const supplement = useDetailsSupplement({
    agentRef,
    ...(scopedRoot === undefined ? {} : { nativeSessionId: scopedRoot }),
    canRefresh: native.nativeSessionId !== undefined,
    nativeRootPending: native.nativeRootPending,
    ...(native.incarnationId === undefined ? {} : { incarnationId: native.incarnationId }),
    ...(native.host === undefined ? {} : { host: native.host }),
    ...(native.harness === undefined ? {} : { harness: native.harness }),
  })
  const data = detailsWithSupplement({
    trace: native.trace, overview: native.overview, supplement,
    ...(native.harness === undefined ? {} : { harness: native.harness }),
    ...(scopedRoot === undefined ? {} : { nativeSessionId: scopedRoot }),
  })
  const traced = inspectedUri?.startsWith('session:') && data.trace.subagents._tag === 'Known'
    ? data.trace.subagents.value.find(child => `session:${child.sessionId}` === inspectedUri) : undefined
  const inspection = resource !== undefined ? { label: resource.card.label, detail: resource.card.detail }
    : traced === undefined ? undefined : {
      label: traced.label._tag === 'Known' ? traced.label.value : 'Traced descendant',
      detail: `Role ${traced.label._tag === 'Known' ? traced.label.value : 'unreported'} · Session ${traced.sessionId} · ${traced.requests} reported requests · $${traced.usd.toFixed(2)} · ${traced.model._tag === 'Known' ? traced.model.value : 'model unreported'} · 7d as reported; parent and first/last timestamps are not supplied`,
    }
  const status: SyncStatus = native.syncStatus ?? (native.state === 'loading'
    ? { _tag: 'Requested', since: requestedAt }
    : native.state === 'error'
      ? { _tag: 'Failed', cause: { _tag: 'Unknown' } }
      : native.stale
        ? gateway?.status._tag === 'Stale' ? gateway.status : { _tag: 'Stale', reason: { _tag: 'Unknown' }, ...(native.observedAt === undefined ? {} : { lastLiveAt: native.observedAt }) }
        : gateway?.status ?? { _tag: 'Live', since: native.observedAt ?? requestedAt })
  const receivedAt = supplement._tag === 'Observed' && supplement.freshness === 'stale' ? supplement.value.receivedAtMs : undefined
  useClockWake(earliestChange(now, [
    syncLineNextChange({ status, label: 'agent details', now, observedAt: native.observedAt ?? requestedAt }),
    receivedAt === undefined ? undefined : receivedAt + (Math.floor(Math.max(0, now - receivedAt) / 1000) + 1) * 1000,
  ]))
  return <div ref={portalContainer} {...stylex.props(styles.content)}>
    {native.harness === 'omp' && <p role="status" data-details-supplement-state={supplement._tag} {...stylex.props(styles.coverage)}>
      {supplement._tag === 'Waiting' ? 'Descendant observations are loading independently of native session facts.'
        : supplement._tag === 'Unavailable' ? `Descendant observations unavailable: ${supplement.detail}`
          : supplement.freshness === 'stale' ? `Last good descendant read ${supplement.value.receivedAtMs === undefined ? '(age unreported)' : `${Math.max(0, Math.floor((now - supplement.value.receivedAtMs) / 1000))}s ago`} · retained/stale. ${supplement.error?.detail ?? 'Last successful read retained while revalidating.'} · Coverage is window-global monetary attribution, not root completeness.` : 'Descendant ledger coverage · 7d as reported. Percentages describe window-global monetary attribution, not root completeness; exact cut bounds and producer observation time are not supplied.'}
    </p>}
    {data.accountingNote !== undefined && <p data-details-accounting-note {...stylex.props(styles.coverage)}>{data.accountingNote}</p>}
    <DetailsPane overview={data.overview} sections={suppliedSections} trace={data.trace} now={now}
      variants={variants} state={native.state} intent={intent} onIntentChange={onIntentChange}
      onOpen={(uri, split) => {
        if (uri.startsWith('details:') || uri.startsWith('session:')) setInspectedUri(uri)
        else onOpen(uri, split)
      }} onRetry={native.onRetry} syncStatus={status}
      hasPriorContent={native.state !== 'loading' && native.state !== 'error'}
      {...(native.observedAt === undefined ? {} : { observedAt: native.observedAt })}
    />
    <ModalOverlay isOpen={inspection !== undefined} onOpenChange={open => { if (!open) setInspectedUri(undefined) }} isDismissable UNSTABLE_portalContainer={portalContainer.current ?? undefined} {...stylex.props(styles.overlay)}>
      <Modal {...stylex.props(styles.modal)}><Dialog aria-label="Reported resource details" {...stylex.props(styles.inspection)}>
        <Heading slot="title">{inspection?.label}</Heading>
        <p {...stylex.props(styles.inspectionText)}>{inspection?.detail}</p>
        <p {...stylex.props(styles.inspectionText)}>Read-only reported metadata; no native navigation is supplied for this resource.</p>
        <Button onPress={() => setInspectedUri(undefined)}>Close</Button>
      </Dialog></Modal>
    </ModalOverlay>
  </div>
}

const styles = stylex.create({
  root: { flexShrink: 0, display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, overflow: 'hidden', backgroundColor: c.canvas, color: c.fg },
  closeBar: { display: 'flex', justifyContent: 'flex-end', flexShrink: 0, padding: s.xs },
  close: { fontSize: t.metaSize, color: c.fgMuted, backgroundColor: 'transparent', borderWidth: 0, padding: s.xs },
  content: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, flexGrow: 1, overflowY: 'auto' },
  coverage: { margin: 0, padding: s.sm, fontSize: t.metaSize, color: c.fgMuted, overflowWrap: 'anywhere', flexShrink: 0 },
  overlay: { position: 'fixed', inset: 0, zIndex: 100, display: 'flex', alignItems: 'center', justifyContent: 'center', backgroundColor: 'rgba(0, 0, 0, 0.4)' },
  modal: { maxWidth: 'min(520px, 90vw)', maxHeight: '90vh', overflowY: 'auto', backgroundColor: c.raised, color: c.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  inspection: { backgroundColor: c.canvas, color: c.fg, padding: s.lg, outline: 'none' },
  inspectionText: { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
})
