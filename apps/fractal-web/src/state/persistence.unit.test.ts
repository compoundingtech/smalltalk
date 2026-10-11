import { Schema } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it } from 'vitest'

import { SubjectAddress } from '../resources/contract.ts'
import {
  defaultResourcePanel,
  initialState,
  monitorDetailBounds,
  parseSubjectUrl,
  reduceLayout,
  subjectUrl,
} from '../shell/state.ts'
import { createPersistence, type StorageAdapter } from './persistence.ts'
import { WindowStateSchema, windowStateAtom } from './window.ts'

/** In-memory storage with the same other-tab-only notification boundary as browser storage. */
const storage = () => {
  let document: string | null = null
  const subscribers = new Map<object, (value: string | null) => void>()
  return {
    tab(): StorageAdapter {
      const id = {}
      return {
        read: () => document,
        write: (value) => {
          document = value
          subscribers.forEach((notify, owner) => {
            if (owner !== id) notify(value)
          })
        },
        subscribe: (notify) => {
          subscribers.set(id, notify)
          return () => {
            subscribers.delete(id)
          }
        },
      }
    },
  }
}

describe('persisted working context', () => {
  it('isolates window navigation, inherits last-used layout, and shares read markers across reloads', () => {
    const backend = storage()
    const first = createPersistence({ adapter: backend.tab() })
    const second = createPersistence({ adapter: backend.tab() })
    const a = AtomRegistry.make()
    const b = AtomRegistry.make()
    const docks = {
      primary: { visible: true, size: 312, activeView: 'agents' },
      panel: { visible: false, size: 180, activeView: 'events' },
    }
    const seed = {
      selected: 'agent/seed',
      layouts: {},
      docks,
      resources: defaultResourcePanel,
      monitorDetailSize: monitorDetailBounds.default,
      read: new Set<string>(),
    }
    const one = windowStateAtom({ gateway: 'test', seed, windowId: 'one', persist: first.atom })
    a.set(one, { ...a.get(one), selected: 'agent/a' })
    first.flush()
    const two = windowStateAtom({ gateway: 'test', seed, windowId: 'two', persist: second.atom })
    expect(b.get(two).selected).toBe('agent/a')
    b.set(two, { ...b.get(two), selected: 'agent/b', read: new Set(['card/seen']) })
    second.flush()
    expect(a.get(one).selected).toBe('agent/a')
    expect(a.get(one).read.has('card/seen')).toBe(true)
    const reloaded = createPersistence({ adapter: backend.tab() })
    const c = AtomRegistry.make()
    try {
      expect(
        c.get(windowStateAtom({ gateway: 'test', seed, windowId: 'one', persist: reloaded.atom }))
          .selected,
      ).toBe('agent/a')
      expect(
        c.get(windowStateAtom({ gateway: 'test', seed, windowId: 'new', persist: reloaded.atom }))
          .selected,
      ).toBe('agent/b')
    } finally {
      first.dispose()
      second.dispose()
      reloaded.dispose()
      a.dispose()
      b.dispose()
      c.dispose()
    }
  })
  it('restores every working-state category through a new registry and storage reader', () => {
    const backend = storage()
    const first = createPersistence({ adapter: backend.tab() })
    const registry = AtomRegistry.make()
    const Context = Schema.Struct({
      selected: Schema.String,
      panes: Schema.Array(
        Schema.Struct({
          tabs: Schema.Array(Schema.String),
          active: Schema.String,
          size: Schema.Finite,
        }),
      ),
      draft: Schema.String,
      filters: Schema.Struct({
        query: Schema.String,
        needsMe: Schema.Boolean,
        collapsed: Schema.Array(Schema.String),
      }),
      anchor: Schema.Struct({ entry: Schema.String, offset: Schema.Finite }),
    })
    const saved = {
      selected: 'agent/compiler',
      panes: [{ tabs: ['chat', 'terminal'], active: 'terminal', size: 0.6 }],
      draft: 'Please retain this unfinished instruction',
      filters: { query: 'compiler', needsMe: true, collapsed: ['build-a/reviews'] },
      anchor: { entry: 'tool/read-42', offset: 18 },
    }
    const context = first.atom({ key: 'gateway:context', schema: Context, defaultValue: saved })
    registry.set(context, { ...saved, draft: 'Unsent edited draft' })
    first.dispose()
    registry.dispose()
    const reloaded = createPersistence({ adapter: backend.tab() })
    const restored = AtomRegistry.make()
    try {
      const value = restored.get(
        reloaded.atom({
          key: 'gateway:context',
          schema: Context,
          defaultValue: { ...saved, selected: 'agent/default', draft: '' },
        }),
      )
      expect(value).toEqual({ ...saved, draft: 'Unsent edited draft' })
    } finally {
      reloaded.dispose()
      restored.dispose()
    }
  })

  it('migrates an old draft schema once and persists the new encoded shape', () => {
    const backend = storage()
    const legacy = createPersistence({ adapter: backend.tab() })
    const registry = AtomRegistry.make()
    registry.set(
      legacy.atom({ key: 'draft', schema: Schema.String, defaultValue: '' }),
      'Keep my older draft',
    )
    legacy.dispose()
    const current = createPersistence({ adapter: backend.tab() })
    const Draft = Schema.Struct({ text: Schema.String, attachments: Schema.Array(Schema.String) })
    const upgraded = current.atom({
      key: 'draft',
      version: 2,
      schema: Draft,
      defaultValue: { text: '', attachments: [] },
      migrate: (value, version) => (version === 1 ? { text: value, attachments: [] } : value),
    })
    try {
      expect(registry.get(upgraded)).toEqual({ text: 'Keep my older draft', attachments: [] })
      current.flush()
      const stored = JSON.parse(backend.tab().read()!)
      expect(stored.entries.draft.version).toBe(2)
      expect(stored.entries.draft.value).toEqual({ text: 'Keep my older draft', attachments: [] })
    } finally {
      current.dispose()
      registry.dispose()
    }
  })

  it('converges cross-tab edits without dropping another tab’s pending unrelated state', () => {
    const backend = storage()
    const first = createPersistence({ adapter: backend.tab() })
    const second = createPersistence({ adapter: backend.tab() })
    const a = AtomRegistry.make()
    const b = AtomRegistry.make()
    const draftA = first.atom({ key: 'draft', schema: Schema.String, defaultValue: '' })
    const draftB = second.atom({ key: 'draft', schema: Schema.String, defaultValue: '' })
    const filterA = first.atom({ key: 'filter', schema: Schema.String, defaultValue: '' })
    const filterB = second.atom({ key: 'filter', schema: Schema.String, defaultValue: '' })
    try {
      a.get(draftA)
      b.get(draftB)
      a.get(filterA)
      b.get(filterB)
      a.set(draftA, 'First edit')
      first.flush()
      expect(b.get(draftB)).toBe('First edit')
      a.set(filterA, 'needs-me')
      b.set(draftB, 'Second edit')
      second.flush()
      first.flush()
      expect(a.get(draftA)).toBe('Second edit')
      expect(b.get(draftB)).toBe('Second edit')
      expect(a.get(filterA)).toBe('needs-me')
      expect(b.get(filterB)).toBe('needs-me')
      const stored = JSON.parse(backend.tab().read()!)
      expect(stored.entries.draft.value).toBe('Second edit')
      expect(stored.entries.filter.value).toBe('needs-me')
    } finally {
      first.dispose()
      second.dispose()
      a.dispose()
      b.dispose()
    }
  })

  it('preserves an unreadable newer document instead of overwriting other saved work', () => {
    const backend = storage()
    const adapter = backend.tab()
    const newer = JSON.stringify({ version: 2, entries: { important: 'future-format draft' } })
    adapter.write(newer)
    const current = createPersistence({ adapter })
    const registry = AtomRegistry.make()
    try {
      const draft = current.atom({ key: 'draft', schema: Schema.String, defaultValue: '' })
      registry.set(draft, 'Still editable in this tab')
      current.flush()
      expect(adapter.read()).toBe(newer)
      expect(registry.get(draft)).toBe('Still editable in this tab')
      expect(current.error).toBeDefined()
    } finally {
      current.dispose()
      registry.dispose()
    }
  })

  it('restores the real split layout codec and set-valued inbox read markers', () => {
    const backend = storage()
    const first = createPersistence({ adapter: backend.tab() })
    const registry = AtomRegistry.make()
    const seed = initialState({
      primary: { visible: true, size: 312, activeView: 'agents' },
      panel: { visible: false, size: 180, activeView: 'events' },
    })
    const opened = reduceLayout({
      state: seed,
      action: { _tag: 'Open', input: { ref: 'agent/compiler', presentation: 'detail' } },
    })
    const split = reduceLayout({
      state: opened,
      action: {
        _tag: 'Open',
        input: { ref: 'terminal/compiler', presentation: 'detail' },
        side: true,
      },
    })
    const { docks, ...layout } = split
    const saved = {
      selected: 'agent/compiler',
      layouts: { 'agent/compiler': layout },
      docks,
      resources: { expanded: true, size: 444 },
      monitorDetailSize: 512,
      read: new Set(['attention/question-1']),
    }
    registry.set(
      first.atom({ key: 'window', schema: WindowStateSchema, defaultValue: saved }),
      saved,
    )
    first.dispose()
    const reloaded = createPersistence({ adapter: backend.tab() })
    try {
      const restored = registry.get(
        reloaded.atom({
          key: 'window',
          schema: WindowStateSchema,
          defaultValue: { ...saved, read: new Set<string>() },
        }),
      )
      expect(restored.selected).toBe('agent/compiler')
      expect(restored.layouts).toEqual(saved.layouts)
      expect(restored.docks).toEqual(saved.docks)
      expect(restored.resources).toEqual(saved.resources)
      expect(restored.read.has('attention/question-1')).toBe(true)
    } finally {
      reloaded.dispose()
      registry.dispose()
    }
  })

  it('rewrites persisted editor identities once without losing panes, focus, read state or drafts', () => {
    const backend = storage()
    const first = createPersistence({ adapter: backend.tab() })
    const registry = AtomRegistry.make()
    const docks = {
      primary: { visible: true, size: 312, activeView: 'navigation' },
      panel: { visible: false, size: 278, activeView: 'monitor.quota' },
    }
    const layout = {
      version: 2,
      editorArea: {
        _tag: 'split',
        dir: 'row',
        children: [
          { _tag: 'group', id: 'g0' },
          { _tag: 'group', id: 'g1' },
        ],
        sizes: [-1, 0],
      },
      groups: {
        g0: {
          editors: [
            { input: { ref: 'agent/compiler', editor: 'wf.agentSession' } },
            { input: { ref: 'agent/compiler', editor: 'wf.agentUsage' } },
            { input: { ref: 'agent/compiler', editor: 'wf.agentResources' } },
            { input: { ref: 'terminal/compiler', editor: 'wf.terminal' } },
          ],
          active: 'wf.agentUsage|agent/compiler',
        },
        g1: {
          editors: [
            { input: { ref: 'mission/compiler', editor: 'wf.mission' } },
            { input: { ref: 'resource/report', editor: 'wf.subject' } },
          ],
          active: 'wf.mission|mission/compiler',
        },
      },
      focusedGroup: 'g1',
      nextGroup: 8,
    }
    const old = {
      selected: 'agent/compiler',
      layouts: { 'agent/compiler': layout },
      docks,
      resources: { expanded: true, size: 444 },
      monitorDetailSize: 512,
    }
    for (const key of ['migration:window:one', 'migration:layout:last-used', 'migration:window']) {
      registry.set(
        first.atom({ key, schema: Schema.Unknown, defaultValue: null }),
        key === 'migration:window' ? { ...old, read: ['attention/read'] } : old,
      )
    }
    registry.set(
      first.atom({ key: 'composer.draft.agent/compiler', schema: Schema.String, defaultValue: '' }),
      'unsent draft stays untouched',
    )
    first.dispose()
    const restoredStore = createPersistence({ adapter: backend.tab() })
    const seed = { ...old, layouts: {}, read: new Set<string>() }
    try {
      const restored = registry.get(
        windowStateAtom({
          gateway: 'migration',
          seed,
          windowId: 'one',
          persist: restoredStore.atom,
        }),
      )
      const current = restored.layouts['agent/compiler']!
      expect(current.groups.g0?.editors.map((entry) => entry.input)).toEqual([
        { ref: 'agent/compiler', presentation: 'detail' },
        { ref: 'agent/compiler', presentation: 'overview' },
        { ref: 'agent/compiler', presentation: 'resources' },
        { ref: 'terminal/compiler', presentation: 'detail' },
      ])
      expect(current.groups.g1?.editors.map((entry) => entry.input)).toEqual([
        { ref: 'mission/compiler', presentation: 'detail' },
        { ref: 'resource/report', presentation: 'detail' },
      ])
      expect(current.groups.g0?.active).toBe('overview|agent/compiler')
      expect(current.groups.g1?.active).toBe('detail|mission/compiler')
      expect(current.editorArea).toEqual({ ...layout.editorArea, sizes: [0.5, 0.5] })
      expect(current.focusedGroup).toBe('g1')
      expect(current.nextGroup).toBe(8)
      expect(restored.docks).toEqual(docks)
      expect(restored.resources).toEqual(old.resources)
      expect(restored.monitorDetailSize).toBe(512)
      expect(restored.read).toEqual(new Set(['attention/read']))
      expect(
        registry.get(
          restoredStore.atom({
            key: 'composer.draft.agent/compiler',
            schema: Schema.String,
            defaultValue: '',
          }),
        ),
      ).toBe('unsent draft stays untouched')
      restoredStore.flush()
      const document = Schema.decodeUnknownSync(
        Schema.Struct({
          entries: Schema.Record(Schema.String, Schema.Struct({ version: Schema.Int })),
        }),
      )(JSON.parse(backend.tab().read()!))
      for (const key of ['migration:window:one', 'migration:layout:last-used', 'migration:window'])
        expect(document.entries[key]?.version).toBe(2)
      const again = registry.get(
        windowStateAtom({
          gateway: 'migration',
          seed,
          windowId: 'one',
          persist: restoredStore.atom,
        }),
      )
      expect(again).toEqual(restored)
      expect(restoredStore.error).toBeUndefined()
    } finally {
      restoredStore.dispose()
      registry.dispose()
    }
  })

  it('roundtrips native subject URLs and rejects malformed generated references or presentations', () => {
    for (const presentation of ['detail', 'overview', 'resources'] as const) {
      const address = Schema.decodeSync(SubjectAddress)({
        ref: 'agent/compiler',
        presentation,
      })
      expect(parseSubjectUrl(subjectUrl(address))).toEqual(address)
    }
    expect(subjectUrl({ ref: 'mission/compiler', presentation: 'detail' })).toBe(
      '/mission/compiler',
    )
    expect(subjectUrl({ ref: 'agent/compiler', presentation: 'overview' })).toBe(
      '/agent/compiler?presentation=overview',
    )
    const escaped = { ref: 'resource/report?section=#result', presentation: 'resources' } as const
    expect(parseSubjectUrl(subjectUrl(escaped))).toEqual(escaped)
    for (const invalid of [
      '/not-a-subject',
      '/agent/',
      '/agent/has%20space',
      '/Agent/compiler',
      '/agent/compiler?presentation=editor',
      '/agent/%ZZ',
    ])
      expect(parseSubjectUrl(invalid)).toBeUndefined()
  })
})
