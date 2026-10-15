import { readFile } from 'node:fs/promises'
import { describe, expect, it } from 'vitest'

import { makeGhosttyKeyboardOn } from './ghosttyKeyboard.ts'
import { instantiateGhosttyNative, makeGhosttyTerminal } from './ghosttyVt.ts'

const meta = { terminalId: 'terminal/test', incarnation: 'test-incarnation', fallbackTitle: 'Terminal' }
const utf8 = new TextEncoder()
const setup = async (cols = 20, rows = 4) => {
  const native = await instantiateGhosttyNative(await readFile(new URL('./assets/ghostty-key-encoder.generated.wasm', import.meta.url)))
  const terminal = makeGhosttyTerminal(native, { cols, rows })
  return { native, terminal, write: (text: string) => terminal.write(utf8.encode(text)), screen: () => terminal.snapshot(meta) }
}

describe('native Ghostty VT viewport', () => {
  it('projects text, palette and RGB styles, wide cells and continuation wraps', async () => {
    const { terminal, write, screen } = await setup(12, 4)
    try {
      write('hi \x1b[31mred\x1b[38;2;1;2;3mRGB\x1b[0m漢字')
      const result = screen()
      expect(result.lines[0]?.runs).toEqual([
        { text: 'hi ', cells: 3 }, { text: 'red', cells: 3, fg: 1 },
        { text: 'RGB', cells: 3, fg: '#010203' }, { text: '漢', cells: 2 },
      ])
      expect(result.lines[1]).toMatchObject({ text: '字', wrapped: true, runs: [{ text: '字', cells: 2 }] })
    } finally { terminal.dispose() }
  })

  it('admits HTTPS and absolute file links but rejects unsafe OSC 8 targets', async () => {
    const { terminal, write, screen } = await setup(40)
    try {
      write('\x1b]8;;https://example.com/path\x1b\\safe\x1b]8;;\x1b\\ ' +
        '\x1b]8;;javascript:alert(1)\x1b\\unsafe\x1b]8;;\x1b\\ ' +
        '\x1b]8;;file:///tmp/demo\x1b\\file\x1b]8;;\x1b\\')
      expect(screen().lines[0]?.runs).toEqual([
        { text: 'safe', cells: 4, link: { uri: 'https://example.com/path' } },
        { text: ' unsafe ', cells: 8 },
        { text: 'file', cells: 4, link: { uri: 'file:///tmp/demo' } },
      ])
    } finally { terminal.dispose() }
  })

  it('keeps erased background blanks and trims invisible trailing blanks', async () => {
    const { terminal, write, screen } = await setup(8)
    try {
      write('x\x1b[44m\x1b[K\x1b[0m\r\nq\x1b[8msecret\x1b[0m')
      expect(screen().lines[0]).toMatchObject({ text: 'x', runs: [{ text: 'x', cells: 1 }, { text: '       ', cells: 7, bg: 4 }] })
      expect(screen().lines[1]?.runs).toEqual([{ text: 'q', cells: 1 }])
      write('\r\n\x1b[48;2;10;20;30m\x1b[K')
      expect(screen().lines[2]?.runs).toEqual([{ text: '        ', cells: 8, bg: '#0a141e' }])
    } finally { terminal.dispose() }
  })

  it('reads cursor visibility, DECSCUSR style and blinking from native render state', async () => {
    const { terminal, write, screen } = await setup()
    try {
      write('\x1b[3;5H\x1b[6 q')
      expect(screen().cursor).toEqual({ row: 2, column: 4, visible: true, style: 'bar', blinking: false })
      write('\x1b[3 q\x1b[?25l')
      expect(screen().cursor).toMatchObject({ visible: false, style: 'underline', blinking: true })
      write('\x1b[2 q\x1b[?25h')
      expect(screen().cursor).toMatchObject({ visible: true, style: 'block', blinking: false })
    } finally { terminal.dispose() }
  })

  it('shares native memory with key encoding and follows DEC, kitty and mouse modes', async () => {
    const { native, terminal, write, screen } = await setup()
    const encoder = makeGhosttyKeyboardOn(native)
    try {
      write('\x1b[?1h\x1b[?66h\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h')
      expect(terminal.modes()).toEqual({ alternate_screen: false, application_cursor: true, application_keypad: true,
        bracketed_paste: true, focus_events: true, mouse_tracking: 'drag', mouse_encoding: 'sgr', kitty_keyboard: 0 })
      expect(new TextDecoder().decode(encoder.key({ event: { code: 'ArrowUp', key: 'ArrowUp', action: 'press',
        shiftKey: false, ctrlKey: false, altKey: false, metaKey: false }, modes: terminal.modes() }))).toBe('\x1bOA')
      write('\x1b[?1049h\x1b[>1u\x1b]2;Native title\x07')
      expect(terminal.modes()).toMatchObject({ alternate_screen: true, kitty_keyboard: 1 })
      expect(screen().title).toBe('Native title')
      write('\x1b[?1049l')
      expect(terminal.modes().alternate_screen).toBe(false)
    } finally { encoder.dispose(); terminal.dispose() }
  })

  it('preserves row and snapshot identity, resizes and fully resets', async () => {
    const { terminal, write, screen } = await setup()
    try {
      write('first\r\nsecond')
      const first = screen()
      expect(screen()).toBe(first)
      write('!')
      const next = screen()
      expect(next).not.toBe(first)
      expect(next.lines[0]).toBe(first.lines[0])
      expect(next.lines[1]).not.toBe(first.lines[1])
      expect(next.lines[2]).toBe(first.lines[2])
      expect(Number(next.revision)).toBeGreaterThan(Number(first.revision))
      terminal.resize({ cols: 30, rows: 6 })
      expect(screen()).toMatchObject({ columns: 30, rows: 6 })
      terminal.reset()
      expect(screen().lines.every((line) => line.text === '' && line.runs.length === 0)).toBe(true)
      expect(screen().title).toBe('Terminal')
    } finally { terminal.dispose(); terminal.dispose() }
  })

  it('renders history from the viewport and resumes following at bottom', async () => {
    const { terminal, write, screen } = await setup(10, 3)
    try {
      write('one\r\ntwo\r\nthree\r\nfour\r\nfive')
      expect(screen().lines.map((line) => line.text)).toEqual(['three', 'four', 'five'])
      expect(terminal.following()).toBe(true)
      terminal.scroll(-2)
      expect(terminal.following()).toBe(false)
      expect(screen().lines.map((line) => line.text)).toEqual(['one', 'two', 'three'])
      expect(screen().cursor.visible).toBe(false)
      terminal.scroll('bottom')
      expect(terminal.following()).toBe(true)
      expect(screen().lines.map((line) => line.text)).toEqual(['three', 'four', 'five'])
    } finally { terminal.dispose() }
  })

  it('caps lines and UTF-8 bytes at the server projection limits', async () => {
    const { terminal, write, screen } = await setup(5000, 201)
    try {
      write('a'.repeat(4100))
      const result = screen()
      expect(result.truncated).toBe(true)
      expect(result.lines).toHaveLength(200)
      expect(result.lines[0]).toMatchObject({ truncated: true, text: 'a'.repeat(4096), runs: [{ cells: 4096, text: 'a'.repeat(4096) }] })
    } finally { terminal.dispose() }
  })
})
