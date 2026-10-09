import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { ComposerPrimitive, useAui } from '@assistant-ui/react'
import { Button as AriaButton } from 'react-aria-components'
import { surfaceVars as surface, textVars as text, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { Icon } from './Icons'
import { Button } from './Controls'
import { EmbraceComposer } from '../EmbraceComposer'
import { CommandTooltip } from '../commands'

export interface ComposerProps {
  agent: string; running?: boolean; folder?: string; branch?: string
  onSend?: (text: string) => void | Promise<void>
  onStop?: () => void | Promise<void>
  onSteer?: (text: string) => void | Promise<void>
  onQueue?: (text: string) => void | Promise<void>
  history?: readonly string[]
  draftKey?: string
  /** Registry ids supply discovery; host performs send/stop against its composer state. */
  commandIds?: { readonly send?: string; readonly stop?: string }
  inputRef?: React.Ref<HTMLTextAreaElement>
}
const drafts = new Map<string, string>()
const queues = new Map<string, readonly string[]>()
const histories = new Map<string, readonly string[]>()
const readDraft = (key: string) => { try { return drafts.get(key) ?? sessionStorage.getItem(`composition.draft.${key}`) ?? '' } catch { return drafts.get(key) ?? '' } }

export const Composer = React.memo(function Composer(props: ComposerProps) {
  const key = props.draftKey ?? `${props.folder}/${props.agent}`
  return <ComposerEditor key={key} {...props} storageKey={key} />
})
function ComposerEditor({ agent, running, folder, branch, onSend, onStop, onSteer, onQueue, history, storageKey, commandIds, inputRef }: ComposerProps & { storageKey: string }) {
  const aui = useAui()
  const [value, setValue] = React.useState(() => readDraft(storageKey))
  const [queued, setQueued] = React.useState<readonly string[]>(() => queues.get(storageKey) ?? [])
  const [failed, setFailed] = React.useState<string>()
  const [dispatching, setDispatching] = React.useState(false)
  const input = React.useRef<HTMLTextAreaElement>(null)
  const current = React.useRef(value)
  const pending = React.useRef(false)
  const composing = React.useRef(false)
  const saveTimer = React.useRef<number | undefined>(undefined)
  const edit = (next: string, publish = true) => {
    current.current = next; drafts.set(storageKey, next); setValue(next)
    if (publish) aui.composer.setText(next)
    clearTimeout(saveTimer.current)
    saveTimer.current = window.setTimeout(() => { try { sessionStorage.setItem(`composition.draft.${storageKey}`, current.current) } catch {} }, 300)
  }
  const dispatch = async (message: string, action: (text: string) => void | Promise<void>) => {
    if (pending.current) return false
    pending.current = true; setDispatching(true); setFailed(undefined)
    try { await action(message); histories.set(storageKey, [...(histories.get(storageKey) ?? []), message]); return true }
    catch { if (current.current === '') edit(message); else setFailed(message); return false }
    finally { pending.current = false; setDispatching(false) }
  }
  const send = (steer = false) => {
    const message = current.current.trim()
    if (!message || pending.current) return
    if (running && !steer) {
      edit('')
      if (onQueue) void dispatch(message, onQueue)
      else { const next = [...queued, message]; queues.set(storageKey, next); setQueued(next) }
      return
    }
    const action = running ? onSteer : onSend
    if (!action) return
    edit(''); void dispatch(message, action)
  }
  React.useLayoutEffect(() => {
    if (running || queued.length === 0 || !onSend || pending.current) return
    const [message, ...rest] = queued
    if (message === undefined) return
    queues.set(storageKey, rest); setQueued(rest)
    void dispatch(message, onSend)
  }, [running, queued, onSend, storageKey, dispatching])
  React.useLayoutEffect(() => {
    return () => { clearTimeout(saveTimer.current); try { sessionStorage.setItem(`composition.draft.${storageKey}`, current.current) } catch {} }
  }, [storageKey])
  const stop = () => {
    if (queued.length > 0) { edit([current.current, ...queued].filter(Boolean).join('\n\n')); queues.delete(storageKey); setQueued([]) }
    void onStop?.()
  }
  return <div data-testid="composer">
    {queued.length > 0 ? <div role="status" {...stylex.props(styles.banner)}>{queued.length} {queued.length === 1 ? 'message' : 'messages'} will send after run<Button onPress={() => { edit([current.current, ...queued].filter(Boolean).join('\n\n')); queues.delete(storageKey); setQueued([]) }}>Edit queue</Button></div> : null}
    {failed ? <div role="alert" {...stylex.props(styles.banner)}>Message could not be sent.<Button onPress={() => { edit([current.current, failed].filter(Boolean).join('\n\n')); setFailed(undefined); input.current?.focus() }}>Restore message</Button></div> : null}
    <EmbraceComposer
      variant="C2"
      plainText
      showQueue={false}
      onRequestSubmit={modified => send(modified)}
      input={(inputStyle, descriptionId) => <ComposerPrimitive.Input asChild ref={node => { input.current = node; if (typeof inputRef === 'function') inputRef(node); else if (inputRef != null) inputRef.current = node }} data-testid="composer-input" aria-label="Message" aria-describedby={descriptionId} placeholder="" value={value} submitMode="none" cancelOnEscape={false} unstable_focusOnRunStart={false} unstable_focusOnScrollToBottom={false} onChange={event => edit(event.target.value, false)} onCompositionStart={() => { composing.current = true }} onCompositionEnd={() => { composing.current = false }} onKeyDown={event => {
        if (event.nativeEvent.isComposing || composing.current || event.keyCode === 229 || event.repeat) return
        if (event.key === 'ArrowUp' && current.current === '') { const previous = (history ?? histories.get(storageKey) ?? []).at(-1); if (previous) { event.preventDefault(); edit(previous) } }
      }} {...stylex.props(inputStyle)}><textarea rows={1} /></ComposerPrimitive.Input>}
      toolbar={<><Button size="md">{agent}<Icon name="chevron-down" /></Button><span {...stylex.props(styles.spacer)} /><ComposerPrimitive.AddAttachment asChild><AriaButton aria-label="Attach context" {...stylex.props(styles.attach)}><Icon name="attach" /></AriaButton></ComposerPrimitive.AddAttachment></>}
      actions={() => running ? <CommandTooltip commandId={commandIds?.stop} label="Stop"><ComposerPrimitive.Cancel asChild><AriaButton aria-label="Stop" onClick={event => { event.preventDefault(); stop() }} {...stylex.props(styles.primary, styles.stop)}><Icon name="stop" /></AriaButton></ComposerPrimitive.Cancel></CommandTooltip> : <CommandTooltip commandId={commandIds?.send} label="Send"><ComposerPrimitive.Send asChild><AriaButton aria-label="Send" isDisabled={value.trim() === '' || onSend === undefined} onClick={event => { event.preventDefault(); send() }} {...stylex.props(styles.primary)}><Icon name="send" /></AriaButton></ComposerPrimitive.Send></CommandTooltip>}
    />
    {folder !== undefined || branch !== undefined ? <div data-testid="composer-context" {...stylex.props(styles.context)}>{folder !== undefined && <span>{folder}</span>}{folder !== undefined && branch !== undefined && <span>·</span>}{branch !== undefined && <span>{branch}</span>}<Icon name="chevron-down" /></div> : null}
  </div>
}
const styles = stylex.create({
  spacer: { flexGrow: 1 },
  attach: { width: g.controlMd, height: g.controlMd, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', backgroundColor: 'transparent', borderWidth: 0, color: text.fgMuted, borderRadius: r.control, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  primary: { width: g.controlLg, height: g.controlLg, borderRadius: '50%', borderWidth: 0, backgroundColor: accent.primary, color: accent.onPrimary, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer', ':disabled': { opacity: 0.64 }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.hairline } }, stop: { backgroundColor: status.danger },
  context: { display: 'flex', alignItems: 'center', gap: s.sm, fontSize: t.metaSize, lineHeight: t.metaLeading, color: text.fgMuted, backgroundColor: surface.controlFill, borderBottomLeftRadius: r.md, borderBottomRightRadius: r.md, marginInline: s.xl, paddingInline: s.lg, paddingBlock: s.xs }, banner: { display: 'flex', alignItems: 'center', gap: s.sm, fontSize: t.metaSize, color: text.fgMuted, padding: s.md },
})
