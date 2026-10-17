import * as React from 'react'
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ExternalStoreAdapter } from '@assistant-ui/react'
import { convertConversationItem } from './embrace-converter'
import type { ConversationItem } from './embrace-data/model.ts'

/** App-owned snapshots and capabilities. No transport, synthetic state, or store is hidden here. */
export type ConversationRuntimeOptions = Omit<ExternalStoreAdapter<ConversationItem>, 'convertMessage'>
export function useConversationRuntime(options: ConversationRuntimeOptions) {
  return useExternalStoreRuntime<ConversationItem>({ ...options, convertMessage: convertConversationItem })
}
/**
 * Message ids the runtime holds after adopting the latest snapshot, or undefined before the first
 * adoption. `Transcript` uses it to tell ids the runtime adopted (its store is still publishing them)
 * from ids it never adopted. It is a store rather than state, so readers re-render only when the
 * ids they are waiting for change, never merely because an adoption happened.
 */
export interface RuntimeAdoption {
  readonly get: () => ReadonlySet<string> | undefined
  /**
   * Whether the provider's current snapshot (its `options`) reached the runtime. It answers per
   * snapshot, not per id: a host may filter entries out of the adapter, so adoption must not be
   * inferred from which ids the snapshot carries.
   */
  readonly isSnapshotAdopted: () => boolean
  readonly subscribe: (listener: () => void) => () => void
}
export const RuntimeAdoptedIds = React.createContext<RuntimeAdoption | undefined>(undefined)
function createRuntimeAdoptionStore() {
  let ids: ReadonlySet<string> | undefined
  let adopted: ConversationRuntimeOptions | undefined
  const listeners = new Set<() => void>()
  return {
    get: () => ids,
    getAdopted: () => adopted,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } },
    publish: (next: ReadonlySet<string>, snapshot: ConversationRuntimeOptions) => { ids = next; adopted = snapshot; for (const listener of listeners) listener() },
  }
}
export function EmbraceRuntimeProvider({ options, children }: { readonly options: ConversationRuntimeOptions; readonly children: React.ReactNode }) {
  const runtime = useConversationRuntime(options)
  const [store] = React.useState(createRuntimeAdoptionStore)
  // Declared after the runtime hook, so it reads the runtime after that hook's adoption effect for the same snapshot.
  React.useEffect(() => store.publish(new Set(runtime.thread.getState().messages.map(message => message.id)), options), [runtime, options, store])
  // Each render's context compares against its own snapshot, so a discarded render never marks adoption.
  const adoption = React.useMemo<RuntimeAdoption>(() => ({ get: store.get, subscribe: store.subscribe, isSnapshotAdopted: () => store.getAdopted() === options }), [store, options])
  return <AssistantRuntimeProvider runtime={runtime}><RuntimeAdoptedIds.Provider value={adoption}>{children}</RuntimeAdoptedIds.Provider></AssistantRuntimeProvider>
}
