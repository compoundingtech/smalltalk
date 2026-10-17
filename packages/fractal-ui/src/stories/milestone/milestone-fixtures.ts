/** Synthetic, real-shaped fixtures for the redesign milestone workshop. Every name, host, path and number is invented. */
import type { ConversationItem, Sender, TextItem } from '../../assistant-ui/embrace-data/model'
import type { TranscriptTurn } from '../../assistant-ui/composition/Transcript'
import type { DiffFile } from '../../assistant-ui/composition/DiffPanel'
import type { SidebarAgentRow } from '../../assistant-ui/sidebar/model'
import type { ResourceData } from '../../assistant-ui/resources/resource-model'
import { workLogTurnFromItems, type WorkLogTurn } from '../../assistant-ui/taste/work-log'

export const milestoneNow = Date.parse('2026-10-17T09:30:00.000Z')
const iso = (secondsAgo: number) => new Date(milestoneNow - secondsAgo * 1000).toISOString()

/** Roster: the same folder → agent → subagent shape the TUI uses; subagents start collapsed. */
export interface RosterFolder { readonly id: string; readonly label: string; readonly rows: readonly SidebarAgentRow[] }

const base = {
  harness: 'seat-1', freshness: 'live', usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Unknown' }, children: [],
} as const satisfies Partial<SidebarAgentRow>

const agent = (row: Omit<SidebarAgentRow, keyof typeof base | 'ref'> & Partial<SidebarAgentRow>): SidebarAgentRow => ({ ...base, ref: `agent/${row.id}`, ...row })

export const mainAgentRef = 'agent/metric-track'
export const reviewerRef = 'agent/metric-track/subagent/reviewer'
/** Empty-thread state opens an idle agent: an idle row never claims work the pane does not show. */
export const idleAgentRef = 'agent/transcript-paging'

const reviewer = agent({
  id: 'metric-track', ref: reviewerRef, parentRef: mainAgentRef, title: 'Review metric track diff', status: 'idle', statusLabel: 'Done', host: 'build-2',
  statusSince: milestoneNow - 140_000, lastTurn: { _tag: 'Known', kind: 'turn-completed', at: milestoneNow - 140_000 },
  usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 0.21, tokens: 48_200 }, duration: { _tag: 'Known', scope: 'lifetime', ms: 96_000 },
})
const checker = agent({
  id: 'metric-track', ref: 'agent/metric-track/subagent/checks', parentRef: mainAgentRef, title: 'Run row fitting checks', status: 'working', statusLabel: 'Working', host: 'build-2',
  statusSince: milestoneNow - 35_000, usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 0.04, tokens: 9_100 },
})

export const rosterFolders: readonly RosterFolder[] = [
  { id: 'fractal-web', label: 'fractal-web', rows: [
    agent({
      id: 'metric-track', ref: mainAgentRef, title: 'Align the sidebar metric track', description: 'Re-running row fitting checks', status: 'working', statusLabel: 'Working', host: 'build-2',
      model: 'model-a', statusSince: milestoneNow - 412_000, lastActivityAt: milestoneNow - 8_000, unread: 2, needsMe: false,
      usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 3.42, tokens: 1_180_000 }, duration: { _tag: 'Known', scope: 'lifetime', ms: 2_460_000 },
      lastTurn: { _tag: 'Known', kind: 'turn-completed', at: milestoneNow - 95_000 }, branch: 'feat/metric-track', children: [reviewer, checker],
    }),
    agent({
      id: 'hover-card', title: 'Hover card shows reported scopes', description: 'Waiting for a decision on the scope label', status: 'waiting', statusLabel: 'Needs you', host: 'build-1',
      statusSince: milestoneNow - 1_260_000, needsMe: true, unread: 1,
      usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 1.08, tokens: 402_000 }, duration: { _tag: 'Known', scope: 'lifetime', ms: 1_320_000 },
      lastTurn: { _tag: 'Known', kind: 'turn-completed', at: milestoneNow - 1_260_000 },
    }),
    agent({
      id: 'transcript-paging', title: 'Keep the scroll anchor while older pages load', status: 'idle', statusLabel: 'Idle', host: 'build-1',
      statusSince: milestoneNow - 7_400_000, usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 6.9, tokens: 2_310_000 },
      duration: { _tag: 'Known', scope: 'lifetime', ms: 5_100_000 }, lastTurn: { _tag: 'Known', kind: 'turn-completed', at: milestoneNow - 7_400_000 },
    }),
  ] },
  { id: 'release', label: 'release', rows: [
    agent({
      id: 'release-notes', title: 'Draft release notes', description: 'Collecting merged entries', status: 'working', statusLabel: 'Working', host: 'build-3',
      statusSince: milestoneNow - 64_000, usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 0.37, tokens: 88_000 },
      duration: { _tag: 'Known', scope: 'lifetime', ms: 310_000 }, lastTurn: { _tag: 'Known', kind: 'activity', at: milestoneNow - 12_000 },
    }),
    agent({
      id: 'flaky-gate', title: 'Fix flaky interaction gate', status: 'stale', statusLabel: 'Stale observation', host: 'build-4', freshness: 'stale',
      statusSince: milestoneNow - 3_100_000, usage: { _tag: 'Known', scope: 'lifetime-incl-subagents', usd: 2.15, tokens: 760_000 },
      lastTurn: { _tag: 'Known', kind: 'turn-completed', at: milestoneNow - 3_100_000 },
    }),
  ] },
  { id: 'unfiled', label: 'Unfiled', rows: [
    agent({ id: 'scratch', title: 'Scratch session', status: 'ended', statusLabel: 'Ended', host: 'build-1', statusSince: milestoneNow - 86_400_000 }),
  ] },
]
export const rosterRows: readonly SidebarAgentRow[] = rosterFolders.flatMap(folder => folder.rows)
export const inboxCount = 3

/** Thread: one settled turn with folded work, an edit, a subagent result and prose file chips; one running turn. */
const operator: Sender = { kind: 'human', label: 'You' }
const worker: Sender = { kind: 'agent', label: 'metric-track' }
const subagent: Sender = { kind: 'subagent', label: 'Review metric track diff', via: 'metric-track' }
const text = (id: string, role: 'user' | 'assistant', body: string, at: string, sender: Sender): TextItem => ({ _tag: 'Text', id, role, text: body, attachments: [], streaming: false, at, sender })
const read = (id: string, path: string, note: string, at: string): ConversationItem => ({ _tag: 'ToolCall', id, callId: id, name: 'read', input: { path }, status: 'success', callSeen: true, at, sender: worker, result: { content: note, isError: false, mediaType: 'text/plain', at } })

const rowsDiff = ['--- a/src/sidebar/row-fit.ts', '+++ b/src/sidebar/row-fit.ts', '@@ -12,3 +12,4 @@', '-const track = measure(rows)', '+const track = Math.max(minTrack, measure(rows))', '+cohort.style.setProperty(TRACK, `${track}px`)', ' return track'].join('\n')
const settledItems: readonly ConversationItem[] = [
  { _tag: 'Reasoning', id: 'm/reasoning', text: 'The metric column widens per row today. A shared track keeps every title edge aligned; measure once per cohort.', durationMs: 2100, streaming: false, at: iso(410), sender: worker },
  text('m/plan', 'assistant', 'Each row measures its own metric column, so titles start at different x positions. I’ll measure the cohort once and share one track.', iso(405), worker),
  read('m/read-row', 'src/sidebar/row-fit.ts', 'Synthetic row-fit.ts: measure(rows) per row, no shared track.', iso(400)),
  read('m/read-signals', 'src/sidebar/signals.tsx', 'Synthetic signals.tsx: line-1 fitting drops scope first.', iso(398)),
  read('m/read-story', 'src/sidebar/roster.stories.tsx', 'Synthetic roster story: 100 rows, three layouts.', iso(396)),
  { _tag: 'ToolCall', id: 'm/edit', callId: 'm-edit', name: 'edit', input: { path: 'src/sidebar/row-fit.ts', oldString: 'const track = measure(rows)', newString: 'const track = Math.max(minTrack, measure(rows))' }, status: 'success', callSeen: true, at: iso(380), sender: worker, result: { content: rowsDiff, mediaType: 'text/x-diff', isError: false, at: iso(379) } },
  { _tag: 'ToolCall', id: 'm/check', callId: 'm-check', name: 'bash', input: { command: 'pnpm check:sidebar-fitting' }, status: 'success', callSeen: true, at: iso(360), sender: worker, result: { content: '100 rows · titles keep ≥60% of line 1 · 0 overlaps · 412 ms', isError: false, mediaType: 'text/plain', at: iso(352) } },
  text('m/result', 'assistant', 'Updated `src/sidebar/row-fit.ts` so every row in a cohort shares one metric track. The fitting check passes on all 100 rows. `src/sidebar/signals.tsx` needed no change.\n\n`agent:metric-track/subagent/reviewer`', iso(140), worker),
]
const settledWork: WorkLogTurn = workLogTurnFromItems(settledItems, { kindFor: name => name === 'read' ? 'read' : name === 'edit' ? 'edit' : 'run', running: false, failed: false, interrupted: false, durationMs: 272_000, startedAt: iso(412), completeHistory: true })
const runningCall: ConversationItem = { _tag: 'ToolCall', id: 'm/running-check', callId: 'm-running', name: 'bash', input: { command: 'pnpm check:sidebar-fitting --narrow' }, status: 'running', callSeen: true, at: iso(30), sender: worker }
const settledTurn: TranscriptTurn = { id: 'm/turn-1', prompt: text('m/prompt-1', 'user', 'Titles in the sidebar start at different x positions depending on the metric. Align them without losing the time column.', iso(412), operator) as TextItem & { role: 'user' }, items: settledItems, work: settledWork }
const followUp = text('m/prompt-2', 'user', 'Also check the narrow sidebar width before you finish.', iso(40), operator) as TextItem & { role: 'user' }

export const mainTurns: readonly TranscriptTurn[] = [
  settledTurn,
  { id: 'm/turn-2', prompt: followUp, items: [runningCall], work: { calls: [{ id: runningCall.id, kind: 'run', title: 'bash', argsSummary: 'pnpm check:sidebar-fitting --narrow', status: 'running', startedAt: iso(30) }], durationMs: undefined, running: true, failed: false, interrupted: false, startedAt: iso(38), foldable: true } },
]

/** Stale (q23): the in-flight call is last-known, not live, so it leaves the work log and moves to the "Last seen" tail. */
export const staleMainTurns: readonly TranscriptTurn[] = [
  settledTurn,
  { id: 'm/turn-2', prompt: followUp, items: [], work: { calls: [], durationMs: undefined, running: false, failed: false, interrupted: false, foldable: true } },
]
export const lastSeen = { activity: 'working', at: milestoneNow - 18_000, detail: 'pnpm check:sidebar-fitting --narrow' } as const

/** The subagent result folds into the parent answer as one RC-1 card; the reviewer's own thread opens on press. */
export const reviewerCard = { path: 'agent:metric-track/subagent/reviewer', summary: 'The shared track keeps every title edge aligned; no row loses its time column.' } as const

const reviewItems: readonly ConversationItem[] = [
  read('r/read-diff', 'src/sidebar/row-fit.ts', 'Synthetic diff: one shared track per cohort.', iso(170)),
  text('r/answer', 'assistant', 'The shared track keeps every title edge aligned; no row loses its time column. Nothing else to change.', iso(150), subagent),
]
export const reviewerTurns: readonly TranscriptTurn[] = [
  { id: 'r/turn-1', prompt: text('r/prompt', 'user', 'Review the metric track change in src/sidebar/row-fit.ts.', iso(180), worker) as TextItem & { role: 'user' }, items: reviewItems, work: workLogTurnFromItems(reviewItems, { kindFor: () => 'read', running: false, failed: false, interrupted: false, durationMs: 30_000, completeHistory: true }) },
]

export const changedFiles: readonly DiffFile[] = [
  { path: 'src/sidebar/row-fit.ts', diff: ['@@ -12,3 +12,4 @@', '-const track = measure(rows)', '+const track = Math.max(minTrack, measure(rows))', '+cohort.style.setProperty(TRACK, `${track}px`)', ' return track'], added: 2, removed: 1 },
]
export const branchFiles: readonly DiffFile[] = [
  ...changedFiles,
  { path: 'src/sidebar/row-fit.test.ts', diff: ['@@ -40,0 +41,4 @@', "+it('shares one track per cohort', () => {", '+  expect(fit(rows).track).toBe(32)', '+})', '+'], added: 4, removed: 0 },
]

/** RC-3 prose chips resolve these exact paths only; unknown paths stay inline code. */
export const proseResources: Readonly<Record<string, ResourceData>> = {
  'src/sidebar/row-fit.ts': { kind: 'diff', label: 'Metric track change', detail: 'feat/metric-track · 1 file changed in this turn', fileCount: { _tag: 'Known', value: 1 }, files: [{ path: 'src/sidebar/row-fit.ts', added: { _tag: 'Known', value: 2 }, removed: { _tag: 'Known', value: 1 }, lines: { _tag: 'Known', value: changedFiles[0]!.diff.slice(1) } }] },
  'src/sidebar/signals.tsx': { kind: 'file', label: 'signals.tsx', detail: 'Read in this turn · unchanged', fileCount: { _tag: 'Known', value: 1 }, files: [{ path: 'src/sidebar/signals.tsx', added: { _tag: 'Known', value: 0 }, removed: { _tag: 'Known', value: 0 }, lines: { _tag: 'Known', value: [' // line-1 fitting drops scope first'] } }] },
}
