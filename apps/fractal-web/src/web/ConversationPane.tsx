import { EmptyState, Note } from '@smalltalk/fractal-ui'
import { EmbraceRuntimeProvider, EmbraceThread as Transcript } from '@smalltalk/fractal-ui/assistant-ui'
import { SyncLine } from '@smalltalk/fractal-ui/assistant-ui/sync'
import { useConversation, useConversationSync, useNow } from '../data/react.tsx'
import { mapConversationFeed, mapConversationSync, transcriptRuntimeOptions } from './conversationTranscript.ts'

/** The selected follow owns content and sync; the kit owns every rendered element. */
export const ConversationPane = ({ agentRef }: { readonly agentRef: string }) => {
  const feed = mapConversationFeed(useConversation(agentRef))
  const sync = mapConversationSync(useConversationSync(agentRef), useNow())
  return <>
    {sync === undefined ? null : <SyncLine {...sync} />}
    {feed._tag === 'Waiting' ? <Note>Waiting for the first conversation observation.</Note>
      : feed._tag === 'Unavailable' ? <EmptyState title={feed.title} hint={feed.detail} />
      : <>
        {feed.notice === undefined ? null : <Note tone="warning">{feed.notice}</Note>}
        {feed.hasOlder ? <Note>Earlier conversation entries are not included in this page.</Note> : null}
        {feed.filteredEmpty ? <Note>This page contains no displayable conversation entries.</Note>
          : <EmbraceRuntimeProvider key={agentRef} options={transcriptRuntimeOptions(feed.items)}>
            <Transcript items={feed.items} composer={false} readingColumn />
          </EmbraceRuntimeProvider>}
      </>}
  </>
}
