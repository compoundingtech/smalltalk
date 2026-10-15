/**
 * Maps bytes from the native Ghostty encoder onto the gateway's `terminal.input` modes.
 *
 * The encoder is mode-aware (application cursor, keypad, kitty flags, bracketed paste), so its
 * bytes are what reaches the PTY. A chunk that is exactly one control key the gateway can name
 * travels in `key` mode under that name; the gateway resolves the name back to the same bytes.
 * Everything else travels base64-encoded in `raw` mode. `line` mode is never used for typing: it
 * appends Enter and needs a screen-sequence fence that a moving screen cannot hold.
 */

/** One `terminal.input` parameter pair. */
export type TerminalInputWire =
  | { readonly mode: 'key'; readonly value: string }
  | { readonly mode: 'raw'; readonly value: string }

const named: ReadonlyArray<readonly [string, string]> = [
  ['enter', '\r'],
  ['tab', '\t'],
  ['shift+tab', '\x1b[Z'],
  ['escape', '\x1b'],
  ['backspace', '\x7f'],
  ['delete', '\x1b[3~'],
  ['up', '\x1b[A'],
  ['down', '\x1b[B'],
  ['right', '\x1b[C'],
  ['left', '\x1b[D'],
  ['home', '\x1b[H'],
  ['end', '\x1b[F'],
  ['pageup', '\x1b[5~'],
  ['pagedown', '\x1b[6~'],
  ['f1', '\x1bOP'],
  ['f2', '\x1bOQ'],
  ['f3', '\x1bOR'],
  ['f4', '\x1bOS'],
  ['f5', '\x1b[15~'],
  ['f6', '\x1b[17~'],
  ['f7', '\x1b[18~'],
  ['f8', '\x1b[19~'],
  ['f9', '\x1b[20~'],
  ['f10', '\x1b[21~'],
  ['f11', '\x1b[23~'],
  ['f12', '\x1b[24~'],
]

// Named keys win over their Ctrl-letter aliases (Ctrl-I is Tab, Ctrl-M is Enter, Ctrl-[ is Escape).
const keyNames = new Map<string, string>([
  ...Array.from({ length: 26 }, (_, index): readonly [string, string] => [
    String.fromCharCode(index + 1),
    `ctrl+${String.fromCharCode(97 + index)}`,
  ]),
  ...named.map(([name, bytes]): readonly [string, string] => [bytes, name]),
])

const latin1 = (bytes: Uint8Array) => {
  let text = ''
  for (const byte of bytes) text += String.fromCharCode(byte)
  return text
}

/** Bytes as one base64 `raw` write. */
export const rawInput = (bytes: Uint8Array): TerminalInputWire => ({ mode: 'raw', value: btoa(latin1(bytes)) })

/** `undefined` when the chunk holds a NUL byte, which no gateway input mode can carry. */
export const terminalInputWire = (bytes: Uint8Array): TerminalInputWire | undefined => {
  if (bytes.includes(0)) return undefined
  const name = keyNames.get(latin1(bytes))
  return name === undefined ? rawInput(bytes) : { mode: 'key', value: name }
}
