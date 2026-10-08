import * as React from 'react'
import { EmbraceRuntimeProvider, Transcript, type WorkLogCall } from '@smalltalk/fractal-ui/assistant-ui'
import { useConversation, useConversationSync, useDataSource, useFeedInterest, useNow } from '../data/react.tsx'
import { mapConversationFeed, transcriptObservedAt, transcriptRuntimeOptions, transcriptSyncStatus } from './conversationTranscript.ts'

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
  // A cold or deep-linked route must follow on mount; the pane is keyed by agent ref, so
  // switching agents releases the previous conversation's demand with this component.
  const interest = React.useMemo(() => source.conversationInterest?.(agentRef), [source, agentRef])
  useFeedInterest({ interest, visible: true })
  const now = useNow()
  const feed = useConversation(agentRef)
  const observation = useConversationSync(agentRef)
  const state = React.useMemo(() => mapConversationFeed(feed, { agentName }), [feed, agentName])
  const options = React.useMemo(
    () => transcriptRuntimeOptions(state._tag === 'Observed' ? state.items : [], state._tag === 'Observed' && state.isRunning),
    [state],
  )
  const retryConversation = source.retryConversation
  return <div style={{ display: 'contents' }}
    data-wf-unavailable={state._tag === 'Unavailable' ? state.classification : undefined}
    data-wf-unavailable-code={state._tag === 'Unavailable' ? state.code : undefined}>
    <EmbraceRuntimeProvider key={agentRef} options={options}>
    <Transcript
      turns={state._tag === 'Observed' ? state.turns : []}
      title={agentName}
      sync={transcriptSyncStatus(observation, feed, now)}
      now={now}
      observedAt={transcriptObservedAt(observation, now)}
      onOpenTool={onOpenTool}
      onRetrySync={retryConversation === undefined ? undefined : () => retryConversation(agentRef)}
      {...(state._tag === 'Unavailable' ? { availability: state.availability } : {})}
      {...(state._tag === 'Observed' && state.history._tag === 'HasOlder' ? { history: state.history } : {})}
      {...(state._tag === 'Observed' && state.emptyState !== undefined ? { emptyState: state.emptyState } : {})}
    />
    </EmbraceRuntimeProvider>
  </div>
}
