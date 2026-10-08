/**
 * Per-follow freshness: the transport-verdict state machine every follow exposes.
 *
 * Derived only from events the SDK actually observes — a subscribe command sent on an open
 * socket, a data frame routed to the subscription, a resync or transient-error frame, the
 * socket dropping, and the follow losing its admission slot. Nothing is time-based and
 * nothing is assumed: until the server ships heartbeat/ready frames (sync-contract PR A),
 * a quiet live follow keeps its last real verdict (`Live` stays grounded by an arrived
 * frame plus an open socket), and every verdict that would need a heartbeat to establish —
 * quiet-liveness, server-side readiness after admission — stays `Unknown`.
 *
 * The five states map onto the sync-contract client projection (Amendment v2):
 * `Requested` → Requested, `Live` → Live, `Reconnecting` → Stale(Reconnecting),
 * `Stale(reason)` → Stale(reason), `Unknown` → Stale(Unknown) or the pre-verdict state.
 */

/** Why data may be behind while the follow still exists. */
export type FreshnessStaleReason =
  /** The server is rereading (resync) or the SDK is retrying after a transient error. */
  | {
      readonly _tag: 'Resync'
      readonly code?: string
      readonly message?: string
      readonly attempt: number
    }
  /** The follow lost its admission slot; re-opening it is a fresh switch. */
  | { readonly _tag: 'Evicted' }
  /** A stale signal without metadata the SDK can name. */
  | { readonly _tag: 'Unknown'; readonly detail: string }

/** One follow's current freshness verdict. */
export type FollowFreshness =
  /** The subscribe command was sent on an open socket; the server has not answered yet. */
  | { readonly _tag: 'Requested'; readonly since: number }
  /** A data frame arrived and routed to this subscription while the socket is open. */
  | { readonly _tag: 'Live'; readonly since: number }
  /** The socket dropped; the channel will resubscribe and this follow stays retained. */
  | { readonly _tag: 'Reconnecting'; readonly attempt: number; readonly issue: string; readonly nextAt?: number }
  | { readonly _tag: 'Stale'; readonly reason: FreshnessStaleReason }
  /** No verdict can be derived yet — pre-send, or a heartbeat-dependent question pre-PR-A. */
  | { readonly _tag: 'Unknown'; readonly detail: string }

/** A transport event the SDK really observed for one follow. */
export type FreshnessEvent =
  | { readonly _tag: 'SubscribeSent' }
  | { readonly _tag: 'Frame' }
  | { readonly _tag: 'Resync'; readonly code?: string; readonly message?: string }
  | { readonly _tag: 'Retry'; readonly code?: string; readonly message?: string }
  | { readonly _tag: 'SocketDropped'; readonly attempt: number; readonly issue: string; readonly nextAt?: number }
  | { readonly _tag: 'Evicted' }

export const initialFreshness = (detail = 'no transport verdict yet'): FollowFreshness => ({
  _tag: 'Unknown',
  detail,
})

/** Fold one observed event into the next verdict; returns the same object when nothing changes. */
export const transitionFreshness = (
  current: FollowFreshness,
  event: FreshnessEvent,
  now: number,
): FollowFreshness => {
  switch (event._tag) {
    case 'SubscribeSent':
      return current._tag === 'Requested' ? current : { _tag: 'Requested', since: now }
    case 'Frame':
      // Only a real frame (first snapshot, delta, screen) can ground Live. Silence — with or
      // without an open socket — never re-confirms it: no heartbeat exists to do that yet.
      return current._tag === 'Live' ? current : { _tag: 'Live', since: now }
    case 'Resync':
    case 'Retry': {
      const attempt =
        current._tag === 'Stale' && current.reason._tag === 'Resync' ? current.reason.attempt + 1 : 1
      return {
        _tag: 'Stale',
        reason: {
          _tag: 'Resync',
          ...(event.code === undefined ? {} : { code: event.code }),
          ...(event.message === undefined ? {} : { message: event.message }),
          attempt,
        },
      }
    }
    case 'SocketDropped':
      return current._tag === 'Reconnecting' &&
        current.attempt === event.attempt &&
        current.issue === event.issue &&
        current.nextAt === event.nextAt
        ? current
        : { _tag: 'Reconnecting', attempt: event.attempt, issue: event.issue, ...(event.nextAt === undefined ? {} : { nextAt: event.nextAt }) }
    case 'Evicted':
      return current._tag === 'Stale' && current.reason._tag === 'Evicted'
        ? current
        : { _tag: 'Stale', reason: { _tag: 'Evicted' } }
  }
}
