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
const button = (name: string, scope: ParentNode = host) => [...scope.querySelectorAll('button')].find((candidate) => candidate.textContent === name)
const paste = (text: string) => act(async () => {
  const event = new Event('paste', { bubbles: true, cancelable: true })
  Object.defineProperty(event, 'clipboardData', { value: { getData: () => text } })
  field().dispatchEvent(event)
})
const key = (init: KeyboardEventInit) => act(async () => {
  field().dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }))
  field().dispatchEvent(new KeyboardEvent('keyup', { bubbles: true, cancelable: true, ...init }))
})
const compose = (data: string) => act(async () => {
  field().dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
  field().dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data }))
})
const insertText = (data: string) => act(async () => {
  field().dispatchEvent(new InputEvent('beforeinput', { bubbles: true, cancelable: true, inputType: 'insertText', data }))
})
const decoded = () => registry.get(fixtureWrites!).map((wire) => wire.mode === 'key' ? wire.value : new TextDecoder().decode(Uint8Array.from(atob(wire.value), (char) => char.charCodeAt(0)))).join('')
let fixtureWrites: ReturnType<typeof makeTerminalInputFixture>['writes'] | undefined
const armed = async () => {
  const fixture = makeTerminalInputFixture({ registry })
  fixtureWrites = fixture.writes
  await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} />)
  await act(async () => button('Enable input')!.click())
  return fixture
}

describe('terminal keyboard', () => {
  it('stays disabled and sends nothing when the source publishes no ordered input factory', async () => {
    await render(<TerminalKeyboardBinding terminalRef="terminal/example" incarnation="incarnation/example" modes={keyboardTestModes} />)
    expect(field().disabled).toBe(true)
    expect(button('Enable input')).toBeUndefined()
    expect(host.querySelector('[role="status"]')?.textContent).toBe('Ordered terminal input is not available from this producer')
    expect(host.querySelector('[data-terminal-input-state]')?.getAttribute('data-terminal-input-state')).toBe('Closed')
  })

  it('sends keys, paste and committed IME text in order through one armed session and never replays after a refusal', async () => {
    const fixture = makeTerminalInputFixture({ registry, autoDeliver: false })
    try {
      await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} />)
      expect(field().disabled).toBe(true)
      await act(async () => button('Enable input')!.click())
      expect(field().disabled).toBe(false)
      await key({ key: 'c', code: 'KeyC', ctrlKey: true })
      await key({ key: 'ArrowUp', code: 'ArrowUp' })
      await paste('one\r\ntwo')
      await act(async () => {
        field().dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
        field().dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'Enter', code: 'Enter', isComposing: true }))
        field().dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本語' }))
      })
      // One action in flight: the rest wait in order behind it.
      expect(registry.get(fixture.writes)).toEqual([{ mode: 'key', value: 'ctrl+c' }])
      for (let answered = 0; answered < 3; answered += 1) await act(async () => fixture.answer({ _tag: 'Delivered' }))
      const decoded = registry.get(fixture.writes).map((wire) => wire.mode === 'key' ? wire.value : new TextDecoder().decode(Uint8Array.from(atob(wire.value), (char) => char.charCodeAt(0))))
      // Key releases encode to nothing in legacy mode; waiting raw chunks merge without reordering.
      expect(decoded).toEqual(['ctrl+c', 'up', '\x1b[200~one\rtwo\x1b[201~日本語'])
      await key({ key: 'b', code: 'KeyB' })
      await key({ key: 'c', code: 'KeyC' })
      await act(async () => fixture.answer({ _tag: 'Refused', reason: 'The terminal changed before this input arrived. Queued keys were dropped.' }))
      expect(field().disabled).toBe(true)
      expect(host.querySelector('[role="status"]')?.textContent).toContain('The terminal changed before this input arrived.')
      await key({ key: 'a', code: 'KeyA' })
      // The refused chunk is not resent, and the key queued behind it is dropped.
      expect(registry.get(fixture.writes)).toEqual([expect.anything(), expect.anything(), expect.anything(), { mode: 'raw', value: btoa('b') }])
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
      expect(registry.get(fixture.writes)).toEqual([{ mode: 'raw', value: btoa('a') }])
    } finally {
      fixture.dispose()
    }
  })

  it('asks before sending a paste over 4 KB and sends it only on confirmation', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    try {
      await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} />)
      await act(async () => button('Enable input')!.click())
      await paste('x'.repeat(5000))
      const dialog = document.querySelector('[role="dialog"]')
      expect(dialog?.textContent).toContain('Paste into the terminal?')
      expect(dialog?.textContent).toContain('This paste is 5 KB.')
      expect(registry.get(fixture.writes)).toEqual([])
      await act(async () => button('Cancel', document.body)!.click())
      expect(document.querySelector('[role="dialog"]')).toBeNull()
      expect(registry.get(fixture.writes)).toEqual([])
      expect(registry.get(fixture.port.state)._tag).toBe('Ready')
      await paste('y'.repeat(5000))
      await act(async () => button('Paste', document.body)!.click())
      expect(registry.get(fixture.writes)).toEqual([{ mode: 'raw', value: btoa(`\x1b[200~${'y'.repeat(5000)}\x1b[201~`) }])
    } finally {
      fixture.dispose()
    }
  })

  it('refuses a paste over the size cap with fixed copy and sends nothing', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    try {
      await render(<TerminalKeyboard modes={keyboardTestModes} port={fixture.port} encoder={encoder} />)
      await act(async () => button('Enable input')!.click())
      await paste('z'.repeat(70 * 1024))
      expect(host.querySelector('[role="alert"]')?.textContent).toBe('This paste is larger than 64 KB. Nothing was sent.')
      expect(document.querySelector('[role="dialog"]')).toBeNull()
      expect(registry.get(fixture.writes)).toEqual([])
    } finally {
      fixture.dispose()
    }
  })

  describe.each([
    ['an IME commit', compose],
    ['inserted text', insertText],
  ] as const)('admits %s by the same policy as a paste', (_source, enter) => {
    it('sends short text directly and multi-line text as one bracketed paste', async () => {
      const fixture = await armed()
      try {
        await enter('日本')
        expect(decoded()).toBe('日本')
        await enter('echo one\r\necho two')
        expect(decoded()).toBe('日本\x1b[200~echo one\recho two\x1b[201~')
      } finally {
        fixture.dispose()
      }
    })

    it('asks before sending bulk text over 4 KB and refuses it over 64 KB', async () => {
      const fixture = await armed()
      try {
        await enter('x'.repeat(5000))
        expect(document.querySelector('[role="dialog"]')?.textContent).toContain('This paste is 5 KB.')
        expect(decoded()).toBe('')
        await act(async () => button('Cancel', document.body)!.click())
        await enter('z'.repeat(70 * 1024))
        expect(host.querySelector('[role="alert"]')?.textContent).toBe('This paste is larger than 64 KB. Nothing was sent.')
        expect(decoded()).toBe('')
      } finally {
        fixture.dispose()
      }
    })
  })

  it.each([
    ['the clipboard', paste],
    ['an IME commit', compose],
    ['inserted text', insertText],
  ] as const)('neutralizes an embedded bracketed-paste end marker from %s', async (_source, enter) => {
    const fixture = await armed()
    try {
      await enter('safe\x1b[201~more')
      expect(decoded()).toBe('\x1b[200~safemore\x1b[201~')
    } finally {
      fixture.dispose()
    }
  })

  it('rejects dropped text with fixed copy and sends nothing', async () => {
    const fixture = await armed()
    try {
      const drop = new Event('drop', { bubbles: true, cancelable: true })
      await act(async () => {
        field().dispatchEvent(drop)
      })
      expect(drop.defaultPrevented).toBe(true)
      expect(host.querySelector('[role="alert"]')?.textContent).toBe('Dropped text is not sent to the terminal. Paste it instead.')
      expect(decoded()).toBe('')
    } finally {
      fixture.dispose()
    }
  })
})
