import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { JSDOM } from 'jsdom'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

const dom = new JSDOM('<!doctype html><html><body></body></html>', { url: 'http://localhost/' })
const saved = new Map()
for (const name of ['window', 'document', 'navigator', 'HTMLElement', 'SVGElement', 'Element', 'Node', 'Document', 'DocumentFragment', 'HTMLButtonElement', 'HTMLInputElement', 'HTMLTextAreaElement', 'MutationObserver', 'Event', 'CustomEvent', 'KeyboardEvent', 'MouseEvent', 'getComputedStyle']) {
  saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: name === 'getComputedStyle' ? dom.window.getComputedStyle.bind(dom.window) : dom.window[name] })
}
for (const name of ['CSS', 'ResizeObserver', 'requestAnimationFrame', 'cancelAnimationFrame', 'IS_REACT_ACT_ENVIRONMENT']) saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
globalThis.CSS = { escape: value => value }
globalThis.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} }
globalThis.requestAnimationFrame = callback => setTimeout(callback, 0)
globalThis.cancelAnimationFrame = clearTimeout
globalThis.IS_REACT_ACT_ENVIRONMENT = true
const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, plugins: [stylex.vite({ useCSSLayers: false })], server: { middlewareMode: true } })
const { Tooltip, Button } = await server.ssrLoadModule('/src/assistant-ui/composition/Controls.tsx')
after(async () => { await server.close(); dom.window.close(); for (const [name, descriptor] of saved) { if (descriptor === undefined) Reflect.deleteProperty(globalThis, name); else Object.defineProperty(globalThis, name, descriptor) } })
const wait = ms => act(async () => { await new Promise(resolve => setTimeout(resolve, ms)) })
const tips = () => [...document.querySelectorAll('[role=tooltip]')]
const key = async (target, value) => act(async () => target.dispatchEvent(new KeyboardEvent('keydown', { key: value, bubbles: true })))

test('main React Aria tooltip supports one open card, Escape and keyboard scroll-settle reopening', async () => {
  const container = document.createElement('section'); document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(React.createElement(React.Fragment, null, ['First', 'Second'].map(label => React.createElement(Tooltip, { key: label, label, delay: 0 }, React.createElement(Button, null, label))))))
    const [first, second] = container.querySelectorAll('button')
    await key(document.body, 'Tab')
    await act(async () => first.focus())
    await wait(20)
    assert.deepEqual(tips().map(tip => tip.textContent), ['First'])
    await act(async () => second.focus())
    await wait(20)
    assert.deepEqual(tips().map(tip => tip.textContent), ['Second'])
    await key(second, 'Escape')
    await wait(120)
    assert.equal(tips().length, 0)
    await act(async () => first.focus())
    await wait(20)
    assert.deepEqual(tips().map(tip => tip.textContent), ['First'])
    await act(async () => container.dispatchEvent(new Event('scroll')))
    assert.equal(tips().length, 0)
    await wait(60)
    await act(async () => container.dispatchEvent(new Event('scroll')))
    await wait(60)
    assert.equal(tips().length, 0, 'each scroll resets the quiet interval')
    await wait(60)
    assert.deepEqual(tips().map(tip => tip.textContent), ['First'])
    await act(async () => container.dispatchEvent(new Event('scroll')))
    await wait(10)
    await key(first, 'Escape')
    await wait(150)
    assert.equal(tips().length, 0, 'Escape cancels reopening while focus stays on the trigger')
    await act(async () => second.focus())
    await wait(20)
    await act(async () => container.dispatchEvent(new Event('scroll')))
    await wait(10)
    await act(async () => first.focus())
    await wait(150)
    assert.deepEqual(tips().map(tip => tip.textContent), ['First'], 'a settling card cannot displace the newer focused card')
  } finally { await act(async () => root.unmount()); container.remove() }
})

test('controlled acknowledgement and shortcut compose with the same tooltip and Escape', async () => {
  const container = document.createElement('section'); document.body.append(container)
  const root = createRoot(container)
  const changes = []
  function Acknowledgement() {
    const [open, setOpen] = React.useState(true)
    return React.createElement(Tooltip, { label: 'Copied', shortcut: 'Ctrl+C', isOpen: open, onOpenChange: value => { changes.push(value); setOpen(value) } }, React.createElement(Button, null, 'Copy'))
  }
  try {
    await act(async () => root.render(React.createElement(Acknowledgement)))
    await key(document.body, 'Tab')
    await act(async () => container.querySelector('button').focus())
    await wait(20)
    assert.equal(tips().length, 1)
    assert.equal(tips()[0].textContent, 'Copied Ctrl+C')
    assert.equal(tips()[0].querySelector('kbd').textContent, 'Ctrl+C')
    await key(container.querySelector('button'), 'Escape')
    await wait(120)
    assert.equal(tips().length, 0)
    assert.ok(changes.includes(false), 'controlled owner receives dismissal')
  } finally { await act(async () => root.unmount()); container.remove() }
})
