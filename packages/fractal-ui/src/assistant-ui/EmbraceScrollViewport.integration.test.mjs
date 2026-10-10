import assert from 'node:assert/strict'
import { after, afterEach, beforeEach, test } from 'node:test'
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { JSDOM } from 'jsdom'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

// Real React/Aria/StyleX components; jsdom supplies no layout, so only geometry,
// animation frames and resize delivery are controlled by this harness.
const dom = new JSDOM('<!doctype html><html><body></body></html>', { url: 'http://localhost/' })
const saved = new Map()
for (const name of ['window', 'document', 'navigator', 'HTMLElement', 'SVGElement', 'Element', 'Node', 'Document', 'DocumentFragment', 'HTMLButtonElement', 'HTMLInputElement', 'HTMLTextAreaElement', 'MutationObserver', 'Event', 'KeyboardEvent', 'WheelEvent', 'getComputedStyle']) {
  saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: name === 'getComputedStyle' ? dom.window.getComputedStyle.bind(dom.window) : dom.window[name] })
}
for (const name of ['CSS', 'ResizeObserver', 'requestAnimationFrame', 'cancelAnimationFrame', 'requestIdleCallback', 'cancelIdleCallback', 'IS_REACT_ACT_ENVIRONMENT']) saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
globalThis.IS_REACT_ACT_ENVIRONMENT = true
const observers = new Set()
// Aria reads only the fixed CSP meta key here; jsdom lacks CSS.escape.
globalThis.CSS = { escape: value => { assert.match(value, /^[a-z][a-z0-9-]*$/i); return value } }
globalThis.ResizeObserver = class {
  targets = new Set()
  constructor(callback) { this.callback = callback; observers.add(this) }
  observe(target) { this.targets.add(target) }
  unobserve(target) { this.targets.delete(target) }
  disconnect() { this.targets.clear(); observers.delete(this) }
}
let frames = new Map()
let frameId = 0
globalThis.requestAnimationFrame = callback => { frames.set(++frameId, callback); return frameId }
globalThis.cancelAnimationFrame = id => frames.delete(id)
let idleTasks = new Map()
let idleId = 0
globalThis.requestIdleCallback = callback => { idleTasks.set(++idleId, callback); return idleId }
globalThis.cancelIdleCallback = id => idleTasks.delete(id)
dom.window.HTMLElement.prototype.checkVisibility = () => true
const server = await createServer({
  configFile: false,
  root: new URL('../../', import.meta.url).pathname,
  plugins: [stylex.vite({ useCSSLayers: false })],
  server: { middlewareMode: true },
})
const { EmbraceScrollViewport, ViewportStore, ViewportStoreContext } = await server.ssrLoadModule('/src/assistant-ui/EmbraceScrollViewport.tsx')
const { Transcript } = await server.ssrLoadModule('/src/assistant-ui/composition/Transcript.tsx')
const { EmbraceRuntimeProvider } = await server.ssrLoadModule('/src/assistant-ui/EmbraceRuntime.tsx')
const { WorkLogV1 } = await server.ssrLoadModule('/src/assistant-ui/taste/WorkLogV1.tsx')
let root
let container
let height
let rowOffset
let handle
const flush = async () => {
  await act(async () => {
    const pending = [...frames.values()]
    frames.clear()
    pending.forEach(callback => callback(0))
  })
}
const resize = async lane => {
  await act(async () => {
    for (const observer of observers) if (observer.targets.has(lane) || observer.targets.has(lane.firstElementChild)) observer.callback([], observer)
  })
  await flush()
}
const mount = async (strict = false) => {
  const element = React.createElement(EmbraceScrollViewport, {
    ref: handle, items: [{ id: 'anchor', version: 'earlier' }, { id: 'last', version: 'reply' }], 'data-testid': 'lane',
  }, React.createElement('div', { 'data-item-id': 'anchor' }, 'Earlier row'), React.createElement('div', { 'data-item-id': 'last' }, 'Reply'))
  await act(async () => root.render(strict ? React.createElement(React.StrictMode, null, element) : element))
  const lane = container.querySelector('[data-testid="lane"]')
  Object.defineProperties(lane, {
    scrollHeight: { configurable: true, get: () => height },
    clientHeight: { configurable: true, get: () => 400 },
    clientWidth: { configurable: true, get: () => 800 },
  })
  lane.getBoundingClientRect = () => ({ top: 0, bottom: 400, left: 0, right: 800, width: 800, height: 400 })
  const row = lane.querySelector('[data-item-id]')
  row.getBoundingClientRect = () => ({ top: rowOffset - lane.scrollTop, bottom: rowOffset - lane.scrollTop + 200, left: 0, right: 800, width: 800, height: 200 })
  const last = lane.querySelector('[data-item-id="last"]')
  last.getBoundingClientRect = () => ({ top: height - 200 - lane.scrollTop, bottom: height - lane.scrollTop, left: 0, right: 800, width: 800, height: 200 })
  await flush()
  // Deliver the native event resulting from the controller's initial end write.
  await act(async () => lane.dispatchEvent(new Event('scroll')))
  return lane
}
beforeEach(() => {
  height = 2000
  rowOffset = 900
  handle = React.createRef()
  frames = new Map()
  idleTasks = new Map()
  container = document.createElement('div')
  document.body.appendChild(container)
  root = createRoot(container)
})
afterEach(async () => {
  await act(async () => root.unmount())
  container.remove()
})
after(async () => {
  await server.close()
  dom.window.close()
  for (const [name, descriptor] of saved) {
    if (descriptor === undefined) Reflect.deleteProperty(globalThis, name)
    else Object.defineProperty(globalThis, name, descriptor)
  }
})

test('backfill through preserveLayout and later asynchronous last-row growth keep the end pinned', async () => {
  const lane = await mount()
  assert.equal(lane.scrollTop, 1600)
  // Backfill/containment remeasurement shifts the old first row by a different
  // amount from the total content growth. Row-delta compensation is for reading,
  // not following: the controller must use the new end instead.
  await act(async () => handle.current.preserveLayout(() => { height = 2400; rowOffset += 100 }))
  await act(async () => lane.dispatchEvent(new Event('scroll')))
  assert.equal(lane.scrollTop, 2000)
  await act(async () => { lane.querySelector('[data-item-id="last"]').append('\nMore asynchronously rendered reply content') })
  height = 2800
  await resize(lane)
  assert.equal(lane.scrollTop, 2400)
  assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
})

test('reader wheel and scroll-up remain detached through asynchronous growth', async () => {
  const lane = await mount()
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
    lane.scrollTop = 700
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  await act(async () => { lane.querySelector('[data-item-id="last"]').append('\nMore asynchronously rendered reply content') })
  height = 2400
  await resize(lane)
  assert.equal(lane.scrollTop, 700)
})

test('backfill compensation uses the controller: following keeps the end', async () => {
  const lane = await mount()
  await act(async () => handle.current.preserveLayout(() => { height += 400; rowOffset += 400 }))
  assert.equal(lane.scrollTop, 2000)
  await resize(lane)
  assert.equal(lane.scrollTop, 2000)
})

test('backfill compensation uses the controller: detached keeps its row anchor', async () => {
  const lane = await mount()
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
    lane.scrollTop = 700
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  const row = lane.querySelector('[data-item-id]')
  const top = row.getBoundingClientRect().top
  await act(async () => handle.current.preserveLayout(() => { height += 400; rowOffset += 400 }))
  assert.equal(row.getBoundingClientRect().top, top)
  assert.equal(lane.scrollTop, 1100)
  await resize(lane)
  assert.equal(lane.scrollTop, 1100)
})

test('held pointer drag remains reader intent after more than 250 ms', async t => {
  let now = 1000
  t.mock.method(performance, 'now', () => now)
  const lane = await mount()
  const down = new Event('pointerdown', { bubbles: true })
  Object.defineProperty(down, 'pointerId', { value: 1 })
  await act(async () => lane.dispatchEvent(down))
  await flush()
  now += 500
  await act(async () => { lane.scrollTop = 700; lane.dispatchEvent(new Event('scroll')) })
  await flush()
  height = 2400
  await resize(lane)
  assert.equal(lane.scrollTop, 700)
  const up = new Event('pointerup', { bubbles: true })
  Object.defineProperty(up, 'pointerId', { value: 1 })
  await act(async () => document.dispatchEvent(up))
})

for (const navigation of ['find-in-page', 'programmatic jump-to-message']) {
  for (const detached of [false, true]) for (const concurrentGrowth of [false, true]) test(`${navigation} from ${detached ? 'detached reading' : 'following'} ${concurrentGrowth ? 'with growth before navigation' : 'without concurrent growth'} preserves the intended mode through later growth`, async t => {
    let now = 1000
    t.mock.method(performance, 'now', () => now)
    const lane = await mount()
    if (detached) {
      await act(async () => {
        lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
        lane.scrollTop = 700
        lane.dispatchEvent(new Event('scroll'))
        lane.dispatchEvent(new Event('scrollend'))
      })
      await flush()
    }
    now += 500
    if (concurrentGrowth) {
      // Growth has happened, but its resize callback has not yet been delivered:
      // changed geometry cannot establish what caused the navigation scroll.
      await act(async () => { lane.querySelector('[data-item-id="last"]').append('\nConcurrent asynchronous reply content') })
      height = 2400
    }
    const target = lane.querySelector('[data-item-id="anchor"]')
    if (navigation === 'find-in-page') {
      const highlight = document.createElement('mark')
      highlight.textContent = 'Earlier row'
      target.replaceChildren(highlight)
    }
    // Native find has no input signal: like the stable controller it remains
    // following. Host-owned navigation explicitly detaches through scrollTo.
    await act(async () => {
      if (navigation === 'programmatic jump-to-message') handle.current.scrollTo(rowOffset)
      else lane.scrollTop = rowOffset
      lane.dispatchEvent(new Event('scroll'))
    })
    await flush()
    const remainsFollowing = navigation === 'find-in-page' && !detached
    assert.equal(lane.scrollTop, remainsFollowing ? height - 400 : 900)
    if (navigation === 'find-in-page') assert.equal(target.querySelector('mark').textContent, 'Earlier row')
    await act(async () => { lane.querySelector('[data-item-id="last"]').append('\nMore asynchronously rendered reply content') })
    height += 400
    await resize(lane)
    assert.equal(lane.scrollTop, remainsFollowing ? height - 400 : 900)
    if (!remainsFollowing) assert.equal(target.getBoundingClientRect().top, 0)
  })
}

test('StrictMode double mount retains one live controller and follows or detaches normally', async () => {
  const lane = await mount(true)
  assert.equal([...observers].filter(observer => observer.targets.has(lane)).length, 1)
  assert.equal(lane.scrollTop, 1600)
  height = 2400
  await resize(lane)
  assert.equal(lane.scrollTop, 2000)
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
    lane.scrollTop = 700
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  height = 2800
  await resize(lane)
  assert.equal(lane.scrollTop, 700)
})

test('native shrink clamping stays at the end and follows later growth', async () => {
  const lane = await mount()
  height = 1500
  // Native clamping puts the existing end coordinate at the new maximum.
  await act(async () => { lane.scrollTop = 1100; lane.dispatchEvent(new Event('scroll')) })
  await resize(lane)
  assert.equal(lane.scrollTop, 1100)
  height = 2400
  await resize(lane)
  assert.equal(lane.scrollTop, 2000)
})

test('pinned streaming survives many growth steps with layout-generated scroll events', async () => {
  const lane = await mount()
  for (let step = 0; step < 40; step++) {
    await act(async () => {
      lane.querySelector('[data-item-id="last"]').append(`\nStreamed line ${step}`)
      height += 24
      // Content remeasurement can publish a native scroll before resize delivery;
      // no wheel/touch/navigation key or held lane pointer requested this move.
      lane.scrollTop -= step % 3 === 0 ? 20 : 8
      lane.dispatchEvent(new Event('scroll'))
    })
    await resize(lane)
    assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0, `stream step ${step}`)
  }
})

test('detached reading compensates changes above its row but not growth below it', async () => {
  const lane = await mount()
  await act(async () => handle.current.scrollTo(700))
  await flush()
  const row = lane.querySelector('[data-item-id="anchor"]')
  const y0 = row.getBoundingClientRect().top
  height += 400
  rowOffset += 400
  await resize(lane)
  assert.equal(lane.scrollTop, 1100)
  assert.equal(row.getBoundingClientRect().top, y0)
  height += 400
  await resize(lane)
  assert.equal(lane.scrollTop, 1100)
  assert.equal(row.getBoundingClientRect().top, y0)
})

test('resize delivery repins before the next animation frame', async () => {
  const lane = await mount()
  height += 234
  await act(async () => {
    for (const observer of observers) if (observer.targets.has(lane.firstElementChild)) observer.callback([], observer)
    assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
  })
})

test('resize repin consumes a pending following frame instead of repeating it', async () => {
  const lane = await mount()
  await act(async () => {
    lane.scrollTop -= 20
    lane.dispatchEvent(new Event('scroll'))
  })
  assert.equal(frames.size, 1)
  height += 234
  await act(async () => {
    for (const observer of observers) if (observer.targets.has(lane.firstElementChild)) observer.callback([], observer)
    assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
    assert.equal(frames.size, 0)
  })
})

for (const input of ['wheel', 'touchmove']) {
  test(`${input} momentum reaching the end after 250 ms resumes following`, async t => {
    let now = 1000
    t.mock.method(performance, 'now', () => now)
    const lane = await mount()
    await act(async () => {
      lane.dispatchEvent(input === 'wheel' ? new WheelEvent('wheel', { deltaY: 900 }) : new Event('touchmove'))
      lane.scrollTop = 700
      lane.dispatchEvent(new Event('scroll'))
    })
    await flush()
    now += 500
    await act(async () => { lane.scrollTop = 1600; lane.dispatchEvent(new Event('scroll')) })
    height += 400
    await resize(lane)
    assert.equal(lane.scrollTop, 2000)
  })

  for (const ended of [false, true]) test(`${input} delayed upward scroll ${ended ? 'after scrollend is layout' : 'before scrollend remains momentum'}`, async t => {
    let now = 1000
    t.mock.method(performance, 'now', () => now)
    const lane = await mount()
    await act(async () => {
      lane.dispatchEvent(input === 'wheel' ? new WheelEvent('wheel', { deltaY: 900 }) : new Event('touchmove'))
      // Even without a coordinate delta, landing at the end restores following.
      lane.dispatchEvent(new Event('scroll'))
    })
    now += 500
    if (ended) await act(async () => lane.dispatchEvent(new Event('scrollend')))
    await act(async () => { lane.scrollTop = 1200; lane.dispatchEvent(new Event('scroll')) })
    height += 400
    await resize(lane)
    assert.equal(lane.scrollTop, ended ? 2000 : 1200)
  })
}

test('detached reader line survives shrink above while a later reply grows', async () => {
  const lane = await mount()
  rowOffset = 1188
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -600 }))
    lane.scrollTop = 1000
    lane.dispatchEvent(new Event('scroll'))
    lane.dispatchEvent(new Event('scrollend'))
  })
  await flush()
  const row = lane.querySelector('[data-item-id="anchor"]')
  const y0 = row.getBoundingClientRect().top
  let top = lane.scrollTop
  const writes = []
  Object.defineProperty(lane, 'scrollTop', { configurable: true, get: () => top, set: value => { writes.push(value); top = value } })
  // Older content remeasures smaller while the later reply grows enough that
  // total content still grows. The reader's visible line must not drift.
  rowOffset -= 328
  height += 200
  await resize(lane)
  assert.equal(row.getBoundingClientRect().top, y0)
  assert.equal(lane.scrollTop, 672)
  assert.deepEqual(writes, [672])
})

test('Tab focus into a row action without scrolling keeps following', async () => {
  const lane = await mount()
  const action = document.createElement('button')
  action.textContent = 'Row action'
  lane.querySelector('[data-item-id="anchor"]').append(action)
  // jsdom does not implement Tab navigation; focus() emits the real focusin.
  await act(async () => {
    lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }))
    action.focus()
  })
  assert.equal(document.activeElement, action)
  height += 400
  await resize(lane)
  assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
})

for (const beforeFocus of [false, true]) test(`focus scrolling ${beforeFocus ? 'before' : 'after'} focusin into history detaches at the actual target`, async () => {
  const lane = await mount()
  const action = document.createElement('button')
  action.textContent = 'Earlier row action'
  lane.querySelector('[data-item-id="anchor"]').append(action)
  await act(async () => {
    if (beforeFocus) lane.scrollTop = 900
    action.focus()
    if (!beforeFocus) lane.scrollTop = 900
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  height += 400
  await resize(lane)
  assert.equal(lane.scrollTop, 900)
})

test('explicit navigation that does not move the lane keeps following', async () => {
  const lane = await mount()
  await act(async () => handle.current.scrollTo(lane.scrollTop))
  height += 400
  await resize(lane)
  assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
})

for (const input of ['wheel', 'touchmove', 'keydown', 'pointerdown']) test(`${input} without an actual reader scroll keeps following`, async () => {
  const lane = await mount()
  const event = input === 'wheel' ? new WheelEvent('wheel', { deltaY: -600 }) : input === 'keydown' ? new KeyboardEvent('keydown', { key: 'ArrowUp' }) : new Event(input, { bubbles: true })
  if (input === 'pointerdown') Object.defineProperty(event, 'pointerId', { value: 1 })
  await act(async () => lane.dispatchEvent(event))
  height += 400
  await resize(lane)
  assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
})

test('focus without movement during a pending layout repin keeps following', async () => {
  const lane = await mount()
  const action = document.createElement('button')
  action.textContent = 'Row action'
  lane.querySelector('[data-item-id="anchor"]').append(action)
  await act(async () => {
    lane.scrollTop = 1580
    lane.dispatchEvent(new Event('scroll'))
    assert.equal(frames.size, 1)
    action.focus({ preventScroll: true })
    assert.equal(lane.scrollTop, 1580)
  })
  height += 400
  await resize(lane)
  assert.equal(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 0)
})

for (const beforeFocus of [false, true]) test(`focus scrolling ${beforeFocus ? 'before' : 'after'} focusin during a pending layout repin preserves the history target`, async () => {
  const lane = await mount()
  const action = document.createElement('button')
  action.textContent = 'Earlier row action'
  lane.querySelector('[data-item-id="anchor"]').append(action)
  await act(async () => {
    lane.scrollTop = 1580
    lane.dispatchEvent(new Event('scroll'))
    assert.equal(frames.size, 1)
    // jsdom has no focus scrolling; emulate either native event ordering.
    if (beforeFocus) lane.scrollTop = 900
    action.focus()
    if (!beforeFocus) lane.scrollTop = 900
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  assert.equal(lane.scrollTop, 900)
  height += 400
  await resize(lane)
  assert.equal(lane.scrollTop, 900)
})


for (const kind of ['tool', 'reasoning']) test(`bounded backfill within a turn anchors its ${kind} row, not the turn wrapper`, async () => {
  const lane = await mount()
  const turn = lane.querySelector('[data-item-id="anchor"]')
  const row = document.createElement('div')
  if (kind === 'tool') row.dataset.toolStatus = 'success'
  else row.dataset.testid = 'thinking-entry'
  turn.append(row)
  let rowTop = 925
  row.getBoundingClientRect = () => ({ top: rowTop - lane.scrollTop, bottom: rowTop - lane.scrollTop + 100 })
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => row })
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -700 }))
    lane.scrollTop = 900
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  const before = row.getBoundingClientRect().top
  await act(async () => handle.current.preserveLayout(() => { rowTop += 120; height += 120 }))
  await flush()
  assert.equal(row.getBoundingClientRect().top, before)
  assert.equal(lane.scrollTop, 1020)
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
})


for (const kind of ['message', 'work summary']) test(`row-gap backfill anchors the nested ${kind}, not its unmoving turn wrapper`, async () => {
  const lane = await mount()
  const turn = lane.querySelector('[data-item-id="anchor"]')
  turn.dataset.testid = 'transcript-turn'
  const row = document.createElement('div')
  if (kind === 'message') row.dataset.itemId = 'nested-answer'
  else row.dataset.workLogSummary = ''
  turn.append(row)
  let rowTop = 925
  row.getBoundingClientRect = () => ({ top: rowTop - lane.scrollTop, bottom: rowTop - lane.scrollTop + 100 })
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => turn })
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -700 }))
    lane.scrollTop = 900
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  const before = row.getBoundingClientRect().top
  await act(async () => handle.current.preserveLayout(() => { rowTop += 120; height += 120 }))
  await flush()
  assert.equal(row.getBoundingClientRect().top, before)
  assert.equal(lane.scrollTop, 1020)
  Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
})

test('switching back restores the reader row although rows were backfilled above it while away', async () => {
  const store = new ViewportStore()
  const render = async stateKey => {
    await act(async () => root.render(React.createElement(ViewportStoreContext.Provider, { value: store }, React.createElement(EmbraceScrollViewport, {
      ref: handle, stateKey, items: [{ id: 'anchor', version: 'earlier' }, { id: 'last', version: 'reply' }], 'data-testid': 'lane',
    }, React.createElement('div', { 'data-item-id': 'anchor' }, 'Earlier row'), React.createElement('div', { 'data-item-id': 'last' }, 'Reply')))))
    await flush()
  }
  await render('a')
  const lane = container.querySelector('[data-testid="lane"]')
  Object.defineProperties(lane, {
    scrollHeight: { configurable: true, get: () => height },
    clientHeight: { configurable: true, get: () => 400 },
    clientWidth: { configurable: true, get: () => 800 },
  })
  lane.getBoundingClientRect = () => ({ top: 0, bottom: 400, left: 0, right: 800, width: 800, height: 400 })
  const row = lane.querySelector('[data-item-id="anchor"]')
  row.getBoundingClientRect = () => ({ top: rowOffset - lane.scrollTop, bottom: rowOffset - lane.scrollTop + 200, left: 0, right: 800, width: 800, height: 200 })
  await act(async () => {
    lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -900 }))
    lane.scrollTop = 877
    lane.dispatchEvent(new Event('scroll'))
  })
  await flush()
  assert.equal(row.getBoundingClientRect().top, 23)
  await render('b')
  // Backfill above the reader's row lands while conversation a is away.
  height += 600
  rowOffset += 600
  await render('a')
  await flush()
  assert.equal(row.getBoundingClientRect().top, 23)
  assert.equal(lane.scrollTop, 1477)
})
