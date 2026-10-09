import * as React from 'react'
import { useAtomValue } from '@effect/atom-react'
import { systemEventsPreference } from './conversationPreferences.ts'
import { EmbraceComposer, EmbraceRuntimeProvider, Transcript, type WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import { useConversation, useConversationSync, useDataSource, useFeedInterest, useGrants, useNow } from '../data/react.tsx'
import { createConversationTranscript, openableImageUrl, transcriptObservedAt, transcriptSyncStatus, type ConversationTranscriptState } from './conversationTranscript.ts'
import { composerSendBinding, type SendRefusal } from './composerSend.ts'
import { spaceVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { LiveAgentTodos } from '../conversation/todos/AgentTodos.tsx'
import type { UxTelemetry } from '../telemetry/ux.ts'
import type { ConversationPage, Feed } from '../data/source.ts'

/** The follow owns content and the kit owns its presentation. The display-contents host
 * boundary observes asynchronous runtime adoption without inventing a kit commit hook.
 * Visibility lives only in this wrapper: toggling it never re-renders the memoized content,
 * so a retained pane is revealed without re-importing its runtime messages. */
export const ConversationPane = ({
  agentRef,
  agentName,
  onOpenTool,
  ux,
  visible = true,
}: {
  readonly agentRef: string
  /** Roster display name: the composition header and assistant sender captions. */
  readonly agentName: string
  /** Host-owned detail surface for a tool call opened from the transcript. */
  readonly onOpenTool: (call: WorkLogCall) => void
  readonly ux?: UxTelemetry
  /** A retained hidden pane keeps its DOM but releases visible follow demand (setVisible false). */
  readonly visible?: boolean
}) => {
  const source = useDataSource()
  const showSystemEvents = useAtomValue(systemEventsPreference)
  // Visible demand is an effect: hiding unmounts the interest, whose finalizer marks the
  // SDK follow invisible and therefore evictable, while the retained DOM stays mounted.
  const interest = React.useMemo(() => source.conversationInterest?.(agentRef), [source, agentRef])
  useFeedInterest({ interest, visible })
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
    <ConversationContent agentRef={agentRef} agentName={agentName} onOpenTool={onOpenTool} feed={feed} state={state} />
  </div>
}

const ConversationContent = React.memo(function ConversationContent({ agentRef, agentName, onOpenTool, feed, state }: {
  readonly agentRef: string
  readonly agentName: string
  readonly onOpenTool: (call: WorkLogCall) => void
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
  const retryConversation = source.retryConversation
  return <EmbraceRuntimeProvider key={agentRef} options={{ ...binding.runtime, isRunning: state._tag === 'Observed' && state.isRunning }}>
    {/* Bound the 100%-height kit frame to the space left above the composer. */}
    <div data-testid="conversation-history-host" style={{ flex: '1 1 0', minHeight: 0, minWidth: 0, overflow: 'hidden' }}>
    <Transcript
      turns={state._tag === 'Observed' ? state.turns : []}
      scrollToBottomKey={feed._tag === 'Observed' ? feed.value.lastSendId : undefined}
      title={agentName}
      sync={transcriptSyncStatus(observation, feed, now)}
      now={now}
      observedAt={transcriptObservedAt(observation, now)}
      onOpenTool={onOpenTool}
      onRetrySync={state._tag === 'Unavailable' || retryConversation === undefined ? undefined : () => retryConversation(agentRef)}
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
    <LiveAgentTodos agentRef={agentRef} />
    {/* An unreadable conversation keeps its composer and draft; the binding names why sending waits. */}
    <div data-testid="conversation-composer-dock" style={{ flexShrink: 0, paddingBottom: spaceVars.lg }}>
      <EmbraceComposer variant="C1" readingColumn disabledReason={binding.disabledReason} />
    </div>
  </EmbraceRuntimeProvider>
})
