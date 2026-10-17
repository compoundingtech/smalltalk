// @vitest-environment jsdom
// The folder tree is virtualized, so row styling can only be observed once the React Aria
// Virtualizer has measured a viewport. jsdom has no layout: ResizeObserver reports a fixed
// sidebar-sized rect so rows render. These assertions pin the row, search-highlight and notice
// styles — the keys were once dropped from stylex.create and the elements silently rendered
// unstyled, because stylex.props skips an undefined style.
import { useAtom } from '@effect/atom-react'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'

// Node tests stub only the CSS runtime (see ConversationPane tests). This stub keeps the one
// fact under test observable: each style a component applies through stylex.props becomes a
// `stylex:<key>` class, and a key missing from stylex.create applies nothing.
vi.mock('@stylexjs/stylex', async () => {
  type Applied = string | false | null | undefined | readonly Applied[]
  const classNames = (style: Applied): readonly string[] =>
    typeof style === 'string' ? [style] : Array.isArray(style) ? style.flatMap(classNames) : []
  return {
    ...await import('../testStylex.ts'),
    create: (styles: Readonly<Record<string, unknown>>) =>
      Object.fromEntries(
        Object.entries(styles).map(([key, style]) => [
          key,
          typeof style === 'function' ? () => `stylex:${key}` : `stylex:${key}`,
        ]),
      ),
    props: (...styles: readonly Applied[]) => ({ className: classNames(styles).join(' ') }),
  }
})
// Folder transport is not under test: live-mode trees read the fixture folder document. fixture.ts imports
// client.ts, so the factory must not import it (that waits on this factory); the export reads a hoisted holder.
const liveFolders = vi.hoisted(() => ({ atom: undefined as unknown }))
vi.mock('../folders/client.ts', async (importOriginal) => ({
  ...(await importOriginal<typeof FolderClient>()),
  get folders() {
    return liveFolders.atom
  },
}))

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider, useSubjectList } from '../data/react.tsx'
import type { DataSource } from '../data/source.ts'
import { fixtureProjections } from '../fixtures/projections.ts'
import type { ArrangementRefusal, SidebarOperation } from '../folders/edit.ts'
import type * as FolderClient from '../folders/client.ts'
import type { FolderState } from '../folders/client.ts'
import { fixtureFolders } from '../folders/fixture.ts'
import { SubjectAddress } from '../resources/contract.ts'
import { persistedAtom } from '../state/persistence.ts'
import { dailyDriver } from './fixtures/layouts.ts'
import { machineMemory } from './sidebar/show.ts'
import { defaultFilters, fixtureSidebarState, sidebarState, type SidebarFilters } from './sidebar/state.ts'
import { createWindow, projectWorkspaces, reduceWindow, type WindowAction } from './workspaces.ts'
import { AgentFolders, ArrangementEditFeedback } from './AgentFolders.tsx'
import { WorkbenchContextProvider } from './context.tsx'

liveFolders.atom = fixtureFolders

const viewport = { width: 340, height: 800 }
const viewportSize: ResizeObserverSize = { inlineSize: viewport.width, blockSize: viewport.height }

class StubResizeObserver implements ResizeObserver {
  private readonly callback: ResizeObserverCallback
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback
  }
  observe(target: Element): void {
    const entry: ResizeObserverEntry = {
      target,
      contentRect: DOMRect.fromRect(viewport),
      borderBoxSize: [viewportSize],
      contentBoxSize: [viewportSize],
      devicePixelContentBoxSize: [viewportSize],
    }
    this.callback([entry], this)
  }
  unobserve(): void {}
  disconnect(): void {}
}
globalThis.ResizeObserver = StubResizeObserver
Element.prototype.getBoundingClientRect = function (): DOMRect {
  return DOMRect.fromRect(viewport)
}

const folderOps = (refs: readonly string[]): readonly SidebarOperation[] => [
  { op: 'folder.create', id: 'product', name: 'Product', parent: null, key: 'V' },
  { op: 'subject.place', subject: refs[0]!, folder: 'product', key: 'W' },
]

const FolderTree = () => {
  const subjects = useSubjectList()
  const [windowAtom] = React.useState(() =>
    Atom.make(createWindow({ seed: dailyDriver, subjects, attention: [] })),
  )
  const [window, setWindow] = useAtom(windowAtom)
  const dispatch = (action: WindowAction) =>
    setWindow((current) => reduceWindow({ window: current, action, subjects, attention: [] }))
  const workspaces = projectWorkspaces({ window, subjects, attention: [] })
  return (
    <WorkbenchContextProvider
      value={{
        subjects: new Map(subjects.map((subject) => [subject.ref, subject])),
        focusedRef: null,
        platform: 'other',
        open: (request) =>
          dispatch({
            _tag: 'Layout',
            action: {
              _tag: 'Open',
              input: Schema.decodeSync(SubjectAddress)({
                ref: request.ref,
                presentation: request.presentation ?? 'detail',
              }),
            },
          }),
      }}
    >
      <AgentFolders
        workspaces={workspaces}
        selection={Atom.map(windowAtom, (current) => current.selected)}
        dispatch={dispatch}
      />
    </WorkbenchContextProvider>
  )
}

const mountTree = ({
  query = '',
  source = fixtureSource({ world: fixtureProjections }),
  filters = { ...defaultFilters, query },
}: {
  readonly query?: string
  readonly source?: DataSource
  readonly filters?: SidebarFilters
}) => {
  const registry = AtomRegistry.make()
  registry.get(fixtureFolders).edit(folderOps(fixtureProjections.agents.map((a) => a.id)))
  const state = (source.mode === 'fixtures' ? fixtureSidebarState : sidebarState)(source.gateway ?? source.mode)
  registry.set(state.filters, filters)
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  flushSync(() => {
    root.render(
      <DataSourceProvider source={source} registry={registry}>
        <FolderTree />
      </DataSourceProvider>,
    )
  })
  return {
    container,
    filters: () => registry.get(state.filters),
    unmount: () => {
      flushSync(() => root.unmount())
      container.remove()
    },
  }
}

describe('AgentFolders row styling', () => {
  it('applies the folder row, search highlight and notice styles', async () => {
    const tree = mountTree({ query: 'product' })
    try {
      await vi.waitFor(() =>
        expect(tree.container.querySelectorAll('[role="row"]').length).toBeGreaterThan(0),
      )
      const folderRow = tree.container.querySelector('[role="row"][aria-label="Product"]')
      expect(folderRow).not.toBeNull()
      expect(folderRow!.querySelector('[class~="stylex:folder"]')).not.toBeNull()
      const marks = [...tree.container.querySelectorAll('mark')]
      expect(marks.length).toBeGreaterThan(0)
      for (const mark of marks)
        expect(mark.classList.contains('stylex:highlight')).toBe(true)
      const notice = tree.container.querySelector('[role="status"]')
      expect(notice).not.toBeNull()
      expect(notice!.classList.contains('stylex:notice')).toBe(true)
    } finally {
      tree.unmount()
    }
  })

  it('applies the notice style to the empty filter result message', async () => {
    const tree = mountTree({ query: 'no-agent-or-folder-matches-this' })
    try {
      await vi.waitFor(() =>
        expect(
          tree.container.querySelector('p[class~="stylex:notice"]'),
        ).not.toBeNull(),
      )
      const empty = tree.container.querySelector('p[class~="stylex:notice"]')
      expect(empty?.textContent).toBe('No agents match these filters.')
      expect(tree.container.querySelector('[role="status"]')?.classList.contains('stylex:notice')).toBe(true)
    } finally {
      tree.unmount()
    }
  })
})

describe('AgentFolders remembered machines', () => {
  const agentRefs = (container: HTMLElement) =>
    [...container.querySelectorAll<HTMLElement>('[data-wf-agent-ref]')].map((row) => row.dataset.wfAgentRef!)
  // A new live gateway whose own settings differ from the defaults.
  const live = (gateway: string) => ({ ...fixtureSource({ world: fixtureProjections }), mode: 'live' as const, gateway })
  const settings: SidebarFilters = { ...defaultFilters, query: '', hideRetired: true, sort: 'name' }
  // The live workspace's write path: the shared browser persistence document.
  const remember = (machines: readonly (readonly string[])[] | null) => {
    const registry = AtomRegistry.make()
    registry.set(persistedAtom(machineMemory), machines)
    registry.dispose()
  }

  it('seeds a new gateway from the remembered machine and keeps its other settings', async () => {
    remember([['build-host-b']])
    const tree = mountTree({ source: live('restore.gateway.invalid'), filters: settings })
    try {
      expect(tree.filters()).toEqual({ ...settings, query: 'machine:build-host-b' })
      await vi.waitFor(() => expect(agentRefs(tree.container).length).toBeGreaterThan(0))
      expect(agentRefs(tree.container).every((ref) => ref.startsWith('agent/build-host-b/'))).toBe(true)
      const trigger = [...tree.container.querySelectorAll('button')].find((button) => button.textContent?.startsWith('Show'))
      expect(trigger?.textContent).toContain(', 1 active')
    } finally {
      tree.unmount()
      remember(null)
    }
  })

  it('restores nothing from corrupt memory another tab wrote', async () => {
    window.dispatchEvent(new StorageEvent('storage', {
      key: 'wf.ui@1',
      newValue: JSON.stringify({
        version: 1,
        entries: { [machineMemory.key]: { version: 2, value: [['BUILD HOST B']], clock: Date.now() + 60_000, writer: 'other-tab' } },
      }),
    }))
    const tree = mountTree({ source: live('corrupt.gateway.invalid'), filters: settings })
    try {
      expect(tree.filters()).toEqual(settings)
      await vi.waitFor(() => expect(agentRefs(tree.container).some((ref) => ref.startsWith('agent/build-host-a/'))).toBe(true))
    } finally {
      tree.unmount()
      remember(null)
    }
  })

  it('drops a gateway’s stale machine token when machines were cleared in this browser', async () => {
    remember([])
    const tree = mountTree({ source: live('cleared.gateway.invalid'), filters: { ...settings, query: 'is:asking machine:build-host-b' } })
    try {
      expect(tree.filters()).toEqual({ ...settings, query: 'is:asking' })
    } finally {
      tree.unmount()
      remember(null)
    }
  })
})

it('discloses Known and Unknown edit refusals only beside affected folder or agent rows', () => {
  const refusals: readonly ArrangementRefusal[] = [
    { reason: { _tag: 'Known', code: 'arrangement-cycle' }, detail: 'The folder cannot contain itself.', targets: ['product', 'agent/example'] },
    { reason: { _tag: 'Unknown' }, detail: 'No reason was returned.', targets: ['product'] },
  ]
  for (const refusal of refusals) {
    const state: FolderState = {
      doc: { folders: {}, placements: {} }, phase: 'fixture', refusal,
      edit: async () => ({ _tag: 'Success' }),
    }
    const registry = AtomRegistry.make({ initialValues: [[fixtureFolders, state]] })
    const source = fixtureSource({ world: fixtureProjections })
    const container = document.createElement('div')
    const root = createRoot(container)
    try {
      flushSync(() => root.render(
        <DataSourceProvider source={source} registry={registry}>
          <ArrangementEditFeedback target="product" />
          <ArrangementEditFeedback target="agent/example" />
          <ArrangementEditFeedback target="unaffected" />
        </DataSourceProvider>,
      ))
      expect(container.querySelectorAll('[role="status"]')).toHaveLength(refusal.targets.length)
      expect(container.textContent).toContain(refusal.reason._tag === 'Known'
        ? 'A folder cannot contain itself; choose a different parent folder.' : 'The change was not saved.')
      expect(container.textContent).not.toContain('Unknown')
      expect(container.textContent).not.toContain('arrangement-cycle')
      expect(container.textContent).not.toContain(refusal.detail)
      for (const status of container.querySelectorAll('[role="status"]')) {
        expect(status.getAttribute('data-wf-refusal-reason')).toBe(refusal.reason._tag)
        expect(status.getAttribute('data-wf-refusal-code')).toBe(refusal.reason.code ?? null)
      }
    } finally {
      flushSync(() => root.unmount())
      registry.dispose()
    }
  }
})
