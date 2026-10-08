import * as React from 'react'
import { useAtomValue } from '@effect/atom-react'
import { systemEventsPreference } from './conversationPreferences.ts'
import { EmbraceComposer, EmbraceRuntimeProvider, Transcript, type WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import { useConversation, useConversationSync, useDataSource, useFeedInterest, useGrants, useNow } from '../data/react.tsx'
import { createConversationTranscript, openableImageUrl, transcriptObservedAt, transcriptSyncStatus } from './conversationTranscript.ts'
import { composerSendBinding, type SendRefusal } from './composerSend.ts'
import { spaceVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { LiveAgentTodos } from '../conversation/todos/AgentTodos.tsx'

/** The selected follow owns content; the gated kit composition owns every rendered element,
 * including the first-observation skeleton, the availability state and the older-history row. */
export const ConversationPane = ({
  agentRef,
  agentName,
  onOpenTool,
}: {
  readonly agentRef: string
  /** Roster display name: the composition header and assistant sender captions. */
  readonly agentName: string
  /** Host-owned detail surface for a tool call opened from the transcript. */
  readonly onOpenTool: (call: WorkLogCall) => void
}) => {
  const source = useDataSource()
  const showSystemEvents = useAtomValue(systemEventsPreference)
  // A cold or deep-linked route must follow on mount; the pane is keyed by agent ref, so
  // switching agents releases the previous conversation's demand with this component.
  const interest = React.useMemo(() => source.conversationInterest?.(agentRef), [source, agentRef])
  useFeedInterest({ interest, visible: true })
  const now = useNow()
  const feed = useConversation(agentRef)
  const observation = useConversationSync(agentRef)
  const projectTranscript = React.useMemo(createConversationTranscript, [])
  const state = projectTranscript(feed, { agentName, showSystemEvents })
  const grants = useGrants()
  const [refusal, setRefusal] = React.useState<SendRefusal>()
  const binding = composerSendBinding({
    source, agentRef, grants,
    readable: state._tag === 'Observed',
    items: state._tag === 'Observed' ? state.items : [],
    refusal, onRefused: setRefusal,
  })
  const retryConversation = source.retryConversation
  return <div style={{ display: 'contents' }}
    data-wf-unavailable={state._tag === 'Unavailable' ? state.classification : undefined}
    data-wf-unavailable-code={state._tag === 'Unavailable' ? state.code : undefined}>
    <EmbraceRuntimeProvider key={agentRef} options={{ ...binding.runtime, isRunning: state._tag === 'Observed' && state.isRunning }}>
    {/* Bound the 100%-height kit frame to the space left above the composer. */}
    <div data-testid="conversation-history-host" style={{ flex: '1 1 0', minHeight: 0, minWidth: 0, overflow: 'hidden' }}>
    <Transcript
      turns={state._tag === 'Observed' ? state.turns : []}
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
      <EmbraceComposer variant="C1" disabledReason={binding.disabledReason} />
    </div>
    </EmbraceRuntimeProvider>
  </div>
}
