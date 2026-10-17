import * as React from 'react'
import { useAtomValue } from '@effect/atom-react'
import { systemEventsPreference } from './conversationPreferences.ts'
import { EmbraceComposer } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceComposer.tsx'
import { EmbraceRuntimeProvider } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx'
import { Transcript } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx'
import type { WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import { useConversation, useConversationSync, useDataSource, useFeedInterest, useGrants, useNow } from '../data/react.tsx'
import { createConversationTranscript, openableImageUrl, transcriptObservedAt, transcriptSyncStatus, type ConversationTranscriptState } from './conversationTranscript.ts'
import { composerSendBinding, type SendRefusal } from './composerSend.ts'
import { spaceVars, geometryVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { LiveAgentTodos } from '../conversation/todos/AgentTodos.tsx'
import { LiveAgentAlerts } from '../conversation/alerts/AgentAlerts.tsx'
import { DOCUMENT_REF } from '../documents/document-port.ts'
import { documentReferences } from '../documents/DocumentLink.tsx'
import type { UxTelemetry } from '../telemetry/ux.ts'
import type { ConversationPage, Feed } from '../data/source.ts'
import type * as Atom from 'effect/reactivity/Atom'

/** The follow owns content and the kit owns its presentation. The display-contents host
 * boundary observes asynchronous runtime adoption without inventing a kit commit hook.
 * Visibility lives only in this wrapper: toggling it never re-renders the memoized content,
 * so a retained pane is revealed without re-importing its runtime messages. */
export const ConversationPane = ({
  agentRef,
  agentName,
  onOpenTool,
  onOpenDocument,
  ux,
  visible = true,
  interest: shellInterest,
}: {
  readonly agentRef: string
  /** Roster display name: the composition header and assistant sender captions. */
  readonly agentName: string
  /** Host-owned detail surface for a tool call opened from the transcript. */
  readonly onOpenTool: (call: WorkLogCall) => void
  /** Opens a `doc/…` named in the conversation; stable, so the memoized transcript keeps still. */
  readonly onOpenDocument?: (ref: string) => void
  readonly ux?: UxTelemetry
  /** A retained hidden pane keeps its DOM but releases visible follow demand (setVisible false). */
  readonly visible?: boolean
  /** Already acquired by the shell before this module loaded; standalone panes acquire their own. */
  readonly interest?: Atom.Atom<void>
}) => {
  const source = useDataSource()
  const showSystemEvents = useAtomValue(systemEventsPreference)
  // The shell owns early demand; a standalone pane owns the same source interest itself.
  const interest = React.useMemo(() => shellInterest ?? source.conversationInterest?.(agentRef), [shellInterest, source, agentRef])
  useFeedInterest({ interest, visible: shellInterest === undefined && visible })
  const feed = useConversation(agentRef)
  const projectTranscript = React.useMemo(createConversationTranscript, [])
  const state = projectTranscript(feed, { agentName, showSystemEvents })
  const observed = state._tag === 'Observed'
  const hasTurns = observed && state.turns.length > 0
  const boundary = React.useRef<HTMLDivElement>(null)
  React.useLayoutEffect(() => {
    const node = boundary.current
    if (node === null) return
    // Only the small composer is inert. Locking the entire transcript would restyle every
    // row in the switch frame. The boundary hides covered content from assistive technology.
    const composer = node.querySelector('textarea')?.closest('form')
    if (composer !== null && composer !== undefined) composer.inert = !visible
    const focused = node.ownerDocument.activeElement
    if (!visible && focused instanceof HTMLElement && node.contains(focused)) focused.blur()
  }, [visible, state._tag])
  React.useLayoutEffect(() => {
    const node = boundary.current
    if (node === null || ux === undefined || !visible || !observed) return
    let cancelPaint: (() => void) | undefined
    const committed = () => {
      if (cancelPaint !== undefined) return
      const lane = node.querySelector('[data-testid="transcript-scroll"]')
      if (lane === null || lane.querySelector('[data-testid="transcript-placeholder"]') !== null) return
      if (lane.querySelector(hasTurns ? '[data-testid="transcript-turn"]' : '[data-testid="transcript-empty"]') === null) return
      cancelPaint = ux.transcriptCommitted(agentRef)
      observer.disconnect()
    }
    // The external-store runtime adopts messages after the parent's layout commit.
    // Observe only this visible pane, and stop after its first real content commit.
    const observer = new MutationObserver(committed)
    observer.observe(node, { childList: true, subtree: true })
    committed()
    return () => { observer.disconnect(); cancelPaint?.() }
  }, [agentRef, observed, hasTurns, ux, visible])
  // Diagnostics stay in data-wf-* attributes; the kit renders only the fixed unavailable copy.
  return <div ref={boundary} style={{ display: 'contents' }} aria-hidden={!visible}
    data-wf-unavailable={state._tag === 'Unavailable' ? state.classification : undefined}
    data-wf-unavailable-code={state._tag === 'Unavailable' ? state.code : undefined}
    onFocusCapture={event => { if (!visible) event.target.blur() }}
    onKeyDownCapture={event => { if (!visible) { event.preventDefault(); event.stopPropagation() } }}
    onClickCapture={event => { if (!visible) { event.preventDefault(); event.stopPropagation() } }}>
    <ConversationContent agentRef={agentRef} agentName={agentName} onOpenTool={onOpenTool} onOpenDocument={onOpenDocument} feed={feed} state={state} ux={ux} />
  </div>
}

const ConversationContent = React.memo(function ConversationContent({ agentRef, agentName, onOpenTool, onOpenDocument, feed, state, ux }: {
  readonly ux?: UxTelemetry
  readonly agentRef: string
  readonly agentName: string
  readonly onOpenTool: (call: WorkLogCall) => void
  readonly onOpenDocument?: (ref: string) => void
  readonly feed: Feed<ConversationPage>
  readonly state: ConversationTranscriptState
}) {
  const source = useDataSource()
  const now = useNow()
  const observation = useConversationSync(agentRef)
  const grants = useGrants()
  const [refusal, setRefusal] = React.useState<SendRefusal>()
  const binding = composerSendBinding({
    source, agentRef, grants,
    readable: state._tag === 'Observed',
    items: state._tag === 'Observed' ? state.items : [],
    refusal, onRefused: setRefusal,
  })
  // The composer's Stop (and Esc) interrupts the agent's turn, as Interrupt in its actions does.
  // A refusal belongs to the run it answered (the running turn's id): it shows beside Stop while that run
  // lasts, clears when the run ends or another starts, and a late answer for an older run is dropped.
  const agentControls = source.agentControls
  const running = state._tag === 'Observed' && state.isRunning
  const runId = state._tag === 'Observed' && state.isRunning ? state.turns.findLast(turn => turn.work.running)?.id ?? '' : undefined
  const [stopRefusal, setStopRefusal] = React.useState<{ readonly runId: string; readonly detail: string }>()
  if (stopRefusal !== undefined && stopRefusal.runId !== runId) setStopRefusal(undefined)
  // The run on screen when an interrupt answer lands; a closure would hold the run of the press.
  const currentRun = React.useRef(runId)
  React.useLayoutEffect(() => { currentRun.current = runId }, [runId])
  const onCancel = React.useMemo(() => agentControls === undefined ? undefined : async () => {
    setStopRefusal(undefined)
    const result = await agentControls.interrupt(agentRef)
    // An answer for an older run must not touch the current run's refusal.
    if (result._tag === 'Refused' && runId !== undefined && currentRun.current === runId) setStopRefusal({ runId, detail: result.detail })
  }, [agentControls, agentRef, runId])
  const retryConversation = source.retryConversation
  const renderInlineReference = React.useMemo(() => onOpenDocument === undefined ? undefined : documentReferences(onOpenDocument), [onOpenDocument])
  // A Markdown link to a `doc/…` opens it in a tab instead of following a relative URL.
  const followDocumentLinks = (event: React.MouseEvent) => {
    const href = event.target instanceof Element ? event.target.closest('a[href]')?.getAttribute('href') ?? '' : ''
    if (onOpenDocument === undefined || !DOCUMENT_REF.test(href)) return
    event.preventDefault()
    event.stopPropagation()
    onOpenDocument(href)
  }
  return <EmbraceRuntimeProvider key={agentRef} options={{ ...binding.runtime, isRunning: running, ...(onCancel === undefined ? {} : { onCancel }) }}>
    {/* Bound the 100%-height kit frame to the space left above the composer. */}
    <div data-testid="conversation-history-host" onClickCapture={followDocumentLinks} style={{ flex: '1 1 0', minHeight: 0, minWidth: 0, overflow: 'hidden' }}>
    <Transcript
      viewportKey={agentRef}
      turns={state._tag === 'Observed' ? state.turns : []}
      scrollToBottomKey={feed._tag === 'Observed' ? feed.value.lastSendId : undefined}
      title={agentName}
      loadingState={<div data-testid="transcript-placeholder" role="status" aria-label="Loading conversation" />}
      sync={transcriptSyncStatus(observation, feed, now)}
      now={now}
      observedAt={transcriptObservedAt(observation, now)}
      onOpenTool={onOpenTool}
      onRetrySync={state._tag === 'Unavailable' || retryConversation === undefined ? undefined : () => retryConversation(agentRef)}
      {...(renderInlineReference === undefined ? {} : { renderInlineReference })}
      onLoadImage={(src) => { const url = openableImageUrl(src); if (url !== undefined) window.open(url, '_blank', 'noopener,noreferrer') }}
      onRetrySend={grants.messageSend === 'granted' ? (id) => {
        if (feed._tag !== 'Observed') return
        const item = feed.value.items.find(item => item.id === id)
        if (item !== undefined) void binding.retry(item)
      } : undefined}
      {...(state._tag === 'Unavailable' ? { availability: {
        ...state.availability,
        // Preserve the follow-retry policy; the unavailable body owns its single recovery action.
        ...(retryConversation === undefined ? {} : { action: { label: 'Try again', onPress: () => retryConversation(agentRef) } }),
      } } : {})}
      {...(state._tag === 'Observed' && state.history._tag === 'HasOlder' ? { history: state.history } : {})}
      {...(state._tag === 'Observed' && state.emptyState !== undefined ? { emptyState: state.emptyState } : {})}
    />
    </div>
    <LiveAgentAlerts agentRef={agentRef} items={state._tag === 'Observed' ? state.items : []} ux={ux} />
    <LiveAgentTodos agentRef={agentRef} />
    {/* An unreadable conversation keeps its composer and draft; the binding names why sending waits. */}
    <div data-testid="conversation-composer-dock" style={{ flexShrink: 0, display: 'flex', flexDirection: 'column', justifyContent: 'flex-end', minHeight: `calc(${geometryVars.controlLg} + ${geometryVars.controlMd} + 3 * ${spaceVars.md} + 2 * ${geometryVars.hairline} + ${spaceVars.lg})`, paddingBottom: spaceVars.lg }}>
      <EmbraceComposer variant="C1" readingColumn disabledReason={binding.disabledReason} {...(stopRefusal === undefined ? {} : { stopRefusal: stopRefusal.detail })} />
    </div>
  </EmbraceRuntimeProvider>
})
