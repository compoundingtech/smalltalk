import * as stylex from '@stylexjs/stylex'
import { SyncLine } from '../st3-views/SyncLine'
import type { SyncStatus } from '../st3-views/sync-status'
import { surfaceVars as surface, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'

export interface TranscriptSkeletonProps {
  readonly sync: SyncStatus
  readonly now: number
  readonly observedAt: number
  readonly onRetrySync?: () => void
}
/** Initial transcript loading state. Imports neither Markdown, its grammars nor the assistant-ui runtime, so a host can show it while the transcript itself loads. */
export function TranscriptSkeleton({ sync, now, observedAt, onRetrySync }: TranscriptSkeletonProps) {
  return <div data-testid="transcript-placeholder" aria-label="Loading conversation" {...stylex.props(styles.placeholder)}><p role="status">Loading conversation…</p><SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} /><div aria-hidden="true" {...stylex.props(styles.turn)}><div {...stylex.props(styles.skeletonPrompt)} /><div {...stylex.props(styles.skeletonWork)} /><div {...stylex.props(styles.skeletonAnswer)} /></div></div>
}
const styles = stylex.create({
  placeholder: { display: 'flex', flexDirection: 'column', gap: s.lg },
  turn: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  skeletonPrompt: { width: '100%', height: `calc(${t.bodyLeading} + ${s.md})`, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: accent.primary, backgroundColor: surface.rowActive },
  skeletonWork: { height: g.toolRow, width: '30%', borderRadius: r.sm, backgroundColor: surface.rowActive },
  skeletonAnswer: { height: g.resourceCard, width: '80%', borderRadius: r.sm, backgroundColor: surface.rowHover },
})
