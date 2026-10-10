import { Schema } from 'effect'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import type * as Atom from 'effect/reactivity/Atom'
import { persistedAtom } from '../state/persistence.ts'

// Stable's per-recipient history key and segment shape; selection and undo stay ephemeral.
const segments = Schema.Array(Schema.Struct({ type: Schema.Literals(['text', 'token']), text: Schema.String, value: Schema.optionalKey(Schema.Unknown) }))
const retained = new Map<string, HistoryAtom>()
type HistorySegments = readonly (readonly { readonly type: 'text' | 'token'; readonly text: string; readonly value?: unknown }[])[]
type HistoryAtom = Atom.Writable<HistorySegments>
export const composerHistoryAtom = (namespace: string) => {
  const existing = retained.get(namespace)
  if (existing !== undefined) return existing
  const atom = persistedAtom({ key: `${namespace}:composer.history`, schema: Schema.Array(segments), defaultValue: [] })
  retained.set(namespace, atom)
  return atom
}
export const submittedHistory = (namespace: string, registry: AtomRegistry.AtomRegistry) => {
  const atom = composerHistoryAtom(namespace)
  return {
    get: () => registry.get(atom).map(draft => draft.map(segment => segment.text).join('')),
    capture: (content: string) => (outcome: { readonly _tag: 'Confirmed' } | { readonly _tag: 'Unconfirmed' }) => {
      if (outcome._tag === 'Confirmed') registry.set(atom, [...registry.get(atom), [{ type: 'text' as const, text: content }]].slice(-100))
    },
  }
}
