import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import { readFile } from 'node:fs/promises'
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

// Same real source transform as the existing kit SSR tests, without a browser.
const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, plugins: [stylex.vite({ useCSSLayers: false })], server: { middlewareMode: true } })
after(() => server.close())
const { TerminalSurface } = await server.ssrLoadModule('/src/assistant-ui/terminal/TerminalSurface.tsx')
const { createTerminalPalette } = await server.ssrLoadModule('/src/assistant-ui/terminal/terminal-palette.ts')
const { appendLocalHistory } = await server.ssrLoadModule('/src/assistant-ui/terminal/terminal-behavior.ts')
const screen = { kind: 'terminal-screen', terminal_id: 'synthetic-shell', runtime_incarnation: 'synthetic-process', revision: '1', next_sequence: 1, title: 'Synthetic shell', columns: 80, rows: 1, truncated: false, cursor: { row: 0, column: 0, visible: true, blinking: true, style: 'block' }, modes: { alternate_screen: false, application_cursor: false, application_keypad: false, bracketed_paste: false, focus_events: false, mouse_tracking: 'none', mouse_encoding: 'default' }, lines: [{ row: 0, text: 'wide 界 output', runs: [{ text: 'wide 界 output', fg: 2 }], redacted: false, truncated: false }] }
for (const scheme of ['dark', 'light']) {
  test(`null-screen SSR is static (${scheme})`, () => {
    assert.equal(typeof window, 'undefined')
    const markup = renderToStaticMarkup(React.createElement(TerminalSurface, { screen: null, connection: { state: 'connecting' }, readOnly: true, palette: createTerminalPalette(scheme) }))
    assert.match(markup, /Connecting to the terminal\./)
    assert.doesNotMatch(markup, /Unknown/)
    // Base fixture control: a placeholder is not projected terminal output.
    assert.throws(() => assert.match(markup, /wide 界 output/))
  })
  test(`screen SSR renders real runs (${scheme})`, () => {
    assert.equal(typeof window, 'undefined')
    const markup = renderToStaticMarkup(React.createElement(TerminalSurface, { screen, connection: { state: 'live' }, readOnly: false, palette: createTerminalPalette(scheme) }))
    assert.match(markup, /wide 界 output/)
    assert.match(markup, /Local scrollback/)
    assert.match(markup, new RegExp(scheme === 'dark' ? '#7295ed' : '#4269df'))
  })
}
test('terminal renderer has no emulator or runtime action imports', async () => {
  const sources = await Promise.all(['TerminalSurface.tsx', 'TerminalDrawer.tsx', 'terminal-palette.ts', 'terminal-behavior.ts', 'terminal-types.ts'].map(name => readFile(new URL(name, import.meta.url), 'utf8')))
  const assertion = source => { assert.doesNotMatch(source, /(?:from\s*|import\s*[(']?\s*)['"](?:@xterm\/|xterm)/); assert.doesNotMatch(source, /import\s+(?!type\b)[^\n]+st3-client/) }
  for (const source of sources) assertion(source)
  assert.throws(() => assertion('import "xterm"'))
  assert.throws(() => assertion('import { terminalInput } from "st3-client"'))
})
test('revision-only changes on repeated rows do not fabricate local history', () => {
  const previous = { ...screen, lines: Array.from({ length: 4 }, (_, row) => ({ row, text: '', runs: [], redacted: false, truncated: false })) }
  const next = { ...previous, revision: '2', cursor: { ...previous.cursor, column: 1 } }
  assert.deepEqual(appendLocalHistory(previous, next, { lines: [], truncated: false }, 1000), { lines: [], truncated: false })
})
