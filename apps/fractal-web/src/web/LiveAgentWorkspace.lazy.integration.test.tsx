// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { flushSync } from 'react-dom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type * as PaneModule from './ConversationPane.tsx'
import type * as WorkspaceModule from './LiveAgentWorkspace.tsx'
import type * as FallbackModule from './ConversationPaneFallback.tsx'
import type { UxTelemetry } from '../telemetry/ux.ts'
import type { Feed, Fleet } from '../data/source.ts'
import type { FeedSyncObservation } from '../data/feedSync.ts'

vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
// Header actions read resources from a data source; they are outside this import boundary.
vi.mock('./ConversationHeaderActions.tsx', () => ({ ConversationHeaderActions: () => null }))
let fallbackRenders = 0
let rosterFeed: Feed<Fleet> = { _tag: 'Waiting' }
vi.mock('./ConversationPaneFallback.tsx', async importOriginal => {
  const original = await importOriginal<typeof FallbackModule>()
  return {
    ...original,
    ConversationPaneFallback: (props: React.ComponentProps<typeof original.ConversationPaneFallback>) => {
      fallbackRenders += 1
      return <original.ConversationPaneFallback {...props} />
    },
  }
})
// Keep the shell and retained-pane implementation real; the import is deliberately held
// pending, and no backend is needed to prove when its code is requested.
let gatewayObservation: FeedSyncObservation | undefined
const reconnect = vi.fn()
vi.mock('../data/react.tsx', () => ({
  useFleet: () => rosterFeed,
  useSubjectList: () => [],
  useConnection: () => ({ _tag: 'Live' }),
  useNow: () => 0,
  useGatewaySync: () => gatewayObservation,
  useDataSource: () => ({ gateway: 'alpha.example', reconnect }),
  useFeedInterest: () => {},
}))

let workspace: typeof WorkspaceModule
let root: Root
let container: HTMLDivElement
let resolveImport: () => void
let rejectImport: (reason: Error) => void
let mountCount: number
let frameId: number
const requested = vi.fn()
const frames = new Map<number, FrameRequestCallback>()
const paintTasks: (() => void)[] = []

beforeEach(async () => {
  vi.resetModules()
  requested.mockClear()
  gatewayObservation = { status: { _tag: 'Live', since: 0 }, observedAt: 0 }
  reconnect.mockClear()
  mountCount = 0
  fallbackRenders = 0
  rosterFeed = { _tag: 'Waiting' }
  frameId = 0
  frames.clear()
  paintTasks.length = 0
  window.localStorage.clear()
  window.history.replaceState(null, '', '/')
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class {
    observe() {}
    unobserve() {}
    disconnect() {}
  })
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    const id = ++frameId
    frames.set(id, callback)
    return id
  })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
  vi.stubGlobal('MessageChannel', class {
    private receive: () => void = () => {}
    private closed = false
    readonly port1 = {
      addEventListener: (_type: string, listener: () => void) => { this.receive = listener },
      start: () => {},
      close: () => { this.closed = true },
    }
    readonly port2 = {
      postMessage: () => paintTasks.push(() => { if (!this.closed) this.receive() }),
      close: () => {},
    }
  })
  const ready = new Promise<void>((resolve, reject) => { resolveImport = resolve; rejectImport = reject })
  vi.doMock('./ConversationPane.tsx', async () => {
    requested()
    await ready
    return {
      ConversationPane: ({ agentRef, agentName, visible }: React.ComponentProps<typeof PaneModule.ConversationPane>) => {
        const [mount] = React.useState(() => ++mountCount)
        return <div data-pane-ref={agentRef} data-mount={mount} aria-hidden={!visible}>{agentName}</div>
      },
    }
  })
  workspace = await import('./LiveAgentWorkspace.tsx')
  container = document.createElement('div')
  document.body.appendChild(container)
  root = createRoot(container)
})

afterEach(async () => {
  await act(async () => root.unmount())
  container.remove()
  vi.doUnmock('./ConversationPane.tsx')
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

const paintShellFrame = async () => {
  await act(async () => {
    const pending = [...frames.values()]
    frames.clear()
    for (const callback of pending) callback(0)
  })
}

const renderShell = async (ux?: UxTelemetry) => {
  // resetModules gives this test a fresh module-scope lazy component and atom context.
  const { RegistryContext } = await import('@effect/atom-react')
  await act(async () => root.render(<RegistryContext.Provider value={AtomRegistry.make()}>
    <workspace.LiveAgentWorkspace ux={ux} />
  </RegistryContext.Provider>))
}


describe('gateway footer honesty', () => {
  it('uses the sync wording rather than a raw Live tag', async () => {
    await renderShell()
    const footer = container.querySelector('footer')
    expect(footer?.textContent).toContain('alpha.example')
    expect(footer?.textContent).not.toContain('Live')
  })

  it('places one explicit reconnect action next to the countdown', async () => {
    gatewayObservation = {
      status: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: 3000, issue: 'Private diagnostic' } },
      observedAt: -2000,
    }
    await renderShell()
    const footer = container.querySelector('footer')
    expect(footer?.textContent).toContain('Reconnecting in 3s · attempt 2')
    expect(footer?.textContent).not.toContain('Private diagnostic')
    const button = [...(footer?.querySelectorAll('button') ?? [])].find(button => button.textContent === 'Reconnect now')
    expect(button).toBeDefined()
    await act(async () => button?.click())
    expect(reconnect).toHaveBeenCalledTimes(1)
  })
})
describe('conversation import boundary', () => {
  it('reserves non-interactive roster rows while waiting and keeps the honest wait notice', async () => {
    await renderShell()
    const roster = container.querySelector('nav[aria-label="Agent roster"]')
    const skeleton = roster?.querySelector('[data-wf-roster-skeleton]')
    expect(skeleton).not.toBeNull()
    expect(skeleton?.getAttribute('aria-hidden')).toBe('true')
    expect(skeleton?.querySelectorAll('[data-wf-skeleton-row]')).toHaveLength(8)
    expect(skeleton?.querySelectorAll('[data-wf-skeleton-line]')).toHaveLength(16)
    expect(skeleton?.querySelectorAll('button, a, input, [tabindex]')).toHaveLength(0)
    expect(container.textContent).toContain('Waiting for the agent roster.')
  })

  it('removes skeletons for observed empty rosters and read refusals rather than inventing rows', async () => {
    rosterFeed = { _tag: 'Observed', freshness: 'live', value: { agents: [], hosts: [] } }
    await renderShell()
    expect(container.querySelector('[data-wf-roster-skeleton]')).toBeNull()
    expect(container.textContent).not.toContain('Waiting for the agent roster.')
    rosterFeed = { _tag: 'Unavailable', reason: 'ungranted', detail: 'not-for-display' }
    await renderShell()
    expect(container.querySelector('[data-wf-roster-skeleton]')).toBeNull()
    expect(container.textContent).toContain('Read access to the agent roster has not been granted.')
    expect(container.textContent).not.toContain('not-for-display')
  })

  it('discloses partial published roster coverage without disguising it as an empty roster', async () => {
    rosterFeed = { _tag: 'Observed', freshness: 'stale', coverage: { _tag: 'Partial' }, value: { agents: [], hosts: [] } }
    await renderShell()
    expect(container.textContent).toContain('Showing a partial roster · live updates pending')
    expect(container.querySelector('[data-wf-roster-skeleton]')).toBeNull()
    rosterFeed = { _tag: 'Observed', freshness: 'live', value: { agents: [], hosts: [] } }
    await renderShell()
    expect(container.textContent).not.toContain('Showing a partial roster')
  })

  it('does not evaluate pane code when the shell module is imported', () => {
    expect(requested).not.toHaveBeenCalled()
  })

  it('requests an initially selected thread immediately and suspends only its retained pane', async () => {
    const show = (current: string) => <workspace.WorkspaceBody current={current} rosterRefs={['agent/a', 'agent/b']} view={{ _tag: 'Thread' }} agentName={current} onOpenTool={() => {}} />
    await act(async () => root.render(show('agent/a')))
    // No frame or post-paint task has run: a selected thread cannot wait a second frame.
    expect(requested).toHaveBeenCalledTimes(1)
    expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
    expect(container.querySelector('[data-testid="transcript-header"]')?.textContent).toBe('agent/a')
    expect(container.querySelector('[data-pane-ref]')).toBeNull()

    await act(async () => root.render(show('agent/b')))
    const placeholders = [...container.querySelectorAll('[data-testid="transcript-placeholder"]')]
    expect(placeholders).toHaveLength(2)
    expect(placeholders[0]?.closest('[aria-hidden]')?.getAttribute('aria-hidden')).toBe('true')
    expect(placeholders[1]?.closest('[aria-hidden]')?.getAttribute('aria-hidden')).toBe('false')
    expect(requested).toHaveBeenCalledTimes(1)

    await act(async () => resolveImport())
    const first = container.querySelector('[data-pane-ref="agent/a"]')
    expect(first).not.toBeNull()
    expect(container.querySelector('[data-testid="transcript-placeholder"]')).toBeNull()
    await act(async () => root.render(show('agent/a')))
    expect(container.querySelector('[data-pane-ref="agent/a"]')).toBe(first)
    expect(first?.getAttribute('aria-hidden')).toBe('false')
    expect(mountCount).toBe(2)
    expect(requested).toHaveBeenCalledTimes(1)
  })

  it('does not suspend a switch on already loaded code after the selected shell prefetch', async () => {
    window.history.replaceState(null, '', '/w/agent/a')
    await renderShell()
    await act(async () => resolveImport())
    expect(container.querySelector('[data-pane-ref="agent/a"]')).not.toBeNull()
    await paintShellFrame()
    await act(async () => paintTasks.shift()?.())
    const before = fallbackRenders
    await act(async () => {
      flushSync(() => {
        window.history.pushState(null, '', '/w/agent/b')
        window.dispatchEvent(new PopStateEvent('popstate'))
      })
    })
    expect(requested).toHaveBeenCalledTimes(1)
    expect(fallbackRenders - before).toBe(0)
    expect(container.querySelector('[data-pane-ref="agent/b"]')).not.toBeNull()
  })

  it('prefetches the empty shell after its first paint, then reuses that import for selection', async () => {
    await renderShell()
    expect(container.querySelector('[data-testid="live-agent-workspace"]')).not.toBeNull()
    expect(requested).not.toHaveBeenCalled()
    await paintShellFrame()
    // rAF precedes paint; module work must wait for the posted post-paint task.
    expect(requested).not.toHaveBeenCalled()
    expect(paintTasks).toHaveLength(1)
    await act(async () => paintTasks.shift()?.())
    expect(requested).toHaveBeenCalledTimes(1)

    await act(async () => {
      window.history.pushState(null, '', '/w/agent/a')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })
    expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
    expect(requested).toHaveBeenCalledTimes(1)
    await act(async () => resolveImport())
    expect(container.querySelector('[data-pane-ref="agent/a"]')).not.toBeNull()
    expect(requested).toHaveBeenCalledTimes(1)
  })

  it('shows a failed prefetch honestly, then retries with a second request and a fresh lazy pane', async () => {
    await renderShell()
    await paintShellFrame()
    await act(async () => paintTasks.shift()?.())
    expect(requested).toHaveBeenCalledTimes(1)
    await act(async () => rejectImport(new Error('Synthetic chunk request failed')))
    await act(async () => {
      window.history.pushState(null, '', '/w/agent/a')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })
    expect(container.querySelector('[role="alert"]')?.textContent).toContain('This conversation view could not load.')
    expect(container.querySelector('[data-pane-ref]')).toBeNull()
    expect(requested).toHaveBeenCalledTimes(1)

    // Replace only the failed module responder, modeling a recovered chunk transport.
    // Keep the real workspace's cached promise and lazy wrappers intact.
    const recovered = new Promise<void>(resolve => { resolveImport = resolve })
    // Vitest caches failed module evaluation as well as the loader's promise. Clearing its
    // module registry leaves the already-imported workspace and React.lazy instances alive.
    vi.resetModules()
    vi.doMock('./ConversationPane.tsx', async () => {
      requested()
      await recovered
      return {
        ConversationPane: ({ agentRef, agentName }: React.ComponentProps<typeof PaneModule.ConversationPane>) =>
          <div data-pane-ref={agentRef}>{agentName}</div>,
      }
    })
    const retry = [...container.querySelectorAll('button')].find(button => button.textContent === 'Try again')
    expect(retry).toBeDefined()
    await act(async () => retry!.click())
    await act(async () => { await vi.waitFor(() => expect(requested).toHaveBeenCalledTimes(2)) })
    expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
    await act(async () => resolveImport())
    expect(container.querySelector('[data-pane-ref="agent/a"]')).not.toBeNull()
    expect(container.querySelector('[role="alert"]')).toBeNull()

    await act(async () => {
      window.history.pushState(null, '', '/w/agent/b')
      window.dispatchEvent(new PopStateEvent('popstate'))
    })
    expect(container.querySelector('[data-pane-ref="agent/b"]')).not.toBeNull()
    expect(container.querySelector('[role="alert"]')).toBeNull()
    expect(requested).toHaveBeenCalledTimes(2)
  })

  it('cancels unmounted shell prefetch without losing the telemetry ref cleanup', async () => {
    const capture = vi.fn()
    const ux: UxTelemetry = {
      activeSpan: () => undefined,
      traceContext: () => undefined,
      shellCommitted: () => capture,
      rosterCommitted: () => () => {},
      beginSwitch: () => {},
      switchDataReady: () => {},
      transcriptCommitted: () => () => {},
      beginSendEcho: () => ({ committed: () => () => {}, cancel: () => {} }),
      observeSync: () => {},
      dispose: () => {},
    }
    await renderShell(ux)
    await paintShellFrame()
    await act(async () => root.render(null))
    await act(async () => { for (const task of paintTasks.splice(0)) task() })
    expect(capture).toHaveBeenCalledTimes(1)
    expect(requested).not.toHaveBeenCalled()
  })

  it('handles speculative import rejection without an unhandled rejection', async () => {
    const unhandled = vi.fn()
    window.addEventListener('unhandledrejection', unhandled)
    try {
      await renderShell()
      await paintShellFrame()
      await act(async () => paintTasks.shift()?.())
      await act(async () => rejectImport(new Error('Chunk unavailable')))
      expect(requested).toHaveBeenCalledTimes(1)
      expect(unhandled).not.toHaveBeenCalled()
      expect(container.querySelector('[data-testid="live-agent-workspace"]')).not.toBeNull()
    } finally {
      window.removeEventListener('unhandledrejection', unhandled)
    }
  })
})
