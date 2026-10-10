import { readFile } from 'node:fs/promises'
import { describe, expect, it } from 'vitest'

import { makeGhosttyKeyboard, type BrowserTerminalKey } from './ghosttyKeyboard.ts'
import { keyboardTestModes } from './terminalInputFixture.ts'
import { directTextMaxBytes, isBulkText, normalizePaste, terminalInputWire } from './terminalInputWire.ts'

const bytes = (value: string) => Uint8Array.from(value, (char) => char.charCodeAt(0))
const utf8 = new TextEncoder()
const base = { action: 'press', shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as const

describe('terminal input wire', () => {
  it('names exact control keys in key mode and keeps everything else raw', () => {
    expect(terminalInputWire(bytes('\x03'))).toEqual({ mode: 'key', value: 'ctrl+c' })
    expect(terminalInputWire(bytes('\r'))).toEqual({ mode: 'key', value: 'enter' })
    expect(terminalInputWire(bytes('\t'))).toEqual({ mode: 'key', value: 'tab' })
    expect(terminalInputWire(bytes('\x1b'))).toEqual({ mode: 'key', value: 'escape' })
    expect(terminalInputWire(bytes('\x7f'))).toEqual({ mode: 'key', value: 'backspace' })
    expect(terminalInputWire(bytes('\x1b[A'))).toEqual({ mode: 'key', value: 'up' })
    expect(terminalInputWire(bytes('\x1b[Z'))).toEqual({ mode: 'key', value: 'shift+tab' })
    // Application-cursor arrows are not a gateway key name: their exact bytes travel raw.
    expect(terminalInputWire(bytes('\x1bOA'))).toEqual({ mode: 'raw', value: btoa('\x1bOA') })
    expect(terminalInputWire(bytes(' '))).toEqual({ mode: 'raw', value: btoa(' ') })
    expect(terminalInputWire(utf8.encode('echo 日本'))).toEqual({
      mode: 'raw',
      value: Buffer.from('echo 日本').toString('base64'),
    })
    expect(terminalInputWire(bytes('a\x00b'))).toBeUndefined()
  })

  it('maps the native encoder output for typed keys onto the wire', async () => {
    const encoder = await makeGhosttyKeyboard(await readFile(new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url)))
    const key = (event: Partial<BrowserTerminalKey> & Pick<BrowserTerminalKey, 'code' | 'key'>, modes = keyboardTestModes) =>
      terminalInputWire(encoder.key({ event: { ...base, ...event }, modes }))
    try {
      expect(key({ code: 'KeyC', key: 'c', ctrlKey: true })).toEqual({ mode: 'key', value: 'ctrl+c' })
      expect(key({ code: 'Enter', key: 'Enter' })).toEqual({ mode: 'key', value: 'enter' })
      expect(key({ code: 'ArrowUp', key: 'ArrowUp' }, { ...keyboardTestModes, application_cursor: true })).toEqual({ mode: 'raw', value: btoa('\x1bOA') })
      expect(terminalInputWire(encoder.text('ls'))).toEqual({ mode: 'raw', value: btoa('ls') })
    } finally {
      encoder.dispose()
    }
  })

  it('brackets a paste per the screen mode and turns CRLF into CR', async () => {
    const encoder = await makeGhosttyKeyboard(await readFile(new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url)))
    const decode = new TextDecoder()
    try {
      expect(normalizePaste('one\r\ntwo\r\n')).toBe('one\rtwo\r')
      expect(decode.decode(encoder.paste({ text: normalizePaste('one\r\ntwo'), modes: keyboardTestModes }))).toBe('\x1b[200~one\rtwo\x1b[201~')
      expect(decode.decode(encoder.paste({ text: normalizePaste('one\r\ntwo'), modes: { ...keyboardTestModes, bracketed_paste: false } }))).toBe('one\rtwo')
    } finally {
      encoder.dispose()
    }
  })

  it('removes bracketed-paste markers, including ones spliced together by a removal', () => {
    expect(normalizePaste('a\x1b[201~b\x1b[200~c')).toBe('abc')
    expect(normalizePaste('a\x1b[20\x1b[201~1~b')).toBe('ab')
  })

  it('admits only a short single line of inserted text directly', () => {
    expect(isBulkText('日本語')).toBe(false)
    expect(isBulkText('x'.repeat(directTextMaxBytes))).toBe(false)
    expect(isBulkText('x'.repeat(directTextMaxBytes + 1))).toBe(true)
    expect(isBulkText('one\ntwo')).toBe(true)
    expect(isBulkText('a\x1bb')).toBe(true)
    expect(isBulkText('a\x7f')).toBe(true)
  })
})
