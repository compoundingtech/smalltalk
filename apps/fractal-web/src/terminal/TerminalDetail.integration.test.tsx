// @vitest-environment jsdom
import { Agent, decodeUnknownSync, type TerminalScreen } from '@smalltalk/st3-client/schema'
import type { PtyMode, PtyOutput, PtySession, PtySize, PtyState, PtyTarget } from '@st3/sdk/effect'
import { Effect, SubscriptionRef } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { observed, unavailable, waiting, type Feed, type Grants } from '../data/source.ts'
import { makeActionTerminalInput } from './actionTerminalInput.ts'
import { makeScreen } from './fixtures.ts'
import type { TerminalInputPortFactory } from './orderedTerminalInput.ts'
import { resizeSettleMillis, type RawTerminalPort } from './rawTerminalState.ts'
import { rawInput, type TerminalInputWire } from './terminalInputWire.ts'
import { TerminalDetail } from './TerminalDetail.tsx'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

const ref = 'terminal/example'
// The daemon names the terminal `terminal/<agent id>`, not by the view's subject address.
const screen: TerminalScreen = { ...makeScreen({ scene: 'F3', columns: 40, rows: 4, frame: 0 }), terminal_id: 'terminal/agent/example' }
const other: `terminal/${string}` = 'terminal/other'
const otherScreen: TerminalScreen = { ...screen, terminal_id: 'terminal/agent/other', runtime_incarnation: 'incarnation/other' }
const agent = (reachability: 'reachable' | 'unreachable') => decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability, state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
const utf8 = new TextEncoder()
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
let frames: FrameRequestCallback[]
/**
 * Every ResizeObserver the pane creates. The viewport always follows its own geometry for scrolling;
 * only a raw ATTACH turns an observed size into a RESIZE vote.
 */
let observers: Array<{ readonly callback: ResizeObserverCallback; readonly elements: Element[]; readonly self: ResizeObserver }>
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  observers = []
  vi.stubGlobal('ResizeObserver', class {
    readonly elements: Element[] = []
    constructor(callback: ResizeObserverCallback) {
      observers.push({ callback, elements: this.elements, self: this as unknown as ResizeObserver })
    }
    observe(element: Element) { this.elements.push(element) }
    unobserve() {}
    disconnect() { this.elements.length = 0 }
  })
  frames = []
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => frames.push(callback))
  vi.stubGlobal('cancelAnimationFrame', () => {})
  // jsdom fetches no module assets; serve the committed Ghostty module the browser would load.
  vi.stubGlobal('fetch', async () => new Response(new Uint8Array(await readFile(join(import.meta.dirname, 'assets/ghostty-key-encoder.generated.wasm'))), { headers: { 'content-type': 'application/wasm' } }))
  // jsdom has no font loading; the registered terminal faces resolve as loaded.
  Object.defineProperty(document, 'fonts', { configurable: true, value: ['Wf Terminal Mono', 'Wf Terminal Nerd Mono'].map((family) => ({ family, load: async () => [] })) })
  registry = AtomRegistry.make()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => {
  await act(async () => root.unmount())
  registry.dispose()
  host.remove()
  vi.unstubAllGlobals()
  Reflect.deleteProperty(document, 'visibilityState')
})
const isFeed = (terminal: Feed<TerminalScreen> | Readonly<Record<string, Feed<TerminalScreen>>>): terminal is Feed<TerminalScreen> =>
  typeof terminal._tag === 'string'
const mount = async ({ terminal, reachability = 'reachable', terminalRaw, terminalInput, grants, at = ref }: {
  readonly terminal: Feed<TerminalScreen> | Readonly<Record<string, Feed<TerminalScreen>>>
  readonly reachability?: 'reachable' | 'unreachable'
  readonly terminalRaw?: RawTerminalPort
  readonly terminalInput?: TerminalInputPortFactory
  readonly grants?: Grants
  readonly at?: `terminal/${string}`
}) => {
  const base = fixtureSource({ world: { now: 0, agents: [agent(reachability)], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } }, overrides: { terminal: isFeed(terminal) ? { [ref]: terminal } : terminal, ...(grants === undefined ? {} : { grants }) } })
  const source = { ...base, ...(terminalRaw === undefined ? {} : { terminalRaw }), ...(terminalInput === undefined ? {} : { terminalInput }) }
  await act(async () => root.render(<DataSourceProvider source={source} registry={registry}><React.Suspense fallback={<p>suspended</p>}><TerminalDetail key={at} address={{ ref: at, presentation: 'detail' }} visibility="visible" /></React.Suspense></DataSourceProvider>))
}
const chip = () => host.querySelector('[data-terminal-mode]')
const field = () => host.querySelector<HTMLTextAreaElement>('textarea[aria-label="Terminal input"]')
const paint = () => act(async () => {
  for (const callback of frames.splice(0)) callback(0)
})

interface FakeSession {
  readonly target: PtyTarget
  readonly mode: PtyMode
  readonly size: PtySize
  readonly sent: string[]
  readonly resized: PtySize[]
  readonly emit: (output: PtyOutput) => void
  readonly go: (state: PtyState) => Promise<void>
  closed: boolean
}

/** A raw port whose sessions the test drives: state transitions, output frames, and recorded DATA. */
const rawSessions = () => {
  const opened: FakeSession[] = []
  const decode = new TextDecoder()
  const port: RawTerminalPort = {
    open: ({ target, mode, size, onOutput }) =>
      Effect.gen(function* () {
        const state = yield* SubscriptionRef.make<PtyState>({ _tag: 'Connecting', attempt: 1 })
        let current: PtyState = { _tag: 'Connecting', attempt: 1 }
        const entry: FakeSession = {
          target,
          mode,
          size,
          sent: [],
          resized: [],
          emit: (output) => act(async () => onOutput(output)),
          go: (next) => act(async () => {
            current = next
            Effect.runSync(SubscriptionRef.set(state, next))
          }),
          closed: false,
        }
        opened.push(entry)
        yield* Effect.addFinalizer(() => Effect.sync(() => {
          entry.closed = true
        }))
        const session: PtySession = {
          mode,
          state,
          send: (bytes) => {
            // As the SDK: a PEEK socket never carries DATA.
            if (current._tag !== 'Live' || mode !== 'attach') return false
            entry.sent.push(decode.decode(bytes))
            return true
          },
          // Record every call: the pane, not the SDK, must keep a PEEK from voting a size.
          resize: (next) => {
            entry.resized.push(next)
          },
          resetInputModes: () => false,
        }
        return session
      }),
  }
  return { opened, port }
}

/**
 * Gateway `terminal.input` sessions over the production ordered input; every write is delivered.
 * With `hold`, each post stays in flight until the test releases it, so later keys queue behind it.
 */
const gatewaySessions = ({ hold = false }: { readonly hold?: boolean } = {}) => {
  const writes: TerminalInputWire[] = []
  const bound: Array<{ readonly subject: string; readonly terminalRef: string; readonly incarnation: string }> = []
  const held: Array<() => void> = []
  const factory: TerminalInputPortFactory = async ({ registry: sessionRegistry, ...binding }) => {
    bound.push(binding)
    return makeActionTerminalInput({
      registry: sessionRegistry,
      submit: async (wire) => {
        writes.push(wire)
        if (hold) {
          const delivered = Promise.withResolvers<void>()
          held.push(delivered.resolve)
          await delivered.promise
        }
        return { _tag: 'Delivered' }
      },
    })
  }
  return { writes, bound, held, factory }
}
/**
 * A hypothetical grant combination, not a current device class: the pinned server grants
 * `terminal.input` only with the `terminal.control` scope. It pins the input-only branch of the
 * decision; today the Gateway path is reached after a granted ATTACH is refused or lost.
 */
const inputWithoutControl: Grants = { actions: 'granted', messageSend: 'ungranted', terminalInput: 'granted', terminalControl: 'ungranted' }
/** What a limited pairing holds on the pinned server: PEEK only (compoundingtech/smalltalk#2873). */
const watchOnly: Grants = { actions: 'granted', messageSend: 'ungranted', terminalInput: 'ungranted', terminalControl: 'ungranted' }
/** Let every settled promise run its continuations: one macrotask turn, no timer advance. */
const drain = () => act(() => new Promise<void>((resolve) => setTimeout(resolve, 0)))
/** The page goes to the background (hidden tab) or is being unloaded or frozen into the bfcache. */
const leavePage = (how: 'hidden' | 'pagehide') => act(async () => {
  if (how === 'pagehide') window.dispatchEvent(new Event('pagehide'))
  else {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' })
    document.dispatchEvent(new Event('visibilitychange'))
  }
})
const returnToPage = () => act(async () => {
  Reflect.deleteProperty(document, 'visibilityState')
  document.dispatchEvent(new Event('visibilitychange'))
})
const press = (key: string, code: string, init: KeyboardEventInit = {}) =>
  field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key, code, ...init }))
const resizeButton = () => [...host.querySelectorAll('button')].find((button) => button.getAttribute('aria-label') === 'Resize terminal')
/**
 * Lay the pane out at 800×400 px with an 8 px cell, notify every live ResizeObserver and let a
 * pending RESIZE settle. Only an ATTACH may turn this into a vote.
 */
const rect = (width: number, height: number) => ({ x: 0, y: 0, top: 0, left: 0, right: width, bottom: height, width, height, toJSON: () => ({}) }) as DOMRect
const relayout = async () => {
  const measured = vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue(rect(80, 16))
  try {
    await act(async () => {
      for (const { callback, elements, self } of observers)
        if (elements.length > 0)
          callback(elements.map((target) => ({ target, contentRect: rect(800, 400) }) as unknown as ResizeObserverEntry), self)
      const settled = Promise.withResolvers<void>()
      setTimeout(settled.resolve, resizeSettleMillis + 30)
      await settled.promise
    })
  } finally {
    measured.mockRestore()
  }
}
const watching = () => observers.filter(({ elements }) => elements.length > 0).length

/** Attach, go live and paint the server's SCREEN replay. */
const live = async (session: FakeSession, text: string) => {
  await session.go({ _tag: 'Live', rows: 4, cols: 40 })
  await session.emit({ _tag: 'Geometry', rows: 4, cols: 40 })
  await session.emit({ _tag: 'Screen', bytes: utf8.encode(text) })
  await paint()
}

describe('terminal detail', () => {
  it.each([
    ['loading', waiting, 'Loading terminal…'],
    ['ungranted', unavailable({ reason: 'ungranted', detail: 'Terminal viewing is not permitted.' }), 'Terminal viewing is not permitted.'],
    ['unsupported', unavailable({ reason: 'unsupported', detail: 'This gateway does not provide terminals.' }), 'This gateway does not provide terminals.'],
    ['failed', unavailable({ reason: 'failed', detail: 'This agent has no terminal.', code: 'no-terminal' }), 'This agent has no terminal.'],
  ] as const)('states %s with its specific reason', async (state, terminal, copy) => {
    await mount({ terminal })
    const status = host.querySelector('[data-terminal-state]')
    expect(status?.getAttribute('data-terminal-state')).toBe(state)
    expect(host.textContent).toBe(copy)
    expect(host.textContent).not.toMatch(/\b(undefined|unknown)\b/i)
  })

  it.each(['', '   ', 'undefined', 'unknown'])('does not render a missing reason (%s)', async detail => {
    await mount({ terminal: unavailable({ reason: 'failed', detail }) })
    expect(host.textContent).toBe('The terminal could not be opened.')
    expect(host.textContent).not.toMatch(/\b(undefined|unknown)\b/i)
  })

  it('renders the projected grid read-only when the source has no raw stream, with history honestly unavailable', async () => {
    await mount({ terminal: observed({ value: screen }) })
    const grid = host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)
    expect(grid?.getAttribute('data-terminal-renderer')).toBe('T1')
    expect(grid?.querySelectorAll('[data-terminal-row]')).toHaveLength(screen.lines.length)
    expect(grid?.textContent).toContain(screen.lines.find((line) => line.text.trim() !== '')?.text.trim())
    expect(host.querySelector('[aria-label="Live"]')).not.toBeNull()
    expect(chip()?.textContent).toBe('Read-only')
    expect(host.textContent).toContain('This gateway does not offer a live terminal stream.')
    expect(field()).toBeNull()
    expect(host.textContent).not.toContain('Enable input')
    expect(host.textContent).not.toContain('Terminal keyboard')
    expect(host.textContent).toContain('does not publish terminal history yet')
  })

  it('marks the terminal ended when its host is unreachable', async () => {
    await mount({ terminal: observed({ value: screen }), reachability: 'unreachable' })
    expect(host.querySelector('[aria-label="Ended"]')).not.toBeNull()
  })

  it('keeps the last grid but says it is stale', async () => {
    await mount({ terminal: observed({ value: screen, freshness: 'stale' }) })
    expect(host.textContent).toContain('Terminal snapshot is stale.')
    expect(host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)).not.toBeNull()
    expect(host.querySelector('[aria-label="Live"]')).toBeNull()
    expect(host.querySelector('[aria-label="Stale"]')).not.toBeNull()
  })
})

describe('raw terminal', () => {
  it('attaches to the shown incarnation, paints the replay through libghostty, and types on the same socket', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    const session = opened[0]!
    expect(session.target).toEqual({ terminalId: 'terminal/agent/example', incarnation: screen.runtime_incarnation })
    expect(session.size).toEqual({ cols: 40, rows: 4 })
    // Until the replay paints, the projected screen stands in.
    expect(chip()?.textContent).toBe('Connecting…')
    await live(session, 'raw \x1b[1mprompt\x1b[0m$ ')
    expect(host.querySelector('[aria-roledescription="terminal"]')?.textContent).toContain('raw prompt$')
    expect(chip()?.textContent).toBe('Click to type')
    expect(host.querySelector('[aria-label="Live"]')).not.toBeNull()
    await act(async () => field()!.focus())
    expect(chip()?.textContent).toBe('Typing')
    await act(async () => {
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'l', code: 'KeyL' }))
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'c', code: 'KeyC', ctrlKey: true }))
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Enter', code: 'Enter' }))
    })
    expect(session.sent).toEqual(['l', '\x03', '\r'])
    // Echo arrives as DATA and is painted on the next frame.
    await session.emit({ _tag: 'Data', bytes: utf8.encode('l\r\n') })
    await paint()
    expect(host.querySelector('[aria-roledescription="terminal"]')?.textContent).toContain('raw prompt$ l')
  })

  it('returns the emulator viewport from scrollback to live output with Latest, without typing', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    const session = opened[0]!
    await live(session, Array.from({ length: 12 }, (_, line) => `line${line}`).join('\r\n'))
    const grid = () => host.querySelector('[aria-roledescription="terminal"]')?.textContent ?? ''
    expect(grid()).toContain('line11')
    await act(async () => {
      host.querySelector('[aria-roledescription="terminal"]')!.dispatchEvent(new WheelEvent('wheel', { bubbles: true, deltaMode: 1, deltaY: -6 }))
    })
    await paint()
    expect(grid()).not.toContain('line11')
    const latest = [...host.querySelectorAll('button')].find((button) => button.textContent === 'Latest')!
    await act(async () => latest.click())
    await paint()
    expect(grid()).toContain('line11')
    expect(session.sent).toEqual([])
    expect(chip()?.textContent).toBe('Click to type')
    await session.emit({ _tag: 'Data', bytes: utf8.encode('\r\nline12') })
    await paint()
    expect(grid()).toContain('line12')
  })

  it('hands keyboard focus to the cursor field when the grid is tabbed to, so an IME composes from the first key', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    const session = opened[0]!
    await live(session, 'raw prompt$ ')
    await act(async () => host.querySelector<HTMLElement>('[aria-roledescription="terminal"]')!.focus())
    expect(document.activeElement).toBe(field())
    expect(chip()?.textContent).toBe('Typing')
    await act(async () => {
      field()!.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
      field()!.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本' }))
    })
    expect(session.sent).toEqual(['日本'])
    // Ctrl-\ leaves typing and parks focus on the grid instead of bouncing back into the field.
    await act(async () => {
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: '\\', code: 'Backslash', ctrlKey: true }))
    })
    expect(document.activeElement?.getAttribute('aria-roledescription')).toBe('terminal')
    expect(chip()?.textContent).toBe('Click to type')
  })

  it.each([
    [
      'a repeated Ctrl-\\ while already parked on the grid',
      async (grid: HTMLElement) => {
        grid.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: '\\', code: 'Backslash', ctrlKey: true }))
        grid.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: '\\', code: 'Backslash', ctrlKey: true }))
      },
    ],
    [
      'a press on the grid released outside the terminal',
      async (grid: HTMLElement) => {
        grid.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
        document.body.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }))
      },
    ],
  ] as const)('still hands keyboard re-entry to the cursor field after %s', async (_case, disturb) => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    const session = opened[0]!
    await live(session, 'raw prompt$ ')
    const grid = host.querySelector<HTMLElement>('[aria-roledescription="terminal"]')!
    await act(async () => grid.focus())
    await act(async () => {
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: '\\', code: 'Backslash', ctrlKey: true }))
    })
    expect(document.activeElement).toBe(grid)
    await act(async () => disturb(grid))
    const outside = document.body.appendChild(document.createElement('button'))
    await act(async () => outside.focus())
    await act(async () => grid.focus())
    outside.remove()
    expect(document.activeElement).toBe(field())
    await act(async () => {
      field()!.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
      field()!.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本' }))
    })
    expect(session.sent).toEqual(['日本'])
  })

  it('drops keys while reconnecting instead of queueing them, and says so', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    const session = opened[0]!
    await live(session, '$ ')
    await session.go({ _tag: 'Reconnecting', attempt: 1, delayMs: 500, issue: 'PTY socket closed' })
    expect(chip()?.textContent).toBe('Reconnecting…')
    await act(async () => {
      field()!.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'x', code: 'KeyX' }))
    })
    expect(session.sent).toEqual([])
    expect(host.querySelector('[role="alert"]')?.textContent).toBe('The terminal is not connected. Nothing was sent, and keys are never replayed.')
    await session.go({ _tag: 'Live', rows: 4, cols: 40 })
    expect(session.sent).toEqual([])
  })

  it('reopens a refused ATTACH as a read-only PEEK when the device cannot type through the gateway either', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    expect(opened[0]!.mode).toBe('attach')
    await opened[0]!.go({ _tag: 'Refused', message: 'forbidden', status: 403, code: 'forbidden' })
    await vi.waitFor(() => expect(opened).toHaveLength(2))
    expect(opened[0]!.closed).toBe(true)
    expect(opened[1]!.mode).toBe('peek')
    await live(opened[1]!, 'peek prompt$ ')
    expect(host.querySelector('[aria-roledescription="terminal"]')?.textContent).toContain('peek prompt$')
    expect(chip()?.textContent).toBe('Read-only')
    expect(chip()?.getAttribute('data-terminal-input-path')).toBe('ReadOnly')
    expect(host.textContent).toContain('This gateway does not accept typing into this terminal.')
    expect(field()).toBeNull()
  })

  it('shows the projected screen read-only when even the PEEK stream is refused', async () => {
    const { opened, port } = rawSessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, grants: watchOnly })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    expect(opened[0]!.mode).toBe('peek')
    await opened[0]!.go({ _tag: 'Refused', message: 'forbidden', status: 403, code: 'forbidden' })
    expect(chip()?.textContent).toBe('Read-only')
    expect(chip()?.getAttribute('data-terminal-mode')).toBe('Projected')
    expect(host.textContent).toContain('This device may watch this terminal but not type into it.')
    expect(host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)?.querySelectorAll('[data-terminal-row]')).toHaveLength(screen.lines.length)
    expect(field()).toBeNull()
    // The PEEK never retries as an ATTACH.
    expect(opened).toHaveLength(1)
  })

  it('detaches when the pane leaves and attaches the next terminal fresh', async () => {
    const { opened, port } = rawSessions()
    const terminals = { [ref]: observed({ value: screen }), [other]: observed({ value: otherScreen }) }
    await mount({ terminal: terminals, terminalRaw: port })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    await live(opened[0]!, '$ ')
    await mount({ terminal: terminals, terminalRaw: port, at: other })
    await vi.waitFor(() => expect(opened).toHaveLength(2))
    expect(opened[0]!.closed).toBe(true)
    expect(opened[1]!.target).toEqual({ terminalId: otherScreen.terminal_id, incarnation: otherScreen.runtime_incarnation })
    expect(opened[1]!.sent).toEqual([])
  })
})

describe('input capability', () => {
  it('types on the raw ATTACH socket and votes the element size when the device holds terminal.control', async () => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    expect(opened[0]!.mode).toBe('attach')
    await live(opened[0]!, '$ ')
    expect(chip()?.getAttribute('data-terminal-input-path')).toBe('Attach')
    await act(async () => field()!.focus())
    await act(async () => press('l', 'KeyL'))
    expect(opened[0]!.sent).toEqual(['l'])
    expect(gateway.bound).toEqual([])
    expect(gateway.writes).toEqual([])
    // The element's size is this client's vote, settled into one RESIZE; there is no explicit resize control.
    await relayout()
    expect(opened[0]!.resized).toEqual([expect.objectContaining({ cols: 100 })])
    expect(resizeButton()).toBeUndefined()
  })

  it('keeps gateway typing over a PEEK stream for terminal.input without terminal.control (hypothetical grant), with explicit resize only', async () => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory, grants: inputWithoutControl })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    expect(opened[0]!.mode).toBe('peek')
    await live(opened[0]!, 'peek prompt$ ')
    expect(host.querySelector('[aria-roledescription="terminal"]')?.textContent).toContain('peek prompt$')
    expect(chip()?.textContent).toBe('Click to type')
    expect(chip()?.getAttribute('data-terminal-input-path')).toBe('Gateway')
    expect(host.textContent).not.toContain('Enable input')
    await act(async () => field()!.focus())
    expect(chip()?.textContent).toBe('Typing')
    await act(async () => {
      press('l', 'KeyL')
      press('c', 'KeyC', { ctrlKey: true })
      press('Enter', 'Enter')
    })
    await vi.waitFor(() => expect(gateway.writes).toHaveLength(3))
    expect(gateway.writes).toEqual([rawInput(utf8.encode('l')), { mode: 'key', value: 'ctrl+c' }, { mode: 'key', value: 'enter' }])
    // One session, bound to the shown terminal and incarnation; nothing rode the PEEK socket.
    expect(gateway.bound).toEqual([{ subject: ref, terminalRef: screen.terminal_id, incarnation: screen.runtime_incarnation }])
    expect(opened[0]!.sent).toEqual([])
    // The viewport still follows its own geometry, but only an ATTACH votes a size; the explicit resize stays.
    expect(watching()).toBeGreaterThan(0)
    await relayout()
    expect(opened[0]!.resized).toEqual([])
    expect(resizeButton()).toBeDefined()
  })

  it('moves typing to the gateway when a granted ATTACH is refused', async () => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    await opened[0]!.go({ _tag: 'Refused', message: 'forbidden', status: 403, code: 'forbidden' })
    await vi.waitFor(() => expect(opened).toHaveLength(2))
    expect(opened[1]!.mode).toBe('peek')
    await live(opened[1]!, '$ ')
    expect(chip()?.getAttribute('data-terminal-input-path')).toBe('Gateway')
    await act(async () => field()!.focus())
    await act(async () => press('l', 'KeyL'))
    await vi.waitFor(() => expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))]))
    expect(opened.flatMap((session) => session.sent)).toEqual([])
    // The fallback PEEK never inherits the ATTACH's size vote.
    await relayout()
    expect(opened[1]!.resized).toEqual([])
    expect(resizeButton()).toBeDefined()
  })

  it.each(['hidden', 'pagehide'] as const)('stops gateway typing when the page is %s, drops queued keys, and waits for a click before typing again', async (how) => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions({ hold: true })
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    await opened[0]!.go({ _tag: 'Refused', message: 'forbidden', status: 403, code: 'forbidden' })
    await vi.waitFor(() => expect(opened).toHaveLength(2))
    await live(opened[1]!, '$ ')
    expect(chip()?.getAttribute('data-terminal-input-path')).toBe('Gateway')
    await act(async () => field()!.focus())
    // `l` is in flight; `a` and `s` queue behind it.
    await act(async () => {
      press('l', 'KeyL')
      press('a', 'KeyA')
      press('s', 'KeyS')
    })
    await vi.waitFor(() => expect(gateway.held).toHaveLength(1))
    expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))])
    await leavePage(how)
    // The in-flight post answers after the hide; the queued keys are gone and nothing else posts.
    await act(async () => gateway.held[0]!())
    await drain()
    expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))])
    expect(host.querySelector('[role="alert"]')?.textContent).toBe(
      'Typing stopped while this page was hidden; unsent keys were dropped. Click the terminal to type again.',
    )
    if (how === 'hidden') await returnToPage()
    // The field kept focus, but a key alone is not the gesture: it is refused and starts no session.
    expect(document.activeElement).toBe(field())
    expect(chip()?.textContent).toBe('Click to type')
    await act(async () => press('x', 'KeyX'))
    await drain()
    expect(gateway.bound).toHaveLength(1)
    expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))])
    expect(host.querySelector('[role="alert"]')?.textContent).toContain('Click the terminal to type again.')
    // A click on the terminal is the gesture: a new session starts and the next key posts.
    await act(async () => field()!.dispatchEvent(new MouseEvent('mouseup', { bubbles: true })))
    expect(chip()?.textContent).toBe('Typing')
    await act(async () => press('y', 'KeyY'))
    await vi.waitFor(() => expect(gateway.writes).toEqual([rawInput(utf8.encode('l')), rawInput(utf8.encode('y'))]))
    expect(gateway.bound).toHaveLength(2)
    expect(host.querySelector('[role="alert"]')).toBeNull()
    expect(opened.flatMap((session) => session.sent)).toEqual([])
  })

  it('does not resume gateway typing on the focus the browser restores after a hidden page; Tab navigation does', async () => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    await opened[0]!.go({ _tag: 'Refused', message: 'forbidden', status: 403, code: 'forbidden' })
    await vi.waitFor(() => expect(opened).toHaveLength(2))
    await live(opened[1]!, '$ ')
    await act(async () => field()!.focus())
    await act(async () => press('l', 'KeyL'))
    await vi.waitFor(() => expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))]))
    // Switching tabs blurs the window: the field blurs but stays the document's active element.
    await act(async () => {
      field()!.dispatchEvent(new FocusEvent('blur'))
      field()!.dispatchEvent(new FocusEvent('focusout', { bubbles: true }))
    })
    await leavePage('hidden')
    // A click reported while the page is hidden is not a gesture.
    await act(async () => field()!.dispatchEvent(new MouseEvent('mouseup', { bubbles: true })))
    await returnToPage()
    // On return the browser gives the same field focus back, with no relatedTarget.
    await act(async () => {
      field()!.dispatchEvent(new FocusEvent('focus'))
      field()!.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    })
    expect(document.activeElement).toBe(field())
    expect(chip()?.textContent).toBe('Click to type')
    await act(async () => press('x', 'KeyX'))
    await drain()
    expect(gateway.bound).toHaveLength(1)
    expect(gateway.writes).toEqual([rawInput(utf8.encode('l'))])
    expect(host.querySelector('[role="alert"]')?.textContent).toContain('Click the terminal to type again.')
    // Tab to the grid is navigation: it hands focus to the field and resumes typing.
    await act(async () => host.querySelector<HTMLElement>('[aria-roledescription="terminal"]')!.focus())
    expect(document.activeElement).toBe(field())
    expect(chip()?.textContent).toBe('Typing')
    await act(async () => press('y', 'KeyY'))
    await vi.waitFor(() => expect(gateway.writes).toEqual([rawInput(utf8.encode('l')), rawInput(utf8.encode('y'))]))
    expect(gateway.bound).toHaveLength(2)
    expect(opened.flatMap((session) => session.sent)).toEqual([])
  })

  it('types through the gateway on the projected screen when the source has no raw stream', async () => {
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalInput: gateway.factory, grants: inputWithoutControl })
    await vi.waitFor(() => expect(field()).not.toBeNull())
    expect(chip()?.getAttribute('data-terminal-mode')).toBe('Projected')
    expect(chip()?.textContent).toBe('Click to type')
    await act(async () => host.querySelector<HTMLElement>('[aria-roledescription="terminal"]')!.focus())
    expect(document.activeElement).toBe(field())
    await act(async () => press('Enter', 'Enter'))
    await vi.waitFor(() => expect(gateway.writes).toEqual([{ mode: 'key', value: 'enter' }]))
    expect(resizeButton()).toBeDefined()
  })

  it('is read-only on the PEEK stream for a limited pairing (no typing grant): no input field, and keys go nowhere', async () => {
    const { opened, port } = rawSessions()
    const gateway = gatewaySessions()
    await mount({ terminal: observed({ value: screen }), terminalRaw: port, terminalInput: gateway.factory, grants: watchOnly })
    await vi.waitFor(() => expect(opened).toHaveLength(1))
    expect(opened[0]!.mode).toBe('peek')
    await live(opened[0]!, '$ ')
    expect(chip()?.textContent).toBe('Read-only')
    expect(host.textContent).toContain('This device may watch this terminal but not type into it.')
    expect(field()).toBeNull()
    const grid = host.querySelector<HTMLElement>('[aria-roledescription="terminal"]')!
    await act(async () => {
      grid.focus()
      grid.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'l', code: 'KeyL' }))
    })
    expect(document.activeElement).toBe(grid)
    expect(opened[0]!.sent).toEqual([])
    expect(gateway.bound).toEqual([])
    expect(gateway.writes).toEqual([])
    expect(watching()).toBeGreaterThan(0)
    await relayout()
    expect(opened[0]!.resized).toEqual([])
  })
})
