import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'

/** One origin-local document. Individual entries carry their own migration version. */
interface Entry {
  readonly version: number
  readonly value: unknown
  readonly clock: number
  readonly writer: string
}
interface Document {
  readonly version: 1
  readonly entries: Record<string, Entry>
}
/** Reads, writes, and observes the shared persistence document across storage clients. */
export interface StorageAdapter {
  read(): string | null
  write(value: string): void
  subscribe(listener: (value: string | null) => void): () => void
}
const storageKey = 'wf.ui@1'
/** Browser storage adapter with legacy developer-bar preference migration. */
export const localStorageAdapter: StorageAdapter = {
  read: () => {
    if (typeof window === 'undefined') return null
    const stored = window.localStorage.getItem(storageKey)
    if (stored !== null) return stored
    const legacy = window.localStorage.getItem('wf.devbar.visible')
    return legacy === 'true' || legacy === 'false'
      ? JSON.stringify({
          version: 1,
          entries: {
            'devbar.visible': {
              version: 1,
              value: legacy === 'true',
              clock: 0,
              writer: 'migration',
            },
          },
        })
      : null
  },
  write: (value) => {
    if (typeof window !== 'undefined') window.localStorage.setItem(storageKey, value)
  },
  subscribe: (listener) => {
    if (typeof window === 'undefined') return () => {}
    const receive = (event: StorageEvent) => {
      if (event.key === storageKey) listener(event.newValue)
    }
    window.addEventListener('storage', receive)
    return () => window.removeEventListener('storage', receive)
  },
}
const parse = (raw: string | null): Document => {
  if (raw === null) return { version: 1, entries: {} }
  const document: unknown = JSON.parse(raw)
  return Schema.decodeUnknownSync(
    Schema.Struct({
      version: Schema.Literal(1),
      entries: Schema.Record(
        Schema.String,
        Schema.Struct({
          version: Schema.Int,
          value: Schema.Unknown,
          clock: Schema.Finite,
          writer: Schema.String,
        }),
      ),
    }),
  )(document)
}
const newer = ({
  next,
  previous,
}: {
  readonly next: Entry
  readonly previous: Entry | undefined
}) =>
  previous === undefined ||
  next.clock > previous.clock ||
  (next.clock === previous.clock && next.writer > previous.writer)
/** Storage failures leave live state intact and are exposed rather than silently erasing drafts. */
export const createPersistence = ({
  adapter,
  debounceMs = 200,
}: {
  readonly adapter: StorageAdapter
  readonly debounceMs?: number
}) => {
  const writer = globalThis.crypto?.randomUUID() ?? String(Math.random())
  let entries: Record<string, Entry> = {}
  let timer: ReturnType<typeof setTimeout> | undefined
  let lastClock = 0
  let error: unknown
  const listeners = new Map<string, Set<() => void>>()
  const load = () => {
    try {
      return parse(adapter.read()).entries
    } catch (cause) {
      error = cause
      return {}
    }
  }
  entries = load()
  for (const entry of Object.values(entries)) lastClock = Math.max(lastClock, entry.clock)
  const merge = (remote: Record<string, Entry>) => {
    for (const [key, entry] of Object.entries(remote)) {
      lastClock = Math.max(lastClock, entry.clock)
      if (!newer({ next: entry, previous: entries[key] })) continue
      entries[key] = entry
      listeners.get(key)?.forEach((notify) => notify())
    }
  }
  const flush = () => {
    if (timer !== undefined) {
      clearTimeout(timer)
      timer = undefined
    }
    try {
      // Never overwrite an unreadable/newer document with an empty recovery document.
      merge(parse(adapter.read()).entries)
      adapter.write(JSON.stringify({ version: 1, entries } satisfies Document))
      error = undefined
    } catch (cause) {
      error = cause
    }
  }
  const unsubscribe = adapter.subscribe((raw) => {
    try {
      merge(parse(raw).entries)
    } catch (cause) {
      error = cause
    }
  })
  const pagehide = () => flush()
  if (typeof window !== 'undefined') window.addEventListener('pagehide', pagehide)
  return {
    atom<A, I>({
      key,
      schema,
      defaultValue,
      version = 1,
      migrate,
    }: {
      readonly key: string
      readonly schema: Schema.Codec<A, I>
      readonly defaultValue: NoInfer<A>
      readonly version?: number
      readonly migrate?: (value: unknown, fromVersion: number) => unknown
    }): Atom.Writable<A> {
      const decode = Schema.decodeUnknownSync(schema)
      const encode = Schema.encodeSync(schema)
      const read = (): A => {
        const entry = entries[key]
        if (entry === undefined) return defaultValue
        try {
          if (entry.version === version) return decode(entry.value)
          if (entry.version > version || migrate === undefined)
            throw new Error(`Unsupported ${key} schema version ${entry.version}`)
          const value = decode(migrate(entry.value, entry.version))
          write(value)
          return value
        } catch (cause) {
          error = cause
          return defaultValue
        }
      }
      const write = (value: A) => {
        const encoded = encode(value)
        lastClock = Math.max(Date.now(), lastClock + 1)
        entries[key] = { version, value: encoded, clock: lastClock, writer }
        listeners.get(key)?.forEach((notify) => notify())
        clearTimeout(timer)
        timer = setTimeout(flush, debounceMs)
      }
      return Atom.writable(
        (get) => {
          const notify = () => get.setSelf(read())
          const subscriptions = listeners.get(key) ?? new Set<() => void>()
          listeners.set(key, subscriptions)
          subscriptions.add(notify)
          get.addFinalizer(() => {
            subscriptions.delete(notify)
          })
          return read()
        },
        (ctx, value: A) => {
          write(value)
          ctx.setSelf(value)
        },
      ).pipe(Atom.keepAlive)
    },
    flush,
    get error() {
      return error
    },
    dispose: () => {
      flush()
      unsubscribe()
      if (typeof window !== 'undefined') window.removeEventListener('pagehide', pagehide)
    },
  }
}
/** Shared browser persistence boundary for application working state. */
export const persistence = createPersistence({ adapter: localStorageAdapter })
/** Creates schema-backed atoms through the shared browser persistence boundary. */
export const persistedAtom = persistence.atom
