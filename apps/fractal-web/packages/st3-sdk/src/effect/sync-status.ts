// Portable SyncStatus Amendment v2 contract.
/** Portable decoded observations. All timestamps are epoch milliseconds supplied by the host. */
export type SyncStage = 'queued' | 'resolving' | 'routing' | 'reading'
export type StaleReason =
  | { readonly _tag: 'Resync'; readonly code: string; readonly message: string; readonly attempt: number }
  | { readonly _tag: 'Quiet'; readonly lastFrameAt: number }
  | { readonly _tag: 'Reconnecting'; readonly attempt: number; readonly nextAt: number; readonly issue: string }
  | { readonly _tag: 'Evicted' }
  | { readonly _tag: 'Unknown' }
export type SyncFailureCause =
  | { readonly _tag: 'Server'; readonly code: string; readonly message: string }
  | { readonly _tag: 'Local'; readonly kind: string; readonly detail?: { readonly cap?: number; readonly message?: string } }
  | { readonly _tag: 'Unknown' }
export type SyncStatus =
  | { readonly _tag: 'Connecting'; readonly attempt: number; readonly since: number }
  | { readonly _tag: 'Requested'; readonly since: number }
  | { readonly _tag: 'Progress'; readonly stage: SyncStage; readonly elapsedMs: number; readonly stageSince: number; readonly reportedAt: number; readonly host?: string; readonly done?: number; readonly total?: number }
  | { readonly _tag: 'Live'; readonly since: number; readonly snapshot?: unknown }
  | { readonly _tag: 'Stale'; readonly reason: StaleReason; readonly lastLiveAt?: number }
  | { readonly _tag: 'Failed'; readonly cause: SyncFailureCause }
