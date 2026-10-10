import type { SyncStatus } from './sync-status'
export const syncNow = Date.UTC(2026, 9, 7, 13, 0, 0)
const since = syncNow - 3000
const older = syncNow - 12000
const reportedAt = syncNow
const progress = { elapsedMs: 3000, stageSince: since, reportedAt } as const
const observations: readonly { readonly id: string; readonly status: SyncStatus }[] = [
  { id: 'Connecting', status: { _tag: 'Connecting', attempt: 0, since } },
  { id: 'Requested', status: { _tag: 'Requested', since } },
  { id: 'Progress · queued', status: { _tag: 'Progress', stage: 'queued', ...progress } },
  { id: 'Progress · resolving', status: { _tag: 'Progress', stage: 'resolving', ...progress } },
  { id: 'Progress · routing', status: { _tag: 'Progress', stage: 'routing', host: 'owner-1', ...progress } },
  { id: 'Progress · reading', status: { _tag: 'Progress', stage: 'reading', ...progress } },
  { id: 'Reading · real counts', status: { _tag: 'Progress', stage: 'reading', host: 'owner-1', done: 184, total: 296, ...progress } },
  { id: 'Live', status: { _tag: 'Live', since } },
  { id: 'Stale · Resync', status: { _tag: 'Stale', reason: { _tag: 'Resync', code: 'internal', message: 'Snapshot read failed; st is retrying.', attempt: 1 }, lastLiveAt: since } },
  { id: 'Stale · Quiet', status: { _tag: 'Stale', reason: { _tag: 'Quiet', lastFrameAt: syncNow - 15000 }, lastLiveAt: older } },
  { id: 'Stale · Reconnecting', status: { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: syncNow + 4000, issue: 'Connection closed.' }, lastLiveAt: since } },
  { id: 'Stale · Evicted', status: { _tag: 'Stale', reason: { _tag: 'Evicted' }, lastLiveAt: older } },
  { id: 'Stale · Unknown', status: { _tag: 'Stale', reason: { _tag: 'Unknown' } } },
  { id: 'Stale · Unknown · last live', status: { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: older } },
  { id: 'Failed', status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'internal', message: 'The follow ended after the snapshot read failed.' } } },
  { id: 'Failed · forbidden', status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'Access denied by gateway.' } } },
  { id: 'Failed · unsupported', status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'unsupported', message: 'Capability absent.' } } },
  { id: 'Failed · Local · cap', status: { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 8 } } } },
  { id: 'Failed · Local · no cap', status: { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit' } } },
  { id: 'Failed · Local · zero cap', status: { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit', detail: { cap: 0 } } } },
  { id: 'Failed · Server · subscription limit', status: { _tag: 'Failed', cause: { _tag: 'Server', code: 'subscription-limit', message: 'The server subscription limit was reached.' } } },
  { id: 'Failed · Unknown', status: { _tag: 'Failed', cause: { _tag: 'Unknown' } } },
  { id: 'Derived stalled · no reply', status: { _tag: 'Requested', since: older } },
  { id: 'Derived stalled · slow stage', status: { _tag: 'Progress', stage: 'reading', elapsedMs: 12000, stageSince: older, reportedAt } },
  { id: 'Derived stalled · silent frames', status: { _tag: 'Progress', stage: 'routing', host: 'owner-1', elapsedMs: 12000, stageSince: older, reportedAt: syncNow - 6000 } },
  { id: 'Transient · under 400ms', status: { _tag: 'Requested', since: syncNow - 200 } },
]
export const syncObservations = observations.map(observation => ({ ...observation, observedAt: observation.status._tag === 'Requested' || observation.status._tag === 'Connecting' ? observation.status.since : observation.status._tag === 'Progress' ? observation.status.stageSince : syncNow - 3000 }))
