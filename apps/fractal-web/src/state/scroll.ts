import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'

import { persistedAtom } from './persistence.ts'

/** Persisted following mode or a reading anchor tied to a stable conversation entry. */
export const ScrollAnchor = Schema.Union([
  Schema.TaggedStruct('Following', {}),
  Schema.TaggedStruct('Reading', {
    entry: Schema.NullOr(Schema.String),
    offset: Schema.Finite,
    scrollTop: Schema.Finite,
  }),
])
export type ScrollAnchor = typeof ScrollAnchor.Type
/** Following is a mode, not a fragile numeric bottom position. Reading uses stable entry identity. */
export const conversationAnchorAtom = ({
  key,
  persist = true,
}: {
  readonly key: string
  readonly persist?: boolean
}) =>
  persist
    ? persistedAtom({
        key: `${key}:conversation.anchor`,
        schema: ScrollAnchor,
        defaultValue: { _tag: 'Following' },
      })
    : Atom.make<ScrollAnchor>({ _tag: 'Following' })
