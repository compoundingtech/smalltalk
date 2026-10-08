import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ThreadHeader } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { ResourceCardV1, ResourceChipV1, type ResourceData, type ResourceFile } from '@smalltalk/fractal-ui/assistant-ui/resources'
import { SyncLine } from '@smalltalk/fractal-ui/assistant-ui/sync'
import type { Agent } from '../data/source.ts'
import { sidebarRow } from './sidebarRow.ts'

// Match the app's node-rendering lane: exercise semantic DOM without the CSS compiler.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

const unknown = { _tag: 'Unknown' } as const
const file: ResourceFile = { path: 'captured/file.ts', added: unknown, removed: unknown, lines: unknown }
const resource: ResourceData = { kind: 'diff', label: 'Recorded capture', detail: 'Captured successful-tool excerpt', fileCount: unknown, files: [file] }
const header = {
  folder: 'Reported folder', title: 'Reported agent', panelOpen: false, drawerOpen: false,
  onTogglePanel() {}, onToggleDrawer() {},
  nativeActions: createElement('button', { 'data-native-action': 'existing' }, 'Bound action'),
}
const agent: Agent = { ref: 'agent/fixture/readonly', terminal: '', name: 'Reported name', lifecycle: unknown, host: 'Reported host', connected: true, activity: 'idle', status: 'idle', usage: unknown, checkout: unknown, workspace: unknown, startedAt: unknown, endedAt: unknown, blockedOn: unknown, ask: unknown, lastActivityAt: unknown }

describe('resource card seam', () => {
  it('keeps unknown counts explicit instead of rendering zero', () => {
    const card = renderToStaticMarkup(createElement(ResourceCardV1, { resource }))
    expect(card).toMatch(/Changed files · count unknown/)
    expect(card).not.toMatch(/\+0|−0|0 changed/)
    expect(renderToStaticMarkup(createElement(ResourceCardV1, { resource: { ...resource, files: [] } }))).not.toMatch(/\+0|−0/)
  })
  it('renders a reported zero as zero', () => {
    const zero: ResourceFile = { ...file, added: { _tag: 'Known', value: 0 }, removed: { _tag: 'Known', value: 0 }, lines: { _tag: 'Known', value: [] } }
    const chip = renderToStaticMarkup(createElement(ResourceChipV1, { resource: { ...resource, files: [zero] }, file: zero }))
    expect(chip).toMatch(/\+0/)
    expect(chip).toMatch(/−0/)
  })
})

describe('thread header seam', () => {
  it('renders only the actions the host binds', () => {
    const bare = renderToStaticMarkup(createElement(ThreadHeader, header))
    expect(bare).toMatch(/data-native-action="existing"/)
    expect(bare).toMatch(/native-action-slot/)
    expect(bare).not.toMatch(/Toggle terminal drawer|>Open|>Commit|1970|Needs you/)
    const bound = renderToStaticMarkup(createElement(ThreadHeader, { ...header, terminalAvailable: true, onOpen() {}, onCommit() {}, status: 'working', freshness: 'stale', statusSince: 1000, now: 2000 }))
    expect(bound).toMatch(/Toggle terminal drawer/)
    expect(bound).toMatch(/>Open/)
    expect(bound).toMatch(/>Commit/)
    expect(bound).toMatch(/stale observation/)
    expect(bound).not.toMatch(/Needs you/)
  })
  it.each([
    ['waiting', 'Native waiting for tool response', false],
    ['errored', 'Native tool execution errored', false],
    ['working', 'Native working before connection loss', true],
  ] as const)('sanitizes unrecognized %s status labels through the roster projection', (activity, reported, stale) => {
    const row = sidebarRow({ agent: { ...agent, activity, status: reported }, stale, now: 2000 })
    const output = renderToStaticMarkup(createElement(ThreadHeader, { ...header, status: row.status, statusLabel: row.statusLabel, statusSince: row.statusSince, freshness: row.freshness, now: 2000 }))
    const expected = stale ? 'Last verified · Not observed' : 'Not observed'
    expect(output).toContain(`aria-label="${expected}; Status boundary unavailable; ${row.freshness} observation"`)
    expect(output).not.toMatch(/Needs you|aria-label="Stale observation;/)
  })
})

describe('sync line renderer', () => {
  it('separates an unanswered request from live data', () => {
    const waiting = renderToStaticMarkup(createElement(SyncLine, { status: { _tag: 'Requested', since: 1000 }, label: 'conversation', observedAt: 1000, now: 6500 }))
    expect(waiting).toMatch(/Loading conversation is taking longer than expected · 5s/)
    expect(waiting).toMatch(/data-sync-state="Requested"/)
    const live = renderToStaticMarkup(createElement(SyncLine, { status: { _tag: 'Live', since: 1000 }, label: 'conversation', observedAt: 1000, now: 6500 }))
    expect(live).not.toMatch(/taking longer than expected|stopped reporting/)
  })
})
