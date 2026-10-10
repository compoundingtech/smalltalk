import { Attention, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'

import type { SubjectSummary } from './context.tsx'
import { subjectKey, initialState, reduceLayout } from './state.ts'
import {
  createWindow,
  createWorkspaceProjection,
  projectWorkspaces,
  reduceWindow,
} from './workspaces.ts'

const subjects: readonly SubjectSummary[] = [
  { ref: 'agent/other', title: 'Other workspace', icon: 'conversation' },
  { ref: 'agent/target', title: 'Target workspace', icon: 'conversation', attention: true },
]
const attention = ['first', 'second'].map((name) =>
  decodeUnknownSync(Attention)({
    id: `attention/${name}`,
    kind: 'attention',
    revision: '1',
    updated_at: '2026-10-03T00:00:00Z',
    requested_at: '2026-10-03T00:00:00Z',
    source_id: 'agent/target',
    person_id: 'person/operator',
    attention_kind: 'agent-request',
    title: `Request ${name}`,
    detail: `Independent request ${name}`,
    priority: 'normal',
    state: 'open',
    actions: [],
  }),
)
const seed = initialState({
  primary: { visible: true, size: 280, activeView: null },
  panel: { visible: false, size: 240, activeView: null },
})

describe('workspace attention read ownership', () => {
  it('keeps sibling cards unread when opening one card in another workspace', () => {
    const initial = createWindow({ seed, subjects, attention })
    expect(
      projectWorkspaces({ window: initial, subjects, attention }).find(
        (workspace) => workspace.id === 'agent/target',
      )?.unread,
    ).toEqual(['attention/first', 'attention/second'])

    const read = reduceWindow({
      window: initial,
      action: { _tag: 'ReadInbox', ref: 'attention/first' },
      subjects,
      attention,
    })
    const opened = reduceWindow({
      window: read,
      action: {
        _tag: 'Layout',
        workspace: 'agent/target',
        action: { _tag: 'Open', input: { ref: 'agent/target', presentation: 'detail' } },
      },
      subjects,
      attention,
    })
    expect(opened.selected).toBe('agent/target')
    expect(opened.read.has('attention/first')).toBe(true)
    expect(opened.read.has('attention/second')).toBe(false)
    expect(
      projectWorkspaces({ window: opened, subjects, attention }).find(
        (workspace) => workspace.id === 'agent/target',
      )?.unread,
    ).toEqual(['attention/second'])
  })

  it('reads all linked card identities on explicit workspace selection without resolving them', () => {
    const window = reduceWindow({
      window: createWindow({ seed, subjects, attention }),
      action: { _tag: 'SelectWorkspace', id: 'agent/target' },
      subjects,
      attention,
    })
    expect(window.read).toEqual(new Set(['attention/first', 'attention/second']))
    expect(
      projectWorkspaces({ window, subjects, attention }).find(
        (workspace) => workspace.id === 'agent/target',
      ),
    ).toMatchObject({ unread: [], notifications: [{ ref: 'agent/target', attention: true }] })
  })

  it('reads linked cards when their workspace is initially visible', () => {
    const input = { ref: 'agent/target', presentation: 'detail' } as const
    const window = createWindow({
      seed: {
        ...seed,
        groups: { g0: { editors: [{ input }], active: subjectKey(input) } },
      },
      subjects,
      attention,
    })
    expect(window.selected).toBe('agent/target')
    expect(window.read).toEqual(new Set(['attention/first', 'attention/second']))
  })
})

describe('incremental workspace projection', () => {
  it('preserves unchanged workspace and default layout identities across roster changes', () => {
    const project = createWorkspaceProjection()
    const window = createWindow({ seed, subjects, attention: [] })
    const first = project({ window, subjects, attention: [] })
    expect(project({ window, subjects: [...subjects], attention: [] })).toBe(first)
    const changedSubjects = subjects.map((subject) =>
      subject.ref === 'agent/target' ? { ...subject, title: 'Renamed target' } : subject,
    )
    const changed = project({ window, subjects: changedSubjects, attention: [] })
    expect(changed[0]).toBe(first[0])
    expect(changed[1]).not.toBe(first[1])
    expect(changed[1]?.layout).toBe(first[1]?.layout)
    const removed = project({ window, subjects: [subjects[0]!], attention: [] })
    expect(removed).toEqual([first[0]])
    expect(removed[0]).toBe(first[0])
  })

  it('updates only the owner when linked unread card identities change', () => {
    const project = createWorkspaceProjection()
    const window = createWindow({ seed, subjects, attention: [] })
    const first = project({ window, subjects, attention: [] })
    const linked = project({ window, subjects, attention })
    expect(linked[0]).toBe(first[0])
    expect(linked[1]).not.toBe(first[1])
    expect(linked[1]?.unread).toEqual(['attention/first', 'attention/second'])
    const read = project({
      window: { ...window, read: new Set(['attention/first']) },
      subjects,
      attention,
    })
    expect(read[0]).toBe(first[0])
    expect(read[1]?.unread).toEqual(['attention/second'])
  })

  it('does not retain a removed saved layout as a default layout', () => {
    const project = createWorkspaceProjection()
    const window = createWindow({ seed, subjects, attention: [] })
    const first = project({ window, subjects, attention: [] })
    const { docks: _docks, ...customized } = reduceLayout({
      state: { ...first[1]!.layout, docks: window.docks },
      action: { _tag: 'Open', input: { ref: 'mission/extra', presentation: 'detail' } },
    })
    project({
      window: { ...window, layouts: { ...window.layouts, 'agent/target': customized } },
      subjects,
      attention: [],
    })
    const reset = project({ window, subjects, attention: [] })
    expect(reset[1]?.layout).not.toBe(customized)
    expect(reset[1]?.layout.focusedGroup).toBe('g0')
    expect(reset[1]?.layout.groups.g0?.editors.map((editor) => editor.input.ref)).toEqual([
      'agent/target',
    ])
  })
})

describe('conversation-first session layout', () => {
  it('opens a session without creating a terminal or empty split', () => {
    const window = reduceWindow({
      window: createWindow({ seed, subjects, attention: [] }),
      action: { _tag: 'SelectWorkspace', id: 'agent/target' },
      subjects,
      attention: [],
    })
    const layout = projectWorkspaces({ window, subjects, attention: [] }).find(
      (workspace) => workspace.id === window.selected,
    )?.layout
    expect(layout?.editorArea).toEqual({ _tag: 'group', id: 'g0' })
    expect(layout?.groups).toEqual({
      g0: {
        editors: [{ input: { ref: 'agent/target', presentation: 'detail' } }],
        active: subjectKey({ ref: 'agent/target', presentation: 'detail' }),
      },
    })
  })

  it('opens the canonical terminal in the bottom panel, or explicitly as a tab or side pane', () => {
    const input = { ref: 'agent/target', presentation: 'detail' } as const
    const conversation = reduceLayout({ state: seed, action: { _tag: 'Open', input } })
    const bottom = reduceLayout({ state: conversation, action: { _tag: 'OpenTerminal' } })
    expect(bottom.groups).toEqual(conversation.groups)
    expect(bottom.focusedGroup).toBe(conversation.focusedGroup)
    expect(bottom.docks.panel).toEqual({
      ...conversation.docks.panel,
      visible: true,
      activeView: 'workspace.terminal',
    })
    const tab = reduceLayout({ state: conversation, action: { _tag: 'OpenTerminal', side: false } })
    expect(tab.editorArea).toEqual(conversation.editorArea)
    expect(tab.groups.g0?.active).toBe(
      subjectKey({ ref: 'terminal/target', presentation: 'detail' }),
    )
    expect(tab.groups.g0?.editors.map((entry) => entry.input.ref)).toEqual([
      'agent/target',
      'terminal/target',
    ])
    const split = reduceLayout({
      state: conversation,
      action: { _tag: 'OpenTerminal', side: true },
    })
    expect(split.editorArea).toMatchObject({ _tag: 'split', dir: 'row' })
    expect(split.groups.g0?.active).toBe(subjectKey(input))
    expect(split.groups.g1?.editors).toEqual([
      { input: { ref: 'terminal/target', presentation: 'detail' } },
    ])
  })

  it('keeps terminal placement and resizing outside the focused mission pane', () => {
    let window = createWindow({ seed, subjects, attention: [] })
    window = reduceWindow({
      window,
      subjects,
      attention: [],
      action: { _tag: 'SelectWorkspace', id: 'agent/target' },
    })
    window = reduceWindow({
      window,
      subjects,
      attention: [],
      action: {
        _tag: 'Layout',
        action: { _tag: 'Open', input: { ref: 'mission/review', presentation: 'detail' } },
      },
    })
    const panes = window.layouts
    window = reduceWindow({
      window,
      subjects,
      attention: [],
      action: { _tag: 'Layout', action: { _tag: 'OpenTerminal' } },
    })
    expect(window.layouts).toEqual(panes)
    expect(window.docks.panel).toMatchObject({ visible: true, activeView: 'workspace.terminal' })
    window = reduceWindow({
      window,
      subjects,
      attention: [],
      action: { _tag: 'Layout', action: { _tag: 'ResizeDock', dock: 'panel', size: 348 } },
    })
    window = reduceWindow({
      window,
      subjects,
      attention: [],
      action: { _tag: 'Layout', action: { _tag: 'ToggleDock', dock: 'panel' } },
    })
    expect(window.docks.panel).toMatchObject({ visible: false, size: 348 })
    expect(window.layouts).toEqual(panes)
  })
})
