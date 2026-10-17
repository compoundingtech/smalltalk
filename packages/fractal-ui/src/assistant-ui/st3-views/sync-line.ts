// The host owns decoded facts and the clock.
import { earliestChange } from '../../time/human-time'
import type { SyncStatus, SyncFailureCause } from './sync-status'
export type { SyncStatus, SyncStage, StaleReason, SyncFailureCause } from './sync-status'
export interface SyncLineValue { readonly text: string; readonly tone: 'neutral' | 'warning' | 'error'; readonly animate: boolean; readonly announce: string }
export interface SyncLineInput { readonly status: SyncStatus; readonly label: string; readonly now: number; readonly observedAt: number; readonly gateway?: string; readonly socket?: boolean }
export interface SyncObservation { readonly status: SyncStatus; readonly observedAt: number }
/** Permissions and unsupported resources cannot be recovered by repeating the same request. */
export const canRetrySyncFailure = (cause: SyncFailureCause): boolean =>
  !(cause._tag === 'Server' && (cause.code === 'forbidden' || cause.code === 'unsupported'))
export const syncFailureRecovery = (cause: SyncFailureCause): string =>
  cause._tag === 'Server' && cause.code === 'forbidden' ? 'Ask an administrator.'
    : cause._tag === 'Server' && cause.code === 'unsupported' ? 'These details are not available here.'
      : 'Retry to load details.'
/** Client-only transition clock, kept outside the generated wire union. Same-stage progress frames retain it. */
export function observeSyncStatus(previous: SyncObservation | undefined, status: SyncStatus, now: number): SyncObservation {
  const key = status._tag === 'Progress' ? `${status._tag}/${status.stage}` : status._tag === 'Stale' ? `${status._tag}/${status.reason._tag}` : status._tag
  const old = previous?.status
  const oldKey = old?._tag === 'Progress' ? `${old._tag}/${old.stage}` : old?._tag === 'Stale' ? `${old._tag}/${old.reason._tag}` : old?._tag
  return { status, observedAt: key === oldKey ? previous!.observedAt : now }
}
const plainMessages: Readonly<Record<string, string>> = {
  'not-found': 'the requested resource was not found',
  internal: 'the server could not complete the request',
  unavailable: 'the server is unavailable',
  'subscription-limit': 'too many active subscriptions; close an unused pane and retry',
}
/** Shared vocabulary home for the workshop; intended to become the SDK-independent st3-views export. */
export function syncLine({ status, label, now, observedAt, gateway, socket = false }: SyncLineInput): SyncLineValue | undefined {
  const since = Math.max(0, now - observedAt)
  // These surface contracts do not carry subscription stages or socket diagnostics.
  if (socket && status._tag !== 'Connecting' && status._tag !== 'Live' && !(status._tag === 'Stale' && status.reason._tag === 'Reconnecting')) return undefined
  if (!socket && label === 'usage' && status._tag !== 'Requested' && status._tag !== 'Live' && status._tag !== 'Failed') return undefined
  if (!socket && label === 'usage' && status._tag === 'Failed' && ((status.cause._tag === 'Server' && status.cause.code === 'subscription-limit') || (status.cause._tag === 'Local' && status.cause.kind === 'subscription-limit'))) return undefined
  if (status._tag === 'Progress' && status.stage === 'resolving' && label !== 'conversation') return undefined
  let text: string
  let announce: string
  let tone: SyncLineValue['tone'] = 'neutral'
  let animate = false
  switch (status._tag) {
    case 'Connecting':
      if (since < 400) return undefined
      text = socket ? `Connecting${gateway === undefined ? '' : ` to ${gateway}`} · ${Math.floor(since / 1000)}s` : 'Connecting'
      announce = socket && gateway !== undefined ? `Connecting to ${gateway}` : 'Connecting'
      break
    case 'Requested':
      if (since < 400) return undefined
      if (since >= 5000) { text = `Loading ${label} is taking longer than expected · ${Math.floor(since / 1000)}s`; announce = `Loading ${label} is taking longer than expected`; tone = 'warning' }
      else { text = `Loading ${label}… · ${Math.floor(since / 1000)}s`; announce = `Loading ${label}` }
      break
    case 'Progress': {
      const stageAge = since
      const reportAge = Math.max(0, now - status.reportedAt)
      const elapsed = status.elapsedMs + reportAge
      if (elapsed < 400) return undefined
      if (reportAge >= 5000) { text = `Loading ${label} · no update for ${Math.floor(reportAge / 1000)}s`; announce = `Loading ${label} · waiting for an update`; tone = 'warning'; break }
      animate = true
      switch (status.stage) {
        case 'queued': text = `Waiting to load ${label} · ${Math.floor(elapsed / 1000)}s`; announce = `Waiting to load ${label}`; break
        case 'resolving': text = `Finding ${label} · ${Math.floor(elapsed / 1000)}s`; announce = `Finding ${label}`; break
        case 'routing': text = `Connecting to ${label} · ${Math.floor(elapsed / 1000)}s`; announce = `Connecting to ${label}`; break
        case 'reading': text = status.done !== undefined && status.total !== undefined ? `Loading ${label} · ${status.done} of ${status.total}` : `Loading ${label}… · ${Math.floor(elapsed / 1000)}s`; announce = `Loading ${label}`; break
      }
      if (stageAge >= 10000) { text += ' · slow'; announce += ' · slow'; tone = 'warning' }
      break
    }
    case 'Live':
      return socket ? { text: gateway ?? 'Connected', tone, animate: false, announce: gateway === undefined ? 'Connected' : `${gateway} connected` } : undefined
    case 'Stale': {
      const lastLive = status.lastLiveAt
      const asOf = lastLive === undefined ? 'Last-known content' : `As of ${new Date(lastLive).toISOString().slice(11, 19)}`
      tone = 'warning'
      switch (status.reason._tag) {
        case 'Evicted': return undefined
        case 'Unknown':
          text = lastLive === undefined ? `Waiting for an update · ${Math.floor(since / 1000)}s` : `Last updated ${Math.floor(Math.max(0, now - lastLive) / 1000)}s ago`
          announce = 'Waiting for an update'
          break
        case 'Resync':
          if (since < 400) return undefined
          text = `${asOf} · retrying: ${plainMessages[status.reason.code] ?? 'the previous request did not complete'}`; announce = `Retrying ${label}`; break
        case 'Quiet': text = `No update for ${Math.floor(Math.max(0, now - status.reason.lastFrameAt) / 1000)}s`; announce = 'Waiting for an update'; break
        case 'Reconnecting':
          if (since < 2000) return undefined
          text = socket ? `Reconnecting in ${Math.ceil(Math.max(0, status.reason.nextAt - now) / 1000)}s · attempt ${status.reason.attempt}` : `${asOf} · reconnecting`; announce = 'Reconnecting'; break
      }
      break
    }
    case 'Failed':
      tone = 'error'
      switch (status.cause._tag) {
        case 'Server':
          text = status.cause.code === 'forbidden' ? `No access to ${label} · ask an administrator` : status.cause.code === 'unsupported' ? `${label} is not available here` : `Couldn't load ${label}: ${plainMessages[status.cause.code] ?? 'the request did not complete'}`
          break
        case 'Local':
          text = status.cause.kind === 'subscription-limit' ? `Couldn't load ${label}: too many active subscriptions${status.cause.detail?.cap === undefined ? '' : ` (cap ${status.cause.detail.cap})`}; close an unused pane and retry` : `Couldn't load ${label}: the request did not complete`
          break
        case 'Unknown':
          text = `Couldn't load ${label}. Try again.`
          break
      }
      announce = text
      break
  }
  return { text, tone, animate, announce }
}

/** Main's sync copy uses whole seconds, including long waits; preserve that vocabulary. */
const nextSecond = (start: number, now: number): number => start + (Math.floor(Math.max(0, now - start) / 1000) + 1) * 1000

/** The next visibility threshold or displayed-second step of syncLine. */
export function syncLineNextChange({ status, now, observedAt, socket = false }: SyncLineInput): number | undefined {
  switch (status._tag) {
    case 'Connecting': return earliestChange(now, [observedAt + 400, socket ? nextSecond(observedAt, now) : undefined])
    case 'Requested': return earliestChange(now, [observedAt + 400, observedAt + 5000, nextSecond(observedAt, now)])
    case 'Progress': {
      const start = status.reportedAt - status.elapsedMs
      return earliestChange(now, [start + 400, status.reportedAt + 5000, observedAt + 10_000, nextSecond(start, now), now - status.reportedAt >= 5000 ? nextSecond(status.reportedAt, now) : undefined])
    }
    case 'Stale':
      switch (status.reason._tag) {
        case 'Unknown': return nextSecond(status.lastLiveAt ?? observedAt, now)
        case 'Quiet': return nextSecond(status.reason.lastFrameAt, now)
        case 'Resync': return earliestChange(now, [observedAt + 400])
        case 'Reconnecting': {
          const target = status.reason.nextAt
          const remaining = target - now
          return earliestChange(now, [observedAt + 2000, socket && remaining > 0 ? target - (Math.ceil(remaining / 1000) - 1) * 1000 : undefined])
        }
        case 'Evicted': return undefined
      }
    case 'Live':
    case 'Failed': return undefined
  }
}
