import * as React from 'react'
import { compactTime } from './model'

export type SidebarClock = { readonly now: number }
type TimeProps = { readonly at: number; readonly kind?: 'status' | 'activity' | 'turn'; readonly compact?: boolean; readonly showKind?: boolean }
const TimeValue = React.memo(function TimeValue({ at, now, kind = 'status', compact = false, showKind = false }: TimeProps & { readonly now: number }) {
  const iso = React.useMemo(() => new Date(at).toISOString(), [at])
  const value = compactTime({ at, now })
  const label = kind === 'turn' ? 'Last completed turn' : kind === 'activity' ? 'Last activity' : 'Observed status since'
  return <time dateTime={iso} title={`${label} ${iso}`} aria-label={`${kind === 'status' ? 'Elapsed in status' : label}: ${value}${kind === 'status' ? '' : ' ago'}`}>{showKind && kind === 'activity' ? 'act' : ''}{value}{kind !== 'status' && !compact ? ' ago' : ''}</time>
})
/** Explicit clock comes from the host; no controller/transport subscription is created. */
export const SidebarTime = React.memo(function SidebarTime({ now, ...props }: TimeProps & SidebarClock) { return <TimeValue {...props} now={now} /> })
