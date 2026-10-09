import type { TerminalScreen } from './terminal-types'
type Key = Pick<KeyboardEvent, 'key' | 'code' | 'ctrlKey' | 'altKey' | 'metaKey' | 'shiftKey' | 'isComposing'>
/** UTF-8 strings, never base64. Shift+Tab remains a keyboard escape from the surface. */
export function encodeTerminalKey(event: Key, modes: TerminalScreen['modes']): string | undefined {
  if (event.isComposing || event.metaKey || (event.key === 'Tab' && event.shiftKey)) return
  let value: string | undefined
  if (modes.application_keypad && event.code.startsWith('Numpad')) {
    const keypad: Record<string, string> = { Numpad0: 'p', Numpad1: 'q', Numpad2: 'r', Numpad3: 's', Numpad4: 't', Numpad5: 'u', Numpad6: 'v', Numpad7: 'w', Numpad8: 'x', Numpad9: 'y', NumpadDecimal: 'n', NumpadAdd: 'k', NumpadSubtract: 'm', NumpadMultiply: 'j', NumpadDivide: 'o', NumpadEnter: 'M' }
    if (keypad[event.code]) value = '\x1bO' + keypad[event.code]
  }
  if (value === undefined && event.ctrlKey) {
    const key = event.key.toUpperCase()
    if (key.length === 1 && key.charCodeAt(0) >= 64 && key.charCodeAt(0) <= 95) value = String.fromCharCode(key.charCodeAt(0) & 31)
    else if (key === ' ') value = '\0'
    else if (key === '?') value = '\x7f'
  } else if (value === undefined) {
    const arrows: Record<string, string> = { ArrowUp: 'A', ArrowDown: 'B', ArrowRight: 'C', ArrowLeft: 'D', Home: 'H', End: 'F' }
    const fixed: Record<string, string> = { Enter: '\r', Backspace: '\x7f', Tab: '\t', Escape: '\x1b', Delete: '\x1b[3~', Insert: '\x1b[2~', PageUp: '\x1b[5~', PageDown: '\x1b[6~', F1: '\x1bOP', F2: '\x1bOQ', F3: '\x1bOR', F4: '\x1bOS', F5: '\x1b[15~', F6: '\x1b[17~', F7: '\x1b[18~', F8: '\x1b[19~', F9: '\x1b[20~', F10: '\x1b[21~', F11: '\x1b[23~', F12: '\x1b[24~' }
    value = arrows[event.key] ? '\x1b' + (modes.application_cursor ? 'O' : '[') + arrows[event.key] : fixed[event.key] ?? (Array.from(event.key).length === 1 ? event.key : undefined)
  }
  return value !== undefined && event.altKey ? '\x1b' + value : value
}
export function encodeTerminalPaste(text: string, bracketed: boolean): string {
  const normalized = text.replace(/\r\n|\n/g, '\r')
  return bracketed ? '\x1b[200~' + normalized + '\x1b[201~' : normalized
}
export interface LocalHistory { readonly lines: TerminalScreen['lines']; readonly truncated: boolean }
/** Projected snapshots contain no PTY history. Only positively matched upward shifts are retained. */
export function appendLocalHistory(previous: TerminalScreen | null, next: TerminalScreen | null, history: LocalHistory, limit: number): LocalHistory {
  const cap = Math.max(0, Math.floor(limit))
  if (!previous || !next || previous.terminal_id !== next.terminal_id || previous.runtime_incarnation !== next.runtime_incarnation || next.modes.alternate_screen) return { lines: [], truncated: false }
  if (previous.revision === next.revision) return { lines: cap === 0 ? [] : history.lines.slice(-cap), truncated: history.truncated || history.lines.length > cap }
  let shifted = 0
  for (let offset = 1; offset < previous.lines.length; offset++) {
    const overlap = previous.lines.length - offset
    if (overlap <= next.lines.length && previous.lines.slice(offset).every((line, index) => JSON.stringify(line.runs) === JSON.stringify(next.lines[index]?.runs) && line.text === next.lines[index]?.text)) { shifted = offset; break }
  }
  const added = previous.lines.slice(0, shifted)
  const all = [...history.lines, ...added]
  return { lines: cap === 0 ? [] : all.slice(-cap), truncated: history.truncated || all.length > cap }
}
