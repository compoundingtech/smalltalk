import type { TerminalModes } from '@smalltalk/st3-client/schema'

import { ghosttyKeyCodes } from './ghosttyKeyCodes.generated.ts'

/** The encoder executes Smalltalk's pinned native Ghostty WASM, not a JS approximation. */
export interface TerminalKeyEncoder {
  key(batch: { readonly event: BrowserTerminalKey; readonly modes: TerminalModes }): Uint8Array
  paste(batch: { readonly text: string; readonly modes: TerminalModes }): Uint8Array
  text(text: string): Uint8Array
  dispose(): void
}

/** One DOM keyboard event reduced to the fields the native encoder needs. */
export interface BrowserTerminalKey {
  readonly code: string
  readonly key: string
  readonly action: 'press' | 'repeat' | 'release'
  readonly shiftKey: boolean
  readonly ctrlKey: boolean
  readonly altKey: boolean
  readonly metaKey: boolean
  readonly capsLock?: boolean
  readonly numLock?: boolean
  readonly altGraph?: boolean
  readonly composing?: boolean
  /** Browser KeyboardLayoutMap, when available, supplies layout-specific unshifted keys. */
  readonly unshifted?: string | undefined
}

const punctuation: Readonly<Record<string, string>> = {
  Backquote: '`',
  Backslash: '\\',
  BracketLeft: '[',
  BracketRight: ']',
  Comma: ',',
  Equal: '=',
  Minus: '-',
  Period: '.',
  Quote: "'",
  Semicolon: ';',
  Slash: '/',
  Space: ' ',
}
const utf8 = new TextEncoder()

/** Load the pinned native Ghostty encoder asset for browser use. */
export const loadGhosttyKeyboard = async (): Promise<TerminalKeyEncoder> => {
  const response = await fetch(
    new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url),
  )
  if (!response.ok) throw new Error(`Native terminal encoder unavailable: HTTP ${response.status}`)
  return makeGhosttyKeyboard(await response.arrayBuffer())
}

/** Separate loader allows browser and Node proofs to execute the identical committed native asset. */
export const makeGhosttyKeyboard = async (bytes: BufferSource): Promise<TerminalKeyEncoder> => {
  let memory: WebAssembly.Memory | undefined
  const instance = await WebAssembly.instantiate(bytes, {
    env: {
      // oxlint-disable-next-line overeng/named-args -- WebAssembly fixes the positional (pointer, length) import ABI.
      log: (pointer: number, length: number) => {
        if (memory !== undefined)
          console.warn(new TextDecoder().decode(new Uint8Array(memory.buffer, pointer, length)))
      },
    },
  })
  const exports = instance.instance.exports
  if (!(exports.memory instanceof WebAssembly.Memory))
    throw new Error('Native encoder has no memory export')
  memory = exports.memory
  const heap = exports.memory
  const call = (name: string, ...args: number[]): number => {
    const fn = exports[name]
    if (typeof fn !== 'function') throw new Error(`Missing native encoder export: ${name}`)
    const result: unknown = fn(...args)
    if (result === undefined) return 0
    if (typeof result !== 'number') throw new Error(`Invalid native encoder return: ${name}`)
    return result
  }
  const checked = (name: string, ...args: number[]) => {
    const result = call(name, ...args)
    if (result !== 0) throw new Error(`Native terminal encoding failed (${name}: ${result})`)
  }
  const slot = call('ghostty_wasm_alloc_usize')
  const option = call('ghostty_wasm_alloc_u8')
  let encoder = 0
  let event = 0
  let input = 0
  let inputCapacity = 0
  let output = 0
  let outputCapacity = 0
  const view = () => new DataView(heap.buffer)
  try {
    checked('ghostty_key_encoder_new', 0, slot)
    encoder = view().getUint32(slot, true)
    checked('ghostty_key_event_new', 0, slot)
    event = view().getUint32(slot, true)
  } catch (cause) {
    call('ghostty_key_encoder_free', encoder)
    call('ghostty_wasm_free_usize', slot)
    call('ghostty_wasm_free_u8', option)
    throw cause
  }
  let disposed = false
  const assertLive = () => {
    if (disposed) throw new Error('Native terminal encoder was disposed')
  }
  const putInput = (text: string) => {
    const encoded = utf8.encode(text)
    if (encoded.length > inputCapacity) {
      if (input !== 0) call('ghostty_wasm_free_u8_array', input, inputCapacity)
      inputCapacity = Math.max(encoded.length, 128)
      input = call('ghostty_wasm_alloc_u8_array', inputCapacity)
    }
    if (encoded.length > 0) new Uint8Array(heap.buffer).set(encoded, input)
    return encoded.length
  }
  const encode = (run: (pointer: number, length: number) => number): Uint8Array => {
    // Query the native encoder; no fixed byte limit for composed or pasted text.
    run(0, 0)
    const required = view().getUint32(slot, true)
    if (required === 0) return new Uint8Array()
    if (required > outputCapacity) {
      if (output !== 0) call('ghostty_wasm_free_u8_array', output, outputCapacity)
      outputCapacity = Math.max(required, 128)
      output = call('ghostty_wasm_alloc_u8_array', outputCapacity)
    }
    const result = run(output, outputCapacity)
    if (result !== 0) throw new Error(`Native terminal encoding failed (${result})`)
    return new Uint8Array(heap.buffer, output, view().getUint32(slot, true)).slice()
  }
  const setOption = ({ id, value }: { readonly id: number; readonly value: number }) => {
    view().setUint8(option, value)
    call('ghostty_key_encoder_setopt', encoder, id, option)
  }
  return {
    key: ({ event: key, modes }) => {
      assertLive()
      setOption({ id: 0, value: Number(modes.application_cursor) })
      setOption({ id: 1, value: Number(modes.application_keypad) })
      setOption({ id: 3, value: 1 })
      setOption({ id: 5, value: modes.kitty_keyboard ?? 0 })
      const printable = [...key.key].length === 1 && key.key !== '\u007f'
      const text = printable && key.action !== 'release' ? key.key : ''
      const length = putInput(text)
      let code = key.code
      // Browsers retain the digit's physical code when NumLock maps it to navigation.
      if (
        code.startsWith('Numpad') &&
        [
          'ArrowUp',
          'ArrowDown',
          'ArrowLeft',
          'ArrowRight',
          'Home',
          'End',
          'Insert',
          'Delete',
          'PageUp',
          'PageDown',
          'Clear',
        ].includes(key.key)
      ) {
        code = `Numpad${key.key.replace('Arrow', '').replace('Clear', 'Begin')}`
      }
      call(
        'ghostty_key_event_set_action',
        event,
        key.action === 'release' ? 0 : key.action === 'repeat' ? 2 : 1,
      )
      call('ghostty_key_event_set_key', event, ghosttyKeyCodes[code] ?? 0)
      const mods =
        Number(key.shiftKey) |
        (Number(key.ctrlKey) << 1) |
        (Number(key.altKey) << 2) |
        (Number(key.metaKey) << 3) |
        (Number(key.capsLock ?? false) << 4) |
        (Number(key.numLock ?? false) << 5)
      call('ghostty_key_event_set_mods', event, mods)
      call('ghostty_key_event_set_consumed_mods', event, key.altGraph ? 6 : 0)
      call('ghostty_key_event_set_composing', event, Number(key.composing ?? false))
      call('ghostty_key_event_set_utf8', event, length === 0 ? 0 : input, length)
      // The browser has no synchronous keyboard-layout API. Use supplied layout map first,
      // logical letters next, and physical US punctuation only as the final fallback.
      const unshifted =
        key.unshifted ??
        (printable && /\p{L}/u.test(key.key)
          ? key.key.toLowerCase()
          : (punctuation[key.code] ?? (key.code.startsWith('Digit') ? key.code.slice(5) : text)))
      call('ghostty_key_event_set_unshifted_codepoint', event, unshifted.codePointAt(0) ?? 0)
      return encode((pointer, size) =>
        call('ghostty_key_encoder_encode', encoder, event, pointer, size, slot),
      )
    },
    paste: ({ text, modes }) => {
      assertLive()
      const length = putInput(text)
      if (length === 0) return new Uint8Array()
      return encode((pointer, size) =>
        call(
          'ghostty_paste_encode',
          input,
          length,
          Number(modes.bracketed_paste),
          pointer,
          size,
          slot,
        ),
      )
    },
    text: (text) => {
      assertLive()
      return utf8.encode(text)
    },
    dispose: () => {
      if (disposed) return
      disposed = true
      call('ghostty_key_event_free', event)
      call('ghostty_key_encoder_free', encoder)
      if (input !== 0) call('ghostty_wasm_free_u8_array', input, inputCapacity)
      if (output !== 0) call('ghostty_wasm_free_u8_array', output, outputCapacity)
      call('ghostty_wasm_free_usize', slot)
      call('ghostty_wasm_free_u8', option)
    },
  }
}
