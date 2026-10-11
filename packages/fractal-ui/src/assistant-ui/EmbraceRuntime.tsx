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
  /** Source ids whose snapshot actually reached the runtime, not merely the host render. */
  readonly getSourceIds: () => ReadonlySet<string> | undefined
  readonly subscribe: (listener: () => void) => () => void
}
export const RuntimeAdoptedIds = React.createContext<RuntimeAdoption | undefined>(undefined)
function createRuntimeAdoption() {
  let ids: ReadonlySet<string> | undefined
  let sourceIds: ReadonlySet<string> | undefined
  const listeners = new Set<() => void>()
  return {
    get: () => ids,
    getSourceIds: () => sourceIds,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } },
    publish: (next: ReadonlySet<string>, source: ReadonlySet<string>) => { ids = next; sourceIds = source; for (const listener of listeners) listener() },
  }
}
export function EmbraceRuntimeProvider({ options, children }: { readonly options: ConversationRuntimeOptions; readonly children: React.ReactNode }) {
  const runtime = useConversationRuntime(options)
  const [adoption] = React.useState(createRuntimeAdoption)
  // Declared after the runtime hook, so it reads the runtime after that hook's adoption effect for the same snapshot.
  React.useEffect(() => adoption.publish(
    new Set(runtime.thread.getState().messages.map(message => message.id)),
    new Set(options.messages?.map(message => message.id)),
  ), [runtime, options, adoption])
  return <AssistantRuntimeProvider runtime={runtime}><RuntimeAdoptedIds.Provider value={adoption}>{children}</RuntimeAdoptedIds.Provider></AssistantRuntimeProvider>
}
