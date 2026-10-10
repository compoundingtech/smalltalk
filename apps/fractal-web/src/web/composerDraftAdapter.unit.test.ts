// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from 'vitest'
import { composerDraftPrefix, createLocalComposerDraftStore, type ComposerDraftStore } from './composerDrafts.ts'
import { createComposerDraftAdapter, type ComposerDraftAdapter, type ComposerDraftSnapshot, type EmbraceComposerHandle } from './composerDraftAdapter.ts'

beforeEach(() => localStorage.clear())
const conversationId = 'agent/example'
const storeForTab = () => createLocalComposerDraftStore({ storage: localStorage, events: window })

/** An actual revision-checked handle model, not a DOM or runtime replacement. */
const kitHandle = () => {
  let snapshot: ComposerDraftSnapshot = { text: '', revision: 0, savedAt: 0, cause: 'user' }
  let onChange: ComposerDraftAdapter['onDraftChange'] = () => {}
  const handle: EmbraceComposerHandle = {
    getDraft: () => snapshot,
    restoreDraft: (draft) => {
      if (draft.expectedRevision !== snapshot.revision || draft.savedAt <= snapshot.savedAt) return false
      snapshot = { text: draft.text, revision: snapshot.revision + 1, savedAt: draft.savedAt, cause: 'restore' }
      onChange(snapshot)
      return true
    },
  }
  return {
    handle,
    connect: (adapter: ComposerDraftAdapter) => { onChange = adapter.onDraftChange },
    change: (text: string, savedAt: number, cause: ComposerDraftSnapshot['cause'] = 'user') => {
      snapshot = { text, savedAt, revision: snapshot.revision + 1, cause }
      onChange(snapshot)
    },
  }
}

describe('kit draft restore adapter', () => {
  it('restores on reopen without treating its own restore as a new edit', () => {
    const firstKit = kitHandle()
    const first = createComposerDraftAdapter({ store: storeForTab(), conversationId, getHandle: () => firstKit.handle, now: () => 10 })
    firstKit.connect(first)
    firstKit.change('Reopen this note', 10)
    first.close()
    const nextKit = kitHandle()
    const store = storeForTab()
    const reopened = createComposerDraftAdapter({ store, conversationId, getHandle: () => nextKit.handle })
    nextKit.connect(reopened)
    expect(reopened.restore()).toBe(true)
    expect(nextKit.handle.getDraft().text).toBe('Reopen this note')
    expect(store.read(conversationId)?.updatedAt).toBe(10)
    reopened.close()
  })

  it('captures the revision before reading and refuses a restore when text changes during that read', () => {
    const storage = storeForTab()
    storage.write(conversationId, { version: 1, text: 'Saved before open', updatedAt: 20 })
    const kit = kitHandle()
    let changed = false
    const store: ComposerDraftStore = {
      ...storage,
      read: (id) => {
        if (!changed) { changed = true; kit.change('Typed while loading', 10) }
        return storage.read(id)
      },
    }
    const adapter = createComposerDraftAdapter({ store, conversationId, getHandle: () => kit.handle })
    expect(adapter.restore()).toBe(false)
    expect(kit.handle.getDraft().text).toBe('Typed while loading')
    adapter.close()
  })

  it('applies newer storage events only before local typing', () => {
    const store = storeForTab()
    const kit = kitHandle()
    const adapter = createComposerDraftAdapter({ store, conversationId, getHandle: () => kit.handle, now: () => 10 })
    kit.connect(adapter)
    adapter.restore()
    const remote = storeForTab()
    const publish = (text: string, updatedAt: number) => {
      const draft = { version: 1 as const, text, updatedAt }
      remote.write(conversationId, draft)
      window.dispatchEvent(new StorageEvent('storage', {
        storageArea: localStorage, key: composerDraftPrefix + encodeURIComponent(conversationId), newValue: JSON.stringify(draft),
      }))
    }
    publish('Other tab', 20)
    expect(kit.handle.getDraft().text).toBe('Other tab')
    kit.change('Here now', 21)
    publish('Other tab later', 30)
    expect(kit.handle.getDraft().text).toBe('Here now')
    adapter.close()
  })

  it.each(['Confirmed', 'Unconfirmed'] as const)('ignores the optimistic clear until the %s outcome', outcome => {
    const store = storeForTab()
    const kit = kitHandle()
    const adapter = createComposerDraftAdapter({ store, conversationId, getHandle: () => kit.handle, now: () => 10 })
    kit.connect(adapter)
    kit.change('Sent note', 10)
    // Real runtime ordering: optimistic clear notifies before source dispatch.
    kit.change('', 11, 'submit-reset')
    expect(store.read(conversationId)?.text).toBe('Sent note')
    const finish = adapter.onSending('Sent note')
    finish({ _tag: outcome })
    expect(store.read(conversationId)?.text).toBe(outcome === 'Confirmed' ? '' : 'Sent note')
    finish({ _tag: 'Confirmed' })
    expect(store.read(conversationId)?.text).toBe(outcome === 'Confirmed' ? '' : 'Sent note')
    if (outcome === 'Unconfirmed') {
      const savedAt = store.read(conversationId)?.updatedAt
      kit.change('Sent note', 12, 'send-failed-restore')
      expect(store.read(conversationId)?.updatedAt).toBe(savedAt)
    }
    adapter.close()
  })

  it('keeps the next nonempty draft typed while an earlier send is in flight', () => {
    const store = storeForTab()
    const kit = kitHandle()
    const adapter = createComposerDraftAdapter({ store, conversationId, getHandle: () => kit.handle, now: () => 10 })
    kit.connect(adapter)
    kit.change('Sent note', 10)
    const finish = adapter.onSending('Sent note')
    kit.change('', 11, 'submit-reset')
    kit.change('Next note', 12)
    finish({ _tag: 'Confirmed' })
    expect(store.read(conversationId)?.text).toBe('Next note')
    kit.change('', 13, 'user')
    expect(store.read(conversationId)?.text).toBe('')
    adapter.close()
  })
})
