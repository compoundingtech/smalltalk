// Keybinding strings (`mod+shift+p`) parsed once, matched against KeyboardEvents and rendered as
// Keyboard `Kbd` chips. Shape: `<Kbd keys="mod+shift+p" />` plus `matchKeybinding`.

import * as stylex from '@stylexjs/stylex'

import { Kbd } from '../ui-compat/components.tsx'
import { scale } from '../ui-compat/tokens.stylex.ts'

/** Parsed chord with exact modifiers and a normalized event key. */
export interface Keybinding {
  readonly mod: boolean
  readonly ctrl: boolean
  readonly alt: boolean
  readonly shift: boolean
  /** Lower-cased `KeyboardEvent.key`, or `digit` + n for `1`…`9` (layout-independent via `code`). */
  readonly key: string
}

/** Platform convention for the primary modifier and keyboard chip labels. */
export type Platform = 'mac' | 'other'

/** Detects Apple keyboard conventions, defaulting to non-Apple during server rendering. */
export const detectPlatform = (): Platform =>
  typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform) ? 'mac' : 'other'

const aliases: Readonly<Record<string, string>> = {
  esc: 'escape',
  left: 'arrowleft',
  right: 'arrowright',
  up: 'arrowup',
  down: 'arrowdown',
  space: ' ',
  plus: '+',
}

/** Parses `mod+shift+p` / `ctrl+\\` / `alt+shift+left`. Unknown modifiers are a programming error. */
export const parseKeybinding = (source: string): Keybinding => {
  const parts = source.toLowerCase().split('+')
  const raw = parts.pop() ?? ''
  const mods = new Set(parts)
  for (const mod of mods) {
    if (!['mod', 'ctrl', 'alt', 'shift'].includes(mod))
      throw new Error(`Unknown modifier "${mod}" in "${source}"`)
  }
  return {
    mod: mods.has('mod'),
    ctrl: mods.has('ctrl'),
    alt: mods.has('alt'),
    shift: mods.has('shift'),
    key: aliases[raw] ?? raw,
  }
}

/** Canonical form, used to detect two bindings claiming the same chord. */
export const keybindingId = (binding: Keybinding): string =>
  [
    binding.mod && 'mod',
    binding.ctrl && 'ctrl',
    binding.alt && 'alt',
    binding.shift && 'shift',
    binding.key,
  ]
    .filter(Boolean)
    .join('+')

const eventKey = (event: KeyboardEvent): string => {
  // `code` keeps digits and `\` stable under Shift/Alt and non-US layouts.
  if (/^Digit[0-9]$/.test(event.code)) return event.code.slice(5)
  if (event.code === 'Backslash') return '\\'
  if (event.code === 'Backquote') return '`'
  if (/^Key[A-Z]$/.test(event.code)) return event.code.slice(3).toLowerCase()
  return event.key.toLowerCase()
}

/** Matches the key and every modifier, resolving `mod` according to the platform. */
export const matchKeybinding = ({
  event,
  binding,
  platform,
}: {
  readonly event: KeyboardEvent
  readonly binding: Keybinding
  readonly platform: Platform
}): boolean => {
  const wantMeta = platform === 'mac' ? binding.mod : false
  const wantCtrl = platform === 'mac' ? binding.ctrl : binding.mod || binding.ctrl
  return (
    event.metaKey === wantMeta &&
    event.ctrlKey === wantCtrl &&
    event.altKey === binding.alt &&
    event.shiftKey === binding.shift &&
    eventKey(event) === binding.key
  )
}

const keyLabel: Readonly<Record<string, string>> = {
  arrowleft: '←',
  arrowright: '→',
  arrowup: '↑',
  arrowdown: '↓',
  escape: 'Esc',
  enter: '↵',
  tab: 'Tab',
  ' ': 'Space',
  pageup: 'PgUp',
  pagedown: 'PgDn',
}

/** Chip labels in platform order: `⌘⇧P` on mac, `Ctrl Shift P` elsewhere. */
export const formatKeybinding = ({
  binding,
  platform,
}: {
  readonly binding: Keybinding
  readonly platform: Platform
}): readonly string[] => {
  const key = keyLabel[binding.key] ?? binding.key.toUpperCase()
  return platform === 'mac'
    ? [
        binding.ctrl && '⌃',
        binding.alt && '⌥',
        binding.shift && '⇧',
        binding.mod && '⌘',
        key,
      ].filter((part): part is string => typeof part === 'string')
    : [
        (binding.mod || binding.ctrl) && 'Ctrl',
        binding.alt && 'Alt',
        binding.shift && 'Shift',
        key,
      ].filter((part): part is string => typeof part === 'string')
}

const styles = stylex.create({
  chord: { display: 'inline-flex', gap: scale.space1, flexShrink: 0 },
})

/** Renders a keybinding string as keyboard `Kbd` chips. */
export const KeybindingHint = ({
  keys,
  platform,
}: {
  readonly keys: string
  readonly platform: Platform
}) => (
  <span
    {...stylex.props(styles.chord)}
    aria-label={formatKeybinding({ binding: parseKeybinding(keys), platform }).join(' ')}
  >
    {formatKeybinding({ binding: parseKeybinding(keys), platform }).map((part) => (
      <Kbd key={part} size="small">
        {part}
      </Kbd>
    ))}
  </span>
)
