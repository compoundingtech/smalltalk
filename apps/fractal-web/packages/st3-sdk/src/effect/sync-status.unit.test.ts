import { describe, expect, it } from '@effect/vitest'
import { Effect, Schema } from 'effect'

import {
  AttachFailure, DecodeFailure, type FollowFreshness, Rejected, SubscriptionLimit,
  type SyncStatus, SyncStatusSchema, syncStatusFromFailure, syncStatusFromFreshness,
} from './mod.ts'

const statuses = [
  { _tag: 'Connecting', attempt: 0, since: 1 },
  { _tag: 'Requested', since: 2 },
  ...(['queued', 'resolving', 'routing', 'reading'] as const).map((stage) => ({
    _tag: 'Progress' as const, stage, elapsedMs: 0, stageSince: 3, reportedAt: 4,
    host: 'host/build-a', done: 0, total: 2,
  })),
  { _tag: 'Live', since: 5 },
  { _tag: 'Live', since: 5, snapshot: { arbitrary: ['opaque', 1] } },
  { _tag: 'Stale', reason: { _tag: 'Resync', code: 'cursor-gap', message: '', attempt: 0 }, lastLiveAt: 5 },
  { _tag: 'Stale', reason: { _tag: 'Quiet', lastFrameAt: 5 } },
  { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 0, nextAt: 6, issue: '' } },
  { _tag: 'Stale', reason: { _tag: 'Evicted' } },
  { _tag: 'Stale', reason: { _tag: 'Unknown' } },
  { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: '' } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit' } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: {} } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 0, message: '' } } },
  { _tag: 'Failed', cause: { _tag: 'Unknown' } },
] satisfies readonly SyncStatus[]

const invalid = [
  { _tag: 'Connecting', attempt: -1, since: 0 },
  { _tag: 'Connecting', attempt: 0.5, since: 0 },
  { _tag: 'Requested', since: Infinity },
  { _tag: 'Requested', since: NaN },
  { _tag: 'Requested', since: '2026-10-08T00:00:00Z' },
  { _tag: 'Progress', stage: 'ready', elapsedMs: 0, stageSince: 0, reportedAt: 0 },
  { _tag: 'Progress', stage: 'reading', elapsedMs: -1, stageSince: 0, reportedAt: 0 },
  { _tag: 'Progress', stage: 'reading', elapsedMs: 0, stageSince: 0, reportedAt: 0, done: 0.5 },
  { _tag: 'Progress', stage: 'reading', elapsedMs: 0, stageSince: 0, reportedAt: 0, total: -1 },
  { _tag: 'Live', since: -Infinity },
  { _tag: 'Stale', reason: { _tag: 'Resync', code: '', message: '', attempt: 0 } },
  { _tag: 'Stale', reason: { _tag: 'Resync', code: 'gap', attempt: 0 } },
  { _tag: 'Stale', reason: { _tag: 'Quiet', lastFrameAt: Infinity } },
  { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, issue: 'offline' } },
  { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: NaN },
  { _tag: 'Failed', cause: { _tag: 'Server', code: '', message: 'refused' } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: '' } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: -1 } } },
  { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 1.5 } } },
  { _tag: 'Ready' },
]

const Count = Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))
const Timestamp = Schema.Number.check(Schema.makeFilter(Number.isFinite))
const freshnessSchemas = [
  Schema.TaggedStruct('Requested', { since: Timestamp }),
  Schema.TaggedStruct('Live', { since: Timestamp }),
  Schema.TaggedStruct('Reconnecting', { attempt: Count, issue: Schema.String, nextAt: Schema.optional(Timestamp) }),
  Schema.TaggedStruct('Stale', { reason: Schema.TaggedStruct('Resync', {
    code: Schema.optional(Schema.String), message: Schema.optional(Schema.String), attempt: Count,
  }) }),
  Schema.TaggedStruct('Stale', { reason: Schema.TaggedStruct('Evicted', {}) }),
  Schema.TaggedStruct('Stale', { reason: Schema.TaggedStruct('Unknown', { detail: Schema.String }) }),
  Schema.TaggedStruct('Unknown', { detail: Schema.String }),
] as const

const mappings: readonly { freshness: FollowFreshness; expected: SyncStatus }[] = [
  { freshness: { _tag: 'Requested', since: 1 }, expected: { _tag: 'Requested', since: 1 } },
  { freshness: { _tag: 'Live', since: 2 }, expected: { _tag: 'Live', since: 2 } },
  { freshness: { _tag: 'Reconnecting', attempt: 1, nextAt: 3, issue: 'dropped' }, expected: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: 3, issue: 'dropped' }, lastLiveAt: 7 } },
  { freshness: { _tag: 'Reconnecting', attempt: 1, issue: 'dropped' }, expected: { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: 7 } },
  { freshness: { _tag: 'Stale', reason: { _tag: 'Resync', code: 'gap', message: '', attempt: 2 } }, expected: { _tag: 'Stale', reason: { _tag: 'Resync', code: 'gap', message: '', attempt: 2 }, lastLiveAt: 7 } },
  ...[
    { _tag: 'Resync' as const, attempt: 1 },
    { _tag: 'Resync' as const, code: '', message: 'gap', attempt: 1 },
    { _tag: 'Resync' as const, code: 'gap', attempt: 1 },
    { _tag: 'Unknown' as const, detail: 'not portable' },
  ].map((reason) => ({ freshness: { _tag: 'Stale' as const, reason }, expected: { _tag: 'Stale' as const, reason: { _tag: 'Unknown' as const }, lastLiveAt: 7 } })),
  { freshness: { _tag: 'Stale', reason: { _tag: 'Evicted' } }, expected: { _tag: 'Stale', reason: { _tag: 'Evicted' }, lastLiveAt: 7 } },
  { freshness: { _tag: 'Unknown', detail: 'not portable' }, expected: { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: 7 } },
]

describe('reviewed SyncStatus Amendment v2', () => {
  it.each(statuses)('round-trips portable member %#', (status) => {
    expect(Schema.decodeUnknownSync(SyncStatusSchema.SyncStatus)(status)).toEqual(status)
    expect(Schema.encodeSync(SyncStatusSchema.SyncStatus)(status)).toEqual(status)
    expect(SyncStatusSchema.SyncStatus.members.filter((member) => Schema.is(member)(status))).toHaveLength(1)
  })

  it.each(invalid)('rejects invalid reviewed member %#', (status) => {
    expect(Schema.is(SyncStatusSchema.SyncStatus)(status)).toBe(false)
  })

  it('strips unreviewed diagnostic fields rather than adding them to the portable union', () => {
    expect(Schema.decodeUnknownSync(SyncStatusSchema.SyncStatus)({
      _tag: 'Stale', reason: { _tag: 'Unknown', detail: 'internal' }, detail: 'internal',
    })).toEqual({ _tag: 'Stale', reason: { _tag: 'Unknown' } })
  })

  it.each(mappings)('maps exact freshness metadata %#', ({ freshness, expected }) => {
    expect(syncStatusFromFreshness(freshness, 7)).toEqual(expected)
  })

  // A separate property per branch guarantees coverage of every FollowFreshness variant,
  // including optional metadata; union generation alone can miss an entire branch.
  for (const [index, freshnessSchema] of freshnessSchemas.entries()) {
    it.effect.prop(`maps every generated freshness in branch ${index} to exactly one valid member`,
      [freshnessSchema, Schema.Union([Timestamp, Schema.Undefined])], ([freshness, lastLiveAt]) => Effect.sync(() => {
        const status = syncStatusFromFreshness(freshness, lastLiveAt)
        expect(Schema.is(SyncStatusSchema.SyncStatus)(status)).toBe(true)
        expect(SyncStatusSchema.SyncStatus.members.filter((member) => Schema.is(member)(status))).toHaveLength(1)
        if (status._tag === 'Stale') {
          if (lastLiveAt === undefined) expect(status).not.toHaveProperty('lastLiveAt')
          else expect(status.lastLiveAt).toBe(lastLiveAt)
        }
      }), { arbitrary: { runs: 100 } })
  }

  it.each([
    { error: new SubscriptionLimit({ cap: 8 }), cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 8 } } },
    { error: new Rejected({ code: 'forbidden', message: 'exact message' }), cause: { _tag: 'Server', code: 'forbidden', message: 'exact message' } },
    { error: new Rejected({ code: 'subscription-limit', message: 'server cap' }), cause: { _tag: 'Server', code: 'subscription-limit', message: 'server cap' } },
    { error: new Rejected({ code: undefined, message: 'not enough metadata' }), cause: { _tag: 'Unknown' } },
    { error: new Rejected({ code: '', message: 'not enough metadata' }), cause: { _tag: 'Unknown' } },
    { error: new DecodeFailure({ message: 'invalid frame' }), cause: { _tag: 'Unknown' } },
    { error: new AttachFailure({ message: 'attach refused' }), cause: { _tag: 'Unknown' } },
    { error: new AttachFailure({ code: 'terminal-unavailable', message: 'attach refused' }), cause: { _tag: 'Server', code: 'terminal-unavailable', message: 'attach refused' } },
  ])('maps failure metadata exactly %#', ({ error, cause }) => {
    const status = syncStatusFromFailure(error)
    expect(status).toEqual({ _tag: 'Failed', cause })
    expect(Schema.is(SyncStatusSchema.SyncStatus)(status)).toBe(true)
  })
})
