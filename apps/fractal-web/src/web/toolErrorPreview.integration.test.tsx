// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { EmbraceToolItem } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceToolCall.tsx'
import { ToolDetailPreview } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Transcript.tsx'

vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}), keyframes: () => 'test-animation', props: () => ({}),
}))

let root: Root
let container: HTMLDivElement
const diagnostic = 'The requested change could not be applied.'
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
})
afterEach(async () => {
  await act(async () => root.unmount())
  container.remove()
  vi.unstubAllGlobals()
})

it.each(['plain', 'markdown', 'diff'] as const)('shows failed tool diagnostics before raw disclosure alongside %s content', async kind => {
  const input = kind === 'markdown' ? { path: 'notes.md', content: 'Proposed **note**.' }
    : kind === 'diff' ? { patch: '--- rows.ts\n+++ rows.ts\n@@ -1 +1 @@\n-old\n+new' } : { command: 'check rows' }
  await act(async () => root.render(<EmbraceToolItem variant="cards" item={{
    _tag: 'ToolCall', id: 'failed-call', callId: 'failed-call', name: kind === 'diff' ? 'edit' : kind === 'markdown' ? 'write' : 'run',
    input, status: 'error', callSeen: true, at: '2026-01-15T12:00:00Z',
    result: { content: diagnostic, isError: true, at: '2026-01-15T12:00:01Z' },
  }} />))
  const raw = container.querySelector('details')!
  expect(raw.open).toBe(false)
  expect([...container.querySelectorAll('pre')].filter(pre => !raw.contains(pre)).map(pre => pre.textContent)).toContain(diagnostic)
})

it('shows a failed transcript tool as its summary and one-line reason; the path dump waits for the raw disclosure', async () => {
  const dump = "Error: ENOENT: no such file or directory, open '/srv/example/workspace/apps/rows/src/rows.ts'\n    at open (/srv/example/workspace/node_modules/loader/index.js:12:3)"
  await act(async () => root.render(<ToolDetailPreview call={{ id: 'failed-call', kind: 'read', title: 'Reading rows', summary: 'Read rows.ts', status: 'error', startedAt: '2026-01-15T12:00:00Z', detail: dump }} />))
  const trigger = container.querySelector('button')!
  expect(trigger.getAttribute('aria-expanded')).toBe('false')
  expect(container.querySelector('p')?.textContent).toBe('Read rows.ts · error')
  expect(container.querySelector('[data-testid="tool-error-reason"]')?.textContent).toBe("Error: ENOENT: no such file or directory, open '…/rows.ts'")
  expect(container.textContent).not.toContain('/srv/example')
  expect(container.querySelector('pre')).toBeNull()
  await act(async () => trigger.click())
  expect(container.querySelector('pre')?.textContent).toBe(dump)
})
