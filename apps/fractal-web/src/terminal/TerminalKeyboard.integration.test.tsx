// @vitest-environment jsdom
import { RegistryContext } from '@effect/atom-react'
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { makeGhosttyKeyboard, type TerminalKeyEncoder } from './ghosttyKeyboard.ts'
import { keyboardTestModes, makeTerminalInputFixture } from './terminalInputFixture.ts'
import { TerminalKeyboard } from './TerminalKeyboard.tsx'
import { TerminalKeyboardBinding } from './TerminalKeyboardBinding.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

let registry: AtomRegistry.AtomRegistry
let host: HTMLDivElement
let root: Root
let encoder: TerminalKeyEncoder
beforeEach(async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
  // jsdom serves modules over http; read the committed asset from disk instead.
  encoder = await makeGhosttyKeyboard(await readFile(join(import.meta.dirname, 'assets/ghostty-key-encoder.generated.wasm')))
})
afterEach(async () => {
  await act(async () => root.unmount())
  encoder.dispose()
  registry.dispose()
  host.remove()
  vi.unstubAllGlobals()
})

const render = (node: React.ReactNode) => act(async () => root.render(<RegistryContext.Provider value={registry}>{node}</RegistryContext.Provider>))
const field = () => host.querySelector('textarea')!
const button = (name: string) => [...host.querySelectorAll('button')].find((candidate) => candidate.textContent === name)
const key = (init: KeyboardEventInit) => act(async () => {
  field().dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }))
  field().dispatchEvent(new KeyboardEvent('keyup', { bubbles: true, cancelable: true, ...init }))
})

describe('terminal keyboard', () => {
  it('stays disabled and sends nothing when the source publishes no ordered input factory', async () => {
    await render(<TerminalKeyboardBinding terminalRef="terminal/example" incarnation="incarnation/example" modes={keyboardTestModes} />)
    expect(field().disabled).toBe(true)
    expect(button('Enable input')).toBeUndefined()
    expect(host.querySelector('[role="status"]')?.textContent).toBe('Ordered terminal input is not available from this producer')
    expect(host.querySelector('[data-terminal-input-state]')?.getAttribute('data-terminal-input-state')).toBe('Closed')
  })

  it('sends keys, paste and committed IME text in order through one armed session and never replays after disconnect', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    try {
      await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} />)
      expect(field().disabled).toBe(true)
      await act(async () => button('Enable input')!.click())
      expect(field().disabled).toBe(false)
      await key({ key: 'c', code: 'KeyC', ctrlKey: true })
      await key({ key: 'ArrowUp', code: 'ArrowUp' })
      await act(async () => {
        const paste = new Event('paste', { bubbles: true, cancelable: true })
        Object.defineProperty(paste, 'clipboardData', { value: { getData: () => 'one\ntwo' } })
        field().dispatchEvent(paste)
      })
      await act(async () => {
        field().dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
        field().dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'Enter', code: 'Enter', isComposing: true }))
        field().dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本語' }))
      })
      const decode = new TextDecoder()
      // Key releases encode to nothing in legacy mode, so only presses, paste and IME text reach the peer.
      expect(registry.get(fixture.writes).map((batch) => decode.decode(new Uint8Array(batch.bytes)))).toEqual(['\x03', '\x1b[A', '\x1b[200~one\ntwo\x1b[201~', '日本語'])
      expect(registry.get(fixture.writes).map((batch) => batch.seq)).toEqual([7, 8, 9, 10])
      await act(async () => fixture.disconnect())
      expect(field().disabled).toBe(true)
      expect(host.querySelector('[role="status"]')?.textContent).toContain('disconnected')
      await key({ key: 'a', code: 'KeyA' })
      expect(registry.get(fixture.writes)).toHaveLength(4)
    } finally {
      fixture.dispose()
    }
  })

  it('leaves input on Ctrl-\\ and requires an explicit new session', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    try {
      await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} onNewSession={() => {}} />)
      await act(async () => button('Enable input')!.click())
      await key({ key: 'a', code: 'KeyA' })
      await key({ key: '\\', code: 'Backslash', ctrlKey: true })
      expect(field().disabled).toBe(true)
      expect(button('New input session')).toBeDefined()
      expect(registry.get(fixture.writes)).toEqual([{ seq: 7, bytes: [97] }])
    } finally {
      fixture.dispose()
    }
  })
})
