import { Schema } from 'effect'

/** Drafts are an explicit browser-local exception; no other conversation state is stored here. */
export const composerDraftPrefix = 'fractal-web.composer-draft.v1/'
export const maxComposerDrafts = 100
export const maxComposerDraftBytes = 64 * 1024
const utf8 = new TextEncoder()
const DraftRecord = Schema.Struct({
  version: Schema.Literal(1),
  text: Schema.String.check(Schema.makeFilter((text) => utf8.encode(text).length <= maxComposerDraftBytes)),
  updatedAt: Schema.Int.check(Schema.isBetween({ minimum: 0, maximum: Number.MAX_SAFE_INTEGER })),
})
const DraftJson = Schema.fromJsonString(DraftRecord)
const decode = Schema.decodeUnknownSync(DraftJson, { onExcessProperty: 'error' })
const encode = Schema.encodeSync(DraftJson)
export type ComposerDraft = typeof DraftRecord.Type
export type DraftWriteResult = { readonly _tag: 'Saved'; readonly draft: ComposerDraft }
  | { readonly _tag: 'Stale'; readonly draft: ComposerDraft }
  | { readonly _tag: 'TooLarge' } | { readonly _tag: 'Unavailable' }

/** A future person-scoped store replaces this port without owning the composer's editing state. */
export interface ComposerDraftStore {
  readonly read: (conversationId: string) => ComposerDraft | undefined
  readonly write: (conversationId: string, draft: ComposerDraft) => DraftWriteResult
  readonly subscribe: (conversationId: string, listener: (draft: ComposerDraft) => void) => () => void
}

const decodeDraft = (value: string | null): ComposerDraft | undefined => {
  if (value === null || value.length > maxComposerDraftBytes * 6 + 128) return undefined
  try { return decode(value) } catch { return undefined }
}
const sameDraft = (left: ComposerDraft, right: ComposerDraft) =>
  left.updatedAt === right.updatedAt && left.text === right.text

/** Reads/writes are synchronous, and every write rechecks the shared value, not a tab's snapshot. */
export const createLocalComposerDraftStore = ({ storage, events }: {
  readonly storage: Storage
  readonly events: Pick<Window, 'addEventListener' | 'removeEventListener'>
}): ComposerDraftStore => {
  const keyFor = (id: string) => composerDraftPrefix + encodeURIComponent(id)
  // Browser storage events never fire in the writing tab. Keep each active subscription's
  // high-water record current on successful local writes as well as received events.
  const subscriptions = new Set<{ id: string; latest: ComposerDraft | undefined }>()
  const read: ComposerDraftStore['read'] = (id) => {
    try { return decodeDraft(storage.getItem(keyFor(id))) } catch { return undefined }
  }
  const trim = () => {
    const entries: { key: string; draft: ComposerDraft }[] = []
    const invalid: string[] = []
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index)
      if (key === null || !key.startsWith(composerDraftPrefix)) continue
      const draft = decodeDraft(storage.getItem(key))
      if (draft === undefined) invalid.push(key)
      else entries.push({ key, draft })
    }
    // Only this port's namespace is disposable. Foreign storage keys are never removed.
    for (const key of invalid) storage.removeItem(key)
    entries.sort((left, right) => left.draft.updatedAt - right.draft.updatedAt || left.key.localeCompare(right.key))
    for (const entry of entries.slice(0, Math.max(0, entries.length - maxComposerDrafts))) storage.removeItem(entry.key)
  }
  const write: ComposerDraftStore['write'] = (id, draft) => {
    if (utf8.encode(draft.text).length > maxComposerDraftBytes) return { _tag: 'TooLarge' }
    try {
      // Validate writes too: an invalid timestamp must not poison all future revisions.
      const value = encode(draft)
      let current = read(id)
      for (const subscription of subscriptions) {
        if (subscription.id === id && subscription.latest !== undefined
          && (current === undefined || subscription.latest.updatedAt > current.updatedAt)) current = subscription.latest
      }
      if (current !== undefined && current.updatedAt >= draft.updatedAt && !sameDraft(current, draft)) {
        return { _tag: 'Stale', draft: current }
      }
      storage.setItem(keyFor(id), value)
      for (const subscription of subscriptions) {
        if (subscription.id === id) subscription.latest = draft
      }
      trim()
      return { _tag: 'Saved', draft }
    } catch { return { _tag: 'Unavailable' } }
  }
  return {
    read, write,
    subscribe: (id, listener) => {
      const subscription = { id, latest: read(id) }
      subscriptions.add(subscription)
      const receive = (event: StorageEvent) => {
        if (event.storageArea !== storage || event.key !== keyFor(id)) return
        const incoming = decodeDraft(event.newValue)
        if (incoming === undefined) return
        const current = read(id)
        const latest = subscription.latest
        const candidates = [latest, current, incoming].filter((draft): draft is ComposerDraft => draft !== undefined)
        const winner = candidates.reduce((left, right) => right.updatedAt > left.updatedAt ? right : left)
        // A delayed event never replaces a later write; repair an older physical value when observed.
        if (current !== undefined && winner.updatedAt > current.updatedAt) write(id, winner)
        if (latest !== undefined && (winner.updatedAt < latest.updatedAt || sameDraft(winner, latest))) return
        subscription.latest = winner
        listener(winner)
      }
      events.addEventListener('storage', receive)
      return () => { events.removeEventListener('storage', receive); subscriptions.delete(subscription) }
    },
  }
}

/** Thin controlled-composer seam: external restores only update a session before its first edit. */
export const createComposerDraftSession = ({ store, conversationId, now = Date.now }: {
  readonly store: ComposerDraftStore
  readonly conversationId: string
  readonly now?: () => number
}): ComposerDraftSession => {
  let draft = store.read(conversationId) ?? { version: 1 as const, text: '', updatedAt: 0 }
  let edited = false
  let highWater = draft.updatedAt
  const listeners = new Set<() => void>()
  const notify = () => { for (const listener of listeners) listener() }
  const stop = store.subscribe(conversationId, (restored) => {
    highWater = Math.max(highWater, restored.updatedAt)
    if (edited || restored.updatedAt <= draft.updatedAt) return
    draft = restored
    notify()
  })
  const nextRevision = () => Math.min(Number.MAX_SAFE_INTEGER, Math.max(now(), highWater + 1))
  const persist = (text: string) => {
    const next = { version: 1 as const, text, updatedAt: nextRevision() }
    const result = store.write(conversationId, next)
    highWater = Math.max(next.updatedAt, result._tag === 'Stale' ? result.draft.updatedAt : 0)
    // Rejected persistence must never discard an edit in the current session.
    draft = next
    notify()
    return result
  }
  return {
    getSnapshot: () => draft,
    subscribe: (listener) => { listeners.add(listener); return () => { listeners.delete(listener) } },
    edit: (text) => { edited = true; return persist(text) },
    beginSend: (content) => {
      const submitted = draft
      return () => {
        if (submitted.text !== content || !sameDraft(submitted, draft)) return
        const current = store.read(conversationId)
        // A confirmed old send must not clear a newer draft authored in another tab.
        if (current !== undefined && !sameDraft(current, submitted)) return
        // Empty records are bounded tombstones: an older tab cannot resurrect the sent text.
        edited = true
        persist('')
      }
    },
    close: () => { stop(); listeners.clear() },
  }
}

export interface ComposerDraftSession {
  readonly getSnapshot: () => ComposerDraft
  readonly subscribe: (listener: () => void) => () => void
  /** Call for user edits only, never the runtime's optimistic reset on submit. */
  readonly edit: (text: string) => DraftWriteResult
  /** Capture at dispatch; invoke the returned function only after server-confirmed completion. */
  readonly beginSend: (content: string) => () => void
  readonly close: () => void
}
