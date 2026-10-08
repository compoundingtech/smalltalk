import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import {
  ActionBarPrimitive, AttachmentPrimitive, BranchPickerPrimitive,
  ComposerPrimitive, ErrorPrimitive, MessagePrimitive,
  ThreadPrimitive, useAuiState,
} from '@assistant-ui/react'
import { readingColumnStyles } from './reading-column.stylex'
import type { ConversationItem } from './embrace-data/model'
import { accentTokens, tokens, geometry, density } from './embrace-tokens.stylex'
import { EmbraceVirtualConversation } from './EmbraceVirtualConversation'
import { WorkLogV1 } from './taste/WorkLogV1'
import { committedWorkLogs, type WorkLogProjection } from './taste/work-log'
import { EmbraceComposer, type EmbraceComposerProps } from './EmbraceComposer'
import { EmbraceToolCall, EmbraceToolRegistrations, EmbraceToolRun, EmbraceToolVariantContext, type ToolVariant } from './EmbraceToolCall'
import { EmbraceScrollViewport } from './EmbraceScrollViewport'
import { Markdown } from './composition/Markdown'
import { ThinkingEntry } from './composition/ThinkingEntry'
import { SendFailure, TranscriptEmptyContent, type TranscriptEmptyState } from './composition/Transcript'

export type EmbraceLevel = 'E1' | 'E2' | 'E3' | 'E4'
export type SenderVariant = 'S1' | 'S2' | 'S3'
export interface EmbraceThreadProps {
  /** Exact visible ConversationItem snapshot supplied to useExternalStoreRuntime. */
  readonly items: readonly ConversationItem[]
  /** Caller-owned lifecycle facts; grouping is validated against committed runtime references. */
  readonly workLogs?: readonly WorkLogProjection[]
  /** Host opt-in: align every actual transcript row with the live composer lane. */
  readonly readingColumn?: boolean
  readonly embrace?: EmbraceLevel
  readonly tools?: ToolVariant
  readonly senders?: SenderVariant
  readonly composer?: 'C1' | 'C2' | 'C3' | false
  readonly history?: readonly string[]
  readonly targetLabel?: string
  readonly disabledReason?: string
  /** E4 owns in-agent thread lists only, never the workbench roster. */
  readonly threadList?: React.ReactNode
  readonly toolbar?: React.ReactNode
  readonly composerProps?: Omit<EmbraceComposerProps, 'variant'>
  readonly onCommit?: React.ProfilerOnRenderCallback
  /** Caller-owned workbench layout overrides the workshop's default frame. */
  readonly style?: stylex.StyleXStyles
  /** Copy for a thread with no messages; defaults to a neutral "No messages yet". */
  readonly emptyState?: React.ReactNode | TranscriptEmptyState
}
interface DisplayOptions { embrace: EmbraceLevel; senders: SenderVariant; showSender: boolean }
const DisplayContext = React.createContext<DisplayOptions>({ embrace: 'E1', senders: 'S2', showSender: true })

function Attachment() {
  return <AttachmentPrimitive.Root {...stylex.props(styles.attachment)}>
    <AttachmentPrimitive.Name />
  </AttachmentPrimitive.Root>
}
function TextPart({ text }: { text: string }) {
  const item = useAuiState(state => state.message.metadata.custom.item) as ConversationItem | undefined
  return <Markdown text={text} streaming={item?._tag === 'Text' && item.streaming} />
}
function ReasoningPart({ text }: { text: string }) {
  const item = useAuiState(state => state.message.metadata.custom.item) as ConversationItem | undefined
  return <ThinkingEntry text={text} streaming={item?._tag === 'Reasoning' && item.streaming} />
}
export function EmbraceMessage() {
  const options = React.useContext(DisplayContext)
  const role = useAuiState(s => s.message.role)
  const custom = useAuiState(s => s.message.metadata.custom)
  const editing = useAuiState(s => s.message.composer.isEditing)
  const item = custom.item as ConversationItem | undefined
  const sender = custom.sender as { label?: string; kind?: string } | undefined
  const label = options.senders === 'S1' ? role : sender?.label ?? sender?.kind ?? role
  return <MessagePrimitive.Root data-testid="transcript-message" data-item-id={item?.id} data-send-state={item?._tag === 'Text' && item.role === 'user' ? (item.sendState?._tag ?? 'Sent').toLowerCase() : undefined} {...stylex.props(styles.message, role === 'user' && styles.userMessage, item?._tag === 'Text' && item.role === 'user' && item.sendState?._tag === 'Pending' && styles.userPending)}>
    {options.showSender ? <header {...stylex.props(styles.sender)}>
      {options.senders === 'S1' ? null : <span aria-hidden="true" {...stylex.props(styles.avatar)}>{label.slice(0, 2).toUpperCase()}</span>}
      <strong {...stylex.props(styles.senderName)}>{label}</strong>
    </header> : null}
    {editing ? <ComposerPrimitive.Root {...stylex.props(styles.edit)}><ComposerPrimitive.Input aria-label="Edit message" {...stylex.props(styles.input)} /><ComposerPrimitive.Send {...stylex.props(styles.button)}>Save edit</ComposerPrimitive.Send><ComposerPrimitive.Cancel {...stylex.props(styles.button)}>Cancel edit</ComposerPrimitive.Cancel></ComposerPrimitive.Root> : <><MessagePrimitive.Parts components={{ Text: TextPart, Reasoning: ReasoningPart, tools: { Fallback: EmbraceToolCall } }} />{item?._tag === 'Text' && item.role === 'user' && item.sendState?._tag === 'Failed' && <SendFailure state={item.sendState} />}</>}
    {options.embrace !== 'E1' ? <>
      <MessagePrimitive.Attachments components={{ Attachment }} />
      <MessagePrimitive.Error><ErrorPrimitive.Root {...stylex.props(styles.error)}><ErrorPrimitive.Message /></ErrorPrimitive.Root></MessagePrimitive.Error>
      <ActionBarPrimitive.Root hideWhenRunning={false} {...stylex.props(styles.actions)}>
        <ActionBarPrimitive.Copy {...stylex.props(styles.button)}>Copy message</ActionBarPrimitive.Copy>
        <ActionBarPrimitive.Edit {...stylex.props(styles.button)}>Edit message</ActionBarPrimitive.Edit>
        <ActionBarPrimitive.Reload {...stylex.props(styles.button)}>Regenerate</ActionBarPrimitive.Reload>
        <BranchPickerPrimitive.Root hideWhenSingleBranch={false} {...stylex.props(styles.actions)}>
          <BranchPickerPrimitive.Previous aria-label="Previous branch" {...stylex.props(styles.button)}>←</BranchPickerPrimitive.Previous>
          <span>Branch <BranchPickerPrimitive.Number /> / <BranchPickerPrimitive.Count /></span>
          <BranchPickerPrimitive.Next aria-label="Next branch" {...stylex.props(styles.button)}>→</BranchPickerPrimitive.Next>
        </BranchPickerPrimitive.Root>
      </ActionBarPrimitive.Root>
    </> : null}
  </MessagePrimitive.Root>
}
const messageComponents = { Message: EmbraceMessage, EditComposer: EmbraceMessage }
const participant = (item: ConversationItem) => JSON.stringify('sender' in item ? item.sender : { _tag: 'system' })
interface TranscriptRow { id: string; indices: readonly number[]; work?: WorkLogProjection; liveWork?: WorkLogProjection; toolItems?: readonly Extract<ConversationItem, { _tag: 'ToolCall' }>[] }
function rowsFor(items: readonly ConversationItem[], tools: ToolVariant, workLogs: readonly WorkLogProjection[]): readonly TranscriptRow[] {
  const rows: TranscriptRow[] = []
  const workAt = committedWorkLogs(items, workLogs)
  for (let index = 0; index < items.length; index++) {
    const item = items[index]!
    const work = workAt.get(index)
    if (work?.mode === 'settled') {
      rows.push({ id: item.id, indices: work.items.map((_, offset) => index + offset), work })
      index += work.items.length - 1
      continue
    }
    const previous = rows.at(-1)
    if (tools === 'grouped' && item._tag === 'ToolCall') {
      if (previous?.toolItems && participant(previous.toolItems[0]!) === participant(item) && work === undefined) {
        rows[rows.length - 1] = { ...previous, indices: [...previous.indices, index], toolItems: [...previous.toolItems, item] }
      } else rows.push({ id: item.id, indices: [index], toolItems: [item], liveWork: work })
    } else rows.push({ id: item.id, indices: [index], liveWork: work })
  }
  return rows
}
const noWorkLogs: readonly WorkLogProjection[] = []
/** Render under AssistantRuntimeProvider. The app keeps ownership of stores and callbacks. */
export function EmbraceThread({ items: snapshot, workLogs, readingColumn = false, embrace = 'E1', tools = 'rows', senders = 'S2', composer = 'C1', history, targetLabel, disabledReason, threadList, toolbar, composerProps, onCommit, emptyState, style }: EmbraceThreadProps) {
  const messages = useAuiState(state => state.thread.messages)
  // The adapter commits after React renders its new input snapshot. Use the
  // runtime's committed identities and source references so RAC never measures
  // a temporarily missing message as a zero-height row, and branches stay aligned.
  const items = React.useMemo(() => {
    const available = new Set(snapshot.map(item => item.id))
    return messages.flatMap(message => {
      const item = message.metadata.custom.item as ConversationItem | undefined
      return item !== undefined && available.has(item.id) ? [item] : []
    })
  }, [messages, snapshot])
  // The controlled turn owner decides folding; fallback grouping remains fully expanded below.
  const rows = React.useMemo(() => rowsFor(items, tools, workLogs ?? noWorkLogs), [items, tools, workLogs])
  const renderMessage = (index: number) => {
    const item = items[index]!
    const previous = items[index - 1]
    const repeatToolSender = tools === 'grouped' && item._tag === 'ToolCall' && previous?._tag === 'ToolCall' && participant(previous) === participant(item)
    const showSender = (senders !== 'S3' || index === 0 || participant(previous!) !== participant(item)) && !repeatToolSender
    return <DisplayContext.Provider key={item.id} value={{ embrace, senders, showSender }}>
      <ThreadPrimitive.Unstable_MessageById messageId={item.id} components={messageComponents} />
    </DisplayContext.Provider>
  }
  const renderRowContent = (row: TranscriptRow) => row.work
    ? <WorkLogV1 key={row.work.id} turn={row.work.turn} listStyle={styles.workList} renderCallDetail={call => renderMessage(row.indices.find(index => items[index]!.id === call.id)!)} />
    : <>{row.liveWork ? <WorkLogV1 key={row.liveWork.id} turn={row.liveWork.turn} /> : null}{row.toolItems ? <EmbraceToolRun forceExpanded={workLogs !== undefined} items={row.toolItems} renderItem={item => renderMessage(row.indices.find(index => items[index]!.id === item.id)!)} /> : renderMessage(row.indices[0]!)}</>
  const renderRow = (row: TranscriptRow) => readingColumn ? <div data-testid="transcript-reading-column" {...stylex.props(readingColumnStyles.column)}>{renderRowContent(row)}</div> : renderRowContent(row)
  const transcript = embrace === 'E3'
    ? <EmbraceScrollViewport items={rows} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.viewport)}>{rows.map(row => <React.Fragment key={row.id}>{renderRow(row)}</React.Fragment>)}</EmbraceScrollViewport>
    : <EmbraceVirtualConversation items={rows} renderItem={renderRow} />
  return <EmbraceToolVariantContext.Provider value={tools}>
    <EmbraceToolRegistrations />
    <ThreadPrimitive.Root {...stylex.props(styles.root, readingColumn && styles.readingRoot, style)}>
      {embrace === 'E4' ? threadList : null}
      <section aria-label="Conversation" {...stylex.props(styles.conversation)}>
        <React.Profiler id="embrace-transcript" onRender={onCommit ?? (() => {})}>{transcript}</React.Profiler>
        <ThreadPrimitive.Empty><div {...stylex.props(styles.empty)}><TranscriptEmptyContent emptyState={emptyState} /></div></ThreadPrimitive.Empty>
        {embrace !== 'E1' ? <ThreadPrimitive.Suggestion prompt="Review the latest change" {...stylex.props(styles.button)}>Review the latest change</ThreadPrimitive.Suggestion> : null}
        {composer === false ? null : <EmbraceComposer history={history} targetLabel={targetLabel} disabledReason={disabledReason} toolbar={toolbar} {...composerProps} variant={composer} />}
      </section>
    </ThreadPrimitive.Root>
  </EmbraceToolVariantContext.Provider>
}
const styles = stylex.create({
  readingRoot: { borderWidth: 0, borderRadius: 0 },
  workList: { maxHeight: 'none', overflowY: 'visible', overscrollBehavior: 'auto' },
  root: { display: 'flex', width: '100%', minWidth: 0, height: '100%', flex: 1, minHeight: 0, backgroundColor: tokens.panel, color: tokens.ink, borderWidth: '1px', borderStyle: 'solid', borderColor: tokens.line, borderRadius: geometry.frameRadius, overflow: 'hidden', fontFamily: geometry.bodyFont, fontSize: density.bodySize },
  conversation: { display: 'flex', flexDirection: 'column', flex: 1, minWidth: 0, minHeight: 0 },
  viewport: { flex: 1, minHeight: 0, overflowY: 'auto', padding: density.messageX, overflowAnchor: 'none' },
  message: { paddingBlock: density.messageY, paddingInline: `calc(${density.messageX} + ${geometry.messageExtraInset})`, borderBottomWidth: geometry.messageRule, borderBottomStyle: 'solid', borderBottomColor: tokens.line },
  userMessage: { backgroundColor: geometry.userBackground, borderInlineStartWidth: geometry.userMarker, borderInlineStartStyle: 'solid', borderInlineStartColor: tokens.line },
  sender: { display: 'flex', alignItems: 'center', gap: density.gap, marginBottom: density.senderBottom, fontSize: density.senderSize, fontFamily: geometry.senderFont },
  senderName: { fontWeight: geometry.senderWeight, letterSpacing: geometry.senderTracking, textTransform: geometry.senderTransform },
  avatar: { display: geometry.avatarDisplay, placeItems: 'center', width: density.avatarSize, height: density.avatarSize, backgroundColor: tokens.recess, borderRadius: geometry.avatarRadius, color: tokens.muted, fontSize: '10px' },
  muted: { color: tokens.muted },
  actions: { display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: '6px', marginTop: '6px', fontSize: '11px' },
  button: { backgroundColor: tokens.recess, color: tokens.ink, borderWidth: '1px', borderStyle: 'solid', borderColor: tokens.line, borderRadius: '4px', paddingBlock: '4px', paddingInline: '8px', fontSize: '11px', cursor: 'pointer', ':focus-visible': { outline: `2px solid ${accentTokens.accent}`, outlineOffset: '2px' }, ':disabled': { opacity: 0.45, cursor: 'not-allowed' } },
  attachment: { display: 'inline-flex', padding: '6px', borderWidth: '1px', borderStyle: 'solid', borderColor: tokens.line, borderRadius: '4px' },
  error: { color: tokens.danger, paddingBlock: '6px' },
  edit: { display: 'flex', flexDirection: 'column', gap: '6px' },
  input: { backgroundColor: tokens.panel, color: tokens.ink, borderColor: tokens.line, padding: '8px' },
  empty: { color: tokens.muted, padding: '20px', margin: 0 },
  userPending: { color: tokens.muted },
})
