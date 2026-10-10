import { toolDiffs, type ToolDiff } from '../../../../packages/fractal-ui/src/assistant-ui/embrace-tool-preview.ts'
import type { ResourceData } from '@smalltalk/fractal-ui/assistant-ui/resources'
import type { ToolCallItem } from '../conversation/model.ts'
import type { ConversationPage } from '../data/source.ts'

export interface CapturedChange {
  readonly id: string
  readonly provenance: Pick<ToolCallItem, 'sender' | 'name' | 'callId' | 'at'>
  readonly diff: ToolDiff
  readonly resource: ResourceData
}

/** Successful tool captures from the public conversation window, never current-worktree totals. */
export const capturedChanges = (page: ConversationPage): readonly CapturedChange[] => page.items.flatMap(item => {
  if (item._tag !== 'ToolCall' || item.status !== 'success' || item.result === undefined || item.result.isError) return []
  return toolDiffs(item.input, item.result.content).flatMap((diff, index) => {
    if (diff.path === '' || diff.lines.length === 0 || diff.added + diff.removed === 0) return []
    const lines = diff.lines.map(line => line.kind === 'added' ? '+' + line.text : line.kind === 'removed' ? '-' + line.text : line.kind === 'hunk' ? line.text : ' ' + line.text)
    return [{
      id: JSON.stringify([item.id, diff.path, index]),
      provenance: { sender: item.sender, name: item.name, callId: item.callId, at: item.at },
      diff,
      resource: {
        kind: 'diff' as const, label: diff.path, fileCount: { _tag: 'Known' as const, value: 1 },
        detail: (diff.excerpt ? 'Captured successful-tool edited excerpt; counts cover shown lines, not current repository contents.' : 'Captured successful-tool patch; counts cover the recorded patch, not current repository contents.') +
          (page.hasOlder ? ' Older transcript history is outside this loaded window.' : ' Changes are scoped to this loaded transcript window.'),
        files: [{ path: diff.path, added: { _tag: 'Known' as const, value: diff.added }, removed: { _tag: 'Known' as const, value: diff.removed }, lines: { _tag: 'Known' as const, value: lines } }],
      },
    }]
  })
})

/** A local selection cannot address content outside the current authoritative window. */
export const selectedCapturedChange = ({ changes, id }: { readonly changes: readonly CapturedChange[]; readonly id: string | undefined }): CapturedChange | undefined =>
  changes.find(change => change.id === id) ?? changes.at(-1)

/** Only reported participant/call identity; the selected agent is not an attribution fallback. */
export const capturedProvenance = ({ provenance }: CapturedChange): string => {
  const sender = provenance.sender
  const participant = sender === undefined ? 'Reported sender unknown' :
    'Reported sender: ' + sender.label + ' (' + sender.kind + ')' + (sender.via === undefined ? '' : ' via ' + sender.via)
  return participant + ' · Tool: ' + provenance.name + ' · Call: ' + provenance.callId + ' · Reported at: ' + provenance.at
}
