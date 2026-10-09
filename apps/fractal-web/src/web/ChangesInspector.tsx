import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { DiffPanel } from '../../../../packages/fractal-ui/src/assistant-ui/composition/DiffPanel.tsx'
import { ResourceCardV1, ResourceChipV1 } from '@smalltalk/fractal-ui/assistant-ui/resources'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useConversation, useDataSource, useFeedInterest } from '../data/react.tsx'
import type { Feed, ConversationPage } from '../data/source.ts'
import { capturedChanges, capturedProvenance, selectedCapturedChange } from './capturedChanges.ts'

/** Shares the retained public conversation feed and acquires demand while the inspector is visible. */
export function ChangesInspector({ agentRef, width }: { readonly agentRef: string; readonly width: number }) {
  const source = useDataSource()
  const interest = React.useMemo(() => source.conversationInterest?.(agentRef), [source, agentRef])
  useFeedInterest({ interest, visible: true })
  const feed = useConversation(agentRef)
  return <CapturedChangesView key={agentRef} feed={feed} width={width} />
}

/** Local card/chip selection never fetches a private endpoint or implies current repository state. */
export function CapturedChangesView({ feed, width }: { readonly feed: Feed<ConversationPage>; readonly width: number }) {
  const page = feed._tag === 'Observed' ? feed.value : undefined
  const changes = React.useMemo(() => page === undefined ? [] : capturedChanges(page), [page])
  const [selected, setSelected] = React.useState<string | undefined>()
  const choice = selectedCapturedChange({ changes, id: selected })
  const lines = choice?.resource.files[0]?.lines
  return <aside aria-label="Changes" style={{ width }} {...stylex.props(styles.root)}>
    <p role="status" {...stylex.props(styles.scope)}>{feed._tag === 'Waiting' ? 'Waiting for captured changes.' : feed._tag === 'Unavailable' ? 'Captured changes unavailable: ' + feed.detail : (feed.freshness === 'stale' ? 'Last verified captured changes · ' : '') + 'Successful-tool captures · loaded transcript window' + (page?.hasOlder ? ' · older history not shown' : '')}{feed._tag === 'Observed' && feed.error ? ' · ' + feed.error.detail : ''}</p>
    {choice === undefined ? <p {...stylex.props(styles.empty)}>{feed._tag === 'Observed' ? 'No successful captured change content in this transcript window. Failed tool results are outside this inspector; failure does not establish that files were unchanged.' : 'Changes appear only after verified transcript observations.'}</p> : <>
      <section aria-label="Captured change resources" {...stylex.props(styles.cards)}>{changes.map(change => <div key={change.id} data-captured-change={change.id} aria-current={change.id === choice.id ? 'true' : undefined} {...stylex.props(styles.card)}>
        <p {...stylex.props(styles.detail)}>{capturedProvenance(change)}</p>
        <p {...stylex.props(styles.detail)}>{change.resource.detail}</p>
        <ResourceCardV1 resource={change.resource} variant="RC-1" onOpen={() => setSelected(change.id)} />
        <ResourceChipV1 resource={change.resource} file={change.resource.files[0]!} onOpen={() => setSelected(change.id)} />
      </div>)}</section>
      <div {...stylex.props(styles.diff)}><DiffPanel key={choice.id} open width={width} title="Captured change" diff={lines?._tag === 'Known' ? lines.value : undefined} path={choice.diff.path} added={choice.diff.added} removed={choice.diff.removed} /></div>
    </>}
  </aside>
}

const styles = stylex.create({
  root: { flexShrink: 0, display: 'flex', flexDirection: 'column', minHeight: 0, overflow: 'hidden', backgroundColor: c.canvas, color: c.fg },
  scope: { flexShrink: 0, margin: 0, padding: s.lg, fontSize: t.metaSize, color: c.fgMuted },
  empty: { margin: 0, padding: s.lg, fontSize: t.metaSize, color: c.fgMuted },
  cards: { flexShrink: 0, maxHeight: '35%', overflowY: 'auto', padding: s.md, display: 'flex', flexDirection: 'column', gap: s.md, minHeight: 0 },
  card: { display: 'flex', flexDirection: 'column', gap: s.sm, minWidth: 0 },
  detail: { margin: 0, overflowWrap: 'anywhere', fontSize: t.denseSize, color: c.fgMuted },
  diff: { flexGrow: 1, minHeight: 0, overflow: 'hidden' },
})
