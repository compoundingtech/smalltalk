import type { TerminalModes } from '@smalltalk/st3-client/schema'
import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { describe, expect, it } from 'vitest'

import { makeGhosttyKeyboard, type BrowserTerminalKey } from './ghosttyKeyboard.ts'
import { keyboardTestModes } from './terminalInputFixture.ts'

const asset = (name: string) => readFile(new URL(`./assets/${name}`, import.meta.url))

const base = { action: 'press', shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as const
const decode = new TextDecoder()

describe('native Ghostty key encoder', () => {
  it('executes the committed asset named by its provenance record', async () => {
    const provenance: unknown = JSON.parse((await asset('ghostty-key-encoder.generated.json')).toString('utf8'))
    const bytes = await asset('ghostty-key-encoder.generated.wasm')
    expect(provenance).toMatchObject({ generated: true, wasmSha256: createHash('sha256').update(bytes).digest('hex') })
  })

  it('encodes cursor, keypad, modifier, kitty flags and safe bracketed paste natively', async () => {
    const encoder = await makeGhosttyKeyboard(await asset('ghostty-key-encoder.generated.wasm'))
    const key = (event: Partial<BrowserTerminalKey> & Pick<BrowserTerminalKey, 'code' | 'key'>, modes: Partial<TerminalModes> = {}) =>
      decode.decode(encoder.key({ event: { ...base, ...event }, modes: { ...keyboardTestModes, ...modes } }))
    try {
      expect(key({ code: 'ArrowUp', key: 'ArrowUp' })).toBe('\x1b[A')
      expect(key({ code: 'ArrowUp', key: 'ArrowUp' }, { application_cursor: true })).toBe('\x1bOA')
      expect(key({ code: 'Numpad1', key: '1' }, { application_keypad: true })).toBe('\x1bOq')
      expect(key({ code: 'KeyC', key: 'c', ctrlKey: true })).toBe('\x03')
      expect(key({ code: 'KeyC', key: 'C', ctrlKey: true, shiftKey: true })).toBe('\x1b[99;6u')
      expect(key({ code: 'KeyX', key: 'x', altKey: true })).toBe('\x1bx')
      expect(key({ code: 'KeyA', key: 'A', shiftKey: true }, { kitty_keyboard: 31 })).toBe('\x1b[97:65;2;65u')
      expect(key({ code: 'KeyA', key: 'a', action: 'release' }, { kitty_keyboard: 31 })).toBe('\x1b[97;1:3u')
      expect(decode.decode(encoder.paste({ text: 'one\ntwo\x1b[201~', modes: keyboardTestModes }))).toBe('\x1b[200~one\ntwo [201~\x1b[201~')
      expect(decode.decode(encoder.paste({ text: 'one\ntwo', modes: { ...keyboardTestModes, bracketed_paste: false } }))).toBe('one\rtwo')
      expect(encoder.key({ event: { ...base, code: 'Enter', key: 'Enter', composing: true }, modes: keyboardTestModes })).toEqual(new Uint8Array())
    } finally {
      encoder.dispose()
    }
  })
})
