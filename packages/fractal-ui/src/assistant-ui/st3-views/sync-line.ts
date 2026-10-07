// The host owns decoded facts and the clock.
import type { SyncStatus } from './sync-status'
export type { SyncStatus, SyncStage, StaleReason } from './sync-status'
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
  'subscription-limit': 'too many active subscriptions; close an unused pane and retry',
  'not-found': 'the requested resource was not found',
  internal: 'st could not complete the read',
  unavailable: 'the owner host is unavailable',
}
/** Shared vocabulary home for the workshop; intended to become the SDK-independent st3-views export. */
export function syncLine({ status, label, now, observedAt, gateway, socket = false }: SyncLineInput): SyncLineValue | undefined {
  const since = Math.max(0, now - observedAt)
  let text: string
  let announce: string
  let tone: SyncLineValue['tone'] = 'neutral'
  let animate = false
  switch (status._tag) {
    case 'Connecting':
      if (since < 400) return undefined
      text = socket ? `Connecting to ${gateway ?? 'gateway'} · ${Math.floor(since / 1000)}s` : 'Connecting'
      announce = socket ? `Connecting to ${gateway ?? 'gateway'}` : 'Connecting'
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
      return socket ? { text: gateway ?? 'gateway', tone, animate: false, announce: `${gateway ?? 'gateway'} connected` } : undefined
    case 'Stale': {
      const lastLive = status.lastLiveAt
      const asOf = lastLive === undefined ? 'Last-known content' : `As of ${new Date(lastLive).toISOString().slice(11, 19)}`
      tone = 'warning'
      switch (status.reason._tag) {
        case 'Evicted': return undefined
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
      text = status.code === 'forbidden' ? `No access to ${label} · ask the gateway owner` : status.code === 'unsupported' ? `This gateway doesn't serve ${label}` : `Couldn't load ${label}: ${plainMessages[status.code] ?? status.message}`
      announce = text
      break
  }
  return { text, tone, animate, announce }
}
