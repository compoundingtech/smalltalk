import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Autocomplete, Button, Dialog, Heading, Input, Menu, MenuItem, MenuSection, Header, Modal, ModalOverlay, SearchField, Text, Tooltip, TooltipTrigger, type ButtonProps } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, elevationVars as elevation } from './composition-tokens.stylex'

export type CommandPlatform = 'mac' | 'other'
export interface CommandChord { readonly key: string; readonly modifiers?: readonly ('Mod' | 'Alt' | 'Shift')[] }
export type CommandShortcut = CommandChord | { readonly mac: CommandChord; readonly other: CommandChord }
export interface KitCommand {
  readonly id: string
  readonly label: string
  readonly group: string
  readonly shortcut?: CommandShortcut
  /** Return true when enabled, or the host's concrete disabled reason. Re-evaluated before perform. */
  readonly when?: () => true | string
  readonly disabledReason?: string
  /** Opt in only for editor actions such as send; ordinary shortcuts never consume typing. */
  readonly allowInEditable?: boolean
  readonly perform: () => void
}
export const commandPlatform = (): CommandPlatform => typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform) ? 'mac' : 'other'
const chordFor = (shortcut: CommandShortcut, platform: CommandPlatform): CommandChord => 'key' in shortcut ? shortcut : shortcut[platform]
export function formatCommandShortcut(shortcut: CommandShortcut | undefined, platform: CommandPlatform): string | undefined {
  if (shortcut === undefined) return undefined
  const chord = chordFor(shortcut, platform)
  return [...(chord.modifiers ?? []).map(modifier => modifier === 'Mod' ? platform === 'mac' ? 'Cmd' : 'Ctrl' : modifier === 'Alt' && platform === 'mac' ? 'Option' : modifier), chord.key.length === 1 ? chord.key.toUpperCase() : chord.key].join('+')
}
export function commandDisabledReason(command: KitCommand): string | undefined {
  if (command.disabledReason !== undefined) return command.disabledReason
  const state = command.when?.()
  return typeof state === 'string' ? state : undefined
}
/** Subsequence matching tolerates omitted letters without hiding unmatched groups. */
export function fuzzyCommandMatch(label: string, query: string): boolean {
  const text = label.toLocaleLowerCase()
  let from = 0
  for (const character of query.trim().toLocaleLowerCase()) {
    if (/\s/.test(character)) continue
    const at = text.indexOf(character, from)
    if (at < 0) return false
    from = at + 1
  }
  return true
}
interface CommandRegistry {
  readonly commands: readonly KitCommand[]
  readonly platform: CommandPlatform
  readonly perform: (id: string) => boolean
  readonly openPalette: () => void
  readonly openShortcuts: () => void
  readonly portalContainer: HTMLElement | null
}
const Registry = React.createContext<CommandRegistry | null>(null)
export const useCommands = () => React.useContext(Registry)
const editableTarget = (target: EventTarget | null): boolean => target instanceof Element && target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"]), [role="textbox"]') !== null
function matches(event: KeyboardEvent, chord: CommandChord, platform: CommandPlatform): boolean {
  const modifiers = chord.modifiers ?? []
  const mod = modifiers.includes('Mod')
  return event.key.toLowerCase() === chord.key.toLowerCase() && event.metaKey === (mod && platform === 'mac') && event.ctrlKey === (mod && platform === 'other') && event.altKey === modifiers.includes('Alt') && event.shiftKey === modifiers.includes('Shift')
}
/** Host registers its complete current command list. One document capture listener owns all shortcuts. */
export function CommandsProvider({ commands, platform = commandPlatform(), children }: { readonly commands: readonly KitCommand[]; readonly platform?: CommandPlatform; readonly children: React.ReactNode }) {
  const parent = useCommands()
  if (parent !== null) throw new Error('CommandsProvider must not be nested: use one host registry.')
  const [overlay, setOverlay] = React.useState<'palette' | 'shortcuts' | null>(null)
  const [portalContainer, setPortalContainer] = React.useState<HTMLDivElement | null>(null)
  const composing = React.useRef(false)
  const perform = (id: string) => {
    const command = commands.find(entry => entry.id === id)
    if (command === undefined || commandDisabledReason(command) !== undefined) return false
    setOverlay(null)
    command.perform()
    return true
  }
  const registry: CommandRegistry = { commands, platform, portalContainer, perform, openPalette: () => setOverlay('palette'), openShortcuts: () => setOverlay('shortcuts') }
  const keyDown = React.useEffectEvent((event: KeyboardEvent) => {
    if (event.defaultPrevented || event.repeat || event.isComposing || composing.current || event.keyCode === 229) return
    if (matches(event, { key: 'k', modifiers: ['Mod'] }, platform)) { event.preventDefault(); event.stopPropagation(); setOverlay('palette'); return }
    if (overlay !== null && event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setOverlay(null); return }
    if (overlay !== null) return
    const editable = editableTarget(event.target)
    if (event.key === '?' && !event.metaKey && !event.ctrlKey && !event.altKey && !editable) { event.preventDefault(); event.stopPropagation(); setOverlay('shortcuts'); return }
    const command = commands.find(entry => entry.shortcut !== undefined && (!editable || entry.allowInEditable === true) && matches(event, chordFor(entry.shortcut, platform), platform))
    if (command !== undefined) { event.preventDefault(); event.stopPropagation(); perform(command.id) }
  })
  React.useLayoutEffect(() => {
    document.addEventListener('keydown', keyDown, true)
    return () => document.removeEventListener('keydown', keyDown, true)
  }, [])
  return <Registry.Provider value={registry}><div ref={setPortalContainer} data-command-scope onCompositionStartCapture={() => { composing.current = true }} onCompositionEndCapture={() => { composing.current = false }} {...stylex.props(styles.scope)}>{children}
    <ModalOverlay UNSTABLE_portalContainer={portalContainer ?? undefined} isOpen={overlay !== null} onOpenChange={open => { if (!open) setOverlay(null) }} isDismissable {...stylex.props(styles.scrim)}><Modal {...stylex.props(styles.modal)}><Dialog aria-label={overlay === 'palette' ? 'Command palette' : 'Keyboard shortcuts'} {...stylex.props(styles.dialog)}>
      <div {...stylex.props(styles.heading)}><Heading slot="title" {...stylex.props(styles.title)}>{overlay === 'palette' ? 'Command palette' : 'Keyboard shortcuts'}</Heading><Button onPress={() => setOverlay(null)} {...stylex.props(styles.close)}>Close · Esc</Button></div>
      {overlay === 'palette' ? <CommandPalette registry={registry} /> : <ShortcutList registry={registry} />}
    </Dialog></Modal></ModalOverlay>
  </div></Registry.Provider>
}
function groups(commands: readonly KitCommand[]) {
  const result = new Map<string, KitCommand[]>()
  for (const command of commands) { const group = result.get(command.group); if (group) group.push(command); else result.set(command.group, [command]) }
  return [...result]
}
function CommandPalette({ registry }: { readonly registry: CommandRegistry }) {
  return <Autocomplete filter={fuzzyCommandMatch}>
    <SearchField aria-label="Search commands" {...stylex.props(styles.search)}><Input autoFocus placeholder="Search commands…" {...stylex.props(styles.input)} /></SearchField>
    <Menu aria-label="Commands" onAction={id => registry.perform(String(id))} disabledKeys={registry.commands.filter(command => commandDisabledReason(command) !== undefined).map(command => command.id)} renderEmptyState={() => <span {...stylex.props(styles.empty)}>No matching commands</span>} {...stylex.props(styles.list)}>
      {groups(registry.commands).map(([group, commands]) => <MenuSection key={group}><Header {...stylex.props(styles.group)}>{group}</Header>{commands.map(command => {
        const reason = commandDisabledReason(command)
        return <MenuItem key={command.id} id={command.id} textValue={command.label} className={state => stylex.props(styles.row, styles.menuRow, state.isFocused && styles.focused, state.isDisabled && styles.disabled).className ?? ''}>
          <Text slot="label"><span {...stylex.props(styles.rowLabel)}><span>{command.label}</span>{' '}<Shortcut shortcut={command.shortcut} platform={registry.platform} /></span>{reason !== undefined && <Text slot="description" {...stylex.props(styles.reason)}>{reason}</Text>}</Text>
        </MenuItem>
      })}</MenuSection>)}
    </Menu>
  </Autocomplete>
}
function ShortcutList({ registry }: { readonly registry: CommandRegistry }) {
  return <div {...stylex.props(styles.list)}>{groups(registry.commands).map(([group, commands]) => <section key={group} aria-label={group}><h3 {...stylex.props(styles.group)}>{group}</h3><ul {...stylex.props(styles.shortcuts)}>{commands.map(command => <li key={command.id} {...stylex.props(styles.row)}><div {...stylex.props(styles.copy)}><span>{command.label}</span>{commandDisabledReason(command) !== undefined && <span {...stylex.props(styles.reason)}>{commandDisabledReason(command)}</span>}</div><Shortcut shortcut={command.shortcut} platform={registry.platform} /></li>)}</ul></section>)}<p {...stylex.props(styles.reason)}>Open command palette: {formatCommandShortcut({ key: 'K', modifiers: ['Mod'] }, registry.platform)} · Keyboard shortcuts: ?</p></div>
}
function Shortcut({ shortcut, platform }: { readonly shortcut?: CommandShortcut; readonly platform: CommandPlatform }) {
  const label = formatCommandShortcut(shortcut, platform)
  return label === undefined ? null : <kbd {...stylex.props(styles.shortcut)}>{label}</kbd>
}
/** Uses the same registry as keyboard dispatch and palette; React Aria opens on hover and keyboard focus. */
export function CommandTooltip({ commandId, label, children, placement = 'bottom', delay = 300 }: { readonly commandId?: string; readonly label?: string; readonly children: React.ReactNode; readonly placement?: 'top' | 'bottom'; readonly delay?: number }) {
  const registry = useCommands()
  const command = registry?.commands.find(entry => entry.id === commandId)
  const text = command?.label ?? label
  if (text === undefined) return <>{children}</>
  return <TooltipTrigger delay={delay} closeDelay={0}>{children}<Tooltip UNSTABLE_portalContainer={registry?.portalContainer ?? undefined} placement={placement} offset={6} {...stylex.props(styles.tooltip)}><span>{text}</span>{command !== undefined && registry !== null && <Shortcut shortcut={command.shortcut} platform={registry.platform} />}</Tooltip></TooltipTrigger>
}
/** Existing controls may omit commandId; registered controls dispatch exclusively through the registry. */
export function CommandButton({ commandId, label, onPress, ...props }: ButtonProps & { readonly commandId?: string; readonly label: string }) {
  const registry = useCommands()
  const command = registry?.commands.find(entry => entry.id === commandId)
  return <CommandTooltip commandId={commandId} label={label}><Button {...props} aria-label={props['aria-label'] ?? label} isDisabled={props.isDisabled || (commandId !== undefined && command === undefined) || (command !== undefined && commandDisabledReason(command) !== undefined)} onPress={event => { if (commandId !== undefined) registry?.perform(commandId); else onPress?.(event) }} /></CommandTooltip>
}
const styles = stylex.create({
  scope: { display: 'contents' },
  scrim: { position: 'fixed', inset: s.zero, zIndex: 100, backgroundColor: surface.scrim, display: 'flex', alignItems: 'flex-start', justifyContent: 'center', paddingBlockStart: '12vh', paddingInline: s.xl, boxSizing: 'border-box' },
  modal: { width: `min(100%, ${g.drawerMax})`, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, boxShadow: elevation.popover, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  dialog: { outlineStyle: 'none', padding: s.lg },
  heading: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: s.md, marginBottom: s.md },
  title: { fontSize: t.headingSize, lineHeight: t.headingLeading, margin: s.zero },
  close: { backgroundColor: surface.controlFill, color: ink.fgMuted, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, padding: s.xs, fontSize: t.metaSize, ':focus-visible': { outline: `${g.focusRing} solid ${accent.primary}` } },
  search: { marginBottom: s.md },
  input: { width: '100%', boxSizing: 'border-box', backgroundColor: surface.canvas, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, padding: s.md, fontFamily: t.fontSans, fontSize: t.uiSize, ':focus-visible': { outline: `${g.focusRing} solid ${accent.primary}`, outlineOffset: g.focusOffset } },
  list: { maxHeight: g.commandMaxHeight, overflowY: 'auto', outlineStyle: 'none' },
  group: { fontSize: t.metaSize, fontWeight: t.weightMedium, color: ink.fgMuted, paddingBlock: s.md, paddingInline: s.sm, margin: s.zero },
  row: { display: 'flex', alignItems: 'center', gap: s.lg, justifyContent: 'space-between', padding: s.md, minHeight: g.menuRow, boxSizing: 'border-box', borderRadius: r.sm, outlineStyle: 'none' },
  menuRow: { flexDirection: 'column', alignItems: 'stretch', gap: s.xs },
  rowLabel: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: s.lg, minWidth: 0, overflowWrap: 'anywhere' },
  focused: { backgroundColor: surface.rowActive, outline: `${g.focusRing} solid ${accent.primary}`, outlineOffset: '-2px' },
  disabled: { color: ink.fgMuted },
  copy: { display: 'flex', flexDirection: 'column', minWidth: 0, overflowWrap: 'anywhere' },
  reason: { display: 'block', color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  shortcut: { flexShrink: 0, color: ink.fgMuted, fontFamily: t.fontMono, fontSize: t.metaSize, whiteSpace: 'nowrap' },
  shortcuts: { listStyleType: 'none', margin: s.zero, padding: s.zero },
  empty: { display: 'block', padding: s.xl, color: ink.fgMuted },
  tooltip: { display: 'flex', alignItems: 'center', gap: s.lg, maxWidth: g.tooltipMax, padding: s.md, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.control, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, boxShadow: elevation.popover },
})
