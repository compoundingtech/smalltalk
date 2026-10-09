// @vitest-environment jsdom
import { St3Client } from '@smalltalk/st3-client'
import { Agent, decodeUnknownSync, type TerminalScreen } from '@smalltalk/st3-client/schema'
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
import recording from '../data/subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import { makeScreen } from './fixtures.ts'
import type { TerminalInputPortFactory } from './orderedTerminalInput.ts'
import { TerminalDetail } from './TerminalDetail.tsx'
import { gatewayTerminalInput } from './terminal-input-port.ts'
import { makeTerminalInputFixture } from './terminalInputFixture.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

const ref = 'terminal/example'
const screen: TerminalScreen = { ...makeScreen({ scene: 'F3', columns: 40, rows: 4, frame: 0 }), terminal_id: ref }
const agent = (reachability: 'reachable' | 'unreachable') => decodeUnknownSync(Agent)({ kind: 'agent', id: 'agent/example', name: 'Example Agent', host_id: 'host/example', runtime_ids: [], reachability, state: 'running', harness_state: 'idle', blocked_on: null, fault: null, revision: '1', updated_at: '2026-10-04T12:00:00.000Z' })
let registry: AtomRegistry.AtomRegistry
let root: Root
let host: HTMLDivElement
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
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
})
const isFeed = (terminal: Feed<TerminalScreen> | Readonly<Record<string, Feed<TerminalScreen>>>): terminal is Feed<TerminalScreen> =>
  typeof terminal._tag === 'string'
const mount = async ({ terminal, reachability = 'reachable', grants, terminalInput, at = ref }: {
  readonly terminal: Feed<TerminalScreen> | Readonly<Record<string, Feed<TerminalScreen>>>
  readonly reachability?: 'reachable' | 'unreachable'
  readonly grants?: Grants
  readonly terminalInput?: TerminalInputPortFactory
  readonly at?: `terminal/${string}`
}) => {
  const source = fixtureSource({ world: { now: 0, agents: [agent(reachability)], missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } }, overrides: { terminal: isFeed(terminal) ? { [ref]: terminal } : terminal, ...(grants === undefined ? {} : { grants }) } })
  await act(async () => root.render(<DataSourceProvider source={terminalInput === undefined ? source : { ...source, terminalInput }} registry={registry}><React.Suspense fallback={<p>suspended</p>}><TerminalDetail key={at} address={{ ref: at, presentation: 'detail' }} visibility="visible" /></React.Suspense></DataSourceProvider>))
}
const button = (name: string) => [...host.querySelectorAll('button')].find((candidate) => candidate.textContent === name)
const field = () => host.querySelector('textarea')!
const type = (char: string) => act(async () => {
  field().dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: char, code: `Key${char.toUpperCase()}` }))
})

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

  it('renders the live grid with input gated exactly as the source publishes it and history honestly unavailable', async () => {
    await mount({ terminal: observed({ value: screen }) })
    const grid = host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)
    expect(grid?.getAttribute('data-terminal-renderer')).toBe('T1')
    expect(grid?.querySelectorAll('[data-terminal-row]')).toHaveLength(screen.lines.length)
    expect(grid?.textContent).toContain(screen.lines.find((line) => line.text.trim() !== '')?.text.trim())
    expect(host.querySelector('[aria-label="Live"]')).not.toBeNull()
    expect(host.querySelector('textarea')?.disabled).toBe(true)
    expect(host.textContent).toContain('Ordered terminal input is not available from this producer')
    expect([...host.querySelectorAll('button')].map((button) => button.textContent)).not.toContain('Enable input')
    expect(host.textContent).toContain('does not publish terminal history yet')
  })

  it('marks the terminal ended and pauses input when its host is unreachable', async () => {
    await mount({ terminal: observed({ value: screen }), reachability: 'unreachable' })
    expect(host.querySelector('[aria-label="Ended"]')).not.toBeNull()
    expect(host.textContent).toContain('Input is paused while this terminal is hidden, disconnected or stale.')
  })

  it('keeps the last grid but says it is stale', async () => {
    await mount({ terminal: observed({ value: screen, freshness: 'stale' }) })
    expect(host.textContent).toContain('Terminal snapshot is stale.')
    expect(host.querySelector(`[aria-label="Terminal: ${screen.title}"]`)).not.toBeNull()
    expect(host.querySelector('[aria-label="Live"]')).toBeNull()
    expect(host.querySelector('[aria-label="Stale"]')).not.toBeNull()
  })
})

describe('terminal detail input', () => {
  beforeEach(() => {
    // jsdom fetches no module assets; serve the committed encoder the browser would load.
    vi.stubGlobal('fetch', async () => new Response(new Uint8Array(await readFile(join(import.meta.dirname, 'assets/ghostty-key-encoder.generated.wasm'))), { headers: { 'content-type': 'application/wasm' } }))
  })
  const sessions = () => {
    const opened: Array<{ readonly terminalRef: string; readonly fixture: ReturnType<typeof makeTerminalInputFixture> }> = []
    const factory: TerminalInputPortFactory = async ({ terminalRef, registry: owner }) => {
      const fixture = makeTerminalInputFixture({ registry: owner, autoDeliver: false })
      opened.push({ terminalRef, fixture })
      return fixture.port
    }
    return { opened, factory }
  }

  it('keeps the keyboard disabled with an honest reason when terminal.input is not granted', async () => {
    const { opened, factory } = sessions()
    await mount({ terminal: observed({ value: screen }), grants: { actions: 'granted', messageSend: 'granted', terminalInput: 'ungranted' }, terminalInput: factory })
    expect(field().disabled).toBe(true)
    expect(host.textContent).toContain('This device is not allowed to type into terminals.')
    expect(button('Enable input')).toBeUndefined()
    expect(opened).toEqual([])
  })

  it('types only into the shown terminal and drops a queue when the view switches terminals', async () => {
    const { opened, factory } = sessions()
    const other: `terminal/${string}` = 'terminal/other'
    const terminals = { [ref]: observed({ value: screen }), [other]: observed({ value: { ...screen, terminal_id: other, runtime_incarnation: 'incarnation/other' } }) }
    await mount({ terminal: terminals, terminalInput: factory })
    await vi.waitFor(() => expect(button('Enable input')).toBeDefined())
    await act(async () => button('Enable input')!.click())
    await type('a')
    await type('b')
    const first = opened[0]!
    expect(first.terminalRef).toBe(ref)
    expect(registry.get(first.fixture.writes)).toEqual([{ mode: 'raw', value: btoa('a') }])
    await mount({ terminal: terminals, terminalInput: factory, at: other })
    // Leaving the pane closes its session: the queued key is dropped, not carried over.
    expect(registry.get(first.fixture.port.state)._tag).toBe('Closed')
    await act(async () => first.fixture.answer({ _tag: 'Delivered' }))
    expect(registry.get(first.fixture.writes)).toHaveLength(1)
    await vi.waitFor(() => expect(button('Enable input')).toBeDefined())
    const second = opened.at(-1)!
    expect(second.terminalRef).toBe(other)
    expect(registry.get(second.fixture.writes)).toEqual([])
  })

  it.each(['switches terminals', 'loses the terminal input grant'] as const)(
    'posts nothing for a key whose snapshot read finishes after the view %s',
    async (cut) => {
      const other: `terminal/${string}` = 'terminal/other'
      const terminals = { [ref]: observed({ value: screen }), [other]: observed({ value: { ...screen, terminal_id: other, runtime_incarnation: 'incarnation/other' } }) }
      const posted: unknown[] = []
      const read = Promise.withResolvers<string | undefined>()
      const reads: Array<PromiseWithResolvers<string | undefined>> = [read]
      const transport: typeof fetch = async (input, init) => {
        if (new URL(String(input)).pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
        posted.push(init?.body)
        return Response.json({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/native' }, value: {} })
      }
      let granted = true
      const terminalInput = gatewayTerminalInput({
        connect: (fetchImpl) => new St3Client({ baseUrl: 'https://gateway.invalid', fetchImpl }),
        transport,
        snapshot: () => reads.shift()?.promise ?? Promise.resolve('snapshot/next'),
        liveScreen: (terminalRef) => {
          const feed = terminals[terminalRef]
          return feed?._tag === 'Observed' ? feed.value : undefined
        },
        granted: () => granted,
      })
      await mount({ terminal: terminals, terminalInput })
      await vi.waitFor(() => expect(button('Enable input')).toBeDefined())
      await act(async () => button('Enable input')!.click())
      await type('a')
      if (cut === 'switches terminals') await mount({ terminal: terminals, terminalInput, at: other })
      else {
        granted = false
        await mount({ terminal: terminals, terminalInput, grants: { actions: 'granted', messageSend: 'granted', terminalInput: 'ungranted' } })
      }
      await act(async () => read.resolve('snapshot/late'))
      expect(posted).toEqual([])
    },
  )
})
