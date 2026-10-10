// @vitest-environment jsdom
import * as React from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { EmbraceComposer } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceComposer.tsx'
import { EmbraceRuntimeProvider } from '../../../../packages/fractal-ui/src/assistant-ui/EmbraceRuntime.tsx'
vi.mock('@stylexjs/stylex', () => ({ create: (v: unknown) => v, defineVars: (v: unknown) => v, keyframes: () => 'animation', props: () => ({}) }))
const container = document.createElement('div')
let root: Root
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  document.body.append(container)
  root = createRoot(container)
})
afterEach(async () => { await React.act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals() })
const mount = async (history: readonly string[]) => React.act(async () => {
  root.render(<EmbraceRuntimeProvider options={{ messages: [], onNew: async () => {} }}><EmbraceComposer variant="C1" showHistory history={history} toolbar={<span>Model</span>} /></EmbraceRuntimeProvider>)
})
it('renders recall beside Send with a host toolbar and restores the previous submitted draft', async () => {
  await mount(['Earlier draft', 'Previous own message\nsecond line'])
  const recall = container.querySelector<HTMLButtonElement>('button[aria-label="Recall previous message"]')!
  expect(recall).not.toBeNull()
  expect(recall.disabled).toBe(false)
  expect(recall.nextElementSibling?.getAttribute('aria-label')).toBe('Send')
  await React.act(async () => recall.click())
  expect(container.querySelector('textarea')?.value).toBe('Previous own message\nsecond line')
  expect(document.activeElement).toBe(container.querySelector('textarea'))
  expect(container.querySelector('button[aria-label="Recall previous message"]')).toBe(recall)
})
it('disables recall when there is no previous message', async () => {
  await mount([])
  expect(container.querySelector<HTMLButtonElement>('button[aria-label="Recall previous message"]')?.disabled).toBe(true)
})
