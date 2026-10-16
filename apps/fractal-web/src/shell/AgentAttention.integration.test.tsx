// @vitest-environment jsdom
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))
import { attentionAnswerFixtures } from '../data/attention.fixtures.ts'
import { fixtureSource, type FixtureProjections } from '../data/fixtureSource.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { observed } from '../data/source.ts'
import { AgentAttentionSignals } from './AgentAttention.tsx'

const world: FixtureProjections = {
  now: 0, events: [], agents: [], missions: [], attention: [], conversations: {},
  terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' },
}
it.each(attentionAnswerFixtures)('badge counts only supported open asks: $name', ({ item, waiting }) => {
  const registry = AtomRegistry.make()
  const attention = Atom.make(observed({ value: [item] })).pipe(Atom.keepAlive)
  const source = { ...fixtureSource({ world }), attention }
  const host = document.createElement('div')
  const root = createRoot(host)
  try {
    flushSync(() => root.render(
      <DataSourceProvider source={source} registry={registry}>
        <AgentAttentionSignals agentRef={item.source_id} />
      </DataSourceProvider>,
    ))
    expect(host.querySelector('[aria-label="1 waiting for answer"]') !== null).toBe(waiting)
    expect(host.querySelector('[aria-label="1 inbox items"]') !== null).toBe(item.attention_kind === 'unread-message')
    flushSync(() => registry.set(attention, observed({ value: [item], freshness: 'stale' })))
    expect(host.querySelector('[aria-label="1 waiting for answer, stale"]') !== null).toBe(waiting)
  } finally {
    flushSync(() => root.unmount())
    registry.dispose()
  }
})
