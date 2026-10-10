import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import { ActionBarPrimitive, MessagePrimitive, ThreadPrimitive, useAuiState } from '@assistant-ui/react'
import { Button, Disclosure, DisclosurePanel, ProgressBar } from 'react-aria-components'
import type { ConversationItem, MessageItem, TextItem } from '../embrace-data/model'
import { EmbraceScrollViewport, type EmbraceScrollViewportHandle, type ViewportAnchorHistory } from '../EmbraceScrollViewport'
import { RuntimeAdoptedIds } from '../EmbraceRuntime'
import { WorkLogV1 } from '../taste/WorkLogV1'
import { formatWorkDuration, workLogOutputLanguage, type WorkLogCall, type WorkLogTurn } from '../taste/work-log'
import { SyncLine } from '../st3-views/SyncLine'
import { syncLine } from '../st3-views/sync-line'
import type { SyncStatus } from '../st3-views/sync-status'
import { ErrorOverlay, ErrorOverlayHost } from './ErrorOverlay'
import { HighlightedSource, Markdown, MarkdownImagePolicy, type MarkdownImageOpener, type MarkdownImageResolver } from './Markdown'
import { ThinkingEntry, ThinkingRun } from './ThinkingEntry'
import { SendFailure, TranscriptEmptyContent, type TranscriptEmptyState } from './TranscriptFeedback'
import { TranscriptSkeleton } from './TranscriptSkeleton'
import { Icon } from './Icons'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, motionVars as m } from '../composition-tokens.stylex'
import { readingColumnStyles } from '../reading-column.stylex'

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
  /** Host-owned first-publication body; an empty body can keep a switch lane stable. */
  readonly loadingState?: React.ReactNode
  /** Workbench conversation key for the owning surface's scroll memory. */
  readonly viewportKey?: string
  /** Host command, e.g. the newest own pending send: a changed key brings the latest turn into view. */
  readonly scrollToBottomKey?: string
  /** Contextual landmark names when multiple transcript views share one canvas. */
  readonly landmarkContext?: string
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
/**
 * Marks turns and turn groups more than a viewport away from the scroller's visible area as `data-distant`; only those
 * skip rendering (`content-visibility: auto`). Turns near the reader render uncontained, so their painting and focus
 * rings match a transcript without containment. A box's first report follows its first layout, so
 * `contain-intrinsic-size: auto` has its real height before it is ever skipped. Reports from a hidden scroller (no root
 * bounds) are ignored.
 */
class TurnProximity {
  private observer: IntersectionObserver | undefined
  private readonly boxes = new Set<Element>()
  readonly attach = (root: Element | null | undefined) => {
    if (root == null || typeof IntersectionObserver === 'undefined') return
    const observer = new IntersectionObserver(entries => {
      for (const entry of entries) if (entry.rootBounds !== null && entry.rootBounds.height > 0) entry.target.toggleAttribute('data-distant', !entry.isIntersecting)
    }, { root, rootMargin: '100% 0px' })
    this.observer = observer
    for (const box of this.boxes) observer.observe(box)
    return () => {
      observer.disconnect()
      if (this.observer === observer) this.observer = undefined
    }
  }
  readonly observe = (box: HTMLElement | null) => {
    if (box === null) return
    this.boxes.add(box)
    this.observer?.observe(box)
    return () => {
      this.boxes.delete(box)
      this.observer?.unobserve(box)
      box.removeAttribute('data-distant')
    }
  }
}
/** EmbraceScrollViewport renders scroller > content > children. */
const scrollerOf = (timeline: Element | null) => timeline?.parentElement?.parentElement
const TurnProximityRef = React.createContext<TurnProximity['observe'] | undefined>(undefined)
const turnGroupSize = 16
/**
 * Turns from `start` in fixed groups of 16 counted from the turn at `origin`. The caller keeps `origin` on the same
 * turn while it stays in the transcript, so neither prepended history nor new turns move an existing turn to another
 * group: turns keep their React identity, disclosure state and focus. A distant full group skips as one box:
 * revealing a pane styles the groups, not every turn. A partial group (the oldest one while older turns backfill or
 * history is prepended, the newest one while turns arrive) is not observed and has no layout box: its children
 * participate in the outer timeline, avoiding a moving aggregate box during bounded prefix backfill. Once full
 * it renders a frame at its full height before its first report can mark it distant.
 */
const turnGroups = (turns: readonly TranscriptTurn[], start: number, origin: number) => {
  const groups: { readonly key: number; readonly turns: readonly TranscriptTurn[] }[] = []
  for (let key = Math.floor((start - origin) / turnGroupSize), first = origin + key * turnGroupSize; first < turns.length; key++, first += turnGroupSize) groups.push({ key, turns: turns.slice(Math.max(first, start), first + turnGroupSize) })
  return groups
}
function TurnGroup({ full, children }: { readonly full: boolean; readonly children: React.ReactNode }) {
  const observe = React.useContext(TurnProximityRef)
  return <div ref={full ? observe : undefined} {...stylex.props(styles.timeline, styles.turnSkip, !full && styles.partialGroup)}>{children}</div>
}
/** Fallback for an item the runtime never adopted: visible in place, never silently dropped. */
function StrandedItem({ item }: { readonly item: ConversationItem }) {
  const text = strandedText(item)
  return <div data-testid="transcript-stranded" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.semantic)}><span {...stylex.props(styles.semanticText)}>{text === undefined || text.trim() === '' ? 'Couldn\u2019t display this entry' : text}</span></div>
}
/** The content-bearing fields of an item; lifecycle and timing fields are left out. */
const itemContent = (item: ConversationItem): string => {
  switch (item._tag) {
    case 'Text': return `${item.text}\u0000${item.attachments.map(attachment => attachment.id).join('\u0000')}`
    case 'Reasoning': return item.text
    case 'Message': return item.title ?? ''
    case 'Notice': return `${item.text}\u0000${item.detail ?? ''}`
    case 'Event': return `${item.title}\u0000${item.text}`
    case 'Status': return item.detail ?? ''
    case 'ToolCall': return `${JSON.stringify(item.input) ?? ''}\u0000${item.result === undefined ? '' : JSON.stringify(item.result.content) ?? ''}`
    case 'UnknownEvent': return JSON.stringify(item.data) ?? ''
    case 'Usage': return ''
  }
}
const contentVersions = new WeakMap<ConversationItem, string>()
/** A cheap content revision (FNV-1a over the content fields), computed once per item object. */
const contentVersion = (item: ConversationItem): string => {
  const cached = contentVersions.get(item)
  if (cached !== undefined) return cached
  const content = itemContent(item)
  let hash = 0x811c9dc5
  for (let index = 0; index < content.length; index++) hash = Math.imul(hash ^ content.charCodeAt(index), 0x01000193)
  const version = `${item.id}:${content.length}:${(hash >>> 0).toString(36)}`
  contentVersions.set(item, version)
  return version
}
/** The window is counted in source rows, not turns: a single agent turn can contain thousands of entries. */
type MountedTurns = { readonly _tag: 'NewestPage' } | { readonly _tag: 'From'; readonly id: string } | { readonly _tag: 'All' }
const newestPageRows = 6
const backfillChunkRows = 4
const mountedStart = (mounted: MountedTurns, rows: readonly ConversationItem[]) =>
  mounted._tag === 'All' ? 0 : mounted._tag === 'NewestPage' ? Math.max(0, rows.length - newestPageRows) : Math.max(0, rows.findIndex(row => row.id === mounted.id))
/** Keep the owning turn's identity and summary, and the newest prompt even outside the source-row suffix. */
const windowTurns = (turns: readonly TranscriptTurn[], start: number): readonly TranscriptTurn[] => {
  let offset = 0
  return turns.flatMap((turn, index) => {
    const length = turn.items.length + (turn.prompt === undefined ? 0 : 1)
    const skip = Math.max(0, start - offset)
    offset += length
    if (length > 0 && skip >= length) return []
    if (skip === 0) return [turn]
    let itemStart = skip - (turn.prompt === undefined ? 0 : 1)
    // Keep a contiguous reasoning disclosure whole, including its first-item key, across prefix reveals.
    while (itemStart > 0 && turn.items[itemStart]?._tag === 'Reasoning' && turn.items[itemStart - 1]?._tag === 'Reasoning') itemStart--
    const items = turn.items.slice(itemStart)
    const ids = new Set(items.map(item => item.id))
    return [{ ...turn, prompt: index === turns.length - 1 ? turn.prompt : undefined, items, work: { ...turn.work, calls: turn.work.calls.filter(call => ids.has(call.id)) } }]
  })
}
/** Older engines and non-layout DOMs lack checkVisibility; retained hidden ancestors still must not backfill. */
const hasVisibleBox = (element: HTMLElement): boolean => {
  if (typeof element.checkVisibility === 'function') return element.checkVisibility()
  for (let ancestor: HTMLElement | null = element; ancestor !== null; ancestor = ancestor.parentElement) {
    const style = element.ownerDocument.defaultView?.getComputedStyle(ancestor)
    if (style?.display === 'none' || style?.contentVisibility === 'hidden') return false
  }
  return true
}
/** Growing a partial turn must not re-render the already mounted message subtrees. */
const PreparedMessage = React.memo(function PreparedMessage({ item, stranded, caption }: { readonly item: ConversationItem; readonly stranded: boolean; readonly caption?: string }) {
  return stranded ? <StrandedItem item={item} /> : <SenderCaption.Provider value={caption}><ThreadPrimitive.Unstable_MessageById messageId={item.id} components={messageComponents} /></SenderCaption.Provider>
})
/** Non-reasoning entries terminate a run, even when the work log renders those entries elsewhere. */
const reasoningRuns = (items: readonly ConversationItem[]) => {
  const runs: Extract<ConversationItem, { _tag: 'Reasoning' }>[][] = []
  let run: Extract<ConversationItem, { _tag: 'Reasoning' }>[] | undefined
  for (const item of items) {
    if (item._tag !== 'Reasoning') { run = undefined; continue }
    if (run === undefined) { run = []; runs.push(run) }
    run.push(item)
  }
  return runs
}
const PreparedTurn = React.memo(function PreparedTurn({ turn, stranded, onOpenTool, onRetryRun, landmarkContext }: { turn: TranscriptTurn; stranded?: ReadonlySet<string>; onOpenTool: TranscriptProps['onOpenTool']; onRetryRun?: () => void; landmarkContext?: string }) {
  const detail = React.useCallback((call: WorkLogCall) => <ToolDetailPreview call={call} onOpen={onOpenTool} />, [onOpenTool])
  const reasoning = reasoningRuns(turn.items)
  return <section ref={React.useContext(TurnProximityRef)} data-testid="transcript-turn" data-item-id={turn.id} {...stylex.props(styles.turn, styles.turnSkip)}>
    {turn.prompt !== undefined && <PreparedMessage item={turn.prompt} stranded={stranded?.has(turn.prompt.id) ?? false} />}
    {(turn.work.calls.length > 0 || reasoning.length > 0) && <WorkLogV1 turn={turn.work} summaryAnchorId={JSON.stringify(['work-summary', turn.id])} ariaLabel={`Work log ${turn.id}${landmarkContext ? `, ${landmarkContext}` : ''}`} listStyle={styles.workList} renderCallDetail={detail} previewCallDetail={!turn.work.running} interactiveCalls={onOpenTool !== undefined} hideLiveRow onRetry={onRetryRun} onOpenOutput={onOpenTool} expandedBody={reasoning.map(items => <ThinkingRun key={items[0]!.id} scrollAnchorId={JSON.stringify(['thinking-summary', turn.id])} items={items} />)} />}
    {turn.items.filter(item => item._tag !== 'ToolCall' && item._tag !== 'Reasoning').map(item => <PreparedMessage key={item.id} item={item} stranded={stranded?.has(item.id) ?? false} caption={turn.senderCaptions?.[item.id]} />)}
    {turn.work.running && <div data-testid="live-work" role="status" aria-label="Response in progress" {...stylex.props(styles.liveActivity)}><span aria-hidden="true">◌</span></div>}
  </section>
})
// Hosts using AssistantRuntimeProvider directly expose no adoption store; its absence never changes.
const staticSubscription = () => () => undefined
/** Locked U2·F3·Y3 presentation under the host's AssistantRuntimeProvider. */
export function Transcript({ turns, title, sync, now, observedAt, onOpenTool, onRetrySync, onRetryRun, onRetrySend, resolveImage, onLoadImage, availability = { _tag: 'Available' }, history = { _tag: 'Complete' }, emptyState, loadingState, viewportKey, scrollToBottomKey, landmarkContext }: TranscriptProps) {
  const messages = useAuiState(state => state.thread.messages)
  // External-store runtimes adopt each snapshot in a passive effect, and the store publishes the
  // adopted messages a task later. A child effect runs before its provider's adoption effect, so
  // a host render alone cannot classify missing ids as stranded. The adoption store first confirms
  // the source snapshot reached the runtime; ids it holds remain pending until publication, while
  // ids it did not adopt may render their fallback. Only unpublished ids subscribe to this seam.
  const [adoptionEpoch, setAdoptionEpoch] = React.useState<readonly TranscriptTurn[]>()
  const published = React.useMemo(() => new Set(messages.map(message => message.id)), [messages])
  const unpublished = React.useMemo(() => turns.flatMap(turn => [...(turn.prompt === undefined ? [] : [turn.prompt.id]), ...turn.items.map(item => item.id)]).filter(id => !published.has(id)), [turns, published])
  const adoption = React.useContext(RuntimeAdoptedIds)
  const getHeldKey = () => {
    const held = adoption?.get()
    const source = adoption?.getSourceIds()
    return JSON.stringify({ held: held === undefined ? [] : unpublished.filter(id => held.has(id)), sourceAdopted: adoption === undefined || source !== undefined && unpublished.every(id => source.has(id)) })
  }
  const heldKey = React.useSyncExternalStore(adoption?.subscribe ?? staticSubscription, getHeldKey, getHeldKey)
  const { runtimeIds, sourceAdopted } = React.useMemo(() => {
    const snapshot = JSON.parse(heldKey)
    return { runtimeIds: new Set<string>(snapshot.held), sourceAdopted: snapshot.sourceAdopted === true }
  }, [heldKey])
  React.useEffect(() => {
    if (sourceAdopted && adoptionEpoch !== turns && unpublished.some(id => !runtimeIds.has(id))) setAdoptionEpoch(turns)
  }, [turns, unpublished, runtimeIds, adoptionEpoch, sourceAdopted])
  const { committed, stranded } = React.useMemo(() => {
    const settled = new Set(adoptionEpoch?.flatMap(turn => [...(turn.prompt === undefined ? [] : [turn.prompt.id]), ...turn.items.map(item => item.id)]))
    const stranded = new Set<string>()
    const visible = (id: string) => {
      if (published.has(id)) return true
      if (!settled.has(id) || runtimeIds.has(id)) return false
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
  }, [published, turns, adoptionEpoch, runtimeIds])
  const strandedIds = [...stranded].join(', ')
  React.useEffect(() => {
    if (strandedIds !== '' && process.env.NODE_ENV !== 'production') console.warn(`Transcript: the runtime never adopted ${strandedIds}; showing a fallback row.`)
  }, [strandedIds])
  const timeline = React.useRef<HTMLDivElement>(null)
  const viewport = React.useRef<EmbraceScrollViewportHandle>(null)
  const [proximity] = React.useState(() => new TurnProximity())
  React.useLayoutEffect(() => proximity.attach(scrollerOf(timeline.current)), [proximity, committed.length === 0])
  // Mount a bounded suffix, filling the visible lane before paint. Older rows are
  // demanded by history-top navigation or browser find, not idle-time visible growth.
  const [mounted, setMounted] = React.useState<MountedTurns>({ _tag: 'NewestPage' })
  const source = React.useMemo(() => committed.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]), [committed])
  const start = mountedStart(mounted, source)
  const visibleTurns = React.useMemo(() => windowTurns(committed, start), [committed, start])
  const turnStart = committed.length - visibleTurns.length
  // The group grid hangs off the first turn this transcript showed, for as long as that turn stays in it.
  const [gridTurn, setGridTurn] = React.useState<string | undefined>(undefined)
  let gridOrigin = committed.findIndex(turn => turn.id === gridTurn)
  if (gridOrigin === -1 && committed.length > 0) {
    gridOrigin = 0
    setGridTurn(committed[0]!.id)
  }
  const latest = React.useRef({ source, start })
  React.useLayoutEffect(() => { latest.current = { source, start } })
  const mountOlder = React.useCallback((all: boolean) => {
    const { source, start } = latest.current
    if (start === 0) return
    const update = () => flushSync(() => setMounted(all ? { _tag: 'All' } : { _tag: 'From', id: source[Math.max(0, start - backfillChunkRows)]!.id }))
    // The lane owns compensation: followers stay at the end, readers keep their row.
    if (viewport.current === null) update()
    else viewport.current.preserveLayout(update)
  }, [])
  const backfilling = start > 0
  React.useLayoutEffect(() => {
    const scroller = scrollerOf(timeline.current)
    if (start > 0 && scroller instanceof HTMLElement && hasVisibleBox(scroller) && scroller.clientHeight > 0 && scroller.scrollHeight <= scroller.clientHeight) setMounted({ _tag: 'From', id: source[Math.max(0, start - backfillChunkRows)]!.id })
  }, [start, source, visibleTurns])
  // Native find must work as soon as the first bounded page can paint.
  React.useLayoutEffect(() => {
    const scroller = scrollerOf(timeline.current)
    if (!backfilling || scroller == null) return
    const nearTop = () => { if (scroller instanceof HTMLElement && scroller.dataset.followState === 'detached' && scroller.scrollTop < scroller.clientHeight) mountOlder(false) }
    const find = (event: KeyboardEvent) => { if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === 'f') mountOlder(true) }
    const view = scroller.ownerDocument.defaultView
    scroller.addEventListener('scroll', nearTop, { passive: true })
    view?.addEventListener('keydown', find, { capture: true })
    return () => {
      scroller.removeEventListener('scroll', nearTop)
      view?.removeEventListener('keydown', find, { capture: true })
    }
  }, [backfilling, mountOlder])
  const imageOptions = React.useMemo(() => ({ resolveImage, onLoadImage }), [resolveImage, onLoadImage])
  // Source membership stays authoritative while adoption, folding or bounded backfill leaves rows unmounted.
  const anchorHistory = React.useMemo<ViewportAnchorHistory>(() => history._tag === 'Complete' && (turns.length > 0 || sync._tag === 'Live')
    ? { _tag: 'Complete', ids: turns.flatMap(turn => {
      const hasReasoning = turn.items.some(item => item._tag === 'Reasoning')
      return [
        turn.id,
        ...(turn.prompt === undefined ? [] : [turn.prompt.id]),
        ...turn.items.map(item => item.id),
        ...(turn.work.calls.length > 0 || hasReasoning ? [JSON.stringify(['work-summary', turn.id])] : []),
        ...(hasReasoning ? [JSON.stringify(['thinking-summary', turn.id])] : []),
      ]
    }) }
    : { _tag: 'Partial' }, [turns, history._tag, sync._tag])
  // New rows or changed content are news; send state, tool status and timing are metadata.
  const rows = React.useMemo(() => committed.map(turn => ({ id: turn.id, version: (turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]).map(contentVersion).join(' ') })), [committed])
  const running = [...committed].reverse().find(turn => turn.work.running)
  const progress = sync._tag === 'Progress' && sync.stage === 'reading' && sync.done !== undefined && sync.total !== undefined ? sync : undefined
  const failure = syncLine({ status: sync, label: 'conversation', now, observedAt })
  const started = Date.parse(running?.work.startedAt ?? '')
  if (availability._tag === 'Unavailable') return <section aria-label={`Conversation unavailable${landmarkContext ? `, ${landmarkContext}` : ''}`} data-testid="transcript-unavailable" {...stylex.props(styles.frame, styles.empty)}><header data-testid="transcript-header" {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{title}</strong></header><div {...stylex.props(styles.emptyBody)}>
    <TranscriptEmptyContent emptyState={{ title: availability.reason }} />
    {availability.action !== undefined && <Button data-testid="transcript-unavailable-action" onPress={availability.action.onPress} {...stylex.props(styles.unavailableAction)}>{availability.action.label}</Button>}
    {availability.detail !== undefined && <Disclosure {...stylex.props(styles.unavailableDetails)}><Button slot="trigger" {...stylex.props(styles.historyLoad)}>Show details</Button><DisclosurePanel><p data-testid="transcript-unavailable-detail" {...stylex.props(styles.unavailableDetail)}>{availability.detail}</p></DisclosurePanel></Disclosure>}
  </div></section>
  const empty = committed.length === 0 && turns.length === 0 && sync._tag === 'Live'
    ? <div aria-label="Empty conversation" data-testid="transcript-empty" {...stylex.props(styles.emptyBody)}><TranscriptEmptyContent emptyState={emptyState} /></div>
    : loadingState ?? <TranscriptSkeleton sync={sync} now={now} observedAt={observedAt} onRetrySync={onRetrySync} />
  return <MarkdownImagePolicy.Provider value={imageOptions}><RetrySend.Provider value={onRetrySend}><ThreadPrimitive.Root aria-label={`Transcript${landmarkContext ? `, ${landmarkContext}` : ''}`} {...stylex.props(styles.frame)}>
    <header data-testid="transcript-header" {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{title}</strong>
      {progress !== undefined && <ProgressBar aria-label="Thread synchronization" value={progress.done} maxValue={progress.total} {...stylex.props(styles.progress)}><div {...stylex.props(styles.track)}><div {...stylex.props(styles.fill(`${progress.total === 0 ? 0 : progress.done! / progress.total! * 100}%`))} /></div></ProgressBar>}
      {committed.length > 0 && <SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} />}
      {running !== undefined && <><div role="progressbar" aria-label="Run in progress" aria-valuetext="Running" data-testid="run-progress" {...stylex.props(styles.runningProgress)}><span {...stylex.props(styles.runningSegment)} /></div><span data-testid="run-elapsed">Running{Number.isFinite(started) && started <= now ? ` · ${formatWorkDuration(now - started) || '<1s'}` : ''}</span></>}
    </header>
    <ErrorOverlayHost lane><EmbraceScrollViewport ref={viewport} items={rows} anchorHistory={anchorHistory} stateKey={viewportKey} scrollToBottomKey={scrollToBottomKey} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.lane)} contentProps={stylex.props(readingColumnStyles.column, styles.content)}>
      <div data-testid="transcript-history-slot" {...stylex.props(styles.historyBoundary)}>
        {start > 0 && <Button data-testid="transcript-reveal-earlier" onPress={() => mountOlder(true)} {...stylex.props(styles.historyLoad)}>Show earlier messages</Button>}
        {history._tag === 'HasOlder' && <div data-testid="history-boundary" {...stylex.props(styles.historyBoundary)}><span {...stylex.props(styles.historyNote)}>Earlier messages not loaded</span>{history.onLoadEarlier !== undefined && <Button onPress={history.onLoadEarlier} {...stylex.props(styles.historyLoad)}>Load earlier messages</Button>}</div>}
      </div>
      {committed.length === 0 ? empty : <div ref={timeline} {...stylex.props(styles.timeline)}><TurnProximityRef.Provider value={proximity.observe}>{turnGroups(visibleTurns, 0, gridOrigin - turnStart).map(group => <TurnGroup key={group.key} full={group.turns.length === turnGroupSize}>{group.turns.map(turn => <PreparedTurn key={turn.id} turn={turn} stranded={turn.prompt !== undefined && stranded.has(turn.prompt.id) || turn.items.some(item => stranded.has(item.id)) ? stranded : undefined} onOpenTool={onOpenTool} onRetryRun={onRetryRun} landmarkContext={landmarkContext} />)}</TurnGroup>)}</TurnProximityRef.Provider></div>}
    </EmbraceScrollViewport>{failure?.tone === 'error' && <ErrorOverlay id={`sync-${failure.text}`} title={failure.text} detail="History stays on screen." onRetry={onRetrySync} />}</ErrorOverlayHost>
  </ThreadPrimitive.Root></RetrySend.Provider></MarkdownImagePolicy.Provider>
}
const runSweep = stylex.keyframes({ from: { transform: 'translateX(0%)' }, to: { transform: 'translateX(300%)' } })
const styles = stylex.create({
  frame: { minWidth: 0, minHeight: g.threadViewportMin, height: '100%', display: 'flex', flexDirection: 'column', overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  header: { height: g.band, minHeight: g.band, position: 'relative', display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { flex: '1 1 0', minWidth: 0, fontWeight: t.weightMedium, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  lane: { flex: '1 1 0', minHeight: 0, minWidth: 0, overflowY: 'auto', overflowX: 'hidden', overflowAnchor: 'none' },
  // The shared reading column keeps rows and a reading-column composer on the same bounds.
  content: { paddingBlock: s.lg }, timeline: { display: 'flex', flexDirection: 'column', gap: s.xl, minWidth: 0 }, turn: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  partialGroup: { display: 'contents' },
  workList: { maxHeight: 'none', overflowY: 'visible', overscrollBehavior: 'auto' },
  // Turns and full turn groups far from the reader skip style, layout and paint; `auto` keeps each box's last rendered
  // height as its placeholder, recorded while the box renders.
  turnSkip: { contentVisibility: { default: 'visible', ':is([data-distant])': 'auto' }, containIntrinsicSize: `auto ${g.estimatedMessageHeight}` },
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
