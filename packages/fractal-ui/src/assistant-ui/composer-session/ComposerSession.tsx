import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Schema } from 'effect'
import { AttachmentPrimitive, ComposerPrimitive, useAui, useAuiState } from '@assistant-ui/react'
import { Button, Dialog, Modal, ModalOverlay, TokenFieldValue } from 'react-aria-components'
import { EmbraceComposer, EmbraceComposerToolbar, ComposerQueueStrip } from '../EmbraceComposer'
import { EmbraceRuntimeProvider, type ConversationRuntimeOptions } from '../EmbraceRuntime'
import { defaultCommands, DraftToken, type Draft, type MentionToken, type SerializedDraft } from '../embrace-composer/draft'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { Icon } from '../composition/Icons'
import type { ComposerTarget } from './composer-fixtures'
import { unknownEffort, type EffortControl } from '../embrace-composer/EffortPicker'

export type ComposerLayout = 'C1' | 'C2' | 'C3'
export type RunningBehavior = 'R1' | 'R2' | 'R3'
export type MentionMode = 'M1' | 'M2' | 'M3'
export type TargetMode = 'K1' | 'K2' | 'K3'
/** Host-backed composer session: durable local draft, C/R/M/K policy and explicit send outcomes. */
export interface ComposerSessionProps {
  readonly options: ConversationRuntimeOptions
  readonly layout: ComposerLayout
  readonly runningBehavior: RunningBehavior
  readonly mentions: MentionMode
  readonly targeting: TargetMode
  readonly initialDraft: string
  readonly initialTokenDraft?: Draft
  readonly initialImage?: File
  readonly recipients: readonly [ComposerTarget, ...ComposerTarget[]]
  readonly models: readonly string[]
  readonly limitedMentions: readonly MentionToken[]
  readonly allMentions: readonly MentionToken[]
  readonly mentionHints?: ReadonlyMap<string, string>
  readonly failure?: string
  readonly offline: boolean
  readonly effortControl?: EffortControl
  readonly queuedEffort?: ReadonlyMap<string, string>
}
const SavedDraft = Schema.Struct({
  text: Schema.String,
  segments: Schema.optional(Schema.Array(Schema.Union([
    Schema.Struct({ type: Schema.Literal('text'), text: Schema.String }),
    Schema.Struct({ type: Schema.Literal('token'), text: Schema.String, value: Schema.optional(DraftToken) }),
  ]))),
})
type SavedDraft = typeof SavedDraft.Type
const savedDraftJson = Schema.fromJsonString(SavedDraft)
const readDraft = (key: string, initialDraft: string): SavedDraft => {
  try { const saved = localStorage.getItem(key); if (saved !== null) return Schema.decodeUnknownSync(savedDraftJson)(saved) } catch { /* The native live draft remains available if browser storage is disabled or the saved value is malformed. */ }
  return { text: initialDraft }
}
export function ComposerSession({ draftKey, ...props }: ComposerSessionProps & { readonly draftKey: string }) {
  return <EmbraceRuntimeProvider options={props.options}><ComposerSessionBody key={draftKey} {...props} draftKey={draftKey} /></EmbraceRuntimeProvider>
}
function ComposerSessionBody({ draftKey, layout, runningBehavior, mentions, targeting, initialDraft, initialTokenDraft, initialImage, recipients, models, limitedMentions, allMentions, mentionHints, failure, offline, effortControl = unknownEffort, queuedEffort }: ComposerSessionProps & { readonly draftKey: string }) {
  const aui = useAui()
  const running = useAuiState(state => state.thread.isRunning)
  const runtimeText = useAuiState(state => state.composer.text)
  const [saved] = React.useState(() => {
    const draft = readDraft(draftKey, initialDraft)
    return draft.segments !== undefined || draft.text !== initialDraft || initialTokenDraft === undefined
      ? draft : { ...draft, segments: initialTokenDraft.segments }
  })
  const [tokenSnapshot, setTokenSnapshot] = React.useState(() => saved.segments === undefined ? undefined : { draft: new TokenFieldValue<DraftToken>(saved.segments), runtimeText: saved.text })
  const tokens = React.useRef<SavedDraft>(saved)
  const [target, setTarget] = React.useState<ComposerTarget>(() => {
    const first = recipients[0]
    return { ref: first.ref, label: first.label, model: first.model }
  })
  const [ask, setAsk] = React.useState(false)
  const [effort, setEffort] = React.useState<string>()
  const [effortPinned, setEffortPinned] = React.useState(false)
  const initialTarget = React.useRef(target)
  React.useLayoutEffect(() => {
    const composer = aui.composer()
    composer.setText(saved.text)
    const runConfig = composer.getState().runConfig
    composer.setRunConfig({ ...runConfig, custom: { ...runConfig.custom, explorerTarget: initialTarget.current } })
    if (initialImage !== undefined) void composer.addAttachment(initialImage)
    let timer: number | undefined
    let lastPersisted: string | null = null
    const persist = () => {
      const text = composer.getState().text
      const next = tokens.current.text === text ? tokens.current : { text }
      const encoded = Schema.encodeSync(savedDraftJson)(next)
      if (encoded === lastPersisted) return
      lastPersisted = encoded
      try { localStorage.setItem(draftKey, encoded) } catch { /* The native draft stays editable without persistence. */ }
    }
    const unsubscribe = aui.subscribe(() => { clearTimeout(timer); timer = window.setTimeout(persist, 300) })
    return () => { unsubscribe(); clearTimeout(timer); persist() }
  }, [aui, draftKey, saved, initialImage])
  const onTokenDraftChange = React.useCallback((draft: Draft, serialized: SerializedDraft) => {
    tokens.current = { text: serialized.content, segments: draft.segments }
    setTokenSnapshot({ draft, runtimeText: serialized.content })
  }, [])
  const writeTarget = (next: ComposerTarget) => {
    const runConfig = aui.composer().getState().runConfig
    aui.composer().setRunConfig({ ...runConfig, custom: { ...runConfig.custom, explorerTarget: next } })
    setTarget(next)
  }
  const send = (steer: boolean) => {
    setAsk(false)
    const composer = aui.composer()
    if (!composer.getState().canSend || offline) return
    const runConfig = composer.getState().runConfig
    const selected = effortControl.state === 'supported'
      ? effort !== undefined && effortControl.values.includes(effort) ? effort : effortControl.default
      : undefined
    const { effort: previousEffort, ...custom } = runConfig.custom ?? {}
    composer.setRunConfig({ ...runConfig, custom: selected === undefined ? custom : { ...custom, effort: selected } })
    composer.send({ steer })
    if (!effortPinned) setEffort(undefined)
  }
  const requestSubmit = (modified: boolean) => {
    if (!aui.composer().getState().canSend) return
    if (mentions === 'M1') {
      const runConfig = aui.composer().getState().runConfig
      aui.composer().setRunConfig({ ...runConfig, custom: { ...runConfig.custom, embraceDraft: { content: aui.composer().getState().text, mentions: [], commands: [] } } })
    }
    if (running && runningBehavior === 'R3') { setAsk(true); return }
    send(running && (runningBehavior === 'R1' ? modified : !modified))
  }
  const toolbar = <EmbraceComposerToolbar target={target} recipients={recipients} models={models} onTargetChange={writeTarget} selectRecipient={targeting !== 'K1'} selectModel={targeting === 'K3'} effort={{ control: effortControl, value: effort, pinned: effortPinned, onChange: setEffort, onPinnedChange: setEffortPinned }} />
  const submitLabel = running ? runningBehavior === 'R1' ? 'Queue' : runningBehavior === 'R2' ? 'Steer' : 'Choose action' : 'Send'
  return <div data-testid="composer-session" data-layout={layout} data-running-behavior={runningBehavior} data-mentions={mentions} data-targeting={targeting} {...stylex.props(styles.stack)}>
    {failure !== undefined && <div role="alert" {...stylex.props(styles.notice)}>{failure}</div>}
    {offline && <div role="status" {...stylex.props(styles.notice)}>Offline: the draft remains editable and saved locally. Reconnect before sending.</div>}
    <div {...stylex.props(styles.attachments)}><ComposerPrimitive.Attachments>{() => <AttachmentPreview />}</ComposerPrimitive.Attachments></div>
    <EmbraceComposer variant={layout} plainText={mentions === 'M1'} toolbar={toolbar} mentionCandidates={mentions === 'M3' ? allMentions : limitedMentions} mentionHints={mentionHints} commands={defaultCommands} tokenDraft={tokenSnapshot?.runtimeText === runtimeText ? tokenSnapshot.draft : undefined} onTokenDraftChange={onTokenDraftChange} onRequestSubmit={requestSubmit} submitLabel={submitLabel} showQueue={false} sendDisabled={offline} />
    <div {...stylex.props(styles.help)}>{running ? runningBehavior === 'R1' ? 'Enter queues · Mod+Enter steers' : runningBehavior === 'R2' ? 'Enter steers · Mod+Enter queues' : 'Choose queue or steer for each message' : 'Enter sends'} · Shift+Enter adds a newline</div>
    <ComposerQueueStrip disabled={offline} effortById={queuedEffort} />
    <ModalOverlay isOpen={ask} isDismissable onOpenChange={setAsk} {...stylex.props(styles.overlay)}><Modal {...stylex.props(styles.modal)}><Dialog aria-label="Choose running message action" {...stylex.props(styles.dialog)}><h2 {...stylex.props(styles.dialogTitle)}>Send while running</h2><p>Queue after the current run, or steer the current run?</p><div {...stylex.props(styles.dialogActions)}><Button onPress={() => send(false)} {...stylex.props(styles.button)}>Queue</Button><Button onPress={() => send(true)} {...stylex.props(styles.button)}>Steer</Button><Button onPress={() => setAsk(false)} {...stylex.props(styles.button)}>Cancel</Button></div></Dialog></Modal></ModalOverlay>
  </div>
}
function AttachmentPreview() {
  const attachment = useAuiState(state => state.attachment)
  const file = 'file' in attachment ? attachment.file : undefined
  const [url, setUrl] = React.useState<string>()
  React.useEffect(() => {
    if (file === undefined) return
    const next = URL.createObjectURL(file)
    setUrl(next)
    return () => URL.revokeObjectURL(next)
  }, [file])
  return <AttachmentPrimitive.Root {...stylex.props(styles.attachment)}>{url !== undefined && <img src={url} alt={attachment.name} {...stylex.props(styles.image)} />}<span {...stylex.props(styles.name)}><AttachmentPrimitive.Name /></span><AttachmentPrimitive.Remove asChild><Button aria-label={`Remove ${attachment.name}`} {...stylex.props(styles.button)}><Icon name="x" /></Button></AttachmentPrimitive.Remove></AttachmentPrimitive.Root>
}
const styles = stylex.create({
  stack: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  button: { display: 'inline-flex', justifyContent: 'center', alignItems: 'center', gap: s.xs, flexShrink: 0, minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary }, ':disabled': { cursor: 'not-allowed', opacity: 0.64 } },
  help: { fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted }, notice: { padding: s.md, borderRadius: r.sm, backgroundColor: surface.rowActive, color: ink.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading },
  attachments: { display: 'flex', gap: s.md, flexWrap: 'wrap' }, attachment: { display: 'flex', alignItems: 'center', gap: s.md, padding: s.md, borderRadius: r.sm, backgroundColor: surface.rowActive, minWidth: 0 }, image: { width: g.band, height: g.band, objectFit: 'cover', borderRadius: r.sm }, name: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', fontSize: t.metaSize },
  overlay: { position: 'fixed', inset: 0, backgroundColor: surface.scrim, display: 'flex', justifyContent: 'center', alignItems: 'center', zIndex: 30, padding: s.xl }, modal: { backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.lg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, maxWidth: '90vw', outline: 'none' }, dialog: { padding: s.xl, borderRadius: r.lg }, dialogTitle: { margin: 0, fontSize: t.headingSize, lineHeight: t.headingLeading }, dialogActions: { display: 'flex', gap: s.md, flexWrap: 'wrap' },
})
