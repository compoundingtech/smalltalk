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
 * from ids it never adopted.
 */
export const RuntimeAdoptedIds = React.createContext<ReadonlySet<string> | undefined>(undefined)
export function EmbraceRuntimeProvider({ options, children }: { readonly options: ConversationRuntimeOptions; readonly children: React.ReactNode }) {
  const runtime = useConversationRuntime(options)
  const [adopted, setAdopted] = React.useState<ReadonlySet<string>>()
  // Declared after the runtime hook, so it reads the runtime after that hook's adoption effect for the same snapshot.
  React.useEffect(() => setAdopted(new Set(runtime.thread.getState().messages.map(message => message.id))), [runtime, options])
  return <AssistantRuntimeProvider runtime={runtime}><RuntimeAdoptedIds.Provider value={adopted}>{children}</RuntimeAdoptedIds.Provider></AssistantRuntimeProvider>
}
