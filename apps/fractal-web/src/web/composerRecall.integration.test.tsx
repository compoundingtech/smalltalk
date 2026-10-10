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
const mount = async (history: readonly string[], props: Partial<React.ComponentProps<typeof EmbraceComposer>> = {}) => React.act(async () => {
  root.render(<EmbraceRuntimeProvider options={{ messages: [], onNew: async () => {} }}><EmbraceComposer variant="C1" showHistory history={history} toolbar={<span>Model</span>} {...props} /></EmbraceRuntimeProvider>)
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
it('loads submitted history only on recall activation, never on mount or transcript rerenders', async () => {
  const get = vi.fn(() => ['Previous submitted message'])
  await mount([], { historySource: { available: true, get } })
  await mount([], { historySource: { available: true, get } })
  expect(get).not.toHaveBeenCalled()
  await React.act(async () => container.querySelector<HTMLButtonElement>('button[aria-label="Recall previous message"]')!.click())
  expect(get).toHaveBeenCalledTimes(1)
  expect(container.querySelector('textarea')?.value).toBe('Previous submitted message')
})
it('resets the recall cursor when submitting before the next confirmed draft appears', async () => {
  await mount(['Earlier', 'Latest'])
  const button = () => container.querySelector<HTMLButtonElement>('button[aria-label="Recall previous message"]')!
  await React.act(async () => button().click())
  await React.act(async () => container.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })))
  await mount(['Earlier', 'Latest', 'New confirmed'])
  await React.act(async () => button().click())
  expect(container.querySelector('textarea')?.value).toBe('New confirmed')
})
it('leaves multiline caret navigation alone away from the first and last line', async () => {
  await mount(['Earlier', 'first\nsecond\nthird'])
  await React.act(async () => container.querySelector<HTMLButtonElement>('button[aria-label="Recall previous message"]')!.click())
  const field = container.querySelector('textarea')!
  field.setSelectionRange(8, 8)
  for (const key of ['ArrowUp', 'ArrowDown']) {
    const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true })
    await React.act(async () => field.dispatchEvent(event))
    expect(event.defaultPrevented).toBe(false)
    expect(field.value).toBe('first\nsecond\nthird')
  }
  field.setSelectionRange(0, 0)
  await React.act(async () => field.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true, cancelable: true })))
  expect(field.value).toBe('Earlier')
})
