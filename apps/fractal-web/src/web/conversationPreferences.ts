import { Schema } from 'effect'
import { persistedAtom } from '../state/persistence.ts'

/** One global display preference; no connection or agent identity enters its storage key. */
export const systemEventsPreference = persistedAtom({
  key: 'conversation.systemEvents',
  schema: Schema.Boolean,
  defaultValue: false,
})
