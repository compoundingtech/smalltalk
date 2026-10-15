import { readFile } from 'node:fs/promises'
import { afterAll, beforeAll, describe, expect, it } from 'vitest'

import { keyboardTestModes } from './fixtures.ts'
import { makeGhosttyKeyboard, type TerminalKeyEncoder } from './ghosttyKeyboard.ts'
import {
  admitPaste,
  admitText,
  directTextMaxBytes,
  isBulkText,
  makeCompositionGate,
  normalizePaste,
  pasteConfirmBytes,
  pasteMaxBytes,
  routeKey,
  type KeyIntent,
  type TerminalAdmission,
} from './rawTerminalInput.ts'

const decode = new TextDecoder()
let encoder: TerminalKeyEncoder

beforeAll(async () => {
  encoder = await makeGhosttyKeyboard(await readFile(new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url)))
})
afterAll(() => encoder.dispose())

const sent = (admission: TerminalAdmission) =>
  admission._tag === 'Refuse' ? admission.notice : `${admission._tag}:${decode.decode(admission.bytes)}`

describe('raw terminal input encoding', () => {
  it('brackets a paste per the emulator modes and turns CRLF into CR', () => {
    expect(normalizePaste('one\r\ntwo\r\n')).toBe('one\rtwo\r')
    expect(sent(admitPaste({ encoder, text: 'one\r\ntwo', modes: keyboardTestModes }))).toBe('Send:\x1b[200~one\rtwo\x1b[201~')
    expect(sent(admitPaste({ encoder, text: 'one\r\ntwo', modes: { ...keyboardTestModes, bracketed_paste: false } }))).toBe('Send:one\rtwo')
  })

  it('removes bracketed-paste markers, including ones spliced together by a removal', () => {
    expect(normalizePaste('a\x1b[201~b\x1b[200~c')).toBe('abc')
    expect(normalizePaste('a\x1b[20\x1b[201~1~b')).toBe('ab')
    // A pasted end marker cannot close the bracket early and run the rest as keys.
    expect(sent(admitPaste({ encoder, text: 'x\x1b[201~rm -rf /\r', modes: keyboardTestModes }))).toBe('Send:\x1b[200~xrm -rf /\r\x1b[201~')
  })

  it('confirms a large paste and refuses one past the cap', () => {
    expect(admitPaste({ encoder, text: 'x'.repeat(pasteConfirmBytes - 20), modes: keyboardTestModes })._tag).toBe('Send')
    expect(admitPaste({ encoder, text: 'x'.repeat(pasteConfirmBytes + 1), modes: keyboardTestModes })._tag).toBe('Confirm')
    expect(admitPaste({ encoder, text: 'z'.repeat(pasteMaxBytes + 1), modes: keyboardTestModes })).toEqual({
      _tag: 'Refuse',
      notice: 'This paste is larger than 64 KB. Nothing was sent.',
    })
  })

  it('sends a short committed line as text and routes bulk insertions through the paste path', () => {
    expect(sent(admitText({ encoder, text: '日本語', modes: keyboardTestModes }))).toBe('Send:日本語')
    expect(sent(admitText({ encoder, text: 'one\ntwo', modes: keyboardTestModes }))).toBe('Send:\x1b[200~one\ntwo\x1b[201~')
    expect(isBulkText('x'.repeat(directTextMaxBytes))).toBe(false)
    expect(isBulkText('x'.repeat(directTextMaxBytes + 1))).toBe(true)
    expect(isBulkText('a\x1bb')).toBe(true)
    expect(isBulkText('a\x7f')).toBe(true)
  })
})

describe('IME composition', () => {
  it('admits a composition once at compositionend, ignoring the trailing duplicate insert', () => {
    const gate = makeCompositionGate()
    gate.start()
    expect(gate.beforeInput({ inputType: 'insertCompositionText', data: 'に', isComposing: true })).toEqual({ _tag: 'Pass' })
    expect(gate.composing()).toBe(true)
    expect(gate.end('日本')).toBe('日本')
    expect(gate.composing()).toBe(false)
    // Safari and some Chromium IMEs repeat the commit as an insert after compositionend.
    expect(gate.beforeInput({ inputType: 'insertFromComposition', data: '日本', isComposing: false })).toEqual({ _tag: 'Take', text: undefined })
    // The next ordinary insertion is admitted again.
    expect(gate.beforeInput({ inputType: 'insertText', data: '日本', isComposing: false })).toEqual({ _tag: 'Take', text: '日本' })
  })

  it('forgets the commit once a key is pressed, and leaves non-insert edits and empty commits alone', () => {
    const gate = makeCompositionGate()
    gate.start()
    expect(gate.end('')).toBeUndefined()
    gate.start()
    gate.end('漢')
    gate.key()
    expect(gate.beforeInput({ inputType: 'insertText', data: '漢', isComposing: false })).toEqual({ _tag: 'Take', text: '漢' })
    expect(gate.beforeInput({ inputType: 'deleteContentBackward', data: null, isComposing: false })).toEqual({ _tag: 'Pass' })
    expect(gate.beforeInput({ inputType: 'insertFromDrop', data: 'x', isComposing: false })).toEqual({ _tag: 'Drop' })
  })

  it('encodes committed text as UTF-8 for the PTY', () => {
    expect([...encoder.text('é日')]).toEqual([...new TextEncoder().encode('é日')])
  })
})

describe('key routing', () => {
  const key = (event: Partial<KeyIntent> & Pick<KeyIntent, 'key' | 'code'>): KeyIntent => ({
    ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, isComposing: false, keyCode: 0, ...event,
  })
  const route = (event: Partial<KeyIntent> & Pick<KeyIntent, 'key' | 'code'>, { typing = true, hasSelection = false } = {}) =>
    routeKey({ event: key(event), typing, hasSelection })

  it('gives Ctrl-C to the terminal unless a selection is there to copy', () => {
    expect(route({ key: 'c', code: 'KeyC', ctrlKey: true })).toBe('encode')
    expect(route({ key: 'c', code: 'KeyC', ctrlKey: true }, { hasSelection: true })).toBe('copy')
    expect(route({ key: 'C', code: 'KeyC', ctrlKey: true, shiftKey: true })).toBe('copy')
    expect(route({ key: 'c', code: 'KeyC', metaKey: true })).toBe('copy')
  })

  it('leaves paste, find and command shortcuts to the platform and the pane', () => {
    expect(route({ key: 'v', code: 'KeyV', ctrlKey: true })).toBe('paste')
    expect(route({ key: 'v', code: 'KeyV', metaKey: true })).toBe('paste')
    expect(route({ key: 'f', code: 'KeyF', ctrlKey: true })).toBe('find')
    expect(route({ key: 'w', code: 'KeyW', metaKey: true })).toBe('browser')
  })

  it('never traps keyboard focus: Ctrl-\\ leaves typing, Tab moves on from the unfocused terminal', () => {
    expect(route({ key: '\\', code: 'Backslash', ctrlKey: true })).toBe('leave')
    expect(route({ key: 'Tab', code: 'Tab' }, { typing: false })).toBe('navigate')
    expect(route({ key: 'Tab', code: 'Tab' })).toBe('encode')
    expect(route({ key: 'a', code: 'KeyA' }, { typing: false })).toBe('encode')
  })

  it('lets the IME own keys while composing', () => {
    expect(route({ key: 'Process', code: 'KeyN', keyCode: 229 })).toBe('compose')
    expect(route({ key: 'Enter', code: 'Enter', isComposing: true })).toBe('compose')
  })
})
