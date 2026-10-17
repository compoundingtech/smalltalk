import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { JSDOM } from 'jsdom'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

// Real React/Aria/StyleX component in jsdom; only the DOM globals are installed here.
const dom = new JSDOM('<!doctype html><html><body></body></html>', { url: 'http://localhost/', pretendToBeVisual: true })
for (const name of ['window', 'document', 'navigator', 'HTMLElement', 'SVGElement', 'Element', 'Node', 'NodeFilter', 'Document', 'DocumentFragment', 'HTMLButtonElement', 'MutationObserver', 'Event', 'CustomEvent', 'UIEvent', 'MouseEvent', 'KeyboardEvent', 'FocusEvent', 'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame']) {
  const value = name === 'getComputedStyle' ? dom.window.getComputedStyle.bind(dom.window) : dom.window[name]
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: typeof value === 'function' && /^[a-z]/.test(name) ? value.bind(dom.window) : value })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
dom.window.HTMLElement.prototype.checkVisibility = () => true
const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, plugins: [stylex.vite({ useCSSLayers: false })], optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true } })
after(() => server.close())
const { SidePanel } = await server.ssrLoadModule('/src/assistant-ui/composition/SidePanel.tsx')

const Host = () => {
  const [open, setOpen] = React.useState(false)
  return React.createElement(React.Fragment, null,
    React.createElement('main', { 'aria-label': 'Workspace' }, React.createElement('button', { id: 'opener', onClick: () => setOpen(value => !value) }, 'Resources')),
    open && React.createElement(SidePanel, { 'aria-label': 'Agent resources', id: 'panel', onClose: () => setOpen(false) },
      React.createElement('button', { id: 'close', onClick: () => setOpen(false) }, 'Close resources'),
      React.createElement('button', { id: 'child', onKeyDown: event => { if (event.key === 'Escape') event.preventDefault() } }, 'Child with its own Escape')))
}

const keydown = (target, key) => target.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }))
// React Aria restores focus on the frame after the scope unmounts.
const frame = () => act(() => new Promise(resolve => requestAnimationFrame(() => resolve())))

test('opens as a labelled complementary region without inerting or hiding the app, and Escape returns focus', async () => {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(React.createElement(Host)))
    const opener = document.getElementById('opener')
    opener.focus()
    await act(async () => opener.click())
    const panel = document.getElementById('panel')
    assert.equal(panel.tagName, 'ASIDE')
    assert.equal(panel.getAttribute('aria-label'), 'Agent resources')
    assert.equal(panel.getAttribute('role'), null)
    assert.equal(panel.getAttribute('aria-modal'), null)
    assert.equal(document.querySelectorAll('[inert], [aria-hidden="true"]').length, 0)
    assert.ok(panel.contains(document.activeElement), 'focus moves into the panel on open')
    // Focus is not trapped: the workspace stays focusable while the panel is open.
    await act(async () => opener.focus())
    assert.equal(document.activeElement, opener)
    // A child that handles Escape keeps the panel open.
    const child = document.getElementById('child')
    await act(async () => child.focus())
    await act(async () => keydown(child, 'Escape'))
    assert.ok(document.getElementById('panel'))
    await act(async () => document.getElementById('close').focus())
    await act(async () => keydown(document.activeElement, 'Escape'))
    await frame()
    assert.equal(document.getElementById('panel'), null)
    assert.equal(document.activeElement, opener)
  } finally {
    await act(async () => root.unmount())
    container.remove()
  }
})
