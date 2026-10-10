import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { expect, it } from 'vitest'
import { composerHistoryAtom, submittedHistory } from './composerHistory.ts'
it('records only confirmed submissions and isolates each recipient', () => {
  const registry = AtomRegistry.make()
  try {
    const first = submittedHistory('history-test:agent/first', registry)
    const second = submittedHistory('history-test:agent/second', registry)
    const settle = first.capture('Own draft')
    expect(first.get()).toEqual([])
    settle({ _tag: 'Unconfirmed' })
    expect(first.get()).toEqual([])
    first.capture('Delivered own draft')({ _tag: 'Confirmed' })
    expect(first.get()).toEqual(['Delivered own draft'])
    expect(second.get()).toEqual([])
    expect(registry.get(composerHistoryAtom('history-test:agent/first'))).toEqual([[{ type: 'text', text: 'Delivered own draft' }]])
  } finally { registry.dispose() }
})
it('reads stable persisted token segments lazily as plain composer history', () => {
  const registry = AtomRegistry.make()
  try {
    const namespace = 'history-test:agent/structured'
    registry.set(composerHistoryAtom(namespace), [[{ type: 'token', text: '@review', value: { _tag: 'Mention', ref: 'agent/review' } }, { type: 'text', text: ' Check this.' }]])
    expect(submittedHistory(namespace, registry).get()).toEqual(['@review Check this.'])
  } finally { registry.dispose() }
})
