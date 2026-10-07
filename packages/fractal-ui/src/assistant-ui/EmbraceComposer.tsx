import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { ComposerPrimitive, QueueItemPrimitive, useAui, useAuiState } from '@assistant-ui/react'
import {
  Autocomplete, Button, ListBox, ListBoxItem, Popover, Text, Token, TokenField, TokenInput,
  type Key,
} from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from './composition-tokens.stylex'
import { Icon } from './composition/Icons'
import {
  activeTrigger, defaultCommands, draftFromText, insertToken, serializeDraft,
  type Draft, type DraftToken, type MentionToken, type SerializedDraft, type SlashCommand,
} from './embrace-composer/draft'

export interface EmbraceComposerProps {
  readonly variant: 'C1' | 'C2' | 'C3'
  /** Oldest first. ArrowUp on an empty field recalls the most recent entry. */
  readonly history?: readonly string[]
  readonly tokenHistory?: readonly Draft[]
  readonly targetLabel?: string
  readonly disabledReason?: string
  readonly cancelUnavailableReason?: string
  readonly maxContentBytes?: number
  readonly toolbar?: React.ReactNode
  readonly mentionCandidates?: readonly MentionToken[]
  readonly commands?: readonly SlashCommand[]
  /** Controlled real-app TokenFieldValue, including selection and inline token segments. */
  readonly tokenDraft?: Draft
  readonly onSendDraft?: (draft: Draft) => void
  readonly onTokenDraftChange?: (draft: Draft, serialized: SerializedDraft) => void
  /** Allows the app to use its own inline subject renderer without copying its registry. */
  readonly renderToken?: (token: DraftToken) => React.ReactNode
  readonly style?: stylex.StyleXStyles
}

const styles = stylex.create({
  root: {
    backgroundColor: surface.glassFill, color: text.fg, borderWidth: g.hairline,
    borderStyle: 'solid', borderColor: border.glassBorder, borderRadius: r.slab,
    position: 'relative', padding: s.xl, display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0,
    backdropFilter: `blur(${g.blur})`,
  },
  input: {
    width: '100%', boxSizing: 'border-box', minHeight: g.editorMin, maxHeight: g.editorMax,
    overflowY: 'auto', resize: 'none', backgroundColor: surface.transparent, color: text.fg,
    borderWidth: 0, borderRadius: r.none, padding: s.xs,
    fontFamily: t.fontSans, fontSize: t.bodySize, lineHeight: t.bodyLeading,
    whiteSpace: 'pre-wrap', overflowWrap: 'anywhere',
    ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  },
  focus: { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  fieldWrap: { position: 'relative', minWidth: 0 },
  placeholder: {
    position: 'absolute', top: s.xs, left: s.xs, right: s.xs,
    color: text.fgMuted, fontSize: t.bodySize, lineHeight: t.bodyLeading, pointerEvents: 'none',
  },
  token: {
    display: 'inline', backgroundColor: surface.rowActive, color: text.fg,
    borderRadius: r.sm, paddingInline: s.xs, borderWidth: g.hairline, borderStyle: 'solid',
    borderColor: border.border, boxDecorationBreak: 'clone',
  },
  selectedToken: { backgroundColor: accent.primary, color: accent.onPrimary },
  footer: { display: 'flex', gap: s.md, flexWrap: 'wrap', alignItems: 'center' },
  grow: { flexGrow: 1, minWidth: 0 },
  help: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: text.fgMuted },
  helpVisibility: { display: 'none' },
  availableReason: { display: 'block' },
  cancelVisibility: { display: { default: 'inline-flex', ':disabled': 'none' } },
  warning: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: status.attention },
  button: {
    minHeight: g.controlMd, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.controlBorder,
    borderRadius: r.control, backgroundColor: surface.controlFill, color: text.fg,
    paddingBlock: s.xs, paddingInline: s.md, fontFamily: t.fontSans, fontSize: t.metaSize,
    cursor: { default: 'pointer', ':disabled': 'not-allowed' }, opacity: { default: 1, ':disabled': 0.64 },
    ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  },
  primary: { width: g.controlLg, height: g.controlLg, minHeight: g.controlLg, padding: 0, borderRadius: r.full, backgroundColor: accent.primary, color: accent.onPrimary, display: 'inline-flex', alignItems: 'center', justifyContent: 'center' },
  historyVisibility: { display: 'inline-flex' },
  menu: {
    backgroundColor: surface.raised, color: text.fg, borderWidth: g.hairline,
    borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.base,
    width: g.tooltipMax, maxWidth: `calc(100vw - ${s.panel})`, maxHeight: g.previewMax, overflowY: 'auto', padding: s.sm,
  },
  menuHeader: { padding: s.sm, color: text.fgMuted, fontSize: t.denseSize },
  option: { padding: s.md, borderRadius: r.sm, fontSize: t.uiSize },
  focusedOption: { backgroundColor: surface.rowActive },
  unavailable: { opacity: 0.64 },
  description: { display: 'block', color: text.fgMuted, fontSize: t.denseSize, marginTop: s.xs2 },
  queue: { display: 'flex', flexDirection: 'column', gap: s.sm },
  queueRow: { display: 'flex', alignItems: 'center', gap: s.sm, backgroundColor: surface.codeBg, borderRadius: r.sm, padding: s.sm },
  queueText: { flexGrow: 1, fontSize: t.metaSize, minWidth: 0, overflowWrap: 'anywhere' },
})

const emptyHistory: readonly string[] = []
const emptyCandidates: readonly MentionToken[] = []

/** Requires the same assistant-ui thread/runtime provider as the transcript. */
export function EmbraceComposer(props: EmbraceComposerProps) {
  return props.variant === 'C1' ? <PlainComposer {...props} /> : <TokenComposer {...props} />
}

function PlainComposer({ targetLabel, disabledReason, toolbar, style }: EmbraceComposerProps) {
  const isDisabled = useAuiState((s) => s.thread.isDisabled)
  const readOnly = disabledReason !== undefined || isDisabled
  const helpId = React.useId()
  return (
    <ComposerPrimitive.Root
      {...stylex.props(styles.root, style)}
      onSubmit={(event) => { if (readOnly) event.preventDefault() }}
    >
      <ComposerPrimitive.Input
        aria-label={targetLabel === undefined ? 'Message' : `Message to ${targetLabel}`}
        aria-describedby={helpId}
        placeholder="Message — Enter to send, Shift+Enter for a new line"
        disabled={readOnly}
        {...stylex.props(styles.input)}
      />
      <div {...stylex.props(styles.footer)}>
        <div id={helpId} {...stylex.props(styles.help, styles.grow)}>
          {disabledReason ?? (isDisabled ? 'This conversation is read-only.' : targetLabel === undefined ? 'Enter to send · Shift+Enter for a new line' : `To ${targetLabel}`)}
        </div>
        {toolbar}
        <ComposerPrimitive.Send disabled={readOnly} {...stylex.props(styles.button, styles.primary)}>Send</ComposerPrimitive.Send>
      </div>
    </ComposerPrimitive.Root>
  )
}

interface Option {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly token: DraftToken
  readonly unavailable?: string
}

function TokenComposer({
  variant, history = emptyHistory, tokenHistory, targetLabel, disabledReason, cancelUnavailableReason, maxContentBytes, toolbar,
  mentionCandidates = emptyCandidates, commands = defaultCommands,
  tokenDraft, onTokenDraftChange, onSendDraft, renderToken, style,
}: EmbraceComposerProps) {
  const aui = useAui()
  const composerStateNow = () => aui.composer.__internal_getRuntime?.().getState() ?? aui.composer.getState()
  const runtimeText = useAuiState((s) => s.composer.text)
  const isDisabled = useAuiState((s) => s.thread.isDisabled)
  const isEditing = useAuiState((s) => s.composer.isEditing)
  const isRunning = useAuiState((s) => s.thread.isRunning)
  const canQueue = useAuiState((s) => s.thread.capabilities.queue)
  const canCancel = useAuiState((s) => s.composer.canCancel)
  const queuedCount = useAuiState((s) => s.composer.queue.length)
  const readOnly = disabledReason !== undefined || isDisabled || !isEditing
  const [local, setLocal] = React.useState(() => ({ draft: draftFromText(runtimeText), runtimeText }))
  // Runtime resets, thread switches, and native suggestion/action writes invalidate the local
  // token snapshot. A controlled app draft remains app-owned; no effect synchronizes two stores.
  const draft = tokenDraft ?? (local.runtimeText === runtimeText ? local.draft : draftFromText(runtimeText))
  const serialized = serializeDraft(draft)
  const tooLarge = maxContentBytes !== undefined && new TextEncoder().encode(serialized.content).length > maxContentBytes
  const [dismissed, setDismissed] = React.useState<string | undefined>()
  const inputRef = React.useRef<HTMLDivElement>(null)
  const composing = React.useRef(false)
  const recall = React.useRef<{ index: number; original: Draft } | undefined>(undefined)
  const helpId = React.useId()
  const trigger = activeTrigger(draft)
  const triggerKey = trigger === undefined ? undefined : `${trigger.index}:${trigger.start}:${trigger.query}`
  const open = !readOnly && trigger !== undefined && triggerKey !== dismissed
  const query = trigger?.query.toLowerCase() ?? ''
  const options: readonly Option[] = !open ? [] : (trigger.kind === '@'
    ? mentionCandidates.map((token) => ({ id: token.ref, label: `@${token.label}`, description: token.ref, token }))
    : commands.map((command) => ({
        id: command.id, label: command.label, description: command.description,
        token: { _tag: 'Command' as const, command: command.id }, unavailable: command.unavailable,
      })))
    .filter((option) => `${option.label} ${option.description}`.toLowerCase().includes(query))

  const bridge = (next: Draft) => {
    const value = serializeDraft(next)
    const runConfig = composerStateNow().runConfig
    aui.composer.setRunConfig({ ...runConfig, custom: { ...runConfig.custom, embraceDraft: value } })
    aui.composer.setText(value.content)
    return value
  }
  const changeDraft = (next: Draft) => {
    const value = bridge(next)
    setLocal({ draft: next, runtimeText: value.content })
    onTokenDraftChange?.(next, value)
    if (activeTrigger(next) === undefined) setDismissed(undefined)
  }
  const submit = () => {
    if (readOnly || tooLarge || composing.current || serialized.content === '' || (isRunning && !canQueue)) return
    bridge(draft)
    // The store client snapshot updates on React commit; the native runtime reflects this bridge immediately.
    if (!composerStateNow().canSend) return
    // Explicit false matters: native send defaults to steering during an active queued run.
    onSendDraft?.(draft)
    aui.composer.send({ steer: false })
    recall.current = undefined
    if (composerStateNow().text === '') {
      const empty = draftFromText('')
      setLocal({ draft: empty, runtimeText: '' })
      onTokenDraftChange?.(empty, serializeDraft(empty))
    }
  }
  const pick = (key: Key) => {
    const option = options.find((candidate) => candidate.id === key)
    if (readOnly || trigger === undefined || option === undefined || option.unavailable !== undefined) return
    recall.current = undefined
    changeDraft(insertToken({ draft, trigger, token: option.token,
      text: option.token._tag === 'Mention' ? `@${option.token.ref}` : option.label }))
    inputRef.current?.focus()
  }
  const recallHistory = (direction: 'previous' | 'next') => {
    if (readOnly || history.length === 0) return
    const state = recall.current ?? { index: history.length, original: draft }
    const index = Math.max(0, Math.min(history.length, state.index + (direction === 'previous' ? -1 : 1)))
    recall.current = index === history.length ? undefined : { ...state, index }
    changeDraft(index === history.length ? state.original : tokenHistory?.[index] ?? draftFromText(history[index]!))
    inputRef.current?.focus()
  }
  const isEmpty = draft.segments.every((segment) => segment.type === 'text' && segment.text === '')
  const canSubmit = !readOnly && !tooLarge && serialized.content !== '' && (!isRunning || canQueue)

  return (
    <ComposerPrimitive.Root
      {...stylex.props(styles.root, style)}
      onSubmit={(event) => { event.preventDefault(); if (!open) submit() }}
      onCompositionStartCapture={() => { composing.current = true }}
      onCompositionEndCapture={() => { composing.current = false }}
      onKeyDownCapture={(event) => {
        if (event.nativeEvent.isComposing || composing.current || event.nativeEvent.keyCode === 229) {
          if (event.key === 'Enter') event.stopPropagation()
          return
        }
        if (variant === 'C3' && event.key === 'Escape' && !open && canCancel && !readOnly) {
          event.preventDefault()
          aui.composer.cancel()
        }
      }}
    >
      <Autocomplete>
        <div {...stylex.props(styles.fieldWrap)}>
          {/* Input asChild/render still imposes a string value, target.value/selectionStart,
              HTMLTextAreaElement ref and setSelectionRange. RAC TokenInput is contenteditable
              with TokenFieldValue segment/caret state. It cannot implement that native contract.
              This is the real app TokenField inside Root, bridged explicitly to composer text. */}
          <TokenField<Draft>
            aria-label={targetLabel === undefined ? 'Message with mentions and commands' : `Message to ${targetLabel}`}
            aria-describedby={helpId}
            value={draft}
            onChange={(next) => {
              if (readOnly) return
              if (next.segments !== draft.segments) recall.current = undefined
              changeDraft(next)
            }}
            allowsNewlines
            isReadOnly={readOnly}
            onSubmit={() => { if (!open || options.length === 0) submit() }}
            onKeyDown={(event) => {
              // RAC exposes this handler at both field and editable scopes.
              // Respect the consumed event when it bubbles so history moves once.
              if (event.defaultPrevented || readOnly || composing.current || ('isComposing' in event && event.isComposing)) return
              if (event.key === 'Escape' && open) { setDismissed(triggerKey); return }
              if (variant !== 'C3' || open || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return
              if (event.key === 'ArrowUp' && (isEmpty || recall.current !== undefined) && history.length > 0) {
                event.preventDefault(); recallHistory('previous')
              } else if (event.key === 'ArrowDown' && recall.current !== undefined) {
                event.preventDefault(); recallHistory('next')
              }
            }}
          >
            {isEmpty ? <span aria-hidden="true" {...stylex.props(styles.placeholder)}>Message — @ to mention, / for commands</span> : null}
            <TokenInput<Draft>
              ref={inputRef}
              className={({ isFocusVisible }) => stylex.props(styles.input, isFocusVisible && styles.focus).className ?? ''}
            >
              {(segment) => <Token className={({ isSelected }) => stylex.props(styles.token, isSelected && styles.selectedToken).className ?? ''}>
                {segment.value === undefined ? segment.text : renderToken?.(segment.value) ??
                  (segment.value._tag === 'Mention' ? `@${segment.value.label}` : segment.text)}
              </Token>}
            </TokenInput>
          </TokenField>
          {open ? <Popover triggerRef={inputRef} isOpen isNonModal placement="top start"
            onOpenChange={(next) => { if (!next) setDismissed(triggerKey) }} {...stylex.props(styles.menu)}>
            <div {...stylex.props(styles.menuHeader)}>{trigger.kind === '@' ? 'Mention a subject' : 'Commands'}</div>
            <ListBox aria-label={trigger.kind === '@' ? 'Mention a subject' : 'Commands'} items={options}
              onAction={pick} disabledKeys={options.filter((option) => option.unavailable !== undefined).map((option) => option.id)}
              renderEmptyState={() => <div {...stylex.props(styles.option)}>No matches</div>}>
              {(option) => <ListBoxItem id={option.id} textValue={option.label}
                className={({ isFocused, isDisabled: optionDisabled }) => stylex.props(styles.option, isFocused && styles.focusedOption, optionDisabled && styles.unavailable).className ?? ''}>
                <Text slot="label">{option.label}</Text>
                <Text slot="description" {...stylex.props(styles.description)}>{option.unavailable ?? option.description}</Text>
              </ListBoxItem>}
            </ListBox>
          </Popover> : null}
        </div>
      </Autocomplete>
      <div id={helpId} {...stylex.props(styles.help, styles.helpVisibility, (disabledReason !== undefined || isDisabled) && styles.availableReason)}>
        {disabledReason ?? (isDisabled ? 'This conversation is read-only.' :
          `${targetLabel === undefined ? '' : `To ${targetLabel} · `}Enter to ${variant === 'C3' && isRunning && canQueue ? 'queue' : 'send'} · Shift+Enter for a new line${variant === 'C3' ? ` · ↑ on empty recalls history${canCancel ? ' · Esc cancels' : ''}` : ''}`)}
      </div>
      {tooLarge ? <div role="status" {...stylex.props(styles.warning)}>Message exceeds the {maxContentBytes} byte inline limit.</div> : null}
      {variant === 'C3' && isRunning && !canQueue ? <div {...stylex.props(styles.warning)}>This runtime does not support queuing. Sending is unavailable while the run is active.</div> : null}
      <div {...stylex.props(styles.footer)}>
        {toolbar}
        <div {...stylex.props(styles.grow)} />
        {variant === 'C3' ? <>
          <Button isDisabled={readOnly || history.length === 0} onPress={() => recallHistory('previous')}
            aria-description="Recall previous message (ArrowUp on empty)" {...stylex.props(styles.button, styles.historyVisibility)} aria-label="Recall previous message"><Icon name="clock" /></Button>
          <ComposerPrimitive.Cancel disabled={readOnly || !canCancel} title={canCancel ? undefined : cancelUnavailableReason} {...stylex.props(styles.button, styles.cancelVisibility)}>Cancel run</ComposerPrimitive.Cancel>
        </> : null}
        {/* A controlled TokenField can contain a draft before runtime text is bridged.
            The explicit adapter submits after bridging instead of inheriting Input's canSend gate. */}
        <Button isDisabled={!canSubmit} onPress={submit} aria-label={variant === 'C3' && isRunning && canQueue ? 'Queue message' : 'Send'}
          {...stylex.props(styles.button, styles.primary)}>
          <Icon name="send" />
        </Button>
      </div>
      {variant === 'C3' && queuedCount > 0 ? <div aria-label="Queued messages" {...stylex.props(styles.queue)}>
        <div role="status" {...stylex.props(styles.help)}>{queuedCount} queued</div>
        <ComposerPrimitive.Queue>{({ queueItem }) => <div {...stylex.props(styles.queueRow)}>
          <QueueItemPrimitive.Text {...stylex.props(styles.queueText)} />
          <QueueItemPrimitive.Steer disabled={readOnly} {...stylex.props(styles.button)}>Run next</QueueItemPrimitive.Steer>
          <QueueItemPrimitive.Remove disabled={readOnly} aria-label={`Remove queued message ${queueItem.id}`}
            {...stylex.props(styles.button)}>Remove</QueueItemPrimitive.Remove>
        </div>}</ComposerPrimitive.Queue>
      </div> : null}
    </ComposerPrimitive.Root>
  )
}
