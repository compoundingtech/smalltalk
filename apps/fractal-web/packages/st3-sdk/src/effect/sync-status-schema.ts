import { Schema } from 'effect'
import type { SyncStatus as PlainSyncStatus } from './sync-status.ts'

const Count = Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))
const Timestamp = Schema.Number.check(Schema.makeFilter(Number.isFinite))
/** Effect 4 spelling of the reviewed portable union's Effect 3 decoder. */
export const SyncStage = Schema.Literals(['queued', 'resolving', 'routing', 'reading'])
export const StaleReason = Schema.Union([
  Schema.TaggedStruct('Resync', { code: Schema.NonEmptyString, message: Schema.String, attempt: Count }),
  Schema.TaggedStruct('Quiet', { lastFrameAt: Timestamp }),
  Schema.TaggedStruct('Reconnecting', { attempt: Count, nextAt: Timestamp, issue: Schema.String }),
  Schema.TaggedStruct('Evicted', {}),
  Schema.TaggedStruct('Unknown', {}),
]).annotate({ identifier: 'St3.StaleReason' })
export const SyncFailureCause = Schema.Union([
  Schema.TaggedStruct('Server', { code: Schema.NonEmptyString, message: Schema.String }),
  Schema.TaggedStruct('Local', { kind: Schema.NonEmptyString, detail: Schema.optional(Schema.Struct({ cap: Schema.optional(Count), message: Schema.optional(Schema.String) })) }),
  Schema.TaggedStruct('Unknown', {}),
]).annotate({ identifier: 'St3.SyncFailureCause' })
export const SyncStatus = Schema.Union([
  Schema.TaggedStruct('Connecting', { attempt: Count, since: Timestamp }),
  Schema.TaggedStruct('Requested', { since: Timestamp }),
  Schema.TaggedStruct('Progress', { stage: SyncStage, elapsedMs: Count, stageSince: Timestamp, reportedAt: Timestamp, host: Schema.optional(Schema.String), done: Schema.optional(Count), total: Schema.optional(Count) }),
  Schema.TaggedStruct('Live', { since: Timestamp, snapshot: Schema.optional(Schema.Unknown) }),
  Schema.TaggedStruct('Stale', { reason: StaleReason, lastLiveAt: Schema.optional(Timestamp) }),
  Schema.TaggedStruct('Failed', { cause: SyncFailureCause }),
]).annotate({ identifier: 'St3.SyncStatus' })
export const St3 = { SyncStatus, SyncStage, StaleReason, SyncFailureCause } as const

type Equal<TA, TB> = (<T>() => T extends TA ? 1 : 2) extends (<T>() => T extends TB ? 1 : 2) ? true : false
type Assert<T extends true> = T
export type SyncStatusSchemaMatchesPlain = Assert<Equal<typeof SyncStatus.Type, PlainSyncStatus>>
