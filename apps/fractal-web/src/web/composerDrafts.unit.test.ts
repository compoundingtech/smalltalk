// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from 'vitest'
import { composerDraftPrefix, createComposerDraftSession, createLocalComposerDraftStore, maxComposerDraftBytes, maxComposerDrafts, type ComposerDraft } from './composerDrafts.ts'

beforeEach(() => localStorage.clear())
const makeStore = () => createLocalComposerDraftStore({ storage: localStorage, events: window })
const record = (text: string, updatedAt: number): ComposerDraft => ({ version: 1, text, updatedAt })
const key = composerDraftPrefix + encodeURIComponent('agent/example')
const publish = (draft: ComposerDraft) => window.dispatchEvent(new StorageEvent('storage', {
  storageArea: localStorage, key, newValue: JSON.stringify(draft),
}))

describe('browser-local composer draft port', () => {
  it('restores a draft on reopening the conversation', () => {
    const first = createComposerDraftSession({ store: makeStore(), conversationId: 'agent/example', now: () => 10 })
    first.edit('Keep this unsent note')
    first.close()
    const reopened = createComposerDraftSession({ store: makeStore(), conversationId: 'agent/example' })
    expect(reopened.getSnapshot()).toEqual(record('Keep this unsent note', 10))
    reopened.close()
  })

  it.each(['local-edit', 'confirmed-send'] as const)('repairs stale physical storage after a newer %s without a local storage event', action => {
    const store = makeStore()
    store.write('agent/example', record('Old text', 10))
    const session = createComposerDraftSession({ store, conversationId: 'agent/example', now: () => 11 })
    if (action === 'local-edit') session.edit('New text')
    else session.beginSend('Old text')()
    const expected = record(action === 'local-edit' ? 'New text' : '', 11)
    localStorage.setItem(key, JSON.stringify(record('Old text', 10)))
    publish(record('Old text', 10))
    expect(store.read('agent/example')).toEqual(expected)
    const reopened = createComposerDraftSession({ store: makeStore(), conversationId: 'agent/example' })
    expect(reopened.getSnapshot()).toEqual(expected)
    reopened.close()
    session.close()
  })

  it('keeps the newer draft across two tabs and ignores delayed older events', () => {
    const left = makeStore()
    const right = makeStore()
    const session = createComposerDraftSession({ store: left, conversationId: 'agent/example' })
    right.write('agent/example', record('Newest', 20))
    publish(record('Newest', 20))
    expect(session.getSnapshot().text).toBe('Newest')
    expect(left.write('agent/example', record('Older', 10))).toMatchObject({ _tag: 'Stale' })
    publish(record('Older', 10))
    expect(session.getSnapshot().text).toBe('Newest')
    expect(right.read('agent/example')?.text).toBe('Newest')
    // Even an older physical write from a foreign producer is repaired when its event arrives.
    localStorage.setItem(key, JSON.stringify(record('Older', 10)))
    publish(record('Older', 10))
    expect(right.read('agent/example')?.text).toBe('Newest')
    session.close()
  })

  it('never replaces text typed this session with an external restore', () => {
    const session = createComposerDraftSession({ store: makeStore(), conversationId: 'agent/example', now: () => 10 })
    session.edit('Typing here')
    const remote = makeStore()
    remote.write('agent/example', record('Later elsewhere', 20))
    publish(record('Later elsewhere', 20))
    expect(session.getSnapshot().text).toBe('Typing here')
    session.edit('Typing here again')
    expect(remote.read('agent/example')).toEqual(record('Typing here again', 21))
    session.close()
  })

  it('keeps submitted text until a send is confirmed, then stores a clearing tombstone', () => {
    const store = makeStore()
    const session = createComposerDraftSession({ store, conversationId: 'agent/example', now: () => 10 })
    session.edit('Send this')
    const confirmed = session.beginSend('Send this')
    expect(store.read('agent/example')?.text).toBe('Send this')
    // A failed send does not invoke the completion callback.
    const reopened = createComposerDraftSession({ store, conversationId: 'agent/example' })
    expect(reopened.getSnapshot().text).toBe('Send this')
    reopened.close()
    confirmed()
    expect(store.read('agent/example')).toEqual(record('', 11))
    expect(store.write('agent/example', record('Send this', 10))._tag).toBe('Stale')
    expect(session.getSnapshot().text).toBe('')
    session.close()
  })

  it('does not clear text edited after dispatch, including text edited in another tab', () => {
    const store = makeStore()
    const session = createComposerDraftSession({ store, conversationId: 'agent/example', now: () => 10 })
    session.edit('First')
    const first = session.beginSend('First')
    session.edit('Next')
    first()
    expect(store.read('agent/example')?.text).toBe('Next')
    const next = session.beginSend('Next')
    store.write('agent/example', record('Another tab', 30))
    next()
    expect(store.read('agent/example')?.text).toBe('Another tab')
    session.close()
  })

  it('caps drafts and UTF-8 bytes, evicting the oldest without touching foreign keys', () => {
    const store = makeStore()
    localStorage.setItem('other-product', 'keep')
    for (let index = 0; index <= maxComposerDrafts; index += 1) {
      expect(store.write(`conversation/${index}`, record(`Note ${index}`, index))._tag).toBe('Saved')
    }
    expect(store.read('conversation/0')).toBeUndefined()
    expect(store.read(`conversation/${maxComposerDrafts}`)?.text).toBe(`Note ${maxComposerDrafts}`)
    expect(localStorage.length).toBe(maxComposerDrafts + 1)
    expect(localStorage.getItem('other-product')).toBe('keep')
    expect(store.write('too-big', record('🙂'.repeat(maxComposerDraftBytes / 4 + 1), 200))._tag).toBe('TooLarge')
    expect(store.read('too-big')).toBeUndefined()
    expect(store.write('at-limit', record('a'.repeat(maxComposerDraftBytes), 201))._tag).toBe('Saved')
  })

  it.each([
    '{broken', 'null', JSON.stringify({ text: 'Foreign', updatedAt: 10 }),
    JSON.stringify({ version: 2, text: 'Foreign', updatedAt: 10 }),
    JSON.stringify({ version: 1, text: 'Foreign', updatedAt: -1 }),
    JSON.stringify({ version: 1, text: 'Foreign', updatedAt: 1.5 }),
    JSON.stringify({ version: 1, text: 'Foreign', updatedAt: 10, unrelated: true }),
  ])('ignores corrupt or foreign records safely: %s', value => {
    localStorage.setItem(key, value)
    const store = makeStore()
    expect(store.read('agent/example')).toBeUndefined()
    const session = createComposerDraftSession({ store, conversationId: 'agent/example' })
    window.dispatchEvent(new StorageEvent('storage', { storageArea: localStorage, key, newValue: value }))
    expect(session.getSnapshot().text).toBe('')
    session.close()
  })

  it('retains in-memory edits when browser storage is unavailable', () => {
    const storage: Storage = {
      length: 0, key: () => null, clear: () => {}, removeItem: () => {},
      getItem: () => { throw new DOMException('Unavailable', 'SecurityError') },
      setItem: () => { throw new DOMException('Unavailable', 'QuotaExceededError') },
    }
    const store = createLocalComposerDraftStore({ storage, events: window })
    const session = createComposerDraftSession({ store, conversationId: 'agent/example', now: () => 10 })
    expect(session.edit('Keep in memory')._tag).toBe('Unavailable')
    expect(session.getSnapshot().text).toBe('Keep in memory')
    session.close()
  })
})
