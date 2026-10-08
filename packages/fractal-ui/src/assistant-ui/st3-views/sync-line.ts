// The host owns decoded facts and the clock.
import type { SyncStatus } from './sync-status'
export type { SyncStatus, SyncStage, StaleReason, SyncFailureCause } from './sync-status'
export interface SyncLineValue { readonly text: string; readonly tone: 'neutral' | 'warning' | 'error'; readonly animate: boolean; readonly announce: string }
export interface SyncLineInput { readonly status: SyncStatus; readonly label: string; readonly now: number; readonly observedAt: number; readonly gateway?: string; readonly socket?: boolean }
export interface SyncObservation { readonly status: SyncStatus; readonly observedAt: number }
/** Client-only transition clock, kept outside the generated wire union. Same-stage progress frames retain it. */
export function observeSyncStatus(previous: SyncObservation | undefined, status: SyncStatus, now: number): SyncObservation {
  const key = status._tag === 'Progress' ? `${status._tag}/${status.stage}` : status._tag === 'Stale' ? `${status._tag}/${status.reason._tag}` : status._tag
  const old = previous?.status
  const oldKey = old?._tag === 'Progress' ? `${old._tag}/${old.stage}` : old?._tag === 'Stale' ? `${old._tag}/${old.reason._tag}` : old?._tag
  return { status, observedAt: key === oldKey ? previous!.observedAt : now }
}
const plainMessages: Readonly<Record<string, string>> = {
  'not-found': 'the requested resource was not found',
  internal: 'st could not complete the read',
  unavailable: 'the owner host is unavailable',
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
      if (since >= 5000) { text = `No reply from st for ${Math.floor(since / 1000)}s`; announce = 'No reply from st'; tone = 'warning' }
      else { text = `Asked st for ${label} · ${Math.floor(since / 1000)}s`; announce = `Asked st for ${label}` }
      break
    case 'Progress': {
      const stageAge = since
      const reportAge = Math.max(0, now - status.reportedAt)
      const elapsed = status.elapsedMs + reportAge
      if (elapsed < 400) return undefined
      if (reportAge >= 5000) { text = `st stopped reporting at ${status.stage} · ${Math.floor(reportAge / 1000)}s`; announce = `st stopped reporting at ${status.stage}`; tone = 'warning'; break }
      animate = true
      const host = status.host === undefined ? '' : ` on ${status.host}`
      switch (status.stage) {
        case 'queued': text = `Queued at st · ${Math.floor(elapsed / 1000)}s`; announce = 'Queued at st'; break
        case 'resolving': text = `Finding where this conversation lives · ${Math.floor(elapsed / 1000)}s${host}`; announce = `Finding where this conversation lives${host}`; break
        case 'routing': text = `Waiting on ${status.host ?? 'owner host'} · ${Math.floor(elapsed / 1000)}s`; announce = `Waiting on ${status.host ?? 'owner host'}`; break
        case 'reading': text = status.done !== undefined && status.total !== undefined ? `Reading ${label} · ${status.done} of ${status.total}${host}` : `Reading ${label} · ${Math.floor(elapsed / 1000)}s${host}`; announce = `Reading ${label}${host}`; break
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
          text = lastLive === undefined ? `Stale · observed ${Math.floor(since / 1000)}s ago` : `Stale · ${Math.floor(Math.max(0, now - lastLive) / 1000)}s since last live`
          announce = 'Stale'
          break
        case 'Resync':
          if (since < 400) return undefined
          text = `${asOf} · st retrying: ${plainMessages[status.reason.code] ?? status.reason.message}`; announce = `st retrying: ${plainMessages[status.reason.code] ?? status.reason.message}`; break
        case 'Quiet': text = `Last heard from st ${Math.floor(Math.max(0, now - status.reason.lastFrameAt) / 1000)}s ago`; announce = 'No heartbeat from st'; break
        case 'Reconnecting':
          if (since < 2000) return undefined
          text = socket ? `Reconnecting in ${Math.ceil(Math.max(0, status.reason.nextAt - now) / 1000)}s · attempt ${status.reason.attempt} · ${status.reason.issue}` : `${asOf} · reconnecting`; announce = 'Reconnecting'; break
      }
      break
    }
    case 'Failed':
      tone = 'error'
      switch (status.cause._tag) {
        case 'Server':
          text = status.cause.code === 'forbidden' ? `No access to ${label} · ask the gateway owner` : status.cause.code === 'unsupported' ? `This gateway doesn't serve ${label}` : `Couldn't load ${label}: ${plainMessages[status.cause.code] ?? status.cause.message}`
          break
        case 'Local':
          text = status.cause.kind === 'subscription-limit' ? `Couldn't load ${label}: too many active subscriptions${status.cause.detail?.cap === undefined ? '' : ` (cap ${status.cause.detail.cap})`}; close an unused pane and retry` : `Couldn't load ${label}: ${status.cause.detail?.message ?? status.cause.kind}`
          break
        case 'Unknown':
          text = `Couldn't load ${label} · cause unknown`
          break
      }
      announce = text
      break
  }
  return { text, tone, animate, announce }
}
