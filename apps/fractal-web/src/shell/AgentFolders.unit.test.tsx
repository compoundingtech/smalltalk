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
vi.mock('@stylexjs/stylex', () => {
  type Applied = string | false | null | undefined | readonly Applied[]
  const classNames = (style: Applied): readonly string[] =>
    typeof style === 'string' ? [style] : Array.isArray(style) ? style.flatMap(classNames) : []
  return {
    create: (styles: Readonly<Record<string, unknown>>) =>
      Object.fromEntries(
        Object.entries(styles).map(([key, style]) => [
          key,
          typeof style === 'function' ? () => `stylex:${key}` : `stylex:${key}`,
        ]),
      ),
    defineVars: (variables: unknown) => variables,
    createTheme: () => ({}),
    keyframes: () => 'test-animation',
    props: (...styles: readonly Applied[]) => ({ className: classNames(styles).join(' ') }),
  }
})

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider, useSubjectList } from '../data/react.tsx'
import { fixtureProjections } from '../fixtures/projections.ts'
import { createFolder, place, type FolderOp } from '../folders/core.mts'
import { fixtureFolders } from '../folders/fixture.ts'
import { SubjectAddress } from '../resources/contract.ts'
import { dailyDriver } from './fixtures/layouts.ts'
import { defaultFilters, fixtureSidebarState } from './sidebar/state.ts'
import { createWindow, projectWorkspaces, reduceWindow, type WindowAction } from './workspaces.ts'
import { AgentFolders } from './AgentFolders.tsx'
import { WorkbenchContextProvider } from './context.tsx'

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

const folderOps = (refs: readonly string[]): readonly FolderOp[] => [
  createFolder('product', 'Product', null, 'V', [1, 0, 'fixture']),
  place(refs[0]!, 'product', 'W', [10, 0, 'fixture']),
]

const FolderTree = ({ query }: { readonly query: string }) => {
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

const mountTree = ({ query }: { readonly query: string }) => {
  const registry = AtomRegistry.make()
  const source = fixtureSource({ world: fixtureProjections })
  registry.get(fixtureFolders).edit(folderOps(fixtureProjections.agents.map((a) => a.id)))
  registry.set(fixtureSidebarState(source.gateway ?? source.mode).filters, {
    ...defaultFilters,
    query,
  })
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  flushSync(() => {
    root.render(
      <DataSourceProvider source={source} registry={registry}>
        <FolderTree query={query} />
      </DataSourceProvider>,
    )
  })
  return {
    container,
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
