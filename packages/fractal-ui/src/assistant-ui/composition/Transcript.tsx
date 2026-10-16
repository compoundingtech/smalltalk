import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { ActionBarPrimitive, MessagePrimitive, ThreadPrimitive, useAui, useAuiState } from '@assistant-ui/react'
import { Button, Disclosure, DisclosurePanel, ProgressBar } from 'react-aria-components'
import type { ConversationItem, MessageItem, TextItem } from '../embrace-data/model'
import { EmbraceScrollViewport } from '../EmbraceScrollViewport'
import { RuntimeAdoptedIds } from '../EmbraceRuntime'
import { WorkLogV1 } from '../taste/WorkLogV1'
import { errorReason, formatWorkDuration, readableCommand, workLogOutputLanguage, type WorkLogCall, type WorkLogTurn } from '../taste/work-log'
import { SyncLine } from '../st3-views/SyncLine'
import { syncLine } from '../st3-views/sync-line'
import type { SyncStatus } from '../st3-views/sync-status'
import { ErrorOverlay, ErrorOverlayHost } from './ErrorOverlay'
import { HighlightedSource, Markdown, MarkdownImagePolicy, type InlineReferenceRenderer, type MarkdownImageOpener, type MarkdownImageResolver } from './Markdown'
import { ThinkingEntry } from './ThinkingEntry'
import { SendFailure, SendUnconfirmed, TranscriptEmptyContent, type TranscriptEmptyState } from './TranscriptFeedback'
import { Icon } from './Icons'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, motionVars as m } from '../composition-tokens.stylex'
import { readingColumnStyles } from '../reading-column.stylex'

export type LiveActivityState = { readonly _tag: 'Working' } | { readonly _tag: 'WaitingForAnswer' }
export interface TranscriptTurn {
  readonly id: string
  /** Omitted when the visible history starts mid-turn or has assistant-only output. */
  readonly prompt?: TextItem & { readonly role: 'user' }
  /** Exact runtime source references. The host owns turn/participant boundaries. */
  readonly items: readonly ConversationItem[]
  readonly work: WorkLogTurn
  /** Host-owned interaction state; the kit never infers a pending question from output text. */
  readonly activity?: LiveActivityState
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
  /** The host's own inline references in message Markdown, such as a `doc/…` name it can open. */
  readonly renderInlineReference?: InlineReferenceRenderer
  readonly availability?: TranscriptAvailability
  readonly history?: TranscriptHistory
  readonly emptyState?: React.ReactNode | TranscriptEmptyState
  /** Workbench conversation key for the owning surface's scroll memory. */
  readonly viewportKey?: string
  /** Host command, e.g. the newest own pending send: a changed key brings the latest turn into view. */
  readonly scrollToBottomKey?: string
  /** Contextual landmark names when multiple transcript views share one canvas. */
  readonly landmarkContext?: string
  /** C8/E14: preserve the current seat prose header until the turn-language pick; hosts may compare the headerless path. */
  readonly showSeatHeader?: boolean
}
const SenderCaption = React.createContext<string | undefined>(undefined)
const SeatHeader = React.createContext(true)
/** True for a prose block that continues the same sender's run inside one turn. */
const RunContinuation = React.createContext(false)
/** Whole-run text for the block that heads a multi-block prose run, so its single Copy covers every block. */
const RunCopyText = React.createContext<string | undefined>(undefined)
const RetrySend = React.createContext<TranscriptProps['onRetrySend']>(undefined)
function TranscriptMessage() {
  const item = useAuiState(state => state.message.metadata.custom.item) as ConversationItem | undefined
  const summary = useAuiState(state => state.message.content.find(part => part.type === 'text')?.text ?? '')
  const caption = React.useContext(SenderCaption)
  const continuation = React.useContext(RunContinuation)
  if (item === undefined || item._tag === 'ToolCall' || item._tag === 'Reasoning') return null
  if (item._tag === 'Text' && item.role !== 'system') return item.role === 'user' ? <UserMessage item={{ ...item, role: 'user' }} /> : <SeatProse item={{ ...item, role: 'assistant' }} senderLine={caption} continuation={continuation} />
  if (item._tag === 'Message') return <SeatProse item={item} senderLine={caption ?? item.sender?.label ?? item.from} />
  if (item._tag === 'UnknownEvent' && item.mixedContent === true) return <MixedContentMessage item={item} />
  return <MessagePrimitive.Root data-testid="transcript-message" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.semantic)}><span {...stylex.props(styles.semanticText)}>{summary}</span></MessagePrimitive.Root>
}
const messageComponents = { Message: TranscriptMessage }
/** Keep mixed bodies out of the collapsed DOM, using the same raw disclosure as tool inspection. */
function MixedContentMessage({ item }: { readonly item: Extract<ConversationItem, { _tag: 'UnknownEvent' }> }) {
  const [expanded, setExpanded] = React.useState(false)
  return <MessagePrimitive.Root data-testid="transcript-message" data-item-id={item.id} data-item-kind={item._tag} {...stylex.props(styles.mixed)}>
    <Disclosure isExpanded={expanded} onExpandedChange={setExpanded}>
      <div {...stylex.props(styles.mixedSummary)}><span>Message with internal records</span><span aria-hidden="true">·</span><Button slot="trigger" {...stylex.props(styles.mixedTrigger)}>{expanded ? 'Hide' : 'Show'}</Button></div>
      <DisclosurePanel>{expanded && <pre data-testid="mixed-content-body" {...stylex.props(styles.output)}><code>{JSON.stringify(item.data, null, 2)}</code></pre>}</DisclosurePanel>
    </Disclosure>
  </MessagePrimitive.Root>
}
export function UserMessage({ item }: { readonly item: TextItem & { readonly role: 'user' } }) {
  const onRetrySend = React.useContext(RetrySend)
  const [expanded, setExpanded] = React.useState(false)
  const [overflows, setOverflows] = React.useState(false)
  const text = React.useRef<HTMLSpanElement>(null)
  const textId = React.useId()
  const long = item.text.length > 600 || item.text.split('\n').length > 8
  React.useLayoutEffect(() => {
    const element = text.current
    if (element === null || expanded) return
    const measure = () => setOverflows(element.scrollHeight > element.clientHeight)
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    measure()
    return () => observer.disconnect()
  }, [item.text, expanded])
  return <MessagePrimitive.Root data-testid="user-message" data-item-id={item.id} data-send-state={(item.sendState?._tag ?? 'Sent').toLowerCase()} {...stylex.props(styles.user, item.sendState?._tag === 'Pending' && styles.userPending)}><span id={textId} ref={text} {...stylex.props(styles.userText, !expanded && styles.userClamp)}>{expanded ? item.text : item.text.slice(0, 600)}</span>{(long || overflows || expanded) && <Button aria-expanded={expanded} aria-controls={textId} onPress={() => setExpanded(!expanded)} {...stylex.props(styles.historyLoad, styles.userDisclosure)}>{expanded ? 'Show less' : 'Show full message'}</Button>}{item.sendState?._tag === 'Failed' && <SendFailure state={item.sendState} onRetry={onRetrySend === undefined ? undefined : () => onRetrySend(item.id)} />}{item.sendState?._tag === 'Unconfirmed' && <SendUnconfirmed state={item.sendState} onRetry={onRetrySend === undefined ? undefined : () => onRetrySend(item.id)} />}</MessagePrimitive.Root>
}
/** Sender, time and Copy share one header line; Copy shows on pointer hover or keyboard focus. A prose run shows the header once. */
export function SeatProse({ item, senderLine, continuation = false }: { readonly item: (TextItem & { readonly role: 'assistant' }) | MessageItem; readonly senderLine?: string; readonly continuation?: boolean }) {
  const streaming = item._tag === 'Text' && item.streaming
  const completed = streaming ? NaN : Date.parse(item.at)
  const showSeatHeader = React.useContext(SeatHeader)
  const header = !continuation && (item._tag !== 'Text' || showSeatHeader) && (senderLine !== undefined || !streaming)
  const runText = React.useContext(RunCopyText)
  const [hovered, setHovered] = React.useState(false)
  return <MessagePrimitive.Root data-testid="agent-message" data-item-id={item.id} data-item-kind={item._tag} data-run-continuation={continuation || undefined} onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)} {...stylex.props(styles.answer)}>
    {header && <div data-testid="message-header" {...stylex.props(styles.answerHeader)}>
      {senderLine !== undefined && <p data-testid="message-sender" {...stylex.props(styles.sender)}>{senderLine}</p>}
      {!streaming && <span {...stylex.props(styles.answerMeta)}>{runText === undefined ? <ActionBarPrimitive.Copy aria-label="Copy answer" {...stylex.props(styles.copy, hovered && styles.copyVisible)}><Icon name="copy" size={12} /></ActionBarPrimitive.Copy> : <RunCopy text={runText} visible={hovered} />}{Number.isFinite(completed) && <time data-testid="message-time" dateTime={new Date(completed).toISOString()}>{new Date(completed).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>}</span>}
    </div>}
    {item._tag === 'Text' ? <Markdown text={item.text} streaming={item.streaming} /> : <MessagePrimitive.Parts components={{ Text: Markdown }} />}
  </MessagePrimitive.Root>
}
/**
 * ActionBarPrimitive.Copy for a whole prose run: it writes the run text instead of this message's own text.
 * Like the primitive, the write starts inside the press; a missing API, sync throw or rejection is swallowed;
 * success sets the message's `isCopied` (`data-copied`) for 3s, cleared on unmount. A write that settles after
 * unmount or a scope change (a new generation or message id) is ignored.
 */
function RunCopy({ text, visible }: { readonly text: string; readonly visible: boolean }) {
  const aui = useAui()
  const isCopied = useAuiState(state => state.message.isCopied)
  const messageId = useAuiState(state => state.message.id)
  const timer = React.useRef<{ readonly id: number; readonly reset: (value: boolean) => void } | undefined>(undefined)
  const generation = React.useRef(0)
  const committedId = React.useRef(messageId)
  React.useInsertionEffect(() => { committedId.current = messageId }, [messageId])
  React.useEffect(() => () => {
    generation.current += 1
    if (timer.current === undefined) return
    window.clearTimeout(timer.current.id)
    timer.current.reset(false)
    timer.current = undefined
  }, [aui, messageId])
  const copy = () => {
    let write: Promise<void>
    try {
      if (typeof navigator === 'undefined' || !navigator.clipboard) return
      write = navigator.clipboard.writeText(text)
    } catch { return }
    const started = generation.current, copiedId = messageId
    const setCopied = aui.message.setIsCopied
    write.then(() => {
      if (started !== generation.current || copiedId !== committedId.current) return
      if (timer.current !== undefined) window.clearTimeout(timer.current.id)
      setCopied(true)
      timer.current = { id: window.setTimeout(() => { timer.current = undefined; setCopied(false) }, 3000), reset: setCopied }
    }, () => {})
  }
  return <Button aria-label="Copy answer" data-copied={isCopied ? 'true' : undefined} onPress={copy} {...stylex.props(styles.copy, visible && styles.copyVisible)}><Icon name="copy" size={12} /></Button>
}
/** Human intent first; a failure shows its one-line reason, the diagnostic itself requires a second disclosure. */
export function ToolDetailPreview({ call, onOpen }: { readonly call: WorkLogCall; readonly onOpen?: (call: WorkLogCall) => void }) {
  const [rawOpen, setRawOpen] = React.useState(false)
  const output = call.detail ?? (call.status === 'running' ? 'No output received yet.' : 'No output recorded.')
  const lines = output.split('\n')
  const remaining = Math.max(0, lines.length - Number(output.endsWith('\n')) - 4)
  const command = readableCommand(call)
  const showOutput = (call.detail ?? '') !== '' || call.rawInput === undefined
  return <div data-testid="tool-detail-preview" {...stylex.props(styles.preview)}>
    <p>{call.summary ?? call.title} · {call.status}</p>
    {call.status === 'error' && call.detail !== undefined && <p data-testid="tool-error-reason" {...stylex.props(styles.errorReason)}>{errorReason(call.detail)}</p>}
    <Disclosure isExpanded={rawOpen} onExpandedChange={setRawOpen}>
      <Button slot="trigger" {...stylex.props(styles.previewOpen)}>Show raw input/output</Button>
      <DisclosurePanel>{rawOpen && <>
        {call.rawInput !== undefined && <pre data-testid="tool-raw-input" {...stylex.props(styles.output)}><code {...stylex.props(styles.outputCode)}>{call.rawInput}</code></pre>}
        {command !== undefined ? <pre data-testid="tool-command" {...stylex.props(styles.output, styles.command)}><code>{command}</code></pre> : call.rawInput === undefined && call.argsSummary !== undefined && <pre {...stylex.props(styles.output)}>{call.argsSummary}</pre>}
        {showOutput && <pre {...stylex.props(styles.output)}><code {...stylex.props(styles.outputCode)}><HighlightedSource code={onOpen === undefined ? output : lines.slice(0, 4).join('\n')} language={workLogOutputLanguage(call)} /></code></pre>}
        {onOpen !== undefined && <div data-testid="tool-preview-actions" {...stylex.props(styles.previewActions)}>{remaining > 0 && <><span data-testid="tool-preview-remaining">+{remaining} {remaining === 1 ? 'line' : 'lines'}</span><span aria-hidden="true">·</span></>}<Button aria-label={`Open ${call.title} tool detail`} onPress={() => onOpen(call)} {...stylex.props(styles.previewOpen)}>Open</Button></div>}
      </>}</DisclosurePanel>
    </Disclosure>
  </div>
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
const PreparedTurn = React.memo(function PreparedTurn({ turn, now, stranded, onOpenTool, onRetryRun, landmarkContext }: { turn: TranscriptTurn; now: number; stranded?: ReadonlySet<string>; onOpenTool: TranscriptProps['onOpenTool']; onRetryRun?: () => void; landmarkContext?: string }) {
  const detail = React.useCallback((call: WorkLogCall) => <ToolDetailPreview call={call} onOpen={onOpenTool} />, [onOpenTool])
  const reasoning = turn.items.filter(item => item._tag === 'Reasoning')
  const shown = turn.items.filter(item => item._tag !== 'ToolCall' && item._tag !== 'Reasoning')
  // A shown block continues a prose run when it and the block before it are the same sender's adopted prose.
  const continues = shown.map((item, index) => [item, shown[index - 1]].every(entry => entry?._tag === 'Text' && entry.role === 'assistant' && stranded?.has(entry.id) !== true) && turn.senderCaptions?.[item.id] === turn.senderCaptions?.[shown[index - 1]!.id])
  // The block heading a run carries the run's full text, blocks joined by a blank line, for its one Copy.
  const runTexts = shown.map((item, index) => {
    if (continues[index] || !continues[index + 1]) return undefined
    const end = continues.indexOf(false, index + 1)
    return shown.slice(index, end === -1 ? undefined : end).map(entry => entry._tag === 'Text' ? entry.text : '').join('\n\n')
  })
  return <section data-testid="transcript-turn" data-item-id={turn.id} {...stylex.props(styles.turn)}>
    {turn.prompt !== undefined && (stranded?.has(turn.prompt.id) ? <StrandedItem item={turn.prompt} /> : <ThreadPrimitive.Unstable_MessageById messageId={turn.prompt.id} components={messageComponents} />)}
    {(turn.work.calls.length > 0 || reasoning.length > 0) && <WorkLogV1 turn={turn.work} ariaLabel={`Work log ${turn.id}${landmarkContext ? `, ${landmarkContext}` : ''}`} listStyle={styles.workList} renderCallDetail={detail} previewCallDetail={!turn.work.running} interactiveCalls={onOpenTool !== undefined} hideLiveRow onRetry={onRetryRun} onOpenOutput={onOpenTool} expandedBody={reasoning.map(item => <ThinkingEntry key={item.id} text={item.text} streaming={item.streaming} />)} />}
    {shown.map((item, index) => stranded?.has(item.id) ? <StrandedItem key={item.id} item={item} /> : <SenderCaption.Provider key={item.id} value={turn.senderCaptions?.[item.id]}><RunContinuation.Provider value={continues[index]!}><RunCopyText.Provider value={runTexts[index]}><ThreadPrimitive.Unstable_MessageById messageId={item.id} components={messageComponents} /></RunCopyText.Provider></RunContinuation.Provider></SenderCaption.Provider>)}
    {turn.work.running && <LiveActivity state={turn.activity ?? { _tag: 'Working' }} startedAt={turn.work.startedAt} now={now} />}
  </section>
})
/** Host clock keeps elapsed labels deterministic and pauses with the conversation clock. */
export function LiveActivity({ state, startedAt, now }: { readonly state: LiveActivityState; readonly startedAt?: string; readonly now: number }) {
  const started = Date.parse(startedAt ?? '')
  const duration = Number.isFinite(started) && started <= now ? formatWorkDuration(now - started) || '<1s' : ''
  const waiting = state._tag === 'WaitingForAnswer'
  return <div data-testid="live-work" role="status" aria-label={waiting ? 'Waiting for your answer' : 'Response in progress'} {...stylex.props(styles.liveActivity)}><span aria-hidden="true" {...stylex.props(styles.liveRing, waiting && styles.waitingRing)} /><span data-testid="live-activity-label" {...stylex.props(styles.liveLabel)}>{`${waiting ? 'Waiting for your answer' : 'Working'}${duration === '' ? '' : ` · ${duration}`}`}</span></div>
}
// Hosts using AssistantRuntimeProvider directly expose no adoption store; its absence never changes.
const staticSubscription = () => () => undefined
/** Locked U2·F3·Y3 presentation under the host's AssistantRuntimeProvider. */
export function Transcript({ turns, title, sync, now, observedAt, onOpenTool, onRetrySync, onRetryRun, onRetrySend, resolveImage, onLoadImage, renderInlineReference, availability = { _tag: 'Available' }, history = { _tag: 'Complete' }, emptyState, viewportKey, scrollToBottomKey, landmarkContext, showSeatHeader = true }: TranscriptProps) {
  const messages = useAuiState(state => state.thread.messages)
  // External-store runtimes adopt each snapshot in a passive effect, and the store publishes the
  // adopted messages a task later. Recording the snapshot after its commit is the adoption epoch.
  // An id is pending (deferred so the turn stays mounted) while its snapshot has not been through a
  // commit or while the runtime holds it and the store has yet to publish it; once committed and
  // absent from the runtime, it is stranded. Only unpublished ids depend on the epoch or on runtime
  // adoption, so neither is recorded while every id is published: a freshly mounted pane commits once.
  const [adoptionEpoch, setAdoptionEpoch] = React.useState<readonly TranscriptTurn[]>()
  const published = React.useMemo(() => new Set(messages.map(message => message.id)), [messages])
  const unpublished = React.useMemo(() => turns.flatMap(turn => [...(turn.prompt === undefined ? [] : [turn.prompt.id]), ...turn.items.map(item => item.id)]).filter(id => !published.has(id)), [turns, published])
  const adoption = React.useContext(RuntimeAdoptedIds)
  const getHeldKey = () => {
    const held = adoption?.get()
    return JSON.stringify(held === undefined ? [] : unpublished.filter(id => held.has(id)))
  }
  const heldKey = React.useSyncExternalStore(adoption?.subscribe ?? staticSubscription, getHeldKey, getHeldKey)
  const runtimeIds = React.useMemo(() => new Set<string>(JSON.parse(heldKey)), [heldKey])
  React.useEffect(() => {
    if (adoptionEpoch !== turns && unpublished.some(id => !runtimeIds.has(id))) setAdoptionEpoch(turns)
  }, [turns, unpublished, runtimeIds, adoptionEpoch])
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
  const imageOptions = React.useMemo(() => ({ resolveImage, onLoadImage, renderInlineReference }), [resolveImage, onLoadImage, renderInlineReference])
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
  const empty = committed.length === 0 && sync._tag === 'Live'
    ? <div aria-label="Empty conversation" data-testid="transcript-empty" {...stylex.props(styles.emptyBody)}><TranscriptEmptyContent emptyState={emptyState} /></div>
    : <div data-testid="transcript-placeholder" aria-label="Loading conversation" {...stylex.props(styles.placeholder)}><p role="status">Loading conversation…</p><SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} /><div aria-hidden="true" {...stylex.props(styles.turn)}><div {...stylex.props(styles.skeletonPrompt)} /><div {...stylex.props(styles.skeletonWork)} /><div {...stylex.props(styles.skeletonAnswer)} /></div></div>
  return <SeatHeader.Provider value={showSeatHeader}><MarkdownImagePolicy.Provider value={imageOptions}><RetrySend.Provider value={onRetrySend}><ThreadPrimitive.Root aria-label={`Transcript${landmarkContext ? `, ${landmarkContext}` : ''}`} {...stylex.props(styles.frame)}>
    <header data-testid="transcript-header" {...stylex.props(styles.header)}><strong {...stylex.props(styles.title)}>{title}</strong>
      {progress !== undefined && <ProgressBar aria-label="Thread synchronization" value={progress.done} maxValue={progress.total} {...stylex.props(styles.progress)}><div {...stylex.props(styles.track)}><div {...stylex.props(styles.fill(`${progress.total === 0 ? 0 : progress.done! / progress.total! * 100}%`))} /></div></ProgressBar>}
      {committed.length > 0 && <SyncLine status={sync} label="conversation" now={now} observedAt={observedAt} onRetry={onRetrySync} />}
      {running !== undefined && <><div role="progressbar" aria-label="Run in progress" aria-valuetext="Running" data-testid="run-progress" {...stylex.props(styles.runningProgress)}><span {...stylex.props(styles.runningSegment)} /></div><span data-testid="run-elapsed">Running{Number.isFinite(started) && started <= now ? ` · ${formatWorkDuration(now - started) || '<1s'}` : ''}</span></>}
    </header>
    <ErrorOverlayHost lane><EmbraceScrollViewport items={rows} stateKey={viewportKey} scrollToBottomKey={scrollToBottomKey} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.lane)} contentProps={stylex.props(readingColumnStyles.column, styles.content)}>
      {history._tag === 'HasOlder' && <div data-testid="history-boundary" {...stylex.props(styles.historyBoundary)}><span {...stylex.props(styles.historyNote)}>Earlier messages not loaded</span>{history.onLoadEarlier !== undefined && <Button onPress={history.onLoadEarlier} {...stylex.props(styles.historyLoad)}>Load earlier messages</Button>}</div>}
      {committed.length === 0 ? empty : <div {...stylex.props(styles.timeline)}>{committed.map(turn => <PreparedTurn key={turn.id} turn={turn} now={turn.work.running ? now : 0} stranded={turn.prompt !== undefined && stranded.has(turn.prompt.id) || turn.items.some(item => stranded.has(item.id)) ? stranded : undefined} onOpenTool={onOpenTool} onRetryRun={onRetryRun} landmarkContext={landmarkContext} />)}</div>}
    </EmbraceScrollViewport>{failure?.tone === 'error' && <ErrorOverlay id={`sync-${failure.text}`} title={failure.text} detail="History stays on screen." onRetry={onRetrySync} />}</ErrorOverlayHost>
  </ThreadPrimitive.Root></RetrySend.Provider></MarkdownImagePolicy.Provider></SeatHeader.Provider>
}
const runSweep = stylex.keyframes({ from: { transform: 'translateX(0%)' }, to: { transform: 'translateX(300%)' } })
const liveSpin = stylex.keyframes({ from: { transform: 'rotate(0deg)' }, to: { transform: 'rotate(360deg)' } })
const styles = stylex.create({
  frame: { minWidth: 0, minHeight: g.threadViewportMin, height: '100%', display: 'flex', flexDirection: 'column', overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  header: { height: g.band, minHeight: g.band, position: 'relative', display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { flex: '1 1 0', minWidth: 0, fontWeight: t.weightMedium, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  lane: { flex: '1 1 0', minHeight: 0, minWidth: 0, overflowY: 'auto', overflowX: 'hidden', overflowAnchor: 'none' },
  // The shared reading column keeps rows and a reading-column composer on the same bounds.
  content: { paddingBlock: s.lg }, timeline: { display: 'flex', flexDirection: 'column', gap: s.xl, minWidth: 0 }, turn: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  workList: { maxHeight: 'none', overflowY: 'visible', overscrollBehavior: 'auto' },
  user: { display: 'flex', flexDirection: 'column', minWidth: 0, color: ink.fg, fontSize: t.bodySize, lineHeight: t.bodyLeading, borderLeftWidth: g.focusRing, borderLeftStyle: 'solid', borderLeftColor: accent.primary, paddingLeft: s.lg, paddingBlock: s.xs }, userText: { minWidth: 0, overflowWrap: 'anywhere', whiteSpace: 'pre-wrap' },
  userClamp: { display: '-webkit-box', WebkitBoxOrient: 'vertical', WebkitLineClamp: 8, maxHeight: `calc(${g.userClamp} - ${s.xs} * 2 - ${g.controlSm})`, overflow: 'hidden' },
  userDisclosure: { paddingBlock: 0, lineHeight: t.metaLeading, alignSelf: 'flex-start' },
  answer: { display: 'flex', flexDirection: 'column', gap: s.xs2, paddingBlock: s.xs2, color: ink.fgSoft, flexShrink: 0 },
  answerHeader: { display: 'flex', alignItems: 'center', gap: s.sm, height: t.metaLeading, minWidth: 0, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted },
  sender: { margin: 0, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightMedium, color: ink.fgMuted },
  semantic: { display: 'flex', minWidth: 0, paddingBlock: s.xs2, color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  semanticText: { minWidth: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
  mixed: { minWidth: 0, paddingBlock: s.xs2, color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  mixedSummary: { display: 'flex', alignItems: 'center', gap: s.xs },
  mixedTrigger: { minHeight: g.toolRow, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ink.fgMuted, outlineOffset: g.focusOffset } },
  answerMeta: { display: 'flex', alignItems: 'center', gap: s.sm, flexShrink: 0, marginInlineStart: 'auto', fontVariantNumeric: 'tabular-nums' },
  // Copy keeps its place in the header and Tab order; pointer hover on the message or keyboard focus shows it.
  copy: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', opacity: { default: 0, ':focus': 1 }, transitionProperty: 'opacity', transitionDuration: { default: m.fast, '@media (prefers-reduced-motion: reduce)': '0s' }, transitionTimingFunction: m.ease, ':hover': { color: ink.fg, backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: `calc(-1 * ${g.focusRing})` } },
  copyVisible: { opacity: 1 },
  progress: { position: 'absolute', top: 0, left: 0, right: 0, height: g.progressSm }, track: { height: g.progressSm, backgroundColor: surface.rowActive, overflow: 'hidden' }, fill: (width: string) => ({ width, height: '100%', backgroundColor: accent.primary }),
  runningProgress: { position: 'absolute', top: 0, left: 0, right: 0, height: g.progressSm, overflow: 'hidden', backgroundColor: surface.rowActive },
  runningSegment: { display: 'block', width: '25%', height: '100%', backgroundColor: accent.primary, animationName: runSweep, animationDuration: m.pulse, animationTimingFunction: m.linear, animationIterationCount: 'infinite', animationDirection: 'alternate', '@media (prefers-reduced-motion: reduce)': { animationName: 'none', transform: 'translateX(150%)' } },
  liveActivity: { display: 'flex', alignItems: 'center', gap: s.sm, minHeight: g.toolRow, color: ink.fgMuted, fontSize: t.uiSize },
  liveRing: { width: g.status, height: g.status, boxSizing: 'border-box', flexShrink: 0, borderRadius: r.full, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderTopColor: ink.fgMuted, animationName: liveSpin, animationDuration: m.spin, animationTimingFunction: m.linear, animationIterationCount: 'infinite', '@media (prefers-reduced-motion: reduce)': { animationName: 'none' } },
  waitingRing: { animationName: 'none' },
  liveLabel: { color: ink.fgSoft },
  preview: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', marginBlockStart: s.xs2, marginInlineStart: g.textColumn, backgroundColor: surface.codeBg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, paddingBlock: s.md, paddingInline: s.lg, gap: s.xs },
  output: { margin: 0, width: '100%', minWidth: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.metaLeading, color: ink.fgSoft }, outputCode: { fontFamily: t.fontMono, fontSize: t.codeSize },
  command: { boxSizing: 'border-box', marginBlockEnd: s.sm, padding: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.codeBg, color: ink.fg },
  errorReason: { margin: 0, maxWidth: '100%', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: ink.fgSoft, fontSize: t.metaSize },
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
