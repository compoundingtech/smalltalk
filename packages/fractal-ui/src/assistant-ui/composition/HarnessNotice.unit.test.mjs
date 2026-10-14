import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { JSDOM } from 'jsdom'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

// Same production component and curated data as Transcript/HarnessNotices. No kit book run.
const dom = new JSDOM('<!doctype html><html><body></body></html>', { url: 'http://localhost/' })
const saved = new Map()
for (const name of ['window', 'document', 'navigator', 'HTMLElement', 'SVGElement', 'Element', 'Node', 'Document', 'DocumentFragment', 'HTMLButtonElement', 'HTMLInputElement', 'HTMLTextAreaElement', 'MutationObserver', 'Event', 'KeyboardEvent', 'MouseEvent', 'getComputedStyle']) {
  saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: name === 'getComputedStyle' ? dom.window.getComputedStyle.bind(dom.window) : dom.window[name] })
}
for (const name of ['CSS', 'ResizeObserver', 'requestAnimationFrame', 'cancelAnimationFrame', 'IS_REACT_ACT_ENVIRONMENT']) saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
globalThis.CSS = { escape: value => value }
globalThis.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} }
globalThis.requestAnimationFrame = callback => setTimeout(callback, 0)
globalThis.cancelAnimationFrame = clearTimeout
globalThis.IS_REACT_ACT_ENVIRONMENT = true
Object.defineProperty(document, 'elementFromPoint', { configurable: true, value: () => null })
const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, plugins: [stylex.vite({ useCSSLayers: false })], server: { middlewareMode: true } })
const { HarnessNotice } = await server.ssrLoadModule('/src/assistant-ui/composition/HarnessNotice.tsx')
const { harnessNoticeExamples, harnessWeightExamples } = await server.ssrLoadModule('/src/assistant-ui/composition/HarnessNotice.fixtures.ts')
const { HarnessWeightComparison } = await server.ssrLoadModule('/src/HarnessWeight.stories.tsx')
const { baselineTheme } = await server.ssrLoadModule('/src/assistant-ui/neutral-theme.ts')
const { lightTheme } = await server.ssrLoadModule('/src/assistant-ui/composition-theme.ts')
const sx = await server.ssrLoadModule('@stylexjs/stylex')
const { UnsupportedRecord } = await server.ssrLoadModule('/src/assistant-ui/composition/UnsupportedRecord.tsx')
const { Transcript } = await server.ssrLoadModule('/src/assistant-ui/composition/Transcript.tsx')
const { EmbraceRuntimeProvider, RuntimeAdoptedIds } = await server.ssrLoadModule('/src/assistant-ui/EmbraceRuntime.tsx')
const { convertConversationItem } = await server.ssrLoadModule('/src/assistant-ui/embrace-converter.ts')

const unsupported = {
  _tag: 'Unsupported', id: 'unsupported/example', harness: 'omp', recordKind: 'model_usage',
  reason: 'unrecognized-record', at: '2026-01-15T12:30:00Z',
  inspect: { text: '{\n  "token": "[redacted]",\n  "message": "capped preview…"\n}', bytes: 8192, truncated: true },
}

test('unsupported converter keeps the marker summary separate from diagnostic text', () => {
  const message = convertConversationItem(unsupported)
  assert.equal(message.role, 'system')
  assert.deepEqual(message.content, [{ type: 'text', text: 'Unsupported omp record · model_usage' }])
  assert.equal(message.metadata.custom.item, unsupported)
  assert.doesNotMatch(message.content[0].text, /redacted|capped preview/)
})

for (const scheme of ['dark', 'light']) test(`${scheme} unsupported marker discloses only its supplied redacted preview`, async () => {
  const container = document.createElement('section'); document.body.append(container)
  Object.assign(container, sx.props(...baselineTheme, scheme === 'light' && lightTheme))
  const root = createRoot(container)
  try {
    await act(async () => root.render(React.createElement(UnsupportedRecord, { item: unsupported })))
    const marker = container.querySelector('[data-testid=unsupported-record]')
    assert.equal(marker.dataset.recordKind, 'model_usage')
    assert.equal(marker.dataset.harness, 'omp')
    const row = marker.querySelector('[data-testid=unsupported-record-row]')
    assert.match(row.textContent, /Unsupported omp record · model_usage.*Inspect/)
    assert.doesNotMatch(row.textContent, /token|redacted|capped preview/)
    assert.equal(row.querySelector('time').dateTime, unsupported.at)
    const trigger = row.querySelector('button')
    assert.equal(trigger.getAttribute('aria-expanded'), 'false')
    assert.ok(marker.querySelector('[data-testid=unsupported-record-panel]').closest('[hidden]'))
    await act(async () => trigger.click())
    assert.equal(trigger.getAttribute('aria-expanded'), 'true')
    assert.equal(marker.querySelector('pre').textContent, unsupported.inspect.text)
    assert.match(marker.querySelector('p').textContent, /8,192 bytes · truncated/)
    await act(async () => trigger.click())
    assert.equal(trigger.getAttribute('aria-expanded'), 'false')
  } finally { await act(async () => root.unmount()); container.remove() }
})

for (const adopted of [true, false]) test(`Transcript renders an explicit unsupported marker when ${adopted ? 'adopted' : 'stranded'}`, async () => {
  const container = document.createElement('section'); document.body.append(container)
  const root = createRoot(container)
  const now = Date.parse(unsupported.at)
  const turn = { id: 'turn/unsupported', items: [unsupported], work: { calls: [], running: false, failed: false, interrupted: false } }
  // A host using AssistantRuntimeProvider directly has no adoption seam. Its missing runtime
  // record is stranded; an empty EmbraceRuntimeProvider source snapshot instead remains pending.
  const transcript = React.createElement(Transcript, { turns: [turn], title: 'Unsupported records', sync: { _tag: 'Live', since: now }, now, observedAt: now })
  try {
    await act(async () => root.render(React.createElement(EmbraceRuntimeProvider, {
      options: { messages: adopted ? [unsupported] : [], isRunning: false, onNew: async () => {} },
    }, adopted ? transcript : React.createElement(RuntimeAdoptedIds.Provider, { value: undefined }, transcript))))
    assert.equal(container.querySelectorAll('[data-testid=unsupported-record]').length, 1)
    assert.equal(container.querySelector('[data-testid=unsupported-record]').dataset.itemId, unsupported.id)
    assert.equal(container.querySelectorAll('[data-testid=transcript-stranded]').length, 0)
    assert.doesNotMatch(container.querySelector('[data-testid=unsupported-record-row]').textContent, /redacted|capped preview/)
    if (adopted) assert.equal(container.querySelector('[data-item-kind=Unsupported]').dataset.testid, 'transcript-message')
  } finally { await act(async () => root.unmount()); container.remove() }
})
for (const scheme of ['dark', 'light']) test(`${scheme} background jobs label each actual outcome`, async () => {
  const container = document.createElement('section'); document.body.append(container)
  Object.assign(container, sx.props(...baselineTheme, scheme === 'light' && lightTheme))
  const root = createRoot(container)
  try {
    for (const state of ['running', 'completed', 'failed', 'cancelled', 'unknown']) {
      const item = { ...harnessNoticeExamples.find(item => item.notice._tag === 'BackgroundJob'), title: `Background job ${state}` }
      item.notice = { ...item.notice, state, payload: '', raw: '' }
      await act(async () => root.render(React.createElement(HarnessNotice, { item })))
      assert.equal(container.querySelector('[data-testid=harness-kind]').textContent, state)
      assert.match(container.querySelector('[data-testid=harness-notice]').textContent, new RegExp(`Background job ${state}`))
      assert.equal(container.querySelector('button').getAttribute('aria-expanded'), 'false')
    }
  } finally { await act(async () => root.unmount()); container.remove() }
})

after(async () => { await server.close(); dom.window.close(); for (const [name, descriptor] of saved) { if (descriptor === undefined) Reflect.deleteProperty(globalThis, name); else Object.defineProperty(globalThis, name, descriptor) } })
for (const scheme of ['dark', 'light']) test(`curated ${scheme} harness cards disclose real Markdown with keyboard semantics`, async () => {
  const container = document.createElement('section'); container.dataset.scheme = scheme; document.body.append(container)
  Object.assign(container, sx.props(...baselineTheme, scheme === 'light' && lightTheme))
  const root = createRoot(container)
  try {
    await act(async () => root.render(React.createElement(React.Fragment, null, harnessNoticeExamples.map(item => React.createElement(HarnessNotice, { key: item.id, item })))))
    assert.equal(container.querySelectorAll('[data-testid=harness-notice]').length, 4)
    const buttons = [...container.querySelectorAll('button[aria-expanded]')]
    assert.equal(buttons.length, 4)
    for (const button of buttons) assert.equal(button.getAttribute('aria-expanded'), 'false')
    const summaries = [...container.querySelectorAll('[data-testid=harness-excerpt]')].map(e => e.textContent).join(' ')
    assert.doesNotMatch(summaries, /<system-notice>|<irc|## Goal|"summary"/)
    const read = buttons.find(b => b.textContent === 'Read summary')
    assert.ok(read)
    await act(async () => read.click())
    assert.equal(read.getAttribute('aria-expanded'), 'true')
    const summary = container.querySelector('[data-testid=compaction-summary]')
    assert.equal(summary.querySelector('h2').textContent, 'Goal')
    assert.equal(summary.querySelectorAll('li').length, 2)
    assert.equal(summary.querySelectorAll('table').length, 1)
    assert.equal(summary.querySelectorAll('pre code').length, 1)
    for (const button of buttons.filter(b => b !== read)) await act(async () => button.click())
    assert.equal(container.querySelectorAll('[data-testid=harness-payload]').length, 3)
    // Every expanded body stays inside its own card, and the trigger states the open state.
    const down = 'M3.5 6 8 10.5 12.5 6'
    const contained = card => { const panel = card.querySelector('[data-testid=harness-panel]'); assert.ok(panel); assert.equal(panel.parentElement.closest('[data-testid=harness-notice]'), card); assert.ok(panel.querySelector('[data-testid=compaction-summary], [data-testid=harness-payload]')) }
    const opened = button => { assert.equal(button.getAttribute('aria-expanded'), 'true'); assert.match(button.textContent, /^Hide (summary|message|result)$/); assert.equal(button.querySelector('svg path').getAttribute('d'), down) }
    for (const card of container.querySelectorAll('[data-testid=harness-notice]')) { contained(card); opened(card.querySelector('[data-testid=harness-trigger]')) }
    assert.deepEqual(buttons.map(b => b.textContent).sort(), ['Hide message', 'Hide result', 'Hide result', 'Hide summary'])
    const card = read.closest('[data-testid=harness-notice]'); const panel = card.querySelector('[data-testid=harness-panel]'); const anchor = document.createComment('panel')
    panel.replaceWith(anchor); container.append(panel)
    assert.throws(() => contained(card)); anchor.replaceWith(panel)
    const path = read.querySelector('svg path'); path.setAttribute('d', 'M6 3.5 10.5 8 6 12.5')
    assert.throws(() => opened(read)); path.setAttribute('d', down)
    // Failing controls exercise both collapsed transport and Markdown-heading assertions.
    const excerpt = container.querySelector('[data-testid=harness-excerpt]')
    const text = excerpt.textContent; excerpt.textContent = '<system-notice>control</system-notice>'
    assert.throws(() => assert.doesNotMatch(excerpt.textContent, /<system-notice>|<irc|## Goal/)); excerpt.textContent = text
    const heading = summary.querySelector('h2'); const replacement = document.createElement('span'); replacement.textContent = '## Goal'; heading.replaceWith(replacement)
    assert.throws(() => assert.equal(summary.querySelector('h2')?.textContent, 'Goal')); replacement.replaceWith(heading)
    await act(async () => read.click()); assert.equal(read.getAttribute('aria-expanded'), 'false'); assert.equal(read.textContent, 'Read summary'); assert.notEqual(read.querySelector('svg path').getAttribute('d'), down)
  } finally { await act(async () => root.unmount()); container.remove() }
})

// q106: the Fractal/Explore comparison renders the production component at both weights.
for (const scheme of ['dark', 'light']) test(`q106 ${scheme} comparison renders all specimens at row and card weight without transport syntax`, async () => {
  const container = document.createElement('div'); document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(React.createElement(HarnessWeightComparison, { scheme })))
    assert.equal(container.querySelector('[data-scheme]').dataset.scheme, scheme)
    const kinds = [...new Set(harnessWeightExamples.map(item => item.notice._tag))].sort()
    assert.deepEqual(kinds, ['BackgroundJob', 'Compaction', 'Delivery', 'Interjection', 'Irc', 'SkillInvocation', 'SubagentResult'])
    assert.equal(harnessWeightExamples.length, 9)
    const transport = /<\/?(system-notice|irc|task-result|skill|output)\b|\[omp compaction\]|## Goal|"overall_correctness"/
    const collapsedText = column => { const copy = column.cloneNode(true); for (const hidden of copy.querySelectorAll('[hidden]')) hidden.remove(); return copy.textContent }
    const classes = {}
    for (const weight of ['row', 'card']) {
      const column = container.querySelector(`[data-testid=harness-weight-${weight}]`)
      const notices = [...column.querySelectorAll('[data-testid=harness-notice]')]
      assert.deepEqual(notices.map(n => n.dataset.itemId), harnessWeightExamples.map(item => item.id))
      for (const notice of notices) assert.equal(notice.dataset.harnessWeight, weight)
      assert.doesNotMatch(collapsedText(column), transport)
      for (const button of column.querySelectorAll('[data-testid=harness-trigger]')) assert.equal(button.getAttribute('aria-expanded'), 'false')
      for (const panel of column.querySelectorAll('[data-testid=harness-panel]')) assert.ok(panel.closest('[hidden]'), 'collapsed panel is hidden')
      assert.ok(column.querySelector('[data-item-id="weight/interjection"]').textContent.includes('Interjection'))
      assert.ok(column.querySelector('[data-item-id="weight/result"]').textContent.includes('completed'))
      const failedJob = column.querySelector('[data-item-id="weight/failed-job"] [data-testid=harness-kind]')
      const completedJob = column.querySelector('[data-item-id="weight/job"] [data-testid=harness-kind]')
      assert.equal(failedJob.textContent, 'failed')
      assert.notEqual(failedJob.className, completedJob.className)
      classes[weight] = notices[0].className
    }
    assert.notEqual(classes.row, classes.card)
    // Failing controls: a leaked transport excerpt and an ignored weight prop must both fail.
    const excerpt = container.querySelector('[data-testid=harness-weight-row] [data-testid=harness-excerpt]'); const text = excerpt.textContent
    excerpt.textContent = '<system-notice>control</system-notice>'
    assert.throws(() => assert.doesNotMatch(collapsedText(container.querySelector('[data-testid=harness-weight-row]')), transport)); excerpt.textContent = text
    assert.throws(() => assert.notEqual(classes.card, classes.card))
    const summary = [...container.querySelectorAll('[data-testid=harness-weight-row] [data-testid=harness-trigger]')].find(b => b.textContent === 'Read summary')
    await act(async () => summary.click())
    const card = summary.closest('[data-testid=harness-notice]')
    assert.equal(card.querySelector('[data-testid=compaction-summary] h2').textContent, 'Goal')
    assert.equal(summary.textContent, 'Hide summary')
  } finally { await act(async () => root.unmount()); container.remove() }
})
