import type { Snapshot, TerminalAttachment } from '@smalltalk/st3-client'
import type { Selector } from './slice.ts'

/** Wire boundaries point into slice state/events, never contain response items. */
export type RecordedFrame = {
  readonly ordinal: number
  readonly at_ms: number
  readonly selector: Selector
} & (
  | { readonly kind: 'conversation'; readonly event?: number; readonly replace: boolean; readonly has_more?: boolean }
  | { readonly kind: 'screen'; readonly event?: number; readonly snapshot: Snapshot }
  | { readonly kind: 'snapshot'; readonly snapshot: Snapshot; readonly item_ids: readonly string[]; readonly order: readonly string[]; readonly has_more: boolean }
  | { readonly kind: 'error'; readonly code?: string; readonly message: string; readonly retryable?: boolean }
)

export interface RecordedHttp {
  readonly ordinal: number
  readonly at_ms: number
  readonly method: string
  readonly path: string
  readonly query: string
  readonly status: number
  readonly request_id: string
  readonly snapshot: Snapshot
  readonly value:
    | { readonly kind: 'screen'; readonly terminal: string }
    | { readonly kind: 'timeline'; readonly agent: string; readonly items: readonly { readonly id: string; readonly blocks: boolean }[]; readonly page: { readonly limit: number; readonly has_more: boolean; readonly next_cursor?: string } }
    | { readonly kind: 'agents'; readonly items?: readonly string[]; readonly page: { readonly limit: number; readonly has_more: boolean; readonly next_cursor?: string }; readonly filters: Readonly<Record<string, string>> }
    | { readonly kind: 'attach'; readonly action_id: string; readonly operation_id: string; readonly snapshot_id: string; readonly attachment: TerminalAttachment }
}

export interface RecordedTransport {
  readonly frames: readonly RecordedFrame[]
  readonly http: readonly RecordedHttp[]
}
