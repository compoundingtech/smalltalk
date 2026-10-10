import { Agent, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { createRoot } from 'react-dom/client'

import { fixtureSource } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { type Feed, observed, waiting } from '../data/source.ts'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

// Executable browser fixture: scripts/shell-geometry-proof.mjs asserts body[data-test-result=pass].
// The real shell renders a long synthetic roster that arrives after first paint, as live data does.
const now = Date.parse('2026-10-08T12:00:00.000Z')
const dayAgo = new Date(now - 86_400_000).toISOString()
const agents = Array.from({ length: 216 }, (_, index) => decodeUnknownSync(Agent, 'strict')({
  kind: 'agent',
  id: `agent/example/seat-${index}`,
  name: `Synthetic seat ${index} with a long descriptive title`,
  host_id: `host/example-${index % 3}`,
  runtime_ids: [],
  reachability: 'reachable',
  // Every third seat is suspended a day ago: its observed status boundary renders elapsed time.
  state: index % 3 === 0 ? 'suspended' : 'running',
  harness_state: 'idle',
  blocked_on: null,
  fault: null,
  revision: '1',
  updated_at: dayAgo,
  ...(index % 3 === 0 ? { suspension: { action: 'suspend', blocking: [], operation_id: `operation/${index}`, phase: 'suspended', suspended_at: dayAgo, updated_at: dayAgo } } : {}),
} satisfies AgentEncoded))

const registry = AtomRegistry.make()
const roster = Atom.make<Feed<readonly Agent[]>>(waiting).pipe(Atom.keepAlive)
const source = {
  ...fixtureSource({ world: { now, events: [], agents, missions: [], attention: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }),
  agents: roster,
}
// Two animation frames: the committed render has been laid out and painted.
const afterPaint = () => {
  const { promise, resolve } = Promise.withResolvers<void>()
  requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
  return promise
}
const failures: string[] = []
const expect = (condition: boolean, message: string) => { if (!condition) failures.push(message) }
const measure = (phase: string) => {
  const shell = document.querySelector('[data-testid="live-agent-workspace"]')!.getBoundingClientRect()
  const roster = document.querySelector('nav[aria-label="Agent roster"]')!
  const facts = {
    phase, innerHeight, scrollHeight: document.documentElement.scrollHeight, bodyScrollHeight: document.body.scrollHeight,
    shellHeight: shell.height, rows: roster.querySelectorAll('[data-testid="taste-agent-row"]').length,
    rosterClientHeight: roster.clientHeight, rosterScrollHeight: roster.scrollHeight,
    breadcrumb: document.querySelector('nav[aria-label="Breadcrumb"]')!.textContent,
  }
  expect(facts.shellHeight === innerHeight, `${phase}: shell height ${facts.shellHeight} != viewport ${innerHeight}`)
  expect(facts.scrollHeight === innerHeight, `${phase}: document scrollHeight ${facts.scrollHeight} != viewport ${innerHeight}`)
  return facts
}

const root = createRoot(document.getElementById('root')!)
try {
  localStorage.clear()
  root.render(<DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>)
  await afterPaint()
  const waitingFacts = measure('waiting')
  // No agent is observed yet: the header names no folder rather than a placeholder.
  expect(waitingFacts.breadcrumb === 'Select an agent', `waiting: breadcrumb is ${JSON.stringify(waitingFacts.breadcrumb)}`)
  registry.set(roster, observed({ value: agents }))
  await afterPaint()
  const populated = measure('populated')
  expect(populated.rows === agents.length, `populated: ${populated.rows} rows rendered`)
  expect(populated.rosterScrollHeight > populated.rosterClientHeight, 'populated: the roster pane does not own the overflow')
  expect(populated.breadcrumb === 'example-0/Synthetic seat 0 with a long descriptive title', `populated: breadcrumb is ${JSON.stringify(populated.breadcrumb)}`)
  const overlaps: string[] = []
  for (const row of document.querySelectorAll('[data-testid="taste-agent-row"]')) {
    const title = row.querySelector('[data-row-column="title-text"]')!.getBoundingClientRect()
    for (const node of row.querySelectorAll('[data-row-column="status"] *, [data-row-trailing-signals] *, [data-row-column="time"] *')) {
      const box = node.getBoundingClientRect()
      if (box.width > 0 && box.height > 0 && box.left < title.right && box.right > title.left && box.top < title.bottom && box.bottom > title.top)
        overlaps.push(`${row.querySelector('[data-row-column="title-text"]')!.textContent}: ${node.tagName} ${node.textContent} at x=${box.left}`)
    }
  }
  expect(overlaps.length === 0, `row signals overlap titles: ${overlaps.slice(0, 3).join('; ')}`)
  expect(document.querySelectorAll('[data-row-column="status"] time').length === 0, 'icon-only status glyph renders visible elapsed text')
  document.body.dataset.proof = JSON.stringify({ waiting: waitingFacts, populated, overlaps: overlaps.length, failures })
  document.body.dataset.testResult = failures.length === 0 ? 'pass' : 'fail'
} catch (error) {
  document.body.dataset.proof = String(error instanceof Error ? error.stack : error)
  document.body.dataset.testResult = 'fail'
} finally {
  root.unmount()
}
