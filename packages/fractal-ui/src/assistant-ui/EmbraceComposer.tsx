import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { ComposerPrimitive, QueueItemPrimitive, useAui, useAuiState } from '@assistant-ui/react'
import {
  Autocomplete, Button, Focusable, Header, ListBox, ListBoxItem, ListBoxSection, Menu, MenuItem, MenuTrigger, Popover, Text, Token, TokenField, TokenInput, Tooltip, TooltipTrigger,
  type Key,
} from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from './composition-tokens.stylex'
import { readingColumnStyles } from './reading-column.stylex'
import { Icon } from './composition/Icons'
import { EffortPicker } from './embrace-composer/EffortPicker'
import {
  activeTrigger, defaultCommands, draftFromText, insertToken, serializeDraft,
  type Draft, type DraftToken, type MentionToken, type SerializedDraft, type SlashCommand,
} from './embrace-composer/draft'

export interface EmbraceComposerProps {
  readonly variant: 'C1' | 'C2' | 'C3'
  /** Text-only capability, independent of the selected layout variant. */
  readonly plainText?: boolean
  /** Host opt-in: bound the composer to the shared reading column beside the transcript. */
  readonly readingColumn?: boolean
  readonly input?: (inputStyle: stylex.StyleXStyles, descriptionId: string) => React.ReactNode
  readonly actions?: (compact: boolean) => React.ReactNode
  /** Oldest first. ArrowUp on an empty field recalls the most recent entry. */
  readonly history?: readonly string[]
  /** Structured drafts parallel to `history`; recall restores chips instead of re-parsing text. */
  readonly tokenHistory?: readonly Draft[]
  readonly targetLabel?: string
  readonly disabledReason?: string
  /** Shown on the C3 cancel control when the runtime cannot cancel the active run. */
  readonly cancelUnavailableReason?: string
  /** Inline content budget; larger drafts stay editable but cannot be sent. */
  readonly maxContentBytes?: number
  readonly toolbar?: React.ReactNode
  readonly mentionCandidates?: readonly MentionToken[]
  /** One source-owned distinguishing hint per subject; identifiers remain accessible separately. */
  readonly mentionHints?: ReadonlyMap<string, string>
  readonly commands?: readonly SlashCommand[]
  /** Controlled real-app TokenFieldValue, including selection and inline token segments. */
  readonly tokenDraft?: Draft
  /** Called with the structured draft immediately before the native runtime send. */
  readonly onSendDraft?: (draft: Draft) => void
  readonly onTokenDraftChange?: (draft: Draft, serialized: SerializedDraft) => void
  /** Allows the app to use its own inline subject renderer without copying its registry. */
  readonly renderToken?: (token: DraftToken) => React.ReactNode
  readonly style?: stylex.StyleXStyles
  /** Optional host-owned keyboard policy; structured draft bridging still happens here. */
  readonly onRequestSubmit?: (modified: boolean) => void
  readonly submitLabel?: string
  /** Renders the submit action icon-only (named by the submit label) where the footer must stay narrow. */
  readonly submitIcon?: React.ReactNode
  readonly inputStyle?: stylex.StyleXStyles
  readonly fieldStyle?: stylex.StyleXStyles
  readonly footerStyle?: stylex.StyleXStyles
  readonly showQueue?: boolean
  /** Capability gate that does not make an offline draft read-only. */
  readonly sendDisabled?: boolean
}

const styles = stylex.create({
  root: {
    backgroundColor: surface.glassFill, color: text.fg, borderWidth: g.hairline,
    borderStyle: 'solid', borderColor: border.glassBorder, borderRadius: r.slab,
    position: 'relative', padding: s.xl, display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0,
    backdropFilter: `blur(${g.blur})`,
  },
  input: {
    display: 'block',
    width: '100%', boxSizing: 'border-box', minHeight: g.editorMin, maxHeight: g.editorMax,
    overflowY: 'auto', resize: 'none', backgroundColor: surface.transparent, color: text.fg,
    borderWidth: 0, borderRadius: r.none, padding: s.xs,
    fontFamily: t.fontSans, fontSize: t.bodySize, lineHeight: t.bodyLeading,
    whiteSpace: 'pre-wrap', overflowWrap: 'anywhere',
    ':focus-visible': { outlineStyle: 'none' },
  },
  fieldWrap: { position: 'relative', minWidth: 0 },
  placeholder: {
    position: 'absolute', top: s.xs, left: s.xs, right: s.xs,
    color: text.fgMuted, fontSize: t.bodySize, lineHeight: t.bodyLeading, pointerEvents: 'none',
    whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis',
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
    minHeight: g.controlMd, flexShrink: 0, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.controlBorder,
    borderRadius: r.control, backgroundColor: surface.controlFill, color: text.fg,
    paddingBlock: s.xs, paddingInline: s.md, fontFamily: t.fontSans, fontSize: t.metaSize,
    cursor: { default: 'pointer', ':disabled': 'not-allowed' }, opacity: { default: 1, ':disabled': 0.64 },
    ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  },
  primary: { backgroundColor: accent.primary, color: accent.onPrimary },
  submitIcon: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', minWidth: g.controlMd, paddingInline: s.xs },
  menu: {
    backgroundColor: surface.raised, color: text.fg, borderWidth: g.hairline,
    borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.base,
    width: `min(${g.modalMax}, var(--trigger-width))`, maxWidth: `calc(100vw - ${s.panel})`, maxHeight: g.tooltipMax, overflowY: 'auto', padding: s.xs,
  },
  menuList: { maxHeight: `calc(${g.tooltipMax} - ${s.lg})`, overflowY: 'auto', outlineStyle: 'none' },
  menuHeader: { position: 'sticky', top: s.zero, zIndex: 1, height: g.controlSm, boxSizing: 'border-box', paddingInline: s.sm, display: 'flex', alignItems: 'center', backgroundColor: surface.raised, color: text.fgMuted, fontSize: t.denseSize },
  option: { height: g.controlMd, minHeight: g.controlMd, boxSizing: 'border-box', paddingInline: s.sm, borderRadius: r.sm, fontSize: t.uiSize, display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, whiteSpace: 'nowrap' },
  optionLabel: { flex: '0 0 auto', width: 'max-content', maxWidth: `calc(100% - ${s.sm})`, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis' },
  focusedOption: { backgroundColor: surface.rowActive },
  unavailable: { opacity: 0.64 },
  description: { flex: '1 1 0', maxWidth: '45%', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', color: text.fgMuted, fontSize: t.denseSize },
  commandDescription: { maxWidth: 'none' },
  queue: { display: 'flex', flexDirection: 'column', gap: s.sm },
  queueRow: { display: 'flex', alignItems: 'center', gap: s.sm, backgroundColor: surface.codeBg, borderRadius: r.sm, padding: s.sm },
  queueText: { flexGrow: 1, fontSize: t.metaSize, minWidth: 0, overflowWrap: 'anywhere' },
  adaptiveRoot: { padding: s.md, gap: s.md, ':focus-within': { outline: `${g.focusRing} solid ${accent.primary}`, outlineOffset: g.focusOffset } },
  pill: { borderRadius: r.full, flexDirection: 'row', alignItems: 'center' },
  slab: { borderRadius: r.slab, flexDirection: 'column' },
  focusSlab: { borderRadius: r.full, flexDirection: 'row', alignItems: 'center', ':focus-within': { borderRadius: r.slab, flexDirection: 'column', alignItems: 'stretch' } },
  growingEditor: { minHeight: g.controlLg, minWidth: 0, fieldSizing: 'content', flex: '1 1 auto' },
  field: { flex: '1 1 auto', minWidth: 0, width: '100%' },
  fieldRow: { flex: '1 1 0', minWidth: 0, width: 'auto' },
  adaptiveFooter: { flexWrap: 'nowrap', flexShrink: 0, gap: s.xs, minWidth: 0, width: 'max-content', maxWidth: '100%', alignSelf: 'flex-end' },
  intrinsicProbe: { position: 'fixed', top: 0, left: 0, visibility: 'hidden', pointerEvents: 'none', width: 'max-content', maxWidth: 'none', whiteSpace: 'pre', overflow: 'hidden' },
  targets: { display: 'flex', flexWrap: 'nowrap', alignItems: 'center', gap: s.xs, minWidth: 0, flex: '0 1 auto' },
  target: { display: 'inline-flex', alignItems: 'center', gap: s.xs, flexShrink: 1, maxWidth: g.crumbMax, minWidth: g.controlMd, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  targetLabel: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  targetCompact: { maxWidth: '72px', paddingInline: s.xs, gap: s.xs2 },
  targetIcon: { width: g.controlMd, minWidth: g.controlMd, maxWidth: g.controlMd, paddingInline: s.xs, justifyContent: 'center' },
  readoutCompact: { maxWidth: '72px', fontSize: t.denseSize },
  readout: { maxWidth: g.crumbMax, minWidth: 0, flexShrink: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: t.metaSize, color: text.fg, ':focus-visible': { outline: `${g.focusRing} solid ${accent.primary}`, outlineOffset: g.focusOffset } },
  iconCompact: { paddingInline: s.xs },
  targetPopup: { maxWidth: 'min(400px,90vw)', maxHeight: g.commandMaxHeight, overflowY: 'auto', backgroundColor: surface.raised, color: text.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, padding: s.xs, fontFamily: t.fontSans, fontSize: t.metaSize },
  targetTooltip: { width: 'max-content', maxWidth: `min(${g.tooltipMax}, calc(100vw - ${s.lg}))`, boxSizing: 'border-box', padding: s.sm, borderRadius: r.sm, backgroundColor: surface.raised, color: text.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, overflowWrap: 'anywhere' },
  targetOption: { minHeight: g.controlLg, display: 'flex', alignItems: 'center', paddingInline: s.md, borderRadius: r.sm, cursor: 'pointer', outlineStyle: 'none', ':focus': { backgroundColor: surface.rowActive } },
})

const emptyHistory: readonly string[] = []
const emptyCandidates: readonly MentionToken[] = []
/** Each suggestion group previews its top matches; the popup budget narrows the count. */
const groupPreviewLimit = 4

/** Requires the same assistant-ui thread/runtime provider as the transcript. */
export function EmbraceComposer(props: EmbraceComposerProps) {
  const runtimeText = useAuiState(state => state.composer.text)
  const root = React.useRef<HTMLDivElement>(null)
  const [wrapped, setWrapped] = React.useState(false)
  const [cramped, setCramped] = React.useState(false)
  const pillLayout = props.variant !== 'C2'
  const [footerDensity, setFooterDensity] = React.useState(0)
  const density = Math.max(pillLayout ? 1 : 0, footerDensity)
  const compact = density > 0
  const iconOnly = density === 2
  const densityMinimums = React.useRef<number[] | null>(null)
  const densityContext = React.useMemo(() => ({ compact, iconOnly }), [compact, iconOnly])
  React.useLayoutEffect(() => {
    const footer = root.current?.querySelector<HTMLElement>('[data-testid="composer-footer"]')
    const surface = footer?.closest('form')
    if (!footer || !surface) return
    const minimums = densityMinimums.current ?? (densityMinimums.current = [])
    const measure = () => {
      const css = getComputedStyle(surface)
      const inner = surface.clientWidth - parseFloat(css.paddingLeft) - parseFloat(css.paddingRight)
      let required = footer.scrollWidth
      // Labels spend the remaining width, but keep three ems before choosing an
      // icon menu. Cache each density's minimum so resizing can restore labels
      // without toggling repeatedly between dense and spacious footers.
      if (!iconOnly) for (const label of footer.querySelectorAll<HTMLElement>('[data-composer-picker-label], [data-composer-readout]')) {
        required += Math.min(label.scrollWidth, 3 * parseFloat(getComputedStyle(label).fontSize)) - label.getBoundingClientRect().width
      }
      minimums[density] = required
      let next = density
      for (let candidate = pillLayout ? 1 : 0; candidate < density; candidate++) {
        if (minimums[candidate] !== undefined && inner >= minimums[candidate]! + 1) { next = candidate; break }
      }
      if (required > inner + 1) next = Math.min(2, density + 1)
      setFooterDensity(next)
    }
    const observer = new ResizeObserver(measure)
    observer.observe(surface)
    observer.observe(footer)
    document.fonts.addEventListener('loadingdone', measure)
    measure()
    return () => {
      observer.disconnect()
      document.fonts.removeEventListener('loadingdone', measure)
    }
  }, [density, iconOnly, pillLayout, props.toolbar, props.actions])
  React.useLayoutEffect(() => {
    const field = root.current?.querySelector<HTMLElement>('textarea, [role="textbox"]')
    const footer = root.current?.querySelector<HTMLElement>('[data-testid="composer-footer"]')
    const surface = field?.closest('form')
    if (!field || !footer || !surface || !pillLayout) return
    const probe = document.createElement('div')
    probe.className = stylex.props(styles.intrinsicProbe).className ?? ''
    probe.setAttribute('aria-hidden', 'true')
    const content = document.createElement('span')
    content.className = field.className
    probe.appendChild(content)
    root.current?.appendChild(probe)
    const copyDraft = () => {
      content.replaceChildren()
      if (field instanceof HTMLTextAreaElement) content.textContent = field.value
      else for (const child of field.childNodes) content.appendChild(child.cloneNode(true))
      for (const element of content.querySelectorAll('[id]')) element.removeAttribute('id')
    }
    // Both modes compare intrinsic draft width to the same compact footer width.
    // The footer never stretches to the slab width, so labels/fonts can be
    // observed directly without switching modes just to measure a hidden pill.
    const measure = () => {
      const css = getComputedStyle(surface)
      const inner = surface.clientWidth - parseFloat(css.paddingLeft) - parseFloat(css.paddingRight)
      const available = inner - footer.scrollWidth - (parseFloat(css.gap) || 0)
      setWrapped(content.getBoundingClientRect().width > available)
      setCramped(available < 0.65 * inner)
    }
    copyDraft()
    const observer = new ResizeObserver(measure)
    observer.observe(surface)
    observer.observe(footer)
    const mutations = new MutationObserver(() => { copyDraft(); measure() })
    mutations.observe(field, { childList: true, subtree: true, characterData: true })
    document.fonts.addEventListener('loadingdone', measure)
    measure()
    return () => {
      observer.disconnect()
      mutations.disconnect()
      document.fonts.removeEventListener('loadingdone', measure)
      probe.remove()
    }
  }, [pillLayout, runtimeText, props.tokenDraft])
  const stacked = props.variant === 'C2' || wrapped || cramped || runtimeText.includes('\n')
  const composerProps = {
    ...props,
    style: [styles.adaptiveRoot, stacked ? styles.slab : props.variant === 'C3' ? styles.focusSlab : styles.pill, props.style],
    inputStyle: [styles.input, styles.growingEditor, props.inputStyle],
    fieldStyle: [stacked ? styles.field : styles.fieldRow, props.fieldStyle],
    footerStyle: [styles.adaptiveFooter, props.footerStyle],
    submitIcon: compact ? <Icon name="send" /> : props.submitIcon,
  }
  return <CompactComposerContext.Provider value={densityContext}><div ref={root} data-testid="kit-composer" {...stylex.props(props.readingColumn === true && readingColumnStyles.column)}>
    {(props.plainText ?? props.variant === 'C1') || props.input !== undefined ? <PlainComposer {...composerProps} /> : <TokenComposer {...composerProps} />}
  </div></CompactComposerContext.Provider>
}

const CompactComposerContext = React.createContext({ compact: false, iconOnly: false })
export interface ComposerRecipient { readonly ref: string; readonly label: string; readonly model?: string }
export function EmbraceComposerToolbar({ target, recipients, models, onTargetChange, selectRecipient = false, selectModel = false, effort }: {
  readonly target: ComposerRecipient
  readonly recipients: readonly ComposerRecipient[]
  readonly models: readonly string[]
  readonly onTargetChange: (target: ComposerRecipient) => void
  readonly selectRecipient?: boolean
  readonly selectModel?: boolean
  readonly effort: Omit<React.ComponentProps<typeof EffortPicker>, 'compact'>
}) {
  const { compact, iconOnly } = React.useContext(CompactComposerContext)
  const running = useAuiState(state => state.thread.isRunning)
  const readoutName = target.model === undefined ? target.label : `${target.label} · ${target.model}`
  return <>
    <div {...stylex.props(styles.targets)}>
      {selectRecipient ? <MenuTrigger>
        <TooltipTrigger delay={150} closeDelay={0}>
          <Button aria-label={`Select recipient: ${target.label}`} {...stylex.props(styles.button, styles.target, compact && styles.targetCompact, iconOnly && styles.targetIcon)}>
            <span data-composer-picker-label={!iconOnly ? true : undefined} {...stylex.props(styles.targetLabel)}>{iconOnly ? <Icon name="message" /> : target.label}</span>
            {!compact && <Icon name="chevron-down" size={12} />}
          </Button>
          <Tooltip {...stylex.props(styles.targetTooltip)}>{target.label}</Tooltip>
        </TooltipTrigger>
        <Popover {...stylex.props(styles.targetPopup)}><Menu aria-label="Recipient" onAction={key => { const choice = recipients.find(choice => choice.ref === key); if (choice !== undefined) onTargetChange(choice) }}>{recipients.map(choice => <MenuItem key={choice.ref} id={choice.ref} textValue={choice.label} {...stylex.props(styles.targetOption)}>{choice.label}</MenuItem>)}</Menu></Popover>
      </MenuTrigger> : <TooltipTrigger delay={150} closeDelay={0}>
        <Focusable><span tabIndex={0} role="img" aria-label={target.model === undefined ? 'Recipient' : 'Recipient and model'} data-composer-target-readout={readoutName} data-composer-readout={!iconOnly ? true : undefined} {...stylex.props(styles.readout, compact && styles.readoutCompact)}>{iconOnly ? <Icon name="message" /> : target.label}</span></Focusable>
        <Tooltip {...stylex.props(styles.targetTooltip)}>{readoutName}</Tooltip>
      </TooltipTrigger>}
      {selectModel && <MenuTrigger>
        <TooltipTrigger delay={150} closeDelay={0}>
          <Button aria-label={`Select model: ${target.model ?? 'Model'}`} {...stylex.props(styles.button, styles.target, compact && styles.targetCompact, iconOnly && styles.targetIcon)}>
            <span data-composer-picker-label={!iconOnly ? true : undefined} {...stylex.props(styles.targetLabel)}>{iconOnly ? <Icon name="gear" /> : target.model ?? 'Model'}</span>
            {!compact && <Icon name="chevron-down" size={12} />}
          </Button>
          <Tooltip {...stylex.props(styles.targetTooltip)}>{target.model ?? 'Model'}</Tooltip>
        </TooltipTrigger>
        <Popover {...stylex.props(styles.targetPopup)}><Menu aria-label="Model" onAction={key => onTargetChange({ ...target, model: String(key) })}>{models.map(model => <MenuItem key={model} id={model} textValue={model} {...stylex.props(styles.targetOption)}>{model}</MenuItem>)}</Menu></Popover>
      </MenuTrigger>}
      <EffortPicker {...effort} compact={compact} />
    </div>
    <ComposerPrimitive.AddAttachment asChild><Button aria-label="Attach image" {...stylex.props(styles.button, compact && styles.iconCompact)}><Icon name="attach" /></Button></ComposerPrimitive.AddAttachment>
    {running && <ComposerPrimitive.Cancel asChild><Button aria-label="Stop run" {...stylex.props(styles.button, compact && styles.iconCompact)}><Icon name="stop" /></Button></ComposerPrimitive.Cancel>}
  </>
}

function PlainComposer({ targetLabel, disabledReason, toolbar, style, onRequestSubmit, submitLabel, submitIcon, inputStyle, fieldStyle, footerStyle, input, actions }: EmbraceComposerProps) {
  const isDisabled = useAuiState((s) => s.thread.isDisabled)
  const readOnly = disabledReason !== undefined || isDisabled
  const helpId = React.useId()
  const guidanceId = React.useId()
  const canSend = useAuiState(s => s.composer.canSend)
  const runtimeText = useAuiState(s => s.composer.text)
  const composing = React.useRef(false)
  const { compact } = React.useContext(CompactComposerContext)
  return (
    <ComposerPrimitive.Root
      {...stylex.props(styles.root, style)}
      onSubmit={event => { if (readOnly || onRequestSubmit !== undefined) event.preventDefault(); if (!readOnly) onRequestSubmit?.(false) }}
      onCompositionStartCapture={() => { composing.current = true }}
      onCompositionEndCapture={() => { composing.current = false }}
      onKeyDownCapture={event => {
        if (event.key !== 'Enter') return
        const target = event.target
        if (!(target instanceof HTMLElement && (target.isContentEditable || target.tagName === 'TEXTAREA' || target.tagName === 'INPUT'))) return
        if (event.nativeEvent.isComposing || composing.current || event.nativeEvent.keyCode === 229) { event.stopPropagation(); return }
        if (event.repeat) { event.preventDefault(); event.stopPropagation(); return }
        if (onRequestSubmit !== undefined && !event.shiftKey && !(window.matchMedia('(pointer: coarse) and (not (any-pointer: fine))').matches && !event.metaKey && !event.ctrlKey)) {
          event.preventDefault(); event.stopPropagation(); if (!readOnly) onRequestSubmit(event.metaKey || event.ctrlKey)
        }
      }}
    >
      <div {...stylex.props(styles.fieldWrap, fieldStyle)}>
      {runtimeText === '' ? <span id={guidanceId} data-testid="composer-placeholder" {...stylex.props(styles.placeholder)}>Message, @ mentions and / commands as text</span> : null}
      {input !== undefined ? input(inputStyle ?? styles.input, [helpId, runtimeText === '' ? guidanceId : undefined].filter(Boolean).join(' ')) : <ComposerPrimitive.Input
        aria-label={targetLabel === undefined ? 'Message' : `Message to ${targetLabel}`}
        aria-describedby={[helpId, runtimeText === '' ? guidanceId : undefined].filter(Boolean).join(' ')}
        placeholder=""
        disabled={readOnly}
        {...stylex.props(styles.input, inputStyle)}
      />}
      </div>
      <div data-testid="composer-footer" {...stylex.props(styles.footer, footerStyle)}>
        <div id={helpId} {...stylex.props(styles.help, styles.helpVisibility, (disabledReason !== undefined || isDisabled) && styles.availableReason)}>
          {disabledReason ?? (isDisabled ? 'This conversation is read-only.' : targetLabel === undefined ? undefined : `To ${targetLabel}`)}
        </div>
        {toolbar}
        <div {...stylex.props(styles.grow)} />
        {actions !== undefined ? actions(compact) : onRequestSubmit === undefined ? <ComposerPrimitive.Send disabled={readOnly} aria-label={submitIcon === undefined ? undefined : 'Send'} {...stylex.props(styles.button, styles.primary, submitIcon !== undefined && styles.submitIcon)}>{submitIcon ?? 'Send'}</ComposerPrimitive.Send> : <Button isDisabled={readOnly || !canSend} onPress={() => onRequestSubmit(false)} aria-label={submitIcon === undefined ? undefined : submitLabel ?? 'Send'} {...stylex.props(styles.button, styles.primary, submitIcon !== undefined && styles.submitIcon)}>{submitIcon ?? submitLabel ?? 'Send'}</Button>}
      </div>
    </ComposerPrimitive.Root>
  )
}

interface Option {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly hint?: string
  readonly token: DraftToken
  readonly unavailable?: string
}

function TokenComposer({
  variant, history = emptyHistory, tokenHistory, targetLabel, disabledReason, cancelUnavailableReason, maxContentBytes, toolbar,
  mentionCandidates = emptyCandidates, mentionHints, commands = defaultCommands,
  tokenDraft, onTokenDraftChange, onSendDraft, renderToken, style, onRequestSubmit, submitLabel, submitIcon, inputStyle, fieldStyle, footerStyle, showQueue = true, sendDisabled = false,
}: EmbraceComposerProps) {
  const aui = useAui()
  const composerStateNow = () => aui.composer.__internal_getRuntime?.().getState() ?? aui.composer.getState()
  const runtimeText = useAuiState((s) => s.composer.text)
  const isDisabled = useAuiState((s) => s.thread.isDisabled)
  const isEditing = useAuiState((s) => s.composer.isEditing)
  const isRunning = useAuiState((s) => s.thread.isRunning)
  const canQueue = useAuiState((s) => s.thread.capabilities.queue)
  const canCancel = useAuiState((s) => s.composer.canCancel)
  const hasAttachments = useAuiState(s => s.composer.attachments.length > 0)
  const readOnly = disabledReason !== undefined || isDisabled || !isEditing
  const [local, setLocal] = React.useState(() => ({ draft: draftFromText(runtimeText), runtimeText }))
  // Runtime resets, thread switches, and native suggestion/action writes invalidate the local
  // token snapshot. A controlled app draft remains app-owned; no effect synchronizes two stores.
  const draft = tokenDraft ?? (local.runtimeText === runtimeText ? local.draft : draftFromText(runtimeText))
  const serialized = serializeDraft(draft)
  const tooLarge = maxContentBytes !== undefined && new TextEncoder().encode(serialized.content).length > maxContentBytes
  const [dismissed, setDismissed] = React.useState<string | undefined>()
  const inputRef = React.useRef<HTMLDivElement>(null)
  const formRef = React.useRef<HTMLFormElement>(null)
  const composing = React.useRef(false)
  const recall = React.useRef<{ index: number; original: Draft } | undefined>(undefined)
  const helpId = React.useId()
  const guidanceId = React.useId()
  const trigger = activeTrigger(draft)
  const triggerKey = trigger === undefined ? undefined : `${trigger.index}:${trigger.start}:${trigger.query}`
  const open = !readOnly && trigger !== undefined && triggerKey !== dismissed
  const query = trigger?.query.toLowerCase() ?? ''
  const options: readonly Option[] = !open ? [] : (trigger.kind === '@'
    ? mentionCandidates.map(token => {
        const label = `@${token.label}`
        const path = token.family === 'file' ? token.ref.replace(/^file\//, '') : undefined
        const hint = mentionHints?.get(token.ref) ?? (path !== undefined && path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : undefined)
        return { id: token.ref, label, description: token.ref, hint: hint !== undefined && !label.toLowerCase().includes(hint.replace(/^@/, '').toLowerCase()) ? hint : undefined, token }
      })
    : commands.map((command) => ({
        id: command.id, label: command.label, description: command.description,
        token: { _tag: 'Command' as const, command: command.id }, unavailable: command.unavailable,
      })))
    .filter((option) => `${option.label} ${option.description}`.toLowerCase().includes(query))
  const groups = [
    { id: 'agent', label: 'Agents' }, { id: 'thread', label: 'Threads' },
    { id: 'file', label: 'Files' }, { id: 'terminal', label: 'Terminals' },
    { id: 'resource', label: 'Resources' }, { id: 'command', label: 'Commands' },
  ].map(group => ({ ...group, options: options.filter(option => option.token._tag === 'Command'
    ? group.id === 'command' : option.token.family === group.id || (group.id === 'resource' && !['agent', 'thread', 'file', 'terminal'].includes(option.token.family))) }))
    .filter(group => group.options.length > 0)
  // Scroll-list budget in px: g.tooltipMax (320) minus menu padding; header g.controlSm, row g.controlMd.
  const previewLimit = Math.max(1, Math.min(groupPreviewLimit, Math.floor((308 - 24 * groups.length) / (28 * groups.length))))
  const previewedGroups = groups.map(group => ({ ...group, hidden: Math.max(0, group.options.length - previewLimit), options: group.options.slice(0, previewLimit) }))

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
  const submit = (modified = false) => {
    if (readOnly || sendDisabled || tooLarge || composing.current || (serialized.content === '' && !hasAttachments) || (isRunning && !canQueue)) return
    bridge(draft)
    // The store client snapshot updates on React commit; the native runtime reflects this bridge immediately.
    if (!composerStateNow().canSend) return
    if (onRequestSubmit === undefined) {
      onSendDraft?.(draft)
      // Explicit false matters: native send defaults to steering during an active queued run.
      aui.composer.send({ steer: false })
    } else onRequestSubmit(modified)
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
  const canSubmit = !readOnly && !sendDisabled && !tooLarge && (serialized.content !== '' || hasAttachments) && (!isRunning || canQueue)

  return (
    <ComposerPrimitive.Root
      ref={formRef}
      {...stylex.props(styles.root, style)}
      onSubmit={(event) => { event.preventDefault(); if (!open) submit() }}
      onCompositionStartCapture={() => { composing.current = true }}
      onCompositionEndCapture={() => { composing.current = false }}
      onKeyDownCapture={(event) => {
        // Only the composer's own text field owns Enter-to-send; buttons inside the
        // footer (menus, attach, send) keep their native key handling.
        const target = event.target
        const inField = target instanceof HTMLElement && (target.isContentEditable || target.tagName === 'TEXTAREA' || target.tagName === 'INPUT')
        if (!inField) return
        if (event.key === 'Enter' && event.repeat) {
          event.preventDefault()
          event.stopPropagation()
          return
        }
        if (event.nativeEvent.isComposing || composing.current || event.nativeEvent.keyCode === 229) {
          if (event.key === 'Enter') event.stopPropagation()
          return
        }
        if (onRequestSubmit !== undefined && event.key === 'Enter' && !event.shiftKey && !open && !(window.matchMedia('(pointer: coarse) and (not (any-pointer: fine))').matches && !event.metaKey && !event.ctrlKey)) {
          event.preventDefault(); event.stopPropagation(); submit(event.metaKey || event.ctrlKey); return
        }
        if (variant === 'C3' && event.key === 'Escape' && !open && canCancel && !readOnly) {
          event.preventDefault()
          aui.composer.cancel()
        }
      }}
    >
      <Autocomplete>
        <div {...stylex.props(styles.fieldWrap, fieldStyle)}>
          {/* Input asChild/render still imposes a string value, target.value/selectionStart,
              HTMLTextAreaElement ref and setSelectionRange. RAC TokenInput is contenteditable
              with TokenFieldValue segment/caret state. It cannot implement that native contract.
              This is the real app TokenField inside Root, bridged explicitly to composer text. */}
          <TokenField<Draft>
            aria-label={targetLabel === undefined ? 'Message with mentions and commands' : `Message to ${targetLabel}`}
            aria-describedby={[helpId, isEmpty ? guidanceId : undefined].filter(Boolean).join(' ')}
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
            {isEmpty ? <span id={guidanceId} data-testid="composer-placeholder" {...stylex.props(styles.placeholder)}>Message, @ to mention, / for commands</span> : null}
            <TokenInput<Draft>
              ref={inputRef}
              className={() => stylex.props(styles.input, inputStyle).className ?? ''}
            >
              {(segment) => <Token className={({ isSelected }) => stylex.props(styles.token, isSelected && styles.selectedToken).className ?? ''}>
                {segment.value === undefined ? segment.text : renderToken?.(segment.value) ??
                  (segment.value._tag === 'Mention' ? `@${segment.value.label}` : segment.text)}
              </Token>}
            </TokenInput>
          </TokenField>
          {open ? <Popover triggerRef={formRef} offset={4} isOpen isNonModal placement="top start" shouldFlip={false}
            onOpenChange={(next) => { if (!next) setDismissed(triggerKey) }} {...stylex.props(styles.menu)}>
            <ListBox autoFocus="first" aria-label={trigger.kind === '@' ? 'Mention a subject' : 'Commands'} items={previewedGroups} {...stylex.props(styles.menuList)}
              onAction={pick} disabledKeys={options.filter((option) => option.unavailable !== undefined).map((option) => option.id)}
              renderEmptyState={() => <div {...stylex.props(styles.option)}>No matches</div>}>
              {(group) => <ListBoxSection id={group.id}>
                <Header {...stylex.props(styles.menuHeader)}>{group.label}{group.hidden > 0 ? ` · ${group.options.length + group.hidden}` : ''}</Header>
                {group.options.map(option => <ListBoxItem key={option.id} id={option.id} textValue={option.label}
                  className={({ isFocused, isDisabled: optionDisabled }) => stylex.props(styles.option, isFocused && styles.focusedOption, optionDisabled && styles.unavailable).className ?? ''}>
                  <Text slot="label" {...stylex.props(styles.optionLabel)}>{option.label}</Text>
                  <Text slot="description" {...stylex.props(styles.helpVisibility)}>{option.unavailable ?? option.description}</Text>
                  {option.token._tag === 'Mention' ? option.hint !== undefined && <span aria-hidden="true" data-mention-hint {...stylex.props(styles.description)}>{option.hint}</span> : <span aria-hidden="true" {...stylex.props(styles.description, styles.commandDescription)}>{option.unavailable ?? option.description}</span>}
                </ListBoxItem>)}
              </ListBoxSection>}
            </ListBox>
          </Popover> : null}
        </div>
      </Autocomplete>
      <div id={helpId} {...stylex.props(styles.help, styles.helpVisibility, (disabledReason !== undefined || isDisabled) && styles.availableReason)}>
        {disabledReason ?? (isDisabled ? 'This conversation is read-only.' :
          `${targetLabel === undefined ? '' : `To ${targetLabel} · `}Enter to ${variant === 'C3' && isRunning && canQueue ? 'queue' : 'send'} · Shift+Enter for a new line${variant === 'C3' ? ` · ↑ on empty recalls history${canCancel ? ' · Esc cancels' : ''}` : ''}`)}
      </div>
      {tooLarge ? <div role="status" {...stylex.props(styles.warning)}>Message exceeds the {maxContentBytes} byte inline limit. Your draft is kept; shorten it to send.</div> : null}
      {variant === 'C3' && isRunning && !canQueue ? <div {...stylex.props(styles.warning)}>This runtime does not support queuing. Cancel the active run before sending.</div> : null}
      <div data-testid="composer-footer" {...stylex.props(styles.footer, footerStyle)}>
        {toolbar}
        <div {...stylex.props(styles.grow)} />
        {variant === 'C3' && toolbar === undefined ? <>
          <Button isDisabled={readOnly || history.length === 0} onPress={() => recallHistory('previous')}
            aria-label="Recall previous message" aria-description="ArrowUp on an empty field" {...stylex.props(styles.button)}><Icon name="clock" /></Button>
          <ComposerPrimitive.Cancel disabled={readOnly || !canCancel} title={canCancel ? undefined : cancelUnavailableReason} {...stylex.props(styles.button, styles.cancelVisibility)}>Cancel run</ComposerPrimitive.Cancel>
        </> : null}
        {/* A controlled TokenField can contain a draft before runtime text is bridged.
            The explicit adapter submits after bridging instead of inheriting Input's canSend gate. */}
        <Button isDisabled={!canSubmit} onPress={() => submit()}
          aria-label={submitIcon === undefined ? undefined : submitLabel ?? 'Send'}
          {...stylex.props(styles.button, styles.primary, submitIcon !== undefined && styles.submitIcon)}>
          {submitIcon ?? submitLabel ?? (variant === 'C3' && isRunning && canQueue ? 'Queue message' : 'Send')}
        </Button>
      </div>
      {variant === 'C3' && showQueue ? <ComposerQueueStrip disabled={readOnly} /> : null}
    </ComposerPrimitive.Root>
  )
}

/** Both plain and structured hosts render the same native queue and actions. */
export function ComposerQueueStrip({ disabled = false, effortById }: { readonly disabled?: boolean; readonly effortById?: ReadonlyMap<string, string> }) {
  const queuedCount = useAuiState(s => s.composer.queue.length)
  if (queuedCount === 0) return null
  return <div aria-label="Queued messages" {...stylex.props(styles.queue)}>
    <div role="status" {...stylex.props(styles.help)}>{queuedCount} queued</div>
    <ComposerPrimitive.Queue>{({ queueItem }) => <div {...stylex.props(styles.queueRow)}>
      <QueueItemPrimitive.Text {...stylex.props(styles.queueText)} />
      {effortById?.has(queueItem.id) && <span aria-label="Queued message effort" {...stylex.props(styles.help)}>Effort: {effortById.get(queueItem.id)}</span>}
      <QueueItemPrimitive.Steer disabled={disabled} {...stylex.props(styles.button)}>Run next</QueueItemPrimitive.Steer>
      <QueueItemPrimitive.Remove disabled={disabled} aria-label={`Remove queued message ${queueItem.id}`} {...stylex.props(styles.button)}>Remove</QueueItemPrimitive.Remove>
    </div>}</ComposerPrimitive.Queue>
  </div>
}
