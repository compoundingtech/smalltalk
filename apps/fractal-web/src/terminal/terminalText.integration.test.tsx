// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

import { DomTerminal } from './DomTerminal.tsx'
import { makeScreen } from './fixtures.ts'
import { defaultTerminalPalette } from './palette.ts'
import { terminalMatches, terminalText } from './terminalText.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

let host: HTMLDivElement
let root: Root
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  Object.defineProperty(document, 'fonts', { configurable: true, value: ['Wf Terminal Mono', 'Wf Terminal Nerd Mono'].map((family) => ({ family, load: async () => [] })) })
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => {
  await act(async () => root.unmount())
  host.remove()
  vi.unstubAllGlobals()
})
const render = (node: React.ReactNode) => act(async () => root.render(<React.Suspense fallback={null}>{node}</React.Suspense>))

it('copies and finds text across styled runs and soft wraps without exposing redacted content', async () => {
  const screen = {
    ...makeScreen({ scene: 'F3', columns: 24, rows: 3, frame: 0 }),
    lines: [
      { row: 0, text: 'hello world', runs: [{ text: 'hello ', bold: true as const }, { text: 'world' }], wrapped: false, redacted: false, truncated: false },
      { row: 1, text: ' continuation', runs: [{ text: ' continuation' }], wrapped: true, redacted: false, truncated: false },
      { row: 2, text: 'secret', runs: [{ text: 'secret' }], redacted: true, truncated: false },
    ],
  }
  await render(<DomTerminal screen={screen} palette={defaultTerminalPalette} />)
  expect(host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)?.textContent).toContain('[redacted]')
  expect(terminalText({ viewport: host })).toBe('hello world continuation\n[redacted]')
  expect(terminalMatches({ viewport: host, query: 'world continuation' })[0]?.toString()).toBe('world continuation')
  expect(terminalMatches({ viewport: host, query: 'secret' })).toEqual([])
  expect(terminalMatches({ viewport: host, query: 'continuation[redacted]' })).toEqual([])
  await render(<DomTerminal screen={{ ...screen, lines: screen.lines.map((line) => Object.assign({}, line, { wrapped: false })) }} palette={defaultTerminalPalette} />)
  expect(terminalText({ viewport: host })).toBe('hello world\n continuation\n[redacted]')
})

it('updates a memoized OSC8 destination and refuses executable links', async () => {
  const base = makeScreen({ scene: 'F3', columns: 24, rows: 1, frame: 0 })
  const screen = (uri: string) => ({ ...base, lines: [{ row: 0, text: 'Review', runs: [{ text: 'Review', link: { uri } }], redacted: false, truncated: false }] })
  await render(<DomTerminal screen={screen('https://example.com/first')} palette={defaultTerminalPalette} />)
  expect(host.querySelector('a')?.getAttribute('href')).toBe('https://example.com/first')
  await render(<DomTerminal screen={screen('https://example.com/second')} palette={defaultTerminalPalette} />)
  expect(host.querySelector('a')?.getAttribute('href')).toBe('https://example.com/second')
  await render(<DomTerminal screen={screen('javascript:alert(1)')} palette={defaultTerminalPalette} />)
  expect(host.textContent).toContain('Review')
  expect(host.querySelector('a')).toBeNull()
})
