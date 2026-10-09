import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { ActionBarPrimitive, MessagePrimitive, ThreadPrimitive, useAuiState } from '@assistant-ui/react'
import { Button, Disclosure, DisclosurePanel, ProgressBar } from 'react-aria-components'
import type { ConversationItem, MessageItem, TextItem } from '../embrace-data/model'
import { EmbraceScrollViewport } from '../EmbraceScrollViewport'
import { RuntimeAdoptedIds } from '../EmbraceRuntime'
import { WorkLogV1 } from '../taste/WorkLogV1'
import { formatWorkDuration, workLogOutputLanguage, type WorkLogCall, type WorkLogTurn } from '../taste/work-log'
import { SyncLine } from '../st3-views/SyncLine'
import { syncLine } from '../st3-views/sync-line'
import type { SyncStatus } from '../st3-views/sync-status'
import { ErrorOverlay, ErrorOverlayHost } from './ErrorOverlay'
import { HighlightedSource, Markdown, MarkdownImagePolicy, type MarkdownImageOpener, type MarkdownImageResolver } from './Markdown'
import { ThinkingEntry } from './ThinkingEntry'
import { SendFailure, TranscriptEmptyContent, type TranscriptEmptyState } from './TranscriptFeedback'
import { Icon } from './Icons'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, motionVars as m } from '../composition-tokens.stylex'

export interface TranscriptTurn {
  readonly id: string
  /** Omitted when the visible history starts mid-turn or has assistant-only output. */
  readonly prompt?: TextItem & { readonly role: 'user' }
  /** Exact runtime source references. The host owns turn/participant boundaries. */
  readonly items: readonly ConversationItem[]
  readonly work: WorkLogTurn
  readonly senderCaptions?: Readonly<Record<string, string | undefined>>
}
/** `Unavailable` is the whole status: one host reason line and no sync line. `detail` may be diagnostic, so it stays behind "Show details". `action` is the host's recovery, e.g. `{ label: 'Try again', onPress }`. */
export type TranscriptAvailability = { readonly _tag: 'Available' } | { readonly _tag: 'Unavailable'; readonly reason: string; readonly detail?: string; readonly action?: { readonly label: string; readonly onPress: () => void } }
export type TranscriptHistory = { readonly _tag: 'Complete' } | { readonly _tag: 'HasOlder'; readonly onLoadEarlier?: () => void }
export interface TranscriptProps {
  readonly turns: readonly TranscriptTurn[]
  readonly title: string
  readonly sync: SyncStatus
  readonly now: number
  readonly observedAt: number
  /** Open output in the host's detail surface. No split/controller is hidden in the kit. */
  readonly onOpenTool?: (call: WorkLogCall) => void
  readonly onRetrySync?: () => void
  readonly onRetryRun?: () => void
  readonly onRetrySend?: (itemId: string) => void
  readonly resolveImage?: MarkdownImageResolver
  /** Opens a deferred image outside the page (for hosts whose CSP forbids inline remote images). */
  readonly onLoadImage?: MarkdownImageOpener
  readonly availability?: TranscriptAvailability
  readonly history?: TranscriptHistory
  readonly emptyState?: React.ReactNode | TranscriptEmptyState
}
const SenderCaption = React.createContext<string | undefined>(undefined)
const RetrySend = React.createContext<TranscriptProps['onRetrySend']>(undefined)
function TranscriptMessage() {
  const item = useAuiState(state => state.message.metadata.custom.item) as ConversationItem | undefined
  const summary = useAuiState(state => state.message.content.find(part => part.type === 'text')?.text ?? '')
  const caption = React.useContext(SenderCaption)
  if (item === undefined || item._tag === 'ToolCall' || item._tag === 'Reasoning') return null
  if (item._tag === 'Text' && item.role !== 'system') return item.role === 'user' ? <UserMessage item={{ ...item, role: 'user' }} /> : <AgentMessage item={{ ...item, role: 'assistant' }} senderLine={caption} />
  if (item._tag === 'Message') return <AgentMessage item={item} senderLine={caption ?? item.sender?.label ?? item.from} />
  return <MessagePrimitive.Root data-testid="transcript-message" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.semantic)}><span {...stylex.props(styles.semanticText)}>{summary}</span></MessagePrimitive.Root>
}
const messageComponents = { Message: TranscriptMessage }
export function UserMessage({ item }: { readonly item: TextItem & { readonly role: 'user' } }) {
  const onRetrySend = React.useContext(RetrySend)
  return <MessagePrimitive.Root data-testid="user-message" data-item-id={item.id} data-send-state={(item.sendState?._tag ?? 'Sent').toLowerCase()} {...stylex.props(styles.user, item.sendState?._tag === 'Pending' && styles.userPending)}><span {...stylex.props(styles.userText)}>{item.text}</span>{item.sendState?._tag === 'Failed' && <SendFailure state={item.sendState} onRetry={onRetrySend === undefined ? undefined : () => onRetrySend(item.id)} />}</MessagePrimitive.Root>
}
/** Settled metadata remains below the prose; unknown time is omitted rather than invented. */
export function AgentMessage({ item, senderLine }: { readonly item: (TextItem & { readonly role: 'assistant' }) | MessageItem; readonly senderLine?: string }) {
  const streaming = item._tag === 'Text' && item.streaming
  const completed = streaming ? NaN : Date.parse(item.at)
  return <MessagePrimitive.Root data-testid="agent-message" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.answer)}>
    {senderLine !== undefined && <p data-testid="message-sender" {...stylex.props(styles.sender)}>{senderLine}</p>}
    {item._tag === 'Text' ? <Markdown text={item.text} streaming={item.streaming} /> : <MessagePrimitive.Parts components={{ Text: Markdown }} />}
    {!streaming && <div data-testid="answer-meta" {...stylex.props(styles.answerMeta)}><ActionBarPrimitive.Copy aria-label="Copy answer" {...stylex.props(styles.copy)}><Icon name="copy" size={14} /></ActionBarPrimitive.Copy>{Number.isFinite(completed) && <time dateTime={new Date(completed).toISOString()}>{new Date(completed).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>}</div>}
  </MessagePrimitive.Root>
}
/** F3: four observed output lines, remaining count and a host-owned Open action. */
export function ToolDetailPreview({ call, onOpen }: { readonly call: WorkLogCall; readonly onOpen?: (call: WorkLogCall) => void }) {
  const output = call.detail ?? (call.status === 'running' ? 'No output received yet.' : 'No output recorded.')
  const lines = output.split('\n')
  const remaining = Math.max(0, lines.length - Number(output.endsWith('\n')) - 4)
  return <div data-testid="tool-detail-preview" {...stylex.props(styles.preview)}><pre {...stylex.props(styles.output)}><code {...stylex.props(styles.outputCode)}><HighlightedSource code={onOpen === undefined ? output : lines.slice(0, 4).join('\n')} language={workLogOutputLanguage(call)} /></code></pre>{onOpen !== undefined && <div data-testid="tool-preview-actions" {...stylex.props(styles.previewActions)}>{remaining > 0 && <><span data-testid="tool-preview-remaining">+{remaining} {remaining === 1 ? 'line' : 'lines'}</span><span aria-hidden="true">·</span></>}<Button aria-label={`Open ${call.title} tool detail`} onPress={() => onOpen(call)} {...stylex.props(styles.previewOpen)}>Open</Button></div>}</div>
}
/** Source text an unadopted item can show without the runtime; absent when the item carries none. */
const strandedText = (item: ConversationItem): string | undefined => {
  switch (item._tag) {
    case 'Text': case 'Reasoning': return item.text
    case 'Message': return item.title
    case 'Notice': return item.detail === undefined ? item.text : `${item.text}\n${item.detail}`
    case 'Event': return `${item.title}\n${item.text}`
    case 'Status': return item.detail
    default: return undefined
  }
}
/** Fallback for an item the runtime never adopted: visible in place, never silently dropped. */
function StrandedItem({ item }: { readonly item: ConversationItem }) {
  const text = strandedText(item)
  return <div data-testid="transcript-stranded" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.semantic)}><span {...stylex.props(styles.semanticText)}>{text === undefined || text.trim() === '' ? 'Couldn\u2019t display this entry' : text}</span></div>
}
const PreparedTurn = React.memo(function PreparedTurn({ turn, stranded, onOpenTool, onRetryRun }: { turn: TranscriptTurn; stranded?: ReadonlySet<string>; onOpenTool: TranscriptProps['onOpenTool']; onRetryRun?: () => void }) {
  const detail = React.useCallback((call: WorkLogCall) => <ToolDetailPreview call={call} onOpen={onOpenTool} />, [onOpenTool])
  const reasoning = turn.items.filter(item => item._tag === 'Reasoning')
  return <section data-testid="transcript-turn" data-item-id={turn.id} {...stylex.props(styles.turn)}>
    {turn.prompt !== undefined && (stranded?.has(turn.prompt.id) ? <StrandedItem item={turn.prompt} /> : <ThreadPrimitive.Unstable_MessageById messageId={turn.prompt.id} components={messageComponents} />)}
    {(turn.work.calls.length > 0 || reasoning.length > 0) && <WorkLogV1 turn={turn.work} ariaLabel={`Work log ${turn.id}`} listStyle={styles.workList} renderCallDetail={detail} previewCallDetail={!turn.work.running} interactiveCalls={onOpenTool !== undefined} hideLiveRow onRetry={onRetryRun} onOpenOutput={onOpenTool} expandedBody={reasoning.map(item => <ThinkingEntry key={item.id} text={item.text} streaming={item.streaming} />)} />}
    {turn.items.filter(item => item._tag !== 'ToolCall' && item._tag !== 'Reasoning').map(item => stranded?.has(item.id) ? <StrandedItem key={item.id} item={item} /> : <SenderCaption.Provider key={item.id} value={turn.senderCaptions?.[item.id]}><ThreadPrimitive.Unstable_MessageById messageId={item.id} components={messageComponents} /></SenderCaption.Provider>)}
    {turn.work.running && <div data-testid="live-work" role="status" aria-label="Response in progress" {...stylex.props(styles.liveActivity)}><span aria-hidden="true">◌</span></div>}
  </section>
})
/** Locked U2·F3·Y3 presentation under the host's AssistantRuntimeProvider. */
export function Transcript({ turns, title, sync, now, observedAt, onOpenTool, onRetrySync, onRetryRun, onRetrySend, resolveImage, onLoadImage, availability = { _tag: 'Available' }, history = { _tag: 'Complete' }, emptyState }: TranscriptProps) {
  const messages = useAuiState(state => state.thread.messages)
  // External-store runtimes adopt each snapshot in a passive effect, and the store publishes the
  // adopted messages a task later. Recording the snapshot after its commit is the adoption epoch.
  // An id is pending (deferred so the turn stays mounted) while its snapshot has not been through a
  // commit or while the runtime holds it and the store has yet to publish it; once committed and
  // absent from the runtime, it is stranded.
  const [adoptionEpoch, setAdoptionEpoch] = React.useState<readonly TranscriptTurn[]>()
  React.useEffect(() => setAdoptionEpoch(turns), [turns])
  const runtimeIds = React.useContext(RuntimeAdoptedIds)
  const { committed, stranded } = React.useMemo(() => {
    const published = new Set(messages.map(message => message.id))
    const settled = new Set(adoptionEpoch?.flatMap(turn => [...(turn.prompt === undefined ? [] : [turn.prompt.id]), ...turn.items.map(item => item.id)]))
    const stranded = new Set<string>()
    const visible = (id: string) => {
      if (published.has(id)) return true
      if (!settled.has(id) || runtimeIds?.has(id)) return false
      stranded.add(id)
      return true
    }
    const committed = turns.flatMap(turn => {
      if (turn.prompt !== undefined && !visible(turn.prompt.id)) return []
      const pendingIndex = turn.items.findIndex(item => !visible(item.id))
      if (pendingIndex === -1) return [turn]
      // Keep the existing turn mounted while waiting, and defer only its pending suffix.
      const items = turn.items.slice(0, pendingIndex)
      if (turn.prompt === undefined && items.length === 0) return []
      const itemIds = new Set(items.map(item => item.id))
      return [{ ...turn, items, work: { ...turn.work, calls: turn.work.calls.filter(call => itemIds.has(call.id)) } }]
    })
    return { committed, stranded }
  }, [messages, turns, adoptionEpoch, runtimeIds])
  const strandedIds = [...stranded].join(', ')
  React.useEffect(() => {
    if (strandedIds !== '' && process.env.NODE_ENV !== 'production') console.warn(`Transcript: the runtime never adopted ${strandedIds}; showing a fallback row.`)
  }, [strandedIds])
  const imageOptions = React.useMemo(() => ({ resolveImage, onLoadImage }), [resolveImage, onLoadImage])
  const running = [...committed].reverse().find(turn => turn.work.running)
  const progress = sync._tag === 'Progress' && sync.stage === 'reading' && sync.done !== undefined && sync.total !== undefined ? sync : undefined
  const failure = syncLine({ status: sync, label: 'conversation', now, observedAt })
  const started = Date.parse(running?.work.startedAt ?? '')
  if (availability._tag === 'Unavailable') return <section aria-label="Conversation unavailable" data-testid="transcript-unavailable" {...stylex.props(styles.frame, styles.empty)}><header data-testid="transcript-header" {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{title}</strong></header><div {...stylex.props(styles.emptyBody)}>
    <TranscriptEmptyContent emptyState={{ title: availability.reason }} />
    {availability.action !== undefined && <Button data-testid="transcript-unavailable-action" onPress={availability.action.onPress} {...stylex.props(styles.unavailableAction)}>{availability.action.label}</Button>}
    {availability.detail !== undefined && <Disclosure {...stylex.props(styles.unavailableDetails)}><Button slot="trigger" {...stylex.props(styles.historyLoad)}>Show details</Button><DisclosurePanel><p data-testid="transcript-unavailable-detail" {...stylex.props(styles.unavailableDetail)}>{availability.detail}</p></DisclosurePanel></Disclosure>}
  </div></section>
  const empty = committed.length === 0 && sync._tag === 'Live'
    ? <div aria-label="Empty conversation" data-testid="transcript-empty" {...stylex.props(styles.emptyBody)}><TranscriptEmptyContent emptyState={emptyState} /></div>
    : <div data-testid="transcript-placeholder" aria-label="Loading conversation" {...stylex.props(styles.placeholder)}><p role="status">Loading conversation…</p><SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} /><div aria-hidden="true" {...stylex.props(styles.turn)}><div {...stylex.props(styles.skeletonPrompt)} /><div {...stylex.props(styles.skeletonWork)} /><div {...stylex.props(styles.skeletonAnswer)} /></div></div>
  return <MarkdownImagePolicy.Provider value={imageOptions}><RetrySend.Provider value={onRetrySend}><ThreadPrimitive.Root aria-label="Transcript" {...stylex.props(styles.frame)}>
    <header data-testid="transcript-header" {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{title}</strong>
      {progress !== undefined && <ProgressBar aria-label="Thread synchronization" value={progress.done} maxValue={progress.total} {...stylex.props(styles.progress)}><div {...stylex.props(styles.track)}><div {...stylex.props(styles.fill(`${progress.total === 0 ? 0 : progress.done! / progress.total! * 100}%`))} /></div></ProgressBar>}
      {committed.length > 0 && <SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} />}
      {running !== undefined && <><div role="progressbar" aria-label="Run in progress" aria-valuetext="Running" data-testid="run-progress" {...stylex.props(styles.runningProgress)}><span {...stylex.props(styles.runningSegment)} /></div><span data-testid="run-elapsed">Running{Number.isFinite(started) && started <= now ? ` · ${formatWorkDuration(now - started) || '<1s'}` : ''}</span></>}
    </header>
    <ErrorOverlayHost lane><EmbraceScrollViewport items={committed} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.lane)} contentProps={stylex.props(styles.content)}>
      {history._tag === 'HasOlder' && <div data-testid="history-boundary" {...stylex.props(styles.historyBoundary)}><span {...stylex.props(styles.historyNote)}>Earlier messages not loaded</span>{history.onLoadEarlier !== undefined && <Button onPress={history.onLoadEarlier} {...stylex.props(styles.historyLoad)}>Load earlier messages</Button>}</div>}
      {committed.length === 0 ? empty : <div {...stylex.props(styles.timeline)}>{committed.map(turn => <PreparedTurn key={turn.id} turn={turn} stranded={turn.prompt !== undefined && stranded.has(turn.prompt.id) || turn.items.some(item => stranded.has(item.id)) ? stranded : undefined} onOpenTool={onOpenTool} onRetryRun={onRetryRun} />)}</div>}
    </EmbraceScrollViewport>{failure?.tone === 'error' && <ErrorOverlay id={`sync-${failure.text}`} title={failure.text} detail="History stays on screen." onRetry={onRetrySync} />}</ErrorOverlayHost>
  </ThreadPrimitive.Root></RetrySend.Provider></MarkdownImagePolicy.Provider>
}
const runSweep = stylex.keyframes({ from: { transform: 'translateX(0%)' }, to: { transform: 'translateX(300%)' } })
const styles = stylex.create({
  frame: { minWidth: 0, minHeight: g.threadViewportMin, height: '100%', display: 'flex', flexDirection: 'column', overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  header: { height: g.band, minHeight: g.band, position: 'relative', display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { flex: '1 1 0', minWidth: 0, fontWeight: t.weightMedium, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  lane: { flex: '1 1 0', minHeight: 0, minWidth: 0, overflowY: 'auto', overflowX: 'hidden', overflowAnchor: 'none' },
  content: { maxWidth: g.lane, marginInline: 'auto', padding: s.lg, minWidth: 0 }, timeline: { display: 'flex', flexDirection: 'column', gap: s.xl, minWidth: 0 }, turn: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  workList: { maxHeight: 'none', overflowY: 'visible', overscrollBehavior: 'auto' },
  user: { display: 'flex', flexDirection: 'column', minWidth: 0, color: ink.fg, fontSize: t.bodySize, lineHeight: t.bodyLeading, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: accent.primary, paddingLeft: s.lg, paddingBlock: s.xs }, userText: { minWidth: 0, overflowWrap: 'anywhere', whiteSpace: 'pre-wrap' },
  answer: { display: 'flex', flexDirection: 'column', gap: s.xs2, paddingBlock: s.xs2, color: ink.fgSoft, flexShrink: 0 }, sender: { margin: 0, fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightMedium, color: ink.fgMuted },
  semantic: { display: 'flex', minWidth: 0, paddingBlock: s.xs2, color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  semanticText: { minWidth: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  answerMeta: { display: 'flex', alignItems: 'center', gap: s.sm, minHeight: g.controlSm, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted },
  copy: { width: g.controlSm, height: g.controlSm, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', ':hover': { color: ink.fg, backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  progress: { position: 'absolute', top: 0, left: 0, right: 0, height: g.progressSm }, track: { height: g.progressSm, backgroundColor: surface.rowActive, overflow: 'hidden' }, fill: (width: string) => ({ width, height: '100%', backgroundColor: accent.primary }),
  runningProgress: { position: 'absolute', top: 0, left: 0, right: 0, height: g.progressSm, overflow: 'hidden', backgroundColor: surface.rowActive },
  runningSegment: { display: 'block', width: '25%', height: '100%', backgroundColor: accent.primary, animationName: runSweep, animationDuration: m.pulse, animationTimingFunction: m.linear, animationIterationCount: 'infinite', animationDirection: 'alternate', '@media (prefers-reduced-motion: reduce)': { animationName: 'none', transform: 'translateX(150%)' } },
  liveActivity: { display: 'flex', alignItems: 'center', gap: s.sm, minHeight: g.toolRow, color: ink.fgMuted, fontSize: t.uiSize },
  preview: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', marginBlock: s.sm, marginInlineStart: s.md, backgroundColor: surface.codeBg, borderRadius: r.sm, padding: s.md, gap: s.xs },
  output: { margin: 0, width: '100%', minWidth: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.metaLeading, color: ink.fgSoft }, outputCode: { fontFamily: t.fontMono, fontSize: t.codeSize },
  previewActions: { display: 'flex', alignItems: 'center', gap: s.xs, minHeight: g.toolRow, color: ink.fgMuted, fontSize: t.metaSize }, previewOpen: { minHeight: g.toolRow, padding: 0, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  placeholder: { display: 'flex', flexDirection: 'column', gap: s.lg }, skeletonPrompt: { width: '100%', height: `calc(${t.bodyLeading} + ${s.md})`, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: accent.primary, backgroundColor: surface.rowActive }, skeletonWork: { height: g.toolRow, width: '30%', borderRadius: r.sm, backgroundColor: surface.rowActive }, skeletonAnswer: { height: g.resourceCard, width: '80%', borderRadius: r.sm, backgroundColor: surface.rowHover },
  empty: { backgroundColor: surface.washSubtle },
  emptyBody: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', gap: s.sm, padding: s.xl, textAlign: 'center' },
  historyBoundary: { display: 'flex', alignItems: 'center', justifyContent: 'center', gap: s.md, minHeight: g.controlMd, flexShrink: 0 },
  historyNote: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted },
  historyLoad: { minHeight: g.controlSm, paddingInline: s.sm, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  unavailableAction: { minHeight: g.controlSm, marginTop: s.xs, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  unavailableDetails: { display: 'flex', flexDirection: 'column', alignItems: 'center', gap: s.xs, maxWidth: g.lane },
  unavailableDetail: { margin: 0, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted, overflowWrap: 'anywhere' },
  userPending: { color: ink.fgMuted },
})
