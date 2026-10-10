// @vitest-environment jsdom
import type * as FoldersClient from '../folders/client.ts'
import type { FolderState } from '../folders/client.ts'
import type { DataSource } from '../data/source.ts'
import { ActionRefused } from '@st3/sdk/effect'
import { Effect } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'

// Inject only the live folder atom boundary; the editor, Tree and refusal rendering stay real.
const { editFolders } = await vi.hoisted(async () => {
  // Static imports run after Vitest's hoisted mock; initialize its atom within this loading boundary.
  const Atom = await import('effect/reactivity/Atom')
  return { editFolders: Atom.make<FolderState>({ doc: { folders: {}, placements: {} }, phase: 'connecting', edit: () => Promise.reject(new Error('The test edit port has not been bound.')) }) }
})
vi.mock('../folders/client.ts', async (importOriginal) => ({ ...await importOriginal<typeof FoldersClient>(), folders: editFolders }))
vi.mock('@stylexjs/stylex', () => {
  type Applied = string | false | null | undefined | readonly Applied[]
  const classes = (value: Applied): readonly string[] => typeof value === 'string' ? [value] : Array.isArray(value) ? value.flatMap(classes) : []
  return {
    create: (styles: Readonly<Record<string, unknown>>) => Object.fromEntries(Object.entries(styles).map(([key, value]) => [key, typeof value === 'function' ? () => `stylex:${key}` : `stylex:${key}`])),
    defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'test-animation',
    props: (...styles: readonly Applied[]) => ({ className: classes(styles).join(' ') }),
  }
})

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureProjections } from '../fixtures/projections.ts'
import { arrangementSidebarDoc } from '../folders/client.ts'
import { createArrangementEditor, refusalText } from '../folders/edit.ts'
import { AgentFolders, ArrangementEditFeedback } from './AgentFolders.tsx'
import { defaultFilters, sidebarState } from './sidebar/state.ts'

const viewport = { width: 340, height: 800 }
const size: ResizeObserverSize = { inlineSize: viewport.width, blockSize: viewport.height }
class SidebarResizeObserver implements ResizeObserver {
  constructor(private readonly callback: ResizeObserverCallback) {}
  observe(target: Element): void { this.callback([{ target, contentRect: DOMRect.fromRect(viewport), borderBoxSize: [size], contentBoxSize: [size], devicePixelContentBoxSize: [size] }], this) }
  unobserve(): void {}
  disconnect(): void {}
}
globalThis.ResizeObserver = SidebarResizeObserver
Element.prototype.getBoundingClientRect = function (): DOMRect { return DOMRect.fromRect(viewport) }

it.each([
  ['', 'arrangement-limit', 'Known', 'The folder layout has reached its limit; reduce the layout before trying again.'],
  ['hidden', 'arrangement-limit', 'Known', 'The folder layout has reached its limit; reduce the layout before trying again.'],
  ['', 'future-code', 'Unknown', 'The change was not saved.'],
  ['hidden', 'future-code', 'Unknown', 'The change was not saved.'],
])('retains a refused Inbox target with plain copy outside query %j (%s)', async (query, code, reason, sentence) => {
  const folder = '00000000-0000-7000-8000-000000000002'
  const registry = AtomRegistry.make()
  const source: DataSource = { ...fixtureSource({ world: fixtureProjections }), mode: 'live' }
  registry.set(sidebarState(source.gateway ?? source.mode).filters, defaultFilters)
  const gate = Promise.withResolvers<void>()
  let submitted = false
  const serverDetail = `Unknown ${code}: opaque server detail`
  const failure = new ActionRefused({ status: 409, message: serverDetail, response: { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/example', code, message: serverDetail, retryable: false, details: {} } })
  const editor = createArrangementEditor({
    owner: 'person/example',
    read: async () => ({ owner: 'person/example', items: [] }),
    actions: {
      snapshot: Effect.succeed('snapshot/example/1'),
      submitAction: () => Effect.sync(() => { submitted = true }).pipe(Effect.flatMap(() => Effect.promise(() => gate.promise)), Effect.flatMap(() => Effect.fail(failure))),
    },
    onState: (state) => registry.set(editFolders, {
      doc: state.arrangement === undefined ? { folders: {}, placements: {} } : arrangementSidebarDoc(state.arrangement),
      phase: state.phase === 'refused' ? 'unavailable' : state.phase, readOnly: false,
      edit: editor.edit, retryEdit: editor.retryEdit, retryReady: state.retryReady,
      ...(state.refusal === undefined ? {} : { refusal: state.refusal, detail: refusalText(state.refusal) }),
    }),
  })
  editor.accept({ owner: 'person/example', items: [] })
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    flushSync(() => root.render(<DataSourceProvider source={source} registry={registry}><AgentFolders workspaces={[]} selection={Atom.make('')} dispatch={() => undefined} /><ArrangementEditFeedback target={folder} /></DataSourceProvider>))
    let pending: Promise<unknown> | undefined
    flushSync(() => { pending = registry.get(editFolders).edit([{ op: 'folder.create', id: folder, name: 'Inbox', parent: null, key: 'a0' }]) })
    await vi.waitFor(() => expect(container.textContent).toContain('Inbox'))
    await vi.waitFor(() => expect(submitted).toBe(true))
    if (query !== '') flushSync(() => registry.set(sidebarState(source.gateway ?? source.mode).filters, { ...defaultFilters, query }))
    await React.act(async () => { gate.resolve(); await pending })
    expect(registry.get(editFolders).doc.folders).toEqual({})
    const target = container.querySelector(`[data-wf-refused-folder="${folder}"]`)
    expect(target).not.toBeNull()
    expect(target?.textContent).toContain('Inbox')
    expect(target?.textContent).toContain(sentence)
    const feedback = container.querySelectorAll('[data-wf-refusal-code]')
    expect(feedback).toHaveLength(2)
    for (const element of feedback) {
      expect(element.getAttribute('data-wf-refusal-reason')).toBe(reason)
      expect(element.getAttribute('data-wf-refusal-code')).toBe(code)
      expect(element.textContent).toContain(sentence)
      expect(element.textContent).not.toContain('Unknown')
      expect(element.textContent).not.toContain(code)
      expect(element.textContent).not.toContain(serverDetail)
    }
    expect(target?.querySelector('button')?.textContent).toContain('Retry')
  } finally {
    editor.close()
    flushSync(() => root.unmount())
    registry.dispose()
    container.remove()
  }
})

it.each([false, true])('names every legacy Sidebar and its deterministic winner without blocking the tree (readOnly=%s)', (readOnly) => {
  const registry = AtomRegistry.make()
  const source: DataSource = { ...fixtureSource({ world: fixtureProjections }), mode: 'live' }
  registry.set(sidebarState(source.gateway ?? source.mode).filters, { ...defaultFilters, query: 'hidden' })
  const earlier = 'arrangement/person/example/00000000-0000-7000-8000-000000000003'
  const later = 'arrangement/person/example/00000000-0000-7000-8000-000000000009'
  registry.set(editFolders, {
    doc: { folders: {}, placements: {} }, phase: 'synced', readOnly,
    sidebarSubject: earlier,
    sidebarCandidates: [{ id: earlier, label: `Sidebar (${earlier})` }, { id: later, label: `Sidebar (${later})` }],
    edit: async () => ({ _tag: 'Success' }),
  })
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    flushSync(() => root.render(<DataSourceProvider source={source} registry={registry}><AgentFolders workspaces={[]} selection={Atom.make('')} dispatch={() => undefined} /></DataSourceProvider>))
    const note = container.querySelector('[role="note"][aria-label="Multiple legacy Sidebars"]')
    expect(note).not.toBeNull()
    expect(note?.textContent).toContain(`Sidebar (${earlier})`)
    expect(note?.textContent).toContain(`Sidebar (${later})`)
    const labels = note?.querySelectorAll('li')
    expect(labels).toHaveLength(2)
    expect(labels?.[0]?.textContent).toContain('Currently used')
    expect(labels?.[1]?.textContent).not.toContain('Currently used')
    expect(note?.querySelector('button')).toBeNull()
    expect(container.querySelector('[aria-label="Agents and folders"]')).not.toBeNull()
    expect(container.querySelector('[role="dialog"]')).toBeNull()
    const reserved = 'arrangement/person/example/00000000-0000-7000-8000-000000000001'
    flushSync(() => registry.set(editFolders, { ...registry.get(editFolders), sidebarSubject: reserved }))
    expect(note?.textContent).toContain(`Currently used Sidebar: ${reserved}`)
    expect(note?.querySelectorAll('li')[0]?.textContent).not.toContain('Currently used')
    expect(note?.querySelectorAll('li')[1]?.textContent).not.toContain('Currently used')
    flushSync(() => registry.set(editFolders, { ...registry.get(editFolders), sidebarCandidates: [{ id: earlier, label: `Sidebar (${earlier})` }] }))
    expect(container.querySelector('[role="note"]')).toBeNull()
  } finally {
    flushSync(() => root.unmount())
    registry.dispose()
    container.remove()
  }
})

it('shows an honest removed Sidebar empty state with disabled Restore and edit retry', () => {
  const registry = AtomRegistry.make()
  const source: DataSource = { ...fixtureSource({ world: fixtureProjections }), mode: 'live' }
  registry.set(sidebarState(source.gateway ?? source.mode).filters, defaultFilters)
  let mutations = 0
  registry.set(editFolders, {
    doc: { folders: {}, placements: {} }, phase: 'unavailable', readOnly: true,
    restoreUnavailable: true,
    sidebarSubject: 'arrangement/person/example/00000000-0000-7000-8000-000000000001',
    retryReady: true,
    edit: async () => { mutations++; return { _tag: 'Success' } },
    retryEdit: async () => { mutations++; return { _tag: 'Success' } },
  })
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    flushSync(() => root.render(<DataSourceProvider source={source} registry={registry}><AgentFolders workspaces={[]} selection={Atom.make('')} dispatch={() => undefined} /></DataSourceProvider>))
    const removed = container.querySelector('[role="status"][aria-label="Removed Sidebar"]')
    expect(removed?.textContent).toContain('Sidebar removed.')
    expect(removed?.textContent).toContain('Restoring a removed Sidebar is not available yet.')
    const restore = removed?.querySelector('button')
    expect(restore?.textContent).toBe('Restore')
    expect(restore?.disabled).toBe(true)
    restore?.click()
    const retry = [...container.querySelectorAll('button')].find((button) => button.textContent === 'Retry edit')
    expect(retry?.disabled).toBe(true)
    retry?.click()
    expect(mutations).toBe(0)
    expect(container.querySelector('[aria-label="Agents and folders"]')).not.toBeNull()
    expect(container.textContent).not.toContain('Inbox')
  } finally {
    flushSync(() => root.unmount())
    registry.dispose()
    container.remove()
  }
})
